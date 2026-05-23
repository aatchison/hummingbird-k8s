#!/usr/bin/env bash
# destroy-cluster.sh — Tear down a cluster that was deployed via
# deploy-cluster.sh, using the same config file. Destroys + undefines every
# CP_NAME and WORKER_NAMES domain, removes its qcow2 + seed ISO from the
# libvirt pool. Idempotent: missing VMs are not an error.
#
# Usage: sudo bash scripts/destroy-cluster.sh <path-to-cluster.local.conf>

set -euo pipefail

log() { printf '[destroy-cluster] %s\n' "$*" >&2; }
fail() { log "ERROR: $*"; exit 1; }

CONFIG_PATH="${1:?usage: $0 <cluster.local.conf>}"
[[ -r "$CONFIG_PATH" ]] || fail "config not readable: $CONFIG_PATH"

# Required commands.
command -v virsh >/dev/null 2>&1 || fail "virsh not on PATH"
[[ $EUID -eq 0 ]] || fail "must run as root (libvirt qemu:///system)"

# shellcheck disable=SC1090
source "$CONFIG_PATH"

# Sensible defaults if the operator's config omits the libvirt pool dir.
POOL_DIR="${POOL_DIR:-/var/lib/libvirt/images}"

: "${CP_NAME:?CP_NAME not set in $CONFIG_PATH}"
# WORKER_NAMES may be absent or empty — treat as no workers.
declare -a WORKER_NAMES_ARR=("${WORKER_NAMES[@]:-}")

destroy_vm() {
  local name="$1"
  [[ -n "$name" ]] || return 0
  if virsh -c qemu:///system dominfo "$name" >/dev/null 2>&1; then
    log "destroying $name"
    virsh -c qemu:///system destroy "$name" >/dev/null 2>&1 || true
    virsh -c qemu:///system undefine --nvram "$name" >/dev/null 2>&1 || true
  else
    log "$name: no such domain (already torn down)"
  fi
  # Clean disk + seed ISO regardless. Trailing-glob covers any clone variants.
  rm -f "${POOL_DIR}/${name}.qcow2" \
        "${POOL_DIR}/${name}-seed.iso" \
        "${POOL_DIR}/${name}-cloud-init.iso" 2>/dev/null || true
}

log "tearing down cluster defined in $CONFIG_PATH"
destroy_vm "$CP_NAME"
for w in "${WORKER_NAMES_ARR[@]}"; do
  destroy_vm "$w"
done

# Also clean the per-deploy scratch dir under the pool, used by
# deploy-cluster.sh to hold transient cloud-init payloads.
if [[ -d "${POOL_DIR}/deploy-cluster" ]]; then
  log "removing scratch dir ${POOL_DIR}/deploy-cluster"
  rm -rf "${POOL_DIR}/deploy-cluster" || true
fi

log "done."
