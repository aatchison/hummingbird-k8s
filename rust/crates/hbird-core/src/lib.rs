//! Placeholder crate for the hummingbird-k8s Rust rewrite (epic #279,
//! foundation #280). Exists to prove the devcontainer + cargo workspace
//! toolchain produces a buildable + testable artifact end-to-end. Real
//! shared types land in future PRs (#282 ClusterConfig, #285 openssh
//! transport, etc.).

/// Marketing name of the project. Pinned here so future crates can depend
/// on `hbird_core::PROJECT` instead of duplicating the literal.
pub const PROJECT: &str = "hummingbird-k8s";

/// Epic this workspace tracks. Convenience for log lines + diagnostics
/// that want to point at the architectural rationale.
pub const EPIC: &str = "https://github.com/aatchison/hummingbird-k8s/issues/279";

#[cfg(test)]
mod tests {
    use super::{EPIC, PROJECT};

    #[test]
    fn project_constant_is_set() {
        assert_eq!(PROJECT, "hummingbird-k8s");
    }

    #[test]
    fn epic_link_points_at_issue_279() {
        assert!(
            EPIC.ends_with("/issues/279"),
            "epic link should point at #279, got {EPIC}"
        );
    }
}
