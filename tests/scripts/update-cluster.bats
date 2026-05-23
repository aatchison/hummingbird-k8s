#!/usr/bin/env bats
#
# Unit tests for scripts/update-cluster.sh (issue #189).
#
# These tests cover flag parsing (mutual-exclusion, unknown-arg rejection,
# --help formatting) and --dry-run sequencing. The dry-run assertions
# deliberately codify the patterns that integration-update-cluster.yml's
# `assert-dry-run-sequence` command keys on — so a refactor that subtly
# changes log lines is caught at pr-validate time, not only on the
# self-hosted runner.
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
  stop_n="$(echo "$output" | grep -cE 'DRY-RUN ssh root@.* systemctl stop bootc-fetch-apply-updates.timer' || true)"
  start_n="$(echo "$output" | grep -cE 'DRY-RUN ssh root@.* systemctl start bootc-fetch-apply-updates.timer' || true)"
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
