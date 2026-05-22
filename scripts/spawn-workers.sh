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

cd "$(dirname "$(readlink -f "$0")")/.."
# shellcheck source=../lib/build-common.sh
source lib/build-common.sh

: "${CP_VM_NAME:=hummingbird-k8s}"
: "${TOKEN_TTL:=2h}"

# Per-worker resource knobs (override via config.local.sh or environment).
# Defaults match the pre-knob hardcoded values so behavior is unchanged
# when unset. See config.example.sh for the full list.
: "${WORKER_MEMORY:=4096}"
: "${WORKER_VCPUS:=2}"

# CP-readiness retry tuning (#90). SSH to the CP can fail when the control
# plane is still coming up; retry rather than spawning a worker with a
# missing join token. Override via env if you need to wait longer.
: "${CP_SSH_RETRIES:=5}"
: "${CP_SSH_RETRY_SLEEP:=10}"

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
#
# Retries up to CP_SSH_RETRIES times because the CP can still be coming up
# when redo-workers.sh chains here right after redo-k8s.sh (#90). Each
# attempt has its own ConnectTimeout=10s; on success we emit the join
# command on stdout, on persistent failure we exit non-zero so the caller
# aborts rather than spawning a worker with no join token.
mint_join_command() {
  local attempt cmd=""
  for attempt in $(seq 1 "$CP_SSH_RETRIES"); do
    if cmd=$(sudo -u "$SUDO_USER" ssh \
                -o StrictHostKeyChecking=accept-new \
                -o ConnectTimeout=10 \
                -o BatchMode=yes \
                "root@${CP_IP}" \
                "kubeadm token create --ttl ${TOKEN_TTL} --print-join-command" \
                2>/dev/null) && [[ -n "$cmd" ]]; then
      printf '%s\n' "$cmd"
      return 0
    fi
    if (( attempt < CP_SSH_RETRIES )); then
      echo "mint_join_command: attempt ${attempt}/${CP_SSH_RETRIES} to ${CP_IP} failed; retrying in ${CP_SSH_RETRY_SLEEP}s..." >&2
      sleep "$CP_SSH_RETRY_SLEEP"
    fi
  done
  echo "mint_join_command: could not reach CP at ${CP_IP} after ${CP_SSH_RETRIES} attempts" >&2
  return 1
}

# Write the join command into the given qcow2 at /etc/hummingbird/worker-join.env.
#
# The Hummingbird worker image is bootc/ostree-based, so neither libguestfs
# OS introspection (`-i`, used by virt-customize and `guestfish -i`) works.
# Both bail with "no operating systems were found in the guest image" because
# the on-disk layout puts the rootfs under /ostree/deploy/<stateroot>/deploy/
# <commit>.0/, not at the partition root. At runtime the kernel mounts that
# deployment dir as /, with its /etc bind-merged on top.
#
# We sidestep introspection entirely: open the qcow2 with guestfish, mount
# /dev/sda4 (the 'root' partition) manually, locate the active deployment
# dir, and write into <deployment>/etc/hummingbird/worker-join.env. That
# path becomes /etc/hummingbird/worker-join.env on the booted guest.
inject_join_env() {
  local qcow="$1" join_cmd="$2" tmpfile
  tmpfile="$(mktemp)"
  printf '%s\n' "$join_cmd" > "$tmpfile"
  case "$INJECTOR" in
    guestfish)
      # Discover the active deployment dir from the qcow2 itself so we don't
      # hard-code per-build hashes. We expect exactly one stateroot (typically
      # 'default') and one deployment dir (named '<commit-sha>.0') inside it.
      local stateroot deploy_basename etc_path
      stateroot=$(guestfish --ro -a "$qcow" <<'GF' 2>/dev/null | grep -v '^$' | head -1
run
mount /dev/sda4 /
ls /ostree/deploy
GF
)
      if [[ -n "$stateroot" ]]; then
        deploy_basename=$(guestfish --ro -a "$qcow" <<GF 2>/dev/null | grep -v '\.origin$' | grep -v '^$' | head -1
run
mount /dev/sda4 /
ls /ostree/deploy/${stateroot}/deploy
GF
)
      fi
      if [[ -n "$stateroot" && -n "$deploy_basename" ]]; then
        etc_path="/ostree/deploy/${stateroot}/deploy/${deploy_basename}/etc/hummingbird"
      else
        # Non-bootc layout (or unexpected): write to the partition root's /etc,
        # which is the live /etc on traditional (non-ostree) images.
        etc_path="/etc/hummingbird"
      fi
      guestfish --rw -a "$qcow" <<EOF
run
mount /dev/sda4 /
mkdir-p ${etc_path}
upload ${tmpfile} ${etc_path}/worker-join.env
chmod 0600 ${etc_path}/worker-join.env
chown 0 0 ${etc_path}/worker-join.env
EOF
      ;;
    virt-customize)
      virt-customize -a "$qcow" \
        --mkdir /etc/hummingbird \
        --upload "${tmpfile}:/etc/hummingbird/worker-join.env" \
        --run-command 'chmod 0600 /etc/hummingbird/worker-join.env' \
        --run-command 'chown root:root /etc/hummingbird/worker-join.env' \
        >/dev/null
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
  # Don't let `set -e` abort before we clean up the half-cloned QCOW (#90):
  # capture the rc explicitly and rm the staged disk on failure.
  if ! JOIN_CMD="$(mint_join_command)"; then
    echo "ERROR: mint_join_command failed for $NAME; removing staged $QCOW." >&2
    rm -f "$QCOW"
    exit 1
  fi
  if ! grep -q '^kubeadm join' <<<"$JOIN_CMD"; then
    echo "ERROR: did not get a valid 'kubeadm join' command from CP at ${CP_IP}." >&2
    echo "Got: $JOIN_CMD" >&2
    rm -f "$QCOW"
    exit 1
  fi

  inject_join_env "$QCOW" "$JOIN_CMD"

  virt-install --connect qemu:///system \
    --name "$NAME" \
    --memory "$WORKER_MEMORY" --vcpus "$WORKER_VCPUS" \
    --disk "$QCOW",format=qcow2,bus=virtio \
    --import \
    --os-variant fedora-unknown \
    --network network=default,model=virtio \
    --graphics vnc,listen=127.0.0.1 \
    --noautoconsole
  echo "Spawned $NAME"
done

virsh -c qemu:///system pool-refresh mass2 >/dev/null || true
