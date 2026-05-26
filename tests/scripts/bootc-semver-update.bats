#!/usr/bin/env bats
#
# Unit tests for containers/shared/bootc-semver-update.sh (PR #181 round-1).
#
# The script runs on the bootc host as a oneshot service; here we exercise
# it under bats by stubbing skopeo / jq / bootc / logger as shell functions
# so the test environment has no dependency on those binaries.
#
# Coverage:
#   1. Missing REPO env -> fail loud (exit non-zero).
#   2. skopeo failure (network/auth) -> exit 1 with captured stderr in
#      the logger output. (Was silently exit 0 pre-round-1 fix.)
#   3. skopeo success but zero semver tags -> exit 0, log "no semver tags".
#   4. skopeo success + tag list with a mix of semver + non-semver -> picks
#      the highest semver only (ignores -rc1/-beta/sha-shortrefs/:latest).
#   5. Target equals current booted image -> exit 0, no `bootc switch` call.
#   6. Target newer than current -> calls `bootc switch <target>` then
#      `bootc upgrade`.
#   7. PREFIX="" (empty) -> matches bare 1.2.3 tags, not v1.2.3.
#
# Run via:
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/scripts/bootc-semver-update.bats
# OR locally:
#   bats tests/scripts/bootc-semver-update.bats

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/containers/shared/bootc-semver-update.sh"

  # Isolate the env file location. The script reads
  # /etc/hummingbird/bootc-update.env when readable; in the bats container
  # that path almost certainly isn't, but be defensive in case a developer
  # runs locally on a host that has one. We don't shadow /etc; instead the
  # tests set REPO/PREFIX via the environment, which the script picks up
  # without sourcing the file.
  unset REPO PREFIX

  # Per-test sandbox for stub binaries.
  STUB_BIN="$BATS_TEST_TMPDIR/stub-bin"
  mkdir -p "$STUB_BIN"
  export PATH="$STUB_BIN:$PATH"

  # logger stub: capture invocations to a file so tests can assert on them.
  LOGGER_OUT="$BATS_TEST_TMPDIR/logger.log"
  : >"$LOGGER_OUT"
  cat >"$STUB_BIN/logger" <<EOF
#!/usr/bin/env bash
# Mimic real logger -t TAG -p PRIORITY MSG by appending the args verbatim.
echo "logger \$*" >>"$LOGGER_OUT"
EOF
  chmod +x "$STUB_BIN/logger"

  # bootc stub: tests override the "status --json" payload and the
  # switch/upgrade behavior by writing to per-test state files.
  BOOTC_STATUS_JSON="$BATS_TEST_TMPDIR/bootc-status.json"
  BOOTC_CALLS="$BATS_TEST_TMPDIR/bootc-calls.log"
  : >"$BOOTC_CALLS"
  cat >"$STUB_BIN/bootc" <<EOF
#!/usr/bin/env bash
echo "bootc \$*" >>"$BOOTC_CALLS"
case "\$1" in
  status)
    if [[ -r "$BOOTC_STATUS_JSON" ]]; then
      cat "$BOOTC_STATUS_JSON"
    else
      echo '{}'
    fi
    ;;
  switch|upgrade) exit 0 ;;
  *) exit 0 ;;
esac
EOF
  chmod +x "$STUB_BIN/bootc"

  # skopeo stub: tests override behavior by writing to per-test files.
  SKOPEO_MODE_FILE="$BATS_TEST_TMPDIR/skopeo-mode"      # success|fail
  SKOPEO_TAGS_FILE="$BATS_TEST_TMPDIR/skopeo-tags.json"
  SKOPEO_STDERR_MSG="$BATS_TEST_TMPDIR/skopeo-stderr"
  cat >"$STUB_BIN/skopeo" <<EOF
#!/usr/bin/env bash
mode="\$(cat "$SKOPEO_MODE_FILE" 2>/dev/null || echo success)"
if [[ "\$mode" = "fail" ]]; then
  # Order matters: send stdout to stderr FIRST, then optionally redirect
  # stderr of cat itself to /dev/null. The original "2>/dev/null >&2"
  # form silently swallowed the stderr-emission because >&2 was evaluated
  # after fd2 had been redirected to /dev/null.
  if [[ -r "$SKOPEO_STDERR_MSG" ]]; then
    cat "$SKOPEO_STDERR_MSG" >&2
  else
    echo 'skopeo: synthetic failure' >&2
  fi
  exit 1
fi
cat "$SKOPEO_TAGS_FILE" 2>/dev/null || echo '{"Tags":[]}'
EOF
  chmod +x "$STUB_BIN/skopeo"

  # jq stub: the bats container doesn't ship jq. We only need to support
  # the two query strings the script uses:
  #   .Tags[]                                    -> emit each tag on a line
  #   .status.booted.image.image.image // empty  -> emit the booted image ref
  # Anything else is a test bug; print an error to stderr and exit non-zero.
  cat >"$STUB_BIN/jq" <<'JQEOF'
#!/usr/bin/env bash
# Strip leading -r flag (the only flag the script uses).
[[ "${1:-}" = "-r" ]] && shift
query="${1:-}"
input="$(cat)"
case "$query" in
  '.Tags[]')
    # Extract each "..."-delimited tag from the Tags array.
    printf '%s' "$input" \
      | sed -n 's/.*"Tags":\[\([^]]*\)\].*/\1/p' \
      | tr ',' '\n' \
      | sed 's/^"//; s/"$//'
    ;;
  *'.status.booted.image.image.image'*)
    # The script uses `... // empty`. Print the image string if present,
    # otherwise nothing.
    printf '%s' "$input" \
      | sed -n 's/.*"image":"\([^"]*\)".*/\1/p' \
      | head -1
    ;;
  *)
    echo "test-jq-stub: unsupported query: $query" >&2
    exit 2
    ;;
esac
JQEOF
  chmod +x "$STUB_BIN/jq"
}

# Helper: write a tag list JSON in the shape skopeo emits.
write_skopeo_tags() {
  local out="$SKOPEO_TAGS_FILE"
  : >"$out"
  printf '{"Tags":[' >"$out"
  local first=1
  for t in "$@"; do
    if (( first )); then
      printf '"%s"' "$t" >>"$out"
      first=0
    else
      printf ',"%s"' "$t" >>"$out"
    fi
  done
  printf ']}' >>"$out"
}

# Helper: write a bootc status payload claiming the given current image.
write_bootc_current() {
  local img="$1"
  cat >"$BOOTC_STATUS_JSON" <<EOF
{"status":{"booted":{"image":{"image":{"image":"$img"}}}}}
EOF
}

# ---------------------------------------------------------------------------
# 1. REPO required
# ---------------------------------------------------------------------------

@test "bootc-semver-update: missing REPO fails loud" {
  # set -u + : "${REPO:?...}" should abort before any skopeo/bootc call.
  run env -i PATH="$PATH" bash "$SCRIPT"
  [ "$status" -ne 0 ]
  [[ "$output" == *"REPO"* ]]
}

# ---------------------------------------------------------------------------
# 2. skopeo failure -> exit 1, error logged (round-1 observability fix)
# ---------------------------------------------------------------------------

@test "bootc-semver-update: skopeo failure logs captured stderr and exits non-zero" {
  echo fail >"$SKOPEO_MODE_FILE"
  echo "manifest unknown to registry" >"$SKOPEO_STDERR_MSG"
  run env REPO="ghcr.io/test/x" bash "$SCRIPT"
  [ "$status" -ne 0 ]
  # logger was invoked with -p user.err and the captured stderr message.
  grep -q 'skopeo list-tags' "$LOGGER_OUT"
  grep -q 'manifest unknown to registry' "$LOGGER_OUT"
  # We must NOT silently exit 0 like the pre-fix path did.
  [ "$status" -eq 1 ]
}

# ---------------------------------------------------------------------------
# 3. zero matching tags -> exit 0, "no semver tags" log
# ---------------------------------------------------------------------------

@test "bootc-semver-update: empty tag list yields 'no semver tags' and exit 0" {
  echo success >"$SKOPEO_MODE_FILE"
  write_skopeo_tags "latest"            # no semver-shaped tags
  run env REPO="ghcr.io/test/x" bash "$SCRIPT"
  [ "$status" -eq 0 ]
  grep -q 'no semver tags at ghcr.io/test/x' "$LOGGER_OUT"
  # bootc was never invoked with switch/upgrade.
  ! grep -qE 'bootc switch|bootc upgrade' "$BOOTC_CALLS"
}

# ---------------------------------------------------------------------------
# 4. mixed tag list -> highest strict semver wins, prereleases excluded
# ---------------------------------------------------------------------------

@test "bootc-semver-update: picks highest vMAJOR.MINOR.PATCH, ignores prerelease + non-semver" {
  echo success >"$SKOPEO_MODE_FILE"
  # Mix: stable semver, prereleases, branch refs, latest, sha-shorts. The
  # winner should be v0.4.2 — NOT v0.5.0-rc1 (prerelease excluded) and NOT
  # `main` or `latest` (not strict semver).
  write_skopeo_tags "v0.1.0" "v0.4.2" "v0.4.1" "v0.5.0-rc1" "latest" "main" "deadbeef"
  write_bootc_current "ghcr.io/test/x:v0.1.0"
  run env REPO="ghcr.io/test/x" bash "$SCRIPT"
  [ "$status" -eq 0 ]
  # advancing log captures both arrow ends; assert the target.
  grep -q 'advancing ghcr.io/test/x:v0.1.0 -> ghcr.io/test/x:v0.4.2' "$LOGGER_OUT"
  grep -qE 'bootc switch ghcr.io/test/x:v0.4.2' "$BOOTC_CALLS"
  grep -qE 'bootc upgrade' "$BOOTC_CALLS"
}

# ---------------------------------------------------------------------------
# 5. already on target -> no switch/upgrade
# ---------------------------------------------------------------------------

@test "bootc-semver-update: already on target image is a clean no-op" {
  echo success >"$SKOPEO_MODE_FILE"
  write_skopeo_tags "v0.4.2"
  write_bootc_current "ghcr.io/test/x:v0.4.2"
  run env REPO="ghcr.io/test/x" bash "$SCRIPT"
  [ "$status" -eq 0 ]
  grep -q 'already on ghcr.io/test/x:v0.4.2' "$LOGGER_OUT"
  ! grep -qE 'bootc switch|bootc upgrade' "$BOOTC_CALLS"
}

# ---------------------------------------------------------------------------
# 6. happy-path advance from v0.4.1 -> v0.4.2
# ---------------------------------------------------------------------------

@test "bootc-semver-update: advances current -> target via bootc switch + upgrade" {
  echo success >"$SKOPEO_MODE_FILE"
  write_skopeo_tags "v0.4.1" "v0.4.2"
  write_bootc_current "ghcr.io/test/x:v0.4.1"
  run env REPO="ghcr.io/test/x" bash "$SCRIPT"
  [ "$status" -eq 0 ]
  grep -qE 'bootc switch ghcr.io/test/x:v0.4.2' "$BOOTC_CALLS"
  grep -qE 'bootc upgrade' "$BOOTC_CALLS"
}

# ---------------------------------------------------------------------------
# 7. PREFIX="" matches bare X.Y.Z, not vX.Y.Z
# ---------------------------------------------------------------------------

@test "bootc-semver-update: empty PREFIX matches bare semver" {
  echo success >"$SKOPEO_MODE_FILE"
  # Tag list has both prefixed + bare; with PREFIX="" only the bare ones match.
  write_skopeo_tags "v1.0.0" "1.2.3" "1.2.4"
  write_bootc_current "ghcr.io/test/x:1.2.3"
  # The script defaults PREFIX=v when sourced env doesn't set it; pass
  # PREFIX explicitly empty here.
  run env REPO="ghcr.io/test/x" PREFIX="" bash "$SCRIPT"
  [ "$status" -eq 0 ]
  grep -qE 'bootc switch ghcr.io/test/x:1.2.4' "$BOOTC_CALLS"
}
