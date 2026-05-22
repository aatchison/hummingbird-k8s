#!/usr/bin/env bash
# Clones the worker template qcow2 into N copies, mints a fresh short-TTL
# kubeadm join token per VM, injects it into the VM's qcow2 at
# /etc/hummingbird/worker-join.env, then virt-installs each.
#
# Usage: sudo bash spawn-workers.sh [count]
#
# See docs/worker-tokens.md for the design rationale (no static long-lived
# token baked into the published worker image).
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo." >&2
  exit 1
fi

: "${SUDO_USER:?must be invoked via sudo so ssh uses the calling user known_hosts/key}"

cd "$(dirname "$(readlink -f "$0")")"
# shellcheck source=lib/build-common.sh
source lib/build-common.sh

: "${CP_VM_NAME:=hummingbird-k8s}"
: "${TOKEN_TTL:=2h}"

COUNT="${1:-2}"
: "${POOL_DIR:=/var/lib/libvirt/images}"
TEMPLATE="${POOL_DIR}/hummingbird-k8s-worker.qcow2"

[[ -r "$TEMPLATE" ]] || { echo "Missing template $TEMPLATE — run build-worker.sh first." >&2; exit 1; }

# Resolve the control plane IP so we can ask it for fresh join tokens.
CP_IP=$(virsh -c qemu:///system domifaddr "$CP_VM_NAME" 2>/dev/null \
          | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}' || true)
if [[ -z "$CP_IP" ]]; then
  echo "Could not resolve IP of CP VM '$CP_VM_NAME' via virsh domifaddr." >&2
  echo "Is the control plane running? Set CP_VM_NAME=... if you renamed it." >&2
  exit 1
fi

# Ensure we have a tool capable of mutating the qcow2's filesystem out-of-band.
# Prefer guestfish: it mounts the raw root partition and writes files without
# needing libguestfs OS introspection, which fails on bootc/ostree images
# ("no operating systems were found in the guest image"). virt-customize is
# the fallback for non-bootc layouts.
INJECTOR=""
if command -v guestfish >/dev/null 2>&1; then
  INJECTOR=guestfish
elif command -v virt-customize >/dev/null 2>&1; then
  INJECTOR=virt-customize
else
  echo "Neither guestfish nor virt-customize found; attempting to install libguestfs-tools-c..." >&2
  if command -v dnf >/dev/null 2>&1; then
    dnf install -y libguestfs-tools-c >/dev/null 2>&1 || \
      dnf install -y libguestfs-tools >/dev/null 2>&1 || true
  fi
  if command -v guestfish >/dev/null 2>&1; then
    INJECTOR=guestfish
  elif command -v virt-customize >/dev/null 2>&1; then
    INJECTOR=virt-customize
  else
    echo "ERROR: guestfish/virt-customize unavailable and could not be installed." >&2
    echo "Install libguestfs-tools-c (or libguestfs-tools) on this KVM host and retry." >&2
    exit 1
  fi
fi

# Mint a fresh kubeadm join token from the CP via SSH and echo the full join
# command (one line, starts with 'kubeadm join ...').
#
# We SSH as root (rather than the unprivileged VM_USER + sudo) because the
# core/wheel-user on the bootc image cannot sudo without a password, and
# because kubeadm requires root to read /etc/kubernetes/pki. The CP image
# ships with ENABLE_ROOT_SSH=1 by default; see docs/worker-tokens.md.
mint_join_command() {
  sudo -u "$SUDO_USER" ssh \
    -o StrictHostKeyChecking=accept-new \
    -o ConnectTimeout=10 \
    "root@${CP_IP}" \
    "kubeadm token create --ttl ${TOKEN_TTL} --print-join-command"
}

# Write the join command into the given qcow2 at /etc/hummingbird/worker-join.env.
inject_join_env() {
  local qcow="$1" join_cmd="$2" tmpfile
  tmpfile="$(mktemp)"
  printf '%s\n' "$join_cmd" > "$tmpfile"
  case "$INJECTOR" in
    virt-customize)
      virt-customize -a "$qcow" \
        --mkdir /etc/hummingbird \
        --upload "${tmpfile}:/etc/hummingbird/worker-join.env" \
        --run-command 'chmod 0600 /etc/hummingbird/worker-join.env' \
        --run-command 'chown root:root /etc/hummingbird/worker-join.env' \
        >/dev/null
      ;;
    guestfish)
      guestfish --rw -a "$qcow" -i <<EOF
mkdir-p /etc/hummingbird
upload ${tmpfile} /etc/hummingbird/worker-join.env
chmod 0600 /etc/hummingbird/worker-join.env
chown 0 0 /etc/hummingbird/worker-join.env
EOF
      ;;
  esac
  rm -f "$tmpfile"
}

for i in $(seq 1 "$COUNT"); do
  NAME="hummingbird-k8s-worker-${i}"
  QCOW="${POOL_DIR}/${NAME}.qcow2"

  if virsh -c qemu:///system dominfo "$NAME" >/dev/null 2>&1; then
    echo "Already defined: $NAME"
    virsh -c qemu:///system start "$NAME" 2>/dev/null || true
    continue
  fi

  cp --reflink=auto "$TEMPLATE" "$QCOW"
  chown root:root "$QCOW"
  chmod 0644 "$QCOW"

  echo "Minting fresh ${TOKEN_TTL}-TTL join token for $NAME..."
  JOIN_CMD="$(mint_join_command)"
  if ! grep -q '^kubeadm join' <<<"$JOIN_CMD"; then
    echo "ERROR: did not get a valid 'kubeadm join' command from CP at ${CP_IP}." >&2
    echo "Got: $JOIN_CMD" >&2
    rm -f "$QCOW"
    exit 1
  fi

  inject_join_env "$QCOW" "$JOIN_CMD"

  virt-install --connect qemu:///system \
    --name "$NAME" \
    --memory 4096 --vcpus 2 \
    --disk "$QCOW",format=qcow2,bus=virtio \
    --import \
    --os-variant fedora-unknown \
    --network network=default,model=virtio \
    --graphics vnc,listen=127.0.0.1 \
    --noautoconsole
  echo "Spawned $NAME"
done

virsh -c qemu:///system pool-refresh mass2 >/dev/null || true
