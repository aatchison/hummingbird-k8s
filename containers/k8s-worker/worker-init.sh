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
# If cloud-init's hostname module already set a meaningful hostname (e.g. from
# a NoCloud seed's local-hostname / #cloud-config hostname: directive), defer
# to it — clobbering it here would override the operator's per-VM declaration
# and surface workers in the cluster under humbird-worker-<machine-id>
# instead of the names cloud-init was told to use (#186, #254).
#
# Resolution rules (#254):
#
#   1. Wait for cloud-init to finish all stages so its hostname module has
#      definitively committed (or definitively skipped) any user-data /
#      meta-data hostname. We can't use `After=cloud-init.target` on the
#      service (it creates an ordering cycle vs multi-user.target — see the
#      service unit comment), so we block at runtime here. Best-effort only:
#      if cloud-init isn't installed (the legacy spawn-workers.sh path) the
#      command is absent and we skip cleanly.
#
#   2. Prefer the PERSISTENT hostname (`hostnamectl --static`, which reads
#      /etc/hostname) over the running kernel hostname. In #254 we observed
#      cloud-init writing /etc/hostname correctly while the running kernel
#      hostname stayed at "localhost.localdomain" because systemd-hostnamed
#      reads /etc/hostname only at boot — before cloud-init's hostname
#      module ran. Reading `hostnamectl --static` picks up cloud-init's
#      write even when `hostname` (kernel) is stale.
#
#   3. If the persistent hostname IS a meaningful name, re-assert it on the
#      running kernel (no-op if already correct, fixes #254 stale-kernel case).
#
#   4. Only fall back to humbird-worker-<machine-id> when the persistent
#      hostname is the localhost default OR empty (the legacy spawn-workers
#      path, which does not seed cloud-init user-data).
if command -v cloud-init >/dev/null 2>&1; then
  cloud-init status --wait >/dev/null 2>&1 || true
fi
static_hostname="$(hostnamectl --static 2>/dev/null || true)"
# Fall back to /etc/hostname directly, then to `hostname`, if hostnamectl
# is missing/erroring (unlikely on Fedora bootc but cheap to guard).
if [[ -z "$static_hostname" && -r /etc/hostname ]]; then
  static_hostname="$(tr -d '[:space:]' < /etc/hostname)"
fi
if [[ -z "$static_hostname" ]]; then
  static_hostname="$(hostname)"
fi
if [[ -z "$static_hostname" || "$static_hostname" == "localhost" || "$static_hostname" == "localhost."* ]]; then
  SUFFIX=$(cut -c1-8 /etc/machine-id)
  hostnamectl set-hostname "humbird-worker-${SUFFIX}"
elif [[ "$(hostname)" != "$static_hostname" ]]; then
  # cloud-init committed a name to /etc/hostname but the running kernel
  # hostname is stale (#254). Re-assert so kubeadm join uses the
  # operator-declared name.
  hostnamectl set-hostname "$static_hostname"
fi

# Wait for cri-o socket
for _ in $(seq 1 30); do
  [[ -S /var/run/crio/crio.sock ]] && break
  sleep 1
done

# Execute the join command exactly as the CP printed it.
bash -c "$(cat "$JOIN_CMD_FILE") --cri-socket=unix:///var/run/crio/crio.sock"

touch "$MARKER"
