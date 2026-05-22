#!/usr/bin/env bash
# Builds the worker image. Refreshes the worker-join.env from the running CP
# (so we always bake a current, non-expiring token).
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo." >&2
  exit 1
fi
: "${SUDO_USER:?must be invoked via sudo}"

POOL_DIR=/mnt/mass2/vms
BASE_IMAGE=quay.io/hummingbird-community/bootc-os:latest
LOCAL_IMAGE=localhost/hummingbird-k8s-worker:latest
BIB=quay.io/centos-bootc/bootc-image-builder:latest
NAME=hummingbird-k8s-worker
QCOW="${POOL_DIR}/${NAME}.qcow2"
PASSWORD='1234asdf'

cd "$(dirname "$(readlink -f "$0")")"

JUDAH_KEY='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIM7TZJS33tTD5V/cikpDYd39V9yOA2R+cvM4f/lkjqas aatchison@fedora'
HOST_KEY="$(cat "/home/${SUDO_USER}/.ssh/id_ed25519.pub")"
PWHASH="$(openssl passwd -6 "$PASSWORD")"

# Refresh the join command from the currently-running CP. Falls back to the
# checked-in worker-join.env if the CP is unreachable.
CP_IP=$(virsh -c qemu:///system domifaddr hummingbird-k8s 2>/dev/null \
          | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}')
if [[ -n "$CP_IP" ]]; then
  sudo -u "$SUDO_USER" ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 \
    "aatchison@${CP_IP}" \
    "echo '$PASSWORD' | sudo -S kubeadm token create --ttl 0 --print-join-command 2>/dev/null" \
    > worker-join.env.new
  if [[ -s worker-join.env.new ]] && grep -q '^kubeadm join' worker-join.env.new; then
    mv worker-join.env.new worker-join.env
    echo "Refreshed worker-join.env from CP at $CP_IP"
  else
    rm -f worker-join.env.new
    echo "WARN: could not refresh from CP; using existing worker-join.env"
  fi
fi
[[ -s worker-join.env ]] || { echo "No worker-join.env"; exit 1; }

printf '[[customizations.user]]\nname = "aatchison"\npassword = "%s"\ngroups = ["wheel"]\nkey = """%s\n%s"""\n' \
  "$PWHASH" "$JUDAH_KEY" "$HOST_KEY" > bib-config-worker.toml

podman pull "$BASE_IMAGE"
podman build -t "$LOCAL_IMAGE" -f Containerfile.k8s-worker .

rm -rf "${POOL_DIR}/qcow2"
mkdir -p "$POOL_DIR"

podman run --rm --privileged --pull=newer \
  --security-opt label=type:unconfined_t \
  -v "$(pwd)/bib-config-worker.toml:/config.toml:ro" \
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
echo "Built worker template: $QCOW"
