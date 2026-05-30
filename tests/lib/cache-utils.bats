#!/usr/bin/env bats
#
# Unit tests for lib/cache-utils.sh — the qcow2/image cache-freshness helpers
# that back deploy-cluster.sh's #373 stale-image detection.
#
# These exercise the pure logic without root, podman, a real cluster, or a
# real GHCR image: `podman` and `git` are stubbed as shell functions so each
# branch (in-sync / drift / unverifiable, fresh / stale / missing-sidecar,
# strict vs non-strict) is hit deterministically.
#
# Run via:
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/lib/cache-utils.bats
# OR locally:
#   bats tests/lib/cache-utils.bats

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  LIB="${REPO_ROOT}/lib/cache-utils.sh"
  [ -r "$LIB" ] || { echo "FATAL: $LIB not readable" >&2; return 1; }
  # Clean slate — never inherit a strict flag from the CI environment.
  unset STRICT_CACHE
  # shellcheck disable=SC1090
  source "$LIB"
}

# ---------------------------------------------------------------------------
# hbird_containerfile_ref — content-addressed, stable
# ---------------------------------------------------------------------------

@test "containerfile_ref: identical content -> identical ref" {
  printf 'FROM scratch\nRUN true\n' > "${BATS_TEST_TMPDIR}/a"
  printf 'FROM scratch\nRUN true\n' > "${BATS_TEST_TMPDIR}/b"
  a="$(hbird_containerfile_ref "${BATS_TEST_TMPDIR}/a")"
  b="$(hbird_containerfile_ref "${BATS_TEST_TMPDIR}/b")"
  [ -n "$a" ]
  [ "$a" = "$b" ]
  # 12 hex chars.
  [[ "$a" =~ ^[0-9a-f]{12}$ ]]
}

@test "containerfile_ref: changed content -> different ref" {
  printf 'FROM scratch\n' > "${BATS_TEST_TMPDIR}/cf"
  before="$(hbird_containerfile_ref "${BATS_TEST_TMPDIR}/cf")"
  printf 'FROM scratch\nRUN echo changed\n' > "${BATS_TEST_TMPDIR}/cf"
  after="$(hbird_containerfile_ref "${BATS_TEST_TMPDIR}/cf")"
  [ "$before" != "$after" ]
}

# ---------------------------------------------------------------------------
# sidecar read / write round-trip
# ---------------------------------------------------------------------------

@test "cache sidecar: write then read round-trips the ref" {
  qcow="${BATS_TEST_TMPDIR}/tpl.qcow2"
  : > "$qcow"
  run hbird_cache_sidecar_path "$qcow"
  [ "$output" = "${qcow}.build-ref" ]

  hbird_cache_write_ref "$qcow" "deadbeef1234"
  [ "$(hbird_cache_read_ref "$qcow")" = "deadbeef1234" ]
}

@test "cache sidecar: read with no sidecar yields empty" {
  qcow="${BATS_TEST_TMPDIR}/none.qcow2"
  [ -z "$(hbird_cache_read_ref "$qcow")" ]
}

@test "cache sidecar: write with empty ref is a no-op (never record unknown)" {
  qcow="${BATS_TEST_TMPDIR}/empty.qcow2"
  run hbird_cache_write_ref "$qcow" ""
  [ "$status" -eq 0 ]
  [ ! -e "${qcow}.build-ref" ]
}

# ---------------------------------------------------------------------------
# hbird_containerfile_changed_since — git-backed (git stubbed)
# ---------------------------------------------------------------------------

@test "changed_since: empty ref -> cannot determine (rc 2)" {
  run hbird_containerfile_changed_since "" containers/k8s/Containerfile
  [ "$status" -eq 2 ]
}

@test "changed_since: unchanged since ref -> rc 0" {
  git() {
    case "$1 $2" in
      "rev-parse --is-inside-work-tree") return 0 ;;
      "cat-file -e") return 0 ;;
      "diff --quiet") return 0 ;;  # no diff
    esac
  }
  export -f git
  run hbird_containerfile_changed_since abc123 containers/k8s/Containerfile
  [ "$status" -eq 0 ]
}

@test "changed_since: changed since ref -> rc 1" {
  git() {
    case "$1 $2" in
      "rev-parse --is-inside-work-tree") return 0 ;;
      "cat-file -e") return 0 ;;
      "diff --quiet") return 1 ;;  # diff present
    esac
  }
  export -f git
  run hbird_containerfile_changed_since abc123 containers/k8s/Containerfile
  [ "$status" -eq 1 ]
}

@test "changed_since: ref not in history -> cannot determine (rc 2)" {
  git() {
    case "$1 $2" in
      "rev-parse --is-inside-work-tree") return 0 ;;
      "cat-file -e") return 1 ;;  # unknown commit
    esac
  }
  export -f git
  run hbird_containerfile_changed_since deadbeef containers/k8s/Containerfile
  [ "$status" -eq 2 ]
}

# ---------------------------------------------------------------------------
# hbird_assess_ghcr_image — the #373 false-positive gate
# ---------------------------------------------------------------------------

_stub_image_ref() {  # $1 = revision the stubbed podman reports
  eval "podman() { printf '%s\n' '$1'; }"
  export -f podman
}

@test "assess_ghcr_image: in-sync image -> rc 0, silent" {
  _stub_image_ref "acef96c"
  git() { case "$1 $2" in
            "rev-parse --is-inside-work-tree") return 0 ;;
            "cat-file -e") return 0 ;;
            "diff --quiet") return 0 ;;  # Containerfile unchanged since image
          esac; }
  export -f git
  run hbird_assess_ghcr_image ghcr.io/x:latest "CP image" containers/k8s/Containerfile
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "assess_ghcr_image: confirmed drift, non-strict -> rc 0 + WARN" {
  _stub_image_ref "991438b3"
  git() { case "$1 $2" in
            "rev-parse --is-inside-work-tree") return 0 ;;
            "cat-file -e") return 0 ;;
            "diff --quiet") return 1 ;;  # Containerfile changed since image
          esac; }
  export -f git
  run hbird_assess_ghcr_image ghcr.io/x:latest "CP image" containers/k8s/Containerfile
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"does NOT reflect"* ]]
}

@test "assess_ghcr_image: confirmed drift, STRICT_CACHE=1 -> rc 3 + ERROR" {
  _stub_image_ref "991438b3"
  git() { case "$1 $2" in
            "rev-parse --is-inside-work-tree") return 0 ;;
            "cat-file -e") return 0 ;;
            "diff --quiet") return 1 ;;
          esac; }
  export -f git
  STRICT_CACHE=1 run hbird_assess_ghcr_image ghcr.io/x:latest "CP image" containers/k8s/Containerfile
  [ "$status" -eq 3 ]
  [[ "$output" == *"ERROR"* ]]
  [[ "$output" == *"STRICT_CACHE=1"* ]]
}

@test "assess_ghcr_image: unverifiable (no label), non-strict -> rc 0, silent" {
  _stub_image_ref ""   # no revision label
  run hbird_assess_ghcr_image ghcr.io/x:latest "CP image" containers/k8s/Containerfile
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "assess_ghcr_image: unverifiable (no label), STRICT_CACHE=1 -> rc 3 (fail closed)" {
  _stub_image_ref ""
  STRICT_CACHE=1 run hbird_assess_ghcr_image ghcr.io/x:latest "CP image" containers/k8s/Containerfile
  [ "$status" -eq 3 ]
  [[ "$output" == *"cannot verify"* ]]
}

# ---------------------------------------------------------------------------
# hbird_assess_qcow2_cache — the build_qcow2 skip-if-exists staleness gate
# ---------------------------------------------------------------------------

@test "assess_qcow2_cache: no cached qcow2 -> rc 0 (build will create it)" {
  run hbird_assess_qcow2_cache "${BATS_TEST_TMPDIR}/absent.qcow2" abc "CP image"
  [ "$status" -eq 0 ]
}

@test "assess_qcow2_cache: sidecar matches expected -> rc 0 (fresh)" {
  qcow="${BATS_TEST_TMPDIR}/fresh.qcow2"; printf qcow2data > "$qcow"
  hbird_cache_write_ref "$qcow" "abc123"
  run hbird_assess_qcow2_cache "$qcow" "abc123" "CP image"
  [ "$status" -eq 0 ]
}

@test "assess_qcow2_cache: sidecar differs, non-strict -> rc 10 + WARN (auto-rebuild)" {
  qcow="${BATS_TEST_TMPDIR}/stale.qcow2"; printf qcow2data > "$qcow"
  hbird_cache_write_ref "$qcow" "oldref00"
  run hbird_assess_qcow2_cache "$qcow" "newref99" "CP image"
  [ "$status" -eq 10 ]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"forcing rebuild"* ]]
}

@test "assess_qcow2_cache: sidecar differs, STRICT_CACHE=1 -> rc 3 (fail closed)" {
  qcow="${BATS_TEST_TMPDIR}/stale2.qcow2"; printf qcow2data > "$qcow"
  hbird_cache_write_ref "$qcow" "oldref00"
  STRICT_CACHE=1 run hbird_assess_qcow2_cache "$qcow" "newref99" "CP image"
  [ "$status" -eq 3 ]
  [[ "$output" == *"ERROR"* ]]
}

@test "assess_qcow2_cache: missing sidecar (legacy qcow2), non-strict -> rc 10" {
  qcow="${BATS_TEST_TMPDIR}/legacy.qcow2"; printf qcow2data > "$qcow"   # exists, no sidecar
  run hbird_assess_qcow2_cache "$qcow" "newref99" "CP image"
  [ "$status" -eq 10 ]
}

@test "assess_qcow2_cache: missing sidecar (legacy qcow2), STRICT_CACHE=1 -> rc 3" {
  qcow="${BATS_TEST_TMPDIR}/legacy2.qcow2"; printf qcow2data > "$qcow"
  STRICT_CACHE=1 run hbird_assess_qcow2_cache "$qcow" "newref99" "CP image"
  [ "$status" -eq 3 ]
}
