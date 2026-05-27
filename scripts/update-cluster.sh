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
#   systemctl stop bootc-{semver,fetch-apply}-update.timer (avoid race
#     with the on-host auto-update timers — we stop both because the
#     semver timer is the post-#181 canonical unit and the legacy fetch
#     timer may still be active on hosts mid-migration)
#   kubectl drain NODE --ignore-daemonsets --delete-emptydir-data --timeout=5m
#   capture pre-reboot .status.nodeInfo.bootID (race-free reboot proof)
#   ssh root@NODE_IP "bootc upgrade --apply"   # --apply auto-reboots
#   wait until ssh comes back, then wait until bootID changes (post-reboot
#     proof — defeats stale-apiserver-cache Ready hit), then wait until
#     kubectl reports the node Ready, then wait until kube-system
#     DaemonSet pods (Cilium / kube-proxy / coredns) on the node are Ready
#   kubectl uncordon NODE
#   systemctl start bootc-semver-update.timer (restore semver auto-update;
#     the legacy fetch timer is left stopped — see timer_start)
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
#   --workers-only             Skip the CP, only roll the workers.
#   --node=NAME                Update exactly one node (CP or a worker).
#                              Mutually exclusive with --workers-only and
#                              --start-from.
#   --start-from=NAME          Resume an interrupted roll. Skip WORKER_NAMES
#                              entries until NAME is encountered, then
#                              process from that point onward. Combines
#                              with --workers-only (which already skips the
#                              CP) but is mutually exclusive with --node=.
#   --continue-on-error        Record per-node failures and proceed instead
#                              of aborting. A summary is printed at the
#                              end; if any node failed the script exits
#                              with rc=3 (distinct from rc=1 fail-fast).
#                              Default behavior remains fail-fast.
#                              NOTE: applies to WORKER failures only; CP
#                              failures still abort the whole run.
#   --no-delete-emptydir-data  Drop --delete-emptydir-data from kubectl
#                              drain. Use for workloads with emptyDir
#                              caches (Prometheus WAL, etc.) the operator
#                              wants preserved. Drain will then block on
#                              such pods; manual eviction may be required.
#   --parallel=N               Process workers in batches of N concurrently
#                              (drain + reboot + wait). Default N=1
#                              (serial; current behavior). The CP is still
#                              updated serially first. Requires that any
#                              workloads with PodDisruptionBudgets can
#                              tolerate N concurrent evictions.
#   --skip-drain               Emergency rollback escape hatch — skip
#                              kubectl drain for workers (CP already skips
#                              drain by design). Also skips the
#                              bootc-*-update.timer stop/restart dance
#                              (operator-driven mode).
#   --skip-gates               Operator escape hatch — skip the bootID-changed
#                              and DaemonSet-readiness gates. Only use if
#                              you've verified the cluster is healthy via
#                              other means; gates exist to prevent dataplane
#                              outages during the roll. wait_node_ready
#                              alone will be the post-reboot signal.
#   --dry-run                  Print the intended actions without
#                              ssh/kubectl.
#
# Environment overrides (all values are durations parsed by sleep / kubectl;
# use bare seconds or Go-style durations as appropriate):
#   DRAIN_TIMEOUT       kubectl drain --timeout (default 5m)
#   READY_TIMEOUT       wait_node_ready seconds (default 300)
#   DAEMONSET_TIMEOUT   wait_node_daemonsets_ready seconds (defaults to
#                       READY_TIMEOUT). Set independently when the
#                       DaemonSet gate needs more (or less) headroom than
#                       the node-Ready gate.
#   APISERVER_TIMEOUT   wait_apiserver_back seconds (default 300)
#   SSH_TIMEOUT         wait_ssh_back seconds (default 300)
#   SSH_DROP_TIMEOUT    wait_ssh_drop seconds (default 30) — how long to
#                       poll for SSH to go DOWN after `bootc upgrade --apply`
#                       before logging WARN and proceeding. (#261)
#   INTER_NODE_SLEEP    seconds to pause between nodes (default 5)
#
# Usage:
#   CONFIG=cluster.local.conf sudo -E bash scripts/update-cluster.sh
#   CONFIG=cluster.local.conf sudo -E bash scripts/update-cluster.sh --workers-only
#   CONFIG=cluster.local.conf sudo -E bash scripts/update-cluster.sh --node=hbird-w1
#   CONFIG=cluster.local.conf sudo -E bash scripts/update-cluster.sh --start-from=hbird-w2
#   CONFIG=cluster.local.conf sudo -E bash scripts/update-cluster.sh --parallel=2 --continue-on-error

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Source-only mode for bats: when HBIRD_UPDATE_CLUSTER_SOURCE_ONLY=1,
# return from `source` here so tests can inspect helpers without
# triggering the SSH-wrap or rolling-update orchestration. (C3, #232.)
if [[ "${HBIRD_UPDATE_CLUSTER_SOURCE_ONLY:-0}" = 1 ]]; then
  return 0
fi

# ---- Remote KVM-host re-exec shim (C3, #232) -------------------------------
# When KVM_HOST is set and we're NOT on the KVM host, re-exec this script
# on the remote host via SSH. See scripts/lib/ssh-wrap.sh for the contract.
# shellcheck source=lib/ssh-wrap.sh
source "${SCRIPT_DIR}/lib/ssh-wrap.sh"
hbird_ssh_wrap_maybe_reexec "$0" "$@"
# ---- End remote re-exec shim -----------------------------------------------

# shellcheck source=../lib/build-common.sh
source "${REPO_ROOT}/lib/build-common.sh"
setup_logging "[update-cluster]"

# ---- Flag parsing -----------------------------------------------------------

WORKERS_ONLY=0
SKIP_DRAIN=0
SKIP_GATES=0
DRY_RUN=0
NODE_FILTER=""
START_FROM=""
CONTINUE_ON_ERROR=0
NO_DELETE_EMPTYDIR_DATA=0
PARALLEL=1

for arg in "$@"; do
  case "$arg" in
    --workers-only)              WORKERS_ONLY=1 ;;
    --skip-drain)                SKIP_DRAIN=1 ;;
    --skip-gates)                SKIP_GATES=1 ;;
    --dry-run)                   DRY_RUN=1 ;;
    --continue-on-error)         CONTINUE_ON_ERROR=1 ;;
    --no-delete-emptydir-data)   NO_DELETE_EMPTYDIR_DATA=1 ;;
    --node=*)                    NODE_FILTER="${arg#--node=}" ;;
    --node)                      fail "--node requires a value: --node=NAME" ;;
    --start-from=*)
      START_FROM="${arg#--start-from=}"
      # `--start-from=` (empty RHS) matched here previously and was silently
      # accepted, then the `[[ -n "$START_FROM" ]]` guard downstream made
      # the script proceed as if the flag had not been passed at all.
      # Reject empty values explicitly so operators get a clear diagnostic.
      [[ -n "$START_FROM" ]] || fail "--start-from= requires a non-empty value: --start-from=NAME"
      ;;
    --start-from)                fail "--start-from requires a value: --start-from=NAME" ;;
    --parallel=*)                PARALLEL="${arg#--parallel=}" ;;
    --parallel)                  fail "--parallel requires a value: --parallel=N" ;;
    -h|--help)
      sed -n '2,103p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) fail "unknown argument: $arg (try --help)" ;;
  esac
done

if (( WORKERS_ONLY == 1 )) && [[ -n "$NODE_FILTER" ]]; then
  fail "--workers-only and --node= are mutually exclusive"
fi

if [[ -n "$NODE_FILTER" ]] && [[ -n "$START_FROM" ]]; then
  fail "--node= and --start-from= are mutually exclusive (--node is single-node mode; --start-from is resume mode)"
fi

# --parallel must be a positive integer.
if ! [[ "$PARALLEL" =~ ^[0-9]+$ ]] || (( PARALLEL < 1 )); then
  fail "--parallel=N requires a positive integer (got: $PARALLEL)"
fi

# ---- Env-tunable timeouts --------------------------------------------------
# Operators tune these per-cluster; defaults are deliberately generous so
# slow networks / heavy workloads don't false-fail. DRAIN_TIMEOUT is passed
# verbatim to `kubectl drain --timeout=` (Go duration string); the rest are
# bare seconds consumed by the wait_* helpers below.
DRAIN_TIMEOUT="${DRAIN_TIMEOUT:-5m}"
READY_TIMEOUT="${READY_TIMEOUT:-300}"
# DAEMONSET_TIMEOUT defaults to READY_TIMEOUT for backward compatibility —
# pre-#208 the DaemonSet gate was bounded by READY_TIMEOUT directly. Operators
# can now tune the gate independently (e.g. a noisy Cilium rollout may want
# more headroom than the node-Ready check).
DAEMONSET_TIMEOUT="${DAEMONSET_TIMEOUT:-${READY_TIMEOUT}}"
APISERVER_TIMEOUT="${APISERVER_TIMEOUT:-300}"
SSH_TIMEOUT="${SSH_TIMEOUT:-300}"
# SSH_DROP_TIMEOUT bounds wait_ssh_drop — how long we poll for SSH on the
# target IP to become unreachable after `bootc upgrade --apply` queues the
# reboot via systemd-run (#261). 30s is generous; bootc's --apply typically
# fires the reboot within ~5s. The gate is DIAGNOSTIC: a timeout logs WARN
# but does NOT fail the run — the bootID-changed gate (when not skipped)
# remains the source of truth for "did the reboot happen".
SSH_DROP_TIMEOUT="${SSH_DROP_TIMEOUT:-30}"
INTER_NODE_SLEEP="${INTER_NODE_SLEEP:-5}"

# ---- Env-knob validation (SECURITY) ----------------------------------------
# The Makefile invokes this script via `sudo -E`, so operator-shell env vars
# survive the privilege boundary and reach the script as root. Five of these
# knobs flow into injection sinks if not validated:
#
#   DRAIN_TIMEOUT  → composed into "--timeout=${DRAIN_TIMEOUT}" then passed
#                    to cp_kubectl, which ssh's the string as a single remote
#                    shell command. Hostile value:
#                       DRAIN_TIMEOUT='5m; rm -rf /; #'
#                    → arbitrary commands executed as root on the CP.
#
#   READY_TIMEOUT  → consumed by `while (( elapsed < timeout ))` bash
#   APISERVER_TIMEOUT  arithmetic context, which command-substitutes.
#   SSH_TIMEOUT       Hostile value: 'a[0$(reboot)]' triggers side effects.
#   INTER_NODE_SLEEP
#
# Validate every knob at the top of the script BEFORE any use. Anything that
# isn't a bare integer (or kubectl-duration string for DRAIN_TIMEOUT) is
# rejected here, so hostile env vars never reach the sinks.
[[ "$DRAIN_TIMEOUT" =~ ^[0-9]+(s|m|h)?$ ]] \
  || fail "DRAIN_TIMEOUT must match ^[0-9]+(s|m|h)?\$ (got: ${DRAIN_TIMEOUT})"
# READY_TIMEOUT / DAEMONSET_TIMEOUT must be STRICTLY positive — a value of 0
# would make the wait loops short-circuit immediately, defeating the gates.
[[ "$READY_TIMEOUT" =~ ^[1-9][0-9]*$ ]] \
  || fail "READY_TIMEOUT must be a positive integer of seconds, >0 (got: ${READY_TIMEOUT})"
[[ "$DAEMONSET_TIMEOUT" =~ ^[1-9][0-9]*$ ]] \
  || fail "DAEMONSET_TIMEOUT must be a positive integer of seconds, >0 (got: ${DAEMONSET_TIMEOUT})"
[[ "$APISERVER_TIMEOUT" =~ ^[0-9]+$ ]] \
  || fail "APISERVER_TIMEOUT must be a positive integer of seconds (got: ${APISERVER_TIMEOUT})"
[[ "$SSH_TIMEOUT" =~ ^[0-9]+$ ]] \
  || fail "SSH_TIMEOUT must be a positive integer of seconds (got: ${SSH_TIMEOUT})"
# SSH_DROP_TIMEOUT must be a STRICTLY positive integer — a value of 0
# would defeat the gate entirely (the while-loop short-circuits and the
# WARN fires immediately, defeating the diagnostic value).
[[ "$SSH_DROP_TIMEOUT" =~ ^[1-9][0-9]*$ ]] \
  || fail "SSH_DROP_TIMEOUT must be a positive integer of seconds, >0 (got: ${SSH_DROP_TIMEOUT})"
[[ "$INTER_NODE_SLEEP" =~ ^[0-9]+$ ]] \
  || fail "INTER_NODE_SLEEP must be a non-negative integer of seconds (got: ${INTER_NODE_SLEEP})"

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

# Validate --start-from references a real worker (else the resume loop
# would silently skip every node).
if [[ -n "$START_FROM" ]]; then
  start_from_found=0
  for w in "${WORKER_NAMES[@]}"; do
    if [[ "$w" == "$START_FROM" ]]; then
      start_from_found=1
      break
    fi
  done
  (( start_from_found == 1 )) || fail "--start-from=${START_FROM} did not match any WORKER_NAMES entry"
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

# Stop/start the on-host auto-update timer(s) so they don't race the manual run.
# Post-#181 the canonical timer is bootc-semver-update.timer (enabled by preset
# on new images). Pre-#181 hosts still have bootc-fetch-apply-updates.timer
# enabled. We stop/start BOTH unconditionally — `systemctl stop` on an inactive
# unit is a no-op and the `|| true` swallows the "Unit not loaded" exit on
# images that don't ship one of the units. This keeps update-cluster.sh
# correct across the migration window where the fleet is mixed.
timer_stop() {
  local ip="$1"
  if (( DRY_RUN == 1 )); then
    log "DRY-RUN ssh root@${ip} systemctl stop bootc-semver-update.timer bootc-fetch-apply-updates.timer"
    return 0
  fi
  if (( SKIP_DRAIN == 1 )); then
    log "  --skip-drain set: leaving bootc-*-update.timer alone on ${ip}"
    return 0
  fi
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "root@${ip}" \
    "systemctl stop bootc-semver-update.timer bootc-fetch-apply-updates.timer 2>/dev/null || true" || true
}

timer_start() {
  local ip="$1"
  if (( DRY_RUN == 1 )); then
    log "DRY-RUN ssh root@${ip} systemctl start bootc-semver-update.timer"
    return 0
  fi
  if (( SKIP_DRAIN == 1 )); then
    return 0
  fi
  # Detect which timer unit exists on the remote and start the right one.
  # On a post-#181 image bootc-semver-update.timer is present and is the
  # canonical updater; on a pre-#181 host only bootc-fetch-apply-updates.timer
  # exists. Blindly starting bootc-semver-update.timer on a pre-#181 host
  # returns rc=5 (no such unit), which the outer `|| true` would swallow —
  # leaving the node with neither timer running after `bootc upgrade` rc=2
  # ("no update available") paths. Probe first, then start whichever timer
  # is actually defined. (#181 round-2 review.)
  # Exit 44 from the bash block if no timer unit is found, so we can
  # distinguish "node has no auto-update on resume" from a real start
  # failure (CodeRabbit #181). Both surface as a WARN; we don't fail the
  # rolling upgrade — operator can re-enable manually.
  # shellcheck disable=SC2029
  if ! ssh "${SSH_OPTS[@]}" "root@${ip}" 'bash -c "
    if systemctl cat bootc-semver-update.timer >/dev/null 2>&1; then
      systemctl start bootc-semver-update.timer
    elif systemctl cat bootc-fetch-apply-updates.timer >/dev/null 2>&1; then
      systemctl start bootc-fetch-apply-updates.timer
    else
      exit 44
    fi
  "'; then
    log "WARN: ${ip}: bootc auto-update timer not restored (no semver/fetch-apply timer present or systemctl start failed)"
  fi
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
# IN_FLIGHT_PHASE names the in-progress step so cleanup_on_exit can surface
# a more specific recovery hint than "cordoned, not uncordoned". Values are
# advisory (operator-readable strings), e.g.
#   pre-drain, post-drain, post-bootID-pre-Ready, post-Ready-pre-DaemonSet,
#   post-DaemonSet-pre-uncordon. Set by update_worker / update_cp at each
# transition; consumed only by cleanup_on_exit.
IN_FLIGHT_PHASE=""

# Per-node result tracking for --continue-on-error. FAILED_NODES is a
# parallel array of "NAME:reason" entries; SUCCEEDED_NODES is just names.
FAILED_NODES=()
SUCCEEDED_NODES=()

# Tracks the per-batch tmpdir during parallel mode. Module-scope so
# cleanup_on_exit can scan it for {node}.in-flight files and remove the
# whole tree on EXIT (including SIGINT). Empty when not in a parallel
# batch.
BATCH_TMPDIR=""

# Prune stale /tmp/hbird-update-batch.* dirs left behind by prior killed
# runs. Best-effort; we only clean OUR-uid-owned dirs to avoid stepping on
# other operators. Done early so a stale dir from a kill -9 yesterday
# doesn't fool today's recovery-hint scan.
if (( DRY_RUN == 0 )); then
  find /tmp -maxdepth 1 -name 'hbird-update-batch.*' -user "$UID" \
    -type d -mmin +5 -exec rm -rf {} + 2>/dev/null || true
fi

# mark_in_flight — write the current IN_FLIGHT_* state to a per-node file
# under BATCH_TMPDIR so the parent's cleanup_on_exit can emit recovery
# hints. Subshells under run_worker_batch get a COPY of the IN_FLIGHT_*
# globals — so the parent has no direct visibility into their state.
# Workaround: each subshell writes its in-flight state to disk at the
# key transitions (post-drain, pre-uncordon); the parent scans those on
# EXIT. No-op when not in parallel mode (BATCH_TMPDIR empty).
mark_in_flight() {
  [[ -n "${BATCH_TMPDIR:-}" ]] || return 0
  [[ -d "$BATCH_TMPDIR" ]]     || return 0
  [[ -n "$IN_FLIGHT_NODE" ]]   || return 0
  printf 'node=%s\nip=%s\ndrained=%d\nuncordoned=%d\nphase=%s\n' \
    "$IN_FLIGHT_NODE" "$IN_FLIGHT_IP" "$IN_FLIGHT_DRAINED" "$IN_FLIGHT_UNCORDONED" "$IN_FLIGHT_PHASE" \
    > "${BATCH_TMPDIR}/${IN_FLIGHT_NODE}.in-flight" 2>/dev/null || true
}

# clear_in_flight — remove the .in-flight file once the node is fully
# uncordoned. Caller invokes at the success boundary.
clear_in_flight() {
  [[ -n "${BATCH_TMPDIR:-}" ]] || return 0
  [[ -n "$IN_FLIGHT_NODE" ]]   || return 0
  rm -f "${BATCH_TMPDIR}/${IN_FLIGHT_NODE}.in-flight" 2>/dev/null || true
}

cleanup_on_exit() {
  local rc=$?
  # If we were mid-flight on a worker (drained but not uncordoned yet),
  # tell the operator how to restore it. Do NOT auto-uncordon.
  if [[ -n "$IN_FLIGHT_NODE" ]] && (( IN_FLIGHT_DRAINED == 1 )) && (( IN_FLIGHT_UNCORDONED == 0 )); then
    printf '\n' >&2
    printf '[update-cluster] ============================================================\n' >&2
    printf '[update-cluster] WARNING: node %s is cordoned and was not uncordoned.\n' "$IN_FLIGHT_NODE" >&2
    if [[ -n "$IN_FLIGHT_PHASE" ]]; then
      printf '[update-cluster] in-flight phase: %s\n' "$IN_FLIGHT_PHASE" >&2
    fi
    printf '[update-cluster] Restore it manually once you have verified its state:\n' >&2
    printf '[update-cluster]   ssh root@%s "kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon %s"\n' \
      "$CP_IP" "$IN_FLIGHT_NODE" >&2
    printf '[update-cluster] ============================================================\n' >&2
  fi
  # In parallel mode the per-worker IN_FLIGHT_* state lives in the
  # subshells, not in this parent. Each subshell writes a .in-flight file
  # at the drain transition; scan them here so a Ctrl-C during a parallel
  # batch still surfaces recovery hints for every still-cordoned node.
  if [[ -n "${BATCH_TMPDIR:-}" && -d "$BATCH_TMPDIR" ]]; then
    local _inflight
    for _inflight in "${BATCH_TMPDIR}"/*.in-flight; do
      [[ -e "$_inflight" ]] || continue
      # shellcheck disable=SC1090
      local _node="" _drained=0 _uncordoned=0 _phase=""
      while IFS='=' read -r _k _v; do
        case "$_k" in
          node)       _node="$_v" ;;
          drained)    _drained="$_v" ;;
          uncordoned) _uncordoned="$_v" ;;
          phase)      _phase="$_v" ;;
        esac
      done < "$_inflight"
      if [[ -n "$_node" ]] && (( _drained == 1 )) && (( _uncordoned == 0 )); then
        printf '\n' >&2
        printf '[update-cluster] ============================================================\n' >&2
        printf '[update-cluster] WARNING: node %s (parallel batch) is cordoned, not uncordoned.\n' "$_node" >&2
        if [[ -n "$_phase" ]]; then
          printf '[update-cluster] in-flight phase: %s\n' "$_phase" >&2
        fi
        printf '[update-cluster] Restore it manually once you have verified its state:\n' >&2
        printf '[update-cluster]   ssh root@%s "kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon %s"\n' \
          "${CP_IP:-CP_IP}" "$_node" >&2
        printf '[update-cluster] ============================================================\n' >&2
      fi
    done
    rm -rf "$BATCH_TMPDIR" 2>/dev/null || true
  fi
  # Try to restart the auto-update timer on the in-flight node so the
  # cluster doesn't permanently lose its auto-update behavior after a
  # mid-flight abort. Best-effort — node may be unreachable. Probe for
  # the timer unit that actually exists on the node (#181 round-2): post-
  # #181 hosts have bootc-semver-update.timer; pre-#181 hosts only have
  # bootc-fetch-apply-updates.timer. Without the probe we'd silently leave
  # legacy hosts with no auto-update timer after a mid-flight abort.
  if [[ -n "$IN_FLIGHT_IP" ]] && (( DRY_RUN == 0 )) && (( SKIP_DRAIN == 0 )); then
    # Exit 44 if no timer unit is found; surface as WARN to the operator
    # who's already inspecting the abort. (CodeRabbit #181.)
    if ! ssh "${SSH_OPTS[@]}" -o ConnectTimeout=3 "root@${IN_FLIGHT_IP}" 'bash -c "
      if systemctl cat bootc-semver-update.timer >/dev/null 2>&1; then
        systemctl start bootc-semver-update.timer
      elif systemctl cat bootc-fetch-apply-updates.timer >/dev/null 2>&1; then
        systemctl start bootc-fetch-apply-updates.timer
      else
        exit 44
      fi
    "'; then
      log "WARN: ${IN_FLIGHT_IP}: bootc auto-update timer not restored on abort cleanup"
    fi
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

# wait_ssh_drop IP [MAX]
#
# Wait for SSH on $ip to become unreachable, indicating the node is
# actually rebooting. Bootc's --apply queues the reboot via systemd —
# usually starts within ~5s — so without this gate, wait_ssh_back fires
# immediately on the pre-reboot SSH connection and declares success
# before the reboot has begun. (#261)
#
# This is a DIAGNOSTIC gate: a timeout logs WARN but does NOT fail the
# run. The bootID-changed gate (when not skipped) remains the source of
# truth for "did the reboot actually happen". When --skip-gates is set
# (bootID gate skipped), this gate is still useful as the only signal
# that the reboot started — operators chasing a regression can grep the
# log for "still up after" to find nodes that bootc queued but did not
# actually reboot.
#
# IMPORTANT: this helper deliberately does NOT reuse the ControlMaster
# socket. The persistent multiplexed session is what `wait_ssh_back`
# uses to short-circuit on the pre-reboot connection (returning ~0s);
# we need a FRESH probe so each iteration actually attempts a new TCP
# handshake to the (possibly-already-down) sshd. -o ControlPath=none
# and BatchMode=yes force the fresh connection.
wait_ssh_drop() {
  local ip="$1" max="${2:-${SSH_DROP_TIMEOUT}}" i=0
  if (( DRY_RUN == 1 )); then
    log "DRY-RUN wait_ssh_drop ${ip} (would poll up to ${max}s)"
    return 0
  fi
  log "waiting for SSH on ${ip} to drop (timeout ${max}s)"
  while (( i < max )); do
    if ! ssh "${SSH_OPTS[@]}" -o ControlPath=none -o ConnectTimeout=3 \
         -o BatchMode=yes "root@${ip}" true >/dev/null 2>&1; then
      log "  SSH on ${ip} dropped after ~${i}s (reboot in progress)"
      return 0
    fi
    sleep 1
    i=$((i + 1))
  done
  log "  WARN: SSH on ${ip} still up after ${max}s — bootc may have queued without rebooting"
  return 1
}

wait_ssh_back() {
  # wait until `ssh root@<ip> true` works again (post-reboot).
  local ip="$1" timeout="${2:-${SSH_TIMEOUT}}" elapsed=0 interval=5
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
  local node="$1" timeout="${2:-${READY_TIMEOUT}}" elapsed=0 interval=10
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

# capture_node_bootid NODE
#
# Returns (on stdout) the node's current .status.nodeInfo.bootID. The bootID
# is a kernel-supplied UUID that's regenerated on every boot — comparing
# pre/post values proves a real reboot completed, which is the only reliable
# way to defeat a stale apiserver cache hit in wait_node_ready (the
# apiserver may still report Ready=True from the pre-reboot lease before
# kubelet has actually re-registered).
#
# Empty stdout + rc=0 is a valid return (kubectl may have returned no value
# in race windows); callers MUST treat empty pre-bootid as "couldn't capture,
# skip the gate" rather than as a comparison sentinel.
#
# Retry: a single transient apiserver flake (or controlmaster reconnect)
# can return an empty string even on a healthy cluster. Round-1 review of
# PR #208 flagged that "empty pre_bootid silently regresses the gate"; we
# now retry up to 3 times with a 2s sleep before giving up. A persistent
# failure logs WARN (greppable in postmortems) AND surfaces the same WARN
# from wait_node_bootid_changed when the empty value is consumed downstream.
capture_node_bootid() {
  local node="$1"
  if (( DRY_RUN == 1 )); then
    log "DRY-RUN would capture pre-reboot bootID for ${node}"
    echo "<dry-run-bootid>"
    return 0
  fi
  local attempt val
  for attempt in 1 2 3; do
    val="$(cp_kubectl "get node ${node} -o jsonpath='{.status.nodeInfo.bootID}'" 2>/dev/null || true)"
    if [[ -n "$val" ]]; then
      printf '%s' "$val"
      return 0
    fi
    # Don't sleep after the last attempt — the caller is waiting.
    (( attempt < 3 )) && sleep 2
  done
  log "WARN: failed to capture pre-reboot bootID for ${node} after 3 attempts; bootID-changed gate will be skipped (apiserver flake?)"
  # Empty stdout. Callers must treat as "skip the gate" per the docstring.
}

# wait_node_bootid_changed NODE PRE_BOOTID
#
# Poll the node's .status.nodeInfo.bootID until it differs from PRE_BOOTID.
# The bootID is reset only on a real reboot — apiserver state caches a node
# Ready=True from before the reboot, so wait_node_ready alone can return
# prematurely on a node that hasn't actually rebooted yet. This gate runs
# BEFORE wait_node_ready and rejects the stale-cache case.
#
# READY_TIMEOUT (existing env knob, validated at startup) bounds the wait.
# Returns 0 on observed change, 1 on timeout.
#
# Special cases:
#   - PRE_BOOTID empty (capture failed or didn't run) → log + return 0.
#     Without a baseline we can't compare; skipping is safer than
#     blocking the update forever on a missing field.
wait_node_bootid_changed() {
  local node="$1" pre_bootid="$2" elapsed=0 interval=5 cur_bootid="" last_heartbeat=0
  if (( SKIP_GATES == 1 )); then
    log "node ${node}: --skip-gates set, skipping bootID-changed gate"
    return 0
  fi
  if [[ -z "$pre_bootid" ]]; then
    log "WARN: node ${node}: pre-reboot bootID was empty; skipping bootID-changed gate"
    return 0
  fi
  log "waiting for node ${node} bootID to change from pre-reboot value (timeout ${READY_TIMEOUT}s)"
  while (( elapsed < READY_TIMEOUT )); do
    if (( DRY_RUN == 1 )); then
      log "DRY-RUN would poll bootID for ${node}"
      return 0
    fi
    cur_bootid=$(cp_kubectl "get node ${node} -o jsonpath='{.status.nodeInfo.bootID}'" 2>/dev/null || true)
    if [[ -n "$cur_bootid" && "$cur_bootid" != "$pre_bootid" ]]; then
      log "node ${node} bootID changed (pre=${pre_bootid:0:8}... post=${cur_bootid:0:8}...) after ~${elapsed}s"
      return 0
    fi
    sleep "$interval"
    elapsed=$((elapsed + interval))
    # Progress heartbeat every ~30s so an operator watching the log knows
    # the gate is still alive on a slow reboot. Rounded to the interval
    # boundary so we don't log every iteration.
    if (( elapsed - last_heartbeat >= 30 )) && (( elapsed < READY_TIMEOUT )); then
      log "  still polling node ${node} for bootID-changed gate after ~${elapsed}s (pre=${pre_bootid:0:8}... cur=${cur_bootid:0:8}...)"
      last_heartbeat=$elapsed
    fi
  done
  # Diagnostic on timeout: surface the last observed pre/cur so an operator
  # can tell at a glance whether the apiserver returned anything at all.
  log "node ${node}: bootID-changed gate timed out after ${READY_TIMEOUT}s (pre=${pre_bootid:0:8}... cur=${cur_bootid:0:8}...)"
  return 1
}

# wait_node_daemonsets_ready NODE
#
# Poll every pod in kube-system scheduled on NODE until all containers
# report Ready=true. Node Ready (the kubelet condition) means "kubelet
# is alive + the CNI binary is present on disk" — it does NOT mean the
# Cilium agent / kube-proxy / coredns DaemonSet pods on this node are
# running and forwarding traffic. Without this gate, the script can
# proceed to drain N+1 while Cilium on N is still CrashLooping, causing
# a brief networking outage as the next eviction lands on N+1 with no
# functional CNI on N to talk to.
#
# Implementation: --field-selector narrows to this node server-side, then
# a simple jsonpath emits one "name=ready,ready,..." line per pod. A
# pod with any "false" is unready. We deliberately don't try to filter
# DaemonSet-only on the API server side (the ownerReferences jsonpath is
# noisy across kubectl versions) — kube-system overwhelmingly hosts
# DaemonSet pods + the CP-only static pods, and the CP-only pods are
# named with the CP node anyway so they self-select via field-selector
# on a worker. False positives from a transient kube-system Job are
# acceptable; better to wait one extra cycle than to miss a real CNI
# crash loop.
#
# DAEMONSET_TIMEOUT bounds the wait. Returns 0 when all pods report Ready,
# 1 on timeout.
#
# Round-1 review hardenings (PR #208):
#   - Phase 1: wait up to 60s for at least one kube-system pod to APPEAR on
#     the node. After a fresh reboot the DaemonSet controller can take a
#     few seconds to schedule pods to the node; without phase 1 we'd race
#     the controller and pass vacuously on the empty result. On phase-1
#     timeout we emit a WARN and proceed (could be a fresh cluster with no
#     DS yet; better than hanging forever).
#   - Baseline-unready exclusion: snapshot the set of pods that are
#     already unready at gate entry. Pre-existing CrashLoops in kube-system
#     that have nothing to do with this upgrade no longer block the whole
#     roll. Only NEW unready pods (post-baseline) gate progress.
#   - Progress heartbeat every ~30s on a slow rollout.
wait_node_daemonsets_ready() {
  local node="$1" elapsed=0 interval=5 line raw new_unready
  local baseline_unready="" pod_count=0 phase1_elapsed=0 last_heartbeat=0
  if (( SKIP_GATES == 1 )); then
    log "node ${node}: --skip-gates set, skipping DaemonSet readiness gate"
    return 0
  fi
  if (( DRY_RUN == 1 )); then
    log "waiting for kube-system DaemonSet pods on ${node} to be Ready (timeout ${DAEMONSET_TIMEOUT}s)"
    log "DRY-RUN would poll kube-system pods on ${node} for Ready"
    return 0
  fi
  # ---- Phase 1: wait for at least one kube-system pod to appear on the node.
  # After a reboot the DaemonSet controller may take a few seconds to bind
  # pods to the freshly-rejoined node; polling Ready before that gives a
  # vacuous pass. Bounded to 60s — long enough for the DS controller to act,
  # short enough that a genuinely-no-DS cluster doesn't stall.
  while (( phase1_elapsed < 60 )); do
    pod_count=$(cp_kubectl "get pods -n kube-system --field-selector=spec.nodeName=${node} --no-headers 2>/dev/null | wc -l" 2>/dev/null || echo 0)
    pod_count="${pod_count//[^0-9]/}"  # strip any stray whitespace/CR
    [[ -z "$pod_count" ]] && pod_count=0
    if (( pod_count > 0 )); then
      break
    fi
    if (( phase1_elapsed == 0 )); then
      log "WARN: no kube-system pods yet observed on ${node}; waiting up to 60s for DaemonSet controller to schedule"
    fi
    sleep 2
    phase1_elapsed=$((phase1_elapsed + 2))
  done
  if (( pod_count == 0 )); then
    log "WARN: no kube-system pods on ${node} after 60s; proceeding (fresh cluster or no DS deployed yet)"
    return 0
  fi
  # ---- Snapshot baseline-unready: any kube-system pod on this node that
  # is ALREADY unready right now. Pre-existing CrashLoops unrelated to the
  # upgrade are excluded from the gate so they don't block the roll.
  raw=$(cp_kubectl "get pods -n kube-system --field-selector=spec.nodeName=${node} -o jsonpath='{range .items[*]}{.metadata.name}={range .status.containerStatuses[*]}{.ready},{end}{\"\\n\"}{end}'" 2>/dev/null || true)
  baseline_unready=$(_collect_unready_names "$raw" | sort -u)
  if [[ -n "$baseline_unready" ]]; then
    # shellcheck disable=SC2001  # tr is the simpler join here
    log "  baseline-unready pods on ${node} (excluded from gate): $(echo "$baseline_unready" | tr '\n' ' ')"
  fi
  log "waiting for kube-system DaemonSet pods on ${node} to be Ready (timeout ${DAEMONSET_TIMEOUT}s)"
  # ---- Phase 2: poll until every NEW (post-baseline) unready pod becomes
  # Ready, bounded by DAEMONSET_TIMEOUT.
  while (( elapsed < DAEMONSET_TIMEOUT )); do
    raw=$(cp_kubectl "get pods -n kube-system --field-selector=spec.nodeName=${node} -o jsonpath='{range .items[*]}{.metadata.name}={range .status.containerStatuses[*]}{.ready},{end}{\"\\n\"}{end}'" 2>/dev/null || true)
    # Compute set difference: current unready MINUS baseline unready.
    new_unready=$(comm -23 <(_collect_unready_names "$raw" | sort -u) <(printf '%s\n' "$baseline_unready" | sort -u) | tr '\n' ' ')
    new_unready="${new_unready% }"
    if [[ -z "$new_unready" ]]; then
      log "node ${node} kube-system DaemonSet pods all Ready after ~${elapsed}s (excluding ${baseline_unready:+pre-existing-unready})"
      return 0
    fi
    sleep "$interval"
    elapsed=$((elapsed + interval))
    if (( elapsed - last_heartbeat >= 30 )) && (( elapsed < DAEMONSET_TIMEOUT )); then
      log "  still polling kube-system DaemonSet pods on ${node} after ~${elapsed}s (new-unready: ${new_unready})"
      last_heartbeat=$elapsed
    fi
  done
  log "node ${node}: kube-system DaemonSet pods (new, post-baseline) still not Ready after ${DAEMONSET_TIMEOUT}s: ${new_unready}"
  return 1
}

# _collect_unready_names — read jsonpath blob on stdin (one
# "podname=true,false,..." line per pod) and emit, one per line, the names
# of pods whose ready set contains "false" or is entirely empty (Pending
# pod with no containerStatuses). Internal helper for
# wait_node_daemonsets_ready.
_collect_unready_names() {
  local raw="$1" line rhs
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    rhs="${line#*=}"
    if [[ -z "$rhs" || "$rhs" == *false* ]]; then
      echo "${line%%=*}"
    fi
  done <<< "$raw"
}

wait_apiserver_back() {
  # CP-specific: after the CP reboots, the apiserver itself goes away.
  # Poll `kubectl get nodes` via ssh until it returns 0 again.
  local timeout="${1:-${APISERVER_TIMEOUT}}" elapsed=0 interval=10
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
#   1 — unexpected non-zero rc from `bootc upgrade` (caller decides whether
#       to fail-fast or record-and-continue via worker_fail; we do NOT call
#       fail() here directly — that would bypass --continue-on-error)
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
      # Surface but DO NOT call fail() — caller (update_worker /
      # update_cp) decides whether to route through worker_fail (which
      # respects --continue-on-error) or to abort hard.
      log "bootc upgrade on ${name} exited unexpectedly (rc=${rc})"
      return 1
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
  IN_FLIGHT_PHASE="pre-upgrade"

  timer_stop "$CP_IP"

  # Capture the CP's pre-reboot bootID. wait_node_bootid_changed compares
  # against this after wait_apiserver_back returns to defeat the
  # stale-apiserver-cache race that wait_node_ready alone can't catch.
  # See docs/update-cluster.md "Reboot detection (bootID)".
  local pre_bootid
  pre_bootid="$(capture_node_bootid "$CP_NAME")"
  log "  pre-reboot bootID: ${pre_bootid:0:8}..."

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

  # rc=1 here means `bootc upgrade` itself failed (not a torn-down ssh —
  # that's rc=255 mapped to 0 inside bootc_upgrade_apply). CP failures
  # always fail-fast: --continue-on-error is documented as worker-only.
  if (( rc == 1 )); then
    fail "bootc upgrade on CP ${CP_NAME} failed (see log above)"
  fi

  # Diagnostic SSH-drop gate (#261): poll for SSH to actually go down
  # before polling for it to come back, so we don't false-success on the
  # pre-reboot connection. A timeout here is logged but NOT fatal — the
  # bootID gate downstream is the source of truth for "rebooted".
  wait_ssh_drop "$CP_IP" "$SSH_DROP_TIMEOUT" || true
  wait_ssh_back "$CP_IP" "$SSH_TIMEOUT" || fail "CP ${CP_NAME} did not come back over SSH within ${SSH_TIMEOUT}s"
  IN_FLIGHT_PHASE="post-reboot-pre-apiserver"
  wait_apiserver_back "$APISERVER_TIMEOUT"   || fail "CP ${CP_NAME} apiserver did not return within ${APISERVER_TIMEOUT}s"
  # bootID gate runs BEFORE wait_node_ready to defeat the stale-apiserver-
  # cache race (apiserver may report Ready=True from the pre-reboot lease
  # before kubelet has actually re-registered).
  IN_FLIGHT_PHASE="post-apiserver-pre-bootID"
  wait_node_bootid_changed "$CP_NAME" "$pre_bootid" \
    || fail "CP node ${CP_NAME} bootID did not change after ${READY_TIMEOUT}s (apiserver may be serving stale state)"
  IN_FLIGHT_PHASE="post-bootID-pre-Ready"
  wait_node_ready "$CP_NAME" "$READY_TIMEOUT" || fail "CP node ${CP_NAME} did not reach Ready within ${READY_TIMEOUT}s"
  # DaemonSet gate: Node Ready means kubelet+CNI binary present, NOT that
  # the Cilium/kube-proxy/coredns pods are forwarding traffic. Block here
  # so we don't proceed to workers while the CP's networking is still
  # CrashLooping. See docs/update-cluster.md "Daemonset readiness gate".
  IN_FLIGHT_PHASE="post-Ready-pre-DaemonSet"
  wait_node_daemonsets_ready "$CP_NAME" \
    || fail "CP ${CP_NAME}: kube-system DaemonSet pods not Ready after ${DAEMONSET_TIMEOUT}s"

  timer_start "$CP_IP"
  log "CP ${CP_NAME} updated and Ready."

  IN_FLIGHT_NODE=""
  IN_FLIGHT_IP=""
  IN_FLIGHT_PHASE=""
}

# update_worker NAME IP
#
# Returns 0 on success, non-zero on failure. With --continue-on-error, the
# caller catches non-zero and records the failure; without it, the inner
# `fail` calls terminate the script.
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
  IN_FLIGHT_PHASE="pre-drain"
  mark_in_flight

  timer_stop "$ip"

  if (( SKIP_DRAIN == 1 )); then
    log "  --skip-drain set: skipping kubectl drain for ${name}"
  else
    # Compose the drain flags. --delete-emptydir-data is included by default
    # so cache-only pods (Prometheus WAL, scratch volumes) don't block
    # drain; --no-delete-emptydir-data drops it for workloads where those
    # caches are precious.
    local drain_flags="--ignore-daemonsets --timeout=${DRAIN_TIMEOUT}"
    if (( NO_DELETE_EMPTYDIR_DATA == 0 )); then
      drain_flags+=" --delete-emptydir-data"
    fi
    log "kubectl drain ${name} ${drain_flags}"
    cp_kubectl "drain ${name} ${drain_flags}" \
      || { worker_fail "$name" "drain failed (use --skip-drain to override)"; return 1; }
    IN_FLIGHT_DRAINED=1
    IN_FLIGHT_PHASE="post-drain-pre-upgrade"
    mark_in_flight
  fi

  # Capture the worker's pre-reboot bootID before kicking off the upgrade.
  # wait_node_bootid_changed compares against this post-reboot to defeat
  # the stale-apiserver-cache race that wait_node_ready alone can't catch.
  # See docs/update-cluster.md "Reboot detection (bootID)".
  local pre_bootid
  pre_bootid="$(capture_node_bootid "$name")"
  log "  pre-reboot bootID: ${pre_bootid:0:8}..."

  log "ssh root@${ip} bootc upgrade --apply  (auto-reboots; digest pre/post compared)"
  set +e
  bootc_upgrade_apply "$ip" "$name"
  rc=$?
  set -e

  # rc=1 from bootc_upgrade_apply now means the upgrade itself failed.
  # Route through worker_fail so --continue-on-error is respected (was
  # previously a direct fail() inside bootc_upgrade_apply that bypassed it).
  if (( rc == 1 )); then
    worker_fail "$name" "bootc upgrade --apply failed (see log above)"
    return 1
  fi

  if (( rc == 2 )); then
    log "worker ${name}: no update available; uncordoning and continuing."
    if (( SKIP_DRAIN == 0 )); then
      log "kubectl uncordon ${name}"
      cp_kubectl "uncordon ${name}" || { worker_fail "$name" "uncordon failed after no-op upgrade"; return 1; }
      IN_FLIGHT_UNCORDONED=1
    fi
    clear_in_flight
    timer_start "$ip"
    IN_FLIGHT_NODE=""
    IN_FLIGHT_IP=""
    SUCCEEDED_NODES+=("$name")
    return 0
  fi

  IN_FLIGHT_PHASE="post-upgrade-pre-ssh"
  mark_in_flight
  # Diagnostic SSH-drop gate (#261): poll for SSH to actually go down
  # before polling for it to come back, so we don't false-success on the
  # pre-reboot connection. A timeout here is logged but NOT fatal — the
  # bootID gate downstream is the source of truth for "rebooted".
  wait_ssh_drop "$ip" "$SSH_DROP_TIMEOUT" || true
  wait_ssh_back "$ip" "$SSH_TIMEOUT"       || { worker_fail "$name" "did not come back over SSH within ${SSH_TIMEOUT}s"; return 1; }
  # bootID gate runs BEFORE wait_node_ready to defeat the stale-apiserver-
  # cache race (apiserver may still serve Ready=True from the pre-reboot
  # lease before kubelet has actually re-registered post-reboot).
  IN_FLIGHT_PHASE="post-ssh-pre-bootID"
  mark_in_flight
  wait_node_bootid_changed "$name" "$pre_bootid" \
    || { worker_fail "$name" "node bootID did not change after ${READY_TIMEOUT}s (apiserver may be serving stale state)"; return 1; }
  # wait_node_ready before uncordon: kubelet has to rejoin (which may
  # report Ready,SchedulingDisabled while still cordoned) — the regex
  # in wait_node_ready handles both forms.
  IN_FLIGHT_PHASE="post-bootID-pre-Ready"
  mark_in_flight
  wait_node_ready "$name" "$READY_TIMEOUT" || { worker_fail "$name" "did not reach Ready within ${READY_TIMEOUT}s"; return 1; }
  # DaemonSet gate: Node Ready means kubelet+CNI binary present, NOT that
  # the Cilium/kube-proxy/coredns pods are forwarding traffic on this
  # node. Block here so we don't move to draining N+1 while the CNI on N
  # is still CrashLooping. See docs/update-cluster.md "Daemonset
  # readiness gate".
  IN_FLIGHT_PHASE="post-Ready-pre-DaemonSet"
  mark_in_flight
  wait_node_daemonsets_ready "$name" \
    || { worker_fail "$name" "kube-system DaemonSet pods not Ready on this node after ${DAEMONSET_TIMEOUT}s"; return 1; }

  IN_FLIGHT_PHASE="post-DaemonSet-pre-uncordon"
  mark_in_flight
  log "kubectl uncordon ${name}"
  cp_kubectl "uncordon ${name}" || { worker_fail "$name" "uncordon failed"; return 1; }
  IN_FLIGHT_UNCORDONED=1
  clear_in_flight

  timer_start "$ip"

  log "node ${name} updated"
  # INTER_NODE_SLEEP is applied per-BATCH (after run_worker_batch returns)
  # in the main loop below — not here. Per-worker would cumulatively delay
  # parallel-batch starts by N * INTER_NODE_SLEEP, which contradicts the
  # documented intent "settle window before the NEXT batch starts."

  IN_FLIGHT_NODE=""
  IN_FLIGHT_IP=""
  IN_FLIGHT_PHASE=""
  SUCCEEDED_NODES+=("$name")
}

# worker_fail NAME REASON — either record + continue (with --continue-on-error)
# or fail fast (default). Called from update_worker on every per-step failure.
worker_fail() {
  local name="$1" reason="$2"
  if (( CONTINUE_ON_ERROR == 1 )); then
    log "  ERROR on ${name}: ${reason} — continuing (--continue-on-error)"
    FAILED_NODES+=("${name}: ${reason}")
    # Restore the timer best-effort so the cluster doesn't permanently
    # lose auto-updates on this node.
    timer_start "${WORKER_IP_MAP[$name]:-}" 2>/dev/null || true
    IN_FLIGHT_NODE=""
    IN_FLIGHT_IP=""
    return 0
  fi
  fail "${name}: ${reason}"
}

# ---- main ------------------------------------------------------------------

log "config: $CONFIG_PATH"
log "CP=${CP_NAME} (${CP_IP}), workers=(${WORKER_NAMES[*]:-})"
log "flags: workers-only=${WORKERS_ONLY} skip-drain=${SKIP_DRAIN} skip-gates=${SKIP_GATES} dry-run=${DRY_RUN}"
log "       node-filter=${NODE_FILTER:-<none>} start-from=${START_FROM:-<none>}"
log "       continue-on-error=${CONTINUE_ON_ERROR} no-delete-emptydir-data=${NO_DELETE_EMPTYDIR_DATA} parallel=${PARALLEL}"
log "timeouts: drain=${DRAIN_TIMEOUT} ready=${READY_TIMEOUT}s daemonset=${DAEMONSET_TIMEOUT}s apiserver=${APISERVER_TIMEOUT}s ssh=${SSH_TIMEOUT}s ssh-drop=${SSH_DROP_TIMEOUT}s inter-node-sleep=${INTER_NODE_SLEEP}s"
# Per-node time budget pre-announcement: worst-case ceiling for an operator
# computing "when will this finish?" off the cluster size. Adds the new
# DaemonSet gate into the published budget (post-#208).
log "per-node worst-case budget: drain ${DRAIN_TIMEOUT} + ssh-back ${SSH_TIMEOUT}s + bootID ${READY_TIMEOUT}s + ready ${READY_TIMEOUT}s + daemonsets ${DAEMONSET_TIMEOUT}s"

# Build the worker list we'll actually process. --start-from skips entries
# until START_FROM is encountered (inclusive). Empty WORKER_NAMES → empty
# WORKERS_TO_RUN; loops below no-op.
WORKERS_TO_RUN=()
if [[ -n "$START_FROM" ]]; then
  seen=0
  for w in "${WORKER_NAMES[@]}"; do
    if [[ "$w" == "$START_FROM" ]]; then
      seen=1
    fi
    if (( seen == 1 )); then
      WORKERS_TO_RUN+=("$w")
    fi
  done
  log "--start-from=${START_FROM}: resuming with workers=(${WORKERS_TO_RUN[*]:-})"
else
  WORKERS_TO_RUN=("${WORKER_NAMES[@]}")
fi

# run_worker_batch NAMES...
#
# Process a batch of workers concurrently. Each name in the batch is
# update_worker'd in a background subshell; we `wait` on the batch and
# collect rcs. With --continue-on-error a per-worker failure is recorded
# and the batch proceeds; without it, any failure causes fail-fast (which
# in turn kills any still-running siblings via the EXIT trap pipeline).
#
# IMPORTANT: subshells get a COPY of FAILED_NODES / SUCCEEDED_NODES, so we
# round-trip them through a per-worker status file under $tmpdir.
run_worker_batch() {
  local batch=("$@")
  local n=${#batch[@]}
  if (( n == 0 )); then
    return 0
  fi
  if (( n == 1 )) || (( PARALLEL == 1 )); then
    # Serial fast path — no need for the tmpdir/subshell dance.
    local rc=0
    update_worker "${batch[0]}" "${WORKER_IP_MAP[${batch[0]}]}" || rc=$?
    return "$rc"
  fi

  log "PARALLEL batch (${n}): ${batch[*]}"
  # NOTE: this batch waits for ALL pids before returning, even when the
  # first worker fails — siblings continue in their subshells. With
  # --continue-on-error this is expected. WITHOUT --continue-on-error the
  # docstring above used to claim siblings get killed via the EXIT trap;
  # that's not what actually happens (the EXIT trap only fires when the
  # parent exits, which is after `wait`). For round-1 we document the
  # actual behavior here rather than introducing a watcher that kills
  # sibling pids — see follow-up issue noted in the commit message.
  #
  # Output is captured per-worker and replayed in batch-order after the
  # batch completes (deterministic logs > live streaming under -parallel).
  # During a long batch the operator will see NO output for the duration;
  # docs/update-cluster.md "Parallel mode" calls this out explicitly.
  # Use the module-scope BATCH_TMPDIR so cleanup_on_exit can rm it on
  # EXIT (even on Ctrl-C during the wait below).
  BATCH_TMPDIR="$(mktemp -d -t hbird-update-batch.XXXXXX)"
  local pids=()
  local i
  for i in "${!batch[@]}"; do
    local w="${batch[$i]}"
    (
      # Capture per-worker stdout/stderr to a log file so concurrent logs
      # don't interleave at the byte level.
      if update_worker "$w" "${WORKER_IP_MAP[$w]}" \
            >"${BATCH_TMPDIR}/${w}.log" 2>&1; then
        echo "0" >"${BATCH_TMPDIR}/${w}.rc"
      else
        echo "$?" >"${BATCH_TMPDIR}/${w}.rc"
      fi
    ) &
    pids+=("$!")
  done
  wait "${pids[@]}" || true

  # Replay each per-worker log in deterministic batch order so the operator
  # sees them grouped (not interleaved).
  local batch_rc=0
  for w in "${batch[@]}"; do
    if [[ -s "${BATCH_TMPDIR}/${w}.log" ]]; then
      sed "s/^/[parallel:${w}] /" "${BATCH_TMPDIR}/${w}.log"
    fi
    local wrc
    wrc="$(cat "${BATCH_TMPDIR}/${w}.rc" 2>/dev/null || echo 1)"
    if (( wrc != 0 )); then
      batch_rc=1
      # The subshell update_worker already populated FAILED_NODES /
      # SUCCEEDED_NODES in its own process; we have to re-mirror that
      # here. Re-parse the log for the "ERROR on NAME" line emitted by
      # worker_fail. (--continue-on-error pre-populates that line.)
      if (( CONTINUE_ON_ERROR == 1 )); then
        local reason
        reason="$(grep -m1 "ERROR on ${w}:" "${BATCH_TMPDIR}/${w}.log" \
                  | sed -E "s/.*ERROR on ${w}: //; s/ — continuing.*//" \
                  || true)"
        FAILED_NODES+=("${w}: ${reason:-unknown failure (see log above)}")
      fi
    else
      SUCCEEDED_NODES+=("$w")
    fi
  done
  # Successful workers had clear_in_flight remove their .in-flight files
  # already; leftover .in-flight files indicate genuinely-stuck nodes.
  # cleanup_on_exit will surface them as recovery hints AND rm the dir.
  # Here we just drop the per-worker log/rc artifacts to keep the tree
  # tidy in the (common) all-success case.
  if (( batch_rc == 0 )) && [[ -d "$BATCH_TMPDIR" ]]; then
    rm -rf "$BATCH_TMPDIR"
    BATCH_TMPDIR=""
  fi

  if (( batch_rc != 0 )) && (( CONTINUE_ON_ERROR == 0 )); then
    fail "one or more workers in parallel batch failed (see [parallel:*] logs above)"
  fi
  return "$batch_rc"
}

if [[ -n "$NODE_FILTER" ]]; then
  # Single-node mode: figure out whether it's the CP or a worker.
  if [[ "$NODE_FILTER" == "$CP_NAME" ]]; then
    update_cp
    SUCCEEDED_NODES+=("$CP_NAME")
  else
    found=0
    for w in "${WORKER_NAMES[@]}"; do
      if [[ "$w" == "$NODE_FILTER" ]]; then
        update_worker "$w" "${WORKER_IP_MAP[$w]}" || true
        found=1
        break
      fi
    done
    (( found == 1 )) || fail "--node=${NODE_FILTER} did not match CP_NAME or any WORKER_NAMES entry"
  fi
else
  if (( WORKERS_ONLY == 0 )); then
    if [[ -n "$START_FROM" ]]; then
      log "--start-from set: skipping CP (resume mode starts at worker '${START_FROM}')"
    else
      update_cp
      SUCCEEDED_NODES+=("$CP_NAME")
    fi
  else
    log "--workers-only: skipping CP"
  fi

  # Walk WORKERS_TO_RUN in batches of $PARALLEL.
  total=${#WORKERS_TO_RUN[@]}
  i=0
  while (( i < total )); do
    end=$(( i + PARALLEL ))
    (( end > total )) && end=$total
    batch=("${WORKERS_TO_RUN[@]:i:PARALLEL}")
    run_worker_batch "${batch[@]}" || true
    i=$end
    # Settle window between batches (NOT between individual workers; see
    # the note in update_worker). Zero value skips the sleep entirely.
    if (( i < total )) && (( INTER_NODE_SLEEP > 0 )); then
      log "pausing ${INTER_NODE_SLEEP}s before next node"
      if (( DRY_RUN == 0 )); then sleep "$INTER_NODE_SLEEP"; fi
    fi
  done
fi

log "============================================================"
log "Rolling update complete."
log "  succeeded (${#SUCCEEDED_NODES[@]}): ${SUCCEEDED_NODES[*]:-<none>}"
if (( ${#FAILED_NODES[@]} > 0 )); then
  log "  FAILED (${#FAILED_NODES[@]}):"
  for entry in "${FAILED_NODES[@]}"; do
    log "    - ${entry}"
  done
fi
log "============================================================"

# Exit code policy:
#   0  — every targeted node succeeded
#   1  — fail-fast abort (lib/build-common.sh::fail() exits 1 before we
#         reach this point; documented for completeness)
#   3  — one or more nodes failed AND --continue-on-error was set
if (( ${#FAILED_NODES[@]} > 0 )); then
  exit 3
fi
