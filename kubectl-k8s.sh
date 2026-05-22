#!/usr/bin/env bash
# Convenience wrapper: kubectl against the hummingbird-k8s VM, from judah.
# Auto-starts an SSH tunnel through geary if one isn't already running.
set -euo pipefail

REMOTE_HOST=thegeary
KCFG=/tmp/k8s-kubeconfig
LOCAL_PORT=6443

if ! ss -ltn | grep -q "127.0.0.1:${LOCAL_PORT} "; then
  VM_IP=$(ssh "$REMOTE_HOST" 'sudo virsh -c qemu:///system domifaddr hummingbird-k8s' \
            | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}')
  [[ -n "$VM_IP" ]] || { echo "Could not find hummingbird-k8s IP" >&2; exit 1; }
  echo "Starting tunnel: localhost:${LOCAL_PORT} -> ${VM_IP}:6443 via ${REMOTE_HOST}" >&2
  ssh -fNL "${LOCAL_PORT}:${VM_IP}:6443" "$REMOTE_HOST"
fi

[[ -f "$KCFG" ]] || {
  echo "Missing $KCFG. Pull /etc/kubernetes/admin.conf from the VM and rewrite server: -> localhost:${LOCAL_PORT}" >&2
  exit 1
}

exec podman run --rm --net=host \
  -v "${KCFG}:/kc:ro,Z" \
  -e KUBECONFIG=/kc \
  docker.io/bitnami/kubectl:latest "$@"
