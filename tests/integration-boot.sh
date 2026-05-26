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
# SERIAL_LOG + SERIAL_READER_PID are populated once the VM is virt-installed.
# Ported forward from the deleted integration-boot-k3s.sh (#224) so the
# "VM boots, sshd never comes up, test times out" failure mode produces a
# diagnostic boot log instead of a bare timeout.
SERIAL_LOG=""
SERIAL_READER_PID=""
# SHARED_DIR is the bib-output staging dir used when bib runs as a sibling
# docker container. Promoted to a script-global so cleanup() can rm it
# WITHOUT having to overwrite the EXIT trap — the previous in-place
# `trap 'rm -rf "$SHARED_DIR"' EXIT` silently disabled the failure-context
# dump AND the VM teardown (the exact bug called out as #165 in the deleted
# k3s test). #224.
SHARED_DIR=""

log() { printf '[integration-boot] %s\n' "$*" >&2; }

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
  # The guest console is logged to ${SERIAL_LOG} via the PTY tee below.
  # Dump the tail so we can see boot messages even when sshd never came up
  # (#224, originally #165 in the deleted k3s test). This is the single most
  # important diagnostic when SSH times out — without it we have zero
  # visibility into what's happening inside the VM.
  if [[ -n "${SERIAL_LOG}" ]] && [[ -s "${SERIAL_LOG}" ]]; then
    log "--- last 200 lines of VM serial console (${SERIAL_LOG}) ---"
    tail -n 200 "${SERIAL_LOG}" >&2 || true
  else
    log "(no serial console log at ${SERIAL_LOG:-<unset>})"
  fi
  if [[ -n "${VM_IP}" ]]; then
    log "--- attempting ssh root@${VM_IP} 'systemctl --failed' ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "root@${VM_IP}" "systemctl --failed --no-pager" >&2 2>/dev/null || \
        log "(SSH unreachable; nothing more to gather over SSH)"
    log "--- last 30 lines of journalctl k8s-init via SSH ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "root@${VM_IP}" \
        "journalctl -u k8s-init --no-pager -n 30" >&2 2>/dev/null || \
        log "(could not pull journal — VM unreachable)"
  fi
  log "------------------------------------------------"
}

cleanup() {
  local rc=$?
  # Stop the serial reader (cat <pty> >SERIAL_LOG) BEFORE dumping failure
  # context so the file has settled. The reader was backgrounded inside a
  # subshell, so it's not a child of THIS shell — we can kill by PID but
  # `wait` would error with "not a child"; just SIGTERM and trust the
  # filesystem write to have flushed.
  if [[ -n "${SERIAL_READER_PID}" ]]; then
    kill "${SERIAL_READER_PID}" 2>/dev/null || true
  fi
  if [[ $rc -ne 0 ]]; then
    dump_failure_context
  fi
  log "tearing down VM ${VM_NAME}"
  virsh -c qemu:///system destroy "${VM_NAME}" >/dev/null 2>&1 || true
  virsh -c qemu:///system undefine --nvram "${VM_NAME}" >/dev/null 2>&1 || true
  rm -f "${POOL_QCOW}" || true
  # Preserve ${SERIAL_LOG} on failure for workflow artifact upload (#224);
  # only remove it on a clean run.
  if [[ $rc -eq 0 ]] && [[ -n "${SERIAL_LOG}" ]]; then
    rm -f "${SERIAL_LOG}" || true
  fi
  if [[ -n "${SHARED_DIR}" ]] && [[ -d "${SHARED_DIR}" ]]; then
    rm -rf "${SHARED_DIR}" 2>/dev/null || true
  fi
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
  # NB: SHARED_DIR is declared at top-of-script so cleanup() can rm it without
  # us overwriting the parent's EXIT trap (the previous in-place
  # `trap 'rm -rf "$SHARED_DIR"' EXIT` silently disabled dump_failure_context
  # AND the VM teardown — #165, recovered via #224).
  SHARED_DIR="${LIBVIRT_POOL_DIR}/integration-runs/${RUN_ID}"
  mkdir -p "${SHARED_DIR}"
  cp "${BIB_CFG}" "${SHARED_DIR}/config.toml"
  BIB_STORAGE="${SHARED_DIR}/storage"
  mkdir -p "${BIB_STORAGE}"
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
# Put the console log under /var/log/libvirt/qemu/ (the directory libvirt
# already uses for per-domain logs — guaranteed to have the right SELinux
# context + qemu-user write access) rather than LIBVIRT_POOL_DIR (where
# the file gets svirt_image_t and qemu cannot open it for write).
# Fall back to /tmp with 0666 if the libvirt log dir doesn't exist.
# Ported from the deleted integration-boot-k3s.sh (#224).
if [[ -d /var/log/libvirt/qemu ]]; then
  SERIAL_LOG="/var/log/libvirt/qemu/${VM_NAME}-console.log"
else
  SERIAL_LOG="/tmp/${VM_NAME}-console.log"
fi
: >"${SERIAL_LOG}"
chmod 0666 "${SERIAL_LOG}"
log "serial console log: ${SERIAL_LOG}"

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
# Capture stderr from `virsh start` so a real start failure surfaces in the
# test log instead of being swallowed.
log "starting VM ${VM_NAME}"
if ! virsh -c qemu:///system start "${VM_NAME}" 2>&1; then
  log "WARN: virsh start exited non-zero — domain may not be running"
fi

# Try to tee the VM's primary console PTY into ${SERIAL_LOG} so we can
# post-mortem first-boot failures (#224). NOTE: this is best-effort and
# routinely no-ops in the geary-docker runner — `virsh ttyconsole`
# returns a path like `/dev/pts/18`, but `/dev/pts/` lives in qemu's
# mount namespace on the HOST, not in the runner container's
# `/dev/pts/`. The runner sees the literal string but `[[ -c $path ]]`
# returns false because the device node doesn't exist in its filesystem
# view. A full fix would require either: (a) running the test on a
# bare-metal runner where qemu and the test driver share /dev/pts/, or
# (b) running a tiny helper on the host (via the docker socket) that
# cats the PTY. Tracked as a runner-side limitation; the SSH-based
# diagnostics in dump_failure_context remain useful when the VM at
# least reaches DHCP/userspace.
SERIAL_READER_PID=""
ttyc_err=""
for _ in 1 2 3 4 5; do
  ttyc_err="$(virsh -c qemu:///system ttyconsole "${VM_NAME}" 2>&1 || true)"
  if [[ -c "${ttyc_err}" ]]; then
    pty="${ttyc_err}"
    ( cat "$pty" >"${SERIAL_LOG}" 2>/dev/null & echo $! ) >"${WORK}/serial.pid"
    SERIAL_READER_PID="$(cat "${WORK}/serial.pid")"
    log "tee'ing ${pty} -> ${SERIAL_LOG} (pid=${SERIAL_READER_PID})"
    break
  fi
  sleep 1
done
if [[ -z "$SERIAL_READER_PID" ]]; then
  log "(no PTY tee — virsh returned '${ttyc_err:-<empty>}' but it lives in the host's mount namespace, not the runner's. SSH-based diagnostics only.)"
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

# --- C. NetworkPolicy enforcement (#6, #94) --------------------------------
#
# Cilium's job is to actually enforce NetworkPolicy resources. A naked
# kubernetes cluster with a non-enforcing CNI happily ignores them, which
# would silently turn the project's "we run Cilium" claim into a lie.
# Smoke test:
#   1. Apply deny-all NetworkPolicy in `default`
#   2. Create curl-source + nginx-target pods (PSA-restricted-compliant)
#   3. Assert curl from source → target TIMES OUT
#   4. Delete the policy
#   5. Assert curl now succeeds
#   6. Clean up
log "assert: NetworkPolicy deny-all is enforced by Cilium"
NP_NS="np-smoke-${RUN_ID}"

np_cleanup() {
  ssh "${ssh_opts[@]}" "root@${VM_IP}" \
    "KUBECONFIG=/etc/kubernetes/admin.conf kubectl delete ns ${NP_NS} --wait=false --ignore-not-found" \
    >/dev/null 2>&1 || true
}

# Layer a per-section cleanup on top of the global teardown trap.
NP_CLEANUP_REGISTERED=1
trap '{ np_cleanup; cleanup; }' EXIT

ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  "KUBECONFIG=/etc/kubernetes/admin.conf kubectl create ns ${NP_NS}" >/dev/null

# nginx target — PSA-restricted-compliant. Listens on 8080.
ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  "KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -n ${NP_NS} -f -" <<'YAML' >/dev/null
apiVersion: v1
kind: Pod
metadata:
  name: target
  labels:
    app: target
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
  name: target
spec:
  selector:
    app: target
  ports:
  - port: 8080
    targetPort: 8080
YAML

log "waiting for target pod Ready (up to 2 min)"
if ! ssh "${ssh_opts[@]}" "root@${VM_IP}" \
       "KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n ${NP_NS} wait --for=condition=ready --timeout=2m pod/target" \
       >&2; then
  log "FAIL: target pod never became Ready"
  ssh "${ssh_opts[@]}" "root@${VM_IP}" \
      "KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n ${NP_NS} get events --sort-by=.lastTimestamp" >&2 || true
  exit 1
fi

# Apply deny-all ingress NetworkPolicy in NP_NS.
ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  "KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -n ${NP_NS} -f -" <<'YAML' >/dev/null
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: deny-all
spec:
  podSelector: {}
  policyTypes:
  - Ingress
YAML

# Give Cilium a moment to translate the policy into its endpoint state. The
# CiliumNetworkPolicy controller reconciles within a few seconds.
sleep 5

# Probe with deny-all in place — expect a curl timeout (exit non-zero).
# `--connect-timeout 3` + `--max-time 5` keeps a hung connection bounded.
curl_overrides='{"spec":{"automountServiceAccountToken":false,"containers":[{"name":"probe","image":"curlimages/curl:8.10.1","stdin":true,"command":["sh","-c","curl --connect-timeout 3 --max-time 5 -fsS http://target:8080"],"securityContext":{"runAsNonRoot":true,"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"seccompProfile":{"type":"RuntimeDefault"}}}]}}'

log "probe with deny-all in place — expect curl to TIMEOUT"
set +e
deny_out=$(ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  "KUBECONFIG=/etc/kubernetes/admin.conf kubectl run probe-deny -n ${NP_NS} --rm -i --restart=Never --image=curlimages/curl:8.10.1 --overrides='${curl_overrides}'" \
  2>&1)
deny_rc=$?
set -e
log "deny probe rc=${deny_rc}"
if [[ "${deny_rc}" -eq 0 ]] && printf '%s' "${deny_out}" | grep -q '<html'; then
  log "FAIL: deny-all NetworkPolicy did not block traffic — got an HTTP response"
  printf '%s\n' "${deny_out}" >&2
  exit 1
fi
log "PASS: deny-all NetworkPolicy blocked traffic as expected"

# Remove the policy.
ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  "KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n ${NP_NS} delete networkpolicy deny-all" >/dev/null

# Cilium needs a moment to re-converge after the policy goes away.
sleep 5

log "probe without policy — expect curl to SUCCEED"
allow_rc=0
allow_out=""
for attempt in 1 2 3; do
  set +e
  allow_out=$(ssh "${ssh_opts[@]}" "root@${VM_IP}" \
    "KUBECONFIG=/etc/kubernetes/admin.conf kubectl run probe-allow-${attempt} -n ${NP_NS} --rm -i --restart=Never --image=curlimages/curl:8.10.1 --overrides='${curl_overrides}'" \
    2>&1)
  allow_rc=$?
  set -e
  if [[ "${allow_rc}" -eq 0 ]] && printf '%s' "${allow_out}" | grep -q '<html'; then
    break
  fi
  sleep 3
done

if [[ "${allow_rc}" -ne 0 ]] || ! printf '%s' "${allow_out}" | grep -q '<html'; then
  log "FAIL: traffic did not flow after removing NetworkPolicy"
  printf '%s\n' "${allow_out}" >&2
  exit 1
fi
log "PASS: traffic flows after policy removed"

np_cleanup
# Restore the original teardown trap.
trap cleanup EXIT
unset NP_CLEANUP_REGISTERED

# --- D. App-deploy smoke (#NEW) --------------------------------------------
#
# Run scripts/verify-app-deploy.sh against the test VM. This exercises a
# normal nginx Deployment + Service + busybox probe under PSA-restricted —
# i.e. the realistic happy path for a tenant workload.
log "assert: verify-app-deploy.sh PASS (run on CP via SSH)"
scp "${ssh_opts[@]}" \
  "$(dirname "$0")/../scripts/verify-app-deploy.sh" \
  "root@${VM_IP}:/tmp/verify-app-deploy.sh" >/dev/null
ssh "${ssh_opts[@]}" "root@${VM_IP}" \
  'chmod +x /tmp/verify-app-deploy.sh && KUBECONFIG=/etc/kubernetes/admin.conf /tmp/verify-app-deploy.sh'

log "ALL CHECKS PASSED"
exit 0
