//! SSH transport for the hummingbird-k8s Rust rewrite.
//!
//! The bash client-side tooling under [`../scripts/`] reaches the KVM
//! host (and the VMs behind it, via ProxyJump) by calling the system
//! `ssh` binary with a canonical option set defined by
//! `ssh_opts_array{,_no_identity}` in `lib/build-common.sh`. This crate
//! reproduces that helper as a typed Rust API so consumer crates (the
//! `update-cluster`, `verify-*`, `export-argocd` re-implementations
//! tracked by [#286–#288]) can drive the same option set without
//! shelling back to bash.
//!
//! # Why shell out to the system `ssh` binary
//!
//! The existing project already hard-depends on the system `ssh` binary
//! — every bash script invokes it. Three design alternatives were
//! considered and rejected:
//!
//! - **Implement SSH from scratch (russh or similar).** Overkill for
//!   our usage: we don't need port forwarding, agent forwarding, or
//!   most of the protocol surface; we DO need exact behavioral parity
//!   with bash, which means matching OpenSSH's `~/.ssh/config`
//!   resolution, `ProxyJump`, multiplex sockets, and so on. The
//!   simplest way to match OpenSSH's behavior is to use OpenSSH.
//! - **Use the `openssh` crate.** It also shells out to `ssh`, but
//!   pulls in tokio as a hard dependency. Async ergonomics aren't
//!   needed at this layer (consumer crates can spawn their own threads
//!   if they want parallelism — the bash twin is synchronous).
//! - **Shell back through `bash` to the existing helpers.** Defeats the
//!   point of the rewrite (operator-mental-model contract from the
//!   epic — the Rust path should not depend on bash state).
//!
//! So this crate is a thin opinionated wrapper over
//! `std::process::Command::new("ssh")` with the option set pinned
//! against `lib/build-common.sh` by an integration test
//! (`tests/options_match_bash_twin.rs`).
//!
//! # Public API
//!
//! - [`SshOptions`] — connection-level config (host, user, port,
//!   identity, ProxyJump, timeout, ControlMaster, batch mode).
//! - [`OpenSshClient`] — owns an [`SshOptions`] and runs commands via
//!   [`OpenSshClient::run`] / [`OpenSshClient::run_with_stdin`].
//! - [`RunOutput`] — captured stdout + stderr + [`std::process::ExitStatus`].
//! - [`Error`] — typed failure modes.
//!
//! # Not in scope (deferred to consumer-crate issues)
//!
//! - SFTP / scp file transfer — add when [#286] needs it (deploy-cluster
//!   uses `scp $CONFIG` and `scp $SSH_PUBKEY_FILE`).
//! - Port forwarding — no current consumer.
//! - Async / streaming output — consumer crates can wrap [`OpenSshClient`]
//!   if they need it.
//!
//! [#286]: https://github.com/aatchison/hummingbird-k8s/issues/286
//! [#287]: https://github.com/aatchison/hummingbird-k8s/issues/287
//! [#288]: https://github.com/aatchison/hummingbird-k8s/issues/288

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::{Command, ExitStatus, Stdio};

mod error;
mod options;

pub use error::Error;
pub use options::SshOptions;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Captured output from a remote command invocation.
///
/// The bash twin propagates exit codes via `exit $?` and writes stdout +
/// stderr inline; this struct preserves all three so Rust callers can
/// inspect each independently. Both byte streams are also decoded
/// lossy-UTF8 into [`Self::stdout_lossy`] / [`Self::stderr_lossy`] for
/// log/diagnostic use — callers that care about exact bytes (e.g.
/// piping through to another process) should use [`Self::stdout`] /
/// [`Self::stderr`].
#[derive(Debug, Clone)]
pub struct RunOutput {
    /// Exit status of the `ssh` process. Non-zero means either an
    /// SSH-layer failure (auth, network, ProxyJump) OR a non-zero exit
    /// from the remote command itself — OpenSSH propagates the remote
    /// exit code directly. Distinguishing the two at this layer would
    /// require parsing OpenSSH's stderr, which is fragile.
    pub status: ExitStatus,
    /// Raw stdout bytes captured from the remote command.
    pub stdout: Vec<u8>,
    /// Raw stderr bytes captured from `ssh` itself + the remote command.
    pub stderr: Vec<u8>,
}

impl RunOutput {
    /// Lossy-UTF8 view of [`Self::stdout`]. Use this for log lines + error
    /// messages; use the raw bytes when piping to another process.
    #[must_use]
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Lossy-UTF8 view of [`Self::stderr`]. See [`Self::stdout_lossy`].
    #[must_use]
    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// SSH client. Owns an [`SshOptions`] and runs remote commands.
///
/// One client = one logical connection target. Construction is cheap
/// (no I/O); the actual SSH handshake happens per-call inside [`Self::run`].
/// Use [`SshOptions::with_controlmaster`] to multiplex multiple `run` calls
/// over a single connection (matches `update-cluster.sh`'s pattern).
///
/// # Example
///
/// ```no_run
/// use hbird_openssh::{OpenSshClient, SshOptions};
///
/// let opts = SshOptions::new("kvm-host")
///     .with_identity_file("/home/op/.ssh/id_ed25519")
///     .with_proxy_jump("bastion");
/// let client = OpenSshClient::new(opts);
/// let out = client.run("hostname")?;
/// println!("{}", out.stdout_lossy().trim());
/// # Ok::<(), hbird_openssh::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct OpenSshClient {
    options: SshOptions,
}

impl OpenSshClient {
    /// Build a client from a fully-constructed [`SshOptions`].
    #[must_use]
    pub fn new(options: SshOptions) -> Self {
        Self { options }
    }

    /// Borrow the underlying options. Useful for tests that want to
    /// re-inspect the argv that will be passed to `ssh`.
    #[must_use]
    pub fn options(&self) -> &SshOptions {
        &self.options
    }

    /// Run a single remote command and capture stdout + stderr +
    /// exit status.
    ///
    /// Mirrors the bash twin's idiom:
    ///
    /// ```text
    /// ssh "${SSH_OPTS[@]}" "$KVM_HOST" "$command"
    /// ```
    ///
    /// `command` is passed as a single argv element to `ssh`, which
    /// then concatenates it with spaces and runs it through the
    /// remote user's login shell (`$SHELL -c`). The caller is
    /// responsible for any quoting needed inside `command` — same as
    /// the bash twin. (For the rare argv-style invocation use the
    /// remote shell's quoting; we deliberately don't add a layer that
    /// hides this.)
    ///
    /// # Errors
    ///
    /// - [`Error::IdentityFileMissing`] when an identity file was
    ///   configured but doesn't exist.
    /// - [`Error::Spawn`] when `ssh` can't be invoked.
    /// - [`Error::Wait`] when waiting on the child process fails.
    /// - [`Error::NonZeroExit`] when `ssh` exits non-zero. The
    ///   captured stdout + stderr are included so the caller can
    ///   distinguish SSH-layer failures from remote-command failures.
    pub fn run(&self, command: &str) -> Result<RunOutput> {
        self.run_inner(command, None)
    }

    /// Run a remote command, feeding `stdin` into its standard input.
    ///
    /// Mirrors:
    ///
    /// ```text
    /// ssh "${SSH_OPTS[@]}" "$KVM_HOST" "$command" < <(printf '%s' "$stdin")
    /// ```
    ///
    /// Useful for the rare commands that read stdin (e.g. piping the
    /// content of a local file into a remote `cat > /path` —
    /// deploy-cluster does this for the cloud-init seed). Most
    /// commands should use [`Self::run`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::run`].
    pub fn run_with_stdin(&self, command: &str, stdin: &[u8]) -> Result<RunOutput> {
        self.run_inner(command, Some(stdin))
    }

    fn run_inner(&self, command: &str, stdin: Option<&[u8]>) -> Result<RunOutput> {
        if let Some(path) = self.options.identity_file()
            && !path.exists()
        {
            return Err(Error::IdentityFileMissing {
                path: path.to_path_buf(),
            });
        }

        let argv = self.options.to_argv();
        // argv[0] is "ssh" — Command::new takes the program separately.
        let (program, rest) = argv.split_first().expect("argv always has ssh at index 0");
        let mut cmd = Command::new(program);
        cmd.args(rest);
        cmd.arg(command);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }

        let mut child = cmd.spawn().map_err(|source| Error::Spawn {
            program: program.clone(),
            source,
        })?;

        // Stream stdin first, drop the handle to signal EOF, then wait
        // for output. For tiny inputs (cloud-init seeds, kubectl
        // manifests) this is fine; large inputs would want a
        // background writer thread, but no current consumer needs that.
        if let Some(bytes) = stdin
            && let Some(mut child_stdin) = child.stdin.take()
        {
            child_stdin
                .write_all(bytes)
                .map_err(|source| Error::Wait { source })?;
            // Dropping child_stdin closes the pipe; the remote sees EOF.
        }

        let output = child
            .wait_with_output()
            .map_err(|source| Error::Wait { source })?;

        if !output.status.success() {
            return Err(Error::NonZeroExit {
                status: output.status,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(RunOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn client_holds_supplied_options() {
        let opts = SshOptions::new("h").with_user("core");
        let client = OpenSshClient::new(opts.clone());
        assert_eq!(client.options(), &opts);
    }

    #[test]
    fn identity_file_missing_pre_check() {
        // Synthesize a path that almost certainly doesn't exist so the
        // pre-check fires before we ever spawn `ssh`.
        let bogus = PathBuf::from("/nonexistent/hbird-openssh/identity-pre-check");
        assert!(!bogus.exists(), "test precondition: path must not exist");

        let opts = SshOptions::new("h").with_identity_file(bogus.clone());
        let client = OpenSshClient::new(opts);
        let err = client.run("true").expect_err("missing identity must error");
        match err {
            Error::IdentityFileMissing { path } => assert_eq!(path, bogus),
            other => panic!("expected IdentityFileMissing, got {other:?}"),
        }
    }

    /// Integration-ish test: invoke `ssh` against a guaranteed-unreachable
    /// host and assert we surface a [`NonZeroExit`] (not a `Spawn` /
    /// `Wait`). This exercises the real subprocess path without needing
    /// a working remote.
    ///
    /// Gated behind `#[ignore]` because the local environment may have
    /// `ssh` proxied / aliased in unusual ways; CI runs the option-set
    /// pin test (`tests/options_match_bash_twin.rs`) which doesn't
    /// require a working `ssh`. Run manually via
    /// `cargo nextest run -- --ignored` when validating the spawn path.
    #[test]
    #[ignore = "requires `ssh` on PATH; run with --ignored when validating spawn path"]
    fn unreachable_host_returns_non_zero_exit() {
        // RFC 5737 TEST-NET-1: guaranteed-unroutable address.
        let opts =
            SshOptions::new("192.0.2.1").with_connect_timeout(std::time::Duration::from_secs(2));
        let client = OpenSshClient::new(opts);
        let err = client
            .run("true")
            .expect_err("unreachable host must produce NonZeroExit");
        assert!(matches!(err, Error::NonZeroExit { .. }));
    }
}
