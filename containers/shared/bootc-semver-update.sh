#!/bin/bash
# bootc-semver-update — find the latest semver-tagged image at the configured
# repo and bootc-switch to it if it's newer than what's currently booted.
# Run by bootc-semver-update.timer (default OnCalendar=daily).
#
# Why this exists: the stock bootc-fetch-apply-updates.timer follows whatever
# ref the host is pinned to (typically `:latest`, a mutable pointer). A bad
# push to `:latest` rolls every VM in the fleet forward to a broken image
# simultaneously. This unit instead resolves the highest semver tag at the
# image's GHCR repo via skopeo list-tags + sort -V and switches to that — so
# the fleet only ever advances when a new immutable tag (e.g. v0.4.2) is
# published, and operators can hold back by simply not tagging a release.
#
# Defaults are baked into /etc/hummingbird/bootc-update.env at image build
# (REPO=ghcr.io/aatchison/hummingbird-<flavor>, PREFIX=v). Operators forking
# the repo override via cloud-init write_files or manual edit. See
# docs/auto-updates.md.
set -euo pipefail

# Configuration from /etc/hummingbird/bootc-update.env.
if [[ -r /etc/hummingbird/bootc-update.env ]]; then
  # shellcheck disable=SC1091
  source /etc/hummingbird/bootc-update.env
fi
: "${REPO:?REPO not set in /etc/hummingbird/bootc-update.env}"
PREFIX="${PREFIX:-v}"

# Resolve the highest semver tag at REPO.
mapfile -t tags < <(skopeo list-tags "docker://${REPO}" 2>/dev/null \
  | jq -r '.Tags[]' \
  | grep -E "^${PREFIX}[0-9]+\.[0-9]+\.[0-9]+$" \
  | sort -V)
if [[ ${#tags[@]} -eq 0 ]]; then
  logger -t bootc-semver-update "no semver tags at ${REPO}; exit 0"
  exit 0
fi
target="${REPO}:${tags[-1]}"

current="$(bootc status --json 2>/dev/null \
  | jq -r '.status.booted.image.image.image // empty')"
if [[ -z "$current" ]]; then
  logger -t bootc-semver-update "could not read current bootc image; exit 1"
  exit 1
fi
if [[ "$current" = "$target" ]]; then
  logger -t bootc-semver-update "already on ${target}"
  exit 0
fi

logger -t bootc-semver-update "advancing ${current} -> ${target}"
bootc switch "$target"
bootc upgrade   # stages; the booted deployment swaps at next reboot
