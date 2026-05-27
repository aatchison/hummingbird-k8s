#!/usr/bin/env bash
# scripts/check-cilium-k8s-compat.sh
#
# Pre-flight: warn (or fail with --strict) when the pinned Cilium
# version doesn't cover the pinned (or target) Kubernetes minor version.
#
# Background — issue #303
# -----------------------
# Hummingbird pins two versions that move on independent schedules:
#
#   * Cilium 1.16.5  in containers/k8s/k8s-init.sh  (`cilium install --version …`)
#   * K8s    v1.31   in containers/k8s/Containerfile (`ARG K8S_VERSION=v1.31`)
#
# Each Cilium minor (1.14, 1.15, …) only e2e-tests against a window of
# K8s minors. Bumping `K8S_VERSION` past the pinned Cilium's window
# breaks NetworkPolicy enforcement, Hubble flow visibility, and the
# CiliumNetworkPolicy CRD reconciler — often silently on first boot.
#
# `docs/k8s-version-upgrade.md` already tells operators to "Check the
# Cilium compatibility matrix" by hand; this script codifies that step
# so the next K8s bump PR can run it as part of pre-flight and the next
# operator doesn't have to remember to read the docs first.
#
# Behavior
# --------
# Default: WARN on mismatch; exit 0 so CI / pre-flight chains keep going.
#          The warning is enough for a human reviewer to notice without
#          blocking a build that may still be intentionally bumped (e.g.
#          during a planned Cilium upgrade window where both PRs are in
#          flight).
#
# --strict: exit 1 on mismatch. Useful when wiring into a pre-merge gate
#           that should hard-fail on incompatibility (e.g. the proposed
#           --kubeadm-upgrade flag in #299 phase 1).
#
# Inputs
# ------
# Defaults read from the repo (no flags = "check the currently-committed pins"):
#   * CILIUM_VERSION — extracted from containers/k8s/k8s-init.sh
#                      (`cilium install --version X.Y.Z`)
#   * K8S_VERSION    — extracted from containers/k8s/Containerfile
#                      (`ARG K8S_VERSION=vX.Y`)
#
# Overrides for "what if I bump to X" checks:
#   --cilium=X.Y.Z   — override the Cilium version (any patch level)
#   --k8s=vX.Y       — override the K8s minor (with or without leading 'v',
#                      patch-level component ignored — matrix is minor-scoped)
#   --strict         — exit 1 on mismatch instead of warning (default: warn)
#
# Examples
# --------
#   # Check the currently-pinned versions, warn on mismatch
#   bash scripts/check-cilium-k8s-compat.sh
#
#   # "Can I bump K8s to v1.32 without bumping Cilium?"
#   bash scripts/check-cilium-k8s-compat.sh --k8s=v1.32
#
#   # Pre-merge gate (CI): hard-fail if the committed pins don't agree
#   bash scripts/check-cilium-k8s-compat.sh --strict
#
# Matrix source
# -------------
# Upstream Cilium docs, per-version compatibility pages:
#   https://docs.cilium.io/en/v1.14/network/kubernetes/compatibility/
#   https://docs.cilium.io/en/v1.15/network/kubernetes/compatibility/
#   https://docs.cilium.io/en/v1.16/network/kubernetes/compatibility/
#   https://docs.cilium.io/en/v1.17/network/kubernetes/compatibility/
#   https://docs.cilium.io/en/v1.18/network/kubernetes/compatibility/
#
# The matrix is embedded (not fetched at runtime) so this script works
# offline and doesn't introduce a new network dependency to pre-flight.
# Refresh by re-reading the upstream pages above when bumping the
# Cilium pin; bats coverage in tests/scripts/check-cilium-k8s-compat.bats
# pins the embedded table to the current pin so a stale matrix is loud.
#
# Run from anywhere; the script resolves paths relative to its own location.

set -euo pipefail

# ---- argument parsing --------------------------------------------------

CILIUM_OVERRIDE=""
K8S_OVERRIDE=""
STRICT=0

usage() {
  cat <<'EOF'
Usage: check-cilium-k8s-compat.sh [--cilium=X.Y.Z] [--k8s=vX.Y] [--strict] [-h|--help]

Warn (or, with --strict, fail) when the pinned Cilium version doesn't
cover the pinned (or target) Kubernetes minor.

Defaults read the pins from:
  containers/k8s/k8s-init.sh     (Cilium)
  containers/k8s/Containerfile   (K8s minor via ARG K8S_VERSION=vX.Y)
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --cilium=*) CILIUM_OVERRIDE="${1#--cilium=}" ;;
    --k8s=*)    K8S_OVERRIDE="${1#--k8s=}" ;;
    --strict)   STRICT=1 ;;
    -h|--help)  usage; exit 0 ;;
    *)
      echo "ERROR: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

# ---- locate repo + read pins ------------------------------------------

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
k8s_init="${repo_root}/containers/k8s/k8s-init.sh"
containerfile="${repo_root}/containers/k8s/Containerfile"

# Cilium pin: the `--version X.Y.Z` flag on the `cilium install` line.
# Anchor on `--version` to avoid matching the cilium-cli's CILIUM_CLI_VERSION
# build-arg in the Containerfile.
extract_cilium_pin() {
  local file="$1"
  # Match e.g. `  --version 1.16.5 \` — the version token after --version,
  # optionally `=` separated. Strip a leading 'v' if upstream ever flips
  # to `--version v1.16.5`.
  grep -E '^[[:space:]]*--version[[:space:]=]+v?[0-9]+\.[0-9]+\.[0-9]+' "$file" \
    | head -n1 \
    | sed -E 's/^[[:space:]]*--version[[:space:]=]+v?([0-9]+\.[0-9]+\.[0-9]+).*/\1/'
}

# K8s pin: the `ARG K8S_VERSION=vX.Y` line in the Containerfile.
# Returns the bare "X.Y" (no 'v') for matrix lookup.
extract_k8s_pin() {
  local file="$1"
  grep -E '^ARG[[:space:]]+K8S_VERSION[[:space:]]*=[[:space:]]*v?[0-9]+\.[0-9]+' "$file" \
    | head -n1 \
    | sed -E 's/^ARG[[:space:]]+K8S_VERSION[[:space:]]*=[[:space:]]*v?([0-9]+\.[0-9]+).*/\1/'
}

# Normalize a K8s version override to bare "X.Y" (drop leading 'v', drop
# any patch component the operator might tack on).
normalize_k8s_minor() {
  local raw="$1"
  printf '%s\n' "$raw" | sed -E 's/^v?([0-9]+\.[0-9]+).*/\1/'
}

if [ -n "$CILIUM_OVERRIDE" ]; then
  cilium_version="$CILIUM_OVERRIDE"
else
  [ -r "$k8s_init" ] || {
    echo "ERROR: cannot read $k8s_init to extract Cilium pin" >&2
    exit 2
  }
  cilium_version="$(extract_cilium_pin "$k8s_init")"
fi

if [ -n "$K8S_OVERRIDE" ]; then
  k8s_minor="$(normalize_k8s_minor "$K8S_OVERRIDE")"
else
  [ -r "$containerfile" ] || {
    echo "ERROR: cannot read $containerfile to extract K8S_VERSION pin" >&2
    exit 2
  }
  k8s_minor="$(extract_k8s_pin "$containerfile")"
fi

if [ -z "${cilium_version:-}" ]; then
  echo "ERROR: could not extract Cilium --version from k8s-init.sh" >&2
  exit 2
fi
if [ -z "${k8s_minor:-}" ]; then
  echo "ERROR: could not extract K8S_VERSION from Containerfile" >&2
  exit 2
fi

# Derive the Cilium minor (e.g. 1.16.5 -> 1.16) for matrix lookup.
cilium_minor="$(printf '%s\n' "$cilium_version" | sed -E 's/^([0-9]+\.[0-9]+).*/\1/')"

# ---- embedded compatibility matrix ------------------------------------
#
# Each row is "<cilium-minor>=<k8s-minor> <k8s-minor> …" — space-separated
# K8s minors that the corresponding Cilium minor e2e-tests against.
#
# Source: upstream Cilium docs (URLs in the header above). Refresh when
# bumping the Cilium pin past the highest known minor here.
#
# Schema note: matrix is minor×minor, not patch×patch. Cilium ships
# patch-level fixes against the same K8s window; a 1.16.x patch bump
# does not change the K8s compat window. So checking minor-vs-minor is
# the right granularity.
cilium_compat_matrix() {
  cat <<'EOF'
1.14=1.19 1.20 1.21 1.22 1.23 1.24 1.25 1.26 1.27
1.15=1.26 1.27 1.28 1.29
1.16=1.27 1.28 1.29 1.30
1.17=1.29 1.30 1.31 1.32
1.18=1.30 1.31 1.32 1.33
EOF
}

# Look up the supported K8s minors for a given Cilium minor; prints them
# space-separated, or empty if the Cilium minor is unknown.
lookup_supported_k8s() {
  local cilium_m="$1"
  cilium_compat_matrix \
    | awk -F= -v c="$cilium_m" '$1 == c { print $2; exit }'
}

# Membership test: is $k8s_minor in the space-separated $supported list?
k8s_in_supported() {
  local needle="$1" haystack="$2" m
  for m in $haystack; do
    [ "$m" = "$needle" ] && return 0
  done
  return 1
}

supported="$(lookup_supported_k8s "$cilium_minor")"

# ---- emit a verdict ---------------------------------------------------

if [ -z "$supported" ]; then
  # Unknown Cilium minor — the matrix is stale relative to the pin.
  # This is a soft signal: warn + exit 0 (or 1 under --strict). Refresh
  # the matrix in this script from the upstream docs to silence it.
  {
    echo "WARN: Cilium minor ${cilium_minor} (pinned ${cilium_version}) is not in the embedded compat matrix."
    echo "      Refresh the matrix in scripts/check-cilium-k8s-compat.sh from:"
    echo "      https://docs.cilium.io/en/v${cilium_minor}/network/kubernetes/compatibility/"
    echo "      K8s pin: ${k8s_minor}"
  } >&2
  [ "$STRICT" -eq 1 ] && exit 1
  exit 0
fi

if k8s_in_supported "$k8s_minor" "$supported"; then
  echo "OK: Cilium ${cilium_version} supports K8s ${k8s_minor} (supported: ${supported})."
  exit 0
fi

# Mismatch — primary case the script exists for.
{
  echo "WARN: Cilium ${cilium_version} does NOT list K8s ${k8s_minor} as a supported minor."
  echo "      Cilium ${cilium_minor}.x supported K8s minors: ${supported}"
  echo "      Bump Cilium first (see docs/cilium-migration.md) OR pick a K8s minor in range."
  echo "      Upstream matrix: https://docs.cilium.io/en/v${cilium_minor}/network/kubernetes/compatibility/"
} >&2

if [ "$STRICT" -eq 1 ]; then
  exit 1
fi
exit 0
