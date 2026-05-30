//! Pure cache-assessment logic for qcow2 template freshness checks.
//!
//! Mirrors `lib/cache-utils.sh::hbird_assess_qcow2_cache` from the bash
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
/// prefix (split on `:`), and different value. Any uncertainty → [`Reuse`].
///
/// # Arguments
///
/// - `cached_ref` — the build ID stored in the `.build-ref` sidecar file.
/// - `expected_ref` — the build ID computed from the current image/source.
/// - `strict` — if `true`, confirmed stale returns [`StrictFail`] instead of
///   [`Rebuild`].
///
/// [`Reuse`]: CacheAssessResult::Reuse
pub fn assess_qcow2_cache(
    cached_ref: Option<&str>,
    expected_ref: Option<&str>,
    strict: bool,
) -> CacheAssessResult {
    let (Some(cached), Some(expected)) = (cached_ref, expected_ref) else {
        // One or both IDs missing — cannot confirm stale.
        return CacheAssessResult::Reuse;
    };
    if cached.is_empty() || expected.is_empty() {
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
    fn assess_qcow2_cache_reuses_when_either_empty() {
        // cached empty
        assert_eq!(
            assess_qcow2_cache(Some(""), Some("ghcr:abc123"), true),
            CacheAssessResult::Reuse
        );
        // expected empty
        assert_eq!(
            assess_qcow2_cache(Some("ghcr:abc123"), Some(""), true),
            CacheAssessResult::Reuse
        );
        // both None
        assert_eq!(
            assess_qcow2_cache(None, None, true),
            CacheAssessResult::Reuse
        );
        // one None
        assert_eq!(
            assess_qcow2_cache(None, Some("ghcr:abc123"), true),
            CacheAssessResult::Reuse
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
}
