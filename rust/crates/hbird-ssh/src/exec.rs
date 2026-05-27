//! Mockable SSH-execution seam (issue #345).
//!
//! [`SshExec`] is the trait consumer crates take when they want a
//! pluggable SSH backend so unit tests can drive helpers that branch on
//! rc / stdout / stderr without touching the network. Production code
//! uses the inherent methods on [`crate::Client`] (which implements the
//! trait); tests use a stand-in implementor that returns canned
//! [`crate::RunOutput`] / [`crate::Error`] values keyed by command.
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
//! Today all in-tree consumers take `&impl SshExec` (monomorphized).
//! The trait is `Send + Sync` so it is shape-compatible with dynamic
//! dispatch (`&dyn SshExec`, `Box<dyn SshExec>`), but the `dyn` path
//! has no real consumer yet — file an issue if you need it so blanket
//! impls + the object-safety guarantee can be pinned by an actual
//! user.
//!
//! # Relationship to `hbird_virt::SshClient`
//!
//! The sibling crate `hbird-virt` has a separate `SshClient` trait
//! (`crates/hbird-virt/src/ssh.rs`) used by its libvirt wrappers.
//! Same conceptual role, different shape; the duplication is
//! intentional (kept independent so each crate's consumers evolve
//! without cross-crate coupling). Unifying onto a single workspace
//! trait is out of scope for #345 — file a follow-up if a real
//! cross-crate consumer arrives.

use crate::{Client, Result, RunOutput};

/// SSH-execution seam.
///
/// Implementations execute a remote command and return the captured
/// stdout/stderr/exit-status. The production implementor is
/// [`crate::Client`] (which shells out to `ssh(1)`); tests use canned
/// implementors that return pre-built [`RunOutput`] / [`crate::Error`]
/// values keyed by command.
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
}
