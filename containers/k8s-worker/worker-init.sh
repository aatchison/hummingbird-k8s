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

# ---- Node identity pinning --------------------------------------------------
# Pin kubelet's --node-ip to the primary NIC's address BEFORE the join.
# Without it kubelet autodetects, and on a dual-NIC VM (EXTRA_NETWORK in
# deploy-cluster.sh) that autodetection is not guaranteed to pick the
# primary NIC — observed live: kubelet selected the second NIC's static
# address as InternalIP even though that NIC carried no default route.
#
# Derivation: the `src` of the default route — the primary NIC BY
# CONSTRUCTION (the second NIC's cloud-init network-config ships no
# gateway/DHCP, and its IPv6 is disabled by a first-boot runcmd, so it can
# never own a default route). Using the route's own `src` rather than the
# interface's first address matches what kubelet/kubeadm autodetection
# would pick when the NIC carries several global addresses.
#
# Written to /etc/sysconfig/kubelet — the kubelet unit's EnvironmentFile.
# This is the right home because the worker joins via a plain CLI
# `kubeadm join` (no JoinConfiguration to carry kubeletExtraArgs), NOT
# because kubeadm regenerates kubeadm-flags.env (it does not: kubeadm
# v1.31 rewrites only /var/lib/kubelet/config.yaml on upgrade).
#
# NODE_IP may be pre-set in /etc/hummingbird/worker-init-local.env
# (cloud-init write_files lands it before this service runs), mirroring
# the CP's k8s-init-local.env contract.
if [[ -r /etc/hummingbird/worker-init-local.env ]]; then
  # shellcheck disable=SC1091
  source /etc/hummingbird/worker-init-local.env
fi

if [[ -z "${NODE_IP:-}" ]]; then
  # Bounded retry: at first boot the DHCP lease and default route may not
  # have landed yet, and this unit is Type=oneshot with no Restart — a
  # single-shot check would leave the node permanently unjoined.
  for _ in $(seq 1 30); do
    NODE_IP="$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}')"
    [[ -n "$NODE_IP" ]] && break
    sleep 1
  done
fi
if [[ -z "${NODE_IP:-}" ]]; then
  # Fall back to the default-route interface's first global address for
  # routes that carry no src hint.
  _def_if="$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')"
  [[ -n "$_def_if" ]] && NODE_IP="$(ip -4 -o addr show dev "$_def_if" scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"
fi
if [[ -z "${NODE_IP:-}" ]]; then
  echo "FATAL: could not derive NODE_IP (no IPv4 default route after 30s, or no global IPv4 on its interface). Set NODE_IP= in /etc/hummingbird/worker-init-local.env to override." >&2
  exit 1
fi

# Merge rather than truncate: /etc/sysconfig/kubelet is kubeadm's
# documented operator escape hatch, and an operator may have seeded
# KUBELET_EXTRA_ARGS via cloud-init write_files (which lands before this
# service). Our --node-ip goes FIRST so an explicit operator --node-ip
# later in the string still wins under pflag last-wins.
#
# Parsed textually rather than by sourcing: systemd EnvironmentFile takes
# the whole rest of the line as the value, but `source` on an unquoted
# multi-flag line (KUBELET_EXTRA_ARGS=--node-ip=X --max-pods=200) assigns
# only the first token and tries to EXECUTE the rest — silently dropping
# every other operator flag. We write the value quoted for the same reason.
_existing=""
if [[ -r /etc/sysconfig/kubelet ]]; then
  _existing="$(sed -n 's/^[[:space:]]*KUBELET_EXTRA_ARGS=//p' /etc/sysconfig/kubelet | tail -1)"
  if [[ "$_existing" == \"*\" || "$_existing" == \'*\' ]]; then
    _existing="${_existing:1:${#_existing}-2}"
  fi
  # Drop any pre-existing --node-ip so ours is unambiguous.
  _existing="$(sed -E 's/--node-ip=[^ ]*//g; s/  +/ /g; s/^ //; s/ $//' <<<"$_existing")"
fi
printf 'KUBELET_EXTRA_ARGS="--node-ip=%s%s"\n' "$NODE_IP" "${_existing:+ ${_existing}}" > /etc/sysconfig/kubelet
echo "worker node identity pinned to ${NODE_IP} (kubelet --node-ip via /etc/sysconfig/kubelet)"

# Unique-ish hostname so kubelet doesn't claim "localhost.localdomain" on every
# worker (kubeadm uses the local hostname as the node name).
#
# If cloud-init's hostname module already set a meaningful hostname (e.g. from
# a NoCloud seed's local-hostname / #cloud-config hostname: directive), defer
# to it — clobbering it here would override the operator's per-VM declaration
# and surface workers in the cluster under humbird-worker-<machine-id>
# instead of the names cloud-init was told to use (#186, #254).
#
# Resolution rules (#254, revised for #265):
#
#   1. Read the PERSISTENT hostname directly via `hostnamectl --static`
#      (which reads /etc/hostname). cloud-init's hostname module runs at
#      the network/init stage, which completes BEFORE multi-user.target
#      activates. worker-init.service is WantedBy=multi-user.target, so by
#      the time this script runs /etc/hostname already has cloud-init's
#      seeded value and `hostnamectl --static` returns it.
#
#      We deliberately do NOT call `cloud-init status --wait`: it blocks
#      on cloud-final.service, which has After=multi-user.target, which
#      is itself blocked by us. Classic three-way deadlock — PR #255
#      introduced exactly that and PR #265 removed it (same bug class as
#      PR #171/#172/#173 fixed on the k8s-init path).
#
#   2. Prefer the PERSISTENT hostname over the running kernel hostname.
#      In #254 we observed cloud-init writing /etc/hostname correctly
#      while the running kernel hostname stayed at "localhost.localdomain"
#      because systemd-hostnamed reads /etc/hostname only at boot — before
#      cloud-init's hostname module ran. Reading `hostnamectl --static`
#      picks up cloud-init's write even when `hostname` (kernel) is stale.
#
#   3. If the persistent hostname IS a meaningful name, re-assert it on the
#      running kernel (no-op if already correct, fixes #254 stale-kernel case).
#
#   4. Only fall back to humbird-worker-<machine-id> when the persistent
#      hostname is the localhost default OR empty (the legacy spawn-workers
#      path, which does not seed cloud-init user-data).
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
