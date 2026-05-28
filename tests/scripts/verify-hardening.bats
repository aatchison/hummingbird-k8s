#!/usr/bin/env bats
#
# Unit tests for issue #271 finding F4 — `scripts/verify-hardening.sh`
# needed `KVM_HOST`/`PROXY_JUMP` plumbing for its kubectl-using paths.
# The fix:
#   * Script honors `KUBECTL=` (default: scripts/kubectl-k8s.sh) for the
#     PSA apply/delete + CP-node lookup, so workstation operators get
#     the SSH-tunnel-through-KVM-host wrapper for free.
#   * Script also calls `resolve_cp_ip "$CP_NAME"` before kubectl, so
#     CP_IP resolution works when kubectl isn't wired up locally.
#   * SSH calls to root@CP_IP already carried `-o ProxyJump=$KVM_HOST`
#     (line 94); these tests pin that contract.
#   * Makefile target now passes `KUBECTL=$(CURDIR)/scripts/kubectl-k8s.sh`
#     and `CONFIG=$(CONFIG)` to the script (matches `make kubectl`).
#
# What these tests check:
#   1. Source-level: KUBECTL default points at scripts/kubectl-k8s.sh,
#      kc() wrapper exists, all kubectl call-sites go through kc.
#   2. Functional: with KVM_HOST set and CP_IP unset, the script invokes
#      ssh with `-o ProxyJump=KVM_HOST` for the on_cp helper, AND
#      attempts CP_IP resolution via resolve_cp_ip (ssh to KVM_HOST
#      running virsh domifaddr) before falling back to kubectl.
#   3. Makefile target wires KUBECTL + CONFIG through.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/verify-hardening.sh"
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  STUB_DIR="$BATS_TEST_TMPDIR/stub-bin"
  mkdir -p "$STUB_DIR"

  SSH_ARGV_DIR="$BATS_TEST_TMPDIR/ssh-argv"
  mkdir -p "$SSH_ARGV_DIR"
  : > "$SSH_ARGV_DIR/counter"
  export SSH_ARGV_DIR
}

# Build the canonical ssh stub: each invocation appends an `argv-N` file
# (one arg per line) for grep assertions. Branches stdout based on the
# remote command:
#   - virsh domifaddr → emit a libvirt domifaddr-shaped table with a
#     fixed CP IP (10.5.6.7) so resolve_cp_ip succeeds.
#   - audit-log probe (the for-loop heredoc) → exit non-zero so the
#     audit check FAILs in isolation; we're not testing the check
#     semantics here, only the SSH argv shape.
#   - everything else → exit 0 silently.
_make_ssh_stub() {
  cat > "$STUB_DIR/ssh" <<'EOF'
#!/usr/bin/env bash
n=$(( $(cat "${SSH_ARGV_DIR}/counter" 2>/dev/null || echo 0) + 1 ))
printf '%s\n' "$n" > "${SSH_ARGV_DIR}/counter"
printf '%s\n' "$@" > "${SSH_ARGV_DIR}/argv-${n}"

# Last arg is the remote command (for `ssh ... HOST CMD`).
remote_cmd="${!#}"
if [[ "$remote_cmd" == *"virsh -c qemu:///system domifaddr"* ]]; then
  cat <<'YML'
 Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 vnet0      52:54:00:aa:bb:cc    ipv4         10.5.6.7/24
YML
  exit 0
fi
# All other ssh calls (on_cp probes) succeed with empty stdout — the
# audit / kubelet checks then FAIL, but that's fine: we're asserting the
# SSH argv shape, not the verification outcomes.
exit 0
EOF
  chmod +x "$STUB_DIR/ssh"
}

# kubectl stub: succeed silently. PSA apply path expects 'violates
# PodSecurity' on stderr/stdout to mark check 1 as PASS — emit it so the
# script reaches the on_cp helpers (where we care about argv).
_make_kubectl_stub() {
  cat > "$STUB_DIR/kubectl" <<'EOF'
#!/usr/bin/env bash
# get nodes -> empty output (resolve_cp_ip should have won first).
# apply -f -  -> PSA rejection text.
# delete pod  -> silent success.
for a in "$@"; do
  if [[ "$a" == "apply" ]]; then
    printf 'Error from server (Forbidden): error when creating "STDIN": pods "verify-hardening-privileged-probe" is forbidden: violates PodSecurity "restricted:latest"\n'
    exit 1
  fi
done
exit 0
EOF
  chmod +x "$STUB_DIR/kubectl"
}

# ---------------------------------------------------------------------------
# Source-level: KUBECTL default, kc() wrapper, no bare kubectl calls.
# ---------------------------------------------------------------------------

@test "F4: KUBECTL defaults to scripts/kubectl-k8s.sh (wrapper)" {
  grep -qE '^: "\$\{KUBECTL:=\$\{_VH_REPO_ROOT\}/scripts/kubectl-k8s\.sh\}"' "$SCRIPT"
}

@test "F4: kc() wrapper is defined and routes to KUBECTL" {
  # Match the function head + the wrapper-detection branch.
  grep -qE '^kc\(\)\s*\{' "$SCRIPT"
  grep -qE 'KUBECTL.*kubectl-k8s\.sh' "$SCRIPT"
}

@test "F4: no bare 'kubectl' invocations remain (all go through kc)" {
  # Strip comments + heredoc body, then look for kubectl invoked as a
  # command. The pattern matches `kubectl ` or `kubectl\n` at a command
  # position (start of line or after `( | && || ; \n etc).
  # Allow it inside heredocs ('EOF' bounded YAML) and in comments.
  # Practical filter: scan only lines that do NOT start with `#`, are
  # not inside a `<<'EOF'` ... `EOF` block, and check for bare-word
  # `kubectl` as a command.
  run awk '
    /^[[:space:]]*#/ { next }
    /<<'\''EOF'\''/  { in_heredoc=1; next }
    in_heredoc && /^EOF$/ { in_heredoc=0; next }
    in_heredoc { next }
    # The variable expansion ${KUBECTL:=...} legitimately mentions "kubectl"
    # in a string position; skip lines that contain ${KUBECTL.
    /\$\{KUBECTL/ { next }
    # Match `kubectl ` or `| kubectl ` or `$ kubectl ` — bare-word use.
    /(^|[[:space:]|;&(]|=)kubectl[[:space:]]/ { print NR": "$0 }
  ' "$SCRIPT"
  [ -z "$output" ]
}

@test "F4: docstring mentions KUBECTL env var" {
  grep -qE 'KUBECTL\s+—' "$SCRIPT"
}

@test "F4: docstring mentions resolve_cp_ip + KVM_HOST in CP_IP resolution" {
  grep -qE 'resolve_cp_ip' "$SCRIPT"
  grep -qE 'KVM_HOST' "$SCRIPT"
}

# ---------------------------------------------------------------------------
# Functional: KVM_HOST set, CP_IP unset → resolve_cp_ip ssh's to KVM_HOST,
# then on_cp ssh's to root@CP_IP carrying -o ProxyJump=$KVM_HOST.
# ---------------------------------------------------------------------------

@test "F4: with KVM_HOST + no CP_IP, ssh argv includes ProxyJump=KVM_HOST for on_cp" {
  _make_ssh_stub
  _make_kubectl_stub

  # KUBECTL=kubectl uses the stub directly (not the wrapper) so the
  # wrapper's KVM_HOST-required guard doesn't fire in this test.
  run env -u CONFIG \
    KVM_HOST=stub-kvm \
    KUBECTL=kubectl \
    CP_NAME=hbird-cp1 \
    PATH="${STUB_DIR}:${PATH}" \
    bash "$SCRIPT"

  # Walk every captured ssh argv. At least one of them — the on_cp call
  # to root@10.5.6.7 — must carry `-o ProxyJump=stub-kvm`. (The
  # resolve_cp_ip call also sshes but goes to KVM_HOST directly and
  # does NOT use ProxyJump; we don't want to assert it's on every
  # call, just at least one.)
  local found_proxy=0
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    # `-o` and `ProxyJump=stub-kvm` are separate argv tokens (the
    # ssh_opts_array_no_identity helper builds them as `-o`
    # `ProxyJump=stub-kvm`).
    if grep -qxF -e 'ProxyJump=stub-kvm' "$f"; then
      found_proxy=1
      break
    fi
  done
  if [ "$found_proxy" -ne 1 ]; then
    echo "FAIL: no ssh argv carried ProxyJump=stub-kvm. Captured argvs:" >&2
    for f in "${SSH_ARGV_DIR}"/argv-*; do
      [ -f "$f" ] || continue
      echo "--- $f ---" >&2
      cat "$f" >&2
    done
    return 1
  fi
}

@test "F4: with KVM_HOST + no CP_IP, resolve_cp_ip ssh's to KVM_HOST running virsh domifaddr" {
  _make_ssh_stub
  _make_kubectl_stub

  run env -u CONFIG \
    KVM_HOST=stub-kvm \
    KUBECTL=kubectl \
    CP_NAME=hbird-cp1 \
    PATH="${STUB_DIR}:${PATH}" \
    bash "$SCRIPT"

  # Find an ssh argv whose remote command was `virsh -c qemu:///system
  # domifaddr 'hbird-cp1'`. resolve_cp_ip in lib/build-common.sh issues
  # the non-sudo form first.
  local found_virsh=0
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qF "virsh -c qemu:///system domifaddr 'hbird-cp1'" "$f"; then
      found_virsh=1
      break
    fi
  done
  if [ "$found_virsh" -ne 1 ]; then
    echo "FAIL: resolve_cp_ip never sshed for the virsh domifaddr probe." >&2
    return 1
  fi
}

@test "F4: with KVM_HOST + no CP_IP, on_cp ssh targets root@<resolved-CP-IP>" {
  _make_ssh_stub
  _make_kubectl_stub

  run env -u CONFIG \
    KVM_HOST=stub-kvm \
    KUBECTL=kubectl \
    CP_NAME=hbird-cp1 \
    PATH="${STUB_DIR}:${PATH}" \
    bash "$SCRIPT"

  # resolve_cp_ip stub emits 10.5.6.7 → on_cp should ssh to root@10.5.6.7.
  local found_root=0
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qxF 'root@10.5.6.7' "$f"; then
      found_root=1
      break
    fi
  done
  if [ "$found_root" -ne 1 ]; then
    echo "FAIL: no ssh argv targeted root@10.5.6.7" >&2
    return 1
  fi
}

# ---------------------------------------------------------------------------
# Without KVM_HOST: resolve_cp_ip falls through (no virsh locally in test
# PATH), kubectl-fallback fires, on_cp does NOT carry ProxyJump.
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# #362: when we're already on the KVM host, the verifier must NOT set
# `-o ProxyJump=$KVM_HOST` — that would resolve to `ssh root@<KVM>` from
# <KVM> itself, hanging on a never-answered password prompt sshd will
# never accept. The detection drops KVM_HOST so the direct-CP SSH path
# fires instead. Gates epic #353 (bash exit code must be honest).
# ---------------------------------------------------------------------------

@test "#362: on KVM_HOST (hostname match) -> no ProxyJump in any ssh argv" {
  _make_ssh_stub
  _make_kubectl_stub

  local_short="$(hostname -s 2>/dev/null || hostname)"

  run env -u CONFIG \
    KVM_HOST="${local_short}" \
    KUBECTL=kubectl \
    CP_NAME=hbird-cp1 \
    CP_IP=192.168.99.42 \
    PATH="${STUB_DIR}:${PATH}" \
    bash "$SCRIPT"

  # No ssh argv should carry ProxyJump=… — the on-KVM-host detection
  # must have unset KVM_HOST before SSH_OPTS was built.
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qE '^ProxyJump=' "$f"; then
      echo "FAIL: ssh argv contains ProxyJump= despite on-KVM_HOST run: $f" >&2
      cat "$f" >&2
      return 1
    fi
  done

  # And the warning line must have been emitted (operator visibility).
  [[ "$output" == *"already on KVM_HOST"* ]]
  [[ "$output" == *"#362"* ]]
}

@test "#362: KVM_HOST set to a DIFFERENT host -> ProxyJump still applied (no false-positive)" {
  _make_ssh_stub
  _make_kubectl_stub

  # Synthetic alias that can't collide with any real hostname.
  run env -u CONFIG \
    KVM_HOST=definitely-not-this-host-xyzzy \
    KUBECTL=kubectl \
    CP_NAME=hbird-cp1 \
    CP_IP=192.168.99.42 \
    PATH="${STUB_DIR}:${PATH}" \
    bash "$SCRIPT"

  # At least one ssh argv must carry ProxyJump=… — the wrapper guard
  # must NOT fire when KVM_HOST is a different host.
  local found_proxy=0
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qxF 'ProxyJump=definitely-not-this-host-xyzzy' "$f"; then
      found_proxy=1
      break
    fi
  done
  [ "$found_proxy" -eq 1 ]
  if [[ "$output" == *"already on KVM_HOST"* ]]; then
    echo "FAIL: false-positive #362 skip with non-matching KVM_HOST" >&2
    return 1
  fi
}

@test "F4: without KVM_HOST, no ProxyJump option appears in any ssh argv" {
  _make_ssh_stub

  # Custom kubectl: emit a valid InternalIP so the kubectl-fallback path
  # for CP_IP resolution succeeds, then we get to on_cp.
  cat > "$STUB_DIR/kubectl" <<'EOF'
#!/usr/bin/env bash
for a in "$@"; do
  if [[ "$a" == "apply" ]]; then
    printf 'Error from server: violates PodSecurity "restricted:latest"\n'
    exit 1
  fi
done
# get nodes -o jsonpath=...InternalIP... → emit a bare IP.
for a in "$@"; do
  if [[ "$a" == "get" ]]; then
    printf '10.9.8.7'
    exit 0
  fi
done
exit 0
EOF
  chmod +x "$STUB_DIR/kubectl"

  # Strip virsh + libvirt from PATH so resolve_cp_ip's local-virsh
  # branch fails and we fall through to the kubectl path. (Without
  # KVM_HOST AND without virsh AND without CP_IP, resolve_cp_ip
  # returns rc=1 and we hit the `$KUBECTL get nodes` fallback.)
  local clean_path="" entry
  IFS=':' read -ra _parts <<<"$PATH"
  for entry in "${_parts[@]}"; do
    [[ -z "$entry" ]] && continue
    [[ -x "${entry}/virsh" ]] && continue
    if [[ -n "$clean_path" ]]; then
      clean_path="${clean_path}:${entry}"
    else
      clean_path="$entry"
    fi
  done

  run env -u CONFIG -u KVM_HOST \
    KUBECTL=kubectl \
    CP_NAME=hbird-cp1 \
    PATH="${STUB_DIR}:${clean_path}" \
    bash "$SCRIPT"

  # No ssh argv should carry ProxyJump=... — without KVM_HOST the
  # ssh_opts_array helper omits the -o ProxyJump line entirely.
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qE '^ProxyJump=' "$f"; then
      echo "FAIL: ssh argv contains ProxyJump= without KVM_HOST set: $f" >&2
      cat "$f" >&2
      return 1
    fi
  done
}

# ---------------------------------------------------------------------------
# CP_IP explicit override: short-circuits resolve_cp_ip + kubectl-fallback.
# ---------------------------------------------------------------------------

@test "F4: explicit CP_IP=<ip> bypasses resolve_cp_ip (no virsh ssh issued)" {
  _make_ssh_stub
  _make_kubectl_stub

  run env -u CONFIG \
    KVM_HOST=stub-kvm \
    KUBECTL=kubectl \
    CP_NAME=hbird-cp1 \
    CP_IP=192.168.99.99 \
    PATH="${STUB_DIR}:${PATH}" \
    bash "$SCRIPT"

  # No ssh argv should issue the virsh domifaddr probe — CP_IP override
  # short-circuits resolve_cp_ip entirely.
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qF 'virsh -c qemu:///system domifaddr' "$f"; then
      echo "FAIL: resolve_cp_ip fired despite explicit CP_IP override" >&2
      cat "$f" >&2
      return 1
    fi
  done

  # AND the on_cp ssh should target root@192.168.99.99 (the override).
  local found_override=0
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qxF 'root@192.168.99.99' "$f"; then
      found_override=1
      break
    fi
  done
  [ "$found_override" -eq 1 ]
}

# ---------------------------------------------------------------------------
# CONFIG file plumbing: when CONFIG points at a cluster.local.conf-like
# file with CP_NAME / KVM_HOST, those values must flow into the script.
# ---------------------------------------------------------------------------

@test "F4: CONFIG=<file> sourcing pulls CP_NAME + KVM_HOST" {
  _make_ssh_stub
  _make_kubectl_stub

  local cfg="$BATS_TEST_TMPDIR/cluster.conf"
  cat > "$cfg" <<EOF
CP_NAME=cfg-cp-name
KVM_HOST=cfg-kvm
EOF

  run env -u KVM_HOST -u CP_NAME \
    CONFIG="$cfg" \
    KUBECTL=kubectl \
    PATH="${STUB_DIR}:${PATH}" \
    bash "$SCRIPT"

  # The resolve_cp_ip ssh probe should target cfg-kvm and pass cfg-cp-name.
  local found=0
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qxF 'cfg-kvm' "$f" \
       && grep -qF "virsh -c qemu:///system domifaddr 'cfg-cp-name'" "$f"; then
      found=1
      break
    fi
  done
  if [ "$found" -ne 1 ]; then
    echo "FAIL: CONFIG values did not flow into resolve_cp_ip" >&2
    for f in "${SSH_ARGV_DIR}"/argv-*; do
      [ -f "$f" ] || continue
      echo "--- $f ---" >&2
      cat "$f" >&2
    done
    return 1
  fi
}

# ---------------------------------------------------------------------------
# Makefile wiring: `make verify-hardening` must pass KUBECTL pointing at
# the in-repo wrapper and forward CONFIG. (Source-level grep only — we
# don't run make in unit tests.)
# ---------------------------------------------------------------------------

@test "F4: Makefile target verify-hardening sets KUBECTL=\$(CURDIR)/scripts/kubectl-k8s.sh" {
  grep -qE 'KUBECTL=.*\$\(CURDIR\)/scripts/kubectl-k8s\.sh' \
    "${REPO_ROOT}/Makefile"
}

@test "F4: Makefile target verify-hardening forwards CONFIG=" {
  # Extract the recipe lines (indented with a TAB) immediately after the
  # `verify-hardening:` target header, stopping at the next blank line.
  # awk's range `/PATTERN1/,/PATTERN2/` is inclusive at both ends, and
  # we need the end-pattern to fire on the FIRST blank line AFTER the
  # target — set a flag the line after the target header is seen.
  awk '
    /^verify-hardening:/ { in_recipe=1; next }
    in_recipe && /^$/    { exit }
    in_recipe            { print }
  ' "${REPO_ROOT}/Makefile" \
    | grep -qE 'CONFIG="\$\(CONFIG\)"'
}

# ---------------------------------------------------------------------------
# #332: PSA-rejection apply must NOT route through the `kc`/$KUBECTL
# wrapper, because scripts/kubectl-k8s.sh sets up a port-forward tunnel
# before invoking kubectl and that setup consumes stdin from the
# heredoc — leaving kubectl with "no objects passed to apply" and the
# check failing against a correctly-hardened cluster. The fix is to go
# direct via `on_cp` (ssh root@CP "kubectl ... apply -f -"), so heredoc
# stdin flows through SSH untouched. Aligns with the Rust twin (#330)
# which already takes this shape via `cp_kubectl_with_stdin_lenient`.
# ---------------------------------------------------------------------------

@test "#332: PSA apply uses on_cp (direct SSH), NOT kc/\$KUBECTL wrapper" {
  # The `apply -f -` line in the PSA check must call on_cp, not kc.
  # Find the line that opens the `apply -f -` heredoc and assert its
  # invocation target.
  run grep -nE "apply -f -.*<<'?EOF'?" "$SCRIPT"
  [ "$status" -eq 0 ]
  # The matched line should start the invocation with on_cp, not kc.
  echo "$output" | grep -qE 'on_cp .*apply -f -.*<<'
  ! echo "$output" | grep -qE '^[0-9]+:[[:space:]]*[^#]*\bkc[[:space:]]+apply -f -'
}

@test "#332: PSA cleanup delete also goes direct via on_cp (not kc)" {
  # The follow-up `delete pod verify-hardening-privileged-probe` must
  # also bypass the wrapper. No `kc ... delete pod
  # verify-hardening-privileged-probe` anywhere in the script.
  ! grep -qE '\bkc[[:space:]].*delete pod verify-hardening-privileged-probe' \
    "$SCRIPT"
  # And on_cp invokes a kubectl delete with the probe pod name + the CP-local
  # admin kubeconfig — confirms the direct-SSH path is wired up correctly.
  grep -qE 'on_cp .*kubectl.*--kubeconfig=/etc/kubernetes/admin\.conf.*delete pod verify-hardening-privileged-probe' \
    "$SCRIPT"
}

@test "#332: docstring explains why PSA probes bypass the kc wrapper" {
  # Future maintainers reading the header must understand the heredoc
  # interaction with kubectl-k8s.sh's port-forward setup. The fix only
  # works if the rationale survives — otherwise someone will "tidy up"
  # the direct-SSH call back into the wrapper.
  grep -qE '#332' "$SCRIPT"
  grep -qE 'port-forward|heredoc|stdin' "$SCRIPT"
}

@test "#332: PSA apply heredoc stdin flows through to ssh's remote-cmd argv" {
  # End-to-end: with CP_IP set (skip resolve_cp_ip) the script's PSA
  # apply path must invoke ssh with `root@<CP_IP>` and a remote command
  # containing `kubectl … apply -f -`, AND the heredoc body bytes must
  # arrive on the stub's stdin (proving the kubectl-k8s.sh wrapper is
  # out of the path entirely).
  _make_ssh_stub_capture_stdin

  run env -u CONFIG -u KVM_HOST \
    KUBECTL=kubectl \
    CP_NAME=hbird-cp1 \
    CP_IP=192.168.99.42 \
    PATH="${STUB_DIR}:${PATH}" \
    bash "$SCRIPT"

  # Find the ssh invocation whose remote command is the PSA apply.
  local found_apply=0
  local apply_stdin_file=""
  for f in "${SSH_ARGV_DIR}"/argv-*; do
    [ -f "$f" ] || continue
    if grep -qF 'kubectl --kubeconfig=/etc/kubernetes/admin.conf apply -f -' "$f"; then
      found_apply=1
      # Same numeric suffix → stdin capture.
      local n="${f##*/argv-}"
      apply_stdin_file="${SSH_ARGV_DIR}/stdin-${n}"
      break
    fi
  done
  if [ "$found_apply" -ne 1 ]; then
    echo "FAIL: no ssh argv carried the PSA 'kubectl … apply -f -' remote cmd" >&2
    for f in "${SSH_ARGV_DIR}"/argv-*; do
      [ -f "$f" ] || continue
      echo "--- $f ---" >&2
      cat "$f" >&2
    done
    return 1
  fi
  [ -s "$apply_stdin_file" ]
  # The heredoc body must include the Pod manifest's marker fields.
  grep -qF 'verify-hardening-privileged-probe' "$apply_stdin_file"
  grep -qF 'privileged: true' "$apply_stdin_file"
}

# ssh stub variant that captures stdin alongside argv. Used by the #332
# end-to-end assertion that the PSA heredoc body actually reaches the
# remote `kubectl apply -f -`.
_make_ssh_stub_capture_stdin() {
  cat > "$STUB_DIR/ssh" <<'EOF'
#!/usr/bin/env bash
n=$(( $(cat "${SSH_ARGV_DIR}/counter" 2>/dev/null || echo 0) + 1 ))
printf '%s\n' "$n" > "${SSH_ARGV_DIR}/counter"
printf '%s\n' "$@" > "${SSH_ARGV_DIR}/argv-${n}"

remote_cmd="${!#}"
# Always drain stdin into a per-call capture file so the PSA-apply test
# can assert the heredoc body arrived. cat will block on a non-pipe
# stdin, so use a non-blocking timeout-ish read via dd with count=0?
# Simplest: read all of stdin (callers without input still close stdin).
cat > "${SSH_ARGV_DIR}/stdin-${n}" 2>/dev/null || true

if [[ "$remote_cmd" == *"virsh -c qemu:///system domifaddr"* ]]; then
  cat <<'YML'
 Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 vnet0      52:54:00:aa:bb:cc    ipv4         10.5.6.7/24
YML
  exit 0
fi
# PSA apply path: emit the PodSecurity rejection marker on stderr so the
# script's grep finds it. (Bash twin parity with apiserver behavior.)
if [[ "$remote_cmd" == *"apply -f -"* ]]; then
  printf 'Error from server (Forbidden): pods "verify-hardening-privileged-probe" is forbidden: violates PodSecurity "restricted:latest"\n' >&2
  exit 1
fi
exit 0
EOF
  chmod +x "$STUB_DIR/ssh"
}
