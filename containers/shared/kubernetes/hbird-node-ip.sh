#!/bin/bash
# hbird-node-ip.sh — re-derive kubelet's --node-ip on every boot.
#
# WHY: k8s-init.sh / worker-init.sh pin --node-ip once, at first boot, and
# are then short-circuited forever by their done-markers. The primary NIC
# takes a DHCP lease, so if that lease ever changes (host reboot with a
# lost dnsmasq lease-db, or a VM powered off past lease expiry while
# another claims its address) kubelet would start with a --node-ip
# matching no local interface and fail to register — NotReady until a
# human intervenes.
#
# Pinning the primary NIC's MAC and adding a DHCP reservation (see
# deploy-cluster.sh) makes that drift unlikely. This unit makes an
# already-drifted node heal itself instead of needing that human.
#
# Scope: kubelet's --node-ip only. A control-plane node whose address
# moves ALSO has a stale advertise-address in
# /etc/kubernetes/manifests/kube-apiserver.yaml and stale apiserver cert
# SANs; neither is safely fixable from a boot script, so we log loudly
# and leave that to the operator. Keeping kubelet able to register is
# still strictly better than not.
set -euo pipefail

SYSCONFIG=/etc/sysconfig/kubelet

# Operator override wins, same contract as the init scripts.
for f in /etc/hummingbird/k8s-init-local.env /etc/hummingbird/worker-init-local.env; do
  if [[ -r "$f" ]]; then
    # shellcheck disable=SC1090
    source "$f"
  fi
done

if [[ -z "${NODE_IP:-}" ]]; then
  # Bounded wait: this runs before kubelet, and the DHCP lease may not
  # have landed yet on a cold boot.
  for _ in $(seq 1 30); do
    NODE_IP="$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}')"
    [[ -n "$NODE_IP" ]] && break
    sleep 1
  done
fi
if [[ -z "${NODE_IP:-}" ]]; then
  _def_if="$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')"
  [[ -n "$_def_if" ]] && NODE_IP="$(ip -4 -o addr show dev "$_def_if" scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"
fi
if [[ -z "${NODE_IP:-}" ]]; then
  echo "hbird-node-ip: could not derive a node IP; leaving ${SYSCONFIG} untouched" >&2
  exit 0   # never block kubelet from starting
fi

# What is pinned right now?
#
# Parsed textually, NOT by sourcing: systemd EnvironmentFile takes the whole
# rest of the line as the value, but `source` on an unquoted multi-flag line
# (KUBELET_EXTRA_ARGS=--node-ip=X --max-pods=200) assigns only the first
# token and then tries to EXECUTE the rest — silently losing every other
# operator flag. We therefore also always write the value QUOTED below, so
# systemd and any future shell reader agree.
CURRENT=""
if [[ -r "$SYSCONFIG" ]]; then
  CURRENT="$(sed -n 's/^[[:space:]]*KUBELET_EXTRA_ARGS=//p' "$SYSCONFIG" | tail -1)"
  # Strip one layer of matching surrounding quotes, as systemd would.
  if [[ "$CURRENT" == \"*\" || "$CURRENT" == \'*\' ]]; then
    CURRENT="${CURRENT:1:${#CURRENT}-2}"
  fi
fi
PINNED="$(grep -o -- '--node-ip=[^ ]*' <<<"$CURRENT" | head -1 | cut -d= -f2 || true)"

if [[ "$PINNED" == "$NODE_IP" ]]; then
  exit 0   # nothing to do, the common case
fi

# Preserve every other flag the operator (or a previous run) set.
REST="$(sed -E 's/--node-ip=[^ ]*//g; s/  +/ /g; s/^ //; s/ $//' <<<"$CURRENT")"
printf 'KUBELET_EXTRA_ARGS="--node-ip=%s%s"\n' "$NODE_IP" "${REST:+ ${REST}}" > "$SYSCONFIG"

if [[ -n "$PINNED" ]]; then
  echo "hbird-node-ip: node IP drifted ${PINNED} -> ${NODE_IP}; updated ${SYSCONFIG}"
  if [[ -f /etc/kubernetes/manifests/kube-apiserver.yaml ]]; then
    echo "hbird-node-ip: WARNING this is a control-plane node — kube-apiserver.yaml still advertises ${PINNED} and the apiserver cert SANs still name it. Kubelet will register on ${NODE_IP}, but the control plane needs manual attention (see docs)." >&2
  fi
else
  echo "hbird-node-ip: pinned node IP ${NODE_IP} in ${SYSCONFIG}"
fi
