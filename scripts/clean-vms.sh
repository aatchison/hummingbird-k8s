#!/usr/bin/env bash
# clean-vms.sh — destroy + undefine all hummingbird-* libvirt domains,
# and sweep stale qcow2 + seed-ISO files from POOL_DIR.
#
# Wrapped by the C3 SSH-wrap shim (scripts/lib/ssh-wrap.sh) — when
# KVM_HOST is set and we're not already on that host, the shim re-execs
# this script on $KVM_HOST so the operator can run `make clean-vms`
# from a workstation with no local libvirt. See issue #271 (F5).
#
# Folds in #221: post-destroy, rm any leftover hummingbird-*.qcow2,
# *-seed.iso, and *-cloud-init.iso under $POOL_DIR (pre-#216 layout
# stragglers that no cluster.local.conf still references), then
# pool-refresh so libvirt drops them from the volumes catalog.
#
# Idempotent: missing domains and missing files are not an error.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"

# Source-only mode for bats: when HBIRD_CLEAN_VMS_SOURCE_ONLY=1, return
# from `source` here so tests can inspect the script without triggering
# the SSH-wrap or libvirt sweep.
if [[ "${HBIRD_CLEAN_VMS_SOURCE_ONLY:-0}" = 1 ]]; then
  return 0
fi

# ---- Remote KVM-host re-exec shim (C3, #232) -------------------------------
# When KVM_HOST is set and we're NOT on the KVM host, re-exec this script
# on the remote host via SSH. The shim runs the remote leg under `sudo
# env ...` so the body below executes as root on the KVM host. See
# scripts/lib/ssh-wrap.sh for the full contract.
# shellcheck source=lib/ssh-wrap.sh
source "${SCRIPT_DIR}/lib/ssh-wrap.sh"
hbird_ssh_wrap_maybe_reexec "$0" "$@"
# ---- End remote re-exec shim -----------------------------------------------

# Past the shim, we're either on the KVM host directly (operator typed
# `make clean-vms` on geary) or the shim re-exec'd us under sudo. In the
# direct-local case the operator did NOT type `sudo make clean-vms` (the
# Makefile recipe no longer prefixes sudo), so self-elevate.
if [[ $EUID -ne 0 ]]; then
  exec sudo -E "$0" "$@"
fi

: "${POOL_DIR:=/var/lib/libvirt/images}"
: "${POOL_NAME:=default}"

# 1. Destroy + undefine all hummingbird-* domains.
mapfile -t domains < <(virsh -c qemu:///system list --all --name 2>/dev/null \
                       | grep '^hummingbird-' || true)
for d in "${domains[@]}"; do
  [[ -z "$d" ]] && continue
  echo "[clean-vms] destroying $d"
  virsh -c qemu:///system destroy   "$d" 2>/dev/null || true
  virsh -c qemu:///system undefine  "$d" --remove-all-storage 2>/dev/null || true
done

# 2. Sweep straggler qcow2 + seed-ISO files under POOL_DIR (#221).
#    `rm -f` keeps the sweep idempotent on a clean host. nullglob so an
#    unmatched glob doesn't leave a literal pattern on the rm argv.
shopt -s nullglob
stragglers=(
  "$POOL_DIR"/hummingbird-*.qcow2
  "$POOL_DIR"/*-seed.iso
  "$POOL_DIR"/*-cloud-init.iso
)
for f in "${stragglers[@]}"; do
  echo "[clean-vms] removing $f"
  rm -f "$f"
done
shopt -u nullglob

# 3. Refresh libvirt's view of POOL_NAME so the volumes catalog drops
#    the rm'd files. Default pool name is "default"; honor POOL_NAME
#    override for non-standard pool setups.
virsh -c qemu:///system pool-refresh "$POOL_NAME" >/dev/null 2>&1 || true

echo "[clean-vms] done."
