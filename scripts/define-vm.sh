#!/usr/bin/env bash
set -euo pipefail

_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
# shellcheck disable=SC1091
[[ -r "${_ROOT}/config.local.sh" ]] && source "${_ROOT}/config.local.sh"
: "${POOL_DIR:=/var/lib/libvirt/images}"

if [[ $EUID -ne 0 ]]; then
  echo "${0##*/}: must be run as root — defines the VM under libvirt qemu:///system. Try: sudo bash $0" >&2
  exit 1
fi

NAME=hummingbird-k3s
QCOW=${POOL_DIR}/${NAME}.qcow2

# Resource knobs (override via config.local.sh or environment). Sizes match
# the pre-knob hardcoded defaults so behavior is unchanged when unset.
# See config.example.sh for the full list.
: "${K3S_MEMORY:=6144}"
: "${K3S_VCPUS:=4}"

[[ -r "$QCOW" ]] || {
  echo "${0##*/}: qcow2 missing or unreadable: $QCOW" >&2
  echo "${0##*/}: build the image first ('sudo make k3s' or 'sudo bash scripts/build-k3s.sh'), or override POOL_DIR if your pool lives elsewhere." >&2
  exit 1
}

virsh -c qemu:///system pool-refresh mass2 >/dev/null || true

if virsh -c qemu:///system dominfo "$NAME" >/dev/null 2>&1; then
  echo "Domain $NAME already defined under system; starting it."
  virsh -c qemu:///system start "$NAME" 2>/dev/null || true
  exit 0
fi

virt-install --connect qemu:///system \
  --name "$NAME" \
  --memory "$K3S_MEMORY" --vcpus "$K3S_VCPUS" \
  --disk "$QCOW",format=qcow2,bus=virtio \
  --import \
  --os-variant fedora-unknown \
  --network network=default,model=virtio \
  --graphics vnc,listen=127.0.0.1 \
  --noautoconsole
