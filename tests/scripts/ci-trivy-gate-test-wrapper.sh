#!/usr/bin/env bash
# Test-only adapter. Production CI must invoke ci-trivy-gate.sh directly.
set -u
fixture=$(mktemp)
cleanup() { rm -f "$fixture"; }
trap cleanup EXIT
scan_rc=0
bash -c "${SCAN_CMD:?SCAN_CMD required}" >"$fixture" || scan_rc=$?
bin=$(mktemp -d)
trap 'rm -rf "$bin"; cleanup' EXIT
cat >"$bin/docker" <<'EOF'
#!/usr/bin/env bash
cat "${TEST_TRIVY_FIXTURE:?fixture missing}"
exit "${TEST_TRIVY_RC:-0}"
EOF
chmod +x "$bin/docker"
TEST_TRIVY_FIXTURE="$fixture" TEST_TRIVY_RC="$scan_rc" PATH="$bin:$PATH" \
  "$(dirname "$0")/../../scripts/ci-trivy-gate.sh" "$@"
