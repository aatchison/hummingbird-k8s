#!/usr/bin/env bash
# Wipe the current hummingbird-k3s VM (and the legacy `hummingbird` name)
# and rebuild from scratch.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo." >&2
  exit 1
fi

cd "$(dirname "$(readlink -f "$0")")"

for name in hummingbird-k3s hummingbird; do
  virsh -c qemu:///system destroy "$name" 2>/dev/null || true
  virsh -c qemu:///system undefine "$name" 2>/dev/null || true
done
rm -f /mnt/mass2/vms/hummingbird-k3s.qcow2 /mnt/mass2/vms/hummingbird.qcow2

bash build.sh
bash define-vm.sh
