#!/usr/bin/env bats
#
# Unit tests for scripts/verify-app-deploy.sh — issue #271 F3.
#
# Issue #271 F3: when KVM_HOST is set, the verifier must default KUBECTL to
# the in-repo scripts/kubectl-k8s.sh wrapper so workstation operators get a
# tunneled kubectl without spelling it out on every invocation. The script
# must still honor an explicit KUBECTL=… override (the legacy contract from
# its top-of-file doc comment), and must fall back to plain `kubectl` on
# PATH when KVM_HOST is unset (the on-CP-host / on-runner contract used by
# tests/integration-boot.sh).
#
# We stub kubectl (and the kubectl-k8s.sh wrapper) on PATH so no real
# kubectl call escapes. The stubs write each invocation's argv0 + argv to
# $KCTL_LOG so the test can assert which command the script actually
# executed.
#
# Notes:
#   * The script creates a namespace `smoketest-$(date +%s)` and applies a
#     Deployment + Service + Pod. Our stubs accept ANY argv and emit canned
#     output ("Welcome to nginx" for the wget probe step) so the script
#     reaches the PASS branch deterministically.
#   * The kubectl-k8s.sh wrapper is shadowed by a stub at the same relative
#     path inside a per-test temp REPO_ROOT — `verify-app-deploy.sh` reads
#     readlink -f "$0" to find its sibling, so we copy the real script into
#     a temp tree and drop the stub alongside it.

setup() {
  REAL_REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT_NAME="verify-app-deploy.sh"
  REAL_SCRIPT="${REAL_REPO_ROOT}/scripts/${SCRIPT_NAME}"

  # Build a fake repo layout in TMPDIR with the real script + a stubbed
  # kubectl-k8s.sh sibling. readlink -f resolves the script path through
  # this directory, so `$(dirname …)/..` lands in $FAKE_REPO_ROOT.
  FAKE_REPO_ROOT="$BATS_TEST_TMPDIR/repo"
  mkdir -p "${FAKE_REPO_ROOT}/scripts"
  cp "$REAL_SCRIPT" "${FAKE_REPO_ROOT}/scripts/${SCRIPT_NAME}"

  # Stub kubectl-k8s.sh: a no-op that logs its argv to $KCTL_LOG with a
  # leading marker so assertions can distinguish "ran via the wrapper"
  # from "ran via plain kubectl on PATH".
  KCTL_LOG="$BATS_TEST_TMPDIR/kctl.log"
  : > "$KCTL_LOG"
  export KCTL_LOG

  cat > "${FAKE_REPO_ROOT}/scripts/kubectl-k8s.sh" <<'EOF'
#!/usr/bin/env bash
# Stub kubectl-k8s.sh. Log invocation marker + argv to $KCTL_LOG and
# emit the canned output the verifier's PASS branch expects.
printf 'WRAPPER %s\n' "$*" >> "$KCTL_LOG"
# `run` (the busybox probe step) is the only call whose stdout the
# verifier inspects — it grep's for "Welcome to nginx". Match that.
case "$1 $2" in
  *"run probe"*) printf 'Welcome to nginx!\n' ;;
esac
exit 0
EOF
  chmod +x "${FAKE_REPO_ROOT}/scripts/kubectl-k8s.sh"

  # Stub plain `kubectl` on PATH. Same logging pattern with a "PLAIN"
  # marker so we can distinguish the two code paths.
  STUB_DIR="$BATS_TEST_TMPDIR/stub-bin"
  mkdir -p "$STUB_DIR"
  cat > "$STUB_DIR/kubectl" <<'EOF'
#!/usr/bin/env bash
printf 'PLAIN %s\n' "$*" >> "$KCTL_LOG"
# Same: emit the canned welcome page for the busybox-probe step so the
# verifier reaches its PASS branch.
case "$1 $2 $3" in
  *"run probe"*) printf 'Welcome to nginx!\n' ;;
esac
exit 0
EOF
  chmod +x "$STUB_DIR/kubectl"

  SCRIPT="${FAKE_REPO_ROOT}/scripts/${SCRIPT_NAME}"
}

# Stub `hostname` on PATH to print a fixed value, so on-KVM_HOST
# detection tests are hermetic and don't couple to the runner's actual
# hostname. Honors `hostname -s` (the form the script invokes) by
# printing the same fixed value either way. Tests that want to assert
# "no match" deliberately pass a synthetic KVM_HOST and don't need this.
_stub_hostname() {
  local fake="${1:-geary}"
  cat > "${STUB_DIR}/hostname" <<EOF
#!/usr/bin/env bash
# Stub: always prints the fixed test hostname, ignoring flags.
printf '%s\n' '${fake}'
EOF
  chmod +x "${STUB_DIR}/hostname"
}

# Invoke the script with the stubs in PATH. Each test passes the env it
# cares about (KVM_HOST, KUBECTL, CONFIG).
_invoke() {
  PATH="${STUB_DIR}:${PATH}" bash "$SCRIPT" "$@"
}

# ---------------------------------------------------------------------------
# F3: when KVM_HOST is set, default KUBECTL to scripts/kubectl-k8s.sh
# ---------------------------------------------------------------------------

@test "verify-app-deploy: KVM_HOST set -> defaults KUBECTL to kubectl-k8s.sh wrapper (#271 F3)" {
  run env -i HOME="$HOME" PATH="${STUB_DIR}:${PATH}" \
      KVM_HOST=stub-kvm KCTL_LOG="$KCTL_LOG" \
      bash "$SCRIPT"
  [ "$status" -eq 0 ]
  # The verifier must have routed every kubectl call through the WRAPPER
  # stub — NOT plain `kubectl` on PATH.
  grep -q '^WRAPPER ' "$KCTL_LOG"
  if grep -q '^PLAIN ' "$KCTL_LOG"; then
    printf 'FAIL: verifier called plain kubectl despite KVM_HOST set\n%s\n' "$(cat "$KCTL_LOG")"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# Legacy contract: no KVM_HOST -> plain kubectl on PATH (on-CP-host path)
# ---------------------------------------------------------------------------

@test "verify-app-deploy: KVM_HOST unset -> falls back to plain kubectl (on-CP-host contract)" {
  run env -i HOME="$HOME" PATH="${STUB_DIR}:${PATH}" \
      KCTL_LOG="$KCTL_LOG" \
      bash "$SCRIPT"
  [ "$status" -eq 0 ]
  # Should have used plain kubectl, NOT the wrapper.
  grep -q '^PLAIN ' "$KCTL_LOG"
  if grep -q '^WRAPPER ' "$KCTL_LOG"; then
    printf 'FAIL: verifier used kubectl-k8s.sh wrapper despite KVM_HOST unset\n%s\n' "$(cat "$KCTL_LOG")"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# Explicit KUBECTL override beats the KVM_HOST default
# ---------------------------------------------------------------------------

@test "verify-app-deploy: explicit KUBECTL= override wins even when KVM_HOST set" {
  # Use the plain-kubectl stub as the explicit override and confirm the
  # wrapper is NOT used despite KVM_HOST being set.
  run env -i HOME="$HOME" PATH="${STUB_DIR}:${PATH}" \
      KVM_HOST=stub-kvm KUBECTL=kubectl KCTL_LOG="$KCTL_LOG" \
      bash "$SCRIPT"
  [ "$status" -eq 0 ]
  grep -q '^PLAIN ' "$KCTL_LOG"
  if grep -q '^WRAPPER ' "$KCTL_LOG"; then
    printf 'FAIL: explicit KUBECTL override was ignored\n%s\n' "$(cat "$KCTL_LOG")"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# CONFIG sourcing: a cluster.local.conf that sets KVM_HOST flips the
# default to the wrapper, same as exporting KVM_HOST directly.
# ---------------------------------------------------------------------------

@test "verify-app-deploy: CONFIG with KVM_HOST=... opts into the tunneled wrapper" {
  CONFIG_FILE="$BATS_TEST_TMPDIR/cluster.local.conf"
  cat > "$CONFIG_FILE" <<'EOF'
KVM_HOST=stub-kvm-from-config
CP_NAME=hbird-cp1
EOF
  run env -i HOME="$HOME" PATH="${STUB_DIR}:${PATH}" \
      CONFIG="$CONFIG_FILE" KCTL_LOG="$KCTL_LOG" \
      bash "$SCRIPT"
  [ "$status" -eq 0 ]
  grep -q '^WRAPPER ' "$KCTL_LOG"
}

@test "verify-app-deploy: CONFIG unreadable -> non-zero exit with clear diagnostic" {
  run env -i HOME="$HOME" PATH="${STUB_DIR}:${PATH}" \
      CONFIG="$BATS_TEST_TMPDIR/does-not-exist.conf" KCTL_LOG="$KCTL_LOG" \
      bash "$SCRIPT"
  [ "$status" -ne 0 ]
  [[ "$output" == *"CONFIG not readable"* ]]
}

# ---------------------------------------------------------------------------
# Regression guard: the path-anchored sibling-resolution line is present.
# Catches a future refactor that drops $REPO_ROOT and re-introduces a bare
# relative `./scripts/kubectl-k8s.sh` (issue #271 F6 anti-pattern).
# ---------------------------------------------------------------------------

@test "verify-app-deploy: source resolves kubectl-k8s.sh via REPO_ROOT (no relative-path assignment)" {
  grep -qF 'REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"' "$REAL_SCRIPT"
  grep -qF '${REPO_ROOT}/scripts/kubectl-k8s.sh' "$REAL_SCRIPT"
  # And confirm we never ASSIGN the wrapper as a bare relative path in
  # active code (a relative ./scripts/… in a comment example block is
  # fine; an assignment is not).
  if grep -qE '^[^#]*KUBECTL=["'\'']?\./scripts/kubectl-k8s\.sh' "$REAL_SCRIPT"; then
    printf 'FAIL: verify-app-deploy.sh assigns KUBECTL=./scripts/kubectl-k8s.sh (relative)\n'
    return 1
  fi
}

# ---------------------------------------------------------------------------
# #362: when we're already on the KVM host, the verifier MUST NOT route
# through scripts/kubectl-k8s.sh — that would resolve to `ssh root@<KVM>`
# from <KVM> itself, hanging on a password prompt sshd will never accept.
# Instead the script should log a skip + exit 0, so deploy-cluster.sh's
# trailing verify step reports honest exit codes (gates epic #353).
# ---------------------------------------------------------------------------

@test "verify-app-deploy: on KVM_HOST (hostname match) -> skips with exit 0 and warning (#362)" {
  # Hermetic: stub hostname so the test passes deterministically on any
  # CI runner / workstation regardless of the real hostname.
  _stub_hostname geary
  run env -i HOME="$HOME" PATH="${STUB_DIR}:${PATH}" \
      KVM_HOST=geary KCTL_LOG="$KCTL_LOG" \
      bash "$SCRIPT"
  [ "$status" -eq 0 ]
  # Confirm NEITHER kubectl path ran — the skip must fire BEFORE any
  # smoketest namespace is created.
  if grep -q '^WRAPPER ' "$KCTL_LOG"; then
    printf 'FAIL: on-KVM_HOST run still invoked kubectl-k8s.sh wrapper\n%s\n' "$(cat "$KCTL_LOG")"
    return 1
  fi
  if grep -q '^PLAIN ' "$KCTL_LOG"; then
    printf 'FAIL: on-KVM_HOST run still invoked plain kubectl\n%s\n' "$(cat "$KCTL_LOG")"
    return 1
  fi
  [[ "$output" == *"already on KVM_HOST"* ]]
  [[ "$output" == *"#362"* ]]
}

@test "verify-app-deploy: on KVM_HOST FQDN form (hostname.example.com) also skips (#362)" {
  # Hermetic stub: real hostname doesn't matter, only the stubbed value.
  _stub_hostname geary
  # The detection strips KVM_HOST to its short form (everything before
  # the first dot), so an FQDN like "geary.example.com" still matches
  # the stubbed "geary".
  run env -i HOME="$HOME" PATH="${STUB_DIR}:${PATH}" \
      KVM_HOST=geary.example.com KCTL_LOG="$KCTL_LOG" \
      bash "$SCRIPT"
  [ "$status" -eq 0 ]
  if grep -q '^WRAPPER \|^PLAIN ' "$KCTL_LOG"; then
    printf 'FAIL: FQDN-form on-KVM_HOST run did not skip\n%s\n' "$(cat "$KCTL_LOG")"
    return 1
  fi
  [[ "$output" == *"already on KVM_HOST"* ]]
}

@test "verify-app-deploy: KVM_HOST set to a DIFFERENT host -> still uses wrapper (no false-positive skip) (#362)" {
  # Sanity check: a hostname that does NOT match ours must not trigger
  # the skip path. Use a synthetic name that can't collide with any
  # real local hostname.
  run env -i HOME="$HOME" PATH="${STUB_DIR}:${PATH}" \
      KVM_HOST=definitely-not-this-host-xyzzy KCTL_LOG="$KCTL_LOG" \
      bash "$SCRIPT"
  [ "$status" -eq 0 ]
  grep -q '^WRAPPER ' "$KCTL_LOG"
  # Positive marker: the script must have progressed past the on-host
  # skip-guard into the smoketest body. Both apply/create are issued by
  # the WRAPPER stub's logger; without this assertion the test would
  # pass even if the script bailed early, since `bash $SCRIPT` returns
  # the stub's exit 0. Grep for evidence of real execution.
  if ! grep -qE 'WRAPPER (apply|create) ' "$KCTL_LOG"; then
    printf 'FAIL: kubectl-k8s.sh wrapper never saw apply/create — script did not progress past skip-guard\n%s\n' "$(cat "$KCTL_LOG")"
    return 1
  fi
  if [[ "$output" == *"already on KVM_HOST"* ]]; then
    printf 'FAIL: false-positive skip when KVM_HOST != local hostname\n%s\n' "$output"
    return 1
  fi
}
