//! Error types for the SSH transport.
//!
//! Mirrors the diagnostic shape that operators already see from the bash
//! twin (`ssh:` prefix from OpenSSH itself, plus the wrapping script's
//! `[script-name] ERROR: ...` line). The Rust caller sees a structured
//! enum; turning that into an operator-facing string is left to the
//! consumer crate (e.g. the `verify-*` re-implementations under
//! [#287](https://github.com/aatchison/hummingbird-k8s/issues/287)).

use std::path::PathBuf;
use std::process::ExitStatus;

/// Errors returned by [`crate::OpenSshClient`] operations.
///
/// Variants stay flat (no nested enums) so consumers can `match` on the
/// shape directly without traversing intermediate layers.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The `ssh` binary failed to spawn — typically because `ssh` is not
    /// on `PATH`, or the child process couldn't be created. Wraps the
    /// underlying [`std::io::Error`] and the path the caller asked us to
    /// invoke (defaults to `"ssh"`).
    #[error("failed to spawn {program}: {source}")]
    Spawn {
        /// Program name passed to [`std::process::Command::new`] (always
        /// `"ssh"` today; surfaced anyway so future overrides surface
        /// here cleanly).
        program: String,
        /// Underlying I/O error from the spawn attempt.
        #[source]
        source: std::io::Error,
    },

    /// Waiting on the spawned `ssh` process failed (rare — typically a
    /// signal-handling race or an OS-level error). Carries the [`ExitStatus`]
    /// only when waiting succeeded; otherwise [`None`].
    #[error("waiting on ssh failed: {source}")]
    Wait {
        /// Underlying I/O error from [`std::process::Child::wait_with_output`].
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
    #[error("ssh command failed with status {status}; stderr: {stderr:?}")]
    NonZeroExit {
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
