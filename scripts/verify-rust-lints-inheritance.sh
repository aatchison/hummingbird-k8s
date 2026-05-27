#!/usr/bin/env bash
# scripts/verify-rust-lints-inheritance.sh
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
# Named with the `verify-*` prefix to match the existing scripts/verify-*
# convention (verify-app-deploy.sh, verify-encryption.sh,
# verify-hardening.sh). PR #314 round-2 review Lens 9 MEDIUM finding.
#
# Run from the repo root:
#   bash scripts/verify-rust-lints-inheritance.sh

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"

# Collect Cargo.toml files via mapfile + null-delimited find — safer than
# unquoted glob expansion (handles paths with whitespace; no shell
# word-splitting traps). PR #314 round-2 review Lens 1 LOW.
declare -a cargo_tomls=()
while IFS= read -r -d '' f; do
  cargo_tomls+=("$f")
done < <(find "${repo_root}/rust/crates" -mindepth 2 -maxdepth 2 -type f -name 'Cargo.toml' -print0 2>/dev/null)

if [ ${#cargo_tomls[@]} -eq 0 ]; then
  echo "No rust/crates/*/Cargo.toml found — nothing to check."
  exit 0
fi

missing=()
for cargo_toml in "${cargo_tomls[@]}"; do
  # Match a `[lints]` table (NOT a sub-table like `[lints.rust]`) whose
  # body contains `workspace = true`. awk keeps the check robust to
  # comment lines or extra keys inside the table. The exit-state
  # variable persists across the `END` regardless of which line set it.
  if ! awk '
    # Exact `[lints]` opener (not `[lints.rust]` etc).
    /^\[lints\][[:space:]]*$/                                       { in_lints = 1; next }
    # Any new bracketed table (including sub-tables of lints) ends the
    # current scan window — only the bare `[lints]` table counts.
    /^\[/                                                           { in_lints = 0; next }
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
    echo "Paste this stanza into each Cargo.toml above:"
    echo
    echo "  [lints]"
    echo "  workspace = true"
    echo
    echo "See rust/Cargo.toml [workspace.lints.*] for what's being inherited"
    echo "(unsafe_code = forbid + clippy = deny by default)."
  } >&2
  exit 1
fi

echo "OK: all ${#cargo_tomls[@]} crate(s) inherit workspace lints."
