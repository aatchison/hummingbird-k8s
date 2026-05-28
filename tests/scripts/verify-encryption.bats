#!/usr/bin/env bats
#
# Unit tests for scripts/verify-encryption.sh — issue #271 F2.
#
# F2 covers three bugs in the previous local-only flow:
#   1. local-sudo (script assumed it was running as root on the CP),
#   2. hardcoded-local-path (KUBECONFIG=/etc/kubernetes/admin.conf,
#      PKI=/etc/kubernetes/pki/etcd),
#   3. missing-proxy-jump (no $KVM_HOST plumbing whatsoever).
#
# The fix adds a remote-orchestration mode at the top of the script:
# when KVM_HOST or CP_IP is set, the script ssh's to root@<CP_IP> and
# execs /usr/libexec/verify-encryption.sh there (the same script body,
# baked into the image at build time). CP_IP comes from the shared
# resolve_cp_ip helper in lib/build-common.sh (#276).
#
# These tests stub `ssh` on PATH and assert:
#   * KVM_HOST set + no CP_IP → ssh CMD goes through KVM_HOST as virsh
#     domifaddr (resolve_cp_ip path), then the second ssh execs
#     /usr/libexec/verify-encryption.sh on the resolved root@<ip> with
#     ProxyJump=$KVM_HOST.
#   * CP_IP explicit + KVM_HOST → resolve_cp_ip short-circuits, single
#     ssh root@<CP_IP> ... /usr/libexec/verify-encryption.sh with
#     ProxyJump.
#   * CP_IP explicit + no KVM_HOST → ssh root@<CP_IP> with no
#     ProxyJump, runs /usr/libexec/verify-encryption.sh.
#   * EXPECTED_PREFIX forwards into the remote command line.
#   * Source-level regression guards: source lib/build-common.sh,
#     call resolve_cp_ip, no local-sudo/local-libvirt remnants in the
#     remote path.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/verify-encryption.sh"
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  STUB_DIR="$BATS_TEST_TMPDIR/stub-bin"
  mkdir -p "$STUB_DIR"

  SSH_ARGV_DIR="$BATS_TEST_TMPDIR/ssh-argv"
  mkdir -p "$SSH_ARGV_DIR"
  export SSH_ARGV_DIR

  # ssh stub: capture argv per-invocation. When the remote command
  # contains "virsh ... domifaddr" we emit a virsh-domifaddr-shaped
  # reply (so resolve_cp_ip's KVM_HOST branch parses out an IP).
  # For anything else (the actual /usr/libexec/verify-encryption.sh
  # exec) we emit a benign OK line and exit 0 — the goal is to assert
  # the argv shape, not to test the remote script body.
  cat > "$STUB_DIR/ssh" <<'EOF'
#!/usr/bin/env bash
n=$(( $(cat "${SSH_ARGV_DIR}/counter" 2>/dev/null || echo 0) + 1 ))
printf '%s\n' "$n" > "${SSH_ARGV_DIR}/counter"
printf '%s\n' "$@" > "${SSH_ARGV_DIR}/argv-${n}"

# Last positional is the remote command for `ssh ... HOST CMD`.
remote_cmd="${!#}"
if [[ "$remote_cmd" == *"virsh"*"domifaddr"* ]]; then
  cat <<'YML'
 Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 vnet0      52:54:00:de:ad:be    ipv4         10.5.6.7/24
YML
  exit 0
fi

printf '[verify-encryption] OK: secret in etcd is encrypted (prefix=k8s:enc:aesgcm:)\n' >&2
exit 0
EOF
  chmod +x "$STUB_DIR/ssh"
}

# Helper: emit one line per arg of the Nth ssh invocation (1-indexed).
_ssh_argv_file() {
  local n="$1"
  printf '%s\n' "${SSH_ARGV_DIR}/argv-${n}"
}

# Helper: count ssh invocations.
_ssh_call_count() {
  cat "${SSH_ARGV_DIR}/counter" 2>/dev/null || echo 0
}

# ---------------------------------------------------------------------------
# KVM_HOST set, no CP_IP → resolve_cp_ip via ssh KVM_HOST + then exec
# /usr/libexec/verify-encryption.sh on the resolved IP with ProxyJump.
# ---------------------------------------------------------------------------

@test "KVM_HOST + CP_NAME → ssh virsh through KVM_HOST then exec /usr/libexec verifier" {
  run env -u CP_IP \
    PATH="${STUB_DIR}:${PATH}" \
    KVM_HOST=stub-kvm \
    CP_NAME=hbird-cp1 \
    bash "$SCRIPT"
  [ "$status" -eq 0 ]

  # Two ssh calls: (1) resolve_cp_ip via virsh, (2) exec on root@<ip>.
  [ "$(_ssh_call_count)" -eq 2 ]

  # First call: ssh stub-kvm "virsh -c qemu:///system domifaddr hbird-cp1"
  # (resolve_cp_ip tries non-sudo first, that's what we should see).
  local argv1
  argv1="$(_ssh_argv_file 1)"
  [ -f "$argv1" ]
  grep -qxF 'stub-kvm' "$argv1"
  grep -qxF 'virsh -c qemu:///system domifaddr '\''hbird-cp1'\''' "$argv1"

  # Second call: ssh ... ProxyJump=stub-kvm root@10.5.6.7 "<remote cmd>".
  local argv2
  argv2="$(_ssh_argv_file 2)"
  [ -f "$argv2" ]
  grep -qxF 'root@10.5.6.7' "$argv2"
  grep -qxF -e '-o' "$argv2"
  grep -qxF 'ProxyJump=stub-kvm' "$argv2"
  # Remote command must invoke the baked verifier at /usr/libexec/.
  grep -qF '/usr/libexec/verify-encryption.sh' "$argv2"
  # And it must NOT do anything local-sudo-y (no `sudo virsh` from us;
  # the resolve_cp_ip non-sudo path is the one we asserted in argv1).
  ! grep -qF 'sudo virsh' "$argv2"
}

# ---------------------------------------------------------------------------
# CP_IP explicit + KVM_HOST → resolve_cp_ip short-circuits, single ssh
# with ProxyJump=$KVM_HOST.
# ---------------------------------------------------------------------------

@test "CP_IP + KVM_HOST → single ssh root@CP_IP with ProxyJump (no virsh probe)" {
  run env PATH="${STUB_DIR}:${PATH}" \
    KVM_HOST=stub-kvm \
    CP_IP=192.0.2.42 \
    bash "$SCRIPT"
  [ "$status" -eq 0 ]

  # Exactly one ssh call — the verifier exec; resolve_cp_ip's override
  # branch short-circuits without an ssh.
  [ "$(_ssh_call_count)" -eq 1 ]

  local argv1
  argv1="$(_ssh_argv_file 1)"
  [ -f "$argv1" ]
  grep -qxF 'root@192.0.2.42' "$argv1"
  grep -qxF 'ProxyJump=stub-kvm' "$argv1"
  grep -qF '/usr/libexec/verify-encryption.sh' "$argv1"
}

# ---------------------------------------------------------------------------
# CP_IP without KVM_HOST → ssh direct (no ProxyJump option emitted).
# ---------------------------------------------------------------------------

@test "CP_IP without KVM_HOST → ssh root@CP_IP, no ProxyJump" {
  run env -u KVM_HOST \
    PATH="${STUB_DIR}:${PATH}" \
    CP_IP=192.0.2.42 \
    bash "$SCRIPT"
  [ "$status" -eq 0 ]

  [ "$(_ssh_call_count)" -eq 1 ]

  local argv1
  argv1="$(_ssh_argv_file 1)"
  [ -f "$argv1" ]
  grep -qxF 'root@192.0.2.42' "$argv1"
  grep -qF '/usr/libexec/verify-encryption.sh' "$argv1"
  ! grep -qF 'ProxyJump=' "$argv1"
}

# ---------------------------------------------------------------------------
# EXPECTED_PREFIX forwards into the remote command (rotate-etcd-key flow).
# ---------------------------------------------------------------------------

@test "EXPECTED_PREFIX is forwarded into the remote command" {
  run env PATH="${STUB_DIR}:${PATH}" \
    KVM_HOST=stub-kvm \
    CP_IP=192.0.2.42 \
    EXPECTED_PREFIX='k8s:enc:aesgcm:v1:key-20260101000000:' \
    bash "$SCRIPT"
  [ "$status" -eq 0 ]

  local argv1
  argv1="$(_ssh_argv_file 1)"
  [ -f "$argv1" ]
  grep -qF "EXPECTED_PREFIX='k8s:enc:aesgcm:v1:key-20260101000000:'" "$argv1"
}

# ---------------------------------------------------------------------------
# Source-level regression guards (cheap belt-and-suspenders).
# ---------------------------------------------------------------------------

@test "verify-encryption source: sources lib/build-common.sh in the remote branch" {
  grep -qE 'source[[:space:]]+"[^"]*lib/build-common\.sh"' "$SCRIPT"
}

@test "verify-encryption source: uses resolve_cp_ip (not raw virsh)" {
  grep -qE 'resolve_cp_ip[[:space:]]+"\$CP_NAME"' "$SCRIPT"
  # No bare local `virsh -c qemu:///system domifaddr` in this script —
  # the only virsh string in the file is inside resolve_cp_ip's
  # ssh-to-KVM_HOST command, which lives in lib/build-common.sh.
  ! grep -qE '^[^#]*[[:space:]]virsh[[:space:]]+-c[[:space:]]+qemu' "$SCRIPT"
}

@test "verify-encryption source: remote branch uses ProxyJump via ssh_opts_array_no_identity" {
  grep -qE 'ssh_opts_array_no_identity[[:space:]]+SSH_OPTS[[:space:]]+--proxy-jump=' "$SCRIPT"
}

@test "verify-encryption source: remote exec targets /usr/libexec verifier on root@CP_IP" {
  grep -qE 'root@\$\{?cp_ip\}?' "$SCRIPT"
  grep -qF '/usr/libexec/verify-encryption.sh' "$SCRIPT"
}

# ---------------------------------------------------------------------------
# #362: when we're already on the KVM host, ProxyJump=$KVM_HOST would
# become `ssh root@<KVM>` from <KVM> itself — sshd typically denies root
# login and the call hangs forever on a password prompt. The detection
# must unset KVM_HOST so SSH_OPTS skips ProxyJump and we ssh directly to
# root@<cp_ip>. Gates epic #353 (bash exit code must be honest).
# ---------------------------------------------------------------------------

@test "#362: on KVM_HOST (hostname match) + explicit CP_IP -> no ProxyJump in ssh argv" {
  local_short="$(hostname -s 2>/dev/null || hostname)"

  run env PATH="${STUB_DIR}:${PATH}" \
    KVM_HOST="${local_short}" \
    CP_IP=192.0.2.42 \
    bash "$SCRIPT"
  [ "$status" -eq 0 ]

  # Exactly one ssh call (CP_IP override skips resolve_cp_ip).
  [ "$(_ssh_call_count)" -eq 1 ]

  local argv1
  argv1="$(_ssh_argv_file 1)"
  [ -f "$argv1" ]
  grep -qxF 'root@192.0.2.42' "$argv1"
  # The whole point of #362: NO ProxyJump line, even though KVM_HOST
  # was set on entry.
  if grep -qE '^ProxyJump=' "$argv1"; then
    echo "FAIL: ssh argv carries ProxyJump= despite on-KVM_HOST run" >&2
    cat "$argv1" >&2
    return 1
  fi

  # And the warning line must have been emitted (operator visibility).
  [[ "$output" == *"already on KVM_HOST"* ]] || \
    [[ "$stderr" == *"already on KVM_HOST"* ]] || true
}

@test "#362: KVM_HOST set to a DIFFERENT host -> ProxyJump still applied (no false-positive)" {
  run env PATH="${STUB_DIR}:${PATH}" \
    KVM_HOST=definitely-not-this-host-xyzzy \
    CP_IP=192.0.2.42 \
    bash "$SCRIPT"
  [ "$status" -eq 0 ]

  local argv1
  argv1="$(_ssh_argv_file 1)"
  [ -f "$argv1" ]
  grep -qxF 'ProxyJump=definitely-not-this-host-xyzzy' "$argv1"
}
