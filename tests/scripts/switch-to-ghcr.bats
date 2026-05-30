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

# ---------------------------------------------------------------------------
# FORCE_REBUILD opt-out in single-VM mode (#375)
# ---------------------------------------------------------------------------
#
# spawn-workers.sh / deploy-cluster.sh call this script in single-VM mode
# immediately after a fresh deploy. When the operator rebuilt the image
# locally (FORCE_REBUILD=1) they are boot-testing local Containerfile
# changes — flipping the just-installed VM to the GHCR-published image would
# track a possibly-stale remote and mask exactly what they are testing.
# Single-VM mode therefore skips the switch unless FORCE_SWITCH=1 opts back
# in, and warns loudly. all-VMs mode (a deliberate `make switch-to-ghcr`)
# is NOT gated on FORCE_REBUILD.

@test "switch-to-ghcr: single-VM mode skips + warns when FORCE_REBUILD=1 (no FORCE_SWITCH) (#375)" {
  # The guard exits 0 BEFORE any virsh/ssh call, so this runs fast with no
  # libvirt. KVM_HOST/BOOTC_SWITCH_TO_GHCR/FORCE_SWITCH cleared so only the
  # FORCE_REBUILD path is exercised.
  run env -u KVM_HOST -u BOOTC_SWITCH_TO_GHCR -u FORCE_SWITCH -u HBIRD_SWITCH_TO_GHCR_SOURCE_ONLY \
      FORCE_REBUILD=1 bash "$SCRIPT" hummingbird-k8s-worker-1 \
      ghcr.io/aatchison/hummingbird-k8s-worker:latest
  [ "$status" -eq 0 ]
  [[ "$output" == *"FORCE_REBUILD=1"* ]]
  [[ "$output" == *"#375"* ]]
  [[ "$output" == *"FORCE_SWITCH=1"* ]]
  # Must NOT have proceeded into switch_one (whose first log resolves the IP).
  [[ "$output" != *"resolving IP"* ]]
}

@test "switch-to-ghcr: FORCE_REBUILD guard lives in single-VM mode, before switch_one (#375)" {
  local single_line guard_line switchcall_line allvms_line
  single_line=$(grep -n '^if \[\[ \$# -ge 1 \]\]; then' "$SCRIPT" | head -1 | cut -d: -f1)
  guard_line=$(grep -n 'FORCE_REBUILD' "$SCRIPT" | grep -v '^[0-9]*:#' | head -1 | cut -d: -f1)
  switchcall_line=$(grep -n 'switch_one "\$vm_name"' "$SCRIPT" | head -1 | cut -d: -f1)
  allvms_line=$(grep -n 'all-VMs mode' "$SCRIPT" | tail -1 | cut -d: -f1)
  [ -n "$single_line" ]
  [ -n "$guard_line" ]
  [ -n "$switchcall_line" ]
  [ -n "$allvms_line" ]
  # Guard is inside the single-VM block...
  [ "$single_line" -lt "$guard_line" ]
  # ...fires before the switch happens...
  [ "$guard_line" -lt "$switchcall_line" ]
  # ...and is NOT in the all-VMs section (deliberate operator action).
  [ "$guard_line" -lt "$allvms_line" ]
}

@test "switch-to-ghcr: guard condition requires FORCE_SWITCH!=1 to skip (FORCE_SWITCH=1 opts back in) (#375)" {
  # Pin the exact boolean: only skips when FORCE_REBUILD=1 AND FORCE_SWITCH!=1.
  grep -Eq '\$\{FORCE_REBUILD:-\}" = "1" && "\$\{FORCE_SWITCH:-\}" != "1"' "$SCRIPT"
}
