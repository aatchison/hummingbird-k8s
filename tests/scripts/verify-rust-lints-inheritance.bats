#!/usr/bin/env bats
#
# Unit tests for scripts/verify-rust-lints-inheritance.sh — PR #314 round-2
# review Lens 5 HIGH (missing bats coverage for a new scripts/ artifact
# violates the project convention; every other scripts/*.sh has a paired
# tests/scripts/*.bats).
#
# The script scans rust/crates/*/Cargo.toml and exits 1 if any crate
# omits the `[lints]` table with `workspace = true`. These tests build
# fake repo layouts in BATS_TEST_TMPDIR and run the script against them
# so the real repo state can't drift the test (and vice versa).

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/verify-rust-lints-inheritance.sh"
  FAKE_REPO="${BATS_TEST_TMPDIR}/fake-repo"
  mkdir -p "${FAKE_REPO}/scripts"
  # The script computes repo_root as $(dirname $0)/.. — symlink in a
  # copy so the relative resolution works from the fake-repo location.
  cp "$SCRIPT" "${FAKE_REPO}/scripts/verify-rust-lints-inheritance.sh"
}

# Helper: write a Cargo.toml at the given crate path with the given
# `[lints]` stanza shape. Shapes:
#   inherited  — `[lints]\nworkspace = true`
#   missing    — no `[lints]` table at all
#   sub_only   — only `[lints.rust]` (NOT the bare `[lints]` table)
#   no_workspace — `[lints]` table but `workspace = false`
_make_crate() {
  local name="$1" shape="$2"
  local dir="${FAKE_REPO}/rust/crates/${name}"
  mkdir -p "${dir}/src"
  case "$shape" in
    inherited)
      cat > "${dir}/Cargo.toml" <<'EOF'
[package]
name = "fake"
version = "0.0.1"
edition = "2024"

[lints]
workspace = true
EOF
      ;;
    missing)
      cat > "${dir}/Cargo.toml" <<'EOF'
[package]
name = "fake"
version = "0.0.1"
edition = "2024"
EOF
      ;;
    sub_only)
      cat > "${dir}/Cargo.toml" <<'EOF'
[package]
name = "fake"
version = "0.0.1"
edition = "2024"

[lints.rust]
unsafe_code = "forbid"
EOF
      ;;
    no_workspace)
      cat > "${dir}/Cargo.toml" <<'EOF'
[package]
name = "fake"
version = "0.0.1"
edition = "2024"

[lints]
EOF
      ;;
  esac
}

@test "verify-rust-lints-inheritance: real repo passes (smoke against actual rust/crates/)" {
  # Sanity check against the live repo — every crate currently in
  # rust/crates/ MUST have the stanza. If this fails on `main`, the
  # workspace lint policy has broken.
  run bash "$SCRIPT"
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
  [[ "$output" == *"inherit workspace lints"* ]]
}

@test "verify-rust-lints-inheritance: empty rust/crates/ -> exit 0 with friendly message" {
  # No crates yet — script must not fail (avoids blocking a future
  # cleanup PR that temporarily removes all crates).
  run bash "${FAKE_REPO}/scripts/verify-rust-lints-inheritance.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"nothing to check"* ]]
}

@test "verify-rust-lints-inheritance: crate with inherited stanza passes" {
  _make_crate "good" inherited
  run bash "${FAKE_REPO}/scripts/verify-rust-lints-inheritance.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"OK"* ]]
}

@test "verify-rust-lints-inheritance: crate missing [lints] table fails" {
  _make_crate "bad" missing
  run bash "${FAKE_REPO}/scripts/verify-rust-lints-inheritance.sh"
  [ "$status" -ne 0 ]
  [[ "$output" == *"FAIL"* ]]
  [[ "$output" == *"missing '[lints] workspace = true'"* ]]
  [[ "$output" == *"rust/crates/bad/Cargo.toml"* ]]
  # Failure output must include the exact stanza to paste (PR #314
  # round-2 review Lens 8 MEDIUM — actionable diagnostics).
  [[ "$output" == *"[lints]"* ]]
  [[ "$output" == *"workspace = true"* ]]
}

@test "verify-rust-lints-inheritance: crate with only [lints.rust] sub-table fails" {
  # PR #314 round-2 review Lens 2 MEDIUM + Lens 5 MEDIUM: the awk regex
  # should NOT treat a `[lints.rust]` sub-table as inheritance. Bare
  # `[lints] workspace = true` is the workspace-inheritance stanza;
  # `[lints.rust]` is a different per-tool override.
  _make_crate "sub" sub_only
  run bash "${FAKE_REPO}/scripts/verify-rust-lints-inheritance.sh"
  [ "$status" -ne 0 ]
  [[ "$output" == *"FAIL"* ]]
  [[ "$output" == *"rust/crates/sub/Cargo.toml"* ]]
}

@test "verify-rust-lints-inheritance: crate with [lints] table but no workspace=true fails" {
  _make_crate "noinherit" no_workspace
  run bash "${FAKE_REPO}/scripts/verify-rust-lints-inheritance.sh"
  [ "$status" -ne 0 ]
  [[ "$output" == *"FAIL"* ]]
  [[ "$output" == *"rust/crates/noinherit/Cargo.toml"* ]]
}

@test "verify-rust-lints-inheritance: mixed-state repo names only the offending crates" {
  _make_crate "good" inherited
  _make_crate "bad" missing
  _make_crate "alsogood" inherited
  run bash "${FAKE_REPO}/scripts/verify-rust-lints-inheritance.sh"
  [ "$status" -ne 0 ]
  [[ "$output" == *"rust/crates/bad/Cargo.toml"* ]]
  [[ "$output" != *"rust/crates/good/Cargo.toml"* ]]
  [[ "$output" != *"rust/crates/alsogood/Cargo.toml"* ]]
}

@test "verify-rust-lints-inheritance: paths with spaces don't break the glob (Lens 1 LOW)" {
  # PR #314 round-2 review Lens 1 LOW: the original unquoted-glob form
  # would break if any path contained whitespace. mapfile + find -print0
  # handles it; this test pins the new behavior.
  local space_repo="${BATS_TEST_TMPDIR}/with spaces"
  mkdir -p "${space_repo}/scripts"
  cp "$SCRIPT" "${space_repo}/scripts/verify-rust-lints-inheritance.sh"
  local dir="${space_repo}/rust/crates/spacecrate"
  mkdir -p "${dir}/src"
  cat > "${dir}/Cargo.toml" <<'EOF'
[package]
name = "spacecrate"
version = "0.0.1"
edition = "2024"

[lints]
workspace = true
EOF
  run bash "${space_repo}/scripts/verify-rust-lints-inheritance.sh"
  [ "$status" -eq 0 ]
}
