#!/usr/bin/env bash
# Shared SSH/virsh/log helpers live in lib/build-common.sh; see docs/development.md.
# scripts/update-cluster.sh — Rolling bootc upgrade across a deployed cluster.
#
# Reads the same CONFIG=<cluster.local.conf> as deploy-cluster.sh and walks
# the cluster one node at a time, in the order:
#
#   1. Control plane (single CP, no peers to drain to — skip drain)
#   2. Each worker, in WORKER_NAMES order (drained, upgraded, uncordoned)
#
# Per worker the flow is:
#   systemctl stop bootc-fetch-apply-updates.timer (avoid race with auto-timer)
#   kubectl drain NODE --ignore-daemonsets --delete-emptydir-data --timeout=5m
#   ssh root@NODE_IP "bootc upgrade --apply"   # --apply auto-reboots
#   wait until ssh comes back AND kubectl reports the node Ready (5min)
#   kubectl uncordon NODE
#   systemctl start bootc-fetch-apply-updates.timer (restore auto-update)
#   sleep 5
#
# For the CP (single-CP topology) we skip drain — there are no peers to
# evict workloads to — and print a clear warning that the apiserver will be
# unavailable for ~60-120s while it reboots.
#
# kubectl is run by SSH-ing to the CP and invoking it there with
# KUBECONFIG=/etc/kubernetes/admin.conf. That mirrors deploy-cluster.sh's
# `cp_ssh` pattern and avoids reinventing the tunnel/kubeconfig dance for
# the read-only orchestration this script does.
#
# Concurrency: a single flock on /run/hbird-update-cluster.lock prevents
# two operators racing each other into a mass-cordon. The lock is released
# when the script exits (normally or via the EXIT trap).
#
# Flags:
#   --workers-only       Skip the CP, only roll the workers.
#   --node=NAME          Update exactly one node (CP or a worker). Mutually
#                        exclusive with --workers-only.
#   --skip-drain         Emergency rollback escape hatch — skip kubectl drain
#                        for workers (CP already skips drain by design).
#                        Also skips the bootc-fetch-apply-updates.timer
#                        stop/restart dance (operator-driven mode).
#   --dry-run            Print the intended actions without ssh/kubectl.
#
# Usage:
#   CONFIG=cluster.local.conf sudo -E bash scripts/update-cluster.sh
#   CONFIG=cluster.local.conf sudo -E bash scripts/update-cluster.sh --workers-only
#   CONFIG=cluster.local.conf sudo -E bash scripts/update-cluster.sh --node=hbird-w1

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# shellcheck source=../lib/build-common.sh
source "${REPO_ROOT}/lib/build-common.sh"
setup_logging "[update-cluster]"

# ---- Flag parsing -----------------------------------------------------------

WORKERS_ONLY=0
SKIP_DRAIN=0
DRY_RUN=0
NODE_FILTER=""

for arg in "$@"; do
  case "$arg" in
    --workers-only)   WORKERS_ONLY=1 ;;
    --skip-drain)     SKIP_DRAIN=1 ;;
    --dry-run)        DRY_RUN=1 ;;
    --node=*)         NODE_FILTER="${arg#--node=}" ;;
    --node)           fail "--node requires a value: --node=NAME" ;;
    -h|--help)
      sed -n '2,46p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) fail "unknown argument: $arg (try --help)" ;;
  esac
done

if (( WORKERS_ONLY == 1 )) && [[ -n "$NODE_FILTER" ]]; then
  fail "--workers-only and --node= are mutually exclusive"
fi

# ---- Root + config ---------------------------------------------------------
# Dry-run lets us iterate the script without sudo (no ssh/kubectl is ever
# actually executed). All real-execution paths need root for the libvirt
# domifaddr lookup used to resolve VM IPs.

if (( DRY_RUN == 0 )) && [[ $EUID -ne 0 ]]; then
  fail "must be run as root (libvirt domifaddr + ssh known_hosts handling). Try: sudo -E bash $0 [flags]"
fi

CONFIG_PATH="${CONFIG:-${REPO_ROOT}/cluster.local.conf}"
[[ -r "$CONFIG_PATH" ]] || fail "config not readable: $CONFIG_PATH (set CONFIG=<path>; start from cluster.example.conf)"

# shellcheck disable=SC1090
source "$CONFIG_PATH"

: "${CP_NAME:?CP_NAME is required in $CONFIG_PATH}"
: "${SSH_PUBKEY_FILE:?SSH_PUBKEY_FILE is required in $CONFIG_PATH}"

# Default WORKER_NAMES to empty array if unset (so --node=CP on a single-node
# cluster still works without tripping `set -u`).
if [[ -z "${WORKER_NAMES+x}" ]]; then
  WORKER_NAMES=()
fi

# ---- SSH identity + opts ---------------------------------------------------
# Pin identity to the private key paired with SSH_PUBKEY_FILE so nested sudo
# can't break us. ssh_opts_array reads SSH_PRIVKEY_FILE; --with-controlmaster
# adds the multiplex options that amortize ssh handshake cost across the
# ~6-12 ssh invocations we make per node. The EXIT trap below `ssh -O exit`s
# the ControlMaster sockets.
if (( DRY_RUN == 1 )); then
  # Dry-run never SSHes; explicit placeholder makes the resolved opt
  # array obviously non-functional if it leaks into a real ssh call.
  # shellcheck disable=SC2034  # SSH_PRIVKEY_FILE is read by ssh_opts_array
  SSH_PRIVKEY_FILE="<dry-run-key>"
else
  # shellcheck disable=SC2034
  SSH_PRIVKEY_FILE="$(derive_ssh_privkey_file "$SSH_PUBKEY_FILE")" \
    || fail "SSH private key not readable next to $SSH_PUBKEY_FILE"
fi
ssh_opts_array SSH_OPTS --with-controlmaster

# Clean up any stale ControlMaster sockets from a prior aborted run.
# Best-effort — won't fail if no stale sockets exist. The path matches
# the ControlPath template in lib/build-common.sh::_ssh_opts_array_impl.
if (( DRY_RUN == 0 )); then
  rm -f /tmp/hbird-ssh-"${UID}"-* 2>/dev/null || true
fi

# ---- IP resolution ---------------------------------------------------------
# deploy-cluster.sh leaves IPs as DHCP-assigned — re-resolve via resolve_vm_ip
# each run. Operator can override via CP_IP / WORKER_IPS (parallel to
# WORKER_NAMES) for clusters with static addressing.
if [[ -z "${CP_IP:-}" ]]; then
  if (( DRY_RUN == 0 )); then
    CP_IP="$(resolve_vm_ip "$CP_NAME" || true)"
    [[ -n "$CP_IP" ]] || fail "could not resolve CP IP for domain '$CP_NAME' via virsh domifaddr (set CP_IP= in env to override)"
  else
    CP_IP="<resolved-at-runtime>"
  fi
fi

# Worker IP map (name -> ip). Parallel WORKER_IPS array wins if set; else
# resolve each via domifaddr.
declare -A WORKER_IP_MAP=()
if [[ -n "${WORKER_IPS+x}" ]] && (( ${#WORKER_IPS[@]} > 0 )); then
  if (( ${#WORKER_IPS[@]} != ${#WORKER_NAMES[@]} )); then
    fail "WORKER_IPS (${#WORKER_IPS[@]}) and WORKER_NAMES (${#WORKER_NAMES[@]}) must be the same length"
  fi
  for i in "${!WORKER_NAMES[@]}"; do
    WORKER_IP_MAP["${WORKER_NAMES[$i]}"]="${WORKER_IPS[$i]}"
  done
else
  for w in "${WORKER_NAMES[@]}"; do
    if (( DRY_RUN == 1 )); then
      WORKER_IP_MAP["$w"]="<resolved-at-runtime>"
    else
      ip="$(resolve_vm_ip "$w" || true)"
      [[ -n "$ip" ]] || fail "could not resolve IP for worker '$w' via virsh domifaddr (set WORKER_IPS=( … ) in $CONFIG_PATH to override)"
      WORKER_IP_MAP["$w"]="$ip"
    fi
  done
fi

# ---- runner abstraction (dry-run aware) ------------------------------------

# kubectl is always issued by ssh-ing to the CP and running kubectl there.
# This sidesteps the local-tunnel kubeconfig dance scripts/kubectl-k8s.sh
# does — fine for orchestration, the CP is the source of truth anyway.
cp_kubectl() {
  if (( DRY_RUN == 1 )); then
    log "DRY-RUN cp_kubectl -- $*"
    return 0
  fi
  # SC2029: $* is composed locally by callers (e.g. "get nodes"); we want
  # it expanded client-side into one quoted argument for the remote shell.
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "root@${CP_IP}" \
    "kubectl --kubeconfig=/etc/kubernetes/admin.conf $*"
}

# ---- bootc capability + state helpers --------------------------------------

# Probe whether the remote bootc supports `upgrade --apply`. bootc <1.1 does
# not, so we fall back to a two-step `bootc upgrade && systemctl reboot` for
# older cohorts. Probe is per-IP because heterogeneous clusters (mid-upgrade
# of bootc itself) are possible.
bootc_has_apply() {
  local ip="$1"
  if (( DRY_RUN == 1 )); then
    return 0
  fi
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "root@${ip}" "bootc upgrade --help 2>/dev/null | grep -q -- '--apply'"
}

# Snapshot the booted image digest. We compare pre/post to detect "already
# current" (bootc upgrade --apply exits 0 with NO reboot in that case, which
# the previous version of this script misread as "updated"). JSONPath:
# `.status.booted.image.imageDigest` per the bootc JSON schema.
bootc_booted_digest() {
  local ip="$1"
  if (( DRY_RUN == 1 )); then
    echo "<dry-run-digest>"
    return 0
  fi
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "root@${ip}" \
    "bootc status --json 2>/dev/null | jq -r '.status.booted.image.imageDigest // .status.booted.image.digest // empty'" \
    2>/dev/null || true
}

# Stop/start the on-host auto-update timer so it doesn't race the manual run.
# The worker preset enables bootc-fetch-apply-updates.timer; CP can have it
# enabled via AUTO_UPDATE_CP=true. Stopping during a manual upgrade avoids
# two concurrent staged deployments.
timer_stop() {
  local ip="$1"
  if (( DRY_RUN == 1 )); then
    log "DRY-RUN ssh root@${ip} systemctl stop bootc-fetch-apply-updates.timer"
    return 0
  fi
  if (( SKIP_DRAIN == 1 )); then
    log "  --skip-drain set: leaving bootc-fetch-apply-updates.timer alone on ${ip}"
    return 0
  fi
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "root@${ip}" \
    "systemctl stop bootc-fetch-apply-updates.timer 2>/dev/null || true" || true
}

timer_start() {
  local ip="$1"
  if (( DRY_RUN == 1 )); then
    log "DRY-RUN ssh root@${ip} systemctl start bootc-fetch-apply-updates.timer"
    return 0
  fi
  if (( SKIP_DRAIN == 1 )); then
    return 0
  fi
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "root@${ip}" \
    "systemctl start bootc-fetch-apply-updates.timer 2>/dev/null || true" || true
}

# ---- EXIT trap: cordon recovery + timer restart ----------------------------
# IN_FLIGHT_NODE / IN_FLIGHT_IP track the worker we're mid-update on. If the
# script aborts between drain and uncordon, surface a recovery command — we
# deliberately do NOT auto-uncordon (an operator Ctrl-C usually means
# something else is wrong; leaving cordoned is safer than handing
# workload to a possibly-half-upgraded node).

IN_FLIGHT_NODE=""
IN_FLIGHT_IP=""
IN_FLIGHT_DRAINED=0
IN_FLIGHT_UNCORDONED=0

cleanup_on_exit() {
  local rc=$?
  # If we were mid-flight on a worker (drained but not uncordoned yet),
  # tell the operator how to restore it. Do NOT auto-uncordon.
  if [[ -n "$IN_FLIGHT_NODE" ]] && (( IN_FLIGHT_DRAINED == 1 )) && (( IN_FLIGHT_UNCORDONED == 0 )); then
    printf '\n' >&2
    printf '[update-cluster] ============================================================\n' >&2
    printf '[update-cluster] WARNING: node %s is cordoned and was not uncordoned.\n' "$IN_FLIGHT_NODE" >&2
    printf '[update-cluster] Restore it manually once you have verified its state:\n' >&2
    printf '[update-cluster]   ssh root@%s "kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon %s"\n' \
      "$CP_IP" "$IN_FLIGHT_NODE" >&2
    printf '[update-cluster] ============================================================\n' >&2
  fi
  # Try to restart the auto-update timer on the in-flight node so the
  # cluster doesn't permanently lose its auto-update behavior after a
  # mid-flight abort. Best-effort — node may be unreachable.
  if [[ -n "$IN_FLIGHT_IP" ]] && (( DRY_RUN == 0 )) && (( SKIP_DRAIN == 0 )); then
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=3 "root@${IN_FLIGHT_IP}" \
      "systemctl start bootc-fetch-apply-updates.timer 2>/dev/null || true" \
      >/dev/null 2>&1 || true
  fi
  # Close ControlMaster sockets explicitly (ControlPersist=60s would
  # eventually expire them, but cleanup is cheap and avoids leaving
  # sockets pointing at unreachable hosts).
  if (( DRY_RUN == 0 )); then
    if [[ -n "${CP_IP:-}" ]]; then
      ssh "${SSH_OPTS[@]}" -O exit "root@${CP_IP}" >/dev/null 2>&1 || true
    fi
    for w in "${WORKER_NAMES[@]}"; do
      local wip="${WORKER_IP_MAP[$w]:-}"
      [[ -n "$wip" ]] || continue
      ssh "${SSH_OPTS[@]}" -O exit "root@${wip}" >/dev/null 2>&1 || true
    done
  fi
  exit "$rc"
}

trap cleanup_on_exit EXIT

# ---- concurrency lock ------------------------------------------------------
# Prevent two `make update-cluster` runs from mass-cordoning the cluster.
# /run is tmpfs on systemd hosts; the lock dies with the kernel. Skip
# locking on dry-run so docs / CI smoke tests don't need /run write access.

if (( DRY_RUN == 0 )); then
  LOCK_FILE="/run/hbird-update-cluster.lock"
  exec 200>"$LOCK_FILE"
  if ! flock -n 200; then
    fail "another update-cluster run is in progress (lock ${LOCK_FILE} held)"
  fi
fi

# ---- readiness helpers -----------------------------------------------------

wait_ssh_back() {
  # wait until `ssh root@<ip> true` works again (post-reboot).
  local ip="$1" timeout="${2:-300}" elapsed=0 interval=5
  log "waiting for SSH to come back on ${ip} (timeout ${timeout}s)"
  while (( elapsed < timeout )); do
    if (( DRY_RUN == 1 )); then
      log "DRY-RUN would poll ssh root@${ip}"
      return 0
    fi
    if ssh "${SSH_OPTS[@]}" "root@${ip}" true >/dev/null 2>&1; then
      log "SSH back on ${ip} after ~${elapsed}s"
      return 0
    fi
    sleep "$interval"
    elapsed=$((elapsed + interval))
  done
  return 1
}

wait_node_ready() {
  # wait until `kubectl get node NAME` reports Ready (5min default).
  # Workers come back cordoned-but-Ready as "Ready,SchedulingDisabled" —
  # match both forms with $2 ~ /^Ready(,|$)/ so the regex covers cordoned
  # AND fully-ready cases. We uncordon *after* this returns.
  local node="$1" timeout="${2:-300}" elapsed=0 interval=10
  log "waiting for node ${node} to report Ready (timeout ${timeout}s)"
  while (( elapsed < timeout )); do
    if (( DRY_RUN == 1 )); then
      log "DRY-RUN would poll kubectl get node ${node}"
      return 0
    fi
    if cp_kubectl "get node ${node} --no-headers 2>/dev/null | awk '\$2 ~ /^Ready(,|\$)/' | grep -q ." ; then
      log "node ${node} Ready after ~${elapsed}s"
      return 0
    fi
    sleep "$interval"
    elapsed=$((elapsed + interval))
  done
  return 1
}

wait_apiserver_back() {
  # CP-specific: after the CP reboots, the apiserver itself goes away.
  # Poll `kubectl get nodes` via ssh until it returns 0 again.
  local timeout="${1:-300}" elapsed=0 interval=10
  log "waiting for apiserver on CP (${CP_IP}) to answer (timeout ${timeout}s)"
  while (( elapsed < timeout )); do
    if (( DRY_RUN == 1 )); then
      log "DRY-RUN would poll apiserver via ssh root@${CP_IP} kubectl get nodes"
      return 0
    fi
    if ssh "${SSH_OPTS[@]}" "root@${CP_IP}" \
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf get --raw=/readyz" \
        >/dev/null 2>&1; then
      log "apiserver back after ~${elapsed}s"
      return 0
    fi
    sleep "$interval"
    elapsed=$((elapsed + interval))
  done
  return 1
}

# ---- per-node operations ---------------------------------------------------

# bootc_upgrade_apply <ip> <name>
# Runs bootc upgrade with --apply if supported, else falls back to two-step
# `bootc upgrade && systemctl reboot` for bootc <1.1 cohorts. Captures the
# booted image digest pre/post to detect "no update available" (the reboot
# side of --apply tears down ssh, returning rc=255 — distinct from rc=0
# meaning "already current, no reboot happened").
#
# Returns:
#   0 — upgrade applied (reboot expected; caller polls wait_ssh_back)
#   2 — no update available (caller should short-circuit the wait loops)
bootc_upgrade_apply() {
  local ip="$1" name="$2"
  local pre_digest="" post_digest="" rc=0

  if (( DRY_RUN == 1 )); then
    log "DRY-RUN ssh root@${ip} bootc upgrade --apply (with pre/post digest compare)"
    return 0
  fi

  pre_digest="$(bootc_booted_digest "$ip")"
  log "  pre-upgrade booted digest: ${pre_digest:-<unknown>}"

  if bootc_has_apply "$ip"; then
    set +e
    # shellcheck disable=SC2029
    ssh "${SSH_OPTS[@]}" "root@${ip}" "bootc upgrade --apply"
    rc=$?
    set -e
  else
    log "  bootc on ${name} lacks --apply; falling back to 'bootc upgrade && systemctl reboot'"
    set +e
    # shellcheck disable=SC2029
    ssh "${SSH_OPTS[@]}" "root@${ip}" "bootc upgrade"
    rc=$?
    set -e
    if (( rc == 0 )); then
      # Reboot in a detached subshell so ssh doesn't hang on the dying
      # session; the reboot kills sshd, so rc=255 is normal.
      set +e
      # shellcheck disable=SC2029
      ssh "${SSH_OPTS[@]}" "root@${ip}" "systemctl reboot" >/dev/null 2>&1
      rc=$?
      set -e
    fi
  fi

  # rc=0  → command returned cleanly (could be "no update" OR "applied but
  #         didn't reboot for some reason"). Disambiguate via digest compare.
  # rc=255→ ssh torn down by reboot — that's the success path for --apply.
  case "$rc" in
    0)
      post_digest="$(bootc_booted_digest "$ip")"
      log "  post-upgrade booted digest: ${post_digest:-<unknown>}"
      if [[ -n "$pre_digest" && -n "$post_digest" && "$pre_digest" == "$post_digest" ]]; then
        log "  no update available on ${name}; skipping wait loops"
        return 2
      fi
      # Digests differ but ssh stayed up — odd, but treat as success and
      # let the wait loops verify Ready.
      return 0
      ;;
    255)
      # Expected: reboot tore down ssh.
      return 0
      ;;
    *)
      fail "bootc upgrade on ${name} exited with rc=${rc}"
      ;;
  esac
}

update_cp() {
  local rc=0
  log "============================================================"
  log "CP: ${CP_NAME} (${CP_IP})"
  log "  single-CP topology: skipping drain (no peers to evict to)"
  log "  apiserver will be unavailable for ~60-120s during reboot"
  log "============================================================"

  IN_FLIGHT_NODE="$CP_NAME"
  IN_FLIGHT_IP="$CP_IP"
  IN_FLIGHT_DRAINED=0
  IN_FLIGHT_UNCORDONED=0

  timer_stop "$CP_IP"

  log "ssh root@${CP_IP} bootc upgrade --apply  (auto-reboots; digest pre/post compared)"
  set +e
  bootc_upgrade_apply "$CP_IP" "$CP_NAME"
  rc=$?
  set -e

  if (( rc == 2 )); then
    log "CP ${CP_NAME}: no update available; restoring timer and continuing."
    timer_start "$CP_IP"
    IN_FLIGHT_NODE=""
    IN_FLIGHT_IP=""
    return 0
  fi

  wait_ssh_back "$CP_IP" 300 || fail "CP ${CP_NAME} did not come back over SSH within 5min"
  wait_apiserver_back 300   || fail "CP ${CP_NAME} apiserver did not return within 5min"
  wait_node_ready "$CP_NAME" 300 || fail "CP node ${CP_NAME} did not reach Ready within 5min"

  timer_start "$CP_IP"
  log "CP ${CP_NAME} updated and Ready."

  IN_FLIGHT_NODE=""
  IN_FLIGHT_IP=""
}

update_worker() {
  local name="$1" ip="$2"
  local rc=0
  log "============================================================"
  log "WORKER: ${name} (${ip})"
  log "============================================================"

  IN_FLIGHT_NODE="$name"
  IN_FLIGHT_IP="$ip"
  IN_FLIGHT_DRAINED=0
  IN_FLIGHT_UNCORDONED=0

  timer_stop "$ip"

  if (( SKIP_DRAIN == 1 )); then
    log "  --skip-drain set: skipping kubectl drain for ${name}"
  else
    log "kubectl drain ${name} --ignore-daemonsets --delete-emptydir-data --timeout=5m"
    cp_kubectl "drain ${name} --ignore-daemonsets --delete-emptydir-data --timeout=5m" \
      || fail "drain failed for ${name}; refusing to continue (use --skip-drain to override)"
    IN_FLIGHT_DRAINED=1
  fi

  log "ssh root@${ip} bootc upgrade --apply  (auto-reboots; digest pre/post compared)"
  set +e
  bootc_upgrade_apply "$ip" "$name"
  rc=$?
  set -e

  if (( rc == 2 )); then
    log "worker ${name}: no update available; uncordoning and continuing."
    if (( SKIP_DRAIN == 0 )); then
      log "kubectl uncordon ${name}"
      cp_kubectl "uncordon ${name}" || fail "uncordon failed for ${name}"
      IN_FLIGHT_UNCORDONED=1
    fi
    timer_start "$ip"
    IN_FLIGHT_NODE=""
    IN_FLIGHT_IP=""
    return 0
  fi

  wait_ssh_back "$ip" 300       || fail "worker ${name} did not come back over SSH within 5min"
  # wait_node_ready before uncordon: kubelet has to rejoin (which may
  # report Ready,SchedulingDisabled while still cordoned) — the regex
  # in wait_node_ready handles both forms.
  wait_node_ready "$name" 300   || fail "worker ${name} did not reach Ready within 5min"

  log "kubectl uncordon ${name}"
  cp_kubectl "uncordon ${name}" || fail "uncordon failed for ${name}"
  IN_FLIGHT_UNCORDONED=1

  timer_start "$ip"

  log "node ${name} updated; pausing 5s before next node"
  if (( DRY_RUN == 0 )); then sleep 5; fi

  IN_FLIGHT_NODE=""
  IN_FLIGHT_IP=""
}

# ---- main ------------------------------------------------------------------

log "config: $CONFIG_PATH"
log "CP=${CP_NAME} (${CP_IP}), workers=(${WORKER_NAMES[*]:-})"
log "flags: workers-only=${WORKERS_ONLY} skip-drain=${SKIP_DRAIN} dry-run=${DRY_RUN} node-filter=${NODE_FILTER:-<none>}"

if [[ -n "$NODE_FILTER" ]]; then
  # Single-node mode: figure out whether it's the CP or a worker.
  if [[ "$NODE_FILTER" == "$CP_NAME" ]]; then
    update_cp
  else
    found=0
    for w in "${WORKER_NAMES[@]}"; do
      if [[ "$w" == "$NODE_FILTER" ]]; then
        update_worker "$w" "${WORKER_IP_MAP[$w]}"
        found=1
        break
      fi
    done
    (( found == 1 )) || fail "--node=${NODE_FILTER} did not match CP_NAME or any WORKER_NAMES entry"
  fi
else
  if (( WORKERS_ONLY == 0 )); then
    update_cp
  else
    log "--workers-only: skipping CP"
  fi
  for w in "${WORKER_NAMES[@]}"; do
    update_worker "$w" "${WORKER_IP_MAP[$w]}"
  done
fi

log "============================================================"
log "Rolling update complete."
log "============================================================"
