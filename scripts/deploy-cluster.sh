#!/usr/bin/env bash
# scripts/deploy-cluster.sh — Hybrid bib + cloud-init cluster orchestrator.
#
# Deploys 1 control plane + N workers on the local KVM host from a single
# config file. The "hybrid" design splits state along a clear seam:
#
#   bib-baked (static, in the image):
#     - Default user account + SSH pubkey set
#     - SSH hardening, kubelet protect-kernel-defaults, PSA, audit
#     - All image content (kubeadm, cri-o, kubelet, cilium prep, etc.)
#
#   cloud-init seed (per-VM, dynamic):
#     - Hostname
#     - For workers: /etc/hummingbird/worker-join.env (the kubeadm join
#       command minted from the live CP — short-TTL, never baked)
#     - First-boot runcmd: `bootc switch` to the GHCR ref; enable the
#       auto-update timer on the CP (overrides #48's opt-out)
#
# Why this is distinct from `make k8s && make workers`:
#
#   make k8s + make workers is the "dev iteration" path: build locally,
#   inject the worker join via guestfish-on-qcow2. That path works but
#   bypasses cloud-init entirely and relies on libguestfs fishing the
#   ostree deployment dir out of a bootc image (#libguestfs-ostree note
#   in the repo).
#
#   This script is the "real deploy" path: ENABLE_CLOUD_INIT=1 images,
#   per-VM NoCloud seed ISOs attached at virt-install, worker join via
#   cloud-init's write_files (no offline qcow2 mutation).
#
# Usage:
#   sudo bash scripts/deploy-cluster.sh [path/to/cluster.local.conf]
#
# If no path is given, defaults to ./cluster.local.conf in the repo root.

set -euo pipefail

# ---- Locate self / repo root ------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# shellcheck source=../lib/build-common.sh
source "${REPO_ROOT}/lib/build-common.sh"
# shellcheck source=lib/cloud-init-seed.sh
source "${SCRIPT_DIR}/lib/cloud-init-seed.sh"

log()  { printf '[deploy-cluster] %s\n' "$*" >&2; }
fail() { printf '[deploy-cluster] ERROR: %s\n' "$*" >&2; exit 1; }

# ---- Root + arg parsing -----------------------------------------------------

if [[ $EUID -ne 0 ]]; then
  fail "must be run as root — libvirt qemu:///system + bootc-image-builder need it. Try: sudo bash $0 [config]"
fi
: "${SUDO_USER:?must be invoked via sudo so SSH to the freshly-booted CP uses the calling-user known_hosts/key}"

CONFIG_PATH="${1:-${REPO_ROOT}/cluster.local.conf}"
[[ -r "$CONFIG_PATH" ]] || fail "config not readable: $CONFIG_PATH (start from cluster.example.conf)"

# Track seed ISOs we create so the failure-trap can clean them up without
# touching VMs the operator might still want to debug.
SEED_ISOS=()
cleanup_on_failure() {
  local rc=$?
  if (( rc != 0 )) && (( ${#SEED_ISOS[@]} > 0 )); then
    log "deploy failed (rc=${rc}); cleaning up half-built seed ISOs."
    local s
    for s in "${SEED_ISOS[@]}"; do
      [[ -e "$s" ]] && rm -f "$s"
    done
  fi
}
trap cleanup_on_failure EXIT

# ---- Source config + validate ----------------------------------------------

# shellcheck disable=SC1090
source "$CONFIG_PATH"

# Required scalars.
: "${CP_NAME:?CP_NAME is required in $CONFIG_PATH}"
: "${SSH_PUBKEY_FILE:?SSH_PUBKEY_FILE is required in $CONFIG_PATH}"
: "${IMAGE_SOURCE:?IMAGE_SOURCE is required (ghcr or local) in $CONFIG_PATH}"

# Defaults for optional knobs.
: "${GHCR_TAG:=latest}"
: "${ENABLE_CLOUD_INIT:=0}"
: "${AUTO_UPDATE_CP:=true}"
: "${SWITCH_TO_GHCR:=true}"
: "${CP_MEMORY:=8192}"
: "${CP_VCPUS:=4}"
: "${WORKER_MEMORY:=4096}"
: "${WORKER_VCPUS:=2}"
: "${POOL_DIR:=/var/lib/libvirt/images}"
: "${RUN_VERIFY:=false}"
: "${KVM_HOST:=}"
# Match spawn-workers.sh's retry tuning so CP-readiness waits behave the same.
: "${CP_READY_RETRIES:=60}"
: "${CP_READY_SLEEP:=10}"
: "${TOKEN_TTL:=2h}"

# Default WORKER_NAMES to a 2-element array if unset/empty.
if [[ -z "${WORKER_NAMES+x}" || ${#WORKER_NAMES[@]} -eq 0 ]]; then
  log "WORKER_NAMES not set — defaulting to (${CP_NAME}-w1 ${CP_NAME}-w2)"
  WORKER_NAMES=("${CP_NAME}-w1" "${CP_NAME}-w2")
fi

# Hard validation.
[[ "$ENABLE_CLOUD_INIT" = "1" ]] || fail "ENABLE_CLOUD_INIT must be 1 for this flow (got '$ENABLE_CLOUD_INIT'). The deploy-cluster path requires cloud-init in the image to inject per-VM hostname + worker join + bootc switch."
[[ -r "$SSH_PUBKEY_FILE" ]] || fail "SSH_PUBKEY_FILE not readable: $SSH_PUBKEY_FILE"
case "$IMAGE_SOURCE" in
  ghcr|local) ;;
  *) fail "IMAGE_SOURCE must be 'ghcr' or 'local' (got '$IMAGE_SOURCE')" ;;
esac
case "$AUTO_UPDATE_CP" in true|false) ;; *) fail "AUTO_UPDATE_CP must be true or false (got '$AUTO_UPDATE_CP')" ;; esac
case "$SWITCH_TO_GHCR" in true|false) ;; *) fail "SWITCH_TO_GHCR must be true or false (got '$SWITCH_TO_GHCR')" ;; esac

SSH_PUBKEY_CONTENT="$(< "$SSH_PUBKEY_FILE")"
[[ -n "$SSH_PUBKEY_CONTENT" ]] || fail "SSH_PUBKEY_FILE is empty: $SSH_PUBKEY_FILE"

log "config OK: CP=${CP_NAME}, workers=(${WORKER_NAMES[*]}), source=${IMAGE_SOURCE}, tag=${GHCR_TAG}"

# ---- Image acquisition ------------------------------------------------------

CP_IMAGE_REF=""
WORKER_IMAGE_REF=""

case "$IMAGE_SOURCE" in
  ghcr)
    CP_IMAGE_REF="ghcr.io/aatchison/hummingbird-k8s:${GHCR_TAG}"
    WORKER_IMAGE_REF="ghcr.io/aatchison/hummingbird-k8s-worker:${GHCR_TAG}"
    log "pulling ${CP_IMAGE_REF}"
    podman pull "$CP_IMAGE_REF"
    log "pulling ${WORKER_IMAGE_REF}"
    podman pull "$WORKER_IMAGE_REF"
    ;;
  local)
    CP_IMAGE_REF="localhost/hummingbird-k8s:latest"
    WORKER_IMAGE_REF="localhost/hummingbird-k8s-worker:latest"
    log "building local images with ENABLE_CLOUD_INIT=1"
    ENABLE_CLOUD_INIT=1 make -C "$REPO_ROOT" image-k8s-with-cloud-init
    ENABLE_CLOUD_INIT=1 make -C "$REPO_ROOT" image-worker-with-cloud-init
    ;;
esac

# ---- bib qcow2 per flavor ---------------------------------------------------
# Reuse lib/build-common.sh's render_bib_config + build_qcow2. We render
# once per flavor; the resulting qcow2 lives under POOL_DIR/<NAME>.qcow2
# and gets cloned per-VM below.

BIB_CFG_CP="${REPO_ROOT}/bib-config-deploy-cp.toml"
BIB_CFG_WORKER="${REPO_ROOT}/bib-config-deploy-worker.toml"

# The bib config bakes ONE static pubkey set; render once and reuse.
render_bib_config > "$BIB_CFG_CP"
cp "$BIB_CFG_CP" "$BIB_CFG_WORKER"

# Template names live under POOL_DIR; clone per-VM below so the operator
# can redeploy individual nodes without rebuilding the whole template.
CP_TEMPLATE_NAME="hummingbird-k8s-deploy"
WORKER_TEMPLATE_NAME="hummingbird-k8s-worker-deploy"

log "building CP qcow2 template"
build_qcow2 "$CP_IMAGE_REF" "$CP_TEMPLATE_NAME" "$BIB_CFG_CP"

log "building worker qcow2 template"
build_qcow2 "$WORKER_IMAGE_REF" "$WORKER_TEMPLATE_NAME" "$BIB_CFG_WORKER"

CP_TEMPLATE_QCOW="${POOL_DIR}/${CP_TEMPLATE_NAME}.qcow2"
WORKER_TEMPLATE_QCOW="${POOL_DIR}/${WORKER_TEMPLATE_NAME}.qcow2"

# ---- CP cloud-init user-data + seed -----------------------------------------

CP_QCOW="${POOL_DIR}/${CP_NAME}.qcow2"
CP_SEED="${POOL_DIR}/${CP_NAME}-seed.iso"
CP_USER_DATA="$(mktemp -t hbird-cp-userdata-XXXXXX.yaml)"

{
  printf '#cloud-config\n'
  printf 'hostname: %s\n' "$CP_NAME"
  printf 'ssh_authorized_keys:\n'
  printf '  - %s\n' "$SSH_PUBKEY_CONTENT"
  # Only emit the runcmd block when at least one entry is needed — keeps
  # the user-data clean and matches the design constraint ("don't leave
  # runcmd in and rely on systemctl no-op").
  if [[ "$SWITCH_TO_GHCR" = "true" || "$AUTO_UPDATE_CP" = "true" ]]; then
    printf 'runcmd:\n'
    if [[ "$SWITCH_TO_GHCR" = "true" ]]; then
      printf '  - [ bootc, switch, ghcr.io/aatchison/hummingbird-k8s:%s ]\n' "$GHCR_TAG"
    fi
    if [[ "$AUTO_UPDATE_CP" = "true" ]]; then
      printf '  - [ systemctl, enable, --now, bootc-fetch-apply-updates.timer ]\n'
    fi
  fi
} > "$CP_USER_DATA"

log "building CP cloud-init seed ${CP_SEED}"
build_cloud_init_seed "$CP_NAME" "$CP_USER_DATA" "$CP_SEED"
SEED_ISOS+=("$CP_SEED")
rm -f "$CP_USER_DATA"

# ---- Stage CP qcow2 + virt-install ------------------------------------------

if virsh -c qemu:///system dominfo "$CP_NAME" >/dev/null 2>&1; then
  fail "CP VM '$CP_NAME' is already defined — refusing to overwrite. Tear down with 'virsh destroy/undefine $CP_NAME' (or 'sudo make clean-vms') and retry."
fi

log "cloning CP qcow2 -> $CP_QCOW"
cp --reflink=auto "$CP_TEMPLATE_QCOW" "$CP_QCOW"
chown root:root "$CP_QCOW"
chmod 0644 "$CP_QCOW"

log "virt-install ${CP_NAME} (memory=${CP_MEMORY} vcpus=${CP_VCPUS})"
virt-install --connect qemu:///system \
  --name "$CP_NAME" \
  --memory "$CP_MEMORY" --vcpus "$CP_VCPUS" \
  --disk "$CP_QCOW",format=qcow2,bus=virtio \
  --disk path="$CP_SEED",device=cdrom,readonly=on \
  --import \
  --os-variant fedora-unknown \
  --network network=default,model=virtio \
  --graphics vnc,listen=127.0.0.1 \
  --noautoconsole

# ---- Wait for CP Ready ------------------------------------------------------

resolve_cp_ip() {
  virsh -c qemu:///system domifaddr "$CP_NAME" 2>/dev/null \
    | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}'
}

log "waiting for CP IP to appear via DHCP..."
CP_IP=""
for _ in $(seq 1 60); do
  CP_IP="$(resolve_cp_ip || true)"
  [[ -n "$CP_IP" ]] && break
  sleep 5
done
[[ -n "$CP_IP" ]] || fail "could not resolve CP IP after ~5 minutes — inspect 'virsh -c qemu:///system console $CP_NAME'"
log "CP IP: $CP_IP"

# SSH-as-the-sudo-user (root authorized_keys is baked from that user's
# pubkey by lib/build-common.sh). Matches spawn-workers.sh's pattern.
cp_ssh() {
  sudo -u "$SUDO_USER" ssh \
    -o StrictHostKeyChecking=accept-new \
    -o ConnectTimeout=10 \
    -o BatchMode=yes \
    "root@${CP_IP}" "$@"
}

log "polling 'kubectl get nodes' on CP until it reports Ready (max ~$((CP_READY_RETRIES * CP_READY_SLEEP))s)"
CP_READY=0
for attempt in $(seq 1 "$CP_READY_RETRIES"); do
  if cp_ssh "kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes --no-headers 2>/dev/null | awk '\$2==\"Ready\"' | grep -q ." ; then
    CP_READY=1
    log "CP Ready (after ${attempt} attempt(s))"
    break
  fi
  log "CP not Ready yet (attempt ${attempt}/${CP_READY_RETRIES}); sleeping ${CP_READY_SLEEP}s"
  sleep "$CP_READY_SLEEP"
done
(( CP_READY == 1 )) || fail "CP never reached Ready — inspect 'ssh root@${CP_IP} journalctl -u k8s-init.service'"

# ---- Mint kubeadm join token from the live CP -------------------------------

log "minting ${TOKEN_TTL}-TTL kubeadm join token from CP"
JOIN_CMD="$(cp_ssh "kubeadm token create --ttl ${TOKEN_TTL} --print-join-command" 2>/dev/null || true)"
[[ "$JOIN_CMD" == kubeadm\ join* ]] || fail "did not get a valid 'kubeadm join ...' command from the CP. Got: ${JOIN_CMD:-<empty>}"

# ---- Per-worker seed + virt-install (parallel) ------------------------------

worker_user_data() {
  local worker_name="$1" out_file="$2"
  {
    printf '#cloud-config\n'
    printf 'hostname: %s\n' "$worker_name"
    printf 'ssh_authorized_keys:\n'
    printf '  - %s\n' "$SSH_PUBKEY_CONTENT"
    printf 'write_files:\n'
    printf '  - path: /etc/hummingbird/worker-join.env\n'
    printf '    owner: root:root\n'
    printf "    permissions: '0600'\n"
    printf '    content: |\n'
    # Indent the join command exactly 6 spaces to match YAML block scalar
    # under "content: |" at 4-space indent.
    printf '      %s\n' "$JOIN_CMD"
    if [[ "$SWITCH_TO_GHCR" = "true" ]]; then
      printf 'runcmd:\n'
      printf '  - [ bootc, switch, ghcr.io/aatchison/hummingbird-k8s-worker:%s ]\n' "$GHCR_TAG"
    fi
  } > "$out_file"
}

WORKER_PIDS=()
for w in "${WORKER_NAMES[@]}"; do
  if virsh -c qemu:///system dominfo "$w" >/dev/null 2>&1; then
    fail "worker VM '$w' is already defined — refusing to overwrite. Tear down first."
  fi

  w_qcow="${POOL_DIR}/${w}.qcow2"
  w_seed="${POOL_DIR}/${w}-seed.iso"
  w_userdata="$(mktemp -t hbird-w-userdata-XXXXXX.yaml)"

  worker_user_data "$w" "$w_userdata"
  build_cloud_init_seed "$w" "$w_userdata" "$w_seed"
  SEED_ISOS+=("$w_seed")
  rm -f "$w_userdata"

  log "cloning worker qcow2 -> $w_qcow"
  cp --reflink=auto "$WORKER_TEMPLATE_QCOW" "$w_qcow"
  chown root:root "$w_qcow"
  chmod 0644 "$w_qcow"

  log "virt-install ${w} (memory=${WORKER_MEMORY} vcpus=${WORKER_VCPUS}) [bg]"
  virt-install --connect qemu:///system \
    --name "$w" \
    --memory "$WORKER_MEMORY" --vcpus "$WORKER_VCPUS" \
    --disk "$w_qcow",format=qcow2,bus=virtio \
    --disk path="$w_seed",device=cdrom,readonly=on \
    --import \
    --os-variant fedora-unknown \
    --network network=default,model=virtio \
    --graphics vnc,listen=127.0.0.1 \
    --noautoconsole &
  WORKER_PIDS+=("$!")
done

# Wait for all worker virt-installs to return. virt-install --noautoconsole
# returns once the domain is defined + started — actual k8s join happens
# inside the guest and is observed by the readiness poll below.
log "waiting for ${#WORKER_PIDS[@]} virt-install processes to settle"
WAIT_RC=0
for pid in "${WORKER_PIDS[@]}"; do
  wait "$pid" || WAIT_RC=$?
done
(( WAIT_RC == 0 )) || fail "one or more worker virt-installs failed (rc=${WAIT_RC})"

# ---- Wait for full cluster Ready --------------------------------------------

EXPECTED_NODES=$(( 1 + ${#WORKER_NAMES[@]} ))
log "polling cluster until ${EXPECTED_NODES} nodes are Ready"
CLUSTER_READY=0
for attempt in $(seq 1 "$CP_READY_RETRIES"); do
  ready_count="$(cp_ssh "kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes --no-headers 2>/dev/null | awk '\$2==\"Ready\"' | wc -l" 2>/dev/null || echo 0)"
  ready_count="${ready_count//[^0-9]/}"
  : "${ready_count:=0}"
  if (( ready_count >= EXPECTED_NODES )); then
    CLUSTER_READY=1
    log "cluster Ready: ${ready_count}/${EXPECTED_NODES} nodes (after ${attempt} attempt(s))"
    break
  fi
  log "cluster not Ready: ${ready_count}/${EXPECTED_NODES} (attempt ${attempt}/${CP_READY_RETRIES})"
  sleep "$CP_READY_SLEEP"
done
(( CLUSTER_READY == 1 )) || fail "cluster never reached ${EXPECTED_NODES} Ready nodes — inspect 'ssh root@${CP_IP} kubectl get nodes' and per-worker 'journalctl -u worker-init.service'"

# ---- Optional verification --------------------------------------------------

if [[ "$RUN_VERIFY" = "true" ]]; then
  if [[ -x "${REPO_ROOT}/scripts/verify-app-deploy.sh" ]]; then
    log "running scripts/verify-app-deploy.sh"
    bash "${REPO_ROOT}/scripts/verify-app-deploy.sh" || log "verify-app-deploy.sh exited non-zero (cluster is up; verifier failure is informational)"
  else
    log "RUN_VERIFY=true but scripts/verify-app-deploy.sh not found/executable; skipping"
  fi
fi

# ---- Summary ----------------------------------------------------------------

log ""
log "=============================================================="
log "Cluster deployed."
log "  CP:         ${CP_NAME} (${CP_IP})"
log "  Workers:    ${WORKER_NAMES[*]}"
log "  Image src:  ${IMAGE_SOURCE} (tag=${GHCR_TAG})"
log "  Kubeconfig: root@${CP_IP}:/etc/kubernetes/admin.conf"
if [[ -n "$KVM_HOST" ]]; then
  log "  Remote access:"
  log "    KVM_HOST=${KVM_HOST} bash scripts/kubectl-k8s.sh get nodes"
else
  log "  Local access (on this KVM host):"
  log "    ssh root@${CP_IP} kubectl get nodes"
fi
log "=============================================================="

# Disarm the failure-trap cleanup so the seed ISOs persist for libvirt
# (a `virsh start` after `destroy` re-attaches them).
trap - EXIT
