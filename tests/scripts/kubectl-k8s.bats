#!/usr/bin/env bats
#
# Unit tests for scripts/kubectl-k8s.sh — issues #247 and #220.
#
# Issue #247 (PATH-stub-driven tests at top): the day-2 kubectl wrapper
# ssh's to $KVM_HOST and runs `sudo virsh -c qemu:///system domifaddr <CP>`
# to find the control-plane VM's IP, then sets up a local-port-forward
# tunnel. Issue #247 fixed two bugs in that flow:
#
#   1. The first ssh invocation MUST pass `-t` so sudo on the remote can
#      prompt for the operator's password when the remote sudo cache is
#      cold. Without -t the call fails with
#      `sudo: a terminal is required to read the password`.
#
#   2. ssh -t allocates a remote TTY which causes sshd to inject \r on
#      every newline of the captured stdout. The IP-extraction awk then
#      sees `192.0.2.10\r` and the carriage return either ends up in the
#      tunnel target ("192.0.2.10\r:6443" — ssh rejects it as malformed)
#      or trips downstream consumers. We strip \r before awk parses.
#
# We stub `ssh`, `ss`, and `kubectl` on PATH so the script's real network
# work is replaced by argv-capture and canned output. The stubs let us
# assert: (a) the first ssh call carries `-t`; (b) the captured stdout
# has \r stripped before awk; (c) the second ssh (-fNL tunnel) gets a
# clean IP without trailing \r.
#
# Issue #220 (CONFIG= threading tests at bottom): sister suite of
# tests/scripts/cp-name-alias.bats, which already pins the four-state
# truth table for the CP_NAME / VM_NAME alias resolver. The #220 tests
# specifically exercise the `CONFIG=<path>` code path that #220 wired
# into `make kubectl` and `make nodes`:
#
#   - When CONFIG= is set and readable, the script sources it FIRST so
#     CP_NAME / KVM_HOST / etc. come from cluster.local.conf.
#   - When CONFIG= is set but NOT readable, the script must exit 2 with
#     a one-line diagnostic that names the offending path.
#   - When CONFIG= is unset, behavior matches pre-#220: source
#     config.local.sh if present, else fall back to default.
#   - CP_NAME from CONFIG must beat the legacy `VM_NAME` alias (no
#     deprecation warning when CP_NAME is what was read).
#
# The #220 tests sidestep the script's SSH/virsh side effects by aborting
# before they fire (env -i + a separate stub-ssh that returns empty).

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/kubectl-k8s.sh"
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  # Stub-bin: every binary the script touches is shimmed so no real
  # network / libvirt / kubectl call escapes.
  STUB_DIR="$BATS_TEST_TMPDIR/stub-bin"
  mkdir -p "$STUB_DIR"

  # ssh stub: write each invocation's argv to $SSH_ARGV_DIR/argv-N (one
  # arg per line, in $SSH_ARGV_DIR/counter we tally calls). On the FIRST
  # call (the domifaddr probe) emit a virsh-formatted line that ENDS with
  # \r, mimicking what an interactive sshd sends back when -t is passed.
  # The test then asserts that the script's `tr -d '\r'` strips it before
  # awk runs.
  cat > "$STUB_DIR/ssh" <<'EOF'
#!/usr/bin/env bash
# Bump the call counter atomically (single-writer; tests are serial).
n=$(( $(cat "${SSH_ARGV_DIR}/counter" 2>/dev/null || echo 0) + 1 ))
printf '%s\n' "$n" > "${SSH_ARGV_DIR}/counter"
# Write argv: one arg per line. Safe for grep -Fx.
printf '%s\n' "$@" > "${SSH_ARGV_DIR}/argv-${n}"

# Distinguish first call (domifaddr) from second (-fNL tunnel). We treat
# any invocation containing `-fNL` as the tunnel setup — no stdout needed,
# just exit 0.
for a in "$@"; do
  if [[ "$a" == "-fNL" ]]; then
    exit 0
  fi
done

# Otherwise: this is the domifaddr probe. Emit virsh-domifaddr-shaped
# stdout WITH explicit \r line endings so the test verifies the script's
# tr -d '\r' strip works against the real sshd-with-TTY output pattern.
printf ' Name       MAC address          Protocol     Address\r\n'
printf '-------------------------------------------------------------------------------\r\n'
printf ' vnet0      52:54:00:de:ad:be    ipv4         192.0.2.10/24\r\n'
EOF
  chmod +x "$STUB_DIR/ssh"

  # ss stub: report no listener so the script enters the tunnel-setup
  # branch (the if !ss -ltn | grep ... block).
  cat > "$STUB_DIR/ss" <<'EOF'
#!/usr/bin/env bash
# Print a deliberately-empty -ltn listing — no 127.0.0.1:$LOCAL_PORT row.
printf 'State    Recv-Q   Send-Q     Local Address:Port    Peer Address:Port\n'
EOF
  chmod +x "$STUB_DIR/ss"

  # kubectl stub: the script does `exec kubectl ... "$@"` at the end. We
  # want the test to reach that point (proving the script successfully
  # parsed an IP + set up the tunnel) without actually running anything.
  cat > "$STUB_DIR/kubectl" <<'EOF'
#!/usr/bin/env bash
printf 'KUBECTL_STUB_RAN\n'
# Echo argv so a future test can assert pass-through if needed.
printf 'argv: %s\n' "$*"
exit 0
EOF
  chmod +x "$STUB_DIR/kubectl"

  # Pre-create a kubeconfig file so the [[ -f $KCFG ]] gate passes.
  KCFG="$BATS_TEST_TMPDIR/kubeconfig"
  : > "$KCFG"

  SSH_ARGV_DIR="$BATS_TEST_TMPDIR/ssh-argv"
  mkdir -p "$SSH_ARGV_DIR"
  export SSH_ARGV_DIR

  # ---------------------------------------------------------------------
  # Issue #220 fixtures: minimal cluster.local.conf-shaped files for the
  # CONFIG= path tests at the bottom of this file. The script sources
  # these verbatim as bash; any side effect lands in the test shell's
  # downstream check.
  # ---------------------------------------------------------------------
  CONF_GOOD="${BATS_TEST_TMPDIR}/cluster.good.conf"
  cat > "$CONF_GOOD" <<'EOF'
# Test fixture for kubectl-k8s.bats — minimal cluster.local.conf shape.
KVM_HOST=stub-kvm-host
CP_NAME=hbird-cp1-from-config
EOF

  CONF_LEGACY="${BATS_TEST_TMPDIR}/cluster.legacy.conf"
  cat > "$CONF_LEGACY" <<'EOF'
# Test fixture: ONLY VM_NAME (legacy) set — exercises the alias warning
# from inside CONFIG sourcing.
KVM_HOST=stub-kvm-host
VM_NAME=legacy-vm-from-config
EOF
}

# Helper: invoke the script with all stubs in PATH.
_invoke() {
  PATH="${STUB_DIR}:${PATH}" \
    KVM_HOST=stub-kvm \
    CP_NAME=hbird-cp1 \
    KCFG="$KCFG" \
    bash "$SCRIPT" "$@"
}

# Helper: emit one line per arg of the Nth ssh invocation (1-indexed).
# Caller can grep -Fx on bare args or grep -F for substrings.
_ssh_argv_file() {
  local n="$1"
  printf '%s\n' "${SSH_ARGV_DIR}/argv-${n}"
}

# Helper (issue #220 tests): run the script with a stub ssh on PATH so
# the script's SSH `domifaddr` call is captured (and short-circuited) by
# a logger instead of trying to talk to a real host. KVM_HOST is set to a
# nonsense value so a leaked SSH call would fail loudly. Returns the
# stub directory path on stdout.
_stub_ssh_dir() {
  local d="$BATS_TEST_TMPDIR/stub-bin-220"
  mkdir -p "$d"
  cat > "$d/ssh" <<'EOF'
#!/usr/bin/env bash
# Stub ssh: log the argv to a sentinel file and exit failure so the
# script proceeds to its diagnostic exit (which is what the test
# greps).
printf 'STUB_SSH_ARGV: %s\n' "$*" >> "${HBIRD_STUB_SSH_LOG:-/dev/null}"
echo ""  # empty domifaddr → script prints "Could not find ... IP" → exit 1
EOF
  chmod +x "$d/ssh"
  echo "$d"
}

# ---------------------------------------------------------------------------
# Issue #247: ssh -t for the sudo virsh domifaddr call
# ---------------------------------------------------------------------------

@test "kubectl-k8s: domifaddr ssh invocation passes -t (issue #247)" {
  run _invoke get nodes
  [ "$status" -eq 0 ]
  [[ "$output" == *"KUBECTL_STUB_RAN"* ]]

  # The FIRST ssh invocation is the `sudo virsh domifaddr` probe. It MUST
  # include -t so the remote sudo can prompt for the operator's password
  # when the cache is cold. argv is one-arg-per-line in $SSH_ARGV_DIR/argv-1.
  local argv1
  argv1="$(_ssh_argv_file 1)"
  [ -f "$argv1" ]
  # busybox grep treats `-t` as a flag — use -e to pass it as a pattern.
  grep -qxF -e '-t' "$argv1"
}

@test "kubectl-k8s: domifaddr ssh argv contains 'sudo virsh' + CP_NAME" {
  run _invoke get nodes
  [ "$status" -eq 0 ]

  local argv1
  argv1="$(_ssh_argv_file 1)"
  [ -f "$argv1" ]
  grep -qxF 'stub-kvm' "$argv1"
  grep -qxF 'sudo virsh -c qemu:///system domifaddr hbird-cp1' "$argv1"
}

# ---------------------------------------------------------------------------
# Issue #247: \r stripping. The ssh -t stub emits CR-terminated lines.
# Without `tr -d '\r'` the awk pipeline would capture "192.0.2.10\r"
# and the tunnel-setup ssh -fNL would get "192.0.2.10\r:6443" as the
# forward target. Assert the second ssh's argv has a clean IP.
# ---------------------------------------------------------------------------

@test "kubectl-k8s: tunnel ssh -fNL gets carriage-return-free IP (issue #247)" {
  run _invoke get nodes
  [ "$status" -eq 0 ]

  # Second ssh call is the -fNL tunnel.
  local argv2
  argv2="$(_ssh_argv_file 2)"
  [ -f "$argv2" ]
  # busybox grep treats `-fNL` as a flag bundle — use -e to pass it as a pattern.
  grep -qxF -e '-fNL' "$argv2"
  # Forward spec must be exactly "6443:192.0.2.10:6443" — no \r between
  # the IP and the port. Match the bare-line arg with grep -Fx.
  grep -qxF -e '6443:192.0.2.10:6443' "$argv2"
  # And confirm there's no literal \r anywhere in the argv.
  if grep -q $'\r' "$argv2"; then
    printf 'FAIL: tunnel argv has embedded CR: %s\n' "$(cat -A "$argv2")"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# Regression guard: the LITERAL bytes `ssh -t` appear in the source at
# the domifaddr line. Catches a future refactor that accidentally drops
# the -t (the per-invocation argv test above would also catch it, but a
# source-grep is a cheap belt-and-suspenders).
# ---------------------------------------------------------------------------

@test "kubectl-k8s: source contains 'ssh -t' for the domifaddr probe" {
  grep -qE 'ssh -t "\$KVM_HOST" "sudo virsh' "${REPO_ROOT}/scripts/kubectl-k8s.sh"
}

@test "kubectl-k8s: source pipes the domifaddr stdout through tr -d for \\r" {
  # Look for the tr -d '\r' immediately following the ssh in the pipeline.
  grep -qE "tr -d '\\\\r'" "${REPO_ROOT}/scripts/kubectl-k8s.sh"
}

# ---------------------------------------------------------------------------
# Issue #220: CONFIG= threading — `make kubectl CONFIG=…` reads CP_NAME /
# KVM_HOST from cluster.local.conf instead of forcing the operator to
# re-export them. The wiring already landed in commit 491a87e (PR #219
# round-2 H2); these tests pin the contract so it cannot silently regress.
# ---------------------------------------------------------------------------

@test "kubectl-k8s: CONFIG=<readable file> is sourced and CP_NAME from CONFIG wins" {
  local stub; stub="$(_stub_ssh_dir)"
  export HBIRD_STUB_SSH_LOG="$BATS_TEST_TMPDIR/ssh.log"
  : > "$HBIRD_STUB_SSH_LOG"

  # CONFIG points at fixture with CP_NAME=hbird-cp1-from-config.
  # KVM_HOST is read from CONFIG too. Script will reach the
  # `Could not find ${CP_NAME} IP` exit path → which echoes CP_NAME
  # verbatim to stderr.
  run env -i PATH="${stub}:/usr/bin:/bin" HOME="$HOME" \
        HBIRD_STUB_SSH_LOG="$HBIRD_STUB_SSH_LOG" \
        CONFIG="$CONF_GOOD" \
        bash "$SCRIPT" get nodes
  # Expected to fail somewhere downstream (no real cluster); we only
  # care that CP_NAME from CONFIG was resolved before the failure.
  [ "$status" -ne 0 ]
  [[ "$output" == *"hbird-cp1-from-config"* ]] || {
    echo "CP_NAME from CONFIG did not propagate (expected hbird-cp1-from-config):" >&2
    echo "$output" >&2
    return 1
  }
  # And the stub-ssh argv must mention the resolved name — confirming
  # the script reached the virsh-over-ssh call with the right CP_NAME.
  grep -q 'hbird-cp1-from-config' "$HBIRD_STUB_SSH_LOG" || {
    echo "stub ssh was never invoked with CP_NAME=hbird-cp1-from-config:" >&2
    cat "$HBIRD_STUB_SSH_LOG" >&2
    return 1
  }
  # No deprecation warning when CP_NAME came directly from CONFIG.
  [[ "$output" != *"VM_NAME is deprecated"* ]]
}

@test "kubectl-k8s: CONFIG=<unreadable path> exits 2 with diagnostic naming the path" {
  # Operator-typo path: missing or unreadable file. Script must exit
  # before doing anything destructive (no SSH, no podman) and the
  # error message must include the bad path so the operator sees what
  # they mistyped.
  run env -i PATH="/usr/bin:/bin" HOME="$HOME" \
        CONFIG="/nonexistent/cluster.local.conf" \
        bash "$SCRIPT" get nodes
  [ "$status" -eq 2 ]
  [[ "$output" == *"CONFIG not readable"* ]]
  [[ "$output" == *"/nonexistent/cluster.local.conf"* ]]
}

@test "kubectl-k8s: CONFIG=<file with VM_NAME only> emits deprecation warning + resolves alias" {
  # Legacy migrators: someone with VM_NAME=… still in cluster.local.conf
  # (not the recommended shape — they should rename to CP_NAME). The
  # script must still resolve, but with a one-line stderr warning.
  local stub; stub="$(_stub_ssh_dir)"
  export HBIRD_STUB_SSH_LOG="$BATS_TEST_TMPDIR/ssh.log"
  : > "$HBIRD_STUB_SSH_LOG"

  run env -i PATH="${stub}:/usr/bin:/bin" HOME="$HOME" \
        HBIRD_STUB_SSH_LOG="$HBIRD_STUB_SSH_LOG" \
        CONFIG="$CONF_LEGACY" \
        bash "$SCRIPT" get nodes
  [ "$status" -ne 0 ]
  [[ "$output" == *"VM_NAME is deprecated"* ]] || {
    echo "expected deprecation warning when only VM_NAME (no CP_NAME) is set:" >&2
    echo "$output" >&2
    return 1
  }
  [[ "$output" == *"legacy-vm-from-config"* ]]
}

@test "kubectl-k8s: usage docstring describes CONFIG= env var (#220 drift fence)" {
  # The script's leading comment block documents the env-var contract
  # — operators read it via `head scripts/kubectl-k8s.sh`. A future
  # change that drops CONFIG= from the recipe but forgets the
  # comment leaves operators reading a stale contract. Fail loudly.
  run grep -E '^#[[:space:]]*CONFIG' "$SCRIPT"
  [ "$status" -eq 0 ] || {
    echo "scripts/kubectl-k8s.sh leading comment block no longer documents CONFIG=" >&2
    return 1
  }
  # And the comment must mention cluster.local.conf so the operator
  # knows WHERE CONFIG= points.
  run grep -F 'cluster.local.conf' "$SCRIPT"
  [ "$status" -eq 0 ] || {
    echo "scripts/kubectl-k8s.sh no longer mentions cluster.local.conf" >&2
    return 1
  }
}
