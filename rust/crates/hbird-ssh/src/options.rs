//! Connection options + argv builder for the SSH transport.
//!
//! The bash twin's `ssh_opts_array` (see `lib/build-common.sh`) emits an
//! array fragment like:
//!
//! ```text
//! -i $SSH_PRIVKEY_FILE                  # only with ssh_opts_array (NOT ssh_opts_array_no_identity)
//! -o StrictHostKeyChecking=no
//! -o UserKnownHostsFile=/dev/null
//! -o LogLevel=ERROR
//! -o ConnectTimeout=10
//! -o BatchMode=yes
//! [-o ControlMaster=auto -o ControlPath=... -o ControlPersist=60s]   # --with-controlmaster
//! [-o ProxyJump=<host>]                                              # --proxy-jump=...
//! ```
//!
//! This module produces the *same* argv vector as a typed Rust API.
//! `tests/options_match_bash_twin.rs` pins the exact ordering against
//! the bash helper so future drift surfaces in CI.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Connection-level configuration shared by every command invocation.
///
/// Construction is via the builder-style setters (`with_*`) starting
/// from [`SshOptions::new`]. The defaults match the bash twin's
/// `ssh_opts_array` output verbatim — operators editing one side of the
/// fence should see identical SSH options when the other side runs.
///
/// # Bash twin
///
/// Reproduces `_ssh_opts_array_impl` from `lib/build-common.sh`. The
/// `--with-controlmaster` flag is exposed here as
/// [`SshOptions::with_controlmaster`]; `--proxy-jump=HOST` as
/// [`SshOptions::with_proxy_jump`]; the `0|1 include_identity` argument
/// as [`SshOptions::without_identity`] (matching
/// `ssh_opts_array_no_identity`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOptions {
    pub(crate) host: String,
    pub(crate) user: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) identity_file: Option<PathBuf>,
    pub(crate) proxy_jump: Option<String>,
    pub(crate) connect_timeout: Duration,
    pub(crate) strict_host_key_checking: bool,
    pub(crate) batch_mode: bool,
    pub(crate) control_master: bool,
    pub(crate) include_identity_flag: bool,
}

impl SshOptions {
    /// Construct a fresh option set targeting `host` with bash-twin
    /// defaults applied:
    ///
    /// - `StrictHostKeyChecking=no`
    /// - `UserKnownHostsFile=/dev/null` (added by [`Self::to_argv`])
    /// - `LogLevel=ERROR` (added by [`Self::to_argv`])
    /// - `ConnectTimeout=10`
    /// - `BatchMode=yes`
    /// - No identity file (caller decides via [`Self::with_identity_file`])
    /// - No ProxyJump
    /// - No ControlMaster
    ///
    /// `host` may be a hostname, IP literal, or `~/.ssh/config` alias —
    /// we don't validate; OpenSSH does.
    #[must_use]
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            user: None,
            port: None,
            identity_file: None,
            proxy_jump: None,
            connect_timeout: Duration::from_secs(10),
            strict_host_key_checking: false,
            batch_mode: true,
            control_master: false,
            include_identity_flag: true,
        }
    }

    /// Override the remote user. Equivalent to `ssh user@host`; the bash
    /// twin doesn't set this explicitly — operators usually configure it
    /// in `~/.ssh/config` — but we expose it so Rust callers can be
    /// explicit when the config path is unavailable.
    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Override the remote port (`-p N`). Default unset → OpenSSH uses 22
    /// or whatever `~/.ssh/config` says.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Set the identity file (`-i <path>`). Mirrors `ssh_opts_array`'s
    /// `-i $SSH_PRIVKEY_FILE`. The file's existence is checked at
    /// [`crate::Client::run`] time, not here.
    #[must_use]
    pub fn with_identity_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity_file = Some(path.into());
        self
    }

    /// Suppress the `-i` flag even if an identity file is present.
    /// Mirrors the bash `ssh_opts_array_no_identity` helper used by
    /// `verify-hardening.sh` + `verify-encryption.sh`, where SSH agent
    /// auth or `~/.ssh/config` IdentityFile entries are preferred over
    /// hardcoded paths.
    #[must_use]
    pub fn without_identity(mut self) -> Self {
        self.include_identity_flag = false;
        self
    }

    /// Set the ProxyJump host (`-o ProxyJump=HOST`). Mirrors the
    /// `--proxy-jump=HOST` flag of `ssh_opts_array`. Used by every
    /// `verify-*` script when `KVM_HOST` is set and the operator is
    /// reaching VMs *through* the KVM host.
    #[must_use]
    pub fn with_proxy_jump(mut self, host: impl Into<String>) -> Self {
        self.proxy_jump = Some(host.into());
        self
    }

    /// Override `ConnectTimeout` (default: 10s, matching the bash twin).
    /// Caller-supplied [`Duration`] is rounded down to whole seconds for
    /// OpenSSH (sub-second values aren't meaningful at this layer).
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Flip `StrictHostKeyChecking` from the bash-twin default (`no`) to
    /// `yes`. Default-OFF matches the deploy-cluster runtime where each
    /// VM presents a fresh host key on first boot; operators who pin
    /// known_hosts manually can opt back in.
    #[must_use]
    pub fn with_strict_host_key_checking(mut self, strict: bool) -> Self {
        self.strict_host_key_checking = strict;
        self
    }

    /// Toggle `BatchMode` (default: `yes` per the bash twin — no
    /// interactive prompts). Flip OFF for the rare interactive path
    /// (manual `ssh -t` for debugging) — most Rust callers won't.
    #[must_use]
    pub fn with_batch_mode(mut self, batch: bool) -> Self {
        self.batch_mode = batch;
        self
    }

    /// Enable connection multiplexing (`ControlMaster=auto`,
    /// `ControlPath=/tmp/hbird-ssh-${UID}-%r@%h:%p`,
    /// `ControlPersist=60s`). Mirrors `ssh_opts_array --with-controlmaster`
    /// from `update-cluster.sh`. The `${UID}` is the *current process's*
    /// UID resolved at [`Self::to_argv`] time — two operators on the
    /// same host won't collide on the multiplex socket.
    #[must_use]
    pub fn with_controlmaster(mut self) -> Self {
        self.control_master = true;
        self
    }

    /// Build the argv vector — including `ssh` itself at index 0 — that
    /// reproduces the bash twin's option ordering exactly.
    ///
    /// The vector is built as:
    ///
    /// 1. `ssh`
    /// 2. `-i <identity>`                  (when identity_file + include_identity_flag)
    /// 3. `-o StrictHostKeyChecking=...`
    /// 4. `-o UserKnownHostsFile=/dev/null`
    /// 5. `-o LogLevel=ERROR`
    /// 6. `-o ConnectTimeout=<secs>`
    /// 7. `-o BatchMode=yes|no`
    /// 8. `-o ControlMaster=auto -o ControlPath=... -o ControlPersist=60s`   (when control_master)
    /// 9. `-o ProxyJump=<host>`            (when proxy_jump)
    /// 10. `-p <port>`                     (when port)
    /// 11. `<user@host>` or `<host>`
    ///
    /// The command-to-run is *not* included — [`crate::Client`]
    /// appends it via `Command::arg`.
    #[must_use]
    pub fn to_argv(&self) -> Vec<String> {
        let mut argv: Vec<String> = Vec::with_capacity(16);
        argv.push("ssh".to_string());

        if self.include_identity_flag {
            if let Some(path) = &self.identity_file {
                argv.push("-i".to_string());
                argv.push(path.display().to_string());
            }
        }

        argv.push("-o".to_string());
        argv.push(format!(
            "StrictHostKeyChecking={}",
            if self.strict_host_key_checking {
                "yes"
            } else {
                "no"
            }
        ));
        argv.push("-o".to_string());
        argv.push("UserKnownHostsFile=/dev/null".to_string());
        argv.push("-o".to_string());
        argv.push("LogLevel=ERROR".to_string());
        argv.push("-o".to_string());
        argv.push(format!("ConnectTimeout={}", self.connect_timeout.as_secs()));
        argv.push("-o".to_string());
        argv.push(format!(
            "BatchMode={}",
            if self.batch_mode { "yes" } else { "no" }
        ));

        if self.control_master {
            // UID resolved at argv-build time so two operators on the
            // same host don't collide on /tmp/hbird-ssh-*. The bash twin
            // uses the same template (lib/build-common.sh).
            //
            // We use libc::getuid via std::os::unix; on non-Unix targets
            // we fall back to "uid" literal (the crate is unix-only in
            // practice — every consumer talks to KVM/libvirt — but the
            // fallback keeps the crate compilable cross-platform for
            // dependency-tree health).
            let uid = current_uid_string();
            argv.push("-o".to_string());
            argv.push("ControlMaster=auto".to_string());
            argv.push("-o".to_string());
            argv.push(format!("ControlPath=/tmp/hbird-ssh-{uid}-%r@%h:%p"));
            argv.push("-o".to_string());
            argv.push("ControlPersist=60s".to_string());
        }

        if let Some(jump) = &self.proxy_jump {
            argv.push("-o".to_string());
            argv.push(format!("ProxyJump={jump}"));
        }

        if let Some(port) = self.port {
            argv.push("-p".to_string());
            argv.push(port.to_string());
        }

        let target = match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        };
        argv.push(target);

        argv
    }

    /// Borrowed accessor for the identity file (so [`crate::Client::run`]
    /// can pre-check existence without re-reading the whole struct).
    pub(crate) fn identity_file(&self) -> Option<&Path> {
        self.identity_file.as_deref()
    }

    /// Borrowed accessor for the configured host. Used by error variants
    /// that need to surface the connection target so a multi-host parallel
    /// run can map an error back to its connection. (PR #317 round-2
    /// review L3 + L8 HIGH.)
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }
}

#[cfg(unix)]
fn current_uid_string() -> String {
    // SAFETY: getuid(2) is always safe — no preconditions, no side
    // effects, returns the real UID. The `unsafe_code = "forbid"`
    // workspace lint requires a per-crate override; we deliberately
    // *don't* override it for hbird-ssh and instead route the UID
    // read through std::os::unix::fs::MetadataExt by way of
    // /proc/self/status. That keeps the crate `unsafe`-free.
    //
    // Implementation: parse `Uid:` from /proc/self/status (Linux-only,
    // which matches the project's KVM-host runtime). If parsing fails
    // for any reason — non-Linux build that still claims `cfg(unix)`,
    // restricted /proc, etc. — fall back to the literal `"uid"` so the
    // argv is still well-formed.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:").map(str::trim))
                .and_then(|tail| tail.split_whitespace().next().map(str::to_string))
        })
        .unwrap_or_else(|| "uid".to_string())
}

#[cfg(not(unix))]
fn current_uid_string() -> String {
    // Non-Unix targets don't have getuid; the ControlPath template
    // becomes /tmp/hbird-ssh-uid-... which still works (just doesn't
    // disambiguate between simultaneous operators). This branch exists
    // for cross-compile health only; the crate is Unix-only at runtime.
    "uid".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_mirror_ssh_opts_array() {
        let opts = SshOptions::new("kvm-host");
        let argv = opts.to_argv();
        assert_eq!(argv[0], "ssh");
        // Default has no -i (no identity file set).
        assert!(!argv.iter().any(|a| a == "-i"), "no identity by default");
        // Default opt set:
        let opt_pairs: Vec<&String> = argv
            .iter()
            .filter(|a| a.contains('=') && !a.starts_with("ssh"))
            .collect();
        assert!(
            opt_pairs
                .iter()
                .any(|a| a.as_str() == "StrictHostKeyChecking=no")
        );
        assert!(
            opt_pairs
                .iter()
                .any(|a| a.as_str() == "UserKnownHostsFile=/dev/null")
        );
        assert!(opt_pairs.iter().any(|a| a.as_str() == "LogLevel=ERROR"));
        assert!(opt_pairs.iter().any(|a| a.as_str() == "ConnectTimeout=10"));
        assert!(opt_pairs.iter().any(|a| a.as_str() == "BatchMode=yes"));
        // No ControlMaster by default.
        assert!(!opt_pairs.iter().any(|a| a.as_str() == "ControlMaster=auto"));
        // Target last.
        assert_eq!(argv.last().map(String::as_str), Some("kvm-host"));
    }

    #[test]
    fn identity_file_emits_minus_i() {
        let opts = SshOptions::new("h").with_identity_file("/keys/id_ed25519");
        let argv = opts.to_argv();
        // -i <path> must appear before the -o pairs to match the bash
        // twin's ordering. Find the position of "-i" and "-o" and
        // assert -i comes first.
        let i_pos = argv.iter().position(|a| a == "-i").expect("-i present");
        let first_o = argv.iter().position(|a| a == "-o").expect("-o present");
        assert!(
            i_pos < first_o,
            "-i must precede -o (matches ssh_opts_array ordering)"
        );
        assert_eq!(argv[i_pos + 1], "/keys/id_ed25519");
    }

    #[test]
    fn without_identity_suppresses_minus_i() {
        let opts = SshOptions::new("h")
            .with_identity_file("/keys/id_ed25519")
            .without_identity();
        let argv = opts.to_argv();
        assert!(
            !argv.iter().any(|a| a == "-i"),
            "without_identity() must drop the -i pair"
        );
    }

    #[test]
    fn proxy_jump_emits_proxyjump_option() {
        let opts = SshOptions::new("vm-host").with_proxy_jump("kvm-host");
        let argv = opts.to_argv();
        assert!(
            argv.iter().any(|a| a == "ProxyJump=kvm-host"),
            "ProxyJump=... must be present; argv was {argv:?}"
        );
    }

    #[test]
    fn user_at_host_format() {
        let opts = SshOptions::new("h").with_user("core");
        assert_eq!(opts.to_argv().last().map(String::as_str), Some("core@h"));
    }

    #[test]
    fn explicit_port_emits_p_flag() {
        let opts = SshOptions::new("h").with_port(2222);
        let argv = opts.to_argv();
        let p_pos = argv.iter().position(|a| a == "-p").expect("-p present");
        assert_eq!(argv[p_pos + 1], "2222");
    }

    #[test]
    fn controlmaster_emits_three_options() {
        let opts = SshOptions::new("h").with_controlmaster();
        let argv = opts.to_argv();
        assert!(argv.iter().any(|a| a == "ControlMaster=auto"));
        assert!(
            argv.iter()
                .any(|a| a.starts_with("ControlPath=/tmp/hbird-ssh-"))
        );
        assert!(argv.iter().any(|a| a == "ControlPersist=60s"));
    }

    #[test]
    fn strict_host_key_checking_flip() {
        let argv = SshOptions::new("h")
            .with_strict_host_key_checking(true)
            .to_argv();
        assert!(argv.iter().any(|a| a == "StrictHostKeyChecking=yes"));
    }

    #[test]
    fn custom_connect_timeout() {
        let argv = SshOptions::new("h")
            .with_connect_timeout(Duration::from_secs(30))
            .to_argv();
        assert!(argv.iter().any(|a| a == "ConnectTimeout=30"));
    }

    #[test]
    fn batch_mode_toggle() {
        let argv = SshOptions::new("h").with_batch_mode(false).to_argv();
        assert!(argv.iter().any(|a| a == "BatchMode=no"));
    }
}
