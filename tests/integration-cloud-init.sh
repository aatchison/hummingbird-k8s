#!/usr/bin/env bash
# integration-cloud-init.sh — End-to-end test of the optional cloud-init path.
#
# Usage:  sudo tests/integration-cloud-init.sh <flavor>
#         where <flavor> is one of: k3s, k8s, k8s-worker
#
# Verifies the ENABLE_CLOUD_INIT=1 build arg (PR #164, issue #163) end to
# end:
#   1. Build the flavor's image LOCALLY with ENABLE_CLOUD_INIT=1 (GHCR does
#      not publish a cloud-init variant — the canonical Hummingbird image is
#      the bib-customized one without cloud-init).
#   2. Build a qcow2 from that local image via bib (sibling-docker pattern,
#      same trick used by integration-boot.sh).
#   3. Generate a NoCloud seed ISO from
#      tests/fixtures/cloud-init-user-data.yaml + a synthetic meta-data file,
#      using `cloud-localds` when present and falling back to genisoimage.
#   4. virt-install the VM with the seed ISO attached as a CD-ROM.
#   5. SSH in (using the key injected into user-data) and assert:
#        - /var/lib/cloud-init-verified exists (runcmd: applied)
#        - (no package check — bootc /usr is read-only)
#        - `cloud-init status` reports done (final stage succeeded)
#
# Tears the VM, qcow2, and seed ISO down unconditionally via trap.
#
# Designed for the geary-docker self-hosted runner with the `kvm,libvirt`
# labels. Requires root (libvirt's qemu:///system, loopback mounts for bib).

set -euo pipefail

# Force podman onto vfs when running inside a containerized self-hosted runner
# whose rootfs is overlayfs (the geary-docker runner). Same reasoning as
# integration-boot.sh (#124).
export STORAGE_DRIVER="${STORAGE_DRIVER:-vfs}"

RUN_ID_TAG="${GITHUB_RUN_ID:-local-$$}"
PODMAN_ROOT="${PODMAN_ROOT:-/var/lib/integration-storage/${RUN_ID_TAG}}"
PODMAN_RUNROOT="${PODMAN_RUNROOT:-/run/integration-storage/${RUN_ID_TAG}}"
export PODMAN_ROOT PODMAN_RUNROOT
mkdir -p "${PODMAN_ROOT}" "${PODMAN_RUNROOT}"

FLAVOR="${1:?flavor required (one of: k3s, k8s, k8s-worker)}"
case "${FLAVOR}" in
  k3s|k8s|k8s-worker) ;;
  *)
    printf '[integration-cloud-init] unknown flavor: %s (want k3s|k8s|k8s-worker)\n' "${FLAVOR}" >&2
    exit 2
    ;;
esac

RUN_ID="${GITHUB_RUN_ID:-local}"
LOCAL_IMAGE="local/hummingbird-${FLAVOR}-ci:test"
CONTAINERFILE="containers/${FLAVOR}/Containerfile"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE="${REPO_ROOT}/tests/fixtures/cloud-init-user-data.yaml"

VM_NAME="hummingbird-it-ci-${FLAVOR}-${RUN_ID}"
WORK="$(mktemp -d -t hb-it-ci-XXXXXX)"
QCOW="${WORK}/disk.qcow2"
KEY="${WORK}/integration-ci-key"
BIB_CFG="${WORK}/bib-config.toml"
USER_DATA="${WORK}/user-data"
META_DATA="${WORK}/meta-data"
SEED_ISO="${WORK}/seed.iso"
LIBVIRT_POOL_DIR="${LIBVIRT_POOL_DIR:-/var/lib/libvirt/images}"
POOL_QCOW="${LIBVIRT_POOL_DIR}/${VM_NAME}.qcow2"
POOL_SEED="${LIBVIRT_POOL_DIR}/${VM_NAME}-seed.iso"
BIB_IMAGE="${BIB_IMAGE:-quay.io/centos-bootc/bootc-image-builder:latest}"
SHARED_DIR=""

VM_IP=""

log() { printf '[integration-cloud-init] %s\n' "$*" >&2; }

dump_failure_context() {
  log "FAILURE CONTEXT --------------------------------"
  log "FLAVOR=${FLAVOR}"
  log "VM_NAME=${VM_NAME}"
  log "VM_IP=${VM_IP:-<unknown>}"
  log "QCOW=${POOL_QCOW}"
  log "SEED=${POOL_SEED}"
  if virsh -c qemu:///system dominfo "${VM_NAME}" >/dev/null 2>&1; then
    log "--- virsh dominfo ---"
    virsh -c qemu:///system dominfo "${VM_NAME}" >&2 || true
  fi
  if [[ -n "${VM_IP}" ]]; then
    log "--- cloud-init status / journal (via SSH) ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "root@${VM_IP}" \
        "cloud-init status --long || true; journalctl -u cloud-final --no-pager -n 50 || true" \
        >&2 2>/dev/null || log "(could not pull cloud-init journal — VM unreachable)"
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
  rm -f "${POOL_QCOW}" "${POOL_SEED}" || true
  if [[ -n "${SHARED_DIR}" ]] && [[ -d "${SHARED_DIR}" ]]; then
    rm -rf "${SHARED_DIR}" || true
  fi
  rm -rf "${WORK}" || true
  # Best-effort: drop the per-run isolated podman roots.
  rm -rf "${PODMAN_ROOT}" "${PODMAN_RUNROOT}" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

if [[ ${EUID} -ne 0 ]]; then
  log "must run as root (libvirt qemu:///system + bib loopback mounts)"
  exit 2
fi

if [[ ! -f "${FIXTURE}" ]]; then
  log "missing fixture: ${FIXTURE}"
  exit 2
fi
if [[ ! -f "${REPO_ROOT}/${CONTAINERFILE}" ]]; then
  log "missing Containerfile: ${REPO_ROOT}/${CONTAINERFILE}"
  exit 2
fi

log "preparing ephemeral SSH key at ${KEY}"
ssh-keygen -q -N '' -t ed25519 -f "${KEY}" -C "integration-cloud-init-${RUN_ID}"
PUB="$(cat "${KEY}.pub")"

# Bib customization config — ephemeral root user with the same key. This lets
# the driver SSH in even if cloud-init's final stage hasn't applied the
# user-data SSH key yet (we'd otherwise be racing the boot timeline).
cat >"${BIB_CFG}" <<EOF
[[customizations.user]]
name = "root"
key = """${PUB}"""
EOF

# Render the user-data fixture with our ephemeral key substituted in. Use a
# python one-liner to avoid sed escaping pain on the OpenSSH base64 payload.
python3 - "${FIXTURE}" "${PUB}" "${USER_DATA}" <<'PY'
import sys, pathlib
src, pub, dst = sys.argv[1], sys.argv[2], sys.argv[3]
txt = pathlib.Path(src).read_text()
if "SSH_KEY_PLACEHOLDER" not in txt:
    sys.exit(f"fixture is missing SSH_KEY_PLACEHOLDER: {src}")
pathlib.Path(dst).write_text(txt.replace("SSH_KEY_PLACEHOLDER", pub))
PY

# Stable instance-id derived from the run — cloud-init only re-runs when this
# changes, which is fine for a single-boot test.
cat >"${META_DATA}" <<EOF
instance-id: hummingbird-ci-${FLAVOR}-${RUN_ID}
local-hostname: hummingbird-ci-${FLAVOR}
EOF

# --- Build the cloud-init-enabled image LOCALLY ----------------------------
#
# GHCR does not publish a cloud-init variant — the canonical Hummingbird
# image is the bib-customized one without cloud-init. To exercise the
# ENABLE_CLOUD_INIT=1 build arg we have to build the image ourselves. Mirror
# integration-boot.sh's sibling-docker preference for the containerized
# runner, with a podman fallback for bare-metal.
log "building cloud-init-enabled image ${LOCAL_IMAGE} from ${CONTAINERFILE}"
if command -v docker >/dev/null && [[ -S /var/run/docker.sock ]]; then
  log "using host docker daemon for image build (sibling-of-runner)"
  docker build \
    --build-arg ENABLE_CLOUD_INIT=1 \
    -t "${LOCAL_IMAGE}" \
    -f "${REPO_ROOT}/${CONTAINERFILE}" \
    "${REPO_ROOT}"
else
  log "using podman for image build (bare-metal path)"
  podman \
    --storage-driver "${STORAGE_DRIVER}" \
    --root "${PODMAN_ROOT}" \
    --runroot "${PODMAN_RUNROOT}" \
    build \
      --build-arg ENABLE_CLOUD_INIT=1 \
      -t "${LOCAL_IMAGE}" \
      -f "${REPO_ROOT}/${CONTAINERFILE}" \
      "${REPO_ROOT}"
fi

# --- bib qcow2 build -------------------------------------------------------
log "building qcow2 from ${LOCAL_IMAGE} via bib"
mkdir -p "${LIBVIRT_POOL_DIR}"
rm -rf "${LIBVIRT_POOL_DIR}/qcow2"

if command -v docker >/dev/null && [[ -S /var/run/docker.sock ]]; then
  log "using host docker daemon for bib (sibling-of-runner)"
  SHARED_DIR="${LIBVIRT_POOL_DIR}/integration-runs/${RUN_ID}-ci-${FLAVOR}"
  mkdir -p "${SHARED_DIR}"
  cp "${BIB_CFG}" "${SHARED_DIR}/config.toml"
  BIB_STORAGE="${SHARED_DIR}/storage"
  mkdir -p "${BIB_STORAGE}"
  # The local image lives in docker's image store on the host, not in bib's
  # /var/lib/containers/storage. Save + load it into bib's storage via a
  # sibling podman container so bib --local can find it.
  log "transferring ${LOCAL_IMAGE} into bib storage via sibling podman"
  docker save "${LOCAL_IMAGE}" -o "${SHARED_DIR}/image.tar"
  docker run --rm --privileged \
    -v "${BIB_STORAGE}:/var/lib/containers/storage" \
    -v "${SHARED_DIR}/image.tar:/image.tar:ro" \
    quay.io/podman/stable:latest \
    podman --root /var/lib/containers/storage load -i /image.tar
  rm -f "${SHARED_DIR}/image.tar"
  docker run --rm --privileged --pull=always \
    -v "${SHARED_DIR}/config.toml:/config.toml:ro" \
    -v "${LIBVIRT_POOL_DIR}:/output" \
    -v "${BIB_STORAGE}:/var/lib/containers/storage" \
    "${BIB_IMAGE}" \
    --type qcow2 --rootfs ext4 \
    --local \
    "${LOCAL_IMAGE}"
else
  log "using podman for bib (bare-metal path)"
  podman \
    --storage-driver "${STORAGE_DRIVER}" \
    --root "${PODMAN_ROOT}" \
    --runroot "${PODMAN_RUNROOT}" \
    run --rm --privileged --pull=newer \
    --net=host --cgroupns=host --cgroup-manager=cgroupfs \
    --security-opt label=disable \
    -v "${BIB_CFG}:/config.toml:ro" \
    -v "${LIBVIRT_POOL_DIR}:/output" \
    -v "${PODMAN_ROOT}:/var/lib/containers/storage" \
    "${BIB_IMAGE}" \
    --type qcow2 --rootfs ext4 \
    --local \
    "${LOCAL_IMAGE}"
fi

mv -f "${LIBVIRT_POOL_DIR}/qcow2/disk.qcow2" "${POOL_QCOW}"
rmdir "${LIBVIRT_POOL_DIR}/qcow2" 2>/dev/null || true
chown root:root "${POOL_QCOW}"
chmod 0644 "${POOL_QCOW}"
ln -sf "${POOL_QCOW}" "${QCOW}"

# --- NoCloud seed ISO ------------------------------------------------------
log "building NoCloud seed ISO at ${SEED_ISO}"
if command -v cloud-localds >/dev/null 2>&1; then
  cloud-localds "${SEED_ISO}" "${USER_DATA}" "${META_DATA}"
elif command -v genisoimage >/dev/null 2>&1; then
  # NoCloud only mandates the volume ID be `cidata` and the iso contain
  # user-data + meta-data at the root. `-rational-rock` makes long names
  # readable on the guest.
  genisoimage -output "${SEED_ISO}" \
    -volid cidata -joliet -rock \
    -graft-points \
      "user-data=${USER_DATA}" \
      "meta-data=${META_DATA}" >/dev/null 2>&1
else
  log "FAIL: neither cloud-localds nor genisoimage available"
  exit 2
fi

cp -f "${SEED_ISO}" "${POOL_SEED}"
chown root:root "${POOL_SEED}"
chmod 0644 "${POOL_SEED}"

# --- VM up -----------------------------------------------------------------
log "virt-installing ${VM_NAME} (seed ISO attached)"
virt-install --connect qemu:///system \
  --name "${VM_NAME}" \
  --memory 4096 --vcpus 2 \
  --disk "${POOL_QCOW},format=qcow2,bus=virtio" \
  --disk "${POOL_SEED},device=cdrom,bus=sata,readonly=on" \
  --import \
  --os-variant fedora-unknown \
  --network network=default,model=virtio \
  --graphics none \
  --noautoconsole \
  --noreboot

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

log "waiting for sshd to answer (up to 5 min)"
ssh_ok=0
for _ in $(seq 1 150); do
  if ssh "${ssh_opts[@]}" "root@${VM_IP}" true >/dev/null 2>&1; then
    ssh_ok=1
    break
  fi
  sleep 2
done
if [[ "${ssh_ok}" -ne 1 ]]; then
  log "FAIL: sshd never came up on ${VM_IP}"
  exit 1
fi

# --- Wait for cloud-init to settle -----------------------------------------
#
# cloud-init status --wait blocks until cloud-final has run (or the deadline
# is reached). Wrap it in our own timeout so a wedged datasource lookup can't
# hang the runner.
log "waiting for cloud-init.target / cloud-final (up to 5 min)"
cinit_ok=0
for _ in $(seq 1 60); do
  if ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       'systemctl is-active --quiet cloud-init.target' >/dev/null 2>&1; then
    cinit_ok=1
    break
  fi
  sleep 5
done
if [[ "${cinit_ok}" -ne 1 ]]; then
  log "FAIL: cloud-init.target never became active"
  exit 1
fi

log "cloud-init.target active — asserting final stage status"
status_out="$(ssh "${ssh_opts[@]}" "root@${VM_IP}" 'cloud-init status' 2>&1 || true)"
log "  cloud-init status: ${status_out}"
if ! printf '%s' "${status_out}" | grep -qE 'status: (done|disabled)'; then
  log "FAIL: cloud-init did not report done"
  ssh "${ssh_opts[@]}" "root@${VM_IP}" 'cloud-init status --long' >&2 || true
  exit 1
fi
if printf '%s' "${status_out}" | grep -q 'status: disabled'; then
  log "FAIL: cloud-init reports disabled — the image was not built with ENABLE_CLOUD_INIT=1"
  exit 1
fi

# --- Assertions ------------------------------------------------------------
log "assert: /var/lib/cloud-init-verified exists (runcmd applied)"
if ! ssh "${ssh_opts[@]}" "root@${VM_IP}" \
     'test -f /var/lib/cloud-init-verified'; then
  log "FAIL: /var/lib/cloud-init-verified missing — runcmd did not fire"
  exit 1
fi

# NOTE: We do NOT assert package install here. bootc/ostree images have a
# read-only /usr; cloud-init's package_update_upgrade_install module always
# fails on them. That's expected — operators wanting package overlays should
# layer them into the Containerfile, not via cloud-init user-data.
log "(skipping package-install check — bootc /usr is read-only by design)"

log "ALL CHECKS PASSED"
exit 0
