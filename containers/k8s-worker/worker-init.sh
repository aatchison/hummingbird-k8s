#!/bin/bash
set -euo pipefail

# Regenerate SSH host keys on first boot so two workers built from the
# same image don't share identical host keys (#80). Runs BEFORE the
# done-marker short-circuit and BEFORE the join logic — once a worker
# has joined we never want a key swap to surprise an operator's
# known_hosts.
SSH_HOSTKEY_MARKER=/var/lib/ssh-host-keys-regenerated
if [[ ! -f "$SSH_HOSTKEY_MARKER" ]]; then
  rm -f /etc/ssh/ssh_host_*
  ssh-keygen -A
  systemctl restart sshd
  touch "$SSH_HOSTKEY_MARKER"
fi

MARKER=/var/lib/worker-init.done
[[ -f "$MARKER" ]] && { echo "worker-init already ran"; exit 0; }

JOIN_CMD_FILE=/etc/hummingbird/worker-join.env
if [[ ! -s "$JOIN_CMD_FILE" ]]; then
  echo "Missing or empty $JOIN_CMD_FILE." >&2
  echo "Did spawn-workers.sh inject the per-VM kubeadm join token into this" >&2
  echo "qcow2 before virt-install? The published template image intentionally" >&2
  echo "ships without a token; see docs/worker-tokens.md." >&2
  exit 1
fi

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
