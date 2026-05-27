#!/usr/bin/env bash
# scripts/check-lints-inheritance.sh
#
# CI guard for the Rust workspace's lint policy (epic #279, #281).
#
# Background: the workspace declares shared lints in `rust/Cargo.toml`
# under `[workspace.lints.rust]` + `[workspace.lints.clippy]`. Cargo
# intentionally requires each crate to opt in via:
#
#   [lints]
#   workspace = true
#
# If a future crate author forgets the stanza, the workspace's
# `unsafe_code = "forbid"` / `clippy = "deny"` policy silently does not
# apply to that crate — a security-relevant footgun flagged as Lens 6
# MEDIUM during the PR #313 round-2 review.
#
# This script is the backstop: it scans `rust/crates/*/Cargo.toml` and
# fails (exit 1) if any crate is missing the inheritance stanza. Wired
# into `.github/workflows/rust-ci.yml::lint-inheritance`.
#
# Run from the repo root:
#   bash scripts/check-lints-inheritance.sh

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
crates_glob="${repo_root}/rust/crates/*/Cargo.toml"

missing=()
shopt -s nullglob
# shellcheck disable=SC2206 # word-splitting the glob is intentional
cargo_tomls=( ${crates_glob} )

if [ ${#cargo_tomls[@]} -eq 0 ]; then
  echo "No rust/crates/*/Cargo.toml found — nothing to check."
  exit 0
fi

for cargo_toml in "${cargo_tomls[@]}"; do
  # Match a `[lints]` table whose body contains `workspace = true`. awk
  # keeps the check robust to comment lines or extra keys inside the
  # table. We deliberately don't try to parse the TOML fully — the
  # signal we want is just "the stanza is present".
  if ! awk '
    /^\[lints\]/                                                    { in_lints = 1; next }
    in_lints && /^\[/                                               { in_lints = 0 }
    in_lints && /^[[:space:]]*workspace[[:space:]]*=[[:space:]]*true/ { found = 1 }
    END                                                             { exit found ? 0 : 1 }
  ' "${cargo_toml}"; then
    missing+=("${cargo_toml#"${repo_root}/"}")
  fi
done

if [ ${#missing[@]} -gt 0 ]; then
  {
    echo "FAIL: the following crates are missing '[lints] workspace = true':"
    printf '  - %s\n' "${missing[@]}"
    echo
    echo "Add the stanza to each Cargo.toml so the workspace's"
    echo "unsafe_code/clippy lint policy applies. See rust/Cargo.toml"
    echo "[workspace.lints.*] for what's being inherited."
  } >&2
  exit 1
fi

echo "OK: all ${#cargo_tomls[@]} crate(s) inherit workspace lints."
