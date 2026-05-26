#!/usr/bin/env bats
#
# Unit tests for scripts/deploy-cluster.sh config parsing — specifically
# the WORKER_NAMES resolution block (PR #219 round-2 H1).
#
# README's Migration table promises that `WORKER_NAMES=()` in
# cluster.local.conf yields a CP-only deploy. The original block
# treated empty arrays the same as "unset" and silently filled in
# two default workers, contradicting the README. These tests pin the
# three-state behavior:
#
#   1. unset             — defaults to (${CP_NAME}-w1 ${CP_NAME}-w2)  [legacy]
#   2. WORKER_NAMES=()   — honored as explicit CP-only intent
#   3. WORKER_NAMES=(…)  — used verbatim
#
# We can't invoke deploy-cluster.sh end-to-end here (it asserts EUID==0
# and runs virt-install). Instead, we extract just the resolver block
# and source it from a harness that supplies the inputs. Keeping the
# tested code as a literal extract (not a paraphrase) makes the test
# meaningful: any future edit to the block has to be mirrored here.
#
# Run via:  bats tests/scripts/deploy-cluster.bats

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/deploy-cluster.sh"
  HARNESS="${BATS_TEST_TMPDIR}/resolve.sh"
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  # Extract the resolver block from deploy-cluster.sh by line markers so
  # the harness stays in lockstep with the script. The block we want
  # spans from the comment "Default WORKER_NAMES" through the matching
  # `fi`. awk grabs everything between the start marker and the next
  # `^fi$` at column 0.
  awk '
    /^# Default WORKER_NAMES/ {capture=1}
    capture {print}
    capture && /^fi$/ {exit}
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/resolver.snippet"

  # Sanity-check the extraction found something — if a refactor moved
  # the comment marker, fail loudly rather than silently passing an
  # empty resolver.
  [ -s "${BATS_TEST_TMPDIR}/resolver.snippet" ] || {
    echo "FATAL: failed to extract WORKER_NAMES resolver block from ${SCRIPT}" >&2
    return 1
  }

  cat > "$HARNESS" <<'HARNESS_EOF'
#!/usr/bin/env bash
set -euo pipefail
# Minimal log() shim — the resolver block calls log(), and we don't
# want to drag in lib/build-common.sh just to print.
log() { printf 'log: %s\n' "$*"; }
: "${CP_NAME:=hbird-cp1}"
# shellcheck disable=SC1091
source "$1"
printf 'count=%d\n' "${#WORKER_NAMES[@]}"
printf 'names=%s\n' "${WORKER_NAMES[*]:-}"
HARNESS_EOF
  chmod +x "$HARNESS"
}

# ---------------------------------------------------------------------------
# WORKER_NAMES resolution — three-state behavior (H1)
# ---------------------------------------------------------------------------

@test "deploy-cluster: WORKER_NAMES unset -> legacy 2-worker default" {
  # Don't pre-set WORKER_NAMES; the resolver should fill it in.
  run env -u WORKER_NAMES CP_NAME=hbird-cp1 \
    bash "$HARNESS" "${BATS_TEST_TMPDIR}/resolver.snippet"
  [ "$status" -eq 0 ]
  [[ "$output" == *"count=2"* ]]
  [[ "$output" == *"names=hbird-cp1-w1 hbird-cp1-w2"* ]]
  # Operator-visible log line still emitted on the "unset" path.
  [[ "$output" == *"WORKER_NAMES not set"* ]]
}

@test "deploy-cluster: WORKER_NAMES=() -> CP-only (zero workers)" {
  # Simulate cluster.local.conf doing `WORKER_NAMES=()`. Arrays don't
  # cross process boundaries via env, so write a self-contained driver
  # that sets the array then sources the resolver snippet directly.
  local driver="${BATS_TEST_TMPDIR}/driver-cponly.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
log() { printf 'log: %s\n' "\$*"; }
CP_NAME=hbird-cp1
WORKER_NAMES=()
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/resolver.snippet"
printf 'count=%d\n' "\${#WORKER_NAMES[@]}"
printf 'names=%s\n' "\${WORKER_NAMES[*]:-}"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"count=0"* ]]
  # CP-only log line should fire — operator needs to see this.
  [[ "$output" == *"CP-only deploy"* ]]
  # Must NOT have fallen back to the 2-worker default.
  [[ "$output" != *"WORKER_NAMES not set"* ]]
}

@test "deploy-cluster: WORKER_NAMES=(custom names) -> used verbatim" {
  local driver="${BATS_TEST_TMPDIR}/driver-custom.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
log() { printf 'log: %s\n' "\$*"; }
CP_NAME=hbird-cp1
WORKER_NAMES=(hbird-w1 hbird-w2 hbird-w3)
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/resolver.snippet"
printf 'count=%d\n' "\${#WORKER_NAMES[@]}"
printf 'names=%s\n' "\${WORKER_NAMES[*]}"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"count=3"* ]]
  [[ "$output" == *"names=hbird-w1 hbird-w2 hbird-w3"* ]]
  # Neither default-fill nor CP-only branch should fire.
  [[ "$output" != *"WORKER_NAMES not set"* ]]
  [[ "$output" != *"CP-only deploy"* ]]
}
