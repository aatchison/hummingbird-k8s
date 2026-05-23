#!/usr/bin/env bash
# scripts/export-argocd.sh — produce an ArgoCD-registerable kubeconfig.
#
# ArgoCD's `cluster add` command consumes a normal kubeconfig and uses it
# ONCE at registration time to bootstrap a ServiceAccount + scoped token
# in the target cluster. After that it discards the original credential
# and authenticates via the SA token it minted. So all we need to hand
# ArgoCD is a valid kubeconfig that points at a reachable apiserver and
# carries enough authority to create that SA.
#
# The simplest such file is /etc/kubernetes/admin.conf on the CP — but
# that's a 0600 root-owned file carrying the cluster signing CA and a
# full-cluster client cert. This script:
#
#   1. SSHes to the CP using the same identity flow as deploy-cluster.sh
#      (private key paired with SSH_PUBKEY_FILE from CONFIG).
#   2. `sudo cat`s admin.conf and streams it back over SSH into a
#      umask-077 temp file.
#   3. Rewrites the `server:` URL so ArgoCD can reach the apiserver from
#      wherever it's running (kubeadm bakes in the CP node's local IP,
#      which often isn't routable from the ArgoCD pod).
#   4. Rewrites the kubeadm default cluster/context/user names from
#      `kubernetes` to the operator-supplied --context-name, so the file
#      doesn't collide with the operator's other kubeconfigs and so
#      ArgoCD registers the cluster under a meaningful name.
#   5. Writes to --output with mode 0600. Never echoes admin.conf
#      contents to stdout.
#
# Security: the output IS admin.conf — a full-cluster credential. After
# `argocd cluster add` succeeds, ArgoCD only needs the SA token it
# created; delete or store the file 0600 once registration is done. If
# the file leaks, follow the kubeadm cert rotation procedure (see
# docs/argocd.md).
#
# Usage:
#   CONFIG=cluster.local.conf bash scripts/export-argocd.sh [flags]
#
# Flags:
#   --output FILE         Output path (default: ./argocd-kubeconfig.yaml)
#   --server URL          Override apiserver URL (default: https://<CP_IP>:6443)
#   --context-name NAME   Cluster/context/user name (default: hummingbird-<CP_NAME>)
#   --force               Overwrite an existing output file

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
# REPO_ROOT is not referenced here, but keeping SCRIPT_DIR computed lets
# future helpers in lib/ sourced from this script find their siblings.
export SCRIPT_DIR

log()  { printf '[export-argocd] %s\n' "$*" >&2; }
fail() { printf '[export-argocd] ERROR: %s\n' "$*" >&2; exit 1; }

# ---- CONFIG + flag parsing --------------------------------------------------

: "${CONFIG:?CONFIG=<path-to-cluster.local.conf> is required}"
[[ -r "$CONFIG" ]] || fail "config not readable: $CONFIG"

OUTPUT=""
SERVER_OVERRIDE=""
CONTEXT_OVERRIDE=""
FORCE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)        OUTPUT="${2:-}";          shift 2 ;;
    --output=*)      OUTPUT="${1#*=}";          shift ;;
    --server)        SERVER_OVERRIDE="${2:-}"; shift 2 ;;
    --server=*)      SERVER_OVERRIDE="${1#*=}"; shift ;;
    --context-name)  CONTEXT_OVERRIDE="${2:-}"; shift 2 ;;
    --context-name=*) CONTEXT_OVERRIDE="${1#*=}"; shift ;;
    --force)         FORCE=1; shift ;;
    -h|--help)
      sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) fail "unknown flag: $1" ;;
  esac
done

# ---- source CONFIG ----------------------------------------------------------

# shellcheck disable=SC1090
source "$CONFIG"

: "${CP_NAME:?CP_NAME is required in $CONFIG}"
: "${SSH_PUBKEY_FILE:?SSH_PUBKEY_FILE is required in $CONFIG}"

# Expand any ~ / $HOME in SSH_PUBKEY_FILE the same way deploy-cluster.sh does
# (it's sourced as bash, so the operator may use $HOME). Already-expanded
# paths are no-ops here.
SSH_PUBKEY_FILE="${SSH_PUBKEY_FILE/#\~/$HOME}"

[[ -r "$SSH_PUBKEY_FILE" ]] || fail "SSH_PUBKEY_FILE not readable: $SSH_PUBKEY_FILE"
SSH_PRIVKEY_FILE="${SSH_PUBKEY_FILE%.pub}"
[[ -r "$SSH_PRIVKEY_FILE" ]] || fail "SSH private key not readable: $SSH_PRIVKEY_FILE (expected next to $SSH_PUBKEY_FILE)"

# ---- resolve CP_IP ----------------------------------------------------------
# CP_IP can come from the config directly (operator pinned), or be resolved
# via libvirt against CP_NAME on the local KVM host. Mirror deploy-cluster.sh.

if [[ -z "${CP_IP:-}" ]]; then
  if command -v virsh >/dev/null 2>&1; then
    CP_IP="$(virsh -c qemu:///system domifaddr "$CP_NAME" 2>/dev/null \
              | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}' || true)"
  fi
fi
[[ -n "${CP_IP:-}" ]] || fail "could not resolve CP_IP — set CP_IP=<ip> in $CONFIG or run from the KVM host with libvirt access to '$CP_NAME'"

# ---- derive defaults --------------------------------------------------------

OUTPUT="${OUTPUT:-${PWD}/argocd-kubeconfig.yaml}"
SERVER_URL="${SERVER_OVERRIDE:-https://${CP_IP}:6443}"
CONTEXT_NAME="${CONTEXT_OVERRIDE:-hummingbird-${CP_NAME}}"

# Validate context name — kubeconfig names can be reasonably permissive but
# ArgoCD treats them as URL-safe IDs. Keep it conservative.
[[ "$CONTEXT_NAME" =~ ^[A-Za-z0-9._-]+$ ]] \
  || fail "--context-name must match [A-Za-z0-9._-]+ (got: '$CONTEXT_NAME')"

if [[ -e "$OUTPUT" && "$FORCE" -ne 1 ]]; then
  fail "output file already exists: $OUTPUT (re-run with --force to overwrite)"
fi

# ---- SSH helper -------------------------------------------------------------

cp_ssh() {
  ssh -i "$SSH_PRIVKEY_FILE" \
    -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR \
    -o ConnectTimeout=10 \
    -o BatchMode=yes \
    "root@${CP_IP}" "$@"
}

# ---- pull admin.conf --------------------------------------------------------
# umask 077 in the subshell so the temp file is mode 0600 from creation —
# never world-readable, even transiently. We `sudo cat` because admin.conf
# is 0600 root, and the SSH user may or may not be root (the deploy flow
# logs in as root, so `sudo` is a passthrough; we keep `sudo` for cases
# where the operator wires up a non-root SSH user later).

TMP_KUBECONFIG="$(umask 077 && mktemp -t argocd-kubeconfig-XXXXXX.yaml)"
cleanup() { [[ -n "${TMP_KUBECONFIG:-}" && -e "$TMP_KUBECONFIG" ]] && rm -f "$TMP_KUBECONFIG"; }
trap cleanup EXIT

log "fetching /etc/kubernetes/admin.conf from root@${CP_IP}"
if ! cp_ssh "sudo cat /etc/kubernetes/admin.conf" > "$TMP_KUBECONFIG" 2>/dev/null; then
  fail "ssh root@${CP_IP} 'sudo cat /etc/kubernetes/admin.conf' failed — is the CP up and reachable on $SSH_PRIVKEY_FILE?"
fi
[[ -s "$TMP_KUBECONFIG" ]] || fail "fetched admin.conf is empty"

# Quick sanity check: looks like a kubeconfig?
if ! grep -q '^apiVersion:' "$TMP_KUBECONFIG" \
  || ! grep -q '^kind: Config' "$TMP_KUBECONFIG"; then
  fail "fetched file does not look like a kubeconfig (no 'kind: Config'). Refusing to continue."
fi

# ---- rewrite server URL -----------------------------------------------------
# Prefer yq if available — surgical and structure-aware. Fall back to a
# constrained sed that only touches the `server: https://…` line inside a
# clusters[].cluster block. kubeadm's admin.conf only has one cluster
# entry, so the simple substitution is safe.

if command -v yq >/dev/null 2>&1; then
  log "rewriting server URL via yq -> ${SERVER_URL}"
  yq -i ".clusters[0].cluster.server = \"${SERVER_URL}\"" "$TMP_KUBECONFIG"
else
  log "rewriting server URL via sed -> ${SERVER_URL}"
  # Escape any sed-meaningful characters in the URL before substitution.
  esc_url="$(printf '%s\n' "$SERVER_URL" | sed -e 's/[\/&]/\\&/g')"
  sed -i -E "s|^([[:space:]]+server:[[:space:]]+).*$|\1${esc_url}|" "$TMP_KUBECONFIG"
fi

# ---- rewrite cluster/context/user names from kubeadm default ----------------
# kubeadm emits cluster=kubernetes, user=kubernetes-admin, context=
# kubernetes-admin@kubernetes. We rename ALL of these to CONTEXT_NAME so
# the file plugs into ArgoCD without colliding with the operator's other
# kubeconfigs (e.g. another in-tree cluster also named "kubernetes").

if command -v yq >/dev/null 2>&1; then
  log "rewriting cluster/context/user names via yq -> ${CONTEXT_NAME}"
  yq -i "
    .clusters[0].name = \"${CONTEXT_NAME}\" |
    .users[0].name = \"${CONTEXT_NAME}\" |
    .contexts[0].name = \"${CONTEXT_NAME}\" |
    .contexts[0].context.cluster = \"${CONTEXT_NAME}\" |
    .contexts[0].context.user = \"${CONTEXT_NAME}\" |
    .current-context = \"${CONTEXT_NAME}\"
  " "$TMP_KUBECONFIG"
else
  log "rewriting cluster/context/user names via sed -> ${CONTEXT_NAME}"
  # Drop all kubeadm defaults to CONTEXT_NAME. Order matters: do the
  # composite context name (kubernetes-admin@kubernetes) BEFORE the
  # singleton "kubernetes" / "kubernetes-admin" replacements, otherwise
  # we'd produce ${CONTEXT_NAME}-admin@${CONTEXT_NAME}.
  esc_ctx="$(printf '%s\n' "$CONTEXT_NAME" | sed -e 's/[\/&]/\\&/g')"
  sed -i -E \
    -e "s/kubernetes-admin@kubernetes/${esc_ctx}/g" \
    -e "s/kubernetes-admin/${esc_ctx}/g" \
    -e "s/(^|[^A-Za-z0-9_-])kubernetes([^A-Za-z0-9_-]|$)/\1${esc_ctx}\2/g" \
    "$TMP_KUBECONFIG"
fi

# ---- move into place with 0600 ----------------------------------------------

chmod 0600 "$TMP_KUBECONFIG"
mv -f "$TMP_KUBECONFIG" "$OUTPUT"
chmod 0600 "$OUTPUT"
# Disarm the EXIT trap — TMP_KUBECONFIG no longer exists.
TMP_KUBECONFIG=""

log "kubeconfig written to ${OUTPUT}"
log "register with:  argocd cluster add ${CONTEXT_NAME} --kubeconfig ${OUTPUT}"
log "sanity check:   KUBECONFIG=${OUTPUT} kubectl get nodes"
