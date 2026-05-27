#!/usr/bin/env bats
#
# Unit tests for scripts/switch-to-ghcr.sh — pinning the C3 SSH-wrap shim
# integration added by #271 F1.
#
# Before the fix:
#   `make switch-to-ghcr` from a workstation called `virsh -c qemu:///system`
#   locally, which fails (no libvirt) and returns either "no running
#   hummingbird-* VMs found" (false negative) or `virsh: command not found`.
#
# After the fix:
#   The script sources scripts/lib/ssh-wrap.sh and invokes
#   hbird_ssh_wrap_maybe_reexec near the top, mirroring the four C3-wrapped
#   lifecycle scripts (deploy-cluster.sh, destroy-cluster.sh,
#   update-cluster.sh, spawn-workers.sh). When KVM_HOST is set, the script
#   re-execs on the KVM host where libvirt actually lives.
#
# This bats file pins:
#   1. The source-only guard (HBIRD_SWITCH_TO_GHCR_SOURCE_ONLY=1) is
#      honored — so this very test can `source` the script without root
#      / libvirt / SSH side effects.
#   2. The shim is invoked AFTER the source-only guard (so test
#      introspection short-circuits before re-exec logic) and BEFORE any
#      `virsh` call (so the workstation never executes a local virsh).
#
# Run via:
#   bats tests/scripts/switch-to-ghcr.bats

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/switch-to-ghcr.sh"
  [ -r "$SCRIPT" ] || { echo "FATAL: $SCRIPT not readable" >&2; return 1; }
}

# ---------------------------------------------------------------------------
# Source-only guard honored
# ---------------------------------------------------------------------------

@test "switch-to-ghcr: HBIRD_SWITCH_TO_GHCR_SOURCE_ONLY=1 returns early (no virsh, no SSH)" {
  # Source the script with the guard set. The script should `return 0`
  # from the source-only check well before any libvirt or SSH code runs.
  # We assert success by checking the source itself doesn't error.
  run bash -c "set -euo pipefail; export HBIRD_SWITCH_TO_GHCR_SOURCE_ONLY=1; source '$SCRIPT'; echo SOURCE_OK"
  [ "$status" -eq 0 ]
  [[ "$output" == *"SOURCE_OK"* ]]
  # Must NOT have hit the BOOTC_SWITCH_TO_GHCR=0 escape-hatch log line —
  # the source-only guard is checked FIRST.
  [[ "$output" != *"BOOTC_SWITCH_TO_GHCR=0"* ]]
}

# ---------------------------------------------------------------------------
# Shim is sourced + invoked
# ---------------------------------------------------------------------------

@test "switch-to-ghcr: sources lib/ssh-wrap.sh (#271 F1)" {
  grep -q 'source "${SCRIPT_DIR}/lib/ssh-wrap.sh"' "$SCRIPT"
}

@test "switch-to-ghcr: invokes hbird_ssh_wrap_maybe_reexec (#271 F1)" {
  grep -q 'hbird_ssh_wrap_maybe_reexec "$0" "$@"' "$SCRIPT"
}

# ---------------------------------------------------------------------------
# Shim placement vs the local-virsh calls
# ---------------------------------------------------------------------------
#
# The whole point of F1 is that workstation invocations never touch local
# virsh. Pin the shim's line number is BEFORE the first `virsh` call in
# the script body. A future refactor that moves the shim down past virsh
# (or drops it entirely) would re-introduce the F1 bug.

@test "switch-to-ghcr: shim invocation appears before any virsh call (#271 F1)" {
  local shim_line virsh_line
  shim_line=$(grep -n 'hbird_ssh_wrap_maybe_reexec' "$SCRIPT" | head -1 | cut -d: -f1)
  virsh_line=$(grep -n '^[^#]*virsh ' "$SCRIPT" | head -1 | cut -d: -f1)
  [ -n "$shim_line" ]
  [ -n "$virsh_line" ]
  [ "$shim_line" -lt "$virsh_line" ]
}

# ---------------------------------------------------------------------------
# Source-only guard placement
# ---------------------------------------------------------------------------
#
# The source-only guard must fire BEFORE the shim — otherwise bats can't
# introspect the script without the shim kicking in and trying to ssh
# somewhere. Mirrors deploy-cluster.sh's ordering.

@test "switch-to-ghcr: source-only guard appears before shim invocation" {
  local guard_line shim_line
  guard_line=$(grep -n 'HBIRD_SWITCH_TO_GHCR_SOURCE_ONLY' "$SCRIPT" | head -1 | cut -d: -f1)
  shim_line=$(grep -n 'hbird_ssh_wrap_maybe_reexec' "$SCRIPT" | head -1 | cut -d: -f1)
  [ -n "$guard_line" ]
  [ -n "$shim_line" ]
  [ "$guard_line" -lt "$shim_line" ]
}
