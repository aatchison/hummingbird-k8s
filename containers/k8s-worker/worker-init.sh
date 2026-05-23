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

# cloud-init's write_files/network/config stages all run BEFORE
# multi-user.target — which is when this service starts — so user-data
# write_files (e.g. worker-join.env) and hostname have already landed.
# cloud-final.service (runcmd, packages) runs AFTER multi-user.target
# by design; we do NOT wait for it (waiting deadlocks against
# multi-user.target itself — see #171, #172).


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
#
# If cloud-init's hostname module already set a meaningful hostname during the
# network stage (e.g. from a NoCloud seed's local-hostname / #cloud-config
# hostname: directive), defer to it — clobbering it here would override the
# operator's per-VM declaration and surface workers in the cluster under
# humbird-worker-<machine-id> instead of the names cloud-init was told to use
# (#186). Only fall back to the machine-id-derived name when the current
# hostname is the localhost default (the legacy spawn-workers.sh path, which
# does not seed cloud-init user-data).
current_hostname="$(hostname)"
if [[ "$current_hostname" == "localhost" || "$current_hostname" == "localhost."* ]]; then
  SUFFIX=$(cut -c1-8 /etc/machine-id)
  hostnamectl set-hostname "humbird-worker-${SUFFIX}"
fi

# Wait for cri-o socket
for _ in $(seq 1 30); do
  [[ -S /var/run/crio/crio.sock ]] && break
  sleep 1
done

# Execute the join command exactly as the CP printed it.
bash -c "$(cat "$JOIN_CMD_FILE") --cri-socket=unix:///var/run/crio/crio.sock"

touch "$MARKER"
