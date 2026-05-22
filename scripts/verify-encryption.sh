#!/bin/bash
# verify-encryption.sh — Verify that etcd at-rest encryption is active.
#
# Strategy:
#   1. Create a probe Secret with a distinctive value via kubectl.
#   2. Read it raw from etcd, in this order:
#        a. `etcdctl` on PATH (rare on bootc CP), OR
#        b. host-side `crictl exec` into the etcd container (k8s 1.31's
#           etcd static pod is distroless, so `kubectl exec ... -- sh` is
#           never available — must go through crictl on the host).
#   3. Assert the stored blob starts with the expected envelope prefix
#      (`k8s:enc:aesgcm:` for AESGCM).
#   4. Clean up the probe Secret either way.
#
# Run as root on the control-plane VM:
#   sudo ./scripts/verify-encryption.sh

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
  log "kubectl not on PATH"
  exit 2
fi

if ! kubectl get --raw=/healthz >/dev/null 2>&1; then
  log "kubectl can't reach the API server (KUBECONFIG=$KUBECONFIG)"
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
