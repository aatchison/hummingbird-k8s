#!/usr/bin/env bash
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo. Defines under qemu:///system." >&2
  exit 1
fi

NAME=hummingbird-k8s
QCOW=/mnt/mass2/vms/${NAME}.qcow2

[[ -r "$QCOW" ]] || { echo "Missing/unreadable: $QCOW" >&2; exit 1; }

virsh -c qemu:///system pool-refresh mass2 >/dev/null || true

if virsh -c qemu:///system dominfo "$NAME" >/dev/null 2>&1; then
  echo "Domain $NAME already defined; starting."
  virsh -c qemu:///system start "$NAME" 2>/dev/null || true
  exit 0
fi

virt-install --connect qemu:///system \
  --name "$NAME" \
  --memory 8192 --vcpus 4 \
  --disk "$QCOW",format=qcow2,bus=virtio \
  --import \
  --os-variant fedora-unknown \
  --network network=default,model=virtio \
  --graphics vnc,listen=127.0.0.1 \
  --noautoconsole
