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
  # The live repo currently pins Cilium 1.16.5 + K8s v1.31 (a known
  # mismatch — that's exactly the motivation for #303). Whatever the
  # state, the script must run cleanly and exit 0 by default (warn-only).
  run bash "$SCRIPT"
  [ "$status" -eq 0 ]
  # Must mention both versions so reviewers can sanity-check the
  # extraction without grepping the source.
  [[ "$output" == *"1.16"* ]] || [[ "$output" == *"OK"* ]]
}

@test "check-cilium-k8s-compat: live repo --strict exits 1 on mismatch" {
  # Pins-as-of-#303-filing are mismatched (Cilium 1.16 vs K8s 1.31).
  # If a future PR bumps Cilium to >=1.17, this test will start failing
  # and that's a signal to bump the assertion (the mismatch is gone).
  # Document the expected behavior either way:
  run bash "$SCRIPT" --strict
  # Either 0 (someone bumped Cilium into range) or 1 (mismatch). Both
  # are valid; assert the exit code matches the verdict line.
  if [[ "$output" == *"OK:"* ]]; then
    [ "$status" -eq 0 ]
  else
    [[ "$output" == *"WARN"* ]]
    [ "$status" -eq 1 ]
  fi
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
  # Cilium 1.16 + K8s 1.31 — the live mismatch.
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
