#!/usr/bin/env bash
# Wipe any prior workers, rebuild template, spawn N fresh workers.
# Usage: sudo bash redo-workers.sh [count]
#
# Note: `set -o pipefail` is REQUIRED here. Operators commonly invoke this
# script piped through tee (e.g. `redo-workers.sh 2 2>&1 | tee out.log`),
# and without pipefail a failure inside spawn-workers.sh would be masked by
# tee's exit 0 and the whole train would silently succeed with no workers
# defined. See issue #34.
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
# spawn-workers.sh runs scripts/switch-to-ghcr.sh per worker once each VM
# has booted, so freshly-spawned workers track ghcr.io/aatchison/
# hummingbird-k8s-worker:latest rather than the install-time
# `localhost/...:latest`. See #138.
bash spawn-workers.sh "$COUNT"

# Explicit "we got here" sentinel so an operator piping through tee can
# eyeball success without having to chase $PIPESTATUS.
echo "redo-workers.sh: spawned $COUNT worker(s) successfully"
