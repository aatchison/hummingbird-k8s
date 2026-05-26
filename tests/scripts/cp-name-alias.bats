#!/usr/bin/env bats
#
# Unit tests for the CP_NAME / legacy-alias resolution in
# scripts/spawn-workers.sh and scripts/kubectl-k8s.sh (PR #219 round-2 M5).
#
# Both scripts use the same pattern:
#
#   : "${CP_NAME:=${LEGACY:-hummingbird-k8s}}"
#
# where LEGACY is `CP_VM_NAME` (spawn-workers) or `VM_NAME` (kubectl-k8s).
# This file pins the four-state truth table:
#
#   1. neither set                  -> default 'hummingbird-k8s'
#   2. only legacy set              -> resolves to legacy value + warning
#   3. only CP_NAME set             -> resolves to CP_NAME, no warning
#   4. both set                     -> CP_NAME wins (alias never overrides)
#
# We sidestep both scripts' downstream behavior (virsh/ssh/virt-install)
# by aborting before any of that fires. For spawn-workers.sh we set
# COUNT=0 and an unreadable POOL_DIR so the script exits at the
# template-readability check (which prints CP_NAME first). For
# kubectl-k8s.sh we set an unreachable KVM_HOST + already-listening
# LOCAL_PORT mock so the script falls through to the kubeconfig check
# (which prints CP_NAME on the error path). Either way we just grep
# the captured stderr / stdout for CP_NAME and the deprecation warning.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  # Build a tiny driver script that exercises ONLY the alias-resolution
  # block from each target script. We extract it verbatim with awk so a
  # future refactor that moves the lines is caught by the test failing
  # to find them.
  #
  # The block we want runs from the deprecation-warning comment through
  # the `: "${CP_NAME:=...}"` line. We capture both with one awk pass
  # per script.
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
  _extract_block "${REPO_ROOT}/scripts/kubectl-k8s.sh" \
                 "${BATS_TEST_TMPDIR}/kubectl-resolver.snippet"
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

# ---------------------------------------------------------------------------
# kubectl-k8s.sh — VM_NAME alias
# ---------------------------------------------------------------------------

@test "kubectl-k8s: neither CP_NAME nor VM_NAME set -> default 'hummingbird-k8s'" {
  run _run_resolver "${BATS_TEST_TMPDIR}/kubectl-resolver.snippet" ""
  [ "$status" -eq 0 ]
  [[ "$output" == *"CP_NAME=hummingbird-k8s"* ]]
  [[ "$output" != *"deprecated"* ]]
}

@test "kubectl-k8s: only VM_NAME set -> resolves to that value + warning" {
  run _run_resolver "${BATS_TEST_TMPDIR}/kubectl-resolver.snippet" "VM_NAME=legacy-vm"
  [ "$status" -eq 0 ]
  [[ "$output" == *"CP_NAME=legacy-vm"* ]]
  [[ "$output" == *"VM_NAME is deprecated"* ]]
}

@test "kubectl-k8s: only CP_NAME set -> resolves, no warning" {
  run _run_resolver "${BATS_TEST_TMPDIR}/kubectl-resolver.snippet" "CP_NAME=hbird-cp1"
  [ "$status" -eq 0 ]
  [[ "$output" == *"CP_NAME=hbird-cp1"* ]]
  [[ "$output" != *"deprecated"* ]]
}

@test "kubectl-k8s: both set -> CP_NAME wins, no warning" {
  run _run_resolver "${BATS_TEST_TMPDIR}/kubectl-resolver.snippet" \
    "CP_NAME=hbird-cp1
VM_NAME=legacy-vm"
  [ "$status" -eq 0 ]
  [[ "$output" == *"CP_NAME=hbird-cp1"* ]]
  [[ "$output" != *"deprecated"* ]]
}
