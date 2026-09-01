#!/usr/bin/env bash
#
# Fixable-CRITICAL gate for the built OCI images (PR #413).
#
# Extracted from an inline `run:` block in .github/workflows/pr-validate.yml
# on purpose: two consecutive fail-open defects shipped in that block because
# embedded CI shell has no tests. This file is linted by the shellcheck job
# (scripts/*.sh) and exercised by tests/scripts/ci-trivy-gate.bats, whose
# predicted-red cases are the ABSENCE cases a fail-open gate cannot see.
#
# Gates, in order:
#   0. consistency  — a non-zero count with an empty id set means extraction
#                     broke; fail rather than infer a clean image.
#   1. known-id     — any id outside the committed allowlist fails, EVEN AT
#                     UNCHANGED COUNT (a count ratchet alone is satisfied by
#                     swapping N old ids for N new ones).
#   2. count ratchet— fail above the committed baseline; notice below it.
#
# The scan command is injectable via SCAN_CMD so the failure modes are
# testable without a container engine. Default is the real trivy invocation.
#
# Usage: ci-trivy-gate.sh <flavor> <baseline-file> <image-ref>
set -euo pipefail

flavor=${1:?flavor required}
baseline_file=${2:?baseline file required}
image=${3:?image ref required}

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
json="$workdir/trivy.json"
seen="$workdir/seen-ids.txt"
known="$workdir/known-ids.txt"

# BASELINE VALIDATION. A malformed row is a fail-open input: with
# `k8s=banana`, both `[ "$count" -gt banana ]` and `-lt` print "integer
# expression expected" and return FALSE inside `if`, which `set -e` does not
# abort on — so the ratchet silently degrades to "always pass". Duplicate
# rows produce a multiline operand with the same effect. Require exactly one
# row and a canonical non-negative decimal integer.
rows=$(grep -cE "^${flavor}=" "$baseline_file" || true)
if [ "$rows" -eq 0 ]; then
  echo "::error::no baseline row for ${flavor} in ${baseline_file}" >&2
  exit 1
fi
if [ "$rows" -gt 1 ]; then
  echo "::error::${flavor}: ${rows} baseline rows in ${baseline_file} — exactly one required." >&2
  exit 1
fi
baseline=$(grep -E "^${flavor}=" "$baseline_file" | cut -d= -f2)
if ! printf '%s' "$baseline" | grep -qE '^(0|[1-9][0-9]*)$'; then
  echo "::error::${flavor}: baseline '${baseline}' is not a non-negative integer — refusing to run a gate whose comparison cannot be trusted." >&2
  exit 1
fi

# FAIL-CLOSED SCAN. Never `|| true` and never `2>/dev/null` on a command that
# PRODUCES gate input: that turns scanner/docker/template failure into
# "0 findings", which SATISFIES the gate. A gate that reports green when the
# scanner did not run is worse than no gate at all.
# Production path is a direct argv call — no shell-evaluated command override.
scan_ok=0
docker run --rm -v /var/run/docker.sock:/var/run/docker.sock \
  "${TRIVY:-aquasec/trivy@sha256:ab70a02200597efa04748f210f793936eb647cbcdb0ea69cc30b226d6f5a22c7}" \
  image --scanners vuln --severity CRITICAL --ignore-unfixed \
  --format json "$image" > "$json" || scan_ok=$?
if [ "$scan_ok" -ne 0 ]; then
  echo "::error::${flavor}: scanner invocation failed (rc=${scan_ok}) — refusing to treat this as a clean image." >&2
  exit 1
fi

# SCHEMA VALIDATION, not mere presence. `has("Results")` is true for
# {"Results":null} and {"Results":"broken"}; the optional iterators `[]?`
# then yield nothing, count=0 and the id set is empty — indistinguishable
# from a genuinely clean scan. Require .Results to be an ARRAY whose entries
# are objects whose Vulnerabilities is absent, null, or an array. This still
# accepts the legitimate clean forms ({"Results":[]} and result objects with
# no findings).
if ! jq -e '
      (.Results | type) == "array"
      and all(.Results[];
        type == "object"
        and (
          (has("Vulnerabilities") | not)
          or (.Vulnerabilities == null)
          or (
            (.Vulnerabilities | type) == "array"
            and all(.Vulnerabilities[];
              type == "object"
              and (.VulnerabilityID | type) == "string"
              and ((.VulnerabilityID | gsub("^[[:space:]]+|[[:space:]]+$"; "")) | length) > 0
            )
          )
        )
      )
    ' "$json" > /dev/null 2>&1; then
  echo "::error::${flavor}: scanner output does not match the expected scan schema (.Results must contain objects with absent/null/array Vulnerabilities, and every vulnerability must have a non-empty string VulnerabilityID) — refusing to infer 0 findings." >&2
  exit 1
fi

# NO prefix filter. The namespace of risk is open-ended — RUSTSEC, RHSA, DSA
# and vendor ids are real. Filtering to ^(CVE|GHSA)- would let a critical in
# another namespace through unseen. Every non-empty id is gate input.
count=$(jq '[.Results[]?.Vulnerabilities[]?] | length' "$json")
jq -r '.Results[]?.Vulnerabilities[]?.VulnerabilityID // empty' "$json" \
  | sed '/^[[:space:]]*$/d' | sort -u > "$seen"

echo "fixable CRITICAL count: ${count} (baseline ${baseline})"
echo "distinct ids observed:"
sed 's/^/  /' "$seen"

# GATE 0 — an empty id set is credible ONLY at a count of exactly zero.
if [ "$count" -gt 0 ] && [ ! -s "$seen" ]; then
  echo "::error::${flavor}: count=${count} but no ids extracted — extraction is broken; failing closed." >&2
  exit 1
fi

# GATE 1 — known-id allowlist.
# This reads a COMMITTED file, not scanner output, so `|| true` here is NOT
# the fail-open pattern under review: an empty allowlist makes the gate
# MAXIMALLY strict — every observed id becomes unknown and GATE 1 fails
# loudly below. Without it, `pipefail` turns "no id= lines" into a bare
# rc=1 with no diagnostic, which is fail-closed but unexplained.
{ grep -E '^id=' "$baseline_file" || true; } \
  | sed 's/#.*//' | cut -d= -f2- | cut -d' ' -f1 \
  | sed 's/[[:space:]]//g; /^$/d' | sort -u > "$known"
new_ids=$(comm -23 "$seen" "$known")
if [ -n "$new_ids" ]; then
  echo "::error::${flavor}: fixable CRITICAL id(s) not in the committed allowlist:" >&2
  printf '%s\n' "$new_ids" | sed 's/^/  /' >&2
  echo "Investigate; add to ${baseline_file} only as explicit, reviewed acceptance." >&2
  exit 1
fi

# GATE 2 — count ratchet.
if [ "$count" -gt "$baseline" ]; then
  echo "::error::${flavor}: ${count} fixable CRITICAL vulns exceeds baseline ${baseline}." >&2
  exit 1
fi
if [ "$count" -lt "$baseline" ]; then
  echo "::notice::${flavor}: criticals dropped to ${count} (baseline ${baseline}) — lower the baseline in ${baseline_file} to lock in the improvement."
fi
echo "gate: PASS (${flavor})"
