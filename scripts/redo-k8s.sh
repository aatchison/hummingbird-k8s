#!/usr/bin/env bash
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "${0##*/}: must be run as root — wipes + redefines the hummingbird-k8s VM under libvirt qemu:///system. Try: sudo bash $0" >&2
  exit 1
fi

cd "$(dirname "$(readlink -f "$0")")"

virsh -c qemu:///system destroy hummingbird-k8s 2>/dev/null || true
virsh -c qemu:///system undefine hummingbird-k8s 2>/dev/null || true
rm -f /mnt/mass2/vms/hummingbird-k8s.qcow2

bash build-k8s.sh
bash define-vm-k8s.sh

# Switch the freshly-installed CP from `localhost/hummingbird-k8s:latest` to
# the GHCR-published image so the auto-update timer (and manual `bootc
# upgrade`s) have a remote to pull from. See #138. The CP image ships with
# the auto-update timer disabled by default (#48), but a manual upgrade
# still needs a remote ref. Set BOOTC_SWITCH_TO_GHCR=0 to skip.
bash switch-to-ghcr.sh hummingbird-k8s ghcr.io/aatchison/hummingbird-k8s:latest || \
  echo "WARN: bootc switch failed; VM still tracks localhost:latest" >&2
