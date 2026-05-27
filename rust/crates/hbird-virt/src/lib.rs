//! `libvirt` (qemu/KVM) wrapper for the hummingbird-k8s Rust rewrite.
//!
//! # Why this crate exists
//!
//! The bash twin under [`../../../scripts/`](../../../scripts/) drives
//! libvirt by shelling out to `virsh -c qemu:///system <verb>` — locally
//! when the operator runs from the KVM host, or over SSH when they
//! don't (see `scripts/kubectl-k8s.sh`, `scripts/spawn-workers.sh`,
//! `scripts/clean-vms.sh`). The Rust rewrite (epic [#279]) keeps that
//! shape: this crate is a typed wrapper around the `virsh` CLI surface
//! invoked via a [`crate::ssh::SshClient`] trait object.
//!
//! What this crate is NOT:
//!
//! - It is **not** a binding to libvirt-client / the `virt` Rust crate.
//!   Those link the libvirt C library and require libvirt headers at
//!   build time. The project's deploy-cluster.sh has always driven
//!   libvirt via SSH-to-`virsh`, and that contract stays for the Rust
//!   rewrite (so the same KVM hosts the bash scripts target keep
//!   working without a libvirt-rust toolchain on the operator's
//!   workstation).
//! - It is **not** an SSH transport. That responsibility lives in
//!   sibling crate `hbird-openssh` (sub-issue [#285], in-flight in
//!   parallel with this one). This crate consumes a `dyn SshClient`
//!   from the [`crate::ssh`] module — tests use a stub; production
//!   wires up `hbird-openssh` in [#286].
//!
//! # API surface
//!
//! - [`QemuSshUri`] — typed parser + [`std::fmt::Display`] roundtrip
//!   for `qemu+ssh://[user@]host[:port]/[system|session][?query]`. This
//!   is the load-bearing piece for [#284]: every operator config
//!   eventually becomes one of these URIs.
//! - [`Connection`] — open connection to a remote libvirt daemon.
//!   Holds a [`QemuSshUri`] + an [`Arc<dyn SshClient>`](std::sync::Arc).
//!   Exposes the minimal verb set the Phase-1 subcommands need:
//!   [`Connection::domains`], [`Connection::domifaddr`],
//!   [`Connection::dominfo`].
//! - [`Error`] — flat enum carrying URI / SSH / virsh-output failures.
//!
//! Mutating libvirt operations: as of [#289] Phase 4,
//! [`Connection::destroy_domain`] and [`Connection::undefine_domain`]
//! are exposed for the destroy-cluster live path; additional verbs
//! (`start`, `define`, etc.) land when consumer crates need them.
//! Auxiliary remote-shell helpers ([`Connection::remote_rm_f`],
//! [`Connection::remote_rm_rf`], [`Connection::remote_path_exists`])
//! are exposed here too — they target the same SSH session the
//! libvirt verbs run over, so callers don't need a second
//! `SshClient` plumb to clean qcow2 + seed ISO artifacts that
//! `virsh` itself can't reach.
//!
//! [#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
//! [#284]: https://github.com/aatchison/hummingbird-k8s/issues/284
//! [#285]: https://github.com/aatchison/hummingbird-k8s/issues/285
//! [#286]: https://github.com/aatchison/hummingbird-k8s/issues/286
//! [#289]: https://github.com/aatchison/hummingbird-k8s/issues/289

#![forbid(unsafe_code)]

use std::net::Ipv4Addr;
use std::sync::Arc;

pub mod error;
pub mod ssh;
mod uri;

pub use error::{Error, Result};
pub use ssh::{SshClient, SshError};
pub use uri::{Instance, QemuSshUri};

/// libvirt domain (VM) handle.
///
/// At this stage of the rewrite the only field consumers need is the
/// VM name — every downstream call (`domifaddr`, `dominfo`, etc.) keys
/// off the name. Extra metadata (state, persistence flag) will land on
/// this struct when the consumer crates need it; keeping it minimal
/// today avoids re-parsing `virsh list` output we don't yet consume.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Domain {
    /// Domain (VM) name as libvirt reports it. This is what every
    /// `virsh <verb> <NAME>` call uses to address the VM.
    pub name: String,
}

/// Parsed `virsh dominfo <NAME>` output (subset).
///
/// The bash twin's `update-cluster` flow reads exactly four fields from
/// `dominfo`: domain name, state ("running" / "shut off"), persistence,
/// and OS-Type (used to gate the bootID check). Anything else is noise
/// at this stage of the rewrite — added on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DomainInfo {
    /// Domain name (echoes the query argument back).
    pub name: String,
    /// Lowercased domain state — e.g. `"running"`, `"shut off"`,
    /// `"paused"`. Left as a string rather than enum-ified because
    /// `virsh` upstream has added new states (`pmsuspended`, etc.) and
    /// we don't want to fail-closed on an unrecognized one.
    pub state: String,
    /// Whether the domain is persistent (defined, not just transient).
    /// Maps from the `Persistent:` row's `yes` / `no`.
    pub persistent: bool,
    /// `OS Type:` field. The bash twin uses this to confirm the VM is
    /// actually a KVM guest (vs. some other libvirt-managed type).
    pub os_type: String,
}

/// Open connection to a remote (or local) libvirt daemon.
///
/// Constructed from a [`QemuSshUri`] + an [`Arc<dyn SshClient>`]. The
/// connection is stateless — every method runs a one-shot `virsh`
/// invocation via the SSH client and parses the captured stdout. Cheap
/// to clone (the `Arc` shares the SSH client).
#[derive(Clone)]
pub struct Connection {
    uri: QemuSshUri,
    ssh: Arc<dyn SshClient>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't try to render the SSH client — it's a trait object and
        // implementations don't have to be Debug. The URI is the
        // identity-bearing piece operators care about in logs.
        f.debug_struct("Connection")
            .field("uri", &self.uri.to_string())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Open a new connection at `uri`, routing remote `virsh` commands
    /// through `ssh`.
    ///
    /// This is purely a value constructor — it does NOT touch the
    /// network. The first real round-trip happens when the caller
    /// invokes [`Self::domains`] / [`Self::domifaddr`] / [`Self::dominfo`].
    #[must_use]
    pub fn new(uri: QemuSshUri, ssh: Arc<dyn SshClient>) -> Self {
        Self { uri, ssh }
    }

    /// The URI this connection targets. Useful for diagnostics.
    #[must_use]
    pub fn uri(&self) -> &QemuSshUri {
        &self.uri
    }

    /// Enumerate all defined VMs (running + shut-off).
    ///
    /// Bash twin: `virsh -c qemu:///system list --all --name`
    /// (see `scripts/clean-vms.sh::50` and `scripts/spawn-workers.sh::98`).
    ///
    /// Returns the names verbatim — empty lines (which `virsh list
    /// --name` emits as a trailing separator) are dropped.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero (e.g.
    ///   libvirt not running on the remote, no libvirt-group
    ///   membership).
    // `err(Debug)` directive demoted to a manual `tracing::debug!` event in
    // the Err branch so callers (not this wrapper) decide ERROR-vs-debug
    // policy per call site. The original `err(Debug)` auto-fired an ERROR
    // span event for benign non-zero virsh exits (e.g. "Domain not found"
    // as a probe). (#331; original wiring #326.)
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri))]
    pub fn domains(&self) -> Result<Vec<Domain>> {
        self.domains_inner()
            .inspect_err(|err| tracing::debug!(error = ?err, "virsh domains failed"))
    }

    fn domains_inner(&self) -> Result<Vec<Domain>> {
        let cmd = format!("virsh -c {} list --all --name", self.uri.remote_uri());
        let stdout = self.run(&cmd)?;
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Domain {
                name: s.to_string(),
            })
            .collect())
    }

    /// Resolve a domain's IPv4 lease via `virsh domifaddr`.
    ///
    /// Bash twin: `scripts/kubectl-k8s.sh::56` and
    /// `scripts/spawn-workers.sh::93`. The bash awk pipeline is:
    ///
    /// ```text
    /// virsh -c qemu:///system domifaddr "$CP_NAME" \
    ///   | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}'
    /// ```
    ///
    /// We mirror it: find the first line whose 4th whitespace-separated
    /// field contains `ipv4`, then split the next field on `/` and take
    /// the prefix. Returns `Ok(None)` when no IPv4 lease is present —
    /// callers must treat that as "VM not yet booted" rather than an
    /// error, matching how the bash twin's `[[ -n "$CP_IP" ]]` guard
    /// distinguishes the two.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero (typically
    ///   "Domain not found" when `domain` is misspelled).
    /// - [`Error::UnparseableOutput`] when `virsh` returns 0 but the
    ///   `ipv4` row's CIDR field doesn't contain a parseable
    ///   [`Ipv4Addr`].
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, domain))]
    pub fn domifaddr(&self, domain: &str) -> Result<Option<Ipv4Addr>> {
        self.domifaddr_inner(domain)
            .inspect_err(|err| tracing::debug!(error = ?err, "virsh domifaddr failed"))
    }

    fn domifaddr_inner(&self, domain: &str) -> Result<Option<Ipv4Addr>> {
        let cmd = format!(
            "virsh -c {} domifaddr {}",
            self.uri.remote_uri(),
            shell_quote(domain),
        );
        let stdout = self.run(&cmd)?;
        parse_domifaddr(&stdout, &cmd)
    }

    /// Fetch `virsh dominfo <NAME>` and parse it into a [`DomainInfo`].
    ///
    /// Bash twin: `scripts/deploy-cluster.sh::482` and
    /// `scripts/destroy-cluster.sh::76` use `dominfo` as an "exists?"
    /// probe; `scripts/update-cluster.sh` reads the state field to gate
    /// the bootID check.
    ///
    /// `virsh dominfo` output is a flat `Key: value` table — the parser
    /// keys off Id / Name / State / Persistent / OS Type (whitespace
    /// in keys handled via prefix-match).
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero (most
    ///   often "Domain not found").
    /// - [`Error::UnparseableOutput`] when the output is missing the
    ///   Name, State, Persistent, or OS Type rows.
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, domain))]
    pub fn dominfo(&self, domain: &str) -> Result<DomainInfo> {
        self.dominfo_inner(domain)
            .inspect_err(|err| tracing::debug!(error = ?err, "virsh dominfo failed"))
    }

    fn dominfo_inner(&self, domain: &str) -> Result<DomainInfo> {
        let cmd = format!(
            "virsh -c {} dominfo {}",
            self.uri.remote_uri(),
            shell_quote(domain),
        );
        let stdout = self.run(&cmd)?;
        parse_dominfo(&stdout, &cmd)
    }

    /// Force-stop a running domain (`virsh destroy`).
    ///
    /// Bash twin: `scripts/destroy-cluster.sh::78` —
    /// `virsh -c qemu:///system destroy "$name" >/dev/null 2>&1 || true`.
    ///
    /// Returns `Ok(())` even if the domain was already shut off — bash's
    /// `|| true` swallows that case. A non-existent domain still surfaces
    /// as [`Error::VirshFailed`]; callers are expected to gate this
    /// behind a [`Self::dominfo`] probe (see destroy-cluster's
    /// `destroy_vm` helper which checks `dominfo` first).
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero for reasons
    ///   other than "domain not running" (which the bash twin already
    ///   silences via `|| true`; we surface it so callers can choose).
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, domain))]
    pub fn destroy_domain(&self, domain: &str) -> Result<()> {
        let cmd = format!(
            "virsh -c {} destroy {}",
            self.uri.remote_uri(),
            shell_quote(domain),
        );
        self.run(&cmd)
            .map(|_| ())
            .inspect_err(|err| tracing::debug!(error = ?err, "virsh destroy failed"))
    }

    /// Undefine a domain, removing the libvirt definition + NVRAM
    /// (`virsh undefine --nvram`).
    ///
    /// Bash twin: `scripts/destroy-cluster.sh::79` —
    /// `virsh -c qemu:///system undefine --nvram "$name" >/dev/null 2>&1 || true`.
    ///
    /// `--nvram` is required on Q35/UEFI guests; the bash twin passes
    /// it unconditionally and we mirror that.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero (e.g.
    ///   domain not defined).
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, domain))]
    pub fn undefine_domain(&self, domain: &str) -> Result<()> {
        let cmd = format!(
            "virsh -c {} undefine --nvram {}",
            self.uri.remote_uri(),
            shell_quote(domain),
        );
        self.run(&cmd)
            .map(|_| ())
            .inspect_err(|err| tracing::debug!(error = ?err, "virsh undefine failed"))
    }

    /// Remove a file on the remote (or local) KVM host via the SSH
    /// transport. Used by destroy-cluster to clean qcow2 + seed ISO +
    /// scratch-dir artifacts that aren't visible to `virsh`.
    ///
    /// Bash twin: `scripts/destroy-cluster.sh::95` —
    /// `rm_err=$(rm -f -- "$f" 2>&1)`. The `--` separator hardens against
    /// filenames that start with `-`. `rm -f` is idempotent on missing
    /// targets, matching the bash twin's idempotent-cleanup contract.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] (overloaded — captures `rm`'s stderr)
    ///   when `rm` exits non-zero. The bash twin surfaces this as a
    ///   `WARN:` log line and continues; callers here are expected to
    ///   do the same (the destroy-cluster command runs each remove
    ///   independently and aggregates warnings).
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, path))]
    pub fn remote_rm_f(&self, path: &str) -> Result<()> {
        let cmd = format!("rm -f -- {}", shell_quote(path));
        self.run(&cmd)
            .map(|_| ())
            .inspect_err(|err| tracing::debug!(error = ?err, "remote rm -f failed"))
    }

    /// **DESTRUCTIVE**: recursively remove a directory on the remote
    /// KVM host (`rm -rf --`). Callers MUST validate that `path` is a
    /// known scratch dir (e.g. under `${POOL_DIR}/`) — never operator-
    /// supplied raw input. A pre-flight guard rejects `/`, top-level
    /// system dirs (`/etc`, `/home`, `/var`, ...), empty paths,
    /// non-absolute paths, and any path containing `..` segments. See
    /// [`reject_destructive_path`] for the full rejection rules.
    ///
    /// Bash twin: `scripts/destroy-cluster.sh::113` —
    /// `_scratch_err=$(rm -rf -- "${POOL_DIR}/deploy-cluster" 2>&1)`.
    /// Idempotent on missing dirs.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] (overloaded — captures `rm`'s stderr)
    ///   when `rm` exits non-zero or when the destructive-path guard
    ///   refuses the request.
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, path))]
    pub fn remote_rm_rf(&self, path: &str) -> Result<()> {
        self.remote_rm_rf_inner(path)
            .inspect_err(|err| tracing::debug!(error = ?err, "remote rm -rf failed"))
    }

    fn remote_rm_rf_inner(&self, path: &str) -> Result<()> {
        // Round-2 lens L5#H2 + L1 MED (convergent): pre-flight guard
        // against catastrophic paths. `rm -rf -- '/'` on a root SSH
        // session would wipe the KVM host. Bash twin gets away with
        // this because POOL_DIR is interpolated into longer paths; the
        // Rust API exposes the raw string so we MUST refuse here.
        reject_destructive_path(path)?;
        let cmd = format!("rm -rf -- {}", shell_quote(path));
        self.run(&cmd).map(|_| ())
    }

    /// Probe whether a path exists on the remote KVM host
    /// (`test -e <path>`). Returns `Ok(true)` when the path exists,
    /// `Ok(false)` when it doesn't, `Err(_)` only on SSH transport
    /// failures.
    ///
    /// Bash twin: `scripts/destroy-cluster.sh::94` — `[[ -e "$f" ]] ||
    /// continue`.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, path))]
    pub fn remote_path_exists(&self, path: &str) -> Result<bool> {
        // `test -e` exits 0 when present, 1 when absent. The SshClient
        // trait surfaces exit-1 as RemoteExit; we translate the binary
        // outcome locally.
        let cmd = format!("test -e {}", shell_quote(path));
        match self.ssh.run(&self.uri.ssh_target(), &cmd) {
            Ok(_) => Ok(true),
            Err(SshError::RemoteExit {
                exit_code: Some(1), ..
            }) => Ok(false),
            Err(other) => {
                let err = Error::from(other);
                tracing::debug!(error = ?err, "remote path-exists probe failed");
                Err(err)
            }
        }
    }

    /// Run a remote command, wrapping transport / non-zero-exit failures
    /// into the crate's [`Error`] type.
    fn run(&self, command: &str) -> Result<String> {
        match self.ssh.run(&self.uri.ssh_target(), command) {
            Ok(stdout) => Ok(stdout),
            Err(SshError::RemoteExit {
                stderr, exit_code, ..
            }) => {
                // Distinguish "ssh succeeded; virsh exited non-zero"
                // (VirshFailed) from "ssh itself failed" (Error::Ssh).
                // The exit_code is informational — operators read
                // stderr, not the integer.
                let _ = exit_code;
                Err(Error::VirshFailed {
                    command: command.to_string(),
                    stderr,
                })
            }
            Err(other) => Err(Error::from(other)),
        }
    }
}

/// Quote `s` so it survives passing through `sh -c` on the remote.
///
/// Defensive — domain names in this project follow `^[a-zA-Z0-9_-]+$`,
/// but accepting that constraint silently would let a malicious /
/// fat-fingered config inject shell metacharacters via a domain name
/// from `cluster.local.conf`. Single-quote wrap; escape any embedded
/// single quote with `'\''`.
/// Reject paths that would be catastrophic for [`Connection::remote_rm_rf`].
/// Round-2 lens L5#H2 + L1 MED on PR #337: `rm -rf -- '/'`, empty paths,
/// `..`-bearing paths, and top-level system dirs are all rejected at
/// the API boundary so an upstream bug or malicious config can't trigger
/// a host-wide wipe via a root SSH session.
///
/// Defense-in-depth — the legitimate callers (destroy-cluster + future
/// deploy-cluster cleanup) always pass `${POOL_DIR}/<subdir>` which
/// won't trip this guard, but a defaulting bug or a config injection
/// could.
fn reject_destructive_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::VirshFailed {
            command: "remote_rm_rf".to_string(),
            stderr: "refusing empty path: rm -rf '' would target cwd".to_string(),
        });
    }
    if !path.starts_with('/') {
        return Err(Error::VirshFailed {
            command: "remote_rm_rf".to_string(),
            stderr: format!("refusing non-absolute path: {path:?} (must start with /)"),
        });
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err(Error::VirshFailed {
            command: "remote_rm_rf".to_string(),
            stderr: format!("refusing path with `..` segment: {path:?}"),
        });
    }
    // Reject `/` and the top-level system dirs an operator should
    // never need to recursively delete via this helper.
    let banned = [
        "/", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib64", "/proc", "/root", "/run",
        "/sbin", "/srv", "/sys", "/tmp", "/usr", "/var",
    ];
    let trimmed = path.trim_end_matches('/');
    let to_check = if trimmed.is_empty() { "/" } else { trimmed };
    if banned.contains(&to_check) {
        return Err(Error::VirshFailed {
            command: "remote_rm_rf".to_string(),
            stderr: format!(
                "refusing destructive path: {path:?} is a top-level system \
                 directory; remote_rm_rf is only meant for cluster scratch \
                 dirs under POOL_DIR"
            ),
        });
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Parse `virsh domifaddr` output. Visible for unit tests in the
/// `tests/` integration suite; not part of the public API.
#[doc(hidden)]
pub fn parse_domifaddr(stdout: &str, command: &str) -> Result<Option<Ipv4Addr>> {
    for line in stdout.lines() {
        // The table header includes the literal token "Name" so we
        // skip any row that doesn't contain "ipv4" — case-insensitive
        // because `virsh`'s capitalization has drifted across releases.
        if !line.to_ascii_lowercase().contains("ipv4") {
            continue;
        }
        // Fields: vif-name  mac  protocol  address-with-cidr
        // The bash awk picks $4 (the address); we mirror it.
        let cidr = line
            .split_whitespace()
            .nth(3)
            .ok_or(Error::UnparseableOutput {
                command: command.to_string(),
                reason: "ipv4 row had fewer than 4 whitespace-separated fields",
            })?;
        let addr = cidr.split('/').next().unwrap_or(cidr);
        let parsed: Ipv4Addr = addr.parse().map_err(|_| Error::UnparseableOutput {
            command: command.to_string(),
            reason: "ipv4 row's address field did not parse as IPv4",
        })?;
        return Ok(Some(parsed));
    }
    Ok(None)
}

/// Parse `virsh dominfo` output. Visible for unit tests in the
/// `tests/` integration suite; not part of the public API.
#[doc(hidden)]
pub fn parse_dominfo(stdout: &str, command: &str) -> Result<DomainInfo> {
    let mut name = None;
    let mut state = None;
    let mut persistent = None;
    let mut os_type = None;

    for line in stdout.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "Name" => name = Some(value.to_string()),
            "State" => state = Some(value.to_ascii_lowercase()),
            "Persistent" => persistent = Some(value.eq_ignore_ascii_case("yes")),
            "OS Type" => os_type = Some(value.to_string()),
            _ => {} // Id, UUID, CPU(s), etc. — not consumed yet.
        }
    }

    Ok(DomainInfo {
        name: name.ok_or(Error::UnparseableOutput {
            command: command.to_string(),
            reason: "dominfo output missing 'Name:' row",
        })?,
        state: state.ok_or(Error::UnparseableOutput {
            command: command.to_string(),
            reason: "dominfo output missing 'State:' row",
        })?,
        persistent: persistent.ok_or(Error::UnparseableOutput {
            command: command.to_string(),
            reason: "dominfo output missing 'Persistent:' row",
        })?,
        os_type: os_type.ok_or(Error::UnparseableOutput {
            command: command.to_string(),
            reason: "dominfo output missing 'OS Type:' row",
        })?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_passthrough_for_safe_chars() {
        assert_eq!(shell_quote("hbird-cp1"), "hbird-cp1");
        assert_eq!(shell_quote("hbird_cp_42"), "hbird_cp_42");
        assert_eq!(shell_quote("cluster.local"), "cluster.local");
    }

    #[test]
    fn shell_quote_wraps_metacharacters() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("$(id)"), "'$(id)'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quote() {
        // `it's` -> 'it'\''s'
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn parse_domifaddr_picks_first_ipv4() {
        // Real `virsh domifaddr` output (with the table-header rows it
        // emits unconditionally).
        let out = " Name       MAC address          Protocol     Address\n\
                    -------------------------------------------------------------------------------\n\
                    vnet0      52:54:00:01:02:03    ipv4         192.168.122.42/24\n";
        let cmd = "virsh -c qemu:///system domifaddr hbird-cp1";
        let ip = parse_domifaddr(out, cmd).expect("parse ok").unwrap();
        assert_eq!(ip, Ipv4Addr::new(192, 168, 122, 42));
    }

    #[test]
    fn parse_domifaddr_returns_none_when_no_lease() {
        // Empty output (VM running but no DHCP lease yet).
        let cmd = "virsh -c qemu:///system domifaddr hbird-cp1";
        assert_eq!(parse_domifaddr("", cmd).expect("parse ok"), None);
        // Header-only output (also a real shape; `virsh` always emits
        // headers even when there are no rows).
        let header_only = " Name       MAC address          Protocol     Address\n\
                            -------------------------------------------------------------------------------\n";
        assert_eq!(parse_domifaddr(header_only, cmd).expect("parse ok"), None);
    }

    #[test]
    fn parse_domifaddr_skips_ipv6_rows() {
        // VMs on dual-stack networks emit both — we want the v4.
        let out = " vnet0      52:54:00:01:02:03    ipv6         fe80::aaaa/64\n\
                    vnet0      52:54:00:01:02:03    ipv4         10.0.0.5/24\n";
        let cmd = "virsh -c qemu:///system domifaddr w1";
        let ip = parse_domifaddr(out, cmd).expect("parse ok").unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 0, 0, 5));
    }

    #[test]
    fn parse_domifaddr_errors_on_garbage_address() {
        let out = " vnet0      52:54:00:01:02:03    ipv4         not-an-ip/24\n";
        let cmd = "virsh -c qemu:///system domifaddr w1";
        let err = parse_domifaddr(out, cmd).expect_err("garbage address should error");
        assert!(matches!(err, Error::UnparseableOutput { .. }));
    }

    #[test]
    fn parse_dominfo_picks_named_rows() {
        let out = "Id:             3\n\
                   Name:           hbird-cp1\n\
                   UUID:           dd2b9a92-aaaa-bbbb-cccc-ddddeeeeffff\n\
                   OS Type:        hvm\n\
                   State:          running\n\
                   CPU(s):         4\n\
                   Persistent:     yes\n\
                   Autostart:      disable\n\
                   Managed save:   no\n";
        let cmd = "virsh -c qemu:///system dominfo hbird-cp1";
        let info = parse_dominfo(out, cmd).expect("parse ok");
        assert_eq!(info.name, "hbird-cp1");
        assert_eq!(info.state, "running");
        assert!(info.persistent);
        assert_eq!(info.os_type, "hvm");
    }

    #[test]
    fn parse_dominfo_missing_state_errors() {
        let out = "Name:           x\nPersistent:     no\nOS Type:        hvm\n";
        let cmd = "virsh -c qemu:///system dominfo x";
        let err = parse_dominfo(out, cmd).expect_err("missing State must error");
        assert!(matches!(err, Error::UnparseableOutput { .. }));
    }
}
