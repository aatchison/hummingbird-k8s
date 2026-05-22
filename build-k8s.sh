#!/usr/bin/env bash
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo." >&2
  exit 1
fi
: "${SUDO_USER:?must be invoked via sudo}"

POOL_DIR=/mnt/mass2/vms
BASE_IMAGE=quay.io/hummingbird-community/bootc-os:latest
LOCAL_IMAGE=localhost/hummingbird-k8s:latest
BIB=quay.io/centos-bootc/bootc-image-builder:latest
NAME=hummingbird-k8s
QCOW="${POOL_DIR}/${NAME}.qcow2"
PASSWORD='1234asdf'

cd "$(dirname "$(readlink -f "$0")")"

JUDAH_KEY='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIM7TZJS33tTD5V/cikpDYd39V9yOA2R+cvM4f/lkjqas aatchison@fedora'
HOST_KEY="$(cat "/home/${SUDO_USER}/.ssh/id_ed25519.pub")"
PWHASH="$(openssl passwd -6 "$PASSWORD")"

printf '[[customizations.user]]\nname = "aatchison"\npassword = "%s"\ngroups = ["wheel"]\nkey = """%s\n%s"""\n' \
  "$PWHASH" "$JUDAH_KEY" "$HOST_KEY" > bib-config-k8s.toml

podman pull "$BASE_IMAGE"
podman build -t "$LOCAL_IMAGE" -f Containerfile.k8s .

rm -rf "${POOL_DIR}/qcow2"
mkdir -p "$POOL_DIR"

podman run --rm --privileged --pull=newer \
  --security-opt label=type:unconfined_t \
  -v "$(pwd)/bib-config-k8s.toml:/config.toml:ro" \
  -v "${POOL_DIR}:/output" \
  -v /var/lib/containers/storage:/var/lib/containers/storage \
  "$BIB" \
  --type qcow2 --rootfs ext4 \
  --local \
  "$LOCAL_IMAGE"

mv -f "${POOL_DIR}/qcow2/disk.qcow2" "$QCOW"
rmdir "${POOL_DIR}/qcow2"
chown root:root "$QCOW"
chmod 0644 "$QCOW"

virsh -c qemu:///system pool-refresh mass2 >/dev/null || true
echo "Built: $QCOW"
