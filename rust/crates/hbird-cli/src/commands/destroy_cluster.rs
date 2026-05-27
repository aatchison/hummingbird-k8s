//! `hbird destroy-cluster` — bash twin: `scripts/destroy-cluster.sh`.
//!
//! Phase 4 of the operator-CLI Rust rewrite (epic [#279], implementation
//! tracked by [#289]). The bash twin is a 119-line cleanup script that
//! tears down every CP_NAME and WORKER_NAMES domain, then removes the
//! qcow2 + seed ISO + deploy-cluster scratch dir from POOL_DIR.
//!
//! # Scope
//!
//! Both dry-run and live execution paths are implemented:
//!
//! - `--dry-run` emits the deterministic plan (which VMs would be
//!   destroyed, which files would be removed). Fixture
//!   `tests/update_cluster/fixtures/dry_run_destroy.txt` pins the output.
//! - Live execution routes virsh through [`hbird_virt::Connection`]
//!   (`destroy_domain` + `undefine_domain` added in this PR), and the
//!   per-file cleanup via SSH `rm -f`. Idempotent: a missing VM is not
//!   an error, matching the bash twin's `>/dev/null 2>&1 || true`
//!   pattern.
//!
//! # Block traceability
//!
//! Each `// ---- <name> ----` header matches a section of
//! `scripts/destroy-cluster.sh`:
//!
//! 1. Config + arg loading       → [`Plan::from_args`]
//! 2. virsh existence probe      → [`destroy_one`] via [`hbird_virt::Connection::dominfo`]
//! 3. virsh destroy + undefine   → [`destroy_one`] via destroy_domain/undefine_domain
//! 4. qcow2 + seed ISO cleanup   → [`destroy_one`] via remote_rm_f
//! 5. Scratch-dir cleanup        → [`cleanup_scratch_dir`] via remote_rm_rf
//!
//! [#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
//! [#289]: https://github.com/aatchison/hummingbird-k8s/issues/289

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::Args;
use clap::builder::BoolishValueParser;

use hbird_config::ClusterConfig;
use hbird_ssh::{Client, SshOptions};
use hbird_virt::ssh::{SshClient, SshError};
use hbird_virt::{Connection, Error as VirtError, Instance, QemuSshUri};

// ---- Arguments (block #1: clap surface) ------------------------------------

/// Arguments for `hbird destroy-cluster`.
#[derive(Debug, Args)]
pub struct DestroyClusterArgs {
    /// Path to `cluster.local.conf`. Required.
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,

    /// SSH alias of the KVM host. Overrides `KVM_HOST` env / config.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// Skip the `sudo` probe on the KVM host (libvirt-group operator
    /// path, #305). `env` mirrors `HBIRD_REMOTE_NO_SUDO=1` used by the
    /// bash twin's `scripts/lib/ssh-wrap.sh`; `BoolishValueParser`
    /// accepts `1`/`0`/`yes`/`no` so the env-var path matches the bash
    /// twin's `[[ -n $HBIRD_REMOTE_NO_SUDO ]]` truthiness.
    #[arg(
        long,
        env = "HBIRD_REMOTE_NO_SUDO",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub no_sudo: bool,

    /// Plan-only mode — print the destroy plan without invoking virsh or
    /// removing files. The bash twin has no `--dry-run` flag; this is a
    /// Rust-side addition so operators can preview the impact before
    /// committing. (#289.)
    #[arg(long)]
    pub dry_run: bool,
}

// ---- Logger ----------------------------------------------------------------

fn log(line: &str) {
    println!("[destroy-cluster] {line}");
}

fn log_warn(line: &str) {
    // Bash twin uses `log "WARN: ..."` to stdout for non-fatal warnings;
    // mirror that channel so grep behavior across both sides matches.
    println!("[destroy-cluster] {line}");
}

// ---- Plan -----------------------------------------------------------------

/// Merged view of args + config + env. Built once at the top of [`run`].
#[derive(Debug, Clone)]
struct Plan {
    config_path: PathBuf,
    cp_name: String,
    worker_names: Vec<String>,
    pool_dir: String,
    kvm_host: Option<String>,
    dry_run: bool,
}

impl Plan {
    fn from_args(args: &DestroyClusterArgs, config: ClusterConfig) -> Self {
        // WORKER_NAMES default: legacy 2-worker is preserved by
        // resolved_worker_names(); explicit empty stays empty.
        let worker_names = config.resolved_worker_names();
        Self {
            config_path: args.config.clone(),
            cp_name: config.cp_name,
            worker_names,
            pool_dir: config.pool_dir,
            // Resolution order: --kvm-host (incl. KVM_HOST env) wins,
            // then config file. Mirrors `nodes.rs::resolve_target`.
            kvm_host: args.kvm_host.clone().or(config.kvm_host),
            dry_run: args.dry_run,
        }
    }

    /// Every VM the bash twin would tear down — CP first, then workers
    /// in declared order. Matches `destroy-cluster.sh::102-105`.
    fn vms(&self) -> Vec<String> {
        let mut v = Vec::with_capacity(1 + self.worker_names.len());
        v.push(self.cp_name.clone());
        v.extend(self.worker_names.iter().cloned());
        v
    }

    /// Per-VM artifact paths under POOL_DIR. Matches
    /// `destroy-cluster.sh::91-93`: `<name>.qcow2`, `<name>-seed.iso`,
    /// `<name>-cloud-init.iso`.
    fn artifact_paths(&self, name: &str) -> [String; 3] {
        [
            format!("{}/{name}.qcow2", self.pool_dir),
            format!("{}/{name}-seed.iso", self.pool_dir),
            format!("{}/{name}-cloud-init.iso", self.pool_dir),
        ]
    }

    /// Per-deploy scratch dir under POOL_DIR (`destroy-cluster.sh::111`).
    fn scratch_dir(&self) -> String {
        format!("{}/deploy-cluster", self.pool_dir)
    }
}

// ---- Block #2-4: destroy a single VM + its artifacts -----------------------

/// Tear down one VM and its on-disk artifacts. Idempotent.
///
/// Mirrors the bash twin's `destroy_vm()` (line 73-99):
///
/// 1. Probe with `virsh dominfo`. If absent, log "no such domain
///    (already torn down)" and skip to artifact cleanup.
/// 2. `virsh destroy` (force-stop). `|| true`.
/// 3. `virsh undefine --nvram`. `|| true`.
/// 4. `rm -f` the qcow2 + seed ISO + cloud-init ISO. Surface a WARN
///    per file that couldn't be removed (typically pre-#305 root-owned
///    files when the operator runs as a libvirt-group user — same
///    diagnostic the bash twin emits).
///
/// Live mode only; dry-run is handled in [`run_dry_run`] up the stack.
#[tracing::instrument(level = "debug", skip(conn, plan), fields(vm = name), err(Debug))]
fn destroy_one(conn: &Connection, plan: &Plan, name: &str) -> Result<()> {
    // Step 1: dominfo probe. We treat any error here as "VM not
    // defined" — same as the bash twin's `>/dev/null 2>&1` redirect.
    // Distinguishing transport failures would be nice but the bash
    // twin doesn't, so we don't either (parity over polish).
    let exists = conn.dominfo(name).is_ok();
    if exists {
        log(&format!("destroying {name}"));
        // Step 2 + 3: destroy + undefine. `|| true` — swallow errors.
        if let Err(e) = conn.destroy_domain(name) {
            // Match bash's silence here — only surface if the operator
            // cranked tracing up. Operators won't see this at default
            // log level, matching parity with `|| true`.
            tracing::debug!(error = ?e, "virsh destroy returned non-zero (likely already shut off — bash twin silences this)");
        }
        if let Err(e) = conn.undefine_domain(name) {
            tracing::debug!(error = ?e, "virsh undefine returned non-zero (bash twin silences this)");
        }
    } else {
        log(&format!("{name}: no such domain (already torn down)"));
    }

    // Step 4: artifact cleanup. Per-path: skip when absent (matches
    // bash `[[ -e $f ]] || continue`); WARN on rm failure.
    for path in plan.artifact_paths(name) {
        match conn.remote_path_exists(&path) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => {
                log_warn(&format!("WARN: could not check existence of {path}: {e}"));
                continue;
            }
        }
        if let Err(e) = conn.remote_rm_f(&path) {
            // Surface the bash twin's recovery hint verbatim — the
            // pre-#305 root-owned-qcow2 migration hazard.
            log_warn(&format!(
                "WARN: could not remove {path}: {e}. If pre-#305 root-owned, run once as root: sudo rm -f {path}"
            ));
        }
    }
    Ok(())
}

// ---- Block #5: scratch-dir cleanup -----------------------------------------

/// Clean the per-deploy scratch dir under POOL_DIR. Mirrors
/// `destroy-cluster.sh::111-117`. Surface rm failures as WARN, never
/// fatal.
#[tracing::instrument(level = "debug", skip(conn, plan), err(Debug))]
fn cleanup_scratch_dir(conn: &Connection, plan: &Plan) -> Result<()> {
    let scratch = plan.scratch_dir();
    match conn.remote_path_exists(&scratch) {
        Ok(false) => return Ok(()),
        Ok(true) => {}
        Err(e) => {
            log_warn(&format!(
                "WARN: could not check existence of {scratch}: {e}"
            ));
            return Ok(());
        }
    }
    log(&format!("removing scratch dir {scratch}"));
    if let Err(e) = conn.remote_rm_rf(&scratch) {
        log_warn(&format!(
            "WARN: could not fully remove {scratch}: {e}. If pre-#305 root-owned, run once as root: sudo rm -rf {scratch}"
        ));
    }
    Ok(())
}

// ---- Dry-run path ----------------------------------------------------------

fn run_dry_run(plan: &Plan) {
    log(&format!("config: {}", plan.config_path.display()));
    log(&format!(
        "DRY-RUN tearing down cluster defined in {}",
        plan.config_path.display(),
    ));
    for vm in plan.vms() {
        log(&format!(
            "DRY-RUN would virsh dominfo {vm} (existence probe)"
        ));
        log(&format!(
            "DRY-RUN if defined: would virsh destroy + undefine --nvram {vm}"
        ));
        for path in plan.artifact_paths(&vm) {
            log(&format!("DRY-RUN would rm -f {path} (if present)"));
        }
    }
    log(&format!(
        "DRY-RUN would rm -rf {} (if present)",
        plan.scratch_dir(),
    ));
    log("DRY-RUN done.");
}

// ---- SshClient bridge ------------------------------------------------------

/// Bridge that lets [`hbird_virt::Connection`] call out via
/// [`hbird_ssh::Client`].
///
/// `hbird-virt` defines its own `SshClient` trait (so the crate stays
/// dep-free of an SSH backend); `hbird-ssh::Client` provides the real
/// OpenSSH-subprocess implementation. This struct is the seam.
///
/// `host` is captured at construction time and overrides the `host`
/// argument passed to `SshClient::run` — when KVM_HOST is set, every
/// call goes to that host regardless of the `qemu+ssh://` URI's
/// embedded host. (The URI's host stays in the embedded
/// `qemu+ssh://...` string for `virsh -c` though.)
struct CliSshBridge {
    inner: Client,
}

impl CliSshBridge {
    fn new(options: SshOptions) -> Self {
        Self {
            inner: Client::new(options),
        }
    }
}

impl SshClient for CliSshBridge {
    fn run(&self, _host: &str, command: &str) -> std::result::Result<String, SshError> {
        // Use the captured options' host rather than the `host` arg.
        // hbird-virt::Connection passes `self.uri.ssh_target()` here,
        // but for destroy-cluster the operator-supplied `--kvm-host`
        // (or the on-host empty case) is the authoritative target.
        match self.inner.run(command) {
            Ok(out) => Ok(out.stdout_lossy()),
            Err(hbird_ssh::Error::NonZeroExit {
                host,
                status,
                stderr,
                ..
            }) => Err(SshError::RemoteExit {
                host,
                command: command.to_string(),
                exit_code: status.code(),
                stderr,
            }),
            Err(e) => Err(SshError::Transport {
                host: self.inner.options().host().to_string(),
                message: e.to_string(),
            }),
        }
    }
}

// ---- run entrypoint --------------------------------------------------------

/// Dispatch entrypoint invoked by `main.rs`.
#[tracing::instrument(level = "debug", skip(args), fields(config = ?args.config, kvm_host = ?args.kvm_host, dry_run = args.dry_run), err(Debug))]
pub fn run(args: DestroyClusterArgs) -> Result<()> {
    let config = hbird_config::parse(&args.config).map_err(|e| anyhow!("{e}"))?;
    let plan = Plan::from_args(&args, config);

    if plan.dry_run {
        run_dry_run(&plan);
        return Ok(());
    }

    // Live mode requires a KVM host alias to SSH to. Empty == operator
    // is on the KVM host directly; that's a configuration we can't yet
    // honor in the Rust path because hbird-ssh::Client always shells
    // out to `ssh`. Surface a clear diagnostic; bash twin handles this
    // by simply not having a re-exec shim trigger.
    let kvm_host = plan.kvm_host.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
        anyhow!(
            "live-mode destroy-cluster requires --kvm-host (or KVM_HOST env / config) to be set. \
             Local libvirt access without SSH is not yet wired in the Rust path; \
             on the KVM host itself, run `bash scripts/destroy-cluster.sh CONFIG` directly. \
             Use --dry-run to preview the plan from the operator's workstation."
        )
    })?;

    // Build the SSH options + QemuSshUri. We tunnel virsh through
    // qemu+ssh://<host>/system to mirror the bash twin's
    // `virsh -c qemu:///system` invoked via the ssh-wrap re-exec.
    let ssh_options = SshOptions::new(kvm_host.to_string());
    let uri = QemuSshUri {
        user: None,
        host: kvm_host.to_string(),
        port: None,
        instance: Instance::System,
        query: None,
    };
    let bridge: Arc<dyn SshClient> = Arc::new(CliSshBridge::new(ssh_options));
    let conn = Connection::new(uri, bridge);

    log(&format!("config: {}", plan.config_path.display()));
    log(&format!(
        "tearing down cluster defined in {}",
        plan.config_path.display(),
    ));

    let mut failed: Vec<(String, VirtError)> = Vec::new();
    for vm in plan.vms() {
        if let Err(e) = destroy_one(&conn, &plan, &vm) {
            // destroy_one returns Ok in all artifact-cleanup paths; an
            // Err here is a transport-level failure we couldn't recover
            // from. Surface as WARN and continue (bash twin keeps going
            // through every VM even if one fails).
            log_warn(&format!("WARN: destroy failed for {vm}: {e}"));
            if let Some(ve) = e.downcast_ref::<VirtError>() {
                failed.push((vm.clone(), clone_virt_err(ve)));
            }
        }
    }

    cleanup_scratch_dir(&conn, &plan)?;

    log("done.");

    if !failed.is_empty() {
        // Aggregate exit code 3 (mirrors update-cluster's partial-failure
        // pattern). The bash twin doesn't distinguish; we surface it.
        bail!(
            "destroy-cluster: {} domain(s) failed to fully tear down — see WARN log lines above",
            failed.len(),
        );
    }

    Ok(())
}

/// Best-effort clone of a [`VirtError`] for aggregation. The error type
/// is `non_exhaustive` + carries a non-Clone inner SSH error in some
/// variants, so a flat string-coercion preserves the operator-relevant
/// info without needing the upstream crate to add a `Clone` derive.
fn clone_virt_err(e: &VirtError) -> VirtError {
    // We can't actually clone VirtError without upstream changes; fold
    // into VirshFailed with the Display string. Accepted lossy form;
    // operators read WARN lines, not this aggregate.
    VirtError::VirshFailed {
        command: "<aggregated>".to_string(),
        stderr: format!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbird_config::parse_str;

    fn cfg() -> ClusterConfig {
        parse_str(
            "CP_NAME=hbird-cp1\n\
             SSH_PUBKEY_FILE=/k\n\
             WORKER_NAMES=(hbird-w1 hbird-w2)\n\
             POOL_DIR=/mnt/pool\n",
        )
        .expect("test cfg parses")
    }

    fn args(dry_run: bool) -> DestroyClusterArgs {
        DestroyClusterArgs {
            config: PathBuf::from("/dev/null"),
            kvm_host: Some("geary".to_string()),
            no_sudo: false,
            dry_run,
        }
    }

    #[test]
    fn plan_vms_lists_cp_then_workers_in_order() {
        let p = Plan::from_args(&args(true), cfg());
        assert_eq!(p.vms(), vec!["hbird-cp1", "hbird-w1", "hbird-w2"]);
    }

    #[test]
    fn plan_artifact_paths_match_bash_twin_layout() {
        let p = Plan::from_args(&args(true), cfg());
        assert_eq!(
            p.artifact_paths("hbird-cp1"),
            [
                "/mnt/pool/hbird-cp1.qcow2".to_string(),
                "/mnt/pool/hbird-cp1-seed.iso".to_string(),
                "/mnt/pool/hbird-cp1-cloud-init.iso".to_string(),
            ]
        );
    }

    #[test]
    fn plan_scratch_dir_under_pool_dir() {
        let p = Plan::from_args(&args(true), cfg());
        assert_eq!(p.scratch_dir(), "/mnt/pool/deploy-cluster");
    }

    #[test]
    fn empty_workers_yields_cp_only_destroy() {
        let cfg = parse_str(
            "CP_NAME=hbird-cp1\nSSH_PUBKEY_FILE=/k\nWORKER_NAMES=()\nPOOL_DIR=/mnt/pool\n",
        )
        .expect("parse");
        let p = Plan::from_args(&args(true), cfg);
        assert_eq!(p.vms(), vec!["hbird-cp1"]);
    }
}
