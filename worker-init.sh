#!/bin/bash
set -euo pipefail

MARKER=/var/lib/worker-init.done
[[ -f "$MARKER" ]] && { echo "worker-init already ran"; exit 0; }

JOIN_CMD_FILE=/etc/hummingbird/worker-join.env
[[ -s "$JOIN_CMD_FILE" ]] || { echo "Missing $JOIN_CMD_FILE"; exit 1; }

swapoff -a || true
modprobe overlay
modprobe br_netfilter
sysctl --system >/dev/null

# Unique-ish hostname so kubelet doesn't claim "localhost.localdomain" on every
# worker (kubeadm uses the local hostname as the node name).
SUFFIX=$(cut -c1-8 /etc/machine-id)
hostnamectl set-hostname "humbird-worker-${SUFFIX}"

# Wait for cri-o socket
for _ in $(seq 1 30); do
  [[ -S /var/run/crio/crio.sock ]] && break
  sleep 1
done

# Execute the join command exactly as the CP printed it.
bash -c "$(cat "$JOIN_CMD_FILE") --cri-socket=unix:///var/run/crio/crio.sock"

touch "$MARKER"
