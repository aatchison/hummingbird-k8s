#!/usr/bin/env bats
#
# Gate tests for scripts/ci-trivy-gate.sh.
#
# WHY THESE EXIST: two consecutive fail-open defects shipped in the inline
# workflow version of this logic (PR #413 review, forge-security). Both were
# ABSENCE bugs — the gate could not see that the scanner had not run. So the
# predicted-red set below is deliberately weighted toward absence:
#
#   scanner failure                     -> must FAIL (was: silent pass)
#   malformed / non-scan JSON           -> must FAIL (was: silent pass)
#   count > 0 but no ids extracted      -> must FAIL (was: silent pass)
#   new vendor-namespace id (RUSTSEC)   -> must FAIL (was: passed the
#                                          ^(CVE|GHSA)- prefix filter)
#   N old ids swapped for N new ids     -> must FAIL at UNCHANGED count
#                                          (the count-ratchet blind spot)
#
# plus the positive controls: the real measured inventory passes, and a
# strict improvement passes.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  GATE="$REPO_ROOT/tests/scripts/ci-trivy-gate-test-wrapper.sh"
  TMP="$BATS_TEST_TMPDIR"

  BASELINE="$TMP/baseline"
  cat > "$BASELINE" <<'EOF'
k8s=8
k8s-worker=8
id=CVE-2025-68121
id=CVE-2026-33186
id=GHSA-5w5r-mf82-595p
EOF
}

# Emit a scan JSON with the given ids, one vulnerability each.
_json_with() {
  printf '{"Results":[{"Vulnerabilities":['
  local first=1 id
  for id in "$@"; do
    [ $first -eq 1 ] || printf ','
    printf '{"VulnerabilityID":"%s","Severity":"CRITICAL"}' "$id"
    first=0
  done
  printf ']}]}'
}

_run_gate() { # $1 = scan command
  SCAN_CMD="$1" run "$GATE" k8s "$BASELINE" dummy-image:tag
}

# ---- positive controls ---------------------------------------------------

@test "passes on the real measured inventory (8 findings, 3 known ids)" {
  ids=(CVE-2025-68121 CVE-2026-33186 CVE-2026-33186 CVE-2026-33186 \
       CVE-2026-33186 GHSA-5w5r-mf82-595p CVE-2025-68121 CVE-2025-68121)
  _run_gate "$(printf 'printf %q' "$(_json_with "${ids[@]}")")"
  [ "$status" -eq 0 ]
  [[ "$output" == *"count: 8 (baseline 8)"* ]]
}

@test "passes and emits a notice on strict improvement (subset, lower count)" {
  _run_gate "$(printf 'printf %q' "$(_json_with CVE-2025-68121)")"
  [ "$status" -eq 0 ]
  [[ "$output" == *"::notice::"* ]]
  [[ "$output" == *"criticals dropped to 1"* ]]
}

@test "passes on a genuinely clean image (zero findings, empty id set)" {
  _run_gate "$(printf 'printf %q' '{"Results":[]}')"
  [ "$status" -eq 0 ]
}

# ---- predicted-red: ABSENCE cases ---------------------------------------

@test "PREDICTED RED: scanner invocation failure must fail closed" {
  _run_gate "false"
  [ "$status" -ne 0 ]
  [[ "$output" == *"scanner invocation failed"* ]]
}

@test "PREDICTED RED: scanner emitting nothing must fail closed" {
  _run_gate "true"
  [ "$status" -ne 0 ]
  [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: malformed JSON must fail closed" {
  _run_gate "printf 'not json at all'"
  [ "$status" -ne 0 ]
  [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: valid JSON without .Results must fail closed" {
  _run_gate "printf '{\"SchemaVersion\":2}'"
  [ "$status" -ne 0 ]
  [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: count>0 with unusable ids must fail closed" {
  # Vulnerabilities present, but every id is empty/whitespace — extraction
  # yields nothing while the count is non-zero.
  json='{"Results":[{"Vulnerabilities":[{"VulnerabilityID":""},{"VulnerabilityID":"  "}]}]}'
  _run_gate "$(printf 'printf %q' "$json")"
  [ "$status" -ne 0 ]
  [[ "$output" == *"extraction is broken"* ]]
}

# ---- predicted-red: SUBSTITUTION and NAMESPACE cases --------------------

@test "PREDICTED RED: new vendor-namespace id (RUSTSEC) must fail" {
  # The previous implementation filtered to ^(CVE|GHSA)- and would not have
  # seen this at all.
  _run_gate "$(printf 'printf %q' "$(_json_with CVE-2025-68121 RUSTSEC-2026-0001)")"
  [ "$status" -ne 0 ]
  [[ "$output" == *"not in the committed allowlist"* ]]
  [[ "$output" == *"RUSTSEC-2026-0001"* ]]
}

@test "PREDICTED RED: RHSA namespace id must fail" {
  _run_gate "$(printf 'printf %q' "$(_json_with RHSA-2026:1234)")"
  [ "$status" -ne 0 ]
  [[ "$output" == *"RHSA-2026:1234"* ]]
}

@test "PREDICTED RED: N old ids swapped for N new ids at UNCHANGED count" {
  # The count-only ratchet passed this; the id gate must not.
  _run_gate "$(printf 'printf %q' "$(_json_with CVE-2099-11111 CVE-2099-22222 CVE-2099-33333)")"
  [ "$status" -ne 0 ]
  [[ "$output" == *"not in the committed allowlist"* ]]
}

@test "PREDICTED RED: count above baseline must fail" {
  ids=(); for i in $(seq 1 9); do ids+=(CVE-2025-68121); done
  _run_gate "$(printf 'printf %q' "$(_json_with "${ids[@]}")")"
  [ "$status" -ne 0 ]
  [[ "$output" == *"exceeds baseline"* ]]
}

@test "PREDICTED RED: missing baseline row must fail" {
  SCAN_CMD="$(printf 'printf %q' "$(_json_with CVE-2025-68121)")" \
    run "$GATE" no-such-flavor "$BASELINE" dummy-image:tag
  [ "$status" -ne 0 ]
  [[ "$output" == *"no baseline row"* ]]
}

@test "PREDICTED RED: empty allowlist fails LOUDLY, not silently" {
  # Regression: `pipefail` on the `grep -E ^id=` read turned a baseline with
  # no id= lines into a bare rc=1 with no diagnostic. Fail-closed is right,
  # but an unexplained exit is not actionable. Empty allowlist must mean
  # "every id is unknown" and say so.
  local b="$TMP/counts-only"
  printf 'k8s=8\n' > "$b"
  SCAN_CMD="$(printf 'printf %q' "$(_json_with CVE-2025-68121)")" \
    run "$GATE" k8s "$b" dummy-image:tag
  [ "$status" -ne 0 ]
  [[ "$output" == *"not in the committed allowlist"* ]]
  [[ "$output" == *"CVE-2025-68121"* ]]
}

# ---- predicted-red: SCANNER OUTPUT SCHEMA (forge-security round 3) -------
#
# `has("Results")` is true for {"Results":null} and {"Results":"broken"}.
# The optional iterators `[]?` then yield nothing, so count=0 and the id set
# is empty — indistinguishable from a genuinely clean scan. Reproduced
# reaching "gate: PASS" before the fix.

@test "PREDICTED RED: Results is null" {
  _run_gate "printf '{\"Results\":null}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: Results is a string" {
  _run_gate "printf '{\"Results\":\"broken\"}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: Results is an object" {
  _run_gate "printf '{\"Results\":{}}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: Results is a scalar" {
  _run_gate "printf '{\"Results\":5}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: Results entry is not an object" {
  _run_gate "printf '{\"Results\":[\"x\"]}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: Vulnerabilities is a string" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":\"broken\"}]}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: Vulnerabilities is an object" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":{\"a\":1}}]}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: Vulnerabilities is a scalar" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":7}]}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: vulnerability entry is not an object" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":[\"broken\"]}]}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: vulnerability entry missing id" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":[{}]}]}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: mixed valid and malformed vulnerability entries" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":[{\"VulnerabilityID\":\"CVE-2025-68121\"},{}]}]}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

@test "PREDICTED RED: vulnerability id must be a non-empty string" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":[{\"VulnerabilityID\":null}]}]}'"
  [ "$status" -ne 0 ]; [[ "$output" == *"expected scan schema"* ]]
}

# Legitimate clean shapes must NOT be rejected — a schema check that also
# fails on real clean scans would just be a different kind of broken gate.

@test "clean form accepted: Results is an empty array" {
  _run_gate "printf '{\"Results\":[]}'"
  [ "$status" -eq 0 ]
}

@test "clean form accepted: result entry with no Vulnerabilities key" {
  _run_gate "printf '{\"Results\":[{\"Target\":\"os\"}]}'"
  [ "$status" -eq 0 ]
}

@test "clean form accepted: Vulnerabilities null" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":null}]}'"
  [ "$status" -eq 0 ]
}

@test "clean form accepted: Vulnerabilities empty array" {
  _run_gate "printf '{\"Results\":[{\"Vulnerabilities\":[]}]}'"
  [ "$status" -eq 0 ]
}

# ---- predicted-red: BASELINE ROW VALIDATION -----------------------------
#
# `[ "$count" -gt banana ]` prints "integer expression expected" and returns
# FALSE inside `if`; `set -e` does not abort. Both comparisons then silently
# no-op and the ratchet degrades to "always pass". Reproduced reaching
# "gate: PASS" before the fix.

_bl() { printf "$1" > "$TMP/bl"; echo "$TMP/bl"; }
_gate_bl() { # $1 = baseline file
  SCAN_CMD="$(printf 'printf %q' "$(_json_with CVE-2025-68121)")" \
    run "$GATE" k8s "$1" dummy-image:tag
}

@test "PREDICTED RED: non-numeric baseline" {
  _gate_bl "$(_bl 'k8s=banana\nid=CVE-2025-68121\n')"
  [ "$status" -ne 0 ]; [[ "$output" == *"not a non-negative integer"* ]]
}

@test "PREDICTED RED: negative baseline" {
  _gate_bl "$(_bl 'k8s=-5\nid=CVE-2025-68121\n')"
  [ "$status" -ne 0 ]; [[ "$output" == *"not a non-negative integer"* ]]
}

@test "PREDICTED RED: empty baseline value" {
  _gate_bl "$(_bl 'k8s=\nid=CVE-2025-68121\n')"
  [ "$status" -ne 0 ]; [[ "$output" == *"not a non-negative integer"* ]]
}

@test "PREDICTED RED: baseline with trailing junk" {
  _gate_bl "$(_bl 'k8s=8abc\nid=CVE-2025-68121\n')"
  [ "$status" -ne 0 ]; [[ "$output" == *"not a non-negative integer"* ]]
}

@test "PREDICTED RED: non-canonical baseline (007)" {
  _gate_bl "$(_bl 'k8s=007\nid=CVE-2025-68121\n')"
  [ "$status" -ne 0 ]; [[ "$output" == *"not a non-negative integer"* ]]
}

@test "PREDICTED RED: duplicate baseline rows" {
  _gate_bl "$(_bl 'k8s=8\nk8s=99\nid=CVE-2025-68121\n')"
  [ "$status" -ne 0 ]; [[ "$output" == *"exactly one required"* ]]
}

@test "baseline of 0 is valid and still ratchets" {
  _gate_bl "$(_bl 'k8s=0\nid=CVE-2025-68121\n')"
  [ "$status" -ne 0 ]; [[ "$output" == *"exceeds baseline"* ]]
}
