#!/usr/bin/env bats
#
# Unit tests for scripts/update-cluster.sh (issues #189, #190, #192, #194).
#
# These tests cover flag parsing (mutual-exclusion, unknown-arg rejection,
# --help formatting), env-var validation (security: env vars survive
# `sudo -E` into root context — hostile values MUST be rejected), and
# --dry-run sequencing. The dry-run assertions deliberately codify the
# patterns that integration-update-cluster.yml's `assert-dry-run-sequence`
# command keys on — so a refactor that subtly changes log lines is caught
# at pr-validate time, not only on the self-hosted runner.
#
# Run via:
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats@sha256:79d75... tests/scripts/
#
# OR locally:  bats tests/scripts/update-cluster.bats
#
# All tests run as a non-root user — update-cluster.sh's root check is
# inside an `if (( DRY_RUN == 0 ))` guard, so dry-run + --help + early
# flag-validation paths all run fine without sudo. No SSH / virsh /
# kubectl is ever invoked.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/update-cluster.sh"
  CONF="${REPO_ROOT}/cluster.example.conf"
  # Some tests need to source the lib indirectly through the script; isolate
  # $HOME so any read of ~/.ssh/* can never hit the operator's real keys.
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"
}

# ---------------------------------------------------------------------------
# Flag parsing — mutual exclusion + bad input
# ---------------------------------------------------------------------------

@test "update-cluster: --workers-only + --node= are mutually exclusive" {
  run env CONFIG="$CONF" bash "$SCRIPT" --workers-only --node=hbird-w1 --dry-run
  [ "$status" -ne 0 ]
  [[ "$output" == *"mutually exclusive"* ]]
}

@test "update-cluster: bare --node (no '=') exits non-zero with diagnostic" {
  # --node without `=NAME` falls through to the `--node)` arm of the case
  # statement, which fails fast with a clear message.
  run env CONFIG="$CONF" bash "$SCRIPT" --node hbird-w1 --dry-run
  [ "$status" -ne 0 ]
  [[ "$output" == *"--node requires a value"* ]]
}

@test "update-cluster: --node=unknown-worker fails after sourcing config" {
  run env CONFIG="$CONF" bash "$SCRIPT" --node=does-not-exist --dry-run
  [ "$status" -ne 0 ]
  # Error must reference the rejected node name AND the set it was checked
  # against (CP_NAME or WORKER_NAMES) so the operator sees why.
  [[ "$output" == *"does-not-exist"* ]]
  [[ "$output" == *"WORKER_NAMES"* ]]
}

@test "update-cluster: unknown flag --bogus exits non-zero with usage hint" {
  run env CONFIG="$CONF" bash "$SCRIPT" --bogus --dry-run
  [ "$status" -ne 0 ]
  [[ "$output" == *"unknown argument"* ]]
  [[ "$output" == *"--help"* ]]
}

# ---------------------------------------------------------------------------
# --help formatting
# ---------------------------------------------------------------------------

@test "update-cluster: --help exits 0 and lists the canonical flags" {
  run bash "$SCRIPT" --help
  [ "$status" -eq 0 ]
  # The script extracts its own usage from the header comment via sed; the
  # canonical flag names below must all appear in that block.
  [[ "$output" == *"--workers-only"* ]]
  [[ "$output" == *"--node="* ]]
  [[ "$output" == *"--skip-drain"* ]]
  [[ "$output" == *"--dry-run"* ]]
}

@test "update-cluster --help lists all new operator-ergonomics flags" {
  run bash "$SCRIPT" --help
  [ "$status" -eq 0 ]
  echo "$output" | grep -q -- "--start-from=NAME"
  echo "$output" | grep -q -- "--continue-on-error"
  echo "$output" | grep -q -- "--no-delete-emptydir-data"
  echo "$output" | grep -q -- "--parallel=N"
  echo "$output" | grep -q "DRAIN_TIMEOUT"
  echo "$output" | grep -q "READY_TIMEOUT"
  echo "$output" | grep -q "APISERVER_TIMEOUT"
  echo "$output" | grep -q "SSH_TIMEOUT"
  echo "$output" | grep -q "INTER_NODE_SLEEP"
}

# ---------------------------------------------------------------------------
# --dry-run sequencing — codifies the patterns integration-update-cluster
# .yml::assert-dry-run-sequence (tests/integration-update-cluster.sh) keys
# on. If a refactor reorders / renames log lines, the integration test will
# silently start matching zero lines; these unit tests catch the regression
# at pr-validate time.
# ---------------------------------------------------------------------------

# Helper: extract WORKER_NAMES from cluster.example.conf so the assertions
# below stay in sync with whatever the operator-facing example ships.
_load_example_workers() {
  # shellcheck disable=SC1090
  ( source "$CONF" && printf '%s\n' "${WORKER_NAMES[@]}" )
}

@test "update-cluster: --dry-run exits 0 and emits the CP header exactly once" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  # cluster.example.conf has CP_NAME=hbird-cp1.
  cp_lines="$(echo "$output" | grep -c 'CP: hbird-cp1' || true)"
  [ "$cp_lines" -eq 1 ]
}

@test "update-cluster: --dry-run emits exactly one WORKER header per WORKER_NAMES entry" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  while read -r w; do
    n="$(echo "$output" | grep -c "WORKER: ${w}" || true)"
    [ "$n" -eq 1 ] || {
      echo "expected exactly 1 'WORKER: ${w}' line, got $n" >&2
      false
    }
  done < <(_load_example_workers)
}

@test "update-cluster: --dry-run emits the per-node timer stop/start patterns the integration test asserts on" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  # cluster.example.conf: 1 CP + 2 workers = 3 nodes.
  # Post-#181: stop targets both semver + legacy timers (mid-migration
  # hosts may have either active); start only restarts the semver timer
  # so update-cluster.sh doesn't silently re-enable the legacy unit on
  # operators who intentionally disabled it.
  stop_n="$(echo "$output" | grep -cE 'DRY-RUN ssh root@.* systemctl stop bootc-semver-update.timer bootc-fetch-apply-updates.timer' || true)"
  start_n="$(echo "$output" | grep -cE 'DRY-RUN ssh root@.* systemctl start bootc-semver-update.timer$' || true)"
  [ "$stop_n" -eq 3 ]
  [ "$start_n" -eq 3 ]
}

@test "update-cluster: --dry-run emits one cp_kubectl drain + uncordon line per worker" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  while read -r w; do
    drain_n="$(echo "$output" | grep -cE "DRY-RUN cp_kubectl -- drain ${w} --ignore-daemonsets" || true)"
    uncordon_n="$(echo "$output" | grep -cE "DRY-RUN cp_kubectl -- uncordon ${w}\$" || true)"
    [ "$drain_n" -eq 1 ] || {
      echo "drain count for ${w}: $drain_n (want 1)" >&2; false
    }
    [ "$uncordon_n" -eq 1 ] || {
      echo "uncordon count for ${w}: $uncordon_n (want 1)" >&2; false
    }
  done < <(_load_example_workers)
}

@test "update-cluster: --dry-run --workers-only does NOT emit the CP header" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --workers-only
  [ "$status" -eq 0 ]
  cp_lines="$(echo "$output" | grep -c 'CP: hbird-cp1' || true)"
  [ "$cp_lines" -eq 0 ]
  # The "skipping CP" notice must appear.
  [[ "$output" == *"--workers-only: skipping CP"* ]]
}

# ---------------------------------------------------------------------------
# --start-from skips early workers (resume mode).
# ---------------------------------------------------------------------------

@test "--start-from=hbird-w2 skips hbird-w1, processes hbird-w2 onwards" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --start-from=hbird-w2
  [ "$status" -eq 0 ]
  # Resume mode skips the CP entirely.
  echo "$output" | grep -q "resume mode starts at worker 'hbird-w2'"
  # WORKER block for hbird-w2 must appear.
  echo "$output" | grep -q "WORKER: hbird-w2"
  # WORKER block for hbird-w1 must NOT appear.
  ! echo "$output" | grep -q "WORKER: hbird-w1"
}

@test "--start-from validates the name against WORKER_NAMES" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --start-from=does-not-exist
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "did not match any WORKER_NAMES entry"
}

@test "--start-from= with empty value is rejected at parse time" {
  # Regression for the security/correctness review: an empty RHS was
  # silently accepted by the `--start-from=*` matcher, then guarded out
  # by `[[ -n "$START_FROM" ]]` — so the operator saw NO error and the
  # script proceeded as if --start-from had not been passed. Now rejected.
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --start-from=
  [ "$status" -ne 0 ]
  echo "$output" | grep -q -- "--start-from="
}

# ---------------------------------------------------------------------------
# --start-from + --node are mutually exclusive.
# ---------------------------------------------------------------------------

@test "--start-from and --node= are rejected together" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --start-from=hbird-w1 --node=hbird-w2
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "mutually exclusive"
  echo "$output" | grep -q -- "--node="
  echo "$output" | grep -q -- "--start-from="
}

# ---------------------------------------------------------------------------
# --parallel batches concurrent workers (in dry-run, the [parallel:NAME]
# log prefix is the visible signal that both subshells fired).
# ---------------------------------------------------------------------------

@test "--parallel=2 fans out two workers concurrently (dry-run logs both)" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --parallel=2
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "PARALLEL batch (2): hbird-w1 hbird-w2"
  echo "$output" | grep -q "\\[parallel:hbird-w1\\]"
  echo "$output" | grep -q "\\[parallel:hbird-w2\\]"
}

@test "--parallel=1 stays on the serial fast path (no PARALLEL log)" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --parallel=1
  [ "$status" -eq 0 ]
  ! echo "$output" | grep -q "PARALLEL batch"
  ! echo "$output" | grep -q "\\[parallel:"
}

@test "--parallel rejects non-integer values" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --parallel=abc
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "requires a positive integer"
}

@test "--parallel rejects zero" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --parallel=0
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "requires a positive integer"
}

# ---------------------------------------------------------------------------
# --no-delete-emptydir-data drops the --delete-emptydir-data flag from drain.
# ---------------------------------------------------------------------------

@test "--no-delete-emptydir-data omits --delete-emptydir-data from drain" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --no-delete-emptydir-data
  [ "$status" -eq 0 ]
  # Drain line must NOT contain --delete-emptydir-data.
  ! echo "$output" | grep -E "drain hbird-w[12] .*--delete-emptydir-data"
  # But it must still contain --ignore-daemonsets + --timeout.
  echo "$output" | grep -q "drain hbird-w1 --ignore-daemonsets --timeout="
}

@test "default drain command includes --delete-emptydir-data" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "drain hbird-w1 --ignore-daemonsets --timeout=5m --delete-emptydir-data"
}

# ---------------------------------------------------------------------------
# DRAIN_TIMEOUT / READY_TIMEOUT / SSH_TIMEOUT / INTER_NODE_SLEEP env overrides
# thread through to kubectl drain / wait_node_ready / wait_ssh_back logs.
# ---------------------------------------------------------------------------

@test "DRAIN_TIMEOUT=10m overrides the default 5m in drain command" {
  DRAIN_TIMEOUT=10m run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "drain hbird-w1 --ignore-daemonsets --timeout=10m"
  echo "$output" | grep -q "drain=10m"
}

@test "READY_TIMEOUT env override surfaces in the wait_node_ready log" {
  READY_TIMEOUT=600 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "report Ready (timeout 600s)"
  echo "$output" | grep -q "ready=600s"
}

@test "SSH_TIMEOUT env override surfaces in the wait_ssh_back log" {
  SSH_TIMEOUT=120 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "SSH to come back on .* (timeout 120s)"
  echo "$output" | grep -q "ssh=120s"
}

@test "INTER_NODE_SLEEP env override surfaces in the pause log" {
  INTER_NODE_SLEEP=30 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "pausing 30s before next node"
  echo "$output" | grep -q "inter-node-sleep=30s"
}

# ---------------------------------------------------------------------------
# SECURITY: env-var validation rejects hostile values that would otherwise
# survive `sudo -E` into the privileged context.
#
# These five knobs flow into shell-injection or bash-arithmetic sinks
# (DRAIN_TIMEOUT → cp_kubectl remote shell; the *_TIMEOUTs and
# INTER_NODE_SLEEP → `while (( elapsed < timeout ))` arithmetic / sleep).
# Each MUST be validated at startup BEFORE any privileged call so hostile
# env vars can never reach the sinks.
# ---------------------------------------------------------------------------

@test "DRAIN_TIMEOUT rejects shell-injection payload" {
  DRAIN_TIMEOUT='5m; rm -rf /; #' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "DRAIN_TIMEOUT"
}

@test "DRAIN_TIMEOUT rejects values with non-duration suffix" {
  DRAIN_TIMEOUT='5x' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "DRAIN_TIMEOUT"
}

@test "DRAIN_TIMEOUT accepts bare integer (no suffix)" {
  DRAIN_TIMEOUT='30' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
}

@test "READY_TIMEOUT rejects bash-arithmetic injection payload" {
  READY_TIMEOUT='a[0$(reboot)]' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "READY_TIMEOUT"
}

@test "READY_TIMEOUT rejects non-integer value" {
  READY_TIMEOUT='5m' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "READY_TIMEOUT"
}

@test "APISERVER_TIMEOUT rejects bash-arithmetic injection payload" {
  APISERVER_TIMEOUT='a[0$(reboot)]' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "APISERVER_TIMEOUT"
}

@test "SSH_TIMEOUT rejects bash-arithmetic injection payload" {
  SSH_TIMEOUT='a[0$(reboot)]' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "SSH_TIMEOUT"
}

@test "INTER_NODE_SLEEP rejects shell-injection / non-integer" {
  INTER_NODE_SLEEP='1; reboot' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "INTER_NODE_SLEEP"
}

@test "INTER_NODE_SLEEP=0 is accepted (skip-sleep case)" {
  INTER_NODE_SLEEP=0 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# --continue-on-error is parsed (mostly a smoke test — actually triggering
# the failure path requires kubectl mocking which is out of scope here).
# ---------------------------------------------------------------------------

@test "--continue-on-error is accepted and surfaces in the flag-summary log" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --continue-on-error
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "continue-on-error=1"
}

# ---------------------------------------------------------------------------
# --workers-only + --start-from compose (both skip the CP).
# ---------------------------------------------------------------------------

@test "--workers-only and --start-from compose" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --workers-only --start-from=hbird-w2
  [ "$status" -eq 0 ]
  ! echo "$output" | grep -q "WORKER: hbird-w1"
  echo "$output" | grep -q "WORKER: hbird-w2"
}

# ---------------------------------------------------------------------------
# bootID + DaemonSet readiness gates (#195).
#
# Two new gates run between wait_ssh_back and uncordon:
#   1. capture_node_bootid before the upgrade + wait_node_bootid_changed
#      after wait_ssh_back, before wait_node_ready. Defeats the stale
#      apiserver-cache Ready=True hit (apiserver may still report Ready
#      from the pre-reboot lease before kubelet has re-registered).
#   2. wait_node_daemonsets_ready after wait_node_ready, before uncordon.
#      Node Ready means kubelet+CNI binary present; not that the Cilium /
#      kube-proxy / coredns DaemonSet pods on this node are actually
#      forwarding traffic.
#
# These tests assert the dry-run log lines for both gates appear in the
# right order on EVERY node (CP + each worker). The integration test will
# pick up real-cluster behavior via the self-hosted runner.
# ---------------------------------------------------------------------------

@test "dry-run emits 'pre-reboot bootID' capture line for the CP and each worker" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  # cluster.example.conf = 1 CP + 2 workers = 3 nodes total.
  capture_n="$(echo "$output" | grep -cE 'DRY-RUN would capture pre-reboot bootID for ' || true)"
  [ "$capture_n" -eq 3 ]
  echo "$output" | grep -q "DRY-RUN would capture pre-reboot bootID for hbird-cp1"
  echo "$output" | grep -q "DRY-RUN would capture pre-reboot bootID for hbird-w1"
  echo "$output" | grep -q "DRY-RUN would capture pre-reboot bootID for hbird-w2"
}

@test "dry-run emits 'waiting for node X bootID to change' for the CP and each worker" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  wait_n="$(echo "$output" | grep -cE 'waiting for node .* bootID to change from pre-reboot value' || true)"
  [ "$wait_n" -eq 3 ]
  echo "$output" | grep -q "waiting for node hbird-cp1 bootID to change"
  echo "$output" | grep -q "waiting for node hbird-w1 bootID to change"
  echo "$output" | grep -q "waiting for node hbird-w2 bootID to change"
}

@test "dry-run emits 'waiting for kube-system DaemonSet pods on X' for the CP and each worker" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  ds_n="$(echo "$output" | grep -cE 'waiting for kube-system DaemonSet pods on .* to be Ready' || true)"
  [ "$ds_n" -eq 3 ]
  echo "$output" | grep -q "waiting for kube-system DaemonSet pods on hbird-cp1"
  echo "$output" | grep -q "waiting for kube-system DaemonSet pods on hbird-w1"
  echo "$output" | grep -q "waiting for kube-system DaemonSet pods on hbird-w2"
}

@test "dry-run gates appear in the documented order on a worker: bootID-changed BEFORE node Ready BEFORE daemonsets-ready BEFORE uncordon" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  # Extract the per-line "marker => line number" for hbird-w1 (any worker
  # would do; w1 is the first in WORKER_NAMES). Anchored grep -n picks the
  # first occurrence.
  bootid_line="$(  echo "$output" | grep -nE 'waiting for node hbird-w1 bootID to change'                | head -1 | cut -d: -f1)"
  ready_line="$(   echo "$output" | grep -nE 'waiting for node hbird-w1 to report Ready'                  | head -1 | cut -d: -f1)"
  ds_line="$(      echo "$output" | grep -nE 'waiting for kube-system DaemonSet pods on hbird-w1'        | head -1 | cut -d: -f1)"
  uncordon_line="$(echo "$output" | grep -nE 'DRY-RUN cp_kubectl -- uncordon hbird-w1$'                  | head -1 | cut -d: -f1)"
  # All four must be present.
  [ -n "$bootid_line"  ]
  [ -n "$ready_line"   ]
  [ -n "$ds_line"      ]
  [ -n "$uncordon_line" ]
  # Order constraint: bootid < ready < daemonsets < uncordon.
  [ "$bootid_line" -lt "$ready_line"  ] || { echo "bootid=$bootid_line ready=$ready_line" >&2; false; }
  [ "$ready_line"  -lt "$ds_line"     ] || { echo "ready=$ready_line ds=$ds_line"         >&2; false; }
  [ "$ds_line"     -lt "$uncordon_line" ] || { echo "ds=$ds_line uncordon=$uncordon_line" >&2; false; }
}

@test "dry-run gates do NOT shift drain/uncordon counts (integration-test contract)" {
  # Belt-and-suspenders: the integration test's assert-dry-run-sequence
  # counts drains + uncordons by anchored cp_kubectl DRY-RUN echoes. The
  # bootID + daemonset gate dry-run lines must NEVER match those patterns.
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  drain_n="$(  echo "$output" | grep -cE 'DRY-RUN cp_kubectl -- drain hbird-w[12] --ignore-daemonsets' || true)"
  uncordon_n="$(echo "$output" | grep -cE 'DRY-RUN cp_kubectl -- uncordon hbird-w[12]$'                 || true)"
  [ "$drain_n"    -eq 2 ]
  [ "$uncordon_n" -eq 2 ]
}

@test "dry-run gates use READY_TIMEOUT, not a hardcoded value" {
  # No new env knob: the bootID + daemonset gates share READY_TIMEOUT.
  # An override must thread through both gate log lines.
  READY_TIMEOUT=600 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "bootID to change from pre-reboot value (timeout 600s)"
  echo "$output" | grep -q "kube-system DaemonSet pods on hbird-w1 to be Ready (timeout 600s)"
}

# ---------------------------------------------------------------------------
# PR #208 round-1 review hardenings — bootID retry, baseline-unready
# exclusion, phase-1 wait, --skip-gates, DAEMONSET_TIMEOUT validation,
# READY_TIMEOUT=0 rejection.
# ---------------------------------------------------------------------------

@test "READY_TIMEOUT=0 is rejected (strict positive)" {
  # Pre-#208 the regex permitted 0, which would defeat the gates.
  READY_TIMEOUT=0 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "READY_TIMEOUT"
}

@test "DAEMONSET_TIMEOUT defaults to READY_TIMEOUT when unset" {
  # No explicit DAEMONSET_TIMEOUT — should mirror READY_TIMEOUT in the
  # startup-flags log and in the gate log line.
  READY_TIMEOUT=450 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "daemonset=450s"
  echo "$output" | grep -q "kube-system DaemonSet pods on hbird-w1 to be Ready (timeout 450s)"
}

@test "DAEMONSET_TIMEOUT env override surfaces independent of READY_TIMEOUT" {
  DAEMONSET_TIMEOUT=900 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "daemonset=900s"
  echo "$output" | grep -q "kube-system DaemonSet pods on hbird-w1 to be Ready (timeout 900s)"
  # But the bootID gate must still be bound by READY_TIMEOUT.
  echo "$output" | grep -q "bootID to change from pre-reboot value (timeout 300s)"
}

@test "DAEMONSET_TIMEOUT rejects bash-arithmetic injection payload" {
  DAEMONSET_TIMEOUT='a[0$(reboot)]' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "DAEMONSET_TIMEOUT"
}

@test "DAEMONSET_TIMEOUT=0 is rejected (strict positive)" {
  DAEMONSET_TIMEOUT=0 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "DAEMONSET_TIMEOUT"
}

@test "--skip-gates skips bootID + DaemonSet gates (dry-run)" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --skip-gates
  [ "$status" -eq 0 ]
  # Flag must surface in the startup summary.
  echo "$output" | grep -q "skip-gates=1"
  # In dry-run, gate log lines are emitted before the skip check
  # short-circuits — but on a REAL run the gates skip-log the early-out.
  # For the dry-run we assert the skip-log appears at least once per node
  # (CP + 2 workers = 3 nodes; skip log fires for both gates).
  # Note: in dry-run we still execute the gate's early `(( DRY_RUN == 1 ))`
  # branch BEFORE the SKIP_GATES check. To make --skip-gates dry-run
  # observable we put SKIP_GATES ahead of DRY_RUN in both gates. So:
  bootid_skip=$(echo "$output" | grep -c "skipping bootID-changed gate" || true)
  ds_skip=$(echo "$output" | grep -c "skipping DaemonSet readiness gate" || true)
  [ "$bootid_skip" -ge 3 ]
  [ "$ds_skip" -ge 3 ]
}

@test "--skip-gates is documented in --help output" {
  run bash "$SCRIPT" --help
  [ "$status" -eq 0 ]
  echo "$output" | grep -q -- "--skip-gates"
}

@test "DAEMONSET_TIMEOUT is documented in --help output" {
  run bash "$SCRIPT" --help
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "DAEMONSET_TIMEOUT"
}

@test "per-node worst-case time budget is announced in the startup banner" {
  # The pre-announce line gives operators a quick "how long will this
  # take" ceiling. Must reflect override values.
  READY_TIMEOUT=400 DAEMONSET_TIMEOUT=500 SSH_TIMEOUT=200 DRAIN_TIMEOUT=10m \
    run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "per-node worst-case budget: drain 10m + ssh-back 200s + bootID 400s + ready 400s + daemonsets 500s"
}

@test "bootID truncation log uses ASCII '...' not U+2026 ellipsis" {
  # grep/copy-paste hygiene — the U+2026 (…) glyph caused trouble in
  # postmortems where operators were greping for "post=" substrings.
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  ! echo "$output" | grep -qF $'\xe2\x80\xa6'
}

# ---------------------------------------------------------------------------
# wait_ssh_drop gate (#261).
#
# bootc --apply queues the reboot via systemd-run (small delay) — without
# a wait_ssh_drop prelude, wait_ssh_back fires immediately on the still-up
# pre-reboot SSH connection and reports "SSH back ~0s", declaring success
# before the reboot has even started. The gate is DIAGNOSTIC: a timeout
# logs WARN but does NOT fail the run; the bootID-changed gate (when not
# skipped) remains the source of truth for "did the reboot happen".
# ---------------------------------------------------------------------------

@test "dry-run emits 'DRY-RUN wait_ssh_drop' line for the CP and each worker" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  # cluster.example.conf = 1 CP + 2 workers = 3 nodes total.
  drop_n="$(echo "$output" | grep -cE 'DRY-RUN wait_ssh_drop ' || true)"
  [ "$drop_n" -eq 3 ]
  echo "$output" | grep -q "DRY-RUN wait_ssh_drop"
  echo "$output" | grep -q "would poll up to 30s"
}

@test "wait_ssh_drop runs BEFORE wait_ssh_back on every node (dry-run)" {
  # The whole point of #261 is sequencing — drop must precede back, else
  # the back-gate false-successes on the pre-reboot connection.
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  # Find first occurrence of each on a worker.
  drop_line="$(echo "$output" | grep -nE 'DRY-RUN wait_ssh_drop ' | head -1 | cut -d: -f1)"
  back_line="$(echo "$output" | grep -nE 'waiting for SSH to come back' | head -1 | cut -d: -f1)"
  [ -n "$drop_line" ]
  [ -n "$back_line" ]
  [ "$drop_line" -lt "$back_line" ] || {
    echo "drop=$drop_line back=$back_line — drop must come first" >&2
    false
  }
}

@test "wait_ssh_drop comes AFTER bootc upgrade --apply (dry-run)" {
  # Inserted between the upgrade call and wait_ssh_back; assert the
  # bootc-upgrade log line precedes the drop poll.
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  bootc_line="$(echo "$output" | grep -nE 'ssh root@.* bootc upgrade --apply' | head -1 | cut -d: -f1)"
  drop_line="$(echo "$output" | grep -nE 'DRY-RUN wait_ssh_drop ' | head -1 | cut -d: -f1)"
  [ -n "$bootc_line" ]
  [ -n "$drop_line" ]
  [ "$bootc_line" -lt "$drop_line" ] || {
    echo "bootc=$bootc_line drop=$drop_line — bootc upgrade must come first" >&2
    false
  }
}

@test "SSH_DROP_TIMEOUT env override surfaces in the wait_ssh_drop log + startup banner" {
  SSH_DROP_TIMEOUT=15 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "would poll up to 15s"
  echo "$output" | grep -q "ssh-drop=15s"
}

@test "SSH_DROP_TIMEOUT default of 30 appears in the startup banner" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "ssh-drop=30s"
}

@test "SSH_DROP_TIMEOUT rejects bash-arithmetic injection payload" {
  SSH_DROP_TIMEOUT='a[0$(reboot)]' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "SSH_DROP_TIMEOUT"
}

@test "SSH_DROP_TIMEOUT=0 is rejected (strict positive — 0 would defeat the gate)" {
  SSH_DROP_TIMEOUT=0 run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "SSH_DROP_TIMEOUT"
}

@test "SSH_DROP_TIMEOUT rejects non-integer value" {
  SSH_DROP_TIMEOUT='5m' run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "SSH_DROP_TIMEOUT"
}

@test "wait_ssh_drop helper returns 1 on 'SSH never drops' simulation" {
  # Mock harness: extract the wait_ssh_drop helper from the script via awk
  # (the script's source-only mode short-circuits BEFORE helper definitions
  # so we can't just `source` it). Then redefine ssh to ALWAYS succeed
  # (simulating "node never rebooted") and confirm wait_ssh_drop exits
  # non-zero AND logs the WARN.
  HELPER="$BATS_TEST_TMPDIR/wait_ssh_drop.sh"
  awk '/^wait_ssh_drop\(\) \{/,/^\}$/' "$SCRIPT" >"$HELPER"
  # Sanity: helper extraction picked up something non-empty.
  [ -s "$HELPER" ]
  run bash -c '
    set -euo pipefail
    DRY_RUN=0
    SSH_OPTS=()
    SSH_DROP_TIMEOUT=2          # keep the test fast (2s max)
    log() { printf "[update-cluster] %s\n" "$*"; }
    # Stub ssh as always-up. true returns 0 → wait_ssh_drop keeps polling.
    ssh() { return 0; }
    source "'"$HELPER"'"
    if wait_ssh_drop 192.0.2.1 2; then
      echo "RC=0"
    else
      echo "RC=$?"
    fi
  '
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "RC=1"
  echo "$output" | grep -q "WARN: SSH on 192.0.2.1 still up after 2s"
}

@test "wait_ssh_drop helper returns 0 promptly when SSH drops" {
  # Inverse of the previous test: stub ssh to always fail (simulating the
  # reboot tearing down sshd). wait_ssh_drop must return 0 on the first
  # iteration AND log the "dropped after ~Xs" line.
  HELPER="$BATS_TEST_TMPDIR/wait_ssh_drop.sh"
  awk '/^wait_ssh_drop\(\) \{/,/^\}$/' "$SCRIPT" >"$HELPER"
  [ -s "$HELPER" ]
  run bash -c '
    set -euo pipefail
    DRY_RUN=0
    SSH_OPTS=()
    SSH_DROP_TIMEOUT=5
    log() { printf "[update-cluster] %s\n" "$*"; }
    # Stub ssh as always-down. Returns 255 (the ssh "could not connect" rc).
    ssh() { return 255; }
    source "'"$HELPER"'"
    if wait_ssh_drop 192.0.2.1 5; then
      echo "RC=0"
    else
      echo "RC=$?"
    fi
  '
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "RC=0"
  echo "$output" | grep -q "dropped after"
  # No WARN should fire on the success path.
  ! echo "$output" | grep -q "WARN: SSH on"
}

@test "wait_ssh_drop --help/banner documents SSH_DROP_TIMEOUT" {
  # The flag-summary log surfaces ssh-drop=<N>s (covered above), and the
  # in-file --help block (extracted via sed from the header comment) must
  # mention SSH_DROP_TIMEOUT for operator discoverability.
  run bash "$SCRIPT" --help
  [ "$status" -eq 0 ]
  echo "$output" | grep -q "SSH_DROP_TIMEOUT"
# #260 — k8s node name resolution + --node-name-override
#
# scripts/update-cluster.sh used to issue `kubectl drain $libvirt_domain`,
# silently failing against pre-PR-#255 clusters where k8s node names
# diverged from libvirt domain names. Now the script resolves
# libvirt-domain -> IP -> k8s-node-name before any kubectl call. The
# --node-name-override flag is the operator escape hatch when auto-
# resolution can't reach the apiserver or returns the wrong node.
#
# In --dry-run mode resolve_k8s_node_name short-circuits to the libvirt
# domain name (no apiserver to call); the resolver itself is exercised
# via a stubbed `cp_kubectl` in source-only mode further below.
# ---------------------------------------------------------------------------

@test "#260 --node-name-override appears in --help output" {
  run bash "$SCRIPT" --help
  [ "$status" -eq 0 ]
  echo "$output" | grep -q -- "--node-name-override"
  echo "$output" | grep -q "DOMAIN=NODE"
}

@test "#260 --node-name-override hbird-w1=hbird-w1-renamed redirects drain target" {
  # With the override in place, the dry-run drain line for hbird-w1's
  # libvirt domain MUST target the renamed k8s node, not the libvirt name.
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run \
      --node-name-override=hbird-w1=hbird-w1-renamed
  [ "$status" -eq 0 ]
  # Drain targets the renamed k8s node.
  echo "$output" | grep -qE "DRY-RUN cp_kubectl -- drain hbird-w1-renamed --ignore-daemonsets"
  # Uncordon targets the renamed k8s node.
  echo "$output" | grep -qE "DRY-RUN cp_kubectl -- uncordon hbird-w1-renamed$"
  # NEGATIVE: no drain or uncordon line should target the bare libvirt
  # domain hbird-w1 (otherwise we'd be back to the pre-#260 bug). We
  # anchor with a word boundary so hbird-w1-renamed doesn't match.
  ! echo "$output" | grep -qE "DRY-RUN cp_kubectl -- drain hbird-w1 "
  ! echo "$output" | grep -qE "DRY-RUN cp_kubectl -- uncordon hbird-w1$"
  # The startup banner surfaces the libvirt->k8s mapping when they diverge.
  echo "$output" | grep -q "resolved libvirt domain hbird-w1 -> k8s node hbird-w1-renamed"
}

@test "#260 --node-name-override supports comma-separated DOMAIN=NODE pairs" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run \
      --node-name-override=hbird-w1=k8s-w1,hbird-w2=k8s-w2
  [ "$status" -eq 0 ]
  echo "$output" | grep -qE "DRY-RUN cp_kubectl -- drain k8s-w1 --ignore-daemonsets"
  echo "$output" | grep -qE "DRY-RUN cp_kubectl -- drain k8s-w2 --ignore-daemonsets"
}

@test "#260 --node-name-override accepts repeated flags" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run \
      --node-name-override=hbird-w1=k8s-w1 \
      --node-name-override=hbird-w2=k8s-w2
  [ "$status" -eq 0 ]
  echo "$output" | grep -qE "DRY-RUN cp_kubectl -- drain k8s-w1 --ignore-daemonsets"
  echo "$output" | grep -qE "DRY-RUN cp_kubectl -- drain k8s-w2 --ignore-daemonsets"
}

@test "#260 --node-name-override rejects unknown DOMAIN" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run \
      --node-name-override=nonexistent=foo
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "nonexistent"
  echo "$output" | grep -q "does not match CP_NAME or any WORKER_NAMES"
}

@test "#260 --node-name-override rejects malformed value (missing =)" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run \
      --node-name-override=hbird-w1
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "DOMAIN=NODE"
}

@test "#260 --node-name-override rejects empty NODE side" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run \
      --node-name-override=hbird-w1=
  [ "$status" -ne 0 ]
  echo "$output" | grep -q "NODE cannot be empty"
}

@test "#260 --node-name-override rejects empty value entirely" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run \
      --node-name-override=
  [ "$status" -ne 0 ]
  echo "$output" | grep -q -- "--node-name-override="
}

@test "#260 bare --node-name-override (no =) exits non-zero with diagnostic" {
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run --node-name-override
  [ "$status" -ne 0 ]
  echo "$output" | grep -q -- "--node-name-override requires a value"
}

@test "#260 --node-name-override CP redirects bootID + Ready gates" {
  # Same divergence story for the CP path. PR #255 normally keeps the CP
  # libvirt/k8s names in sync, but we resolve defensively. With the
  # override we must see the renamed k8s name in the CP gate logs.
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run \
      --node-name-override=hbird-cp1=hbird-cp1-k8s
  [ "$status" -eq 0 ]
  # Capture/gate log lines now refer to the renamed CP k8s node.
  echo "$output" | grep -q "DRY-RUN would capture pre-reboot bootID for hbird-cp1-k8s"
  echo "$output" | grep -q "waiting for node hbird-cp1-k8s bootID to change"
  echo "$output" | grep -q "waiting for node hbird-cp1-k8s to report Ready"
  # The CP libvirt-name header (operator-readable) still shows the libvirt
  # domain so the abort-recovery hint is greppable.
  echo "$output" | grep -q "CP: hbird-cp1 .* k8s-node=hbird-cp1-k8s"
}

@test "#260 dry-run without override resolves k8s name == libvirt name (legacy compat)" {
  # In dry-run resolve_k8s_node_name returns the libvirt domain so existing
  # bats / integration assertions keep working. NO "resolved libvirt
  # domain -> k8s node" banner line should appear because there's no
  # divergence in this case.
  run env CONFIG="$CONF" bash "$SCRIPT" --dry-run
  [ "$status" -eq 0 ]
  ! echo "$output" | grep -q "resolved libvirt domain"
  # Drain still targets the libvirt name (which equals the k8s name here).
  echo "$output" | grep -q "DRY-RUN cp_kubectl -- drain hbird-w1 --ignore-daemonsets"
}

# ---------------------------------------------------------------------------
# resolve_k8s_node_name extracted via awk + stubbed cp_kubectl.
#
# The source-only mode (HBIRD_UPDATE_CLUSTER_SOURCE_ONLY=1) returns BEFORE
# the helper functions are defined, so we can't load resolve_k8s_node_name
# that way. Instead we extract the function definition block with awk —
# the same pattern tests/scripts/cp-name-alias.bats uses — and drive it
# with a stub cp_kubectl that emulates a real apiserver response.
# ---------------------------------------------------------------------------

# _extract_resolver_snippet — copy the resolve_k8s_node_name function body
# out of scripts/update-cluster.sh into a sourceable snippet. We anchor on
# the function header line and stop at the closing brace at column 0. If
# a future refactor renames the function, the resulting empty snippet
# makes every dependent test fail loudly.
_extract_resolver_snippet() {
  local out="$1"
  awk '
    /^resolve_k8s_node_name\(\) \{/ {capture=1}
    capture {print}
    capture && /^\}$/ {exit}
  ' "$SCRIPT" > "$out"
  [ -s "$out" ] || {
    echo "FATAL: failed to extract resolve_k8s_node_name from $SCRIPT" >&2
    return 1
  }
}

@test "#260 resolve_k8s_node_name returns the kubectl-reported name on match" {
  _extract_resolver_snippet "${BATS_TEST_TMPDIR}/resolver.snippet"
  driver="${BATS_TEST_TMPDIR}/resolve-match.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
DRY_RUN=0
declare -A NODE_NAME_OVERRIDE_MAP=()
log() { :; }
# Stub cp_kubectl: return what an apiserver would return when an address
# matches. We don't care about the exact request shape — just that the
# helper passes the stdout through.
cp_kubectl() { echo "humbird-worker-748f4cf5"; }
# shellcheck disable=SC1090
source "${BATS_TEST_TMPDIR}/resolver.snippet"
out="\$(resolve_k8s_node_name hbird-w1 192.168.122.11)"
echo "RESOLVED=\$out"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ] || { echo "$output" >&2; false; }
  echo "$output" | grep -q "RESOLVED=humbird-worker-748f4cf5"
}

@test "#260 resolve_k8s_node_name uses override before kubectl" {
  _extract_resolver_snippet "${BATS_TEST_TMPDIR}/resolver.snippet"
  driver="${BATS_TEST_TMPDIR}/resolve-override.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
DRY_RUN=0
declare -A NODE_NAME_OVERRIDE_MAP=([hbird-w1]=hbird-w1-renamed)
log() { :; }
# cp_kubectl must NOT be reached on the override path.
cp_kubectl() { echo "should-not-be-used"; }
# shellcheck disable=SC1090
source "${BATS_TEST_TMPDIR}/resolver.snippet"
out="\$(resolve_k8s_node_name hbird-w1 192.168.122.11)"
echo "RESOLVED=\$out"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ] || { echo "$output" >&2; false; }
  echo "$output" | grep -q "RESOLVED=hbird-w1-renamed"
}

@test "#260 resolve_k8s_node_name emits diagnostic + non-zero rc when kubectl returns empty" {
  _extract_resolver_snippet "${BATS_TEST_TMPDIR}/resolver.snippet"
  driver="${BATS_TEST_TMPDIR}/resolve-empty.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
DRY_RUN=0
declare -A NODE_NAME_OVERRIDE_MAP=()
# Capture the log-line argument so the test can grep for it.
log() { printf 'LOG: %s\n' "\$*" >&2; }
# Empty apiserver response = no node has that IP.
cp_kubectl() { printf ''; }
# shellcheck disable=SC1090
source "${BATS_TEST_TMPDIR}/resolver.snippet"
set +e
out="\$(resolve_k8s_node_name hbird-w1 192.168.122.11)"
rc=\$?
set -e
echo "RC=\$rc"
echo "OUT=[\${out}]"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ] || { echo "$output" >&2; false; }
  echo "$output" | grep -q "RC=1"
  echo "$output" | grep -q "OUT=\[\]"
  # Diagnostic surfaces both the libvirt domain AND the IP so an operator
  # can find the failing node at a glance.
  echo "$output" | grep -q "could not resolve k8s node for libvirt domain hbird-w1"
  echo "$output" | grep -q "ip=192.168.122.11"
  echo "$output" | grep -q -- "--node-name-override"
}

@test "#260 resolve_k8s_node_name trims whitespace from kubectl jsonpath output" {
  # kubectl jsonpath has been observed to append a trailing newline on
  # some apiserver versions; the resolver must strip it so callers don't
  # compose `kubectl drain $name\n`.
  _extract_resolver_snippet "${BATS_TEST_TMPDIR}/resolver.snippet"
  driver="${BATS_TEST_TMPDIR}/resolve-trim.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
DRY_RUN=0
declare -A NODE_NAME_OVERRIDE_MAP=()
log() { :; }
# Trailing newline + leading whitespace.
cp_kubectl() { printf '  humbird-worker-abc\n'; }
# shellcheck disable=SC1090
source "${BATS_TEST_TMPDIR}/resolver.snippet"
out="\$(resolve_k8s_node_name hbird-w1 192.168.122.11)"
printf 'RESOLVED=[%s]\n' "\$out"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ] || { echo "$output" >&2; false; }
  echo "$output" | grep -q "RESOLVED=\\[humbird-worker-abc\\]"
}
