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

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"

# Source-only mode for bats: when HBIRD_SPAWN_WORKERS_SOURCE_ONLY=1,
# return from `source` here so tests can inspect helpers without
# triggering the SSH-wrap or libvirt orchestration. (C3, #232.)
if [[ "${HBIRD_SPAWN_WORKERS_SOURCE_ONLY:-0}" = 1 ]]; then
  return 0
fi

# ---- Remote KVM-host re-exec shim (C3, #232) -------------------------------
# When KVM_HOST is set and we're NOT on the KVM host, re-exec this script
# on the remote host via SSH. Placed BEFORE the EUID/SUDO_USER checks so
# the client never needs sudo locally — sudo happens on the remote.
# See scripts/lib/ssh-wrap.sh for the contract.
# shellcheck source=lib/ssh-wrap.sh
source "${SCRIPT_DIR}/lib/ssh-wrap.sh"
hbird_ssh_wrap_maybe_reexec "$0" "$@"
# ---- End remote re-exec shim -----------------------------------------------

if [[ $EUID -ne 0 ]]; then
  echo "${0##*/}: must be run as root — clones the worker qcow2 + virt-installs under qemu:///system. Try: sudo bash $0 [count]" >&2
  exit 1
fi

: "${SUDO_USER:?must be invoked via sudo so ssh uses the calling user known_hosts/key}"

cd "${SCRIPT_DIR}/.."
# Opt into autoloading config.local.sh for POOL_DIR / image-build inputs.
# VM-sizing knobs (WORKER_MEMORY / WORKER_VCPUS) come from cluster.local.conf
# via the CONFIG= path below (see cluster.example.conf). See
# docs/development.md for the HBIRD_AUTOLOAD_CONFIG_LOCAL flag rationale.
export HBIRD_AUTOLOAD_CONFIG_LOCAL=1
# shellcheck source=../lib/build-common.sh
source lib/build-common.sh

# Optional cluster-topology config (cluster.local.conf). When supplied,
# overrides CP_NAME / WORKER_MEMORY / WORKER_VCPUS / POOL_DIR with the
# same values the deploy-cluster flow used — operators no longer need
# to keep two copies in sync. Matches the `make get-kubeconfig` /
# `make update-cluster` CONFIG= pattern.
if [[ -n "${CONFIG:-}" ]]; then
  [[ -r "$CONFIG" ]] || { echo "${0##*/}: CONFIG not readable: $CONFIG" >&2; exit 2; }
  # shellcheck disable=SC1090
  source "$CONFIG"
fi

# Operator-visible deprecation signal when only the legacy name is set.
# The alias below still resolves, but we want a one-line warning so the
# operator notices to migrate to CP_NAME (cluster.local.conf naming).
if [[ -n "${CP_VM_NAME:-}" && -z "${CP_NAME:-}" ]]; then
  echo "${0##*/}: warning: CP_VM_NAME is deprecated; use CP_NAME instead (see PR #219)" >&2
fi
# Align on CP_NAME (used by cluster.local.conf, deploy-cluster.sh, et al);
# preserve CP_VM_NAME as a backward-compat alias so operator shell history
# from before this rename keeps working. See PR #219 round-1 review (H3).
: "${CP_NAME:=${CP_VM_NAME:-hummingbird-k8s}}"
: "${TOKEN_TTL:=2h}"

# Per-worker resource knobs (override via cluster.local.conf — see
# cluster.example.conf — or environment). Defaults match the pre-knob
# hardcoded values so behavior is unchanged when unset.
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

[[ -r "$TEMPLATE" ]] || {
  echo "${0##*/}: worker template qcow2 missing or unreadable: $TEMPLATE" >&2
  echo "${0##*/}: build it first via 'sudo bash scripts/build-worker.sh' (or run 'make deploy-cluster CONFIG=…' to build + spawn together)." >&2
  exit 1
}

# Resolve the control plane IP so we can ask it for fresh join tokens.
CP_IP=$(virsh -c qemu:///system domifaddr "$CP_NAME" 2>/dev/null \
          | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}' || true)
if [[ -z "$CP_IP" ]]; then
  echo "${0##*/}: could not resolve IP of CP VM '$CP_NAME' via 'virsh -c qemu:///system domifaddr'." >&2
  echo "${0##*/}: verify the CP is running ('virsh -c qemu:///system list'), or override the VM name with CP_NAME=<name> (CP_VM_NAME also honored as a back-compat alias)." >&2
  echo "${0##*/}: known domains: $(virsh -c qemu:///system list --all --name 2>/dev/null | tr '\n' ' ')" >&2
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
    echo "${0##*/}: guestfish/virt-customize unavailable and auto-install failed." >&2
    echo "${0##*/}: install libguestfs-tools-c (Fedora/RHEL) or libguestfs-tools (Debian/Ubuntu) on this KVM host and retry." >&2
    echo "${0##*/}: an injector is required to write /etc/hummingbird/worker-join.env into each worker qcow2 before boot." >&2
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
# when deploy-cluster.sh chains here right after a fresh CP boot (#90). Each
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
      if ! guestfish --rw -a "$qcow" <<EOF
run
mount /dev/sda4 /
mkdir-p ${etc_path}
upload ${tmpfile} ${etc_path}/worker-join.env
chmod 0600 ${etc_path}/worker-join.env
chown 0 0 ${etc_path}/worker-join.env
EOF
      then
        echo "${0##*/}: inject_join_env: guestfish failed to write ${etc_path}/worker-join.env into $qcow" >&2
        echo "${0##*/}: hints: (a) /dev/sda4 is the expected bootc 'root' partition — confirm with 'virt-filesystems --partitions -a $qcow'; (b) stateroot='${stateroot:-<unresolved>}' deploy='${deploy_basename:-<unresolved>}'; (c) ensure no other process has $qcow open." >&2
        rm -f "$tmpfile"
        return 1
      fi
      ;;
    virt-customize)
      if ! virt-customize -a "$qcow" \
        --mkdir /etc/hummingbird \
        --upload "${tmpfile}:/etc/hummingbird/worker-join.env" \
        --run-command 'chmod 0600 /etc/hummingbird/worker-join.env' \
        --run-command 'chown root:root /etc/hummingbird/worker-join.env' \
        >/dev/null; then
        echo "${0##*/}: inject_join_env: virt-customize failed to write /etc/hummingbird/worker-join.env into $qcow" >&2
        echo "${0##*/}: hint: virt-customize uses libguestfs OS-introspection ('-i'), which fails on bootc/ostree images. Install guestfish on this host to use the bootc-aware code path." >&2
        rm -f "$tmpfile"
        return 1
      fi
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

  if ! inject_join_env "$QCOW" "$JOIN_CMD"; then
    echo "${0##*/}: ERROR: inject_join_env failed for $NAME; removing staged $QCOW so a retry starts clean." >&2
    rm -f "$QCOW"
    exit 1
  fi

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

# Each freshly-spawned worker tracks `localhost/hummingbird-k8s-worker:latest`
# (the local podman build), so the bootc auto-update timer has nothing to
# pull from GHCR. Switch each worker to the GHCR-published image so updates
# actually flow. See #138. Best-effort per worker: if one fails (e.g. GHCR
# ref hasn't been published yet for this flavor) we continue with the rest.
# Set BOOTC_SWITCH_TO_GHCR=0 to skip.
#
# FORCE_REBUILD opt-out (#375): when the operator set FORCE_REBUILD=1 they
# rebuilt the worker image from local Containerfile changes (defeating
# build_qcow2's skip-if-exists cache — see lib/build-common.sh). Flipping the
# freshly-spawned worker to the GHCR-published `:latest` would immediately
# track a STALE remote image and silently mask exactly the local change the
# operator is boot-testing — a false-positive boot test. So skip the
# post-spawn switch in that case unless FORCE_SWITCH=1 explicitly opts back
# in, and WARN loudly. Mirrors the HBIRD_REMOTE_STRICT explicit-mode pattern
# (#371/#365): the safe default warns; the operator re-enables deliberately.
if [[ "${FORCE_REBUILD:-}" = "1" && "${FORCE_SWITCH:-}" != "1" ]]; then
  echo "${0##*/}: WARN: FORCE_REBUILD=1 — skipping post-spawn switch-to-ghcr so the freshly-built worker keeps tracking localhost/hummingbird-k8s-worker:latest (the image you just built)." >&2
  echo "${0##*/}: WARN: this avoids a false-positive boot test where the worker would flip to a possibly-stale GHCR image and mask your local change (#375)." >&2
  echo "${0##*/}: WARN: set FORCE_SWITCH=1 (alongside FORCE_REBUILD=1) to switch anyway, or unset FORCE_REBUILD for the normal GHCR-tracking behavior." >&2
else
  for i in $(seq 1 "$COUNT"); do
    NAME="hummingbird-k8s-worker-${i}"
    bash scripts/switch-to-ghcr.sh "$NAME" ghcr.io/aatchison/hummingbird-k8s-worker:latest || \
      echo "WARN: bootc switch failed for $NAME; VM still tracks localhost:latest" >&2
  done
fi
