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
# Core principle (#373 round-2): ACT ONLY ON A CONFIRMED MISMATCH. Both refs
# must be present (non-empty) AND differ before we WARN/rebuild/hard-fail.
# Anything unverifiable — empty ref on either side, or two refs from different
# sources that aren't comparable — is "cannot tell" and resolves to "reuse
# silently" in BOTH normal and STRICT_CACHE modes. This matters because the
# DEFAULT path is `IMAGE_SOURCE=ghcr` and the published Containerfiles do not
# stamp `org.opencontainers.image.revision` yet (release.yml passes a REVISION
# build-arg the Containerfile drops). If "unverifiable" forced action, every
# default deploy would do a full bib rebuild (or hard-fail under STRICT_CACHE)
# even when nothing changed — defeating build_qcow2's skip-if-exists fast path.
# Teaching the Containerfiles to stamp the label is an out-of-scope follow-up;
# once it lands, the CONFIRMED-mismatch path starts firing for ghcr too.

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

# hbird_cache_build_id <source> <id>: a source-namespaced build identity for
# the sidecar, e.g. `local:9f2c…` or `ghcr:acef96c`. Namespacing keeps the two
# identity spaces apart so switching IMAGE_SOURCE between deploys does not read
# as a stale-template "mismatch" (a content-hash and a git-commit are never
# equal even when the bits are identical — #373 round-2 MED). An empty <id>
# (unverifiable — e.g. a GHCR image with no revision label) yields EMPTY so it
# is never recorded and never actionable.
hbird_cache_build_id() {
  local source="$1" id="$2"
  [[ -n "$id" ]] || return 0
  printf '%s:%s\n' "$source" "$id"
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
      # Cannot determine: no revision label on the image (the current default
      # — see header), or its commit is not in this checkout's git history.
      # Unverifiable is NOT a confirmed mismatch, so it resolves to "reuse
      # silently" in BOTH modes (#373 round-2 HIGH). Failing closed here would
      # make STRICT_CACHE=1 + the default ghcr path fail on EVERY deploy until
      # the Containerfiles stamp a revision label. Only a CONFIRMED drift
      # (rc 1 above) is actionable.
      return 0
      ;;
  esac
}

# hbird_assess_qcow2_cache <qcow> <expected-build-id> <label>
# For the build_qcow2 skip-if-exists template cache: is the cached qcow2
# CONFIRMED stale relative to the identity we are about to build from?
# <expected-build-id> and the recorded sidecar are source-namespaced
# (`<source>:<id>`, see hbird_cache_build_id).
#   rc 0  = reuse — fresh, OR unverifiable/cross-source (cannot confirm stale)
#   rc 10 = CONFIRMED stale -> auto-rebuild (caller sets FORCE_REBUILD=1)
#   rc 3  = CONFIRMED stale under STRICT_CACHE=1 (caller MUST abort)
hbird_assess_qcow2_cache() {
  local qcow="$1" expected="$2" label="$3"
  # Nothing cached yet — build_qcow2 will create it; no staleness possible.
  [[ -s "$qcow" ]] || return 0
  local cached
  cached="$(hbird_cache_read_ref "$qcow")"

  # Split each ref into <source> and <id> (split on the FIRST colon). A ref
  # with no colon (a legacy pre-namespacing sidecar) parses to id==src==whole,
  # so its source won't match a namespaced expected — and it falls through to
  # "cannot confirm -> reuse", which is the safe outcome.
  local exp_src exp_id cached_src cached_id
  exp_src="${expected%%:*}";   exp_id="${expected#*:}"
  cached_src="${cached%%:*}";  cached_id="${cached#*:}"

  # Act ONLY on a CONFIRMED mismatch: both ids present, SAME source, differing.
  # Empty id on either side (unverifiable — e.g. the default ghcr path's
  # missing revision label, or a sidecar-less legacy qcow2) or a cross-source
  # comparison (operator switched IMAGE_SOURCE) is "cannot tell" -> reuse
  # silently. This preserves skip-if-exists on the default path and avoids the
  # cross-mode churn that a content-hash-vs-git-commit compare would cause.
  # (#373 round-2 HIGH + MED.)
  [[ -n "$cached_id" && -n "$exp_id" ]] || return 0
  [[ "$cached_src" == "$exp_src" ]]     || return 0
  [[ "$cached_id" != "$exp_id" ]]       || return 0

  if hbird_cache_strict_enabled; then
    printf 'ERROR: cached %s (%s) build-ref %s != expected %s — STRICT_CACHE=1 refuses to reuse it. Set FORCE_REBUILD=1 to rebuild.\n' \
      "$label" "$qcow" "$cached" "$expected" >&2
    return 3
  fi
  printf 'WARN: cached %s build-ref %s differs from expected %s; forcing rebuild. (#373)\n' \
    "$label" "$cached" "$expected" >&2
  return 10
}
