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
# Honor explicitly-empty PREFIX (operators tagging bare 1.2.3, no leading v).
# `${PREFIX-v}` defaults only when unset; `${PREFIX:-v}` would replace empty
# with "v" too, contradicting the documented "set to empty for unprefixed
# semver" behavior in docs/auto-updates.md. (#181 round-1 review.)
PREFIX="${PREFIX-v}"

# Validate PREFIX is regex-safe before interpolating into grep -E below.
# Restrict to alphanumeric + . _ - so a malformed env override can't widen
# tag matching (CodeRabbit #181). Empty PREFIX is fine (unprefixed semver).
if [[ -n "$PREFIX" && ! "$PREFIX" =~ ^[A-Za-z0-9._-]+$ ]]; then
  logger -t bootc-semver-update -p user.err -- "invalid PREFIX '${PREFIX}': only [A-Za-z0-9._-] allowed"
  exit 1
fi

# Resolve the highest semver tag at REPO.
#
# Run skopeo first into a temp file so we can distinguish three failure
# modes that previously all looked like "no semver tags":
#
#   1. skopeo itself fails (network, auth, REPO doesn't exist) — we log
#      the captured stderr to journal and exit non-zero so the operator
#      sees a real error in `systemctl status` rather than a silent OK.
#   2. skopeo succeeds but the repo really has no semver-shaped tags
#      (e.g. brand-new repo, or only :latest published) — log + exit 0;
#      that's a legitimate "nothing to do" state.
#   3. skopeo succeeds and we find a target — proceed.
#
# Without this split, a transient network outage would log "no semver
# tags at ${REPO}; exit 0" and the operator would assume the repo
# regressed. (#181 round-1 review.)
# Don't use `trap 'rm -f "$skopeo_stderr"' EXIT` for cleanup: bash allows
# only one EXIT handler, so a later contributor adding a second EXIT trap
# would silently shadow this one and leak the tempfile. The tempfile is
# only used inside this one block, so do the cleanup explicitly in both
# branches and skip the trap entirely. (#181 round-2 review.)
skopeo_stderr="$(mktemp)"
if ! skopeo_out="$(skopeo list-tags "docker://${REPO}" 2>"$skopeo_stderr")"; then
  logger -t bootc-semver-update -p user.err -- \
    "skopeo list-tags docker://${REPO} failed: $(tr '\n' ' ' < "$skopeo_stderr")"
  rm -f "$skopeo_stderr"
  exit 1
fi
rm -f "$skopeo_stderr"
mapfile -t tags < <(printf '%s\n' "$skopeo_out" \
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
  logger -t bootc-semver-update -p user.err -- "could not read current bootc image; exit 1"
  exit 1
fi
if [[ "$current" = "$target" ]]; then
  logger -t bootc-semver-update "already on ${target}"
  exit 0
fi

logger -t bootc-semver-update "advancing ${current} -> ${target}"
bootc switch "$target"
bootc upgrade   # stages; the booted deployment swaps at next reboot
