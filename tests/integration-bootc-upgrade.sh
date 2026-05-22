#!/usr/bin/env bash
# integration-bootc-upgrade.sh — End-to-end bootc upgrade test (#12).
#
# Usage:  sudo tests/integration-bootc-upgrade.sh <from_tag> <to_tag>
#         (e.g. sudo tests/integration-bootc-upgrade.sh v0.1.9 v0.1.10)
#
# Builds a qcow2 from <from_tag>, virt-installs it, waits for k8s-init,
# captures pre-upgrade state, then inside the VM runs:
#   bootc switch ghcr.io/aatchison/hummingbird-k8s:<to_tag>
#   bootc upgrade
#   systemctl reboot
# Waits for the VM to come back, asserts the booted image is <to_tag>,
# /var/lib/k8s-init.done is still present, kubelet is running, and the node
# is Ready. Tears the VM down unconditionally via trap.
#
# Designed for the self-hosted runner with `kvm,libvirt` labels.

set -euo pipefail

# Force podman onto vfs when running inside a containerized self-hosted runner
# whose rootfs is overlayfs (the geary-docker runner). Overriding only when
# unset lets a caller still pick overlay/fuse-overlayfs on a bare-metal host
# (#124).
export STORAGE_DRIVER="${STORAGE_DRIVER:-vfs}"

# Use isolated per-run podman graph + run roots so a stale libpod database on
# the host (e.g. from a prior overlay-driver session) cannot silently override
# our vfs choice. See tests/integration-boot.sh / #139 for the full backstory.
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

FROM_TAG="${1:?from_tag required (e.g. v0.1.9)}"
TO_TAG="${2:?to_tag required (e.g. v0.1.10)}"
RUN_ID="${GITHUB_RUN_ID:-local}"

REPO="ghcr.io/aatchison/hummingbird-k8s"
FROM_IMAGE="${REPO}:${FROM_TAG}"
TO_IMAGE="${REPO}:${TO_TAG}"

VM_NAME="hummingbird-it-upgrade-${RUN_ID}"
WORK="$(mktemp -d -t hb-it-upgrade-XXXXXX)"
KEY="${WORK}/integration-upgrade-key"
BIB_CFG="${WORK}/bib-config.toml"
LIBVIRT_POOL_DIR="${LIBVIRT_POOL_DIR:-/var/lib/libvirt/images}"
POOL_QCOW="${LIBVIRT_POOL_DIR}/${VM_NAME}.qcow2"
BIB_IMAGE="${BIB_IMAGE:-quay.io/centos-bootc/bootc-image-builder:latest}"

VM_IP=""

log() { printf '[integration-bootc-upgrade] %s\n' "$*" >&2; }

dump_failure_context() {
  log "FAILURE CONTEXT --------------------------------"
  log "VM_NAME=${VM_NAME}"
  log "VM_IP=${VM_IP:-<unknown>}"
  log "FROM=${FROM_IMAGE}"
  log "TO  =${TO_IMAGE}"
  if virsh -c qemu:///system dominfo "${VM_NAME}" >/dev/null 2>&1; then
    log "--- virsh dominfo ---"
    virsh -c qemu:///system dominfo "${VM_NAME}" >&2 || true
  fi
  if [[ -n "${VM_IP}" ]]; then
    log "--- last 30 lines of journalctl k8s-init via SSH ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o ConnectTimeout=5 "root@${VM_IP}" \
        "journalctl -u k8s-init --no-pager -n 30; echo ---; bootc status || true" \
        >&2 2>/dev/null || log "(could not reach VM)"
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

ssh_opts=(-i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no
          -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5)

# --- 0. ephemeral key + bib config -----------------------------------------

log "preparing ephemeral SSH key at ${KEY}"
ssh-keygen -q -N '' -t ed25519 -f "${KEY}" -C "integration-upgrade-${RUN_ID}"

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

# --- 1. build qcow2 from from_tag ------------------------------------------

log "building qcow2 from ${FROM_IMAGE} via bib"
mkdir -p "${LIBVIRT_POOL_DIR}"
rm -rf "${LIBVIRT_POOL_DIR}/qcow2"
podman_isolated pull "${FROM_IMAGE}"

# Bind-mount the isolated graph root into bib at the path it expects so
# `--local` resolves to the image we just pulled.
podman_isolated run --rm --privileged --pull=newer \
  --net=host --cgroupns=host --cgroup-manager=cgroupfs \
  --security-opt label=disable \
  -v "${BIB_CFG}:/config.toml:ro" \
  -v "${LIBVIRT_POOL_DIR}:/output" \
  -v "${PODMAN_ROOT}:/var/lib/containers/storage" \
  "${BIB_IMAGE}" \
  --type qcow2 --rootfs ext4 \
  --local \
  "${FROM_IMAGE}"

mv -f "${LIBVIRT_POOL_DIR}/qcow2/disk.qcow2" "${POOL_QCOW}"
rmdir "${LIBVIRT_POOL_DIR}/qcow2" 2>/dev/null || true
chown root:root "${POOL_QCOW}"
chmod 0644 "${POOL_QCOW}"

# --- 2. virt-install + wait ------------------------------------------------

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
virsh -c qemu:///system start "${VM_NAME}" >/dev/null 2>&1 || true

wait_for_ip() {
  local n="$1" tries="${2:-90}"
  local lease
  VM_IP=""
  for _ in $(seq 1 "${tries}"); do
    lease="$(virsh -c qemu:///system domifaddr "${n}" 2>/dev/null \
              | awk '/ipv4/ {print $4}' | head -1 || true)"
    if [[ -n "${lease}" ]]; then
      VM_IP="${lease%/*}"
      return 0
    fi
    sleep 2
  done
  return 1
}

log "waiting for DHCP lease (up to 3 min)"
if ! wait_for_ip "${VM_NAME}" 90; then
  log "FAIL: never got a DHCP lease"
  exit 1
fi
log "VM_IP=${VM_IP}"

wait_for_ssh() {
  local tries="${1:-60}"
  for _ in $(seq 1 "${tries}"); do
    if ssh "${ssh_opts[@]}" "root@${VM_IP}" true >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  return 1
}

log "waiting for sshd (up to 2 min)"
if ! wait_for_ssh 60; then
  log "FAIL: sshd never came up"
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
  log "FAIL: k8s-init.done never appeared on initial boot"
  exit 1
fi

# --- 3. capture pre-upgrade state ------------------------------------------

log "capturing pre-upgrade state"
PRE_STATUS="$(ssh "${ssh_opts[@]}" "root@${VM_IP}" 'bootc status --format=json 2>/dev/null || bootc status')"
PRE_KUBELET="$(ssh "${ssh_opts[@]}" "root@${VM_IP}" 'kubelet --version || true')"
log "pre-upgrade kubelet: ${PRE_KUBELET}"
log "pre-upgrade bootc status (first 20 lines):"
printf '%s\n' "${PRE_STATUS}" | head -20 >&2

# --- 4. switch + upgrade + reboot ------------------------------------------

log "switching bootc image to ${TO_IMAGE}"
ssh "${ssh_opts[@]}" "root@${VM_IP}" "bootc switch '${TO_IMAGE}'"

log "running bootc upgrade"
# bootc upgrade is a no-op when already on target (switch staged the update);
# run it anyway to exercise the same upgrade path users hit when no switch
# is needed.
ssh "${ssh_opts[@]}" "root@${VM_IP}" 'bootc upgrade || true'

log "rebooting VM"
# `systemctl reboot` will drop our connection — ignore the resulting exit.
ssh "${ssh_opts[@]}" "root@${VM_IP}" 'systemctl reboot' || true

# Give the VM time to actually start shutting down before we probe again.
sleep 10

# DHCP lease may or may not change on reboot. Re-resolve to be safe.
log "waiting for VM to come back up (up to 3 min)"
if ! wait_for_ip "${VM_NAME}" 90; then
  log "FAIL: VM never got a DHCP lease after reboot"
  exit 1
fi
log "post-reboot VM_IP=${VM_IP}"

if ! wait_for_ssh 60; then
  log "FAIL: sshd never came back after reboot"
  exit 1
fi

# --- 5. post-upgrade assertions -------------------------------------------

log "assert: bootc status reports ${TO_TAG} as booted"
POST_STATUS="$(ssh "${ssh_opts[@]}" "root@${VM_IP}" 'bootc status')"
printf '%s\n' "${POST_STATUS}" | head -40 >&2
if ! printf '%s' "${POST_STATUS}" | grep -q "${TO_TAG}"; then
  log "FAIL: bootc status does not mention ${TO_TAG}"
  exit 1
fi

log "assert: /var/lib/k8s-init.done still present"
if ! ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       'test -f /var/lib/k8s-init.done' >/dev/null 2>&1; then
  log "FAIL: k8s-init.done is gone after upgrade"
  exit 1
fi

log "assert: kubelet active"
if ! ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       'systemctl is-active kubelet' >/dev/null 2>&1; then
  log "FAIL: kubelet is not active after upgrade"
  exit 1
fi

log "assert: at least one Ready node (allow 2 min for apiserver to come back)"
ready=0
for _ in $(seq 1 24); do
  if ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       'KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes --no-headers 2>/dev/null | awk "{print \$2}" | grep -qx Ready'; then
    ready=1
    break
  fi
  sleep 5
done
if [[ "${ready}" -ne 1 ]]; then
  log "FAIL: no node reached Ready within 2 min after reboot"
  exit 1
fi

log "ALL CHECKS PASSED"
exit 0
