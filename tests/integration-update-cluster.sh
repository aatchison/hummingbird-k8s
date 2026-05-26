#!/usr/bin/env bash
# integration-update-cluster.sh — drive scripts/update-cluster.sh assertions.
#
# Used by .github/workflows/integration-update-cluster.yml. Lives outside
# the YAML so shellcheck can lint it and so the logic is reviewable as
# bash rather than as embedded `run: |` blocks. Matches the convention
# established by tests/integration-cloud-init.sh.
#
# Sub-commands:
#   assert-dry-run-sequence   Run `update-cluster.sh --dry-run` and grep
#                             the captured log for the expected per-target
#                             action lines. Counts (not "exactly one") are
#                             load-bearing: drain/uncordon/timer-stop fire
#                             once per CP+worker, and cp_kubectl's DRY-RUN
#                             echo duplicates the real-action log line.
#
#   force-worker-upgrade <w>  Install a /usr/local/bin/bootc shim on worker
#                             <w> that (a) makes `bootc status --json` emit
#                             pre/post-distinct digests across calls so
#                             update-cluster's "no update available"
#                             short-circuit DOESN'T fire, and (b) issues
#                             `systemctl reboot` immediately after the
#                             real `bootc upgrade --apply` completes. The
#                             reboot drops ssh (rc=255 → script treats
#                             that as "upgrade applied; reboot expected")
#                             so wait_ssh_back + wait_node_ready actually
#                             run — the exact codepath PR #187's
#                             `$2 ~ /^Ready(,|$)/` regex fix exercises.
#                             Without this the workflow runs against a
#                             cluster that's already on :latest and
#                             bootc_upgrade_apply short-circuits at line
#                             441 of update-cluster.sh, defeating the
#                             regression guard.
#
#   unforce-worker-upgrade <w>
#                             Remove the shim from worker <w> so subsequent
#                             update-cluster invocations exercise the real
#                             bootc binary again. Safe to run repeatedly.
#
#   assert-update-node-real <w>
#                             Run `make update-node NODE=<w>` and assert
#                             the actual wait_node_ready codepath fired:
#                             worker reached "Ready,SchedulingDisabled"
#                             at least once during the run AND was
#                             uncordoned afterwards.
#
#   assert-exit-trap <w>      Launch `make update-node NODE=<w>` in the
#                             background, wait until the worker actually
#                             reaches `unschedulable=true` (drain done,
#                             NOT just "drain log line printed" — the
#                             cp_kubectl DRY-RUN echo races the real
#                             action), then SIGINT the process group.
#                             Assert update-cluster.sh's EXIT-trap
#                             recovery banner surfaced the manual
#                             uncordon command.
#
# All sub-commands source $CONFIG to get CP_NAME / WORKER_NAMES.
# $SSH_PRIVKEY_FILE must point at the runner's per-job key (the workflow
# generates it as ~/.ssh/integration_test_key).

set -euo pipefail

CONFIG="${CONFIG:?CONFIG=<path-to-cluster.ci.conf> is required}"
[[ -r "$CONFIG" ]] || { echo "config not readable: $CONFIG" >&2; exit 2; }

# shellcheck disable=SC1090
source "$CONFIG"

SSH_PRIVKEY_FILE="${SSH_PRIVKEY_FILE:-${HOME}/.ssh/integration_test_key}"

REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

ssh_root() {
  local host="$1"; shift
  ssh -i "$SSH_PRIVKEY_FILE" \
      -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR \
      -o ConnectTimeout=10 \
      -o BatchMode=yes \
      "root@${host}" "$@"
}

resolve_ip() {
  local domain="$1"
  sudo virsh -c qemu:///system domifaddr "$domain" 2>/dev/null \
    | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}'
}

cp_ip() {
  local ip
  ip="$(resolve_ip "$CP_NAME")"
  [[ -n "$ip" ]] || { echo "::error::cannot resolve CP_IP for $CP_NAME" >&2; exit 1; }
  printf '%s\n' "$ip"
}

# ---- assert-dry-run-sequence ------------------------------------------------

cmd_assert_dry_run_sequence() {
  local out
  out="$(mktemp)"
  # update-cluster.sh's log() writes to stderr (scripts/update-cluster.sh:52),
  # so capture both streams. Without 2>&1 every assertion below would see
  # an empty file and emit a spurious "got 0" error.
  ( cd "$REPO_ROOT" && CONFIG="$CONFIG" bash scripts/update-cluster.sh --dry-run 2>&1 ) \
    | tee "$out"

  local fail=0
  local n
  # assert_count <expected_count> <pattern> <label>
  assert_count() {
    local want="$1" pattern="$2" label="$3"
    n="$(grep -cE "$pattern" "$out" || true)"
    if [[ "$n" -ne "$want" ]]; then
      echo "::error::[$label] expected $want matches for /$pattern/, got $n" >&2
      fail=1
    fi
  }
  # assert_at_least <min_count> <pattern> <label>
  assert_at_least() {
    local min="$1" pattern="$2" label="$3"
    n="$(grep -cE "$pattern" "$out" || true)"
    if [[ "$n" -lt "$min" ]]; then
      echo "::error::[$label] expected >=$min matches for /$pattern/, got $n" >&2
      fail=1
    fi
  }

  local nodes=$(( 1 + ${#WORKER_NAMES[@]} ))  # CP + workers

  # timer_stop / timer_start fire once per node (CP + each worker). The
  # script emits one "DRY-RUN ssh root@... systemctl stop ..." per node.
  # Post-#181: timer_stop stops both the semver-update timer (canonical
  # post-PR unit) AND the legacy fetch-apply timer (still active on
  # mid-migration hosts); timer_start only restarts the semver timer.
  assert_count "$nodes" \
    'DRY-RUN ssh root@.* systemctl stop bootc-semver-update.timer bootc-fetch-apply-updates.timer' \
    'timer-stop per-node'
  assert_count "$nodes" \
    'DRY-RUN ssh root@.* systemctl start bootc-semver-update.timer' \
    'timer-start per-node'

  # CP header appears exactly once.
  assert_count 1 "CP: ${CP_NAME}" "CP header"

  for w in "${WORKER_NAMES[@]}"; do
    assert_count 1 "WORKER: ${w}" "worker header ${w}"

    # update-cluster.sh emits TWO log lines per worker drain:
    #   (1) the real-action "kubectl drain ${w} --ignore-daemonsets..." at
    #       scripts/update-cluster.sh:514
    #   (2) cp_kubectl's "DRY-RUN cp_kubectl -- drain ${w}..." at line 181
    # Anchor to the cp_kubectl DRY-RUN echo (unique) for an exact-1 count.
    assert_count 1 "DRY-RUN cp_kubectl -- drain ${w} --ignore-daemonsets" \
      "worker drain ${w}"

    # Same for uncordon: line 529 ("kubectl uncordon ${w}") AND cp_kubectl's
    # DRY-RUN echo of the same. Anchor with a terminal `$` against the
    # cp_kubectl line. fgrep-style anchor: full pattern below.
    assert_count 1 "DRY-RUN cp_kubectl -- uncordon ${w}$" \
      "worker uncordon ${w}"
  done

  if (( fail != 0 )); then
    return 1
  fi
  echo "ok: dry-run sequence verified (CP + ${#WORKER_NAMES[@]} workers)"
}

# ---- force-worker-upgrade ---------------------------------------------------

# Install a /usr/local/bin/bootc shim on the worker that:
#   - For `bootc upgrade --apply`: runs the real /usr/bin/bootc, then
#     immediately schedules `systemctl reboot`. The reboot kills sshd,
#     so the script's ssh returns rc=255 — bootc_upgrade_apply() treats
#     that as "upgrade applied; reboot expected" (the success branch).
#   - For `bootc status --json`: returns a JSON blob with a synthetic
#     digest that flips between two values across calls (tracked via
#     /var/lib/bootc-shim-counter). The script's pre/post digest compare
#     then sees a difference and does NOT short-circuit at the "no update
#     available" branch.
#   - For anything else: passthrough to /usr/bin/bootc.
#
# /usr/local/bin precedes /usr/bin in root's default PATH on Fedora bootc,
# so an unqualified `bootc` in the remote shell resolves to the shim.
cmd_force_worker_upgrade() {
  local worker="$1"
  [[ -n "$worker" ]] || { echo "force-worker-upgrade <worker>" >&2; exit 2; }
  local ip
  ip="$(resolve_ip "$worker")"
  [[ -n "$ip" ]] || { echo "::error::cannot resolve IP for $worker" >&2; exit 1; }

  ssh_root "$ip" 'cat > /usr/local/bin/bootc' <<'SHIM'
#!/bin/bash
# Test-only shim — DO NOT ship this image. See
# tests/integration-update-cluster.sh::cmd_force_worker_upgrade.
set -u
REAL=/usr/bin/bootc
COUNTER=/var/lib/bootc-shim-counter

case "$*" in
  'status --json'*)
    n=0
    [[ -r "$COUNTER" ]] && n="$(cat "$COUNTER")"
    n=$(( n + 1 ))
    printf '%s\n' "$n" > "$COUNTER"
    if (( n <= 1 )); then
      digest='sha256:0000000000000000000000000000000000000000000000000000000000000001'
    else
      digest='sha256:0000000000000000000000000000000000000000000000000000000000000002'
    fi
    printf '{"status":{"booted":{"image":{"imageDigest":"%s"}}}}\n' "$digest"
    exit 0
    ;;
  'upgrade --apply'*)
    # Run the real bootc upgrade (no-op if no new image), then reboot to
    # exercise the wait_ssh_back + wait_node_ready codepath. Detach the
    # reboot so this shim returns first; the parent ssh dies when sshd
    # goes down (rc=255 → the script's success branch).
    "$REAL" upgrade || true
    ( sleep 1; systemctl reboot ) >/dev/null 2>&1 &
    disown
    exit 0
    ;;
  *)
    exec "$REAL" "$@"
    ;;
esac
SHIM
  ssh_root "$ip" 'chmod 0755 /usr/local/bin/bootc; rm -f /var/lib/bootc-shim-counter'
  echo "ok: installed bootc shim on ${worker} (${ip})"
}

cmd_unforce_worker_upgrade() {
  local worker="$1"
  [[ -n "$worker" ]] || { echo "unforce-worker-upgrade <worker>" >&2; exit 2; }
  local ip
  ip="$(resolve_ip "$worker")"
  [[ -n "$ip" ]] || return 0  # node may be torn down already; tolerate
  ssh_root "$ip" 'rm -f /usr/local/bin/bootc /var/lib/bootc-shim-counter' || true
  echo "ok: removed bootc shim from ${worker}"
}

# ---- assert-update-node-real ------------------------------------------------

cmd_assert_update_node_real() {
  local worker="$1"
  [[ -n "$worker" ]] || { echo "assert-update-node-real <worker>" >&2; exit 2; }

  local out start end elapsed
  out="$(mktemp)"
  start="$(date +%s)"
  ( cd "$REPO_ROOT" && \
      sudo --preserve-env=STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT \
        make update-node CONFIG="$CONFIG" NODE="$worker" ) 2>&1 | tee "$out"
  end="$(date +%s)"
  elapsed=$(( end - start ))
  echo "update-node ${worker} elapsed: ${elapsed}s"

  local fail=0

  # Did the script reach wait_node_ready? The log line is "waiting for
  # node ${worker} to report Ready" at update-cluster.sh:346. Must appear
  # — its absence means the no-update short-circuit fired (BLOCKER #2).
  if ! grep -qE "waiting for node ${worker} to report Ready" "$out"; then
    echo "::error::wait_node_ready never ran for ${worker} — the BLOCKER #2 short-circuit is still in play" >&2
    fail=1
  fi

  # Did the worker actually go through Ready,SchedulingDisabled? Check
  # via the CP. The script only proceeds past wait_node_ready if the
  # regex matched. The "node ${worker} Ready after ~Ns" log line is the
  # post-match success message.
  if ! grep -qE "node ${worker} Ready after ~[0-9]+s" "$out"; then
    echo "::error::wait_node_ready did not report ${worker} Ready post-reboot — wait_node_ready regex bug?" >&2
    fail=1
  fi

  # Final state: uncordoned.
  local CP_IP unsched
  CP_IP="$(cp_ip)"
  unsched="$(ssh_root "$CP_IP" \
    "kubectl --kubeconfig=/etc/kubernetes/admin.conf get node ${worker} -o jsonpath='{.spec.unschedulable}'" \
    2>/dev/null || true)"
  if [[ "$unsched" == "true" ]]; then
    echo "::error::worker ${worker} was left cordoned after update-node" >&2
    fail=1
  fi

  if (( fail != 0 )); then
    return 1
  fi
  echo "ok: update-node ${worker} exercised wait_node_ready end-to-end in ${elapsed}s"
}

# ---- assert-exit-trap -------------------------------------------------------

cmd_assert_exit_trap() {
  local target="$1"
  [[ -n "$target" ]] || { echo "assert-exit-trap <worker>" >&2; exit 2; }

  local out CP_IP mk_pid
  out="$(mktemp)"
  CP_IP="$(cp_ip)"

  # Launch update-node in its own process group so we can SIGINT the
  # whole tree (sudo + make + bash + ssh + kubectl).
  setsid -- bash -c "
    cd '${REPO_ROOT}' && \
    sudo --preserve-env=STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT \
      make update-node CONFIG='${CONFIG}' NODE='${target}'
  " >"$out" 2>&1 &
  mk_pid=$!

  # Wait until the worker is actually cordoned. update-cluster.sh's
  # `log "kubectl drain ..."` line (the previous test signal) fires
  # BEFORE cp_kubectl actually executes — SIGINT'ing on that log line
  # would kill the script mid-drain, leaving IN_FLIGHT_DRAINED=0 and
  # the EXIT trap silently skipping the recovery banner.
  #
  # `kubectl drain` sets .spec.unschedulable=true atomically as its
  # first step, BEFORE the eviction wait loop. Once we see it, the
  # script is past `cp_kubectl drain` returning 0 → IN_FLIGHT_DRAINED=1.
  # NOW SIGINT is safe.
  local deadline=$((SECONDS + 180))
  local cordoned=0
  while (( SECONDS < deadline )); do
    if [[ "$(ssh_root "$CP_IP" \
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf get node ${target} -o jsonpath='{.spec.unschedulable}'" \
        2>/dev/null || true)" == "true" ]]; then
      cordoned=1
      break
    fi
    sleep 3
  done

  if (( cordoned != 1 )); then
    echo "::error::worker ${target} never reached unschedulable=true within 180s" >&2
    sudo kill -TERM "-${mk_pid}" 2>/dev/null || true
    cat "$out" || true
    return 1
  fi

  # Now SIGINT the make process group. IN_FLIGHT_DRAINED is 1, so
  # cleanup_on_exit (update-cluster.sh:271) will print the banner.
  sudo kill -INT "-${mk_pid}" 2>/dev/null || true
  wait "$mk_pid" 2>/dev/null || true

  # Banner contains the manual uncordon command; the printf format at
  # scripts/update-cluster.sh:276 embeds the full kubectl invocation.
  local deadline2=$((SECONDS + 30))
  local banner=0
  while (( SECONDS < deadline2 )); do
    if grep -qE "kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon ${target}" "$out"; then
      banner=1
      break
    fi
    sleep 2
  done

  # Belt-and-suspenders: regardless of banner outcome, restore the
  # worker so the destroy step doesn't trip over it.
  ssh_root "$CP_IP" \
    "kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon ${target}" \
    >/dev/null 2>&1 || true

  if (( banner != 1 )); then
    echo "::error::EXIT-trap recovery banner did not surface uncordon for ${target}" >&2
    cat "$out" || true
    return 1
  fi
  echo "ok: EXIT-trap surfaced recovery banner after mid-flight SIGINT"
}

# ---- dispatch ---------------------------------------------------------------

cmd="${1:-}"
shift || true
case "$cmd" in
  assert-dry-run-sequence)   cmd_assert_dry_run_sequence "$@" ;;
  force-worker-upgrade)      cmd_force_worker_upgrade "$@" ;;
  unforce-worker-upgrade)    cmd_unforce_worker_upgrade "$@" ;;
  assert-update-node-real)   cmd_assert_update_node_real "$@" ;;
  assert-exit-trap)          cmd_assert_exit_trap "$@" ;;
  *)
    echo "usage: CONFIG=<path> $0 {assert-dry-run-sequence|force-worker-upgrade WORKER|unforce-worker-upgrade WORKER|assert-update-node-real WORKER|assert-exit-trap WORKER}" >&2
    exit 2
    ;;
esac
