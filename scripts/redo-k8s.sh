#!/usr/bin/env bash
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo." >&2
  exit 1
fi

cd "$(dirname "$(readlink -f "$0")")"

virsh -c qemu:///system destroy hummingbird-k8s 2>/dev/null || true
virsh -c qemu:///system undefine hummingbird-k8s 2>/dev/null || true
rm -f /mnt/mass2/vms/hummingbird-k8s.qcow2

bash build-k8s.sh
bash define-vm-k8s.sh
