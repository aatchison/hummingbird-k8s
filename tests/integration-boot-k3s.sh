#!/usr/bin/env bash
# integration-boot-k3s.sh — End-to-end boot-time CP test for the k3s flavor.
#
# Usage:  sudo tests/integration-boot-k3s.sh <image-tag>
#         (e.g. sudo tests/integration-boot-k3s.sh v0.1.12)
#
# Builds a qcow2 from the published hummingbird-k3s image, virt-installs a VM,
# waits for k3s to come up, then asserts:
#   - k3s.service active
#   - exactly one node, Ready (via the k3s-shipped kubectl)
#   - a tiny PSA-compliant pod can be scheduled, becomes Ready, and serves
#     traffic over a ClusterIP Service (smoke for CNI + DNS + scheduler).
#
# Hardening checks (verify-hardening.sh) are k8s-only and intentionally NOT
# run here — k3s ships a different stack (sqlite + flannel + traefik) with
# different on-disk paths and its own opinionated defaults.
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
# the host cannot silently override our vfs choice (#139).
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

TAG="${1:?image tag required (e.g. v0.1.12)}"
RUN_ID="${GITHUB_RUN_ID:-local}"
IMAGE="ghcr.io/aatchison/hummingbird-k3s:${TAG}"

VM_NAME="hummingbird-it-boot-k3s-${RUN_ID}"
WORK="$(mktemp -d -t hb-it-boot-k3s-XXXXXX)"
QCOW="${WORK}/disk.qcow2"
KEY="${WORK}/integration-boot-k3s-key"
BIB_CFG="${WORK}/bib-config.toml"
LIBVIRT_POOL_DIR="${LIBVIRT_POOL_DIR:-/var/lib/libvirt/images}"
POOL_QCOW="${LIBVIRT_POOL_DIR}/${VM_NAME}.qcow2"
BIB_IMAGE="${BIB_IMAGE:-quay.io/centos-bootc/bootc-image-builder:latest}"

VM_IP=""
SHARED_DIR=""
# SERIAL_LOG is computed once we know LIBVIRT_POOL_DIR+VM_NAME and the dir
# exists; kept as a global so dump_failure_context + cleanup can find it.
SERIAL_LOG=""

log() { printf '[integration-boot-k3s] %s\n' "$*" >&2; }

dump_failure_context() {
  log "FAILURE CONTEXT --------------------------------"
  log "VM_NAME=${VM_NAME}"
  log "VM_IP=${VM_IP:-<unknown>}"
  log "QCOW=${POOL_QCOW}"
  if virsh -c qemu:///system dominfo "${VM_NAME}" >/dev/null 2>&1; then
    log "--- virsh dominfo ---"
    virsh -c qemu:///system dominfo "${VM_NAME}" >&2 || true
    log "--- virsh domstate ---"
    virsh -c qemu:///system domstate "${VM_NAME}" >&2 || true
  fi
  # The guest console is logged to ${SERIAL_LOG} via virt-install's
  # `--console pty,log.file=...`. Dump the tail so we can see boot messages
  # even when sshd never came up (#165). This is the single most important
  # diagnostic when SSH times out — without it we have zero visibility into
  # what's happening inside the VM.
  if [[ -s "${SERIAL_LOG}" ]]; then
    log "--- last 200 lines of VM serial console (${SERIAL_LOG}) ---"
    tail -n 200 "${SERIAL_LOG}" >&2 || true
  else
    log "(no serial console log at ${SERIAL_LOG})"
  fi
  if [[ -n "${VM_IP}" ]]; then
    log "--- attempting ssh root@${VM_IP} 'systemctl --failed' ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "root@${VM_IP}" "systemctl --failed --no-pager" >&2 2>/dev/null || \
        log "(SSH unreachable; nothing more to gather over SSH)"
    log "--- last 60 lines of journalctl -b (via SSH) ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "root@${VM_IP}" "journalctl -b --no-pager -n 60" >&2 2>/dev/null || true
    log "--- last 40 lines of journalctl k3s via SSH ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "root@${VM_IP}" "journalctl -u k3s --no-pager -n 40" >&2 2>/dev/null || true
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
  rm -f "${POOL_QCOW}" "${SERIAL_LOG}" || true
  if [[ -n "${SHARED_DIR}" ]] && [[ -d "${SHARED_DIR}" ]]; then
    rm -rf "${SHARED_DIR}" 2>/dev/null || true
  fi
  rm -rf "${WORK}" || true
  rm -rf "${PODMAN_ROOT}" "${PODMAN_RUNROOT}" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

if [[ ${EUID} -ne 0 ]]; then
  log "must run as root (libvirt qemu:///system + bib loopback mounts)"
  exit 2
fi

log "preparing ephemeral SSH key at ${KEY}"
ssh-keygen -q -N '' -t ed25519 -f "${KEY}" -C "integration-boot-k3s-${RUN_ID}"

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

# Sibling-of-runner docker is preferred over nested podman in the
# geary-docker runner container (#150). See tests/integration-boot.sh for
# the full rationale.
if command -v docker >/dev/null && [[ -S /var/run/docker.sock ]]; then
  log "using host docker daemon for bib (sibling-of-runner)"
  # NB: SHARED_DIR is declared at top-of-script so cleanup() can rm it without
  # us overwriting the parent's EXIT trap (the previous code did exactly that
  # and silently disabled both dump_failure_context AND the VM teardown, which
  # is why failure runs leaked their VMs and produced no diagnostics — #165).
  SHARED_DIR="${LIBVIRT_POOL_DIR}/integration-runs/${RUN_ID}-k3s"
  mkdir -p "${SHARED_DIR}"
  cp "${BIB_CFG}" "${SHARED_DIR}/config.toml"
  BIB_STORAGE="${SHARED_DIR}/storage"
  mkdir -p "${BIB_STORAGE}"
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
ln -sf "${POOL_QCOW}" "${QCOW}"

log "virt-installing ${VM_NAME}"
# k3s needs more breathing room than k8s at boot (embedded sqlite, traefik,
# coredns, metrics-server all come up in a single process). The
# scripts/define-vm.sh production default is 6144M/4vCPUs; match that here
# so the integration VM doesn't OOM-stall before sshd is reachable.
SERIAL_LOG="${LIBVIRT_POOL_DIR}/${VM_NAME}-console.log"
: >"${SERIAL_LOG}"
chown qemu:qemu "${SERIAL_LOG}" 2>/dev/null || true
chmod 0660 "${SERIAL_LOG}"
virt-install --connect qemu:///system \
  --name "${VM_NAME}" \
  --memory 6144 --vcpus 4 \
  --disk "${POOL_QCOW},format=qcow2,bus=virtio" \
  --import \
  --os-variant fedora-unknown \
  --network network=default,model=virtio \
  --graphics none \
  --noautoconsole \
  --noreboot

# Bolt a `<serial type='file'>` device onto the domain BEFORE first boot.
# We do this as a post-virt-install XML patch (rather than `--serial
# file,...`) because the runner's virt-install/libvirt combination silently
# produces an unstartable domain when the inline forms are used (the
# previous re-test ended with State=shut off and `virsh start` failed
# without diagnostic; #165). Patching the XML directly is the most
# portable form: target port='1' so it adds a second serial port rather
# than fighting whatever device virt-install put on port=0. Best-effort —
# if anything in this block fails, we just won't have a console log;
# dump_failure_context already gracefully handles an empty SERIAL_LOG.
if virsh -c qemu:///system dumpxml "${VM_NAME}" >/dev/null 2>&1; then
  tmp_xml="${WORK}/domain.xml"
  if virsh -c qemu:///system dumpxml "${VM_NAME}" >"${tmp_xml}"; then
    if ! grep -q "serial type='file'" "${tmp_xml}"; then
      if python3 - "${tmp_xml}" "${SERIAL_LOG}" <<'PY'
import sys, xml.etree.ElementTree as ET
xml_path, log_path = sys.argv[1], sys.argv[2]
tree = ET.parse(xml_path)
root = tree.getroot()
devices = root.find('devices')
if devices is None:
    sys.exit(2)
ser = ET.SubElement(devices, 'serial', {'type': 'file'})
ET.SubElement(ser, 'source', {'path': log_path, 'append': 'on'})
ET.SubElement(ser, 'target', {'port': '1'})
tree.write(xml_path, encoding='unicode')
PY
      then
        virsh -c qemu:///system define "${tmp_xml}" >/dev/null 2>&1 \
          || log "(virsh define after serial-file patch failed; continuing without console log)"
      else
        log "(python XML patch failed; continuing without console log)"
      fi
    fi
  fi
fi

# Capture stderr from `virsh start` so a real start failure (the previous
# re-test left the domain in State=shut off without telling us why; #165)
# surfaces in the test log.
log "starting VM ${VM_NAME}"
if ! virsh -c qemu:///system start "${VM_NAME}" 2>&1; then
  log "WARN: virsh start exited non-zero — domain may not be running"
fi

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

log "waiting for sshd to answer (up to 5 min — k3s boot can be slow)"
# k3s does a lot on first boot (container runtime, embedded etcd/sqlite,
# traefik+coredns manifests, ssh-hostkey-regen) and the libvirt DHCP lease
# can appear well before sshd is actually accepting connections. Give it
# more headroom than the k8s flavor needs.
for _ in $(seq 1 150); do
  if ssh "${ssh_opts[@]}" "root@${VM_IP}" true >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
if ! ssh "${ssh_opts[@]}" "root@${VM_IP}" true >/dev/null 2>&1; then
  log "FAIL: sshd never came up on ${VM_IP}"
  exit 1
fi

log "waiting for k3s.service active (up to 5 min)"
k3s_ok=0
for _ in $(seq 1 60); do
  if ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       'systemctl is-active k3s' >/dev/null 2>&1; then
    k3s_ok=1
    break
  fi
  sleep 5
done
if [[ "${k3s_ok}" -ne 1 ]]; then
  log "FAIL: k3s.service never went active"
  exit 1
fi
log "k3s.service active"

# All k3s kubectl invocations use the k3s-shipped binary and its kubeconfig.
# `k3s kubectl ...` is the standard entry point on the node.
KCTL='k3s kubectl'
KUBECONFIG_FLAG=''  # k3s kubectl auto-discovers /etc/rancher/k3s/k3s.yaml

log "waiting for apiserver to answer (up to 2 min)"
api_ok=0
for _ in $(seq 1 24); do
  if ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       "${KCTL} ${KUBECONFIG_FLAG} get --raw=/healthz" >/dev/null 2>&1; then
    api_ok=1
    break
  fi
  sleep 5
done
if [[ "${api_ok}" -ne 1 ]]; then
  log "FAIL: apiserver /healthz did not respond"
  exit 1
fi

log "assert: exactly one Ready node"
ready=0
for _ in $(seq 1 24); do
  status="$(ssh "${ssh_opts[@]}" "root@${VM_IP}" \
              "${KCTL} get nodes --no-headers 2>/dev/null | awk '{print \$2}'" || true)"
  if [[ "${status}" == "Ready" ]]; then
    ready=1
    break
  fi
  sleep 5
done
if [[ "${ready}" -ne 1 ]]; then
  log "FAIL: node never reached Ready (last status: '${status:-<empty>}')"
  ssh "${ssh_opts[@]}" "root@${VM_IP}" "${KCTL} get nodes -o wide" >&2 || true
  exit 1
fi

# Confirm exactly one node (not 0, not >1).
node_count="$(ssh "${ssh_opts[@]}" "root@${VM_IP}" \
                "${KCTL} get nodes --no-headers 2>/dev/null | wc -l" || echo 0)"
node_count="${node_count//[^0-9]/}"
if [[ "${node_count}" != "1" ]]; then
  log "FAIL: expected exactly one node, got ${node_count}"
  exit 1
fi
log "node count = 1 Ready"

# --- smoke deploy of a tiny PSA-compliant pod + Service --------------------
#
# k3s does NOT enforce PSA-restricted by default, but we still ship a
# restricted-compliant manifest here so the same yaml works against the k8s
# flavor's verify-app-deploy.sh.
log "smoke: deploy a tiny pod + Service and curl it"
NS="smoke-${RUN_ID}"
# shellcheck disable=SC2087  # we want host-side expansion of NS into the heredoc
ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  "${KCTL} create ns ${NS}" >/dev/null

# Apply manifest. nginx-unprivileged listens on 8080 (no root needed) and
# satisfies the PSA `restricted` profile. The probe Pod uses busybox.
ssh "${ssh_opts[@]}" "root@${VM_IP}" "${KCTL} apply -n ${NS} -f -" <<'YAML' >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nginx
spec:
  replicas: 1
  selector:
    matchLabels:
      app: nginx
  template:
    metadata:
      labels:
        app: nginx
    spec:
      automountServiceAccountToken: false
      containers:
      - name: nginx
        image: nginxinc/nginx-unprivileged:stable
        ports:
        - containerPort: 8080
        securityContext:
          runAsNonRoot: true
          allowPrivilegeEscalation: false
          capabilities:
            drop: [ALL]
          seccompProfile:
            type: RuntimeDefault
---
apiVersion: v1
kind: Service
metadata:
  name: nginx
spec:
  selector:
    app: nginx
  ports:
  - port: 8080
    targetPort: 8080
YAML

log "waiting up to 3 min for deployment/nginx to become Available"
if ! ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       "${KCTL} -n ${NS} wait --for=condition=available --timeout=3m deployment/nginx" \
       >&2; then
  log "FAIL: deployment/nginx never became Available"
  ssh "${ssh_opts[@]}" "root@${VM_IP}" \
      "${KCTL} -n ${NS} get events --sort-by=.lastTimestamp" >&2 || true
  ssh "${ssh_opts[@]}" "root@${VM_IP}" \
      "${KCTL} -n ${NS} get pods -o wide" >&2 || true
  ssh "${ssh_opts[@]}" "root@${VM_IP}" \
      "${KCTL} delete ns ${NS} --wait=false --ignore-not-found" >/dev/null 2>&1 || true
  exit 1
fi

log "probing http://nginx:8080 from an in-cluster busybox pod"
# PSA-restricted-compliant probe pod (same shape as scripts/verify-app-deploy.sh).
probe_overrides='{"spec":{"automountServiceAccountToken":false,"containers":[{"name":"probe","image":"busybox:stable","stdin":true,"command":["sh","-c","wget -qO- http://nginx:8080"],"securityContext":{"runAsNonRoot":true,"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"seccompProfile":{"type":"RuntimeDefault"}}}]}}'

probe_out=$(ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  "${KCTL} run probe -n ${NS} --rm -i --restart=Never --image=busybox:stable --overrides='${probe_overrides}'" \
  2>&1) || {
    log "FAIL: probe pod exited non-zero"
    printf '%s\n' "${probe_out}" >&2
    ssh "${ssh_opts[@]}" "root@${VM_IP}" \
        "${KCTL} delete ns ${NS} --wait=false --ignore-not-found" >/dev/null 2>&1 || true
    exit 1
  }

if printf '%s' "${probe_out}" | grep -q 'Welcome to nginx'; then
  log "PASS: nginx welcome page served over ClusterIP"
else
  log "FAIL: probe did not see the nginx welcome page"
  printf '%s\n' "${probe_out}" >&2
  ssh "${ssh_opts[@]}" "root@${VM_IP}" \
      "${KCTL} delete ns ${NS} --wait=false --ignore-not-found" >/dev/null 2>&1 || true
  exit 1
fi

ssh "${ssh_opts[@]}" "root@${VM_IP}" \
    "${KCTL} delete ns ${NS} --wait=false --ignore-not-found" >/dev/null 2>&1 || true

log "ALL CHECKS PASSED"
exit 0
