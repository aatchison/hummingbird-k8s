#!/usr/bin/env bash
# Wipe the current hummingbird-k3s VM (and the legacy `hummingbird` name)
# and rebuild from scratch.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "${0##*/}: must be run as root — wipes + redefines the hummingbird-k3s VM under libvirt qemu:///system. Try: sudo bash $0" >&2
  exit 1
fi

# Sit in scripts/ so we can invoke our sibling scripts by relative name.
cd "$(dirname "$(readlink -f "$0")")"

_ROOT="$(cd .. && pwd)"
# shellcheck disable=SC1091
[[ -r "${_ROOT}/config.local.sh" ]] && source "${_ROOT}/config.local.sh"
: "${POOL_DIR:=/var/lib/libvirt/images}"

for name in hummingbird-k3s hummingbird; do
  virsh -c qemu:///system destroy "$name" 2>/dev/null || true
  virsh -c qemu:///system undefine "$name" 2>/dev/null || true
done
rm -f "${POOL_DIR}/hummingbird-k3s.qcow2" "${POOL_DIR}/hummingbird.qcow2"

bash build-k3s.sh
bash define-vm.sh

# Fresh VMs install from `localhost/hummingbird-k3s:latest` (the local podman
# build), which means the bootc auto-update timer has no remote to pull from.
# Switch the new VM to track the GHCR-published image so subsequent
# `bootc upgrade`s (manual or via the timer) actually do something. See #138.
# Set BOOTC_SWITCH_TO_GHCR=0 to skip (offline lab use).
bash switch-to-ghcr.sh hummingbird-k3s ghcr.io/aatchison/hummingbird-k3s:latest || \
  echo "WARN: bootc switch failed; VM still tracks localhost:latest" >&2
