//! Error types for the SSH transport.
//!
//! Mirrors the diagnostic shape that operators already see from the bash
//! twin (`ssh:` prefix from OpenSSH itself, plus the wrapping script's
//! `[script-name] ERROR: ...` line). The Rust caller sees a structured
//! enum; turning that into an operator-facing string is left to the
//! consumer crate (e.g. the `verify-*` re-implementations under
//! [#287](https://github.com/aatchison/hummingbird-k8s/issues/287)).
//!
//! # Round-2 review additions (#317)
//!
//! - Every variant that names a remote operation carries `host: String`
//!   so an operator running parallel SSH ops across N hosts can map an
//!   error back to a connection (L3 + L8 cross-lens HIGH finding).
//! - `Spawn` carries a `kind` discriminator so "ssh binary not on PATH"
//!   is distinguishable from any other I/O error (L3 MEDIUM finding).
//! - `StdinWrite` is a dedicated variant — previously a stdin-pipe write
//!   failure was mislabeled `Error::Wait`, mismatching the actual code
//!   path (L2 + L3 + L9 cross-lens HIGH finding).

use std::path::PathBuf;
use std::process::ExitStatus;

/// Discriminator for [`Error::Spawn`] failures.
///
/// `SshBinaryMissing` matches [`std::io::ErrorKind::NotFound`] specifically;
/// the operator's remediation is "install openssh-client" rather than
/// "check resource limits / signal handling". `Other` captures everything
/// else without imposing further structure (the wrapped I/O error already
/// carries the OS-level cause).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpawnKind {
    /// `ssh` binary is not on `PATH` (or otherwise can't be located).
    SshBinaryMissing,
    /// Any other spawn failure — EMFILE, EAGAIN, signal-handling race, etc.
    Other,
}

/// Errors returned by [`crate::Client`] operations.
///
/// Variants stay flat (no nested enums) so consumers can `match` on the
/// shape directly without traversing intermediate layers.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The `ssh` binary failed to spawn — typically because `ssh` is not
    /// on `PATH`, or the child process couldn't be created. The `kind`
    /// discriminator lets the caller pattern-match the
    /// `SshBinaryMissing` case (the operator-actionable one) without
    /// downcasting [`std::io::Error::kind`].
    #[error("failed to spawn {program} for host {host}: {source}")]
    Spawn {
        /// Program name passed to [`std::process::Command::new`] (always
        /// `"ssh"` today; surfaced anyway so future overrides surface
        /// here cleanly).
        program: String,
        /// Target host from the [`crate::SshOptions`] in use, so a multi-host
        /// parallel run can map this error to a connection.
        host: String,
        /// Classification of the failure for pattern-matching.
        kind: SpawnKind,
        /// Underlying I/O error from the spawn attempt.
        #[source]
        source: std::io::Error,
    },

    /// Waiting on the spawned `ssh` process failed (rare — typically a
    /// signal-handling race or an OS-level error). Round-2 split this
    /// off from stdin-write failures (which were previously mislabeled
    /// `Wait` — see [`Error::StdinWrite`]).
    #[error("waiting on ssh for host {host} failed: {source}")]
    Wait {
        /// Target host from the [`crate::SshOptions`] in use.
        host: String,
        /// Underlying I/O error from [`std::process::Child::wait_with_output`].
        #[source]
        source: std::io::Error,
    },

    /// Writing the caller-supplied bytes to the spawned `ssh` process's
    /// stdin failed. Distinct from [`Error::Wait`]: this error path runs
    /// BEFORE the wait, and the spawned child is still reaped (no zombie
    /// process). Round-2 introduced to fix the mislabel in the pre-round-2
    /// code that mapped both stdin failures and wait failures to `Wait`.
    /// (PR #317 round-2 review L2 + L3 + L9 cross-lens HIGH finding.)
    #[error("writing stdin to ssh for host {host} failed: {source}")]
    StdinWrite {
        /// Target host from the [`crate::SshOptions`] in use.
        host: String,
        /// Underlying I/O error from the pipe write.
        #[source]
        source: std::io::Error,
    },

    /// The remote command ran to completion but exited non-zero. The
    /// bash twin propagates the exit code via `exit $?`; Rust callers
    /// get the captured stdout + stderr alongside the [`ExitStatus`].
    /// SSH-layer failures (network, auth, ProxyJump) and remote-command
    /// failures both land here — distinguishing them at this layer
    /// would require parsing OpenSSH's stderr, which is fragile. The
    /// consumer can inspect [`Self::NonZeroExit::stderr`] for the usual
    /// `ssh: Could not resolve hostname ...` shape.
    #[error("ssh to {host} command failed with status {status}; stderr: {stderr:?}")]
    NonZeroExit {
        /// Target host from the [`crate::SshOptions`] in use.
        host: String,
        /// Exit status from the `ssh` process.
        status: ExitStatus,
        /// Captured stdout (lossy-UTF8 decoded).
        stdout: String,
        /// Captured stderr (lossy-UTF8 decoded).
        stderr: String,
    },

    /// The caller passed an identity file path that doesn't exist. We
    /// pre-check before invoking `ssh` so the operator gets a clear
    /// diagnostic instead of OpenSSH's terser `no such identity` line.
    #[error("identity file does not exist: {path}")]
    IdentityFileMissing {
        /// Path the caller passed as `identity_file`.
        path: PathBuf,
    },
}
