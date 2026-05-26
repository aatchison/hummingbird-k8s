#!/usr/bin/env bash
# Convenience wrapper: kubectl against the hummingbird-k8s VM via SSH tunnel
# through the KVM host. Prefers a native `kubectl` on PATH; falls back to a
# podman-run kubectl container for hosts without kubectl installed.
#
# Env:
#   CONFIG        — Optional path to cluster.local.conf. When set, sourced
#                   so CP_NAME / KVM_HOST come from the topology file
#                   (matches the `make get-kubeconfig` / `make update-cluster`
#                   pattern). Falls through to config.local.sh + defaults
#                   if unset.
#   KVM_HOST      — SSH alias of the KVM host (required). Pulled from
#                   CONFIG or config.local.sh if present.
#   KCFG          — Path to a kubeconfig with `server: https://localhost:6443` (default: /tmp/k8s-kubeconfig)
#   LOCAL_PORT    — Local port for the tunnel (default: 6443)
#   CP_NAME       — libvirt domain of the control plane (default: hummingbird-k8s).
#                   VM_NAME is honored as a backward-compat alias.
set -euo pipefail

_KK_REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
# Prefer CONFIG (cluster topology) when supplied — that's where CP_NAME
# now lives post-#216. Fall back to config.local.sh (image-build inputs),
# which historically carried VM_NAME for single-VM deploys.
if [[ -n "${CONFIG:-}" ]]; then
  [[ -r "$CONFIG" ]] || { echo "${0##*/}: CONFIG not readable: $CONFIG" >&2; exit 2; }
  # shellcheck disable=SC1090
  source "$CONFIG"
elif [[ -r "${_KK_REPO_ROOT}/config.local.sh" ]]; then
  # shellcheck disable=SC1091
  source "${_KK_REPO_ROOT}/config.local.sh"
fi

: "${KCFG:=/tmp/k8s-kubeconfig}"
: "${LOCAL_PORT:=6443}"
# Operator-visible deprecation signal when only the legacy name is set.
# The alias below still resolves, but we want a one-line warning so the
# operator notices to migrate to CP_NAME.
if [[ -n "${VM_NAME:-}" && -z "${CP_NAME:-}" ]]; then
  echo "${0##*/}: warning: VM_NAME is deprecated; use CP_NAME instead (see PR #219)" >&2
fi
# Align on CP_NAME (used by cluster.local.conf, deploy-cluster.sh, et al);
# preserve VM_NAME as a backward-compat alias so operator shell history
# from before this rename keeps working. See PR #219 round-1 review (H3).
: "${CP_NAME:=${VM_NAME:-hummingbird-k8s}}"
: "${KVM_HOST:?Set KVM_HOST (SSH alias of the KVM host) — see config.example.sh}"

if ! ss -ltn | grep -q "127.0.0.1:${LOCAL_PORT} "; then
  # ssh -t: sudo on the KVM host needs a TTY to prompt for the operator's
  # password when the remote sudo cache is cold. Without -t the call fails
  # with `sudo: a terminal is required to read the password`. The remote
  # TTY makes ssh inject \r into the captured stdout, so strip carriage
  # returns before awk parses the IP. See issue #247.
  VM_IP=$(ssh -t "$KVM_HOST" "sudo virsh -c qemu:///system domifaddr ${CP_NAME}" \
            | tr -d '\r' \
            | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}')
  [[ -n "$VM_IP" ]] || { echo "Could not find ${CP_NAME} IP" >&2; exit 1; }
  echo "Starting tunnel: localhost:${LOCAL_PORT} -> ${VM_IP}:6443 via ${KVM_HOST}" >&2
  ssh -fNL "${LOCAL_PORT}:${VM_IP}:6443" "$KVM_HOST"
fi

[[ -f "$KCFG" ]] || {
  echo "Missing $KCFG. Pull /etc/kubernetes/admin.conf from the VM and rewrite server: -> localhost:${LOCAL_PORT}" >&2
  exit 1
}

# Prefer native kubectl when available. It reads the kubeconfig directly
# (0600 is fine — no world-readable chmod dance needed for a container UID),
# and stdin (heredocs, `apply -f -`) flows through without a podman wrapper.
if command -v kubectl >/dev/null 2>&1; then
  exec kubectl --kubeconfig "$KCFG" "$@"
fi

# Fallback: container kubectl (kept for hosts without kubectl installed).
exec podman run --rm --net=host \
  -v "${KCFG}:/kc:ro,Z" \
  -e KUBECONFIG=/kc \
  docker.io/bitnami/kubectl:latest "$@"
