#!/usr/bin/env bash
# rotate-etcd-encryption-key.sh — rotate the etcd at-rest encryption key.
#
# Closes #120. Operator-driven; pauses for confirmation before every
# destructive step. The script never runs end-to-end without prompts —
# encryption-key rotation is the kind of operation where a wrong-order
# step (e.g. dropping the old key before secrets are re-encrypted) makes
# every existing Secret unreadable, so each stage gates on `read`.
#
# Flow (mirrors the upstream Kubernetes key-rotation playbook):
#
#   Stage 1 - Generate a fresh 32-byte random key, build a new
#             EncryptionConfiguration with the NEW key as the primary
#             provider entry and the OLD key as the secondary (so the
#             apiserver can still decrypt rows written with the old
#             key). Copy to the CP and touch the apiserver static-pod
#             manifest to trigger a reload.
#
#   Stage 2 - Re-encrypt every existing Secret / ConfigMap in place
#             with `kubectl get ... -o json | kubectl replace -f -`.
#             Each replace causes the apiserver to rewrite the row,
#             and because the NEW key is now primary the rewrite is
#             encrypted under the new key.
#
#   Stage 3 - Build a final EncryptionConfiguration that drops the
#             OLD key, leaving only the NEW key (plus the trailing
#             `identity` fallback that was already there). Copy to
#             the CP and reload the apiserver again. At this point
#             the old key material is no longer in use.
#
#   Stage 4 - Run /usr/libexec/verify-encryption.sh on the CP to
#             prove a Secret reads back as `k8s:enc:aesgcm:v1:...`.
#
# Pre-flight: this script *strongly* recommends taking a labeled etcd
# snapshot via `make backup-etcd LABEL=pre-key-rotation` first. If
# Stage 2 fails partway, you may need to restore from the snapshot
# before any rows get rewritten with a key you no longer have the
# config for.
#
# Algorithm note: we use `aesgcm` (matches k8s-init.sh and the existing
# /etc/kubernetes/encryption-config.yaml shape).
#
# Env:
#   CP_IP     — CP node IP. Defaults to the InternalIP of the first
#               control-plane node (via scripts/kubectl-k8s.sh).
#   KVM_HOST  — Optional SSH ProxyJump host. If set, SSH/SCP to the
#               CP routes through this host (matches backup-etcd.sh /
#               restore-etcd.sh conventions).
#
# Usage:
#   scripts/rotate-etcd-encryption-key.sh
#   # or: make rotate-etcd-key
#
# Path-anchoring: this script resolves the sibling kubectl-k8s.sh via
# $REPO_ROOT derived from its own location, so it works no matter what
# cwd the operator runs it from. See issue #271 F6.
set -euo pipefail

# Resolve repo root from this script's location so relative siblings
# (notably scripts/kubectl-k8s.sh) work from any cwd. Matches the
# pattern established by scripts/run-kube-bench.sh.
REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

log() { printf '[rotate-etcd-key] %s\n' "$*" >&2; }

confirm() {
  local prompt="$1"
  local ans
  read -rp "$prompt [y/N] " ans
  [[ "$ans" =~ ^[Yy]$ ]] || { log "aborted by operator"; exit 1; }
}

CP_IP="${CP_IP:-$("${REPO_ROOT}/scripts/kubectl-k8s.sh" get nodes \
  -l node-role.kubernetes.io/control-plane \
  -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')}"
[[ -n "$CP_IP" ]] || { log "could not resolve control-plane IP"; exit 1; }

SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=no)
[[ -n "${KVM_HOST:-}" ]] && SSH_OPTS+=(-o ProxyJump="$KVM_HOST")

log "control plane: $CP_IP"
log "pre-flight: did you 'make backup-etcd LABEL=pre-key-rotation' already?"
log "            (see docs/backup-restore.md 'When to snapshot')"
confirm "Continue with rotation?"

# ----------------------------------------------------------------------
# Stage 0: capture the current config so we can derive the new one.
# ----------------------------------------------------------------------
WORKDIR=$(mktemp -d -t rotate-etcd-key.XXXXXX)
trap 'rm -rf "$WORKDIR"' EXIT
BEFORE="$WORKDIR/encryption-config.before.yaml"
NEWCFG="$WORKDIR/encryption-config.new.yaml"
FINAL="$WORKDIR/encryption-config.final.yaml"

log "fetching current /etc/kubernetes/encryption-config.yaml from CP"
ssh "${SSH_OPTS[@]}" "root@$CP_IP" \
  "cat /etc/kubernetes/encryption-config.yaml" > "$BEFORE"
[[ -s "$BEFORE" ]] || { log "fetched encryption-config is empty"; exit 1; }

# ----------------------------------------------------------------------
# Stage 1: build dual-key config (NEW primary, OLD secondary).
# ----------------------------------------------------------------------
NEW_KEY="$(head -c 32 /dev/urandom | base64 -w0)"
NEW_KEY_NAME="key-$(date -u +%Y%m%d%H%M%S)"
log "generated new key (base64, 32 bytes) named '$NEW_KEY_NAME'"

# Use python3 + PyYAML on the host. We rewrite the providers' keys
# array: new key first, then keep whatever was there. This preserves
# the trailing `identity` provider (read-fallback for legacy rows).
NEW_KEY_NAME="$NEW_KEY_NAME" NEW_KEY="$NEW_KEY" \
INFILE="$BEFORE" OUTFILE="$NEWCFG" python3 - <<'PY'
import os, yaml
with open(os.environ["INFILE"]) as f:
    cfg = yaml.safe_load(f)
providers = cfg["resources"][0]["providers"]
# Find the aesgcm provider entry; we know k8s-init.sh writes aesgcm.
aes_idx = next(
    (i for i, p in enumerate(providers) if "aesgcm" in p),
    None,
)
if aes_idx is None:
    raise SystemExit("no aesgcm provider found in encryption config")
old_keys = providers[aes_idx]["aesgcm"]["keys"]
new_entry = {
    "name": os.environ["NEW_KEY_NAME"],
    "secret": os.environ["NEW_KEY"],
}
# New key is primary (index 0); old keys stay so existing rows decrypt.
providers[aes_idx]["aesgcm"]["keys"] = [new_entry] + old_keys
with open(os.environ["OUTFILE"], "w") as f:
    yaml.safe_dump(cfg, f, default_flow_style=False, sort_keys=False)
PY

log "Stage 1: install dual-key config (new=primary, old=secondary) + reload apiserver"
confirm "Proceed with Stage 1?"
scp "${SSH_OPTS[@]}" "$NEWCFG" "root@$CP_IP:/etc/kubernetes/encryption-config.yaml.new"
ssh "${SSH_OPTS[@]}" "root@$CP_IP" '
  set -euo pipefail
  install -m 0600 -o root -g root \
    /etc/kubernetes/encryption-config.yaml.new \
    /etc/kubernetes/encryption-config.yaml
  rm -f /etc/kubernetes/encryption-config.yaml.new
  # Touching the static-pod manifest is the documented way to make the
  # kubelet re-create the apiserver pod, which re-reads the encryption
  # config file. The manifest contents do not need to change.
  touch /etc/kubernetes/manifests/kube-apiserver.yaml
'
log "waiting 30s for apiserver to come back with the new config"
sleep 30
ssh "${SSH_OPTS[@]}" "root@$CP_IP" '
  set -euo pipefail
  KUBECONFIG=/etc/kubernetes/admin.conf kubectl get --raw=/healthz >/dev/null
' || { log "apiserver healthz failed after Stage 1 reload"; exit 1; }

# ----------------------------------------------------------------------
# Stage 2: re-encrypt every Secret and ConfigMap in place.
# ----------------------------------------------------------------------
log "Stage 2: re-encrypt every existing Secret and ConfigMap"
log "         (kubectl get -A -o json | kubectl replace -f -)"
confirm "Proceed with Stage 2?"
ssh "${SSH_OPTS[@]}" "root@$CP_IP" '
  set -euo pipefail
  export KUBECONFIG=/etc/kubernetes/admin.conf
  # Secrets first, then ConfigMaps. Each `replace` triggers an etcd
  # rewrite under the now-primary new key. `--force` would recreate
  # the resource which can break selectors/UIDs — we deliberately
  # stick to plain replace.
  kubectl get secrets -A -o json | kubectl replace -f -
  kubectl get configmaps -A -o json | kubectl replace -f -
'

# ----------------------------------------------------------------------
# Stage 3: drop the OLD key from the config; new key is now sole key.
# ----------------------------------------------------------------------
INFILE="$NEWCFG" OUTFILE="$FINAL" python3 - <<'PY'
import os, yaml
with open(os.environ["INFILE"]) as f:
    cfg = yaml.safe_load(f)
providers = cfg["resources"][0]["providers"]
aes_idx = next(
    (i for i, p in enumerate(providers) if "aesgcm" in p),
    None,
)
if aes_idx is None:
    raise SystemExit("no aesgcm provider found in encryption config")
# Drop everything but the new (index 0) key.
providers[aes_idx]["aesgcm"]["keys"] = providers[aes_idx]["aesgcm"]["keys"][:1]
with open(os.environ["OUTFILE"], "w") as f:
    yaml.safe_dump(cfg, f, default_flow_style=False, sort_keys=False)
PY

log "Stage 3: drop old key from config + reload apiserver"
confirm "Proceed with Stage 3?"
scp "${SSH_OPTS[@]}" "$FINAL" "root@$CP_IP:/etc/kubernetes/encryption-config.yaml.new"
ssh "${SSH_OPTS[@]}" "root@$CP_IP" '
  set -euo pipefail
  install -m 0600 -o root -g root \
    /etc/kubernetes/encryption-config.yaml.new \
    /etc/kubernetes/encryption-config.yaml
  rm -f /etc/kubernetes/encryption-config.yaml.new
  touch /etc/kubernetes/manifests/kube-apiserver.yaml
'
log "waiting 30s for apiserver to come back with the single-key config"
sleep 30
ssh "${SSH_OPTS[@]}" "root@$CP_IP" '
  set -euo pipefail
  KUBECONFIG=/etc/kubernetes/admin.conf kubectl get --raw=/healthz >/dev/null
' || { log "apiserver healthz failed after Stage 3 reload"; exit 1; }

# ----------------------------------------------------------------------
# Stage 4: verify a Secret round-trips through the new key.
# ----------------------------------------------------------------------
log "Stage 4: verify via /usr/libexec/verify-encryption.sh"
# The shipped verifier accepts EXPECTED_PREFIX so callers can gate on
# the specific keyname rather than just `k8s:enc:aesgcm:`.
ssh "${SSH_OPTS[@]}" "root@$CP_IP" \
  "EXPECTED_PREFIX='k8s:enc:aesgcm:v1:${NEW_KEY_NAME}:' /usr/libexec/verify-encryption.sh"

log "rotation complete; new key name on CP: $NEW_KEY_NAME"
log "post-flight: consider 'make backup-etcd LABEL=post-key-rotation'"
