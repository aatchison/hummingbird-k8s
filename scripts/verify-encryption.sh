#!/bin/bash
# verify-encryption.sh — Verify that etcd at-rest encryption is active.
#
# Two operating modes:
#
#   1. REMOTE (workstation operation, via KVM_HOST) — closes #271 F2.
#      When `KVM_HOST` is set (or `CP_IP` is provided), the script ssh's
#      to root@<CP_IP> and runs the baked-in copy at
#      `/usr/libexec/verify-encryption.sh` on the CP. This is the path
#      the Makefile target `make verify-encryption` exercises from a
#      workstation that has no local libvirt or kubectl wiring. CP_IP
#      is resolved by the shared `resolve_cp_ip` helper in
#      `lib/build-common.sh` (added in #276): explicit CP_IP wins,
#      otherwise ssh through KVM_HOST runs `virsh -c qemu:///system
#      domifaddr`, otherwise local virsh, otherwise a clear failure.
#
#   2. LOCAL (on the CP node) — original flow.
#      When neither KVM_HOST nor CP_IP is set AND a local `kubectl` can
#      reach `https://127.0.0.1` via the kubeadm admin.conf, we assume
#      we're running on the CP itself (the image's
#      /usr/libexec/verify-encryption.sh re-invokes this same script
#      body, and `rotate-etcd-encryption-key.sh` ssh's it onto the CP).
#      Strategy on the CP:
#        a. Create a probe Secret with a distinctive value via kubectl.
#        b. Read it raw from etcd, in this order:
#             i. `etcdctl` on PATH (rare on bootc CP), OR
#            ii. host-side `crictl exec` into the etcd container (k8s
#                1.31's etcd static pod is distroless, so
#                `kubectl exec ... -- sh` is never available — must go
#                through crictl on the host).
#        c. Assert the stored blob starts with the expected envelope
#           prefix (`k8s:enc:aesgcm:` for AESGCM).
#        d. Clean up the probe Secret either way.
#
# Examples:
#   # From a workstation, tunneling via the KVM host:
#   KVM_HOST=geary CP_NAME=hbird-cp1 ./scripts/verify-encryption.sh
#
#   # From a workstation, with CP_IP explicit (no libvirt query needed):
#   CP_IP=192.168.122.42 ./scripts/verify-encryption.sh
#
#   # On the CP itself (used by /usr/libexec/verify-encryption.sh and by
#   # rotate-etcd-encryption-key.sh's Stage 4):
#   sudo ./scripts/verify-encryption.sh
#
# Override the keyname check by setting EXPECTED_PREFIX (useful in
# rotation tests, e.g. EXPECTED_PREFIX='k8s:enc:aesgcm:v1:key-...:').
#
# Path-anchoring: the remote-mode branch sources lib/build-common.sh
# from this script's location, so it works no matter what cwd the
# operator runs it from. See issue #271 F6.

set -euo pipefail

NS="${NS:-default}"
PROBE_NAME="${PROBE_NAME:-etcd-encryption-probe-$$}"
PROBE_KEY="${PROBE_KEY:-probe}"
PROBE_VALUE="${PROBE_VALUE:-hummingbird-encryption-probe-value}"
# The full envelope prefix on the row is
# `k8s:enc:aesgcm:v1:<keyname>:<binary>`. Our keyname is `bootstrap` (see
# k8s-init.sh). A substring check on `k8s:enc:aesgcm:` is sufficient to
# prove encryption is active, but we accept an override for stricter
# callers (e.g. CI gating on the specific keyname).
EXPECTED_PREFIX="${EXPECTED_PREFIX:-k8s:enc:aesgcm:}"

# ──────────────────────────────────────────────────────────────────────
# Mode selection: remote (workstation, via KVM_HOST / CP_IP) vs local
# (running on the CP itself, e.g. the image's /usr/libexec copy).
#
# We pick "remote" whenever the operator has provided ANY plumbing that
# only makes sense from a workstation — KVM_HOST (ProxyJump alias) OR
# CP_IP (explicit target). On the CP node itself /usr/libexec invokes
# us without those vars, so we fall through to the local flow there.
# ──────────────────────────────────────────────────────────────────────
if [[ -n "${KVM_HOST:-}" || -n "${CP_IP:-}" ]]; then
  REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
  # shellcheck source=../lib/build-common.sh
  source "${REPO_ROOT}/lib/build-common.sh"

  log() { printf '[verify-encryption] %s\n' "$*" >&2; }

  # #362: when we're already on the KVM host (e.g. deploy-cluster
  # re-exec, or operator running the verifier directly on the
  # hypervisor as root), ProxyJump=$KVM_HOST below would become
  # `ssh root@KVM_HOST` from KVM_HOST itself — sshd typically denies
  # root login and the call hangs on a never-answered password prompt.
  # In that case the libvirt NAT subnet IS already routable (we're on
  # it), so we can ssh directly to root@<cp_ip> without a ProxyJump.
  # Hostname detection mirrors scripts/lib/ssh-wrap.sh.
  _ve_local_host="$(hostname -s 2>/dev/null || hostname)"
  if [[ -n "${KVM_HOST:-}" && "${_ve_local_host}" == "${KVM_HOST%%.*}" ]]; then
    log "already on KVM_HOST (${KVM_HOST}); dropping ProxyJump for direct CP SSH (#362)"
    unset KVM_HOST
  fi
  unset _ve_local_host

  CP_NAME="${CP_NAME:-hummingbird-k8s}"
  cp_ip="$(resolve_cp_ip "$CP_NAME" "${CP_IP:-}")" || {
    log "could not resolve control-plane IP; set CP_IP=<ip> or KVM_HOST=<ssh-alias>"
    exit 2
  }

  # ssh options: no SSH_PRIVKEY_FILE in scope — rely on agent / ssh_config,
  # matching the verify-hardening.sh pattern. ProxyJump=$KVM_HOST so the
  # libvirt NAT subnet (192.168.122.0/24 by default) doesn't have to be
  # routable from the workstation.
  if [[ -n "${KVM_HOST:-}" ]]; then
    ssh_opts_array_no_identity SSH_OPTS --proxy-jump="${KVM_HOST}"
  else
    ssh_opts_array_no_identity SSH_OPTS
  fi

  log "remote mode: ssh root@${cp_ip}${KVM_HOST:+ via ${KVM_HOST}}"
  # Forward EXPECTED_PREFIX so rotation-time callers can keep their
  # stricter `aesgcm:v1:<new-key>:` assertion. The remote
  # /usr/libexec/verify-encryption.sh is this same script body baked
  # into the image at build time (see containers/k8s/Containerfile).
  exec ssh "${SSH_OPTS[@]}" "root@${cp_ip}" \
    "EXPECTED_PREFIX='${EXPECTED_PREFIX}' /usr/libexec/verify-encryption.sh"
fi

# ──────────────────────────────────────────────────────────────────────
# Local mode: we're on the CP. Run kubectl/crictl/etcdctl directly.
# ──────────────────────────────────────────────────────────────────────
KUBECONFIG="${KUBECONFIG:-/etc/kubernetes/admin.conf}"
export KUBECONFIG

PKI=/etc/kubernetes/pki/etcd
ETCD_ENDPOINT="${ETCD_ENDPOINT:-https://127.0.0.1:2379}"

log() { printf '[verify-encryption] %s\n' "$*" >&2; }

cleanup() {
  local rc=$?
  kubectl -n "$NS" delete secret "$PROBE_NAME" --ignore-not-found=true \
    --wait=false >/dev/null 2>&1 || true
  exit "$rc"
}
trap cleanup EXIT

if ! command -v kubectl >/dev/null 2>&1; then
  log "kubectl not on PATH (run on the CP, or set KVM_HOST=<ssh-alias> to ssh to one)"
  exit 2
fi

if ! kubectl get --raw=/healthz >/dev/null 2>&1; then
  log "kubectl can't reach the API server (KUBECONFIG=$KUBECONFIG)"
  log "  hint: set KVM_HOST=<ssh-alias> so the script ssh's to root@<CP_IP> and runs /usr/libexec/verify-encryption.sh"
  exit 2
fi

log "creating probe secret $NS/$PROBE_NAME"
kubectl -n "$NS" create secret generic "$PROBE_NAME" \
  --from-literal="$PROBE_KEY=$PROBE_VALUE" >/dev/null

ETCD_KEY="/registry/secrets/${NS}/${PROBE_NAME}"

read_etcd_local() {
  ETCDCTL_API=3 etcdctl \
    --endpoints="$ETCD_ENDPOINT" \
    --cacert="$PKI/ca.crt" \
    --cert="$PKI/server.crt" \
    --key="$PKI/server.key" \
    get "$ETCD_KEY" --print-value-only
}

read_etcd_via_crictl() {
  # k8s 1.31's etcd static pod image is distroless: no /bin/sh, so
  # `kubectl exec etcd-$node -- sh -c '...'` exits with "exec: ...: not
  # found in $PATH". Instead we go through the host CRI: find the etcd
  # container ID via crictl and exec etcdctl in it directly (no shell).
  local etcd_id
  etcd_id="$(crictl ps --name '^etcd$' -q 2>/dev/null | head -n1)"
  if [[ -z "$etcd_id" ]]; then
    log "could not find running etcd container via crictl"
    return 1
  fi
  crictl exec "$etcd_id" etcdctl \
    --endpoints="$ETCD_ENDPOINT" \
    --cacert=/etc/kubernetes/pki/etcd/ca.crt \
    --cert=/etc/kubernetes/pki/etcd/server.crt \
    --key=/etc/kubernetes/pki/etcd/server.key \
    get "$ETCD_KEY" --print-value-only
}

raw=""
if command -v etcdctl >/dev/null 2>&1 && [[ -r "$PKI/ca.crt" ]]; then
  log "reading via local etcdctl"
  raw="$(read_etcd_local || true)"
elif command -v crictl >/dev/null 2>&1; then
  log "local etcdctl unavailable, falling back to crictl exec on etcd container"
  raw="$(read_etcd_via_crictl || true)"
else
  log "FAIL: need etcdctl or crictl on PATH (run as root on the CP VM)"
  log "      manual workaround: sudo crictl ps | grep etcd ; sudo crictl exec <id> etcdctl ..."
  exit 1
fi

if [[ -z "$raw" ]]; then
  log "FAIL: could not read $ETCD_KEY from etcd"
  exit 1
fi

# `raw` is binary; the encryption envelope prefix is plain ASCII at the
# start, so a substring check is sufficient and robust.
if [[ "$raw" == ${EXPECTED_PREFIX}* ]]; then
  log "OK: secret in etcd is encrypted (prefix=${EXPECTED_PREFIX})"
  exit 0
fi

# If the literal probe value is in the blob, encryption is off.
if printf '%s' "$raw" | grep -q -- "$PROBE_VALUE"; then
  log "FAIL: probe value is present in plaintext in etcd"
  exit 1
fi

log "FAIL: unexpected etcd blob (no $EXPECTED_PREFIX prefix)"
exit 1
