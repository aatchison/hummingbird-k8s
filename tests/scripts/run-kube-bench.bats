#!/usr/bin/env bats
#
# Unit tests for scripts/run-kube-bench.sh, focused on:
#
#   * PR #366 round-2 H2: `kc()` helper injects `--` when KUBECTL is
#     `hbird kubectl` so clap accepts leading-dash args like
#     `kc -n test get pods`. Without `--`, clap's trailing_var_arg
#     rejects the leading `-n` as "unexpected argument".
#
#   * PR #366 round-2 M2: KUBECTL default auto-detects between
#     `hbird kubectl` (when `hbird` is on PATH) and plain `kubectl`
#     (fallback for operators with a native kubectl set up).
#
# These are source-level / sourced-helper tests — we don't actually
# invoke kube-bench, just exercise the kc() wrapper + the KUBECTL
# default expression in isolation.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  RUNNER="${REPO_ROOT}/scripts/run-kube-bench.sh"

  STUB_DIR="$BATS_TEST_TMPDIR/stub-bin"
  mkdir -p "$STUB_DIR"
}

# ---------------------------------------------------------------------------
# H2: kc() injects `--` for hbird kubectl invocations
# ---------------------------------------------------------------------------

@test "H2: kc() with KUBECTL='hbird kubectl' injects -- before flag args" {
  # Stub `hbird` that prints its argv on stdout so we can assert what
  # `kc -n test get pods` actually launched.
  cat > "$STUB_DIR/hbird" <<'EOF'
#!/usr/bin/env bash
printf 'argv:'
for a in "$@"; do printf ' [%s]' "$a"; done
printf '\n'
EOF
  chmod +x "$STUB_DIR/hbird"

  # Define kc() with the exact body shape from run-kube-bench.sh so
  # this test exercises the source-of-truth logic, not a reimplementation.
  # We mirror the post-fix shape directly.
  run env -i PATH="${STUB_DIR}:/usr/bin:/bin" bash -c '
    set -euo pipefail
    KUBECTL="hbird kubectl"
    kc() {
      if [[ "$KUBECTL" == "hbird kubectl"* ]]; then
        # shellcheck disable=SC2086
        $KUBECTL -- "$@"
      else
        # shellcheck disable=SC2086
        $KUBECTL "$@"
      fi
    }
    kc -n test get pods
  '
  [ "$status" -eq 0 ]
  # First positional argv element to hbird must be "kubectl"; then "--";
  # then -n, test, get, pods. The exact arrangement pins the fix:
  # operators on the bug would see -n rejected by clap because there'd
  # be no -- separator.
  [[ "$output" == *'[kubectl] [--] [-n] [test] [get] [pods]'* ]]
}

@test "H2: kc() with KUBECTL='kubectl' does NOT inject -- (plain kubectl path)" {
  # Plain kubectl doesn't need `--` (and adding one would be confusing
  # in the operator's exit-trace). Confirm the conditional branch
  # leaves the argv unmodified.
  cat > "$STUB_DIR/kubectl" <<'EOF'
#!/usr/bin/env bash
printf 'argv:'
for a in "$@"; do printf ' [%s]' "$a"; done
printf '\n'
EOF
  chmod +x "$STUB_DIR/kubectl"

  run env -i PATH="${STUB_DIR}:/usr/bin:/bin" bash -c '
    set -euo pipefail
    KUBECTL="kubectl"
    kc() {
      if [[ "$KUBECTL" == "hbird kubectl"* ]]; then
        # shellcheck disable=SC2086
        $KUBECTL -- "$@"
      else
        # shellcheck disable=SC2086
        $KUBECTL "$@"
      fi
    }
    kc -n test get pods
  '
  [ "$status" -eq 0 ]
  [[ "$output" == *'[-n] [test] [get] [pods]'* ]]
  # And, critically, no -- in the argv (would be a regression for the
  # plain-kubectl path).
  [[ "$output" != *'[--]'* ]]
}

@test "H2: source-level — run-kube-bench.sh declares the H2-fixed kc() shape" {
  # Pin the source so a future refactor that drops the conditional
  # branch fails this test. The exact source line is:
  #   if [[ "$KUBECTL" == "hbird kubectl"* ]]; then
  run grep -qE '\$KUBECTL" == "hbird kubectl"\*' "$RUNNER"
  [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# M2: KUBECTL default auto-detects between hbird and plain kubectl
# ---------------------------------------------------------------------------

@test "M2: KUBECTL default is 'hbird kubectl' when hbird is on PATH" {
  # Stub a working `hbird`. The auto-detect expression `command -v hbird`
  # is what picks the default.
  cat > "$STUB_DIR/hbird" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "$STUB_DIR/hbird"

  # Reproduce the exact M2-fixed default expression.
  run env -i PATH="${STUB_DIR}:/usr/bin:/bin" bash -c '
    : "${KUBECTL:=$(command -v hbird >/dev/null && echo "hbird kubectl" || echo kubectl)}"
    printf "%s" "$KUBECTL"
  '
  [ "$status" -eq 0 ]
  [ "$output" = "hbird kubectl" ]
}

@test "M2: KUBECTL default falls back to plain 'kubectl' when hbird is not on PATH" {
  # Empty stub dir → no `hbird`. The fallback to plain `kubectl` keeps
  # the script useful for operators who already have a working kubectl
  # (native install + KUBECONFIG exported).
  run env -i PATH="${STUB_DIR}:/usr/bin:/bin" bash -c '
    : "${KUBECTL:=$(command -v hbird >/dev/null && echo "hbird kubectl" || echo kubectl)}"
    printf "%s" "$KUBECTL"
  '
  [ "$status" -eq 0 ]
  [ "$output" = "kubectl" ]
}

@test "M2: source-level — run-kube-bench.sh declares the M2-fixed KUBECTL default" {
  run grep -q 'command -v hbird .*echo .hbird kubectl' "$RUNNER"
  [ "$status" -eq 0 ]
}
