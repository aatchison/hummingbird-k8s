#!/usr/bin/env bats
#
# tests/scripts/clean-vms.bats — Unit tests for scripts/clean-vms.sh.
#
# Covers:
#
#   1. The script sources scripts/lib/ssh-wrap.sh and invokes the shim
#      via hbird_ssh_wrap_maybe_reexec, matching the pattern used by the
#      other C3-wrapped scripts (deploy-cluster, destroy-cluster,
#      update-cluster, spawn-workers). (#271 F5.)
#
#   2. KVM_HOST set + we're not the KVM host -> the shim re-execs over
#      SSH (we assert via HBIRD_SSH_WRAP_DRY_RUN=1 sentinel).
#
#   3. Source-only guard works: HBIRD_CLEAN_VMS_SOURCE_ONLY=1 returns
#      from `source` before any libvirt / sudo code fires, so tests can
#      inspect helpers without running them.
#
#   4. POOL_DIR / POOL_NAME defaults match the documented defaults
#      (drift fence against silently changing the sweep targets).
#
# Tests that need to actually exercise the virsh loop + rm sweep would
# require a libvirt mock; we leave that to integration tests on the
# self-hosted KVM runner. Here we pin the contract.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/clean-vms.sh"
  [ -r "$SCRIPT" ] || { echo "FATAL: $SCRIPT not readable" >&2; return 1; }
  [ -x "$SCRIPT" ] || { echo "FATAL: $SCRIPT not executable" >&2; return 1; }

  # Clean slate for env vars the shim consults.
  unset KVM_HOST HBIRD_REMOTE_REEXEC HBIRD_SSH_WRAP_DRY_RUN \
        HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT HBIRD_REMOTE_REPO \
        CONFIG POOL_DIR POOL_NAME \
        HBIRD_CLEAN_VMS_SOURCE_ONLY HBIRD_SSH_WRAP_DRY_RUN_SCP

  LOCAL_HOST="$(hostname -s 2>/dev/null || hostname)"
}

# ---------------------------------------------------------------------------
# 1. Wiring: script sources ssh-wrap.sh and calls the shim entrypoint.
# ---------------------------------------------------------------------------

@test "clean-vms: sources scripts/lib/ssh-wrap.sh" {
  run grep -E '^\s*source ".*lib/ssh-wrap\.sh"' "$SCRIPT"
  [ "$status" -eq 0 ]
}

@test "clean-vms: invokes hbird_ssh_wrap_maybe_reexec" {
  run grep -E 'hbird_ssh_wrap_maybe_reexec "\$0" "\$@"' "$SCRIPT"
  [ "$status" -eq 0 ]
}

@test "clean-vms: source-only guard present" {
  # Drift fence: every C3-wrapped script needs a source-only sentinel so
  # bats can inspect helpers without triggering the shim + libvirt body.
  run grep -E 'HBIRD_CLEAN_VMS_SOURCE_ONLY' "$SCRIPT"
  [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# 2. KVM_HOST behavior: when set + we're remote, the shim re-execs.
# ---------------------------------------------------------------------------

@test "clean-vms: KVM_HOST unset + source-only -> no shim fire, returns cleanly" {
  # With KVM_HOST unset the shim returns 0 and the script continues to
  # the EUID/sudo branch. The source-only guard short-circuits BEFORE
  # the shim so we never reach virsh / sudo. We just need the source to
  # return 0.
  HBIRD_CLEAN_VMS_SOURCE_ONLY=1 run bash -c "source '$SCRIPT'"
  [ "$status" -eq 0 ]
}

@test "clean-vms: KVM_HOST set + not on that host -> shim prints SSH_WRAP_CMD" {
  # Use a synthetic remote name guaranteed not to match $(hostname -s).
  # Dry-run + preflight-skip so the shim prints the command and exits.
  KVM_HOST="not-${LOCAL_HOST}-remote-xyz" \
  HBIRD_SSH_WRAP_DRY_RUN=1 \
  HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 \
    run bash "$SCRIPT"
  [ "$status" -eq 0 ]
  [[ "$output" == *"SSH_WRAP_CMD:"* ]]
  [[ "$output" == *"clean-vms.sh"* ]]
}

@test "clean-vms: KVM_HOST == local host -> no re-exec (operator on the KVM host)" {
  # When KVM_HOST resolves to ourselves, the shim is a no-op. We can't
  # let execution fall through to the sudo + virsh body, so use the
  # source-only guard to short-circuit before the shim.
  HBIRD_CLEAN_VMS_SOURCE_ONLY=1 \
  KVM_HOST="$LOCAL_HOST" \
    run bash -c "source '$SCRIPT'"
  [ "$status" -eq 0 ]
  # No SSH would be invoked from a sourced clean-vms.sh in source-only
  # mode; assert the negative for paranoia.
  [[ "$output" != *"SSH_WRAP_CMD:"* ]]
  [[ "$output" != *"re-execing"* ]]
}

# ---------------------------------------------------------------------------
# 3. Defaults: POOL_DIR / POOL_NAME match documented values.
# ---------------------------------------------------------------------------

@test "clean-vms: POOL_DIR default is /var/lib/libvirt/images" {
  run grep -E '^\s*:\s*"\$\{POOL_DIR:=/var/lib/libvirt/images\}"' "$SCRIPT"
  [ "$status" -eq 0 ]
}

@test "clean-vms: POOL_NAME default is 'default'" {
  run grep -E '^\s*:\s*"\$\{POOL_NAME:=default\}"' "$SCRIPT"
  [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# 4. Sweep targets: drift fence against silently changing what gets rm'd.
# ---------------------------------------------------------------------------

@test "clean-vms: sweeps hummingbird-*.qcow2 under POOL_DIR" {
  run grep -E '"\$POOL_DIR"/hummingbird-\*\.qcow2' "$SCRIPT"
  [ "$status" -eq 0 ]
}

@test "clean-vms: sweeps *-seed.iso under POOL_DIR (#221)" {
  run grep -E '"\$POOL_DIR"/\*-seed\.iso' "$SCRIPT"
  [ "$status" -eq 0 ]
}

@test "clean-vms: sweeps *-cloud-init.iso under POOL_DIR (alternate seed-iso name)" {
  run grep -E '"\$POOL_DIR"/\*-cloud-init\.iso' "$SCRIPT"
  [ "$status" -eq 0 ]
}

@test "clean-vms: calls virsh pool-refresh to drop rm'd volumes from catalog" {
  run grep -E 'virsh .* pool-refresh "\$POOL_NAME"' "$SCRIPT"
  [ "$status" -eq 0 ]
}
