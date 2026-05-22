#!/usr/bin/env bash
# Convenience wrapper: kubectl against the hummingbird-k8s VM via SSH tunnel
# through the KVM host. No local kubectl install required.
#
# Env:
#   KVM_HOST      — SSH alias of the KVM host (required). Pulled from config.local.sh if present.
#   KCFG          — Path to a kubeconfig with `server: https://localhost:6443` (default: /tmp/k8s-kubeconfig)
#   LOCAL_PORT    — Local port for the tunnel (default: 6443)
#   VM_NAME       — libvirt domain of the control plane (default: hummingbird-k8s)
set -euo pipefail

_KK_REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
if [[ -r "${_KK_REPO_ROOT}/config.local.sh" ]]; then
  # shellcheck disable=SC1091
  source "${_KK_REPO_ROOT}/config.local.sh"
fi

: "${KCFG:=/tmp/k8s-kubeconfig}"
: "${LOCAL_PORT:=6443}"
: "${VM_NAME:=hummingbird-k8s}"
: "${KVM_HOST:?Set KVM_HOST (SSH alias of the KVM host) — see config.example.sh}"

if ! ss -ltn | grep -q "127.0.0.1:${LOCAL_PORT} "; then
  VM_IP=$(ssh "$KVM_HOST" "sudo virsh -c qemu:///system domifaddr ${VM_NAME}" \
            | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}')
  [[ -n "$VM_IP" ]] || { echo "Could not find ${VM_NAME} IP" >&2; exit 1; }
  echo "Starting tunnel: localhost:${LOCAL_PORT} -> ${VM_IP}:6443 via ${KVM_HOST}" >&2
  ssh -fNL "${LOCAL_PORT}:${VM_IP}:6443" "$KVM_HOST"
fi

[[ -f "$KCFG" ]] || {
  echo "Missing $KCFG. Pull /etc/kubernetes/admin.conf from the VM and rewrite server: -> localhost:${LOCAL_PORT}" >&2
  exit 1
}

exec podman run --rm --net=host \
  -v "${KCFG}:/kc:ro,Z" \
  -e KUBECONFIG=/kc \
  docker.io/bitnami/kubectl:latest "$@"
