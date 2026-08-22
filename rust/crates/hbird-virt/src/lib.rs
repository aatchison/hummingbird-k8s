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
//! are exposed for the destroy-cluster live path. As of [#289] Stage 1,
//! [`Connection::start_domain`], [`Connection::virsh_pool_refresh`],
//! [`Connection::virt_install`], and [`Connection::remote_cp_reflink`]
//! are added for the deploy-cluster/spawn-workers live paths (S2/S3).
//! Auxiliary remote-shell helpers ([`Connection::remote_rm_f`],
//! [`Connection::remote_rm_rf`], [`Connection::remote_path_exists`])
//! are exposed here too — they target the same SSH session the
//! libvirt verbs run over, so callers don't need a second
//! `SshClient` plumb to clean qcow2 + seed ISO artifacts that
//! `virsh` itself can't reach.
//!
//! A shared retry/poll helper lives in [`crate::poll`] for use by
//! CP-IP and cluster-ready polls in S2/S3.
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
mod local;
pub mod poll;
pub mod ssh;
mod uri;

pub use error::{Error, Result};
pub use local::LocalClient;
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

/// Full VM description for [`Connection::virt_install_vm`] (#405/#409).
///
/// Groups what would otherwise be seven positional arguments (clippy's
/// `too_many_arguments`, and four adjacent `&str`s a call site could
/// silently transpose). Lifetimed borrows — the spec is a per-call view,
/// not an owned model.
#[derive(Debug, Clone)]
pub struct VmSpec<'a> {
    /// Libvirt domain name.
    pub name: &'a str,
    /// Memory in MiB (`--memory`).
    pub memory_mib: u64,
    /// vCPU count (`--vcpus`).
    pub vcpus: u32,
    /// Primary qcow2 disk path.
    pub disk_path: &'a str,
    /// Optional cloud-init seed ISO attached as a read-only cdrom.
    pub cdrom: Option<&'a str>,
    /// Optional pinned MAC for the primary NIC (deploy-cluster #409).
    /// Callers must pass a validated `aa:bb:cc:dd:ee:ff` string.
    pub primary_mac: Option<&'a str>,
    /// Optional second NIC (deploy-cluster #405 `EXTRA_NETWORK`).
    pub extra_nic: Option<ExtraNic<'a>>,
}

/// Second-NIC attachment for [`VmSpec`]: a named libvirt network plus the
/// guest-visible MAC that the cloud-init network-config matches on.
#[derive(Debug, Clone)]
pub struct ExtraNic<'a> {
    /// Libvirt network name (`EXTRA_NETWORK`).
    pub network: &'a str,
    /// Validated `aa:bb:cc:dd:ee:ff` MAC (`EXTRA_NET_*_MAC`).
    pub mac: &'a str,
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

    /// Open a local connection targeting `qemu:///system` on this machine.
    ///
    /// Uses [`LocalClient`] to run `virsh`/`cp`/`virt-install` via
    /// `sh -c` rather than SSH. This is the operator-on-KVM-host path
    /// (Option A, #289 S2a): `kvm_host` absent or empty in the config.
    #[must_use]
    pub fn new_local() -> Self {
        Self::new_local_with_client(Arc::new(LocalClient::new()))
    }

    /// Open a local-transport connection with an injected [`SshClient`].
    ///
    /// Like [`Self::new_local`] but accepts any `SshClient` implementation.
    /// Primary use: unit tests that inject a stub to capture command strings
    /// without running anything locally (mirrors the [`Self::new`] + stub
    /// pattern used for the SSH path).
    #[must_use]
    pub fn new_local_with_client(client: Arc<dyn SshClient>) -> Self {
        // A synthetic URI is needed for `remote_uri()` → `qemu:///system`.
        // The `ssh_target()` ("localhost") is passed to the SshClient but
        // LocalClient ignores it; stubs key only on the command string.
        let uri = QemuSshUri::parse("qemu+ssh://localhost/system")
            .expect("static local URI is always valid");
        Self { uri, ssh: client }
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
        Ok(parse_domain_list(&stdout))
    }

    /// Enumerate only the **running** VMs (`virsh list --name`, without
    /// `--all`).
    ///
    /// Bash twin: `scripts/switch-to-ghcr.sh::236` —
    /// `virsh -c qemu:///system list --name 2>/dev/null | grep '^hummingbird-'`.
    ///
    /// Distinct from [`Self::domains`], which passes `--all` and therefore
    /// also returns shut-off domains. `switch-to-ghcr` must NOT touch a
    /// shut-off domain (there is no sshd to reach), so the two call sites
    /// need different virsh invocations rather than a post-filter.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero.
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri))]
    pub fn running_domains(&self) -> Result<Vec<Domain>> {
        self.running_domains_inner()
            .inspect_err(|err| tracing::debug!(error = ?err, "virsh running domains failed"))
    }

    fn running_domains_inner(&self) -> Result<Vec<Domain>> {
        let cmd = format!("virsh -c {} list --name", self.uri.remote_uri());
        let stdout = self.run(&cmd)?;
        Ok(parse_domain_list(&stdout))
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

    /// Undefine a domain **and delete every storage volume it owns**
    /// (`virsh undefine <NAME> --remove-all-storage`).
    ///
    /// Bash twin: `scripts/clean-vms.sh::57` —
    /// `virsh -c qemu:///system undefine "$d" --remove-all-storage 2>/dev/null || true`.
    ///
    /// **DESTRUCTIVE and distinct from [`Self::undefine_domain`]**: that
    /// one passes `--nvram` and leaves the qcow2 on disk (destroy-cluster
    /// removes the files itself, by path). This one asks libvirt to drop
    /// the backing volumes, which is what `make clean-vms` wants — it
    /// sweeps domains it did not necessarily deploy, so it cannot know
    /// their disk paths.
    ///
    /// Flag order mirrors the bash twin (name first, flag second) so the
    /// emitted command string is greppable against `scripts/clean-vms.sh`.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero (e.g. domain
    ///   not defined, or a volume libvirt refuses to delete).
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, domain))]
    pub fn undefine_domain_remove_all_storage(&self, domain: &str) -> Result<()> {
        let cmd = format!(
            "virsh -c {} undefine {} --remove-all-storage",
            self.uri.remote_uri(),
            shell_quote(domain),
        );
        self.run(&cmd).map(|_| ()).inspect_err(
            |err| tracing::debug!(error = ?err, "virsh undefine --remove-all-storage failed"),
        )
    }

    /// List the entries of a directory on the KVM host (`ls -1 -- <dir>`).
    ///
    /// Bash twin: the `shopt -s nullglob` + `"$POOL_DIR"/hummingbird-*.qcow2`
    /// glob block in `scripts/clean-vms.sh::62-72`. Expanding the glob in
    /// bash means the *remote shell* decides what matches; doing the
    /// listing here and the matching in the caller keeps the match rule in
    /// Rust where it can be unit-tested (and cannot be widened by a stray
    /// `shopt`).
    ///
    /// Returns bare entry names (no directory prefix), in the order `ls`
    /// emitted them. Empty lines are dropped.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] (overloaded — captures `ls`'s stderr) when
    ///   `ls` exits non-zero, e.g. the directory does not exist. Callers
    ///   that treat a missing pool dir as "nothing to sweep" must map that
    ///   variant themselves; `nullglob` in the bash twin silently yields an
    ///   empty list for the same case.
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, dir))]
    pub fn remote_ls(&self, dir: &str) -> Result<Vec<String>> {
        let cmd = format!("ls -1 -- {}", shell_quote(dir));
        let stdout = self
            .run(&cmd)
            .inspect_err(|err| tracing::debug!(error = ?err, "remote ls failed"))?;
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
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

    /// Start a defined (but shut-off) domain (`virsh start`).
    ///
    /// Bash twin: `scripts/spawn-workers.sh::247` —
    /// `virsh -c qemu:///system start "$NAME" 2>/dev/null || true`.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero (e.g. domain
    ///   not defined or already running).
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, domain))]
    pub fn start_domain(&self, domain: &str) -> Result<()> {
        let cmd = format!(
            "virsh -c {} start {}",
            self.uri.remote_uri(),
            shell_quote(domain),
        );
        self.run(&cmd)
            .map(|_| ())
            .inspect_err(|err| tracing::debug!(error = ?err, "virsh start failed"))
    }

    /// Refresh a libvirt storage pool (`virsh pool-refresh`).
    ///
    /// Bash twin: `scripts/spawn-workers.sh::288` —
    /// `virsh -c qemu:///system pool-refresh mass2 >/dev/null || true`.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virsh` exits non-zero.
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, pool))]
    pub fn virsh_pool_refresh(&self, pool: &str) -> Result<()> {
        let cmd = format!(
            "virsh -c {} pool-refresh {}",
            self.uri.remote_uri(),
            shell_quote(pool),
        );
        self.run(&cmd)
            .map(|_| ())
            .inspect_err(|err| tracing::debug!(error = ?err, "virsh pool-refresh failed"))
    }

    /// Copy a file on the remote KVM host using `cp --reflink=auto`.
    ///
    /// Bash twin: `scripts/deploy-cluster.sh::691` and
    /// `scripts/spawn-workers.sh::251` —
    /// `cp --reflink=auto "$TEMPLATE" "$QCOW"`.
    ///
    /// `--reflink=auto` produces a copy-on-write clone on btrfs/xfs
    /// (zero-cost until the first write), falling back to a full copy on
    /// filesystems that don't support reflinking.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] (overloaded) when `cp` exits non-zero.
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, src, dst))]
    pub fn remote_cp_reflink(&self, src: &str, dst: &str) -> Result<()> {
        let cmd = format!(
            "cp --reflink=auto {} {}",
            shell_quote(src),
            shell_quote(dst),
        );
        self.run(&cmd)
            .map(|_| ())
            .inspect_err(|err| tracing::debug!(error = ?err, "cp --reflink=auto failed"))
    }

    /// Install a new VM via `virt-install --import --noautoconsole`.
    ///
    /// Mirrors the bash twin's invocation shape exactly (flags, order)
    /// so the generated remote command is diff-friendly against the bash
    /// scripts.
    ///
    /// Bash twins:
    /// - CP variant (with cdrom): `scripts/deploy-cluster.sh::700-710`.
    /// - Worker variant (no cdrom): `scripts/spawn-workers.sh::276-284`.
    ///
    /// # Arguments
    ///
    /// - `name` — VM name (must not contain shell metacharacters; see
    ///   [`shell_quote`] for the quoting applied).
    /// - `memory_mib` — RAM in MiB (`--memory`).
    /// - `vcpus` — virtual CPUs (`--vcpus`).
    /// - `disk_path` — absolute path to the qcow2 disk image on the KVM
    ///   host. Passed as `--disk <path>,format=qcow2,bus=virtio`.
    /// - `cdrom` — optional path to a cloud-init seed ISO on the KVM
    ///   host. When present, appended as
    ///   `--disk path=<cdrom>,device=cdrom,readonly=on`.
    ///
    /// # Errors
    ///
    /// - [`Error::Ssh`] for SSH transport failures.
    /// - [`Error::VirshFailed`] when `virt-install` exits non-zero.
    #[tracing::instrument(level = "debug", skip(self), fields(uri = %self.uri, name, memory_mib, vcpus))]
    pub fn virt_install(
        &self,
        name: &str,
        memory_mib: u64,
        vcpus: u32,
        disk_path: &str,
        cdrom: Option<&str>,
    ) -> Result<()> {
        self.virt_install_vm(&VmSpec {
            name,
            memory_mib,
            vcpus,
            disk_path,
            cdrom,
            primary_mac: None,
            extra_nic: None,
        })
    }

    /// `virt-install` with full NIC control (#405/#409).
    ///
    /// Same command skeleton as [`Connection::virt_install`], plus:
    /// - `spec.primary_mac` pins the primary NIC's MAC
    ///   (`--network network=default,model=virtio,mac=<mac>`) so a rebuilt
    ///   VM keeps its DHCP lease — bash twin `deploy-cluster.sh` #409.
    /// - `spec.extra_nic` appends a second `--network network=<net>,mac=<mac>`.
    ///   No `model=` on the second NIC: for a hostdev/VF-pool network
    ///   libvirt ignores it (bash twin comment, deploy-cluster.sh ~L1010).
    ///
    /// Takes a [`VmSpec`] rather than positional args: seven-plus
    /// parameters is clippy's `too_many_arguments` territory and the
    /// string-typed fields would be silently transposable at call sites.
    pub fn virt_install_vm(&self, spec: &VmSpec<'_>) -> Result<()> {
        let mut cmd = format!(
            "virt-install --connect {} --name {} --memory {} --vcpus {} --disk {},format=qcow2,bus=virtio",
            self.uri.remote_uri(),
            shell_quote(spec.name),
            spec.memory_mib,
            spec.vcpus,
            shell_quote(spec.disk_path),
        );
        if let Some(cdrom_path) = spec.cdrom {
            cmd.push_str(&format!(
                " --disk path={},device=cdrom,readonly=on",
                shell_quote(cdrom_path),
            ));
        }
        // NIC arguments stay unquoted (byte-parity with the historical
        // command shape pinned by tests/virsh_commands.rs). MACs are
        // caller-validated `aa:bb:cc:dd:ee:ff` strings; only the operator-
        // supplied extra-network NAME goes through shell_quote.
        cmd.push_str(
            " --import --os-variant fedora-unknown --network network=default,model=virtio",
        );
        if let Some(mac) = spec.primary_mac {
            cmd.push_str(&format!(",mac={mac}"));
        }
        if let Some(extra) = &spec.extra_nic {
            cmd.push_str(&format!(
                " --network network={},mac={}",
                shell_quote(extra.network),
                extra.mac,
            ));
        }
        cmd.push_str(" --graphics vnc,listen=127.0.0.1 --noautoconsole");
        self.run(&cmd)
            .map(|_| ())
            .inspect_err(|err| tracing::debug!(error = ?err, "virt-install failed"))
    }

    /// `virsh net-dumpxml <network>` — raw XML of a libvirt network.
    ///
    /// Used by deploy-cluster's DHCP-reservation idempotency probe (bash
    /// twin `ensure_dhcp_reservation`, deploy-cluster.sh #409): the caller
    /// greps the XML for an existing `mac='…'` / `ip='…'` entry before
    /// attempting `net-update`.
    pub fn net_dumpxml(&self, network: &str) -> Result<String> {
        let cmd = format!(
            "virsh -c {} net-dumpxml {}",
            self.uri.remote_uri(),
            shell_quote(network),
        );
        self.run(&cmd)
    }

    /// `virsh net-info <network>` — raw human-readable network summary.
    ///
    /// deploy-cluster's EXTRA_NETWORK preflight (#405) reads two facts
    /// from it: whether the network is defined at all (exit code) and the
    /// `Active:` line (bash: `awk '/^Active:/{print $2}' | grep -qi yes`).
    pub fn net_info(&self, network: &str) -> Result<String> {
        let cmd = format!(
            "virsh -c {} net-info {}",
            self.uri.remote_uri(),
            shell_quote(network),
        );
        self.run(&cmd)
    }

    /// `virsh net-update <network> add ip-dhcp-host "<host …/>" --live --config`.
    ///
    /// Adds a DHCP reservation so a VM's primary address is a RESERVATION,
    /// not merely a sticky lease. XML payload mirrors the bash twin
    /// byte-for-byte: `<host mac='<mac>' name='<name>' ip='<ip>'/>`.
    pub fn net_update_add_ip_dhcp_host(
        &self,
        network: &str,
        mac: &str,
        name: &str,
        ip: &str,
    ) -> Result<()> {
        let xml = format!("<host mac='{mac}' name='{name}' ip='{ip}'/>");
        let cmd = format!(
            "virsh -c {} net-update {} add ip-dhcp-host {} --live --config",
            self.uri.remote_uri(),
            shell_quote(network),
            shell_quote(&xml),
        );
        self.run(&cmd)
            .map(|_| ())
            .inspect_err(|err| tracing::debug!(error = ?err, "net-update failed"))
    }

    /// Execute an arbitrary shell command via this connection's transport.
    ///
    /// For a local transport ([`LocalClient`]): runs via `sh -c` on this
    /// machine. For an SSH transport ([`Connection::new`]): runs on the
    /// remote host configured in the [`QemuSshUri`].
    ///
    /// Used by deploy-cluster for `podman`/`make`/bib operations that share
    /// the same transport as the libvirt verb calls — so the caller doesn't
    /// need a second SSH client plumbed alongside the `Connection`.
    pub fn exec_shell(&self, cmd: &str) -> Result<String> {
        self.run(cmd)
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

/// Split `virsh list [--all] --name` stdout into [`Domain`] values.
///
/// `virsh list --name` emits one name per line plus a trailing blank
/// separator line; both call sites want the same trim-and-drop-empties
/// treatment, so the rule lives here rather than being duplicated in
/// [`Connection::domains`] and [`Connection::running_domains`].
fn parse_domain_list(stdout: &str) -> Vec<Domain> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Domain {
            name: s.to_string(),
        })
        .collect()
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
