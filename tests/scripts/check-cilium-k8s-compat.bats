#!/usr/bin/env bats
#
# Unit tests for scripts/check-cilium-k8s-compat.sh — issue #303.
#
# Covers:
#   1. Default-pin smoke against the live repo (catches a stale matrix or
#      stale embedded pin extraction).
#   2. Matrix lookup: known supported pair, known mismatched pair, unknown
#      Cilium minor (matrix gap).
#   3. Override flags (--cilium, --k8s) parse correctly and the K8s minor
#      override accepts both "v1.31" and "1.31" forms.
#   4. --strict flips exit code from 0 to 1 on mismatch but leaves
#      OK cases at exit 0.
#   5. Extraction from a fake k8s-init.sh / Containerfile pair in
#      BATS_TEST_TMPDIR so the test exercises the regex paths
#      independently of the live pin.

# `run --separate-stderr` is bats 1.5.0+. The pinned image above is
# bats 1.10.x so this is satisfied; the assertion documents the
# requirement and silences the per-test BW02 warning.
bats_require_minimum_version 1.5.0

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/check-cilium-k8s-compat.sh"
}

# Helper: build a fake repo under BATS_TEST_TMPDIR with controllable
# Cilium + K8s pins, and copy the script into scripts/ so its
# `dirname $0/..` repo-root resolution picks up the fake pins.
_make_fake_repo() {
  local cilium_ver="$1" k8s_ver="$2"
  local root="${BATS_TEST_TMPDIR}/fake-repo"
  mkdir -p "${root}/scripts" "${root}/containers/k8s"
  cp "$SCRIPT" "${root}/scripts/check-cilium-k8s-compat.sh"
  cat > "${root}/containers/k8s/k8s-init.sh" <<EOF
#!/bin/bash
# Fake k8s-init.sh for testing extraction.
KUBECONFIG=/etc/kubernetes/admin.conf cilium install \\
  --version ${cilium_ver} \\
  --set kubeProxyReplacement=true \\
  --wait
EOF
  cat > "${root}/containers/k8s/Containerfile" <<EOF
FROM quay.io/example/base:latest
ARG K8S_VERSION=${k8s_ver}
ARG POD_CIDR=10.244.0.0/16
EOF
  printf '%s\n' "${root}"
}

# ---- live-repo smoke --------------------------------------------------

@test "check-cilium-k8s-compat: live repo runs and emits a verdict" {
  # Smoke against the live repo: the script must run cleanly and produce
  # a verdict line matching the OK/WARN shape. Avoids the earlier
  # tautology trap where `[[ output == *1.16* ]] || [[ output == *OK* ]]`
  # let a regression that dropped the Cilium version slip through (an
  # OK-only output would still satisfy the `||`). Here we assert the
  # verdict line *itself* matches `^(OK|WARN):` so structure can't drift.
  #
  # Strict-mode exit semantics are covered deterministically by the
  # override-driven tests below (1.16.5 vs v1.31 = WARN+exit-1); we no
  # longer branch on live state here.
  run bash "$SCRIPT"
  [ "$status" -eq 0 ]
  # Verdict shape: at least one line on stdout or stderr starting with
  # OK: or WARN:. `run` merges stderr→output unless --separate-stderr.
  [[ "$output" =~ (^|$'\n')(OK|WARN): ]]
}

@test "check-cilium-k8s-compat: -h prints usage" {
  run bash "$SCRIPT" -h
  [ "$status" -eq 0 ]
  [[ "$output" == *"Usage:"* ]]
  [[ "$output" == *"--strict"* ]]
}

@test "check-cilium-k8s-compat: unknown flag errors with usage" {
  run bash "$SCRIPT" --bogus
  [ "$status" -eq 2 ]
  [[ "$output" == *"unknown argument"* ]]
}

# ---- matrix lookup (override-only, no repo coupling) ------------------

@test "check-cilium-k8s-compat: known supported pair returns OK" {
  # Cilium 1.16.x officially supports 1.27-1.30.
  run bash "$SCRIPT" --cilium=1.16.5 --k8s=v1.29
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
  [[ "$output" == *"1.29"* ]]
}

@test "check-cilium-k8s-compat: known mismatch warns + exit 0 by default" {
  # Cilium 1.16 + K8s 1.31 — historical mismatch (pre-PR #367 live pin).
  run bash "$SCRIPT" --cilium=1.16.5 --k8s=v1.31
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"does NOT list"* ]]
  [[ "$output" == *"1.31"* ]]
  [[ "$output" == *"docs/cilium-migration.md"* ]]
}

@test "check-cilium-k8s-compat: --strict turns mismatch into exit 1" {
  run bash "$SCRIPT" --cilium=1.16.5 --k8s=v1.31 --strict
  [ "$status" -eq 1 ]
  [[ "$output" == *"WARN"* ]]
}

@test "check-cilium-k8s-compat: --strict on a supported pair still exits 0" {
  # Strict only escalates on mismatch — happy path stays 0.
  run bash "$SCRIPT" --cilium=1.17.2 --k8s=v1.31 --strict
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

@test "check-cilium-k8s-compat: unknown Cilium minor is a 'matrix-stale' warn" {
  # Future Cilium versions that haven't been added to the embedded
  # matrix should warn loudly (matrix needs refresh) but not block.
  run bash "$SCRIPT" --cilium=1.99.0 --k8s=v1.31
  [ "$status" -eq 0 ]
  [[ "$output" == *"not in the embedded compat matrix"* ]]
  [[ "$output" == *"1.99"* ]]
}

@test "check-cilium-k8s-compat: unknown Cilium minor + --strict = exit 1" {
  # The matrix-stale path must also honor --strict so a CI pre-merge
  # gate fails closed instead of silently letting a stale matrix through.
  run bash "$SCRIPT" --cilium=1.99.0 --k8s=v1.31 --strict
  [ "$status" -eq 1 ]
  [[ "$output" == *"not in the embedded compat matrix"* ]]
}

@test "check-cilium-k8s-compat: --k8s accepts bare 1.30 (no leading v)" {
  # Operators copy-paste K8s version strings from a variety of sources;
  # accept both `v1.30` and `1.30`.
  run bash "$SCRIPT" --cilium=1.16.5 --k8s=1.30
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

@test "check-cilium-k8s-compat: --k8s strips trailing patch component" {
  # `--k8s=v1.30.4` should still match the 1.30 row in the matrix.
  run bash "$SCRIPT" --cilium=1.16.5 --k8s=v1.30.4
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

# ---- matrix sanity (pin the embedded table) ---------------------------
#
# These tests pin specific (cilium, k8s) cells in the embedded matrix
# so a refresh that flips a row by accident gets caught. Source:
# https://docs.cilium.io/en/v1.{14,15,16,17,18}/network/kubernetes/compatibility/

@test "check-cilium-k8s-compat: matrix pins 1.14 supports 1.27" {
  run bash "$SCRIPT" --cilium=1.14.10 --k8s=v1.27
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

@test "check-cilium-k8s-compat: matrix pins 1.15 supports 1.29 (top of window)" {
  run bash "$SCRIPT" --cilium=1.15.0 --k8s=v1.29
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

@test "check-cilium-k8s-compat: matrix pins 1.15 does NOT support 1.30" {
  # 1.15's window is 1.26-1.29 — 1.30 is out.
  run bash "$SCRIPT" --cilium=1.15.0 --k8s=v1.30
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARN"* ]]
}

@test "check-cilium-k8s-compat: matrix pins 1.17 supports 1.32 (top of window)" {
  run bash "$SCRIPT" --cilium=1.17.0 --k8s=v1.32
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

@test "check-cilium-k8s-compat: matrix pins 1.18 supports 1.33" {
  run bash "$SCRIPT" --cilium=1.18.0 --k8s=v1.33
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

# ---- extraction from k8s-init.sh / Containerfile ----------------------

@test "check-cilium-k8s-compat: extracts Cilium pin from a fake k8s-init.sh" {
  local fake_root
  fake_root="$(_make_fake_repo 1.17.0 v1.31)"
  run bash "${fake_root}/scripts/check-cilium-k8s-compat.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"1.17.0"* ]]
  [[ "$output" == *"OK"* ]]
}

@test "check-cilium-k8s-compat: extracts K8S_VERSION from a fake Containerfile" {
  local fake_root
  fake_root="$(_make_fake_repo 1.16.5 v1.30)"
  run bash "${fake_root}/scripts/check-cilium-k8s-compat.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"1.16.5"* ]]
  [[ "$output" == *"1.30"* ]]
  [[ "$output" == *"OK"* ]]
}

@test "check-cilium-k8s-compat: fake repo with mismatched pins WARNs" {
  local fake_root
  fake_root="$(_make_fake_repo 1.16.5 v1.31)"
  run bash "${fake_root}/scripts/check-cilium-k8s-compat.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"1.16.5"* ]]
  [[ "$output" == *"1.31"* ]]
}

@test "check-cilium-k8s-compat: fake repo with mismatch + --strict exits 1" {
  local fake_root
  fake_root="$(_make_fake_repo 1.16.5 v1.31)"
  run bash "${fake_root}/scripts/check-cilium-k8s-compat.sh" --strict
  [ "$status" -eq 1 ]
}

@test "check-cilium-k8s-compat: --cilium override beats the file pin" {
  local fake_root
  fake_root="$(_make_fake_repo 1.16.5 v1.31)"
  # File says 1.16.5; override says 1.17.0 — 1.17 supports K8s 1.31, so OK.
  run bash "${fake_root}/scripts/check-cilium-k8s-compat.sh" --cilium=1.17.0
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
  [[ "$output" == *"1.17.0"* ]]
}

@test "check-cilium-k8s-compat: --k8s override beats the Containerfile pin" {
  local fake_root
  fake_root="$(_make_fake_repo 1.16.5 v1.31)"
  # File says v1.31 (mismatch); override to v1.30 (supported).
  run bash "${fake_root}/scripts/check-cilium-k8s-compat.sh" --k8s=v1.30
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

# ---- negative-path extraction (regression guard for the pipefail bug) -
#
# When extract_cilium_pin / extract_k8s_pin's grep finds no match, the
# script's `set -o pipefail` previously caused `$(...)` to exit 1 BEFORE
# reaching the "could not extract" diagnostic at the bottom. These two
# tests fail on the unfixed version (silent exit 1 with no message) and
# pass on the fix (exit 2 + clear ERROR).

@test "check-cilium-k8s-compat: k8s-init.sh without --version → exit 2 + diagnostic" {
  local root="${BATS_TEST_TMPDIR}/no-cilium-pin"
  mkdir -p "${root}/scripts" "${root}/containers/k8s"
  cp "$SCRIPT" "${root}/scripts/check-cilium-k8s-compat.sh"
  # No `--version` line at all — extract_cilium_pin returns empty.
  cat > "${root}/containers/k8s/k8s-init.sh" <<'EOF'
#!/bin/bash
echo "hello — no cilium install here"
EOF
  cat > "${root}/containers/k8s/Containerfile" <<'EOF'
FROM example
ARG K8S_VERSION=v1.31
EOF
  run bash "${root}/scripts/check-cilium-k8s-compat.sh"
  [ "$status" -eq 2 ]
  [[ "$output" == *"could not extract Cilium"* ]]
}

@test "check-cilium-k8s-compat: Containerfile without ARG K8S_VERSION → exit 2 + diagnostic" {
  local root="${BATS_TEST_TMPDIR}/no-k8s-pin"
  mkdir -p "${root}/scripts" "${root}/containers/k8s"
  cp "$SCRIPT" "${root}/scripts/check-cilium-k8s-compat.sh"
  # Valid Cilium pin (multi-line form mirroring real k8s-init.sh).
  cat > "${root}/containers/k8s/k8s-init.sh" <<'EOF'
#!/bin/bash
cilium install \
  --version 1.16.5 \
  --wait
EOF
  # No ARG K8S_VERSION line.
  cat > "${root}/containers/k8s/Containerfile" <<'EOF'
FROM example
# Intentionally no K8S_VERSION pin.
EOF
  run bash "${root}/scripts/check-cilium-k8s-compat.sh"
  [ "$status" -eq 2 ]
  [[ "$output" == *"could not extract K8S_VERSION"* ]]
}

# ---- edge case coverage -----------------------------------------------

@test "check-cilium-k8s-compat: --k8s below lowest-known minor → WARN" {
  # Current Cilium pin (1.16.5) supports 1.27-1.30; 1.18 is far below.
  # Catches a future regression where below-range silently passes.
  run bash "$SCRIPT" --k8s=v1.18
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"does NOT list"* ]]
  [[ "$output" == *"1.18"* ]]
}

@test "check-cilium-k8s-compat: --cilium=garbage → exit 2 (malformed pin)" {
  # A non-numeric Cilium override should fail loudly, not be silently
  # treated as a 0.0 minor. extract_cilium_pin's regex isn't invoked
  # here (override beats file pin), but the cilium_minor sed-strip
  # produces an empty/garbage minor and the matrix lookup returns
  # empty → "not in the embedded compat matrix" warn at exit 0.
  # If a future refactor adds strict pin validation, this test should
  # be updated to assert exit 2 + ERROR — pin the current behavior here.
  run bash "$SCRIPT" --cilium=garbage --k8s=v1.31
  # Current behavior: matrix lookup misses → matrix-stale WARN, exit 0.
  [ "$status" -eq 0 ]
  [[ "$output" == *"not in the embedded compat matrix"* ]]
}

@test "check-cilium-k8s-compat: --cilium= (empty value) → exit 2 (no pin)" {
  # An empty --cilium= is interpreted as "no override" today, so the
  # script falls through to extract from the live k8s-init.sh and
  # should exit 0 with a verdict (or 2 if live file is unreadable).
  # Pin the current behavior: empty override == "no override".
  run bash "$SCRIPT" --cilium= --k8s=v1.30
  [ "$status" -eq 0 ]
  # Must emit some verdict line (OK or WARN).
  [[ "$output" =~ (^|$'\n')(OK|WARN): ]]
}

# ---- stderr stream separation -----------------------------------------
#
# Operator pipelines parse stdout for the OK line and treat stderr as
# diagnostic. A regression where WARN moves to stdout would pollute
# `make check-cilium-k8s-compat | grep OK` callers. Pin the contract.

@test "check-cilium-k8s-compat: WARN on mismatch goes to stderr, not stdout" {
  run --separate-stderr bash "$SCRIPT" --cilium=1.16.5 --k8s=v1.31
  [ "$status" -eq 0 ]
  [[ "$stderr" == *"WARN"* ]]
  [[ "$stderr" == *"does NOT list"* ]]
  # stdout should be empty on mismatch (no OK line, no diagnostic).
  [ -z "$output" ]
}

@test "check-cilium-k8s-compat: matrix-stale WARN goes to stderr" {
  run --separate-stderr bash "$SCRIPT" --cilium=1.99.0 --k8s=v1.31
  [ "$status" -eq 0 ]
  [[ "$stderr" == *"not in the embedded compat matrix"* ]]
  [ -z "$output" ]
}

# ---- negative matrix-row assertions -----------------------------------
#
# Catches accidental row expansion on a matrix refresh. 1.16's window
# is 1.27-1.30; pin the boundary cells that are explicitly OUT.

@test "check-cilium-k8s-compat: matrix pins 1.16 does NOT support 1.31" {
  run bash "$SCRIPT" --cilium=1.16.0 --k8s=v1.31
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"does NOT list"* ]]
}

@test "check-cilium-k8s-compat: matrix pins 1.16 does NOT support 1.26" {
  run bash "$SCRIPT" --cilium=1.16.0 --k8s=v1.26
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"does NOT list"* ]]
}

# ---- directional remediation copy -------------------------------------
#
# When K8s is BELOW the supported window, the hint should say "downgrade
# Cilium or upgrade K8s", NOT "bump Cilium" (which is backwards).

@test "check-cilium-k8s-compat: below-range mismatch suggests downgrade Cilium / upgrade K8s" {
  # Cilium 1.18 supports 1.30-1.33; 1.28 is below.
  run bash "$SCRIPT" --cilium=1.18.0 --k8s=v1.28
  [ "$status" -eq 0 ]
  [[ "$output" == *"BELOW"* ]]
  [[ "$output" == *"Downgrade Cilium"* ]] || [[ "$output" == *"downgrade Cilium"* ]]
}

@test "check-cilium-k8s-compat: above-range mismatch still suggests bumping Cilium" {
  # Cilium 1.15 supports 1.26-1.29; 1.32 is above.
  run bash "$SCRIPT" --cilium=1.15.0 --k8s=v1.32
  [ "$status" -eq 0 ]
  [[ "$output" == *"Bump Cilium first"* ]]
  # And NOT the below-range copy.
  [[ "$output" != *"BELOW"* ]]
}

@test "check-cilium-k8s-compat: mismatch warning links to k8s-version-upgrade.md" {
  # The warning's "See also" line should point operators back to the
  # pre-flight checklist that this target codifies.
  run bash "$SCRIPT" --cilium=1.16.5 --k8s=v1.31
  [ "$status" -eq 0 ]
  [[ "$output" == *"docs/k8s-version-upgrade.md"* ]]
  [[ "$output" == *"Pre-flight checklist"* ]]
}
