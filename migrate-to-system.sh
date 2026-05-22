#!/usr/bin/env bash
# One-shot: move an existing hummingbird VM out of qemu:///session and
# redefine it under qemu:///system with the default NAT network.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo." >&2
  exit 1
fi
: "${SUDO_USER:?must be invoked via sudo so we can reach the caller session libvirt}"

NAME=hummingbird
OLD_QCOW=/mnt/mass2/vms/userspace/${NAME}.qcow2
NEW_QCOW=/mnt/mass2/vms/${NAME}.qcow2

# 1. Tear down the session-mode VM if it exists.
if sudo -u "$SUDO_USER" virsh -c qemu:///session dominfo "$NAME" >/dev/null 2>&1; then
  sudo -u "$SUDO_USER" virsh -c qemu:///session destroy "$NAME" 2>/dev/null || true
  sudo -u "$SUDO_USER" virsh -c qemu:///session undefine "$NAME"
fi

# 2. Move the qcow2 into the mass2 system pool (same filesystem; mv is instant).
if [[ -f "$OLD_QCOW" && ! -f "$NEW_QCOW" ]]; then
  mv "$OLD_QCOW" "$NEW_QCOW"
  rmdir /mnt/mass2/vms/userspace 2>/dev/null || true
fi
chown root:root "$NEW_QCOW"
chmod 0644 "$NEW_QCOW"

virsh -c qemu:///system pool-refresh mass2 >/dev/null || true

# 3. Define under system libvirt.
exec "$(dirname "$(readlink -f "$0")")/define-vm.sh"
