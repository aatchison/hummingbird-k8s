#!/usr/bin/env bash
# Shared SSH/virsh/log helpers live in lib/build-common.sh; see docs/development.md.
# destroy-cluster.sh — Tear down a cluster that was deployed via
# deploy-cluster.sh, using the same config file. Destroys + undefines every
# CP_NAME and WORKER_NAMES domain, removes its qcow2 + seed ISO from the
# libvirt pool. Idempotent: missing VMs are not an error.
#
# Usage: sudo bash scripts/destroy-cluster.sh <path-to-cluster.local.conf>

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Source-only mode for bats: when HBIRD_DESTROY_CLUSTER_SOURCE_ONLY=1,
# return from `source` here so tests can inspect helpers without
# triggering the SSH-wrap or libvirt orchestration. (C3, #232.)
if [[ "${HBIRD_DESTROY_CLUSTER_SOURCE_ONLY:-0}" = 1 ]]; then
  return 0
fi

# ---- Remote KVM-host re-exec shim (C3, #232) -------------------------------
# When KVM_HOST is set and we're NOT on the KVM host, re-exec this script
# on the remote host via SSH. See scripts/lib/ssh-wrap.sh for the contract.
# shellcheck source=lib/ssh-wrap.sh
source "${SCRIPT_DIR}/lib/ssh-wrap.sh"
hbird_ssh_wrap_maybe_reexec "$0" "$@"
# ---- End remote re-exec shim -----------------------------------------------

# shellcheck source=../lib/build-common.sh
source "${REPO_ROOT}/lib/build-common.sh"
setup_logging "[destroy-cluster]"

CONFIG_PATH="${1:?usage: $0 <cluster.local.conf>}"
[[ -r "$CONFIG_PATH" ]] || fail "config not readable: $CONFIG_PATH"

# Required commands.
command -v virsh >/dev/null 2>&1 || fail "virsh not on PATH"
# destroy-cluster.sh historically required root for libvirt qemu:///system +
# rm of root-owned qcow2/seed files in POOL_DIR. With #305 we accept either
# root OR a member of the `libvirt` group on the KVM host (mirrors #272's
# update-cluster pattern). libvirt authorizes qemu:///system via the
# unix-socket group, not sudo; POOL_DIR files deployed via the no-sudo path
# (#305) land as <operator>:libvirt and are removable by the operator. Files
# left over from a pre-#305 sudo-driven deploy are owned by root and will
# fail to remove for a non-root operator — that's a one-time migration hazard,
# diagnosed via the rm errors below. Non-root + not in libvirt group is a
# hard fail with an actionable hint.
if [[ $EUID -ne 0 ]]; then
  if ! id -nG 2>/dev/null | tr ' ' '\n' | grep -qx libvirt; then
    fail "must be root or a member of the libvirt group on this host. Add yourself with:
  sudo usermod -aG libvirt \$USER && newgrp libvirt
then rerun. See docs/deploy-cluster.md#running-without-sudo for the full no-sudo path."
  fi
fi

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
