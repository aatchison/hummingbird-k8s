//! Pure cache-assessment logic for qcow2 template freshness checks.
//!
//! Mirrors `the deleted lib/cache-utils.sh::hbird_assess_qcow2_cache` from the bash
//! twin. All functions in this module are pure (no subprocesses, no
//! network); they exist so callers can unit-test the policy without running
//! real `podman` commands.
//!
//! # Policy
//!
//! Only act on a **confirmed** mismatch: both the cached and expected IDs
//! are non-empty, they share the same source prefix (e.g. both `"ghcr:…"`),
//! and the hash/revision differs. Any other combination (missing ID, cross-
//! source comparison) is treated as "cannot confirm stale → reuse".
//!
//! On confirmed stale:
//! - `STRICT_CACHE=0` → [`CacheAssessResult::Rebuild`] (auto-rebuild, log WARN)
//! - `STRICT_CACHE=1` → [`CacheAssessResult::StrictFail`] (hard-fail)

use std::io::{self, Write};
use std::path::Path;

// ---- Public types ----------------------------------------------------------

/// Decision returned by [`assess_qcow2_cache`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheAssessResult {
    /// The cached artefact is fresh (or unverifiable); reuse it.
    Reuse,
    /// The cached artefact is confirmed stale; rebuild it.
    Rebuild,
    /// Confirmed stale and `STRICT_CACHE=1` — caller must hard-fail.
    StrictFail,
}

// ---- Public functions ------------------------------------------------------

/// Decide whether to reuse a cached qcow2 template.
///
/// Only acts on a **confirmed mismatch**: both IDs non-empty, same source
/// prefix (split on `:`), and different value.
///
/// Under `STRICT_CACHE=1` (`strict=true`), **unverifiable** freshness (either
/// ID missing or empty) also returns [`StrictFail`] rather than [`Reuse`].
/// This mirrors the bash twin `the deleted lib/cache-utils.sh::hbird_assess_ghcr_image`
/// (`rc 3` for unverifiable under `STRICT_CACHE=1`, merged PR #25 `fbe2082`).
///
/// # Arguments
///
/// - `cached_ref` — the build ID stored in the `.build-ref` sidecar file.
/// - `expected_ref` — the build ID computed from the current image/source.
/// - `strict` — if `true`, confirmed stale AND unverifiable both return
///   [`StrictFail`].
///
/// [`Reuse`]: CacheAssessResult::Reuse
pub fn assess_qcow2_cache(
    cached_ref: Option<&str>,
    expected_ref: Option<&str>,
    strict: bool,
) -> CacheAssessResult {
    // #9c parity: under STRICT_CACHE=1, unverifiable (missing/empty IDs) → StrictFail.
    let (Some(cached), Some(expected)) = (cached_ref, expected_ref) else {
        if strict {
            return CacheAssessResult::StrictFail;
        }
        // One or both IDs missing — cannot confirm stale.
        return CacheAssessResult::Reuse;
    };
    if cached.is_empty() || expected.is_empty() {
        if strict {
            return CacheAssessResult::StrictFail;
        }
        return CacheAssessResult::Reuse;
    }
    // Extract source prefix (everything before the first `:`).
    let cached_prefix = cached.split(':').next().unwrap_or("");
    let expected_prefix = expected.split(':').next().unwrap_or("");
    if cached_prefix != expected_prefix {
        // Cross-source comparison (e.g. cached from "ghcr", expected is "local")
        // — cannot confirm stale.
        return CacheAssessResult::Reuse;
    }
    if cached == expected {
        return CacheAssessResult::Reuse;
    }
    // Confirmed mismatch — same source prefix, different value.
    if strict {
        CacheAssessResult::StrictFail
    } else {
        CacheAssessResult::Rebuild
    }
}

/// FNV-1a content hash of a file, returned as a 12 hex-char string.
///
/// Returns `None` on any I/O error (unreadable path, directory, etc.).
/// The 12-char truncation mirrors the bash twin's usage of short hashes
/// for human-readable build IDs (`"local:<12-hex>"`).
pub fn containerfile_hash(path: &Path) -> Option<String> {
    let contents = std::fs::read(path).ok()?;
    let hash = fnv1a_64(&contents);
    // Take the lower 48 bits → 12 hex chars (sufficient for build-ID
    // collision resistance on a single repository's Containerfiles).
    Some(format!("{:012x}", hash & 0x0000_ffff_ffff_ffff))
}

/// Build a source-namespaced build ID string.
///
/// Returns `None` when `id` is empty (unverifiable). Returns
/// `Some("source:id")` otherwise.
///
/// # Examples
///
/// ```
/// use hbird_cli::cache::build_id;
/// assert_eq!(build_id("ghcr", "abc123"), Some("ghcr:abc123".to_string()));
/// assert_eq!(build_id("ghcr", ""), None);
/// ```
pub fn build_id(source: &str, id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    Some(format!("{source}:{id}"))
}

/// Read the sidecar file alongside a qcow2 template.
///
/// The sidecar lives at `<qcow_path>.build-ref` (plain text, one line).
/// Returns `None` when the file is absent or unreadable.
pub fn read_sidecar(qcow: &Path) -> Option<String> {
    let sidecar = sidecar_path(qcow);
    let raw = std::fs::read_to_string(sidecar).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Write the sidecar atomically alongside a qcow2 template.
///
/// The sidecar path is `<qcow_path>.build-ref`. The write is atomic:
/// a temp file is written next to the target and then renamed. This avoids
/// leaving a half-written sidecar if the process is killed mid-write.
///
/// No-op (returns `Ok(())`) when `id` is empty.
pub fn write_sidecar(qcow: &Path, id: &str) -> io::Result<()> {
    if id.is_empty() {
        return Ok(());
    }
    let sidecar = sidecar_path(qcow);
    // Temp file in the same directory so rename is atomic (same filesystem).
    let parent = sidecar.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(".build-ref-tmp-{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        writeln!(f, "{id}")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &sidecar)
}

// ---- Private helpers -------------------------------------------------------

fn sidecar_path(qcow: &Path) -> std::path::PathBuf {
    let mut p = qcow.as_os_str().to_owned();
    p.push(".build-ref");
    std::path::PathBuf::from(p)
}

/// FNV-1a 64-bit hash of `data`.
///
/// Pure-Rust, no external crates. Spec:
/// - offset_basis = 14695981039346656037u64
/// - prime = 1099511628211u64
fn fnv1a_64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    let mut hash = OFFSET_BASIS;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

// ---- Tests -----------------------------------------------------------------

// ---- GHCR image freshness (#373) -------------------------------------------

/// Whether a pulled image reflects the on-disk Containerfile.
///
/// Bash twin: `the deleted lib/cache-utils.sh::hbird_containerfile_changed_since` rc
/// 0/1/2, consumed by `hbird_assess_ghcr_image`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFreshness {
    /// The Containerfile is unchanged since the image's revision commit.
    InSync,
    /// On-disk Containerfile has drifted from the image's revision commit.
    Drifted,
    /// Cannot prove either way: no revision label, no repo available, or the
    /// commit is not in this checkout's history.
    Unverifiable,
}

/// Outcome of assessing a pulled GHCR/Forgejo image.
///
/// There is no local rebuild path here — rebuilding needs `IMAGE_SOURCE=local`
/// — so the outcome is warn-or-abort, never auto-rebuild. Mirrors the bash
/// twin's rc 0 (proceed, possibly after a WARN) / rc 3 (caller MUST abort).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhcrAssessResult {
    /// Proceed silently.
    Fresh,
    /// Proceed, but emit this operator-facing warning first.
    Warn(String),
    /// Abort: `STRICT_CACHE=1` and freshness is drifted or unprovable.
    StrictFail(String),
}

/// Classify a pulled image against the on-disk Containerfile.
///
/// Pure: callers supply the already-determined [`ImageFreshness`] and the
/// image's revision label, so this is exhaustively unit-testable without
/// podman or git. The impure lookups live in [`image_vcs_ref`] and
/// [`containerfile_changed_since`].
///
/// Wording is preserved from the bash twin verbatim — operators grep
/// "does NOT reflect on-disk" and "cannot prove freshness".
pub fn assess_ghcr_image(
    freshness: ImageFreshness,
    label: &str,
    vcs_ref: &str,
    containerfiles: &str,
    strict: bool,
) -> GhcrAssessResult {
    match freshness {
        ImageFreshness::InSync => GhcrAssessResult::Fresh,
        ImageFreshness::Drifted if strict => GhcrAssessResult::StrictFail(format!(
            "pulled {label} (vcs-ref {vcs_ref}) predates on-disk {containerfiles} — \
             STRICT_CACHE=1 refuses a stale boot-test. Rebuild from source: \
             IMAGE_SOURCE=local FORCE_REBUILD=1."
        )),
        ImageFreshness::Drifted => GhcrAssessResult::Warn(format!(
            "WARN: pulled {label} (vcs-ref {vcs_ref}) does NOT reflect on-disk \
             {containerfiles}; this deploy tests the PUBLISHED image, not your local \
             change. Use IMAGE_SOURCE=local FORCE_REBUILD=1 to test local edits. (#373)"
        )),
        ImageFreshness::Unverifiable if strict => GhcrAssessResult::StrictFail(
            "cannot prove freshness under STRICT_CACHE=1; rebuild with FORCE_REBUILD=1. \
             (Freshness needs the repo: run from a checkout or pass --repo-root.)"
                .to_string(),
        ),
        ImageFreshness::Unverifiable => GhcrAssessResult::Fresh,
    }
}

/// Read an image's `org.opencontainers.image.revision` label via podman.
///
/// Returns `None` when podman is absent, the image is not present, or the
/// label is unset — all of which the caller treats as [`ImageFreshness::
/// Unverifiable`], matching the bash twin's `|| true`.
pub fn image_vcs_ref(image: &str) -> Option<String> {
    let out = std::process::Command::new("podman")
        .args([
            "image",
            "inspect",
            "--format",
            "{{ index .Config.Labels \"org.opencontainers.image.revision\" }}",
            image,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "<no value>" {
        None
    } else {
        Some(s)
    }
}

/// Is the on-disk Containerfile changed since `vcs_ref`?
///
/// Needs a git checkout, so under the checkout-free operating model (where
/// `hbird` runs from an installed binary with no repo) this returns
/// [`ImageFreshness::Unverifiable`] rather than failing — the caller decides
/// whether that is fatal via `STRICT_CACHE`.
pub fn containerfile_changed_since(
    repo_root: &Path,
    vcs_ref: &str,
    containerfiles: &[&str],
) -> ImageFreshness {
    if vcs_ref.is_empty() {
        return ImageFreshness::Unverifiable;
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(repo_root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    if git(&["rev-parse", "--is-inside-work-tree"]).is_none() {
        return ImageFreshness::Unverifiable;
    }
    // Commit must exist in THIS checkout's history, else we cannot compare.
    if git(&["cat-file", "-e", &format!("{vcs_ref}^{{commit}}")]).is_none() {
        return ImageFreshness::Unverifiable;
    }
    let mut args: Vec<&str> = vec!["diff", "--quiet", vcs_ref, "--"];
    args.extend_from_slice(containerfiles);
    match git(&args) {
        Some(_) => ImageFreshness::InSync,
        None => ImageFreshness::Drifted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- assess_qcow2_cache ------------------------------------------------

    #[test]
    fn assess_qcow2_cache_reuses_when_matching() {
        let r = assess_qcow2_cache(Some("ghcr:abc123"), Some("ghcr:abc123"), false);
        assert_eq!(r, CacheAssessResult::Reuse);
    }

    #[test]
    fn assess_qcow2_cache_rebuilds_on_stale_id() {
        let r = assess_qcow2_cache(Some("ghcr:abc123"), Some("ghcr:def456"), false);
        assert_eq!(r, CacheAssessResult::Rebuild);
    }

    #[test]
    fn assess_qcow2_cache_strict_fails_on_stale() {
        let r = assess_qcow2_cache(Some("ghcr:abc123"), Some("ghcr:def456"), true);
        assert_eq!(r, CacheAssessResult::StrictFail);
    }

    #[test]
    fn assess_qcow2_cache_reuses_when_either_empty_non_strict() {
        // Non-strict: unverifiable IDs → Reuse (cannot confirm stale).
        // cached empty, strict=false
        assert_eq!(
            assess_qcow2_cache(Some(""), Some("ghcr:abc123"), false),
            CacheAssessResult::Reuse
        );
        // expected empty, strict=false
        assert_eq!(
            assess_qcow2_cache(Some("ghcr:abc123"), Some(""), false),
            CacheAssessResult::Reuse
        );
        // both None, strict=false
        assert_eq!(
            assess_qcow2_cache(None, None, false),
            CacheAssessResult::Reuse
        );
        // one None, strict=false
        assert_eq!(
            assess_qcow2_cache(None, Some("ghcr:abc123"), false),
            CacheAssessResult::Reuse
        );
    }

    // #9c parity: under STRICT_CACHE=1, unverifiable freshness → StrictFail.
    #[test]
    fn assess_qcow2_cache_strict_fails_on_unverifiable() {
        // cached empty, strict=true → StrictFail
        assert_eq!(
            assess_qcow2_cache(Some(""), Some("ghcr:abc123"), true),
            CacheAssessResult::StrictFail
        );
        // expected empty, strict=true → StrictFail
        assert_eq!(
            assess_qcow2_cache(Some("ghcr:abc123"), Some(""), true),
            CacheAssessResult::StrictFail
        );
        // both None, strict=true → StrictFail
        assert_eq!(
            assess_qcow2_cache(None, None, true),
            CacheAssessResult::StrictFail
        );
        // one None, strict=true → StrictFail
        assert_eq!(
            assess_qcow2_cache(None, Some("ghcr:abc123"), true),
            CacheAssessResult::StrictFail
        );
    }

    #[test]
    fn assess_qcow2_cache_reuses_on_cross_source() {
        // cached from ghcr, expected from local — different source prefix
        let r = assess_qcow2_cache(Some("ghcr:abc123"), Some("local:def456"), true);
        assert_eq!(r, CacheAssessResult::Reuse);
    }

    // ---- containerfile_hash ------------------------------------------------

    #[test]
    fn containerfile_hash_produces_12hex() {
        // Write a tiny temp file and verify the hash is exactly 12 hex chars.
        let dir = std::env::temp_dir().join(format!("hbird-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Containerfile");
        std::fs::write(&path, b"FROM scratch\n").unwrap();

        let h = containerfile_hash(&path).expect("hash succeeds");
        assert_eq!(h.len(), 12, "hash must be 12 chars, got: {h}");
        assert!(
            h.chars().all(|c| c.is_ascii_hexdigit()),
            "hash must be hex, got: {h}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- build_id ----------------------------------------------------------

    #[test]
    fn build_id_returns_none_for_empty_id() {
        assert_eq!(build_id("ghcr", ""), None);
        assert_eq!(build_id("local", ""), None);
    }

    #[test]
    fn build_id_formats_correctly() {
        assert_eq!(build_id("ghcr", "abc123"), Some("ghcr:abc123".to_string()));
        assert_eq!(
            build_id("forgejo", "deadbeef"),
            Some("forgejo:deadbeef".to_string())
        );
        assert_eq!(
            build_id("local", "000011112222"),
            Some("local:000011112222".to_string())
        );
    }

    // ---- GHCR freshness (#373) -----------------------------------------

    #[test]
    fn ghcr_in_sync_is_fresh_in_both_modes() {
        for strict in [false, true] {
            assert_eq!(
                assess_ghcr_image(
                    ImageFreshness::InSync,
                    "CP image",
                    "abc123",
                    "Containerfile",
                    strict
                ),
                GhcrAssessResult::Fresh,
            );
        }
    }

    /// Non-strict drift must WARN and proceed — there is no rebuild path on
    /// the ghcr source, so aborting would be worse than telling the operator.
    #[test]
    fn ghcr_drift_warns_when_not_strict() {
        let r = assess_ghcr_image(
            ImageFreshness::Drifted,
            "CP image",
            "abc123",
            "containers/k8s/Containerfile",
            false,
        );
        match r {
            GhcrAssessResult::Warn(msg) => {
                // Operators grep this wording; it is preserved from the twin.
                assert!(msg.contains("does NOT reflect on-disk"), "{msg}");
                assert!(msg.contains("abc123"), "must name the vcs-ref: {msg}");
                assert!(msg.contains("IMAGE_SOURCE=local FORCE_REBUILD=1"), "{msg}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn ghcr_drift_hard_fails_under_strict() {
        let r = assess_ghcr_image(
            ImageFreshness::Drifted,
            "CP image",
            "abc123",
            "Containerfile",
            true,
        );
        match r {
            GhcrAssessResult::StrictFail(msg) => {
                assert!(msg.contains("predates on-disk"), "{msg}");
                assert!(msg.contains("STRICT_CACHE=1"), "{msg}");
            }
            other => panic!("expected StrictFail, got {other:?}"),
        }
    }

    /// The checkout-free case. `hbird` is expected to run from an installed
    /// binary with no repo, so "cannot prove" is NORMAL and must not fail a
    /// default deploy — but STRICT_CACHE=1 explicitly asks for proof, and
    /// unprovable is not proof.
    #[test]
    fn ghcr_unverifiable_is_fresh_by_default_and_fatal_under_strict() {
        assert_eq!(
            assess_ghcr_image(
                ImageFreshness::Unverifiable,
                "CP image",
                "",
                "Containerfile",
                false
            ),
            GhcrAssessResult::Fresh,
        );
        match assess_ghcr_image(
            ImageFreshness::Unverifiable,
            "CP image",
            "",
            "Containerfile",
            true,
        ) {
            GhcrAssessResult::StrictFail(msg) => {
                assert!(msg.contains("cannot prove freshness"), "{msg}");
                assert!(
                    msg.contains("--repo-root"),
                    "must tell them how to enable it: {msg}"
                );
            }
            other => panic!("expected StrictFail, got {other:?}"),
        }
    }

    /// An empty vcs-ref can never be compared — guard the git path from
    /// being invoked with a meaningless revision.
    #[test]
    fn containerfile_changed_since_empty_ref_is_unverifiable() {
        assert_eq!(
            containerfile_changed_since(Path::new("/nonexistent"), "", &["Containerfile"]),
            ImageFreshness::Unverifiable,
        );
    }

    /// No repo at the given root => unverifiable, never a panic or a false
    /// "in sync". This is the path taken on a host with no checkout.
    #[test]
    fn containerfile_changed_since_without_a_repo_is_unverifiable() {
        let tmp = std::env::temp_dir().join(format!("hbird-cache-norepo-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        assert_eq!(
            containerfile_changed_since(&tmp, "deadbeef", &["Containerfile"]),
            ImageFreshness::Unverifiable,
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
