//! Mockable SSH-execution seam.
//!
//! [`SshExec`] is the trait consumer crates take when they want a
//! pluggable SSH backend so unit tests can drive helpers that branch on
//! rc / stdout / stderr without touching the network. Production code
//! uses the inherent methods on [`crate::Client`] (which implements the
//! trait); tests use a stand-in implementor that returns canned
//! [`crate::RunOutput`] / [`crate::Error`] values keyed by command.
//!
//! # Why a trait?
//!
//! The bash twin's `bootc_upgrade_apply` rc-classification (rc=255 →
//! Applied, rc=0+matching-digest → AlreadyCurrent, rc=0+differing-digest
//! → Applied, rc=other → UpgradeFailed; see
//! `scripts/update-cluster.sh:1114`) is subtle enough that a regression
//! in any branch is operator-visible only mid-cluster-upgrade. PR #344's
//! 7-lens review (L5 MEDIUM) flagged that the Rust port of this branch
//! set ran ONLY through `live-validate` — a real cluster cycle had to
//! fire before any branch was exercised. Issue #345 tracks closing that
//! gap with a mock-friendly trait + unit tests that pin each branch.
//!
//! # Why not promote `hbird_virt::SshClient`?
//!
//! The sibling crate `hbird-virt` already has an `SshClient` trait
//! (`crates/hbird-virt/src/ssh.rs`) used by its libvirt wrappers. The
//! long-term home for *one* shared SSH trait is an open question — at
//! the time #318 landed, that trait deliberately stayed local to avoid
//! blocking #285 (this crate) and #284 (hbird-virt) on each other.
//! Issue #345 picks the smaller, lower-risk move: add [`SshExec`] here
//! so cycle 2's bootc helpers gain unit-test coverage now, and leave
//! the hbird-virt cross-promotion as a follow-up that can be designed
//! once both crates' consumers are settled.
//!
//! # Trait shape
//!
//! Two methods that mirror the two inherent [`crate::Client`] entry
//! points: [`SshExec::run`] (no stdin) and [`SshExec::run_with_stdin`]
//! (pipe input). Same input types, same return types — a consumer can
//! swap `&impl SshExec` for `&Client` without further changes. Default
//! method bodies are NOT supplied; the production
//! [`impl SshExec for Client`] delegates to the inherent methods so
//! their behavior remains the single source of truth.
//!
//! Trait-object friendly: the trait is `Send + Sync` so callers can
//! store `Arc<dyn SshExec>` when they want runtime polymorphism (e.g.
//! threading the same mock across worker threads in a test).

use crate::{Client, Result, RunOutput};

/// SSH-execution seam.
///
/// Implementations execute a remote command and return the captured
/// stdout/stderr/exit-status. The production implementor is
/// [`crate::Client`] (which shells out to `ssh(1)`); tests use canned
/// implementors that return pre-built [`RunOutput`] / [`crate::Error`]
/// values keyed by command.
///
/// See the module-level docs in `crates/hbird-ssh/src/exec.rs` for the
/// rationale behind a trait (issue #345 background).
///
/// # Errors
///
/// Implementations return [`crate::Error`] for any failure mode —
/// transport-level (spawn / wait / identity-missing / stdin-write) or
/// remote-command-level ([`crate::Error::NonZeroExit`]). Consumers
/// distinguish the two by matching on the variant.
pub trait SshExec: Send + Sync {
    /// Run a single remote command. Same contract as [`Client::run`].
    ///
    /// # Errors
    ///
    /// Same as [`Client::run`].
    fn run(&self, command: &str) -> Result<RunOutput>;

    /// Run a remote command with bytes piped to its stdin. Same
    /// contract as [`Client::run_with_stdin`].
    ///
    /// # Errors
    ///
    /// Same as [`Client::run_with_stdin`].
    fn run_with_stdin(&self, command: &str, stdin: &[u8]) -> Result<RunOutput>;
}

impl SshExec for Client {
    fn run(&self, command: &str) -> Result<RunOutput> {
        Client::run(self, command)
    }

    fn run_with_stdin(&self, command: &str, stdin: &[u8]) -> Result<RunOutput> {
        Client::run_with_stdin(self, command, stdin)
    }
}

// Blanket impl so `&T where T: SshExec` is itself `SshExec`. Lets
// callers take `&impl SshExec` against either an owned client or a
// borrowed one without an extra `&&` layer.
impl<T: SshExec + ?Sized> SshExec for &T {
    fn run(&self, command: &str) -> Result<RunOutput> {
        (**self).run(command)
    }

    fn run_with_stdin(&self, command: &str, stdin: &[u8]) -> Result<RunOutput> {
        (**self).run_with_stdin(command, stdin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, SshOptions};

    /// The trait is implemented for [`Client`] so consumer crates can
    /// take `&impl SshExec` and pass a real [`Client`] in production.
    #[test]
    fn client_implements_ssh_exec_trait() {
        fn takes_trait<T: SshExec>(_: &T) {}
        let client = Client::new(SshOptions::new("h"));
        takes_trait(&client);
    }

    /// Trait-object construction: callers can erase the concrete type
    /// behind `&dyn SshExec` (or `Box<dyn SshExec>`) when they want
    /// runtime polymorphism. Compiles-or-fails — no assert needed.
    #[test]
    fn client_is_object_safe() {
        let client = Client::new(SshOptions::new("h"));
        let _erased: &dyn SshExec = &client;
    }
}
