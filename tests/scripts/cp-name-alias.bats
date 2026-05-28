#!/usr/bin/env bats
#
# Unit tests for the CP_NAME / legacy-alias resolution in
# scripts/spawn-workers.sh (PR #219 round-2 M5).
#
# Originally this file also covered scripts/kubectl-k8s.sh, but that
# script was removed in the v0.1.0 partial bash->Rust cutover (#353).
# The Rust twin `hbird kubectl` does not carry the legacy VM_NAME alias
# (operators on the new path use CP_NAME only). The spawn-workers half
# is retained because spawn-workers.sh is still bash (deferred to v0.2.0
# pending #289 destructive Rust impl).
#
# The script uses the pattern:
#
#   : "${CP_NAME:=${CP_VM_NAME:-hummingbird-k8s}}"
#
# This file pins the four-state truth table:
#
#   1. neither set                  -> default 'hummingbird-k8s'
#   2. only CP_VM_NAME set          -> resolves to legacy value + warning
#   3. only CP_NAME set             -> resolves to CP_NAME, no warning
#   4. both set                     -> CP_NAME wins (alias never overrides)
#
# We sidestep spawn-workers.sh's downstream behavior (virsh/virt-install)
# by setting COUNT=0 and an unreadable POOL_DIR so the script exits at
# the template-readability check (which prints CP_NAME first). We just
# grep the captured stderr / stdout for CP_NAME and the deprecation
# warning.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  # Build a tiny driver script that exercises ONLY the alias-resolution
  # block from spawn-workers.sh. We extract it verbatim with awk so a
  # future refactor that moves the lines is caught by the test failing
  # to find them.
  #
  # The block we want runs from the deprecation-warning comment through
  # the `: "${CP_NAME:=...}"` line.
  _extract_block() {
    local src="$1" out="$2"
    awk '
      /^# Operator-visible deprecation signal/ {capture=1}
      capture {print}
      capture && /^: "\$\{CP_NAME:=/ {exit}
    ' "$src" > "$out"
    [ -s "$out" ] || {
      echo "FATAL: failed to extract CP_NAME resolver block from $src" >&2
      return 1
    }
  }

  _extract_block "${REPO_ROOT}/scripts/spawn-workers.sh" \
                 "${BATS_TEST_TMPDIR}/spawn-resolver.snippet"
}

# Helper: run a driver that pre-sets env vars, sources the resolver
# snippet, then prints the resolved CP_NAME to stdout. The driver
# itself injects the var sets so they can't leak between cases.
_run_resolver() {
  local snippet="$1" preamble="$2"
  local driver="${BATS_TEST_TMPDIR}/driver-$$.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
# Mimic the script's own argv[0] so the deprecation warning's prefix
# (\${0##*/}) is meaningful in test output.
${preamble}
# shellcheck disable=SC1091
source "${snippet}"
printf 'CP_NAME=%s\n' "\${CP_NAME}"
EOF
  chmod +x "$driver"
  # Run with a recognizable argv[0] so the warning is easy to spot.
  bash "$driver"
}

# ---------------------------------------------------------------------------
# spawn-workers.sh — CP_VM_NAME alias
# ---------------------------------------------------------------------------

@test "spawn-workers: neither CP_NAME nor CP_VM_NAME set -> default 'hummingbird-k8s'" {
  run _run_resolver "${BATS_TEST_TMPDIR}/spawn-resolver.snippet" ""
  [ "$status" -eq 0 ]
  [[ "$output" == *"CP_NAME=hummingbird-k8s"* ]]
  [[ "$output" != *"deprecated"* ]]
}

@test "spawn-workers: only CP_VM_NAME set -> resolves to that value + warning" {
  run _run_resolver "${BATS_TEST_TMPDIR}/spawn-resolver.snippet" "CP_VM_NAME=legacy-cp"
  [ "$status" -eq 0 ]
  [[ "$output" == *"CP_NAME=legacy-cp"* ]]
  [[ "$output" == *"CP_VM_NAME is deprecated"* ]]
}

@test "spawn-workers: only CP_NAME set -> resolves, no warning" {
  run _run_resolver "${BATS_TEST_TMPDIR}/spawn-resolver.snippet" "CP_NAME=hbird-cp1"
  [ "$status" -eq 0 ]
  [[ "$output" == *"CP_NAME=hbird-cp1"* ]]
  [[ "$output" != *"deprecated"* ]]
}

@test "spawn-workers: both set -> CP_NAME wins, no warning" {
  run _run_resolver "${BATS_TEST_TMPDIR}/spawn-resolver.snippet" \
    "CP_NAME=hbird-cp1
CP_VM_NAME=legacy-cp"
  [ "$status" -eq 0 ]
  [[ "$output" == *"CP_NAME=hbird-cp1"* ]]
  # Operator clearly knows the new name; warning would be noise.
  [[ "$output" != *"deprecated"* ]]
}

# kubectl-k8s.sh tests removed with PR #353 partial cutover.
# scripts/kubectl-k8s.sh deleted; Rust twin `hbird kubectl` does not
# carry the legacy VM_NAME alias.
