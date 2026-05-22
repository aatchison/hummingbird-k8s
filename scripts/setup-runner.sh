#!/usr/bin/env bash
#
# setup-runner.sh — register this KVM host as a self-hosted GitHub Actions
# runner for aatchison/hummingbird-k8s.
#
# Run as the operator's *non-root* user on the KVM host. The systemd-install
# step alone elevates via sudo; everything else stays user-owned.
#
# Usage:
#   bash scripts/setup-runner.sh <registration-token>
#
# Get a registration token (requires `repo` scope, so the operator runs this
# themselves — not the script):
#   gh api repos/aatchison/hummingbird-k8s/actions/runners/registration-token \
#     -X POST --jq .token
#
set -euo pipefail

REPO_URL="https://github.com/aatchison/hummingbird-k8s"
REPO_SLUG="aatchison/hummingbird-k8s"
RUNNER_DIR="${HOME}/actions-runner"
LABELS="kvm,libvirt"

die() { echo "error: $*" >&2; exit 1; }

[[ $# -ge 1 ]] || die "usage: $0 <registration-token>
get one with:
  gh api repos/${REPO_SLUG}/actions/runners/registration-token -X POST --jq .token"

TOKEN="$1"

[[ $EUID -ne 0 ]] || die "do not run as root; run as your normal login user (sudo is used only for the systemd-install step)"

command -v curl >/dev/null || die "curl is required"
command -v jq >/dev/null   || die "jq is required"
command -v tar >/dev/null  || die "tar is required"
command -v sudo >/dev/null || die "sudo is required"

RUNNER_USER="$(id -un)"
RUNNER_NAME="$(hostname -s)"

echo ">> resolving latest actions/runner release"
LATEST_JSON="$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest)"
VERSION_TAG="$(echo "$LATEST_JSON" | jq -r .tag_name)"
VERSION="${VERSION_TAG#v}"
[[ -n "$VERSION" && "$VERSION" != "null" ]] || die "could not resolve latest runner version"
TARBALL="actions-runner-linux-x64-${VERSION}.tar.gz"
URL="https://github.com/actions/runner/releases/download/v${VERSION}/${TARBALL}"

echo ">> latest runner is v${VERSION}"
mkdir -p "$RUNNER_DIR"
cd "$RUNNER_DIR"

if [[ ! -f "./config.sh" ]]; then
  echo ">> downloading ${TARBALL}"
  curl -fsSL -o "$TARBALL" "$URL"
  echo ">> unpacking"
  tar xzf "$TARBALL"
  rm -f "$TARBALL"
else
  echo ">> runner already unpacked in ${RUNNER_DIR}, skipping download"
fi

# If a previous service is installed, stop+uninstall so --replace works cleanly.
if [[ -f "./svc.sh" ]] && sudo ./svc.sh status >/dev/null 2>&1; then
  echo ">> stopping existing runner service"
  sudo ./svc.sh stop  || true
  sudo ./svc.sh uninstall || true
fi

echo ">> registering runner '${RUNNER_NAME}' with labels '${LABELS}'"
./config.sh \
  --url "$REPO_URL" \
  --token "$TOKEN" \
  --name "$RUNNER_NAME" \
  --labels "$LABELS" \
  --unattended \
  --replace

echo ">> installing systemd service (requires sudo)"
sudo ./svc.sh install "$RUNNER_USER"

echo ">> starting systemd service"
sudo ./svc.sh start

echo ">> verifying registration via GitHub API"
if command -v gh >/dev/null; then
  gh api "repos/${REPO_SLUG}/actions/runners" --jq '.runners[] | "\(.name)\t\(.status)\t\(.labels | map(.name) | join(","))"' || \
    echo "(gh api call failed; verify manually with: gh api repos/${REPO_SLUG}/actions/runners)"
else
  echo "gh not installed; verify manually:"
  echo "  gh api repos/${REPO_SLUG}/actions/runners --jq '.runners[].name'"
fi

echo ">> done. runner '${RUNNER_NAME}' should now appear online."
