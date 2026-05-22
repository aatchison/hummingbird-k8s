#!/bin/bash
# verify-encryption.sh — Verify that etcd at-rest encryption is active.
#
# Strategy:
#   1. Create a probe Secret with a distinctive value via kubectl.
#   2. Read it raw from etcd (either via `etcdctl` on PATH or via
#      `kubectl exec` into the static etcd pod).
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

read_etcd_via_pod() {
  local node etcd_pod
  node="$(kubectl get nodes \
    -l node-role.kubernetes.io/control-plane \
    -o jsonpath='{.items[0].metadata.name}')"
  if [[ -z "$node" ]]; then
    log "no control-plane node found"
    return 1
  fi
  etcd_pod="etcd-${node}"
  kubectl -n kube-system exec "$etcd_pod" -- sh -c "
    ETCDCTL_API=3 etcdctl \
      --endpoints=$ETCD_ENDPOINT \
      --cacert=/etc/kubernetes/pki/etcd/ca.crt \
      --cert=/etc/kubernetes/pki/etcd/server.crt \
      --key=/etc/kubernetes/pki/etcd/server.key \
      get '$ETCD_KEY' --print-value-only
  "
}

raw=""
if command -v etcdctl >/dev/null 2>&1 && [[ -r "$PKI/ca.crt" ]]; then
  log "reading via local etcdctl"
  raw="$(read_etcd_local || true)"
else
  log "local etcdctl unavailable, falling back to kubectl exec"
  raw="$(read_etcd_via_pod || true)"
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
