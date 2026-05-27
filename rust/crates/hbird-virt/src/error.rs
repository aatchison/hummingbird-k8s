//! Error types for [`crate::VirtConnection`] and [`crate::QemuSshUri`].
//!
//! Errors stay flat (no nested enums) so consumer crates can `match` on
//! the shape directly. Variants carry just enough context for the bash
//! twin's failure mode to be reproducible: which domain, which URI,
//! which raw `virsh` stderr.

use std::num::ParseIntError;

use crate::ssh::SshError;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by [`crate::VirtConnection`] and [`crate::QemuSshUri`].
///
/// Variants stay flat so consumers can `match` directly. SSH transport
/// errors wrap [`SshError`] from the [`crate::ssh`] module so the
/// distinction between "SSH failed" and "virsh ran but emitted an error"
/// stays visible in the type system.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The `qemu+ssh://` URI did not match the libvirt URI grammar
    /// documented at <https://libvirt.org/uri.html>. The raw URI is
    /// preserved so the operator can see exactly what the parser
    /// rejected.
    #[error("invalid qemu+ssh URI: {raw:?} — {reason}")]
    InvalidUri {
        /// The raw URI string the parser was asked to handle.
        raw: String,
        /// Specific grammar violation (e.g. "missing scheme",
        /// "unsupported transport", "empty host").
        reason: &'static str,
    },

    /// A URI's port component was syntactically present but did not
    /// parse as a `u16`.
    #[error("invalid port in qemu+ssh URI {raw:?}: {source}")]
    InvalidPort {
        /// The raw URI string.
        raw: String,
        /// Underlying integer parse error.
        #[source]
        source: ParseIntError,
    },

    /// SSH transport-level failure (could not connect, connection
    /// dropped mid-command, etc.). Distinct from [`Error::VirshFailed`]
    /// — this means we never got far enough to see `virsh`'s exit
    /// status.
    #[error("SSH transport error: {source}")]
    Ssh {
        /// Underlying transport error from the [`crate::ssh::SshClient`]
        /// implementation.
        #[source]
        source: SshError,
    },

    /// `virsh` ran on the remote and exited non-zero. The captured
    /// stderr is preserved so the operator sees the same diagnostic the
    /// bash twin would print (the bash twin shells out to `virsh` and
    /// surfaces its stderr verbatim).
    #[error("virsh failed for command {command:?}: {stderr}")]
    VirshFailed {
        /// The `virsh` command that was run (verb + args, without the
        /// `virsh -c URI` prefix).
        command: String,
        /// Captured stderr from `virsh`.
        stderr: String,
    },

    /// `virsh` ran successfully but emitted output the parser couldn't
    /// reconcile with the expected shape — e.g. `domifaddr` returned
    /// rows but none contained an `ipv4` address. Distinct from
    /// [`Error::VirshFailed`] which carries a non-zero exit; this means
    /// "virsh exit 0, but the output is unusable".
    #[error("could not parse virsh output for {command:?}: {reason}")]
    UnparseableOutput {
        /// The `virsh` command whose output didn't parse.
        command: String,
        /// What the parser was looking for and couldn't find.
        reason: &'static str,
    },
}

impl From<SshError> for Error {
    fn from(source: SshError) -> Self {
        Self::Ssh { source }
    }
}
