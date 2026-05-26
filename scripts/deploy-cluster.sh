#!/usr/bin/env bash
# Shared SSH/virsh/log helpers live in lib/build-common.sh; see docs/development.md.
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
# This is the canonical (and, since #216, only) supported way to stand
# up a Hummingbird cluster: ENABLE_CLOUD_INIT=1 images, per-VM NoCloud
# seed ISOs attached at virt-install time, worker join via cloud-init's
# write_files (no offline qcow2 mutation, no libguestfs fishing the
# ostree deployment dir out of the bootc image).
#
# Usage:
#   sudo bash scripts/deploy-cluster.sh [path/to/cluster.local.conf]
#
# If no path is given, defaults to ./cluster.local.conf in the repo root.

set -euo pipefail

# ---- Locate self / repo root ------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# render_cp_user_data — emit the CP cloud-init user-data YAML to stdout.
# Inputs (env vars): CP_NAME, SSH_PUBKEY_CONTENT, GHCR_TAG, SWITCH_TO_GHCR,
# AUTO_UPDATE_CP, BOOTC_UPDATE_SCHEDULE, BOOTC_UPDATE_REPO_K8S.
#
# Extracted from the inline `{ ... } > $CP_USER_DATA` block so
# tests/scripts/deploy-cluster.bats can exercise the rendered output
# without invoking the rest of the script (which requires root + libvirt).
# Defined here, above the root/sudo checks, so the source-only guard
# below can short-circuit and expose the function to tests. (#181 round-2
# review.)
render_cp_user_data() {
  printf '#cloud-config\n'
  printf 'hostname: %s\n' "$CP_NAME"
  # Set the key on root explicitly. The top-level `ssh_authorized_keys`
  # defaults to cloud-init's "default user" which on Fedora is `fedora` —
  # that fights the Hummingbird sudoless-root-only model.
  printf 'disable_root: false\n'
  printf 'users:\n'
  printf '  - name: root\n'
  printf '    ssh_authorized_keys:\n'
  printf '      - %s\n' "$SSH_PUBKEY_CONTENT"
  # write_files for bootc-semver-update overrides. Only emit the block
  # when at least one override is set, otherwise the YAML stays clean
  # (no empty write_files array).
  if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" || -n "${BOOTC_UPDATE_REPO_K8S:-}" ]]; then
    printf 'write_files:\n'
    if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
      # Drop-in to override the image's OnCalendar=. Empty OnCalendar=
      # first clears the default; the second line sets the override.
      printf '  - path: /etc/systemd/system/bootc-semver-update.timer.d/schedule.conf\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0644'\n"
      printf '    content: |\n'
      printf '      [Timer]\n'
      printf '      OnCalendar=\n'
      printf '      OnCalendar=%s\n' "$BOOTC_UPDATE_SCHEDULE"
    fi
    if [[ -n "${BOOTC_UPDATE_REPO_K8S:-}" ]]; then
      printf '  - path: /etc/hummingbird/bootc-update.env\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0644'\n"
      printf '    content: |\n'
      printf '      REPO=%s\n' "$BOOTC_UPDATE_REPO_K8S"
      printf '      PREFIX=v\n'
    fi
  fi
  # Always emit a runcmd block now: the AUTO_UPDATE_CP=false branch needs
  # to actively disable the timer (the image's preset enables it
  # unconditionally on factory reset, so AUTO_UPDATE_CP=false alone was a
  # no-op pre-#181-round-2). #181 round-2 review.
  printf 'runcmd:\n'
  if [[ "$SWITCH_TO_GHCR" = "true" ]]; then
    printf '  - [ bootc, switch, ghcr.io/aatchison/hummingbird-k8s:%s ]\n' "$GHCR_TAG"
  fi
  if [[ "$AUTO_UPDATE_CP" = "true" ]]; then
    # Enable the semver-aware updater (this PR's mechanism) — the image's
    # preset already enables it on a fresh boot, but a runtime `systemctl
    # enable --now` is the belt-and-suspenders path that also makes the
    # timer fire its OnBootSec=15min window immediately rather than after
    # the next reboot. The legacy bootc-fetch-apply-updates.timer is
    # disabled in the new preset; re-enabling it here would fight the
    # operator's "advance only on new semver tags" intent.
    printf '  - [ systemctl, enable, --now, bootc-semver-update.timer ]\n'
    # Belt-and-suspenders: disable the legacy timer too, in case a
    # pre-#181 host is being re-cloud-inited (the new image's preset
    # would have already disabled it on first boot, but explicit disable
    # is cheap and idempotent on already-disabled units). (#181 round-2
    # review.)
    printf '  - [ systemctl, disable, --now, bootc-fetch-apply-updates.timer ]\n'
  else
    # AUTO_UPDATE_CP=false: actively counter the preset's unconditional
    # enable so the operator's "no auto-updates on the CP" intent is
    # honored. (#181 round-2 review.)
    printf '  - [ systemctl, disable, --now, bootc-semver-update.timer ]\n'
  fi
  if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
    # Re-read the drop-in cloud-init just wrote, then restart the timer
    # so the override takes effect this boot (not just next).
    printf '  - [ systemctl, daemon-reload ]\n'
    printf '  - [ systemctl, restart, bootc-semver-update.timer ]\n'
  fi
}

# Source-only mode for bats: when HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY=1, return
# from `source` here so the test can call render_cp_user_data without the
# script's root/libvirt orchestration kicking in. (#181 round-2 review.)
if [[ "${HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY:-0}" = 1 ]]; then
  return 0
fi

# shellcheck source=../lib/build-common.sh
source "${REPO_ROOT}/lib/build-common.sh"
# shellcheck source=lib/cloud-init-seed.sh
source "${SCRIPT_DIR}/lib/cloud-init-seed.sh"

setup_logging "[deploy-cluster]"

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
# Optional bootc-semver-update overrides. Empty = use the image default
# (timer schedule baked in containers/shared/, repo baked per-flavor at
# build time). When non-empty, cloud-init emits a write_files entry that
# overrides the image default on first boot.
: "${BOOTC_UPDATE_SCHEDULE:=}"
: "${BOOTC_UPDATE_REPO_K8S:=}"
: "${BOOTC_UPDATE_REPO_WORKER:=}"
# Match spawn-workers.sh's retry tuning so CP-readiness waits behave the same.
: "${CP_READY_RETRIES:=60}"
: "${CP_READY_SLEEP:=10}"
: "${TOKEN_TTL:=2h}"

# Default WORKER_NAMES to a 2-element array if completely unset.
# Distinguish "unset" (legacy configs that never declared WORKER_NAMES —
# preserve historical 2-worker default) from "explicit empty array"
# (`WORKER_NAMES=()` — operator's documented way to ask for CP-only;
# README's Migration table promises this honors CP-only intent).
#
# Note: bash's `${arr+x}` parameter expansion returns empty for both
# unset arrays AND empty-but-declared arrays, so it can't tell those
# apart. `declare -p` IS reliable: it exits non-zero for unset names
# and prints `declare -a NAME=()` for explicit empty arrays.
if ! declare -p WORKER_NAMES >/dev/null 2>&1; then
  log "WORKER_NAMES not set — defaulting to (${CP_NAME}-w1 ${CP_NAME}-w2)"
  WORKER_NAMES=("${CP_NAME}-w1" "${CP_NAME}-w2")
elif [[ ${#WORKER_NAMES[@]} -eq 0 ]]; then
  log "WORKER_NAMES=() — CP-only deploy (no workers)"
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

# Thread SSH_PUBKEY_FILE through to lib/build-common.sh, which reads
# SSH_PUBKEY_FILES (colon-separated). Without this export, build_qcow2 falls
# back to ~SUDO_USER/.ssh/id_ed25519.pub and gives a confusing error when the
# operator pointed at a different key.
export SSH_PUBKEY_FILES="$SSH_PUBKEY_FILE"

log "config OK: CP=${CP_NAME}, workers=(${WORKER_NAMES[*]}), source=${IMAGE_SOURCE}, tag=${GHCR_TAG}"

# ---- Image acquisition ------------------------------------------------------

CP_IMAGE_REF=""
WORKER_IMAGE_REF=""

case "$IMAGE_SOURCE" in
  ghcr)
    CP_IMAGE_REF="ghcr.io/aatchison/hummingbird-k8s:${GHCR_TAG}"
    WORKER_IMAGE_REF="ghcr.io/aatchison/hummingbird-k8s-worker:${GHCR_TAG}"
    # Isolation contract (issue #199): the outer `podman pull` MUST
    # land in the same graphroot build_qcow2 will look in below; bare
    # `podman pull` would land in /var/lib/containers/storage while
    # build_qcow2 uses --root $PODMAN_ROOT and BIB's --local lookup
    # would then fail image-not-found.
    mapfile -t _PODMAN_OPTS < <(podman_storage_opts)
    log "pulling ${CP_IMAGE_REF}"
    podman "${_PODMAN_OPTS[@]}" pull "$CP_IMAGE_REF"
    log "pulling ${WORKER_IMAGE_REF}"
    podman "${_PODMAN_OPTS[@]}" pull "$WORKER_IMAGE_REF"
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

# render_cp_user_data is defined near the top of this script (above the
# root/sudo checks) so the source-only test guard can expose it.
render_cp_user_data > "$CP_USER_DATA"

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

log "waiting for CP IP to appear via DHCP..."
CP_IP="$(resolve_vm_ip "$CP_NAME" 60 5 || true)"
[[ -n "$CP_IP" ]] || fail "could not resolve CP IP after ~5 minutes — inspect 'virsh -c qemu:///system console $CP_NAME'"
log "CP IP: $CP_IP"

# SSH to the CP using the operator's private key explicitly. Nested sudo
# (sudo make -> sudo bash) resets SUDO_USER to root, breaking the previous
# `sudo -u $SUDO_USER ssh` approach. Pin the identity to the private key
# paired with SSH_PUBKEY_FILE — which is the same key bib bakes into root's
# authorized_keys.
# shellcheck disable=SC2034  # SSH_PRIVKEY_FILE is read by ssh_opts_array (lib/build-common.sh)
SSH_PRIVKEY_FILE="$(derive_ssh_privkey_file "$SSH_PUBKEY_FILE")" \
  || fail "SSH private key not readable next to $SSH_PUBKEY_FILE"
ssh_opts_array CP_SSH_OPTS
cp_ssh() { ssh "${CP_SSH_OPTS[@]}" "root@${CP_IP}" "$@"; }

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
    # See CP user-data for the disable_root / users-as-root rationale.
    printf 'disable_root: false\n'
    printf 'users:\n'
    printf '  - name: root\n'
    printf '    ssh_authorized_keys:\n'
    printf '      - %s\n' "$SSH_PUBKEY_CONTENT"
    printf 'write_files:\n'
    printf '  - path: /etc/hummingbird/worker-join.env\n'
    printf '    owner: root:root\n'
    printf "    permissions: '0600'\n"
    printf '    content: |\n'
    # Indent the join command exactly 6 spaces to match YAML block scalar
    # under "content: |" at 4-space indent.
    printf '      %s\n' "$JOIN_CMD"
    # bootc-semver-update overrides. Only emit each entry when the
    # corresponding operator var is set.
    if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
      printf '  - path: /etc/systemd/system/bootc-semver-update.timer.d/schedule.conf\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0644'\n"
      printf '    content: |\n'
      printf '      [Timer]\n'
      printf '      OnCalendar=\n'
      printf '      OnCalendar=%s\n' "$BOOTC_UPDATE_SCHEDULE"
    fi
    if [[ -n "${BOOTC_UPDATE_REPO_WORKER:-}" ]]; then
      printf '  - path: /etc/hummingbird/bootc-update.env\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0644'\n"
      printf '    content: |\n'
      printf '      REPO=%s\n' "$BOOTC_UPDATE_REPO_WORKER"
      printf '      PREFIX=v\n'
    fi
    if [[ "$SWITCH_TO_GHCR" = "true" || -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
      printf 'runcmd:\n'
      if [[ "$SWITCH_TO_GHCR" = "true" ]]; then
        printf '  - [ bootc, switch, ghcr.io/aatchison/hummingbird-k8s-worker:%s ]\n' "$GHCR_TAG"
      fi
      if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
        # Re-read the drop-in cloud-init just wrote, then restart the
        # timer so the override takes effect this boot.
        printf '  - [ systemctl, daemon-reload ]\n'
        printf '  - [ systemctl, restart, bootc-semver-update.timer ]\n'
      fi
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
