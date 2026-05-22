#!/usr/bin/env bash
# integration-boot.sh — End-to-end boot-time CP test (#32).
#
# Usage:  sudo tests/integration-boot.sh <image-tag>
#         (e.g. sudo tests/integration-boot.sh v0.1.10)
#
# Builds a qcow2 from the published image, virt-installs a VM, waits for
# k8s-init to finish, then asserts:
#   - exactly one node, Ready
#   - verify-hardening.sh PASS (PodSecurity restricted, audit log, kubelet
#     --protect-kernel-defaults)
#
# Tears the VM down unconditionally via trap.
#
# Designed for a self-hosted runner with the `kvm,libvirt` labels. Requires
# root (libvirt's qemu:///system, loopback mounts for bib).

set -euo pipefail

# Force podman onto vfs when running inside a containerized self-hosted runner
# whose rootfs is overlayfs (the geary-docker runner). Overriding only when
# unset lets a caller still pick overlay/fuse-overlayfs on a bare-metal host
# (#124).
export STORAGE_DRIVER="${STORAGE_DRIVER:-vfs}"

# Use isolated per-run podman graph + run roots so a stale libpod database on
# the host (e.g. from a prior overlay-driver session) cannot silently override
# our vfs choice with:
#   "User-selected graph driver \"vfs\" overwritten by graph driver
#    \"overlay\" from database"
# (#139). Defaults are workflow-controlled but the script also works locally.
RUN_ID_TAG="${GITHUB_RUN_ID:-local-$$}"
PODMAN_ROOT="${PODMAN_ROOT:-/var/lib/integration-storage/${RUN_ID_TAG}}"
PODMAN_RUNROOT="${PODMAN_RUNROOT:-/run/integration-storage/${RUN_ID_TAG}}"
export PODMAN_ROOT PODMAN_RUNROOT
mkdir -p "${PODMAN_ROOT}" "${PODMAN_RUNROOT}"

podman_isolated() {
  podman \
    --storage-driver "${STORAGE_DRIVER}" \
    --root "${PODMAN_ROOT}" \
    --runroot "${PODMAN_RUNROOT}" \
    "$@"
}

TAG="${1:?image tag required (e.g. v0.1.10)}"
RUN_ID="${GITHUB_RUN_ID:-local}"
IMAGE="ghcr.io/aatchison/hummingbird-k8s:${TAG}"

VM_NAME="hummingbird-it-boot-${RUN_ID}"
WORK="$(mktemp -d -t hb-it-boot-XXXXXX)"
QCOW="${WORK}/disk.qcow2"
KEY="${WORK}/integration-boot-key"
BIB_CFG="${WORK}/bib-config.toml"
LIBVIRT_POOL_DIR="${LIBVIRT_POOL_DIR:-/var/lib/libvirt/images}"
POOL_QCOW="${LIBVIRT_POOL_DIR}/${VM_NAME}.qcow2"
BIB_IMAGE="${BIB_IMAGE:-quay.io/centos-bootc/bootc-image-builder:latest}"

VM_IP=""

log() { printf '[integration-boot] %s\n' "$*" >&2; }

dump_failure_context() {
  log "FAILURE CONTEXT --------------------------------"
  log "VM_NAME=${VM_NAME}"
  log "VM_IP=${VM_IP:-<unknown>}"
  log "QCOW=${POOL_QCOW}"
  if virsh -c qemu:///system dominfo "${VM_NAME}" >/dev/null 2>&1; then
    log "--- virsh dominfo ---"
    virsh -c qemu:///system dominfo "${VM_NAME}" >&2 || true
  fi
  if [[ -n "${VM_IP}" ]]; then
    log "--- last 30 lines of journalctl k8s-init via SSH ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o ConnectTimeout=5 "root@${VM_IP}" \
        "journalctl -u k8s-init --no-pager -n 30" >&2 2>/dev/null || \
        log "(could not pull journal — VM unreachable)"
  fi
  log "------------------------------------------------"
}

cleanup() {
  local rc=$?
  if [[ $rc -ne 0 ]]; then
    dump_failure_context
  fi
  log "tearing down VM ${VM_NAME}"
  virsh -c qemu:///system destroy "${VM_NAME}" >/dev/null 2>&1 || true
  virsh -c qemu:///system undefine --nvram "${VM_NAME}" >/dev/null 2>&1 || true
  rm -f "${POOL_QCOW}" || true
  rm -rf "${WORK}" || true
  # Best-effort: drop the per-run isolated podman roots (the workflow also
  # cleans these up in an always() step, but local invocations rely on us).
  rm -rf "${PODMAN_ROOT}" "${PODMAN_RUNROOT}" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

if [[ ${EUID} -ne 0 ]]; then
  log "must run as root (libvirt qemu:///system + bib loopback mounts)"
  exit 2
fi

log "preparing ephemeral SSH key at ${KEY}"
ssh-keygen -q -N '' -t ed25519 -f "${KEY}" -C "integration-boot-${RUN_ID}"

# Per-test bib config: ephemeral user, ephemeral key, root SSH enabled so
# verify-hardening.sh can ssh in as root.
PUB="$(cat "${KEY}.pub")"
RAND_USER="ittest$((RANDOM))"

cat >"${BIB_CFG}" <<EOF
[[customizations.user]]
name = "${RAND_USER}"
groups = ["wheel"]
key = """${PUB}"""

[[customizations.user]]
name = "root"
key = """${PUB}"""
EOF

log "building qcow2 from ${IMAGE} via bib"
mkdir -p "${LIBVIRT_POOL_DIR}"
rm -rf "${LIBVIRT_POOL_DIR}/qcow2"

# When running inside the containerized self-hosted runner, nested podman hits
# vfs+SELinux+cgroup limits we can't work around (#150). Instead, launch bib as
# a SIBLING container on the host docker daemon (socket is mounted into the
# runner). bib pulls IMAGE from GHCR itself — no --local needed.
# For local/bare-metal runs we still prefer podman.
if command -v docker >/dev/null && [[ -S /var/run/docker.sock ]]; then
  log "using host docker daemon for bib (sibling-of-runner)"
  # docker socket sees the HOST fs (geary), not the runner container's. Use
  # LIBVIRT_POOL_DIR (mounted into runner from host) for all bib-visible paths.
  SHARED_DIR="${LIBVIRT_POOL_DIR}/integration-runs/${RUN_ID}"
  mkdir -p "${SHARED_DIR}"
  cp "${BIB_CFG}" "${SHARED_DIR}/config.toml"
  BIB_STORAGE="${SHARED_DIR}/storage"
  mkdir -p "${BIB_STORAGE}"
  trap 'rm -rf "'"${SHARED_DIR}"'" 2>/dev/null || true' EXIT
  # Pre-pull image into bib storage via a sibling podman container.
  log "pre-pulling ${IMAGE} into bib storage via sibling podman"
  docker run --rm --privileged \
    -v "${BIB_STORAGE}:/var/lib/containers/storage" \
    quay.io/podman/stable:latest \
    podman --root /var/lib/containers/storage pull "${IMAGE}"
  docker run --rm --privileged --pull=always \
    -v "${SHARED_DIR}/config.toml:/config.toml:ro" \
    -v "${LIBVIRT_POOL_DIR}:/output" \
    -v "${BIB_STORAGE}:/var/lib/containers/storage" \
    "${BIB_IMAGE}" \
    --type qcow2 --rootfs ext4 \
    --local \
    "${IMAGE}"
else
  log "using podman for bib (bare-metal path)"
  podman_isolated pull "${IMAGE}"
  podman_isolated run --rm --privileged --pull=newer \
    --net=host --cgroupns=host --cgroup-manager=cgroupfs \
    --security-opt label=disable \
    -v "${BIB_CFG}:/config.toml:ro" \
    -v "${LIBVIRT_POOL_DIR}:/output" \
    -v "${PODMAN_ROOT}:/var/lib/containers/storage" \
    "${BIB_IMAGE}" \
    --type qcow2 --rootfs ext4 \
    --local \
    "${IMAGE}"
fi

mv -f "${LIBVIRT_POOL_DIR}/qcow2/disk.qcow2" "${POOL_QCOW}"
rmdir "${LIBVIRT_POOL_DIR}/qcow2" 2>/dev/null || true
chown root:root "${POOL_QCOW}"
chmod 0644 "${POOL_QCOW}"
# Keep a symlink so dump_failure_context output is honest.
ln -sf "${POOL_QCOW}" "${QCOW}"

log "virt-installing ${VM_NAME}"
virt-install --connect qemu:///system \
  --name "${VM_NAME}" \
  --memory 4096 --vcpus 2 \
  --disk "${POOL_QCOW},format=qcow2,bus=virtio" \
  --import \
  --os-variant fedora-unknown \
  --network network=default,model=virtio \
  --graphics none \
  --noautoconsole \
  --noreboot

# `--noreboot` leaves the domain stopped after install completes; start it
# explicitly so the boot path is unambiguous.
virsh -c qemu:///system start "${VM_NAME}" >/dev/null 2>&1 || true

log "waiting for DHCP lease (up to 3 min)"
VM_IP=""
for _ in $(seq 1 90); do
  lease="$(virsh -c qemu:///system domifaddr "${VM_NAME}" --source agent 2>/dev/null \
            | awk '/ipv4/ {print $4}' | head -1 || true)"
  if [[ -z "${lease}" ]]; then
    lease="$(virsh -c qemu:///system domifaddr "${VM_NAME}" 2>/dev/null \
              | awk '/ipv4/ {print $4}' | head -1 || true)"
  fi
  if [[ -n "${lease}" ]]; then
    VM_IP="${lease%/*}"
    break
  fi
  sleep 2
done
if [[ -z "${VM_IP}" ]]; then
  log "FAIL: never got a DHCP lease for ${VM_NAME}"
  exit 1
fi
log "VM_IP=${VM_IP}"

ssh_opts=(-i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no
          -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5)

log "waiting for sshd to answer (up to 2 min)"
for _ in $(seq 1 60); do
  if ssh "${ssh_opts[@]}" "root@${VM_IP}" true >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
if ! ssh "${ssh_opts[@]}" "root@${VM_IP}" true >/dev/null 2>&1; then
  log "FAIL: sshd never came up on ${VM_IP}"
  exit 1
fi

log "waiting for /var/lib/k8s-init.done (up to 5 min)"
init_ok=0
for _ in $(seq 1 60); do
  if ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       'test -f /var/lib/k8s-init.done' >/dev/null 2>&1; then
    init_ok=1
    break
  fi
  sleep 5
done
if [[ "${init_ok}" -ne 1 ]]; then
  log "FAIL: k8s-init.done never appeared"
  exit 1
fi
log "k8s-init.done present"

log "assert: exactly one Ready node"
nodes_json="$(ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  'KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes -o json')"
ready_count="$(printf '%s' "${nodes_json}" \
  | grep -c '"type": "Ready"' || true)"
node_count="$(printf '%s' "${nodes_json}" \
  | grep -c '"kind": "Node"' || true)"

# Newer kubectl renders one "Node" kind per node in the list; older lists wrap
# them in a List kind. Fall back to the items[] approach using jq if available.
if command -v jq >/dev/null 2>&1; then
  node_count="$(printf '%s' "${nodes_json}" | jq '.items | length')"
  ready_count="$(printf '%s' "${nodes_json}" \
    | jq '[.items[].status.conditions[]? | select(.type=="Ready" and .status=="True")] | length')"
fi

log "  nodes=${node_count} ready=${ready_count}"
if [[ "${node_count}" -ne 1 ]] || [[ "${ready_count}" -lt 1 ]]; then
  log "FAIL: expected exactly one Ready node, got nodes=${node_count} ready=${ready_count}"
  exit 1
fi

log "assert: verify-hardening.sh PASS (run on CP via SSH)"
# Run the in-tree verifier from the CP itself — it expects kubectl + SSH
# access to root@CP_IP, which from the VM's perspective is localhost.
# Copy it in so we don't depend on whatever's baked into the image.
scp "${ssh_opts[@]}" \
  "$(dirname "$0")/../scripts/verify-hardening.sh" \
  "root@${VM_IP}:/tmp/verify-hardening.sh" >/dev/null
ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  'chmod +x /tmp/verify-hardening.sh && KUBECONFIG=/etc/kubernetes/admin.conf CP_IP=127.0.0.1 /tmp/verify-hardening.sh'

log "ALL CHECKS PASSED"
exit 0
