#!/usr/bin/env bats
#
# Pins the bootc-update timer contract across the two surviving image
# flavors (issue #225). The point of these tests is NOT to verify
# systemd-preset mechanics — that's systemd's job. The point is to make
# the *intentional* drift between the preset layer and the documented
# CP/worker posture (docs/auto-updates.md) explicit, so a future
# maintainer "aligning" the two files doesn't silently break either
# flavor.
#
# Contract under test (see header comments in both preset files):
#   - Both flavors disable bootc-fetch-apply-updates.timer (legacy).
#   - Both flavors enable bootc-semver-update.timer (#181 canonical).
#   - The CP/worker auto-update posture split is enforced at deploy
#     time via scripts/deploy-cluster.sh AUTO_UPDATE_CP, NOT here.
#
# Run via:
#   make test-lib
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/lib/preset_contract.bats

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  CP_PRESET="${REPO_ROOT}/containers/k8s/10-k8s.preset"
  WK_PRESET="${REPO_ROOT}/containers/k8s-worker/10-k8s-worker.preset"
}

# ---------------------------------------------------------------------------
# Files exist + are non-empty
# ---------------------------------------------------------------------------

@test "preset files exist for both surviving flavors" {
  [ -f "$CP_PRESET" ]
  [ -f "$WK_PRESET" ]
  [ -s "$CP_PRESET" ]
  [ -s "$WK_PRESET" ]
}

# ---------------------------------------------------------------------------
# Legacy timer: both flavors must disable it (post-#181 contract)
# ---------------------------------------------------------------------------

@test "k8s preset disables bootc-fetch-apply-updates.timer" {
  grep -qxF 'disable bootc-fetch-apply-updates.timer' "$CP_PRESET"
}

@test "k8s-worker preset disables bootc-fetch-apply-updates.timer" {
  grep -qxF 'disable bootc-fetch-apply-updates.timer' "$WK_PRESET"
}

@test "no preset accidentally enables the legacy fetch-apply-updates timer" {
  ! grep -qxF 'enable bootc-fetch-apply-updates.timer' "$CP_PRESET"
  ! grep -qxF 'enable bootc-fetch-apply-updates.timer' "$WK_PRESET"
}

# ---------------------------------------------------------------------------
# Semver timer: both flavors must enable it at the preset layer
#
# The CP-side disable lives in scripts/deploy-cluster.sh (AUTO_UPDATE_CP=false
# runcmd), NOT in the preset. See docs/auto-updates.md "Where the CP/worker
# split is enforced (#225)".
# ---------------------------------------------------------------------------

@test "k8s preset enables bootc-semver-update.timer (CP-side disable is deploy-time, not preset)" {
  grep -qxF 'enable bootc-semver-update.timer' "$CP_PRESET"
}

@test "k8s-worker preset enables bootc-semver-update.timer" {
  grep -qxF 'enable bootc-semver-update.timer' "$WK_PRESET"
}

# ---------------------------------------------------------------------------
# Init service per flavor
# ---------------------------------------------------------------------------

@test "k8s preset enables k8s-init.service (and not worker-init.service)" {
  grep -qxF 'enable k8s-init.service' "$CP_PRESET"
  ! grep -qF 'worker-init.service' "$CP_PRESET"
}

@test "k8s-worker preset enables worker-init.service (and not k8s-init.service)" {
  grep -qxF 'enable worker-init.service' "$WK_PRESET"
  ! grep -qF 'k8s-init.service' "$WK_PRESET"
}

# ---------------------------------------------------------------------------
# Shared service set (the per-flavor diff is intentionally minimal)
# ---------------------------------------------------------------------------

@test "both presets enable crio.service" {
  grep -qxF 'enable crio.service' "$CP_PRESET"
  grep -qxF 'enable crio.service' "$WK_PRESET"
}

@test "both presets enable kubelet.service" {
  grep -qxF 'enable kubelet.service' "$CP_PRESET"
  grep -qxF 'enable kubelet.service' "$WK_PRESET"
}

@test "both presets enable health-check-rollback.timer" {
  grep -qxF 'enable health-check-rollback.timer' "$CP_PRESET"
  grep -qxF 'enable health-check-rollback.timer' "$WK_PRESET"
}

# ---------------------------------------------------------------------------
# Drift-explainer comments must be present so the contract is discoverable
# from the preset file itself, not just docs/auto-updates.md.
# ---------------------------------------------------------------------------

@test "k8s preset header comment references #225 and docs/auto-updates.md" {
  grep -qE '^#.*#225' "$CP_PRESET"
  grep -qE '^#.*docs/auto-updates\.md' "$CP_PRESET"
}

@test "k8s-worker preset header comment references #225 and docs/auto-updates.md" {
  grep -qE '^#.*#225' "$WK_PRESET"
  grep -qE '^#.*docs/auto-updates\.md' "$WK_PRESET"
}

# ---------------------------------------------------------------------------
# Sanity: every non-comment, non-blank line in each preset must be a valid
# preset directive (disable|enable <unit>). Catches typos / stray text.
# ---------------------------------------------------------------------------

@test "k8s preset: every non-comment line is a valid systemd preset directive" {
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^(disable|enable)[[:space:]]+[A-Za-z0-9._@-]+$ ]] \
      || { echo "bad line in $CP_PRESET: $line" >&2; return 1; }
  done < "$CP_PRESET"
}

@test "k8s-worker preset: every non-comment line is a valid systemd preset directive" {
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^(disable|enable)[[:space:]]+[A-Za-z0-9._@-]+$ ]] \
      || { echo "bad line in $WK_PRESET: $line" >&2; return 1; }
  done < "$WK_PRESET"
}
