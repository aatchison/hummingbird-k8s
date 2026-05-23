#!/usr/bin/env bash
# integration-workers-join.sh — End-to-end worker-join integration test.
#
# Usage:  sudo tests/integration-workers-join.sh <cp_tag> <worker_tag> [worker_count]
#         (e.g. sudo tests/integration-workers-join.sh v0.1.33 v0.1.9 2)
#
# What this verifies:
#   1. Builds a qcow2 from the published hummingbird-k8s image and stands up
#      a single-node control plane.
#   2. Mints a short-TTL kubeadm join token on the CP.
#   3. Builds a qcow2 from the published hummingbird-k8s-worker image, then
#      for each requested worker:
#        - cp --reflink=auto a private copy of the qcow2
#        - inject the join command into /etc/hummingbird/worker-join.env on
#          the qcow2 using guestfish + raw partition mount (the bootc/ostree
#          layout breaks libguestfs OS introspection — see scripts/spawn-workers.sh)
#        - virt-install (parallel)
#   4. Wait until `kubectl get nodes` shows worker_count + 1 Ready nodes.
#
# Teardown via trap unconditionally destroys + undefines every VM and removes
# every per-test qcow2. VM names are `hummingbird-it-workers-<run_id>-cp`
# and `hummingbird-it-workers-<run_id>-w<i>`, so we never touch the LIVE
# cluster on geary.
#
# Designed for a self-hosted runner with the `kvm,libvirt` labels.

set -euo pipefail

export STORAGE_DRIVER="${STORAGE_DRIVER:-vfs}"

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

CP_TAG="${1:?cp_tag required (e.g. v0.1.33)}"
WORKER_TAG="${2:?worker_tag required (e.g. v0.1.9)}"
WORKER_COUNT="${3:-2}"

RUN_ID="${GITHUB_RUN_ID:-local}"

CP_IMAGE="ghcr.io/aatchison/hummingbird-k8s:${CP_TAG}"
WORKER_IMAGE="ghcr.io/aatchison/hummingbird-k8s-worker:${WORKER_TAG}"

CP_VM="hummingbird-it-workers-${RUN_ID}-cp"
WORK="$(mktemp -d -t hb-it-workers-XXXXXX)"
KEY="${WORK}/integration-workers-key"
CP_BIB_CFG="${WORK}/cp-bib-config.toml"
WK_BIB_CFG="${WORK}/wk-bib-config.toml"
LIBVIRT_POOL_DIR="${LIBVIRT_POOL_DIR:-/var/lib/libvirt/images}"
CP_POOL_QCOW="${LIBVIRT_POOL_DIR}/${CP_VM}.qcow2"
WK_TEMPLATE_QCOW="${LIBVIRT_POOL_DIR}/hummingbird-it-workers-${RUN_ID}-template.qcow2"
BIB_IMAGE="${BIB_IMAGE:-quay.io/centos-bootc/bootc-image-builder:latest}"

# Track every worker we virt-installed so cleanup tears them ALL down — even
# the ones still in flight when something fails mid-spawn.
SPAWNED_WORKERS=()

CP_IP=""
CP_SERIAL_READER_PID=""

log() { printf '[integration-workers-join] %s\n' "$*" >&2; }

# Compute the per-domain console log path. /var/log/libvirt/qemu/ has the
# right SELinux context for qemu-user writes; fall back to /tmp if that
# directory doesn't exist (e.g. bare-metal dev box without libvirt
# pre-installed). Used by both the start path AND dump_failure_context /
# cleanup so they all agree on where the file lives.
console_path() {
  local dom="$1"
  if [[ -d /var/log/libvirt/qemu ]]; then
    printf '/var/log/libvirt/qemu/%s-console.log\n' "$dom"
  else
    printf '/tmp/%s-console.log\n' "$dom"
  fi
}

dump_failure_context() {
  log "FAILURE CONTEXT --------------------------------"
  log "CP_VM=${CP_VM} CP_IP=${CP_IP:-<unknown>}"
  log "WORKERS=${SPAWNED_WORKERS[*]:-<none>}"
  if virsh -c qemu:///system dominfo "${CP_VM}" >/dev/null 2>&1; then
    log "--- virsh list (running) ---"
    virsh -c qemu:///system list >&2 || true
  fi
  if [[ -n "${CP_IP}" ]]; then
    log "--- kubectl get nodes -o wide ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "root@${CP_IP}" \
        'KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes -o wide' >&2 2>/dev/null || true
    log "--- last 20 lines of k8s-init journal ---"
    ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "root@${CP_IP}" \
        'journalctl -u k8s-init --no-pager -n 20' >&2 2>/dev/null || true
  fi
  # CP console log (best-effort — only useful if CP boot itself failed).
  local cp_console
  cp_console="$(console_path "${CP_VM}")"
  if [[ -s "${cp_console}" ]]; then
    log "--- last 80 lines of CP console (${cp_console}) ---"
    tail -n 80 "${cp_console}" >&2 || true
  fi
  # When workers never register, the ONLY way to know why is to look at each
  # worker's worker-init.service journal (#166). The CP can't tell us
  # anything — kubeadm join is initiated FROM the worker, not the CP. First
  # try the on-host console log (no SSH dependency), then fall back to ssh.
  for w in "${SPAWNED_WORKERS[@]:-}"; do
    [[ -z "$w" ]] && continue
    local w_console
    w_console="$(console_path "$w")"
    if [[ -s "${w_console}" ]]; then
      log "--- last 120 lines of worker ${w} console (${w_console}) ---"
      tail -n 120 "${w_console}" >&2 || true
    fi
    local w_ip=""
    w_ip=$(virsh -c qemu:///system domifaddr "$w" 2>/dev/null \
            | awk '/ipv4/ {print $4}' | head -1 | cut -d/ -f1 || true)
    log "--- worker ${w} ip=${w_ip:-<none>} ---"
    if [[ -n "$w_ip" ]]; then
      ssh -i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no \
          -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
          "root@${w_ip}" \
          'ls -l /etc/hummingbird/worker-join.env 2>&1 | head -3
           systemctl status worker-init.service --no-pager 2>&1 | head -15
           echo "--- worker-init journal ---"
           journalctl -u worker-init.service --no-pager -n 60 2>&1' \
          >&2 2>/dev/null || log "(could not ssh root@${w_ip})"
    fi
  done
  log "------------------------------------------------"
}

cleanup() {
  local rc=$?
  # Stop the CP serial reader (cat <pty> >CP_CONSOLE_LOG) BEFORE dumping
  # failure context so the file has settled. Worker readers were started
  # inside parallel subshells and are already orphaned to PID 1; they exit
  # naturally when libvirt destroys their PTYs in the VM-teardown loop
  # below.
  if [[ -n "${CP_SERIAL_READER_PID:-}" ]]; then
    kill "${CP_SERIAL_READER_PID}" 2>/dev/null || true
  fi
  if [[ $rc -ne 0 ]]; then
    dump_failure_context
  fi
  log "tearing down VMs"
  for w in "${SPAWNED_WORKERS[@]:-}"; do
    [[ -z "$w" ]] && continue
    virsh -c qemu:///system destroy "${w}" >/dev/null 2>&1 || true
    virsh -c qemu:///system undefine --nvram "${w}" >/dev/null 2>&1 || true
    rm -f "${LIBVIRT_POOL_DIR}/${w}.qcow2" "$(console_path "$w")" || true
  done
  virsh -c qemu:///system destroy "${CP_VM}" >/dev/null 2>&1 || true
  virsh -c qemu:///system undefine --nvram "${CP_VM}" >/dev/null 2>&1 || true
  rm -f "${CP_POOL_QCOW}" "${WK_TEMPLATE_QCOW}" \
        "$(console_path "${CP_VM}")" || true
  rm -rf "${WORK}" || true
  rm -rf "${PODMAN_ROOT}" "${PODMAN_RUNROOT}" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

if [[ ${EUID} -ne 0 ]]; then
  log "must run as root (libvirt qemu:///system + bib loopback mounts + guestfish)"
  exit 2
fi

# --- guestfish discovery (same logic as scripts/spawn-workers.sh) ----------
if ! command -v guestfish >/dev/null 2>&1; then
  log "guestfish missing; attempting install (libguestfs-tools-c or libguestfs-tools)"
  if command -v dnf >/dev/null 2>&1; then
    dnf install -y libguestfs-tools-c >/dev/null 2>&1 || \
      dnf install -y libguestfs-tools >/dev/null 2>&1 || true
  elif command -v apt-get >/dev/null 2>&1; then
    apt-get update -q && apt-get install -y -q libguestfs-tools || true
  fi
fi
if ! command -v guestfish >/dev/null 2>&1; then
  log "ERROR: guestfish unavailable; cannot inject worker-join.env into bootc/ostree qcow2"
  exit 1
fi

# --- 0. ephemeral key + bib configs ----------------------------------------
log "preparing ephemeral SSH key at ${KEY}"
ssh-keygen -q -N '' -t ed25519 -f "${KEY}" -C "integration-workers-${RUN_ID}"
PUB="$(cat "${KEY}.pub")"
RAND_USER="ittest$((RANDOM))"

for cfg in "${CP_BIB_CFG}" "${WK_BIB_CFG}"; do
  cat >"${cfg}" <<EOF
[[customizations.user]]
name = "${RAND_USER}"
groups = ["wheel"]
key = """${PUB}"""

[[customizations.user]]
name = "root"
key = """${PUB}"""
EOF
done

# --- helper: bib build qcow2 ------------------------------------------------
# bib_build <image_ref> <bib_cfg> <dest_qcow_in_pool>
bib_build() {
  local image="$1" cfg="$2" dest="$3"
  mkdir -p "${LIBVIRT_POOL_DIR}"
  rm -rf "${LIBVIRT_POOL_DIR}/qcow2"

  if command -v docker >/dev/null && [[ -S /var/run/docker.sock ]]; then
    log "bib_build(${image}): using host docker daemon (sibling-of-runner)"
    local shared="${LIBVIRT_POOL_DIR}/integration-runs/${RUN_ID}-$(basename "${dest}" .qcow2)"
    mkdir -p "${shared}"
    cp "${cfg}" "${shared}/config.toml"
    local bib_storage="${shared}/storage"
    mkdir -p "${bib_storage}"
    docker run --rm --privileged \
      -v "${bib_storage}:/var/lib/containers/storage" \
      quay.io/podman/stable:latest \
      podman --root /var/lib/containers/storage pull "${image}"
    docker run --rm --privileged --pull=always \
      -v "${shared}/config.toml:/config.toml:ro" \
      -v "${LIBVIRT_POOL_DIR}:/output" \
      -v "${bib_storage}:/var/lib/containers/storage" \
      "${BIB_IMAGE}" \
      --type qcow2 --rootfs ext4 \
      --local \
      "${image}"
    rm -rf "${shared}" 2>/dev/null || true
  else
    log "bib_build(${image}): using podman (bare-metal path)"
    podman_isolated pull "${image}"
    podman_isolated run --rm --privileged --pull=newer \
      --net=host --cgroupns=host --cgroup-manager=cgroupfs \
      --security-opt label=disable \
      -v "${cfg}:/config.toml:ro" \
      -v "${LIBVIRT_POOL_DIR}:/output" \
      -v "${PODMAN_ROOT}:/var/lib/containers/storage" \
      "${BIB_IMAGE}" \
      --type qcow2 --rootfs ext4 \
      --local \
      "${image}"
  fi

  mv -f "${LIBVIRT_POOL_DIR}/qcow2/disk.qcow2" "${dest}"
  rmdir "${LIBVIRT_POOL_DIR}/qcow2" 2>/dev/null || true
  chown root:root "${dest}"
  chmod 0644 "${dest}"
}

# --- 1. build + boot the CP ------------------------------------------------
log "building CP qcow2 from ${CP_IMAGE}"
bib_build "${CP_IMAGE}" "${CP_BIB_CFG}" "${CP_POOL_QCOW}"

# Tee the named domain's primary serial PTY (port=0) to a host-side file
# so we can post-mortem first-boot failures even when sshd never comes up.
# Earlier attempts at `--console pty,log.file=…` or post-XML
# `<serial type='file'>` either produced unstartable domains or empty
# logs on the runner — the simplest path that actually works is to ask
# libvirt for the ttyconsole PTY AFTER `virsh start` returns and `cat` it
# into a file in the background. Best-effort with a few retries to give
# libvirt a moment to wire the PTY. Echoes the reader PID on stdout so the
# caller can kill it during cleanup.
tee_console_to_file() {
  local dom="$1" log_path="$2"
  : >"${log_path}"
  chmod 0666 "${log_path}"
  local out=""
  for _ in 1 2 3 4 5; do
    out="$(virsh -c qemu:///system ttyconsole "${dom}" 2>&1 || true)"
    if [[ -c "${out}" ]]; then
      ( cat "${out}" >"${log_path}" 2>/dev/null & echo $! )
      return 0
    fi
    sleep 1
  done
  log "(could not resolve ttyconsole PTY for ${dom}; last err: ${out:-<empty>}; console log will be empty)"
}

log "virt-installing CP ${CP_VM}"
CP_CONSOLE_LOG="$(console_path "${CP_VM}")"
virt-install --connect qemu:///system \
  --name "${CP_VM}" \
  --memory 4096 --vcpus 2 \
  --disk "${CP_POOL_QCOW},format=qcow2,bus=virtio" \
  --import \
  --os-variant fedora-unknown \
  --network network=default,model=virtio \
  --graphics none \
  --noautoconsole \
  --noreboot
log "starting CP ${CP_VM}"
if ! virsh -c qemu:///system start "${CP_VM}" 2>&1; then
  log "WARN: virsh start CP exited non-zero — domain may not be running"
fi
CP_SERIAL_READER_PID="$(tee_console_to_file "${CP_VM}" "${CP_CONSOLE_LOG}" || true)"

# Resolve CP IP.
log "waiting for CP DHCP lease (up to 3 min)"
for _ in $(seq 1 90); do
  lease="$(virsh -c qemu:///system domifaddr "${CP_VM}" --source agent 2>/dev/null \
            | awk '/ipv4/ {print $4}' | head -1 || true)"
  if [[ -z "${lease}" ]]; then
    lease="$(virsh -c qemu:///system domifaddr "${CP_VM}" 2>/dev/null \
              | awk '/ipv4/ {print $4}' | head -1 || true)"
  fi
  if [[ -n "${lease}" ]]; then
    CP_IP="${lease%/*}"
    break
  fi
  sleep 2
done
if [[ -z "${CP_IP}" ]]; then
  log "FAIL: CP never got a DHCP lease"
  exit 1
fi
log "CP_IP=${CP_IP}"

ssh_opts=(-i "${KEY}" -o BatchMode=yes -o StrictHostKeyChecking=no
          -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5)

log "waiting for CP sshd (up to 2 min)"
for _ in $(seq 1 60); do
  if ssh "${ssh_opts[@]}" "root@${CP_IP}" true >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
if ! ssh "${ssh_opts[@]}" "root@${CP_IP}" true >/dev/null 2>&1; then
  log "FAIL: CP sshd never came up"
  exit 1
fi

log "waiting for /var/lib/k8s-init.done (up to 5 min)"
init_ok=0
for _ in $(seq 1 60); do
  if ssh "${ssh_opts[@]}" "root@${CP_IP}" \
       'test -f /var/lib/k8s-init.done' >/dev/null 2>&1; then
    init_ok=1
    break
  fi
  sleep 5
done
if [[ "${init_ok}" -ne 1 ]]; then
  log "FAIL: k8s-init.done never appeared on CP"
  exit 1
fi
log "CP k8s-init.done present"

# --- 2. mint a 2h join token on the CP -------------------------------------
log "minting kubeadm join token on CP (TTL 2h)"
JOIN_CMD="$(ssh "${ssh_opts[@]}" "root@${CP_IP}" \
  'kubeadm token create --ttl 2h --print-join-command' 2>/dev/null)"
if ! grep -q '^kubeadm join' <<<"${JOIN_CMD}"; then
  log "FAIL: did not get a valid 'kubeadm join' command from CP"
  log "got: ${JOIN_CMD}"
  exit 1
fi
log "join command minted (${#JOIN_CMD} chars)"

# --- 3. build the worker template qcow2 ------------------------------------
log "building worker template qcow2 from ${WORKER_IMAGE}"
bib_build "${WORKER_IMAGE}" "${WK_BIB_CFG}" "${WK_TEMPLATE_QCOW}"

# --- 3a. helper: inject join env into a qcow2 (raw partition mount) --------
# Mirrors scripts/spawn-workers.sh — the bootc/ostree layout breaks libguestfs
# OS introspection so we mount /dev/sda4 raw and write the file into the
# active ostree deployment dir.
inject_join_env() {
  local qcow="$1" join_cmd="$2" tmpfile
  tmpfile="$(mktemp)"
  printf '%s\n' "${join_cmd}" > "${tmpfile}"

  local stateroot deploy_basename etc_path
  stateroot=$(guestfish --ro -a "${qcow}" <<'GF' 2>/dev/null | grep -v '^$' | head -1
run
mount /dev/sda4 /
ls /ostree/deploy
GF
)
  if [[ -n "${stateroot}" ]]; then
    deploy_basename=$(guestfish --ro -a "${qcow}" <<GF 2>/dev/null | grep -v '\.origin$' | grep -v '^$' | head -1
run
mount /dev/sda4 /
ls /ostree/deploy/${stateroot}/deploy
GF
)
  fi
  if [[ -n "${stateroot}" && -n "${deploy_basename}" ]]; then
    etc_path="/ostree/deploy/${stateroot}/deploy/${deploy_basename}/etc/hummingbird"
  else
    # Non-bootc fallback path.
    etc_path="/etc/hummingbird"
  fi

  guestfish --rw -a "${qcow}" <<EOF
run
mount /dev/sda4 /
mkdir-p ${etc_path}
upload ${tmpfile} ${etc_path}/worker-join.env
chmod 0600 ${etc_path}/worker-join.env
chown 0 0 ${etc_path}/worker-join.env
EOF

  rm -f "${tmpfile}"
}

# --- 4. clone + inject + virt-install workers in parallel ------------------
log "spawning ${WORKER_COUNT} worker VM(s) in parallel"

spawn_worker() {
  local idx="$1"
  local name="hummingbird-it-workers-${RUN_ID}-w${idx}"
  local qcow="${LIBVIRT_POOL_DIR}/${name}.qcow2"
  # Per-worker console log so we can post-mortem first-boot failures (#166).
  local console_log
  console_log="$(console_path "${name}")"

  # NB: send EVERY noisy subcommand's stdout to stderr so the function's
  # own stdout is reserved exclusively for the `echo "${name}"` at the end.
  # The previous code let virt-install's "Starting install... Domain
  # creation completed..." text pollute the .name capture file, which made
  # SPAWNED_WORKERS contain multi-line garbage instead of clean VM names —
  # so the later `virsh destroy "${w}"` calls in cleanup silently no-op'd
  # and the post-failure SSH-into-worker diagnostic could never look up
  # the right host (#166).
  cp --reflink=auto "${WK_TEMPLATE_QCOW}" "${qcow}" >&2
  chown root:root "${qcow}" >&2
  chmod 0644 "${qcow}" >&2

  inject_join_env "${qcow}" "${JOIN_CMD}" >&2

  # Production scripts/spawn-workers.sh uses --memory 4096 for workers; the
  # initial 2048 here was too tight and would have OOMed first-boot in the
  # already-resource-constrained runner host.
  virt-install --connect qemu:///system \
    --name "${name}" \
    --memory 4096 --vcpus 2 \
    --disk "${qcow},format=qcow2,bus=virtio" \
    --import \
    --os-variant fedora-unknown \
    --network network=default,model=virtio \
    --graphics none \
    --noautoconsole \
    --noreboot >&2
  # Surface virsh start errors to stderr — the prior `>/dev/null 2>&1 || true`
  # silently masked unstartable-domain failures, leaving cleanup confused
  # about whether the worker had actually been launched (#166).
  if ! virsh -c qemu:///system start "${name}" >&2; then
    echo "WARN: virsh start ${name} exited non-zero" >&2
  fi
  # tee_console_to_file echoes the reader PID to stdout; redirect that so it
  # doesn't pollute the function's own `echo "${name}"` below.
  tee_console_to_file "${name}" "${console_log}" >&2 || true
  echo "${name}"
}

worker_pids=()
worker_out="${WORK}/worker-spawn"
mkdir -p "${worker_out}"
for i in $(seq 1 "${WORKER_COUNT}"); do
  (
    # CRITICAL: drop the parent's EXIT trap before doing anything else.
    # Otherwise the parent's `cleanup` runs in this subshell when it exits
    # and tears down the CP VM + qcow2 + worker template — destroying the
    # very cluster we're about to join workers to. The v0.1.33+v0.1.9 dry
    # run hit exactly this and silently left the CP standing alone.
    trap - EXIT
    if spawn_worker "${i}" >"${worker_out}/${i}.name" 2>"${worker_out}/${i}.log"; then
      exit 0
    else
      exit 1
    fi
  ) &
  worker_pids+=("$!")
done

# Wait for all worker spawns. Even if one fails we want to know about it.
spawn_failed=0
for pid in "${worker_pids[@]}"; do
  if ! wait "${pid}"; then
    spawn_failed=1
  fi
done

# Collect names of every worker that was at least virt-installed so cleanup
# can tear them down even if some failed mid-spawn.
for i in $(seq 1 "${WORKER_COUNT}"); do
  if [[ -s "${worker_out}/${i}.name" ]]; then
    SPAWNED_WORKERS+=("$(cat "${worker_out}/${i}.name")")
  fi
done

if [[ "${spawn_failed}" -ne 0 ]]; then
  log "FAIL: at least one worker spawn failed"
  for i in $(seq 1 "${WORKER_COUNT}"); do
    if [[ -s "${worker_out}/${i}.log" ]]; then
      log "--- worker ${i} log ---"
      cat "${worker_out}/${i}.log" >&2 || true
    fi
  done
  exit 1
fi
log "all ${WORKER_COUNT} worker VMs virt-installed: ${SPAWNED_WORKERS[*]}"

# --- 5. wait for nodes to register + go Ready ------------------------------
EXPECTED=$(( WORKER_COUNT + 1 ))
log "waiting for ${EXPECTED} Ready nodes (up to 10 min)"

ready_ok=0
for _ in $(seq 1 60); do
  json="$(ssh "${ssh_opts[@]}" "root@${CP_IP}" \
            'KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes -o json' 2>/dev/null || true)"
  if [[ -z "${json}" ]]; then
    sleep 10
    continue
  fi
  if command -v jq >/dev/null 2>&1; then
    node_count="$(printf '%s' "${json}" | jq '.items | length')"
    ready_count="$(printf '%s' "${json}" \
      | jq '[.items[].status.conditions[]? | select(.type=="Ready" and .status=="True")] | length')"
  else
    # Crude jq-free fallback.
    node_count="$(printf '%s' "${json}" | grep -c '"kind": "Node"' || true)"
    ready_count="$(printf '%s' "${json}" | grep -c '"type": "Ready"' || true)"
  fi
  log "  nodes=${node_count} ready=${ready_count} (need ${EXPECTED})"
  if [[ "${node_count}" == "${EXPECTED}" ]] && [[ "${ready_count}" == "${EXPECTED}" ]]; then
    ready_ok=1
    break
  fi
  sleep 10
done

if [[ "${ready_ok}" -ne 1 ]]; then
  log "FAIL: never reached ${EXPECTED} Ready nodes"
  ssh "${ssh_opts[@]}" "root@${CP_IP}" \
    'KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes -o wide' >&2 || true
  exit 1
fi

log "ALL CHECKS PASSED — ${EXPECTED} Ready nodes (1 CP + ${WORKER_COUNT} workers)"
exit 0
