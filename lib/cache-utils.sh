#!/usr/bin/env bash
# lib/cache-utils.sh — qcow2 / image cache-freshness helpers (#373).
#
# Why this exists
# ---------------
# deploy-cluster.sh acquires an image (pull from GHCR, or `make` a local
# build) and then bakes a qcow2 *template* under POOL_DIR via
# lib/build-common.sh's build_qcow2(). Both layers cache:
#
#   * GHCR `:latest` lags HEAD — a `podman pull` of `:latest` can return an
#     image built from a commit that PREDATES the operator's on-disk
#     Containerfile change. The deploy then "succeeds" against the wrong
#     bits and the boot-test reports a FALSE-POSITIVE green (#373, surfaced
#     by PR #367's Cilium bump).
#   * build_qcow2() has a skip-if-exists shortcut: if ${name}.qcow2 already
#     exists and FORCE_REBUILD!=1 it reuses it — even when the on-disk
#     Containerfile changed since that template was baked.
#
# These helpers detect both. Policy:
#   * default            — WARN loudly; for the locally-rebuildable case also
#                          signal an auto-rebuild (caller sets FORCE_REBUILD=1).
#   * STRICT_CACHE=1      — hard-fail instead (CI / boot-test gate; mirrors the
#                          HBIRD_REMOTE_STRICT fail-closed model from #371).
#
# Every function is small and stubbable (podman/git are the only externals)
# so tests/lib/cache-utils.bats exercises them without root, podman, or a
# real cluster.
#
# NOTE: the GHCR image revision check reads the OCI label
# `org.opencontainers.image.revision`. The published Containerfiles do not
# emit that label yet (release.yml passes a REVISION build-arg that the
# Containerfile currently drops on the floor) — so on today's images the
# GHCR check degrades to "cannot verify" (WARN, or fail under STRICT_CACHE).
# That is the correct fail-closed posture for a boot-test gate. Teaching the
# Containerfiles to stamp the label is a separate, out-of-scope follow-up.

# Guard against double-source (deploy-cluster.sh + a test both sourcing).
[[ -n "${_HBIRD_CACHE_UTILS_SH:-}" ]] && return 0
_HBIRD_CACHE_UTILS_SH=1

# hbird_cache_strict_enabled: true when STRICT_CACHE=1.
hbird_cache_strict_enabled() { [[ "${STRICT_CACHE:-0}" == "1" ]]; }

# hbird_containerfile_ref <path>...: a stable short identity of the on-disk
# Containerfile(s). Hashes file *contents* (not names or mtimes) so a no-op
# `touch` does not churn the cache. Prints 12 hex chars, or empty on error.
hbird_containerfile_ref() {
  local sum
  sum="$(cat -- "$@" 2>/dev/null | sha256sum 2>/dev/null)" || return 0
  printf '%s\n' "${sum:0:12}"
}

# hbird_image_vcs_ref <image-ref>: the OCI revision (vcs-ref) label of a
# locally-available podman image, or empty if the image/label is absent.
hbird_image_vcs_ref() {
  podman image inspect \
    --format '{{ index .Config.Labels "org.opencontainers.image.revision" }}' \
    "$1" 2>/dev/null || true
}

# hbird_cache_sidecar_path <qcow>: path of the sidecar that records the
# build identity a cached qcow2 template was baked from.
hbird_cache_sidecar_path() { printf '%s.build-ref\n' "$1"; }

# hbird_cache_read_ref <qcow>: the recorded build identity, or empty.
hbird_cache_read_ref() {
  local sc
  sc="$(hbird_cache_sidecar_path "$1")"
  [[ -r "$sc" ]] && cat -- "$sc" || true
}

# hbird_cache_write_ref <qcow> <ref>: record the build identity atomically
# (mktemp + mv) so a crash mid-write can't leave a half-written sidecar.
# No-op (success) when <ref> is empty — we never record an unknown identity.
hbird_cache_write_ref() {
  local qcow="$1" ref="$2" sc tmp
  [[ -n "$ref" ]] || return 0
  sc="$(hbird_cache_sidecar_path "$qcow")"
  tmp="$(mktemp "${sc}.XXXXXX" 2>/dev/null)" || return 1
  printf '%s\n' "$ref" >"$tmp" && mv -f "$tmp" "$sc"
}

# hbird_containerfile_changed_since <git-ref> <path>...
#   rc 0 = Containerfile(s) unchanged since <git-ref>
#   rc 1 = changed since <git-ref>
#   rc 2 = cannot determine (empty ref, ref not in local history, or the
#          working tree is not a git checkout)
hbird_containerfile_changed_since() {
  local ref="$1"
  shift
  [[ -n "$ref" ]] || return 2
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || return 2
  git cat-file -e "${ref}^{commit}" 2>/dev/null || return 2
  if git diff --quiet "$ref" -- "$@" 2>/dev/null; then
    return 0
  fi
  return 1
}

# hbird_assess_ghcr_image <image-ref> <label> <containerfile>...
# For IMAGE_SOURCE=ghcr: confirm the PULLED image actually reflects the
# on-disk Containerfile. There is no local rebuild path here (that needs
# IMAGE_SOURCE=local), so the outcome is WARN-or-abort, never auto-rebuild.
#   rc 0 = in sync, OR drift/unverifiable in non-strict mode (WARN emitted)
#   rc 3 = drift/unverifiable under STRICT_CACHE=1 (caller MUST abort)
hbird_assess_ghcr_image() {
  local img="$1" label="$2"
  shift 2
  local ref
  ref="$(hbird_image_vcs_ref "$img")"
  hbird_containerfile_changed_since "$ref" "$@"
  case $? in
    0)
      return 0
      ;;
    1)
      if hbird_cache_strict_enabled; then
        printf 'ERROR: pulled %s (vcs-ref %s) predates on-disk %s — STRICT_CACHE=1 refuses a stale boot-test. Rebuild from source: IMAGE_SOURCE=local FORCE_REBUILD=1.\n' \
          "$label" "$ref" "$*" >&2
        return 3
      fi
      printf 'WARN: pulled %s (vcs-ref %s) does NOT reflect on-disk %s; this deploy tests the PUBLISHED image, not your local change. Use IMAGE_SOURCE=local FORCE_REBUILD=1 to test local edits. (#373)\n' \
        "$label" "$ref" "$*" >&2
      return 0
      ;;
    *)
      # Cannot determine: no revision label on the image, or its commit is not
      # in this checkout's git history. A boot-test gate (STRICT_CACHE=1) must
      # fail closed — an unverifiable image is exactly the false-positive #373
      # is about. But the DEFAULT deploy path is ghcr+latest and the published
      # images do not stamp a revision label yet, so nagging on every routine
      # interactive deploy would be pure noise — stay silent there.
      if hbird_cache_strict_enabled; then
        printf 'ERROR: cannot verify pulled %s against on-disk %s (vcs-ref %s not in local git history) — STRICT_CACHE=1 refuses an unverifiable boot-test. Rebuild from source: IMAGE_SOURCE=local FORCE_REBUILD=1.\n' \
          "$label" "$*" "${ref:-<none>}" >&2
        return 3
      fi
      return 0
      ;;
  esac
}

# hbird_assess_qcow2_cache <qcow> <expected-build-id> <label>
# For the build_qcow2 skip-if-exists template cache: is the cached qcow2
# stale relative to the identity we are about to build from?
#   rc 0  = fresh, reuse
#   rc 10 = stale/unverifiable -> auto-rebuild (caller sets FORCE_REBUILD=1)
#   rc 3  = stale/unverifiable under STRICT_CACHE=1 (caller MUST abort)
hbird_assess_qcow2_cache() {
  local qcow="$1" expected="$2" label="$3"
  # Nothing cached yet — build_qcow2 will create it; no staleness possible.
  [[ -s "$qcow" ]] || return 0
  local cached
  cached="$(hbird_cache_read_ref "$qcow")"
  if [[ -n "$cached" && -n "$expected" && "$cached" == "$expected" ]]; then
    return 0
  fi
  if hbird_cache_strict_enabled; then
    printf 'ERROR: cached %s (%s) build-ref %s != expected %s — STRICT_CACHE=1 refuses to reuse it. Set FORCE_REBUILD=1 to rebuild.\n' \
      "$label" "$qcow" "${cached:-<none>}" "${expected:-<unknown>}" >&2
    return 3
  fi
  printf 'WARN: cached %s build-ref %s differs from expected %s; forcing rebuild. (#373)\n' \
    "$label" "${cached:-<none>}" "${expected:-<unknown>}" >&2
  return 10
}
