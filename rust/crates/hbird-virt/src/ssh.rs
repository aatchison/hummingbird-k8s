//! SSH transport trait.
//!
//! [`SshClient`] is the seam between [`crate::VirtConnection`] and a real
//! SSH implementation. The crate intentionally does NOT depend on a
//! concrete SSH library — that responsibility belongs to the sibling
//! `hbird-openssh` crate ([#285]). Wiring is mechanical: implement
//! [`SshClient::run`] on top of whatever SSH backend the caller wants
//! (the real one is OpenSSH-via-subprocess for parity with the bash
//! twin's `ssh "$KVM_HOST" "..."` pattern), and pass it to
//! [`crate::VirtConnection::new`].
//!
//! Tests in this crate use a stub implementation (see
//! `tests/virsh_commands.rs`) that returns canned responses keyed by
//! command — no real SSH, no real libvirt, fully hermetic.
//!
//! [#285]: https://github.com/aatchison/hummingbird-k8s/issues/285

use std::fmt;

/// Trait-object-safe SSH client.
///
/// Implementations run a single command on a remote host and return the
/// command's stdout (UTF-8) on success or an [`SshError`] carrying
/// stderr + exit context on failure. Implementations are responsible
/// for connection setup/teardown — [`crate::VirtConnection`] does not
/// keep a persistent session.
///
/// The trait is intentionally minimal so a future `hbird-openssh`
/// implementation can be swapped in without touching consumers. It is
/// `Send + Sync` so callers can store it behind `Arc<dyn SshClient>`
/// when the same connection is shared across tasks.
pub trait SshClient: Send + Sync {
    /// Run `command` on `host` and return stdout on success.
    ///
    /// `host` is the SSH-target string (typically a user@host alias
    /// from `~/.ssh/config`, matching the bash twin's `KVM_HOST`
    /// convention). `command` is the remote shell command line —
    /// implementations are responsible for any quoting/escaping needed
    /// to pass it through `sh -c`.
    ///
    /// # Errors
    ///
    /// Returns [`SshError`] for any transport-level failure (connection
    /// refused, auth failure, network drop) OR a non-zero remote exit
    /// status. Consumers should treat both as "remote command did not
    /// produce usable stdout".
    fn run(&self, host: &str, command: &str) -> Result<String, SshError>;
}

/// SSH transport error.
///
/// Two-state by design: either we never reached the remote (`Transport`)
/// or the remote command exited non-zero (`RemoteExit`). Implementations
/// must distinguish the two so callers can decide whether a retry is
/// worth attempting.
#[derive(Debug)]
#[non_exhaustive]
pub enum SshError {
    /// Could not establish or maintain the SSH connection (DNS failure,
    /// connection refused, auth failure, network drop mid-stream).
    /// The `message` is operator-facing diagnostic text — typically the
    /// SSH client's stderr verbatim.
    Transport {
        /// SSH target (host or user@host) the call attempted.
        host: String,
        /// Free-form diagnostic from the underlying SSH layer.
        message: String,
    },

    /// The SSH connection succeeded and the remote command ran, but it
    /// exited non-zero. `stderr` is the captured stderr from the remote
    /// process; `exit_code` is `None` when the process was killed by a
    /// signal rather than exiting cleanly.
    RemoteExit {
        /// SSH target the call ran against.
        host: String,
        /// The remote command line (post-quoting).
        command: String,
        /// Remote process exit code, or `None` if signalled.
        exit_code: Option<i32>,
        /// Captured remote stderr.
        stderr: String,
    },
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport { host, message } => {
                write!(f, "SSH transport failure for {host}: {message}")
            }
            Self::RemoteExit {
                host,
                command,
                exit_code,
                stderr,
            } => {
                let code = exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signalled".to_string());
                write!(
                    f,
                    "remote command on {host} exited {code}: {command:?}\n--- stderr ---\n{stderr}"
                )
            }
        }
    }
}

impl std::error::Error for SshError {}
