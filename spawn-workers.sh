#!/usr/bin/env bash
# Clones the worker template qcow2 into N copies and virt-installs each.
# Usage: sudo bash spawn-workers.sh [count]
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo." >&2
  exit 1
fi

COUNT="${1:-2}"
POOL_DIR=/mnt/mass2/vms
TEMPLATE="${POOL_DIR}/hummingbird-k8s-worker.qcow2"

[[ -r "$TEMPLATE" ]] || { echo "Missing template $TEMPLATE — run build-worker.sh first." >&2; exit 1; }

for i in $(seq 1 "$COUNT"); do
  NAME="hummingbird-k8s-worker-${i}"
  QCOW="${POOL_DIR}/${NAME}.qcow2"

  if virsh -c qemu:///system dominfo "$NAME" >/dev/null 2>&1; then
    echo "Already defined: $NAME"
    virsh -c qemu:///system start "$NAME" 2>/dev/null || true
    continue
  fi

  cp --reflink=auto "$TEMPLATE" "$QCOW"
  chown root:root "$QCOW"
  chmod 0644 "$QCOW"

  virt-install --connect qemu:///system \
    --name "$NAME" \
    --memory 4096 --vcpus 2 \
    --disk "$QCOW",format=qcow2,bus=virtio \
    --import \
    --os-variant fedora-unknown \
    --network network=default,model=virtio \
    --graphics vnc,listen=127.0.0.1 \
    --noautoconsole
  echo "Spawned $NAME"
done

virsh -c qemu:///system pool-refresh mass2 >/dev/null || true
