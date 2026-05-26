#!/usr/bin/env bats
#
# tests/scripts/ssh-wrap.bats — C3 (#232) SSH-wrap shim contract.
#
# The four libvirt-touching scripts (deploy-cluster, destroy-cluster,
# update-cluster, spawn-workers) share scripts/lib/ssh-wrap.sh, which
# re-execs them on the KVM host via SSH when KVM_HOST is set and we're
# not already on that host. The client never needs sudo or libvirt;
# only ssh + the operator's SSH key.
#
# What we pin here:
#
# 1. The four no-re-exec paths (regression guards):
#    a. KVM_HOST unset                                     -> local
#    b. KVM_HOST=$(hostname -s) (we ARE the KVM host)      -> local
#    c. HBIRD_REMOTE_REEXEC=1 sentinel set                 -> local
#    d. KVM_HOST=<short>.<domain> matching hostname short  -> local
#
# 2. The env-var allowlist (the critical contract). The script must
#    forward EXACTLY the vars in HBIRD_SSH_WRAP_ALLOWED_ENV and nothing
#    more — opaque forwarding is a footgun, so any future widening MUST
#    update this test in lockstep.
#
# 3. The remote-checkout exec model (round-2 architectural pivot): the
#    shim execs `bash scripts/<name>.sh` from disk on the remote at
#    $HBIRD_REMOTE_REPO, NOT via `bash -s` stdin streaming.
#
# We use a sentinel + dry-run to avoid spawning real ssh:
#   HBIRD_SSH_WRAP_DRY_RUN=1 makes the shim print the would-be SSH
#   command (prefixed `SSH_WRAP_CMD: `) and exit 0.
#
# Run via:
#   bats tests/scripts/ssh-wrap.bats
# OR:
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/scripts/ssh-wrap.bats

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  LIB="${REPO_ROOT}/scripts/lib/ssh-wrap.sh"
  [ -r "$LIB" ] || { echo "FATAL: $LIB not readable" >&2; return 1; }

  # Each test starts from a clean slate — no inherited KVM_HOST / sentinel.
  unset KVM_HOST HBIRD_REMOTE_REEXEC HBIRD_SSH_WRAP_DRY_RUN \
        HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT HBIRD_REMOTE_REPO
  unset CONFIG FLAGS AUTO_UPDATE_CP SWITCH_TO_GHCR \
        BOOTC_UPDATE_SCHEDULE BOOTC_UPDATE_REPO_K8S BOOTC_UPDATE_REPO_WORKER \
        IMAGE_SOURCE GHCR_TAG DRY_RUN SKIP_DRAIN WORKERS_ONLY NODE \
        START_FROM PARALLEL READY_TIMEOUT DRAIN_TIMEOUT APISERVER_TIMEOUT \
        SSH_TIMEOUT INTER_NODE_SLEEP DAEMONSET_TIMEOUT CP_NAME \
        WORKER_NAMES POOL_DIR \
        VM_USER STORAGE_DRIVER PODMAN_ROOT PODMAN_RUNROOT APISERVER_EXTRA_SANS \
        HBIRD_AUTOLOAD_CONFIG_LOCAL

  LOCAL_HOST="$(hostname -s 2>/dev/null || hostname)"
}

# Helper: source the lib in a subshell + invoke the shim. Returns the
# combined stdout/stderr; status is the shim's exit status (or 0 if the
# guard short-circuited).
invoke_shim() {
  # shellcheck disable=SC1090
  ( source "$LIB"; hbird_ssh_wrap_maybe_reexec "/fake/path/scripts/foo.sh" "$@" )
}

# ---------------------------------------------------------------------------
# No-re-exec paths
# ---------------------------------------------------------------------------

@test "ssh-wrap: KVM_HOST unset -> no re-exec (returns, stays local)" {
  run invoke_shim
  [ "$status" -eq 0 ]
  [[ "$output" != *"SSH_WRAP_CMD:"* ]]
  [[ "$output" != *"re-execing"* ]]
}

@test "ssh-wrap: KVM_HOST == hostname -> no re-exec (operator on the KVM host)" {
  KVM_HOST="$LOCAL_HOST" HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 run invoke_shim
  [ "$status" -eq 0 ]
  [[ "$output" != *"SSH_WRAP_CMD:"* ]]
}

@test "ssh-wrap: KVM_HOST=<hostname>.<domain> short-form matches -> no re-exec" {
  KVM_HOST="${LOCAL_HOST}.example.lan" HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 run invoke_shim
  [ "$status" -eq 0 ]
  [[ "$output" != *"SSH_WRAP_CMD:"* ]]
}

@test "ssh-wrap: HBIRD_REMOTE_REEXEC=1 sentinel -> no re-exec (we ARE the remote)" {
  KVM_HOST=otherhost HBIRD_REMOTE_REEXEC=1 HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 run invoke_shim
  [ "$status" -eq 0 ]
  [[ "$output" != *"SSH_WRAP_CMD:"* ]]
}

# ---------------------------------------------------------------------------
# The shim fires — env-allowlist contract
# ---------------------------------------------------------------------------

@test "ssh-wrap: KVM_HOST=otherhost fires re-exec, prints SSH_WRAP_CMD with remote script path" {
  KVM_HOST=otherhost HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 run invoke_shim
  [ "$status" -eq 0 ]
  # Round-2 shape: ssh -t HOST cd <REPO> && sudo env ... bash <REMOTE_SCRIPT> ARGS
  [[ "$output" == *"SSH_WRAP_CMD: ssh -t otherhost cd "* ]]
  [[ "$output" == *"sudo env HBIRD_REMOTE_REEXEC=1"* ]]
  # Script basename is invoked from disk on the remote, NOT streamed via stdin.
  [[ "$output" == *"/scripts/foo.sh"* ]]
  # No more `bash -s --` streaming pattern.
  [[ "$output" != *"bash -s --"* ]]
}

@test "ssh-wrap: positional args are forwarded verbatim after the remote script path" {
  KVM_HOST=otherhost HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 run invoke_shim --workers-only --node=hbird-w1
  [ "$status" -eq 0 ]
  [[ "$output" == *"/scripts/foo.sh --workers-only --node=hbird-w1"* ]]
}

@test "ssh-wrap: default HBIRD_REMOTE_REPO is ~/hummingbird-k8s" {
  KVM_HOST=otherhost HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 run invoke_shim
  [ "$status" -eq 0 ]
  [[ "$output" == *"cd ~/hummingbird-k8s"* ]]
  [[ "$output" == *"bash ~/hummingbird-k8s/scripts/foo.sh"* ]]
}

@test "ssh-wrap: HBIRD_REMOTE_REPO override is honored in the remote command" {
  KVM_HOST=otherhost HBIRD_REMOTE_REPO=/opt/hummingbird-k8s \
    HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 \
    run invoke_shim
  [ "$status" -eq 0 ]
  [[ "$output" == *"cd /opt/hummingbird-k8s"* ]]
  [[ "$output" == *"bash /opt/hummingbird-k8s/scripts/foo.sh"* ]]
}

@test "ssh-wrap: only allowlisted env vars are forwarded (CONFIG, IMAGE_SOURCE present; SECRET absent)" {
  # CONFIG=/nonexistent skips the scp branch (the `-f` test fails),
  # so we exercise only the env-allowlist path here.
  KVM_HOST=otherhost \
    HBIRD_SSH_WRAP_DRY_RUN=1 \
    HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 \
    CONFIG=/nonexistent/cluster.local.conf \
    IMAGE_SOURCE=ghcr \
    GHCR_TAG=v0.4.2 \
    AUTO_UPDATE_CP=true \
    HBIRD_NOT_ALLOWLISTED=secret-value-do-not-forward \
    AWS_SECRET_ACCESS_KEY=likewise-do-not-forward \
    run invoke_shim
  [ "$status" -eq 0 ]
  # Allowlisted vars: present.
  [[ "$output" == *"CONFIG=/nonexistent/cluster.local.conf"* ]]
  [[ "$output" == *"IMAGE_SOURCE=ghcr"* ]]
  [[ "$output" == *"GHCR_TAG=v0.4.2"* ]]
  [[ "$output" == *"AUTO_UPDATE_CP=true"* ]]
  # Non-allowlisted vars: absent.
  [[ "$output" != *"HBIRD_NOT_ALLOWLISTED"* ]]
  [[ "$output" != *"secret-value-do-not-forward"* ]]
  [[ "$output" != *"AWS_SECRET_ACCESS_KEY"* ]]
  [[ "$output" != *"likewise-do-not-forward"* ]]
}

@test "ssh-wrap: empty-but-set var is still forwarded (DRY_RUN= empty -> DRY_RUN='')" {
  # Distinguishes "set to empty" from "unset" — bash's [[ -n "${v+x}" ]]
  # treats empty-set as set. Empty SKIP_DRAIN= is a legit operator value
  # ("disable" vs unset = "default"), so it should be forwarded.
  # printf %q of empty string renders as `''`; printf %q of "1" is `1`.
  KVM_HOST=otherhost HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 \
    DRY_RUN= SKIP_DRAIN=1 \
    run invoke_shim
  [ "$status" -eq 0 ]
  ssh_cmd_line="$(printf '%s\n' "$output" | grep '^SSH_WRAP_CMD:')"
  [[ "$ssh_cmd_line" == *"DRY_RUN=''"* ]]
  [[ "$ssh_cmd_line" == *"SKIP_DRAIN=1"* ]]
}

# ---------------------------------------------------------------------------
# M7: shell-meta-safety. Values with spaces and shell metas MUST be quoted
# on the way out, not word-split by the remote bash. (Round-2 HIGH fix.)
# ---------------------------------------------------------------------------

@test "ssh-wrap: FLAGS with embedded space is printf %q quoted in the remote command" {
  KVM_HOST=otherhost HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 \
    FLAGS="--foo bar" \
    run invoke_shim
  [ "$status" -eq 0 ]
  # Extract the SSH_WRAP_CMD line only — the shim ALSO emits a
  # human-readable "[foo.sh] re-execing ... (env: ...)" log line where
  # the env array is rendered unquoted by design (for operator
  # readability). The bytes that actually reach the remote bash are on
  # the SSH_WRAP_CMD line.
  ssh_cmd_line="$(printf '%s\n' "$output" | grep '^SSH_WRAP_CMD:')"
  [ -n "$ssh_cmd_line" ]
  # printf %q of "--foo bar" renders as `--foo\ bar` (backslash-escaped
  # space) on modern bash, or `'--foo bar'` on some variants. Either is
  # acceptable — what matters is the embedded space is NOT a bare
  # unquoted value on the SSH command line.
  [[ "$ssh_cmd_line" != *"FLAGS=--foo bar "* ]]
  [[ "$ssh_cmd_line" == *"FLAGS=--foo\\ bar"* || "$ssh_cmd_line" == *"FLAGS='--foo bar'"* ]]
}

@test "ssh-wrap: positional arg with shell meta is printf %q quoted" {
  KVM_HOST=otherhost HBIRD_SSH_WRAP_DRY_RUN=1 HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 \
    run invoke_shim 'arg with space' '--evil=$(rm -rf /)'
  [ "$status" -eq 0 ]
  # The literal `$(rm -rf /)` substring (unquoted) must NOT appear in the
  # output — printf %q escapes the `$(` / spaces.
  [[ "$output" != *' --evil=$(rm -rf /)'* ]]
  # And the embedded space in 'arg with space' must not appear as a bare
  # value either (would be three separate words on the remote).
  [[ "$output" != *' arg with space '* ]]
}

# ---------------------------------------------------------------------------
# M3: CONFIG scp branch coverage
# ---------------------------------------------------------------------------

@test "ssh-wrap: CONFIG=<local file> triggers scp branch and rewrites CONFIG to remote path" {
  # Create a real local file so the `[[ -f "$CONFIG" ]]` test passes.
  # mktemp(1) on alpine/busybox doesn't support `-t TEMPLATE.conf`
  # suffix syntax — use a portable path instead.
  tmp_dir="${BATS_TEST_TMPDIR:-/tmp}/hbird-wrap-test-$$"
  mkdir -p "$tmp_dir"
  tmp_cfg="${tmp_dir}/cluster.local.conf"
  echo "# test config" > "$tmp_cfg"

  KVM_HOST=otherhost \
    HBIRD_SSH_WRAP_DRY_RUN=1 \
    HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 \
    HBIRD_SSH_WRAP_DRY_RUN_SCP=1 \
    CONFIG="$tmp_cfg" \
    run invoke_shim

  rm -rf "$tmp_dir"

  [ "$status" -eq 0 ]
  # Stub scp message printed to stderr.
  [[ "$output" == *"SCP_WOULD_RUN: ${tmp_cfg} -> otherhost:/tmp/hbird-dryrun/cluster.local.conf"* ]]
  # Rewritten CONFIG entry points at the remote tempdir path, not the
  # local path.
  [[ "$output" == *"CONFIG=/tmp/hbird-dryrun/cluster.local.conf"* ]]
  # And the local path does NOT appear as a forwarded env value on the
  # actual SSH command line.
  ssh_cmd_line="$(printf '%s\n' "$output" | grep '^SSH_WRAP_CMD:')"
  [[ "$ssh_cmd_line" != *"CONFIG=${tmp_cfg} "* ]]
}

# ---------------------------------------------------------------------------
# The pinned allowlist itself
# ---------------------------------------------------------------------------
#
# This test pins the EXACT set of env vars the shim is willing to
# forward. Any future change to HBIRD_SSH_WRAP_ALLOWED_ENV in
# scripts/lib/ssh-wrap.sh must update this list. That is intentional:
# widening the allowlist is a security-relevant change ("which of my
# local exports silently mutate remote behavior?") and should never
# slip through unreviewed. (C3, #232.)

@test "ssh-wrap: HBIRD_SSH_WRAP_ALLOWED_ENV pins the exact allowlist set" {
  # shellcheck disable=SC1090
  source "$LIB"
  expected=(
    CONFIG FLAGS
    AUTO_UPDATE_CP SWITCH_TO_GHCR
    BOOTC_UPDATE_SCHEDULE BOOTC_UPDATE_REPO_K8S BOOTC_UPDATE_REPO_WORKER
    IMAGE_SOURCE GHCR_TAG
    DRY_RUN SKIP_DRAIN WORKERS_ONLY NODE START_FROM PARALLEL
    READY_TIMEOUT DRAIN_TIMEOUT APISERVER_TIMEOUT SSH_TIMEOUT
    INTER_NODE_SLEEP DAEMONSET_TIMEOUT
    CP_NAME WORKER_NAMES POOL_DIR
    VM_USER STORAGE_DRIVER PODMAN_ROOT PODMAN_RUNROOT APISERVER_EXTRA_SANS
    HBIRD_AUTOLOAD_CONFIG_LOCAL HBIRD_REMOTE_REPO
  )
  # Sorted compare so reordering doesn't trip the test (the contract is
  # the set, not the order).
  expected_sorted="$(printf '%s\n' "${expected[@]}" | LC_ALL=C sort | tr '\n' ' ')"
  actual_sorted="$(printf '%s\n' "${HBIRD_SSH_WRAP_ALLOWED_ENV[@]}" | LC_ALL=C sort | tr '\n' ' ')"
  [ "$expected_sorted" = "$actual_sorted" ]
}

# ---------------------------------------------------------------------------
# Each of the 4 wrapped scripts sources the shim
# ---------------------------------------------------------------------------
#
# We don't actually run these end-to-end (they'd need root + libvirt),
# but we DO verify each script's top-of-file region literally sources
# scripts/lib/ssh-wrap.sh and invokes hbird_ssh_wrap_maybe_reexec.
# That way a future refactor that accidentally drops the shim from one
# script gets caught immediately.

@test "ssh-wrap: deploy-cluster.sh sources lib/ssh-wrap.sh + invokes the shim" {
  grep -q 'source "${SCRIPT_DIR}/lib/ssh-wrap.sh"' "${REPO_ROOT}/scripts/deploy-cluster.sh"
  grep -q 'hbird_ssh_wrap_maybe_reexec "$0" "$@"' "${REPO_ROOT}/scripts/deploy-cluster.sh"
}

@test "ssh-wrap: destroy-cluster.sh sources lib/ssh-wrap.sh + invokes the shim" {
  grep -q 'source "${SCRIPT_DIR}/lib/ssh-wrap.sh"' "${REPO_ROOT}/scripts/destroy-cluster.sh"
  grep -q 'hbird_ssh_wrap_maybe_reexec "$0" "$@"' "${REPO_ROOT}/scripts/destroy-cluster.sh"
}

@test "ssh-wrap: update-cluster.sh sources lib/ssh-wrap.sh + invokes the shim" {
  grep -q 'source "${SCRIPT_DIR}/lib/ssh-wrap.sh"' "${REPO_ROOT}/scripts/update-cluster.sh"
  grep -q 'hbird_ssh_wrap_maybe_reexec "$0" "$@"' "${REPO_ROOT}/scripts/update-cluster.sh"
}

@test "ssh-wrap: spawn-workers.sh sources lib/ssh-wrap.sh + invokes the shim" {
  grep -q 'source "${SCRIPT_DIR}/lib/ssh-wrap.sh"' "${REPO_ROOT}/scripts/spawn-workers.sh"
  grep -q 'hbird_ssh_wrap_maybe_reexec "$0" "$@"' "${REPO_ROOT}/scripts/spawn-workers.sh"
}

# ---------------------------------------------------------------------------
# The shim is placed BEFORE EUID/root checks (spawn-workers special case)
# ---------------------------------------------------------------------------
#
# In spawn-workers.sh, the EUID==0 check used to be the first thing
# after `set -euo pipefail`. The shim has to fire FIRST so the client
# doesn't need to type `sudo` locally — sudo happens on the remote.
# Verify by line-number: hbird_ssh_wrap_maybe_reexec invocation must
# appear above the `EUID -ne 0` test.

@test "ssh-wrap: spawn-workers.sh places shim before the EUID check" {
  local shim_line euid_line
  shim_line=$(grep -n 'hbird_ssh_wrap_maybe_reexec' "${REPO_ROOT}/scripts/spawn-workers.sh" | head -1 | cut -d: -f1)
  euid_line=$(grep -n '\$EUID -ne 0' "${REPO_ROOT}/scripts/spawn-workers.sh" | head -1 | cut -d: -f1)
  [ -n "$shim_line" ]
  [ -n "$euid_line" ]
  [ "$shim_line" -lt "$euid_line" ]
}
