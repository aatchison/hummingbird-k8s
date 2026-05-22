#!/usr/bin/env bash
# Wipe any prior workers, rebuild template, spawn N fresh workers.
# Usage: sudo bash redo-workers.sh [count]
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo." >&2
  exit 1
fi

cd "$(dirname "$(readlink -f "$0")")"

COUNT="${1:-2}"

# Remove any existing worker VMs and their per-instance qcow2s.
for d in $(virsh -c qemu:///system list --all --name 2>/dev/null | grep '^hummingbird-k8s-worker'); do
  virsh -c qemu:///system destroy "$d" 2>/dev/null || true
  virsh -c qemu:///system undefine "$d" 2>/dev/null || true
done
rm -f /mnt/mass2/vms/hummingbird-k8s-worker-*.qcow2 /mnt/mass2/vms/hummingbird-k8s-worker.qcow2

bash build-worker.sh
bash spawn-workers.sh "$COUNT"
