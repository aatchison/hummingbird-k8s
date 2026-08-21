//! `hbird spawn-workers` — bash twin: `scripts/spawn-workers.sh`.
//!
//! Phase 4 of the operator-CLI Rust rewrite (epic [#279], implementation
//! tracked by [#289]). The bash twin (300 LOC) clones the worker
//! template qcow2 into N copies, mints a fresh short-TTL kubeadm join
//! token per VM from the live CP, injects it into the qcow2 at
//! `/etc/hummingbird/worker-join.env` via `guestfish` (bootc/ostree
//! workaround), then virt-installs each.
//!
//! # Scope
//!
//! Dry-run path is implemented (planner output pinned by fixture
//! `tests/update_cluster/fixtures/dry_run_spawn.txt`). Live execution
//! is implemented in S3: the live path drives `guestfish`/`virt-customize`
//! injection via `conn.exec_shell()`, with bootc/ostree path discovery.
//! Tracked by [#289] S3.
//!
//! # Block traceability
//!
//! Each `// ---- <name> ----` header matches a section of
//! `scripts/spawn-workers.sh`:
//!
//! 1. Config + arg loading       → [`SpawnWorkersArgs`] + [`Plan::from_args`]
//! 2. CP IP resolve              → [`plan_cp_ip_resolve`]
//! 3. Injector detection         → [`plan_injector_detect`]
//! 4. Per-worker mint + inject   → [`plan_worker_loop`]
//! 5. virt-install per worker    → [`plan_worker_loop`]
//! 6. bootc-switch-to-ghcr       → [`plan_bootc_switch`]
//!
//! [#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
//! [#289]: https://github.com/aatchison/hummingbird-k8s/issues/289
//! [#335]: https://github.com/aatchison/hummingbird-k8s/issues/335

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;
use clap::builder::BoolishValueParser;

use hbird_config::ClusterConfig;
use hbird_virt::Connection;

use crate::commands::deploy_cluster::{cp_ssh_cmd, derive_privkey_path, sh_quote};
use crate::virt_bridge::build_connection;

// ---- Arguments (block #1: clap surface) ------------------------------------

/// Arguments for `hbird spawn-workers`.
///
/// Mirrors the bash twin: `scripts/spawn-workers.sh [count]` consults
/// `CONFIG=<path>` (env), `KVM_HOST` (env), `CP_NAME`, `POOL_DIR`,
/// `WORKER_MEMORY`, `WORKER_VCPUS`, `TOKEN_TTL` from the config.
#[derive(Debug, Args)]
pub struct SpawnWorkersArgs {
    /// Path to `cluster.local.conf`. Required (the bash twin sources it
    /// to read `CP_NAME` / `POOL_DIR` / `WORKER_MEMORY` / `WORKER_VCPUS`).
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,

    /// Number of workers to spawn. Bash twin's positional arg.
    #[arg(long, default_value_t = 2, value_name = "N")]
    pub count: u32,

    /// SSH alias of the KVM host. Overrides `KVM_HOST` env / config.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// Skip the `sudo` probe on the KVM host (libvirt-group operator
    /// path, #305).
    #[arg(
        long,
        env = "HBIRD_REMOTE_NO_SUDO",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub no_sudo: bool,

    /// Plan-only mode — print the spawn plan without invoking
    /// libvirt / guestfish / virt-install. (#289.)
    #[arg(long)]
    pub dry_run: bool,

    /// CP SSH retry count for join-token mint. Env: CP_SSH_RETRIES. Default: 5.
    #[arg(long, env = "CP_SSH_RETRIES", default_value = "5")]
    pub cp_ssh_retries: u32,

    /// Sleep seconds between CP SSH retries. Env: CP_SSH_RETRY_SLEEP. Default: 10.
    #[arg(long, env = "CP_SSH_RETRY_SLEEP", default_value = "10")]
    pub cp_ssh_retry_sleep_secs: u64,

    /// kubeadm join-token TTL. Env: TOKEN_TTL. Default: "2h".
    #[arg(long, env = "TOKEN_TTL", default_value = "2h")]
    pub token_ttl: String,

    /// Skip post-spawn bootc switch if worker was built locally. Env: FORCE_REBUILD.
    #[arg(
        long,
        env = "FORCE_REBUILD",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub force_rebuild: bool,

    /// Override FORCE_REBUILD skip for bootc switch. Env: FORCE_SWITCH.
    #[arg(
        long,
        env = "FORCE_SWITCH",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub force_switch: bool,
}

// ---- Logger ----------------------------------------------------------------

fn log(line: &str) {
    println!("[spawn-workers] {line}");
}

// ---- Plan -----------------------------------------------------------------

#[derive(Debug, Clone)]
struct Plan {
    config_path: PathBuf,
    cp_name: String,
    pool_dir: String,
    count: u32,
    worker_memory: u32,
    worker_vcpus: u32,
    token_ttl: String,
    kvm_host: Option<String>,
    #[allow(dead_code)] // consumed by live-execution helpers when no_sudo plumbing is added.
    no_sudo: bool,
    dry_run: bool,
    cp_ssh_retries: u32,
    cp_ssh_retry_sleep_secs: u64,
    force_rebuild: bool,
    force_switch: bool,
    ssh_pubkey_file: String,
}

impl Plan {
    fn from_args(args: &SpawnWorkersArgs, config: ClusterConfig) -> Self {
        Self {
            config_path: args.config.clone(),
            cp_name: config.cp_name,
            pool_dir: config.pool_dir,
            count: args.count,
            worker_memory: config.worker_memory,
            worker_vcpus: config.worker_vcpus,
            token_ttl: args.token_ttl.clone(),
            kvm_host: args.kvm_host.clone().or(config.kvm_host),
            no_sudo: args.no_sudo,
            dry_run: args.dry_run,
            cp_ssh_retries: args.cp_ssh_retries,
            cp_ssh_retry_sleep_secs: args.cp_ssh_retry_sleep_secs,
            force_rebuild: args.force_rebuild,
            force_switch: args.force_switch,
            ssh_pubkey_file: config.ssh_pubkey_file,
        }
    }

    /// Resolve the per-worker name. Mirrors bash twin line 242:
    /// `NAME="hummingbird-k8s-worker-${i}"` (1-indexed).
    fn worker_name(&self, i: u32) -> String {
        format!("hummingbird-k8s-worker-{i}")
    }

    /// Path to the worker template qcow2. Mirrors bash twin line 84.
    fn template_path(&self) -> String {
        format!("{}/hummingbird-k8s-worker.qcow2", self.pool_dir)
    }
}

// ---- Injector enum ---------------------------------------------------------

/// Which tool is available on the KVM host for qcow2 injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Injector {
    Guestfish,
    VirtCustomize,
}

// ---- Block #2: CP IP resolve ------------------------------------------------

fn plan_cp_ip_resolve(plan: &Plan, conn: &Connection) -> Result<String> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would resolve CP IP via 'virsh domifaddr {}'",
            plan.cp_name,
        ));
        return Ok("<resolved-at-runtime>".to_string());
    }

    // Live path: poll domifaddr until CP gets an IP.
    log(&format!(
        "waiting for CP IP via virsh domifaddr {}...",
        plan.cp_name
    ));
    let ip_cell = std::cell::Cell::new(None::<std::net::Ipv4Addr>);
    let found =
        hbird_virt::poll::retry(
            plan.cp_ssh_retries,
            plan.cp_ssh_retry_sleep_secs,
            || match conn.domifaddr(&plan.cp_name) {
                Ok(Some(ip)) => {
                    ip_cell.set(Some(ip));
                    Ok(true)
                }
                Ok(None) => Ok(false),
                Err(e) => {
                    tracing::debug!("domifaddr probe: {e}");
                    Ok(false)
                }
            },
        )
        .map_err(|e: anyhow::Error| e)?;

    if !found {
        return Err(anyhow!(
            "could not resolve CP IP via virsh domifaddr after {} retries",
            plan.cp_ssh_retries
        ));
    }
    let ip = ip_cell.get().unwrap();
    log(&format!("CP IP: {ip}"));
    Ok(ip.to_string())
}

// ---- Block #3: injector detection ------------------------------------------

fn plan_injector_detect(plan: &Plan, conn: &Connection) -> Result<Injector> {
    if plan.dry_run {
        log(
            "DRY-RUN would probe for guestfish (preferred for bootc/ostree) or virt-customize (fallback) on the KVM host",
        );
        // Return a placeholder; dry-run never uses the value.
        return Ok(Injector::Guestfish);
    }

    // Probe for available injector.
    let probe_result = conn
        .exec_shell(
            "command -v guestfish && echo guestfish || (command -v virt-customize && echo virt-customize) || echo none",
        )
        .unwrap_or_default();

    let probe = probe_result.trim();
    if probe.contains("guestfish") {
        return Ok(Injector::Guestfish);
    }
    if probe.contains("virt-customize") {
        return Ok(Injector::VirtCustomize);
    }

    // Try auto-install via dnf (best-effort).
    log("guestfish/virt-customize not found — attempting dnf install...");
    let _ = conn.exec_shell(
        "command -v dnf >/dev/null 2>&1 && \
         (dnf install -y libguestfs-tools-c >/dev/null 2>&1 || \
          dnf install -y libguestfs-tools >/dev/null 2>&1) || true",
    );

    let probe2 = conn
        .exec_shell(
            "command -v guestfish && echo guestfish || (command -v virt-customize && echo virt-customize) || echo none",
        )
        .unwrap_or_default();
    let probe2 = probe2.trim();

    if probe2.contains("guestfish") {
        return Ok(Injector::Guestfish);
    }
    if probe2.contains("virt-customize") {
        return Ok(Injector::VirtCustomize);
    }

    Err(anyhow!(
        "guestfish/virt-customize unavailable and auto-install failed; \
         install libguestfs-tools-c (or libguestfs-tools) on the KVM host"
    ))
}

// ---- Block #4+5: per-worker mint + inject + virt-install --------------------

fn plan_worker_loop(
    plan: &Plan,
    conn: &Connection,
    cp_ip: &str,
    privkey_path: &str,
    injector: Injector,
) -> Result<()> {
    let template = plan.template_path();
    log(&format!(
        "DRY-RUN worker template qcow2: {template} (must exist; bash twin fails fast when missing)"
    ));
    for i in 1..=plan.count {
        let name = plan.worker_name(i);
        let qcow = format!("{}/{name}.qcow2", plan.pool_dir);
        if plan.dry_run {
            log(&format!(
                "DRY-RUN worker {i}/{}: would skip {name} if 'virsh dominfo {name}' succeeds (re-uses existing VM via 'virsh start')",
                plan.count,
            ));
            log(&format!(
                "DRY-RUN worker {i}/{}: would clone {template} -> {qcow} (reflink=auto), chmod 0644",
                plan.count,
            ));
            log(&format!(
                "DRY-RUN worker {i}/{}: would mint {}-TTL kubeadm join token via 'ssh root@{cp_ip} kubeadm token create --print-join-command' (with retry)",
                plan.count, plan.token_ttl,
            ));
            log(&format!(
                "DRY-RUN worker {i}/{}: would inject join command into {qcow} at /etc/hummingbird/worker-join.env via guestfish (bootc-aware: discovers /ostree/deploy/<stateroot>/deploy/<commit>.0)",
                plan.count,
            ));
            log(&format!(
                "DRY-RUN worker {i}/{}: would virt-install {name} (memory={} vcpus={}) attaching {qcow}",
                plan.count, plan.worker_memory, plan.worker_vcpus,
            ));
        } else {
            // Live path.

            // Check if VM already exists.
            match conn.dominfo(&name) {
                Ok(_) => {
                    // Already defined — start it and move on.
                    log(&format!(
                        "worker {i}/{}: {name} already defined — starting",
                        plan.count
                    ));
                    if let Err(e) = conn.start_domain(&name) {
                        log(&format!(
                            "WARN: virsh start {name} failed (may already be running): {e}"
                        ));
                    }
                    continue;
                }
                Err(hbird_virt::Error::VirshFailed { .. }) => {
                    // Not defined — proceed to create.
                }
                Err(e) => return Err(anyhow!("dominfo probe for {name}: {e}")),
            }

            // Clone template.
            log(&format!(
                "worker {i}/{}: cloning {template} -> {qcow}",
                plan.count
            ));
            conn.remote_cp_reflink(&template, &qcow)
                .map_err(|e| anyhow!("reflink clone {template} -> {qcow} failed: {e}"))?;

            // Mint join token with retry.
            log(&format!(
                "worker {i}/{}: minting {}-TTL kubeadm join token from CP",
                plan.count, plan.token_ttl
            ));
            let kubeadm_cmd = format!(
                "kubeadm token create --ttl {} --print-join-command",
                plan.token_ttl
            );
            let ssh_join_cmd = cp_ssh_cmd(privkey_path, cp_ip, &kubeadm_cmd);

            let join_cell = std::cell::RefCell::new(None::<String>);
            let minted =
                hbird_virt::poll::retry(plan.cp_ssh_retries, plan.cp_ssh_retry_sleep_secs, || {
                    match conn.exec_shell(&ssh_join_cmd) {
                        Ok(out) => {
                            let trimmed = out.trim().to_string();
                            if trimmed.starts_with("kubeadm join") {
                                *join_cell.borrow_mut() = Some(trimmed);
                                Ok(true)
                            } else {
                                tracing::debug!(
                                    "kubeadm token create unexpected output: {trimmed}"
                                );
                                Ok(false)
                            }
                        }
                        Err(e) => {
                            tracing::debug!("kubeadm token create failed: {e}");
                            Ok(false)
                        }
                    }
                })
                .map_err(|e: anyhow::Error| e)?;

            let join_cmd = if minted {
                join_cell.into_inner().unwrap()
            } else {
                // Cleanup cloned qcow2 before propagating.
                let _ = conn.remote_rm_f(&qcow);
                return Err(anyhow!(
                    "could not mint kubeadm join token from CP after {} retries",
                    plan.cp_ssh_retries
                ));
            };

            // Inject join env into qcow2.
            log(&format!(
                "worker {i}/{}: injecting join env into {qcow}",
                plan.count
            ));
            if let Err(e) = inject_join_env(conn, &qcow, &join_cmd, injector) {
                let _ = conn.remote_rm_f(&qcow);
                return Err(anyhow!("inject_join_env for {name} failed: {e}"));
            }

            // virt-install (no seed ISO — spawn-workers uses guestfish injection).
            log(&format!(
                "worker {i}/{}: virt-install {name} (memory={} vcpus={})",
                plan.count, plan.worker_memory, plan.worker_vcpus
            ));
            conn.virt_install(
                &name,
                plan.worker_memory as u64,
                plan.worker_vcpus,
                &qcow,
                None,
            )
            .map_err(|e| anyhow!("virt-install {name} failed: {e}"))?;
        }
    }

    if plan.dry_run {
        log(&format!(
            "DRY-RUN would virsh pool-refresh after spawning {} worker(s)",
            plan.count,
        ));
    } else {
        // Pool refresh (best-effort — mirrors bash twin `|| true`).
        if let Err(e) = conn.virsh_pool_refresh("mass2") {
            log(&format!("WARN: virsh pool-refresh mass2 failed: {e}"));
        }
    }
    Ok(())
}

// ---- Block #6: bootc switch-to-ghcr -----------------------------------------

fn plan_bootc_switch(plan: &Plan, conn: &Connection, privkey_path: &str) -> Result<()> {
    // Round-2 lens L2 MEDIUM: honor BOOTC_SWITCH_TO_GHCR=0 to skip
    // (bash twin `spawn-workers.sh:295`). Operators set this when their
    // workers are already on the GHCR image and the re-switch is
    // unnecessary churn.
    if std::env::var("BOOTC_SWITCH_TO_GHCR").as_deref() == Ok("0") {
        log("skipped (BOOTC_SWITCH_TO_GHCR=0)");
        return Ok(());
    }
    if plan.dry_run {
        for i in 1..=plan.count {
            let name = plan.worker_name(i);
            log(&format!(
                "DRY-RUN would run 'scripts/switch-to-ghcr.sh {name} ghcr.io/aatchison/hummingbird-k8s-worker:latest' (best-effort per worker)"
            ));
        }
        return Ok(());
    }

    // #375 guard: FORCE_REBUILD=1 without FORCE_SWITCH=1 → skip bootc switch.
    if plan.force_rebuild && !plan.force_switch {
        log("WARN: FORCE_REBUILD=1 — skipping post-spawn bootc switch (#375)");
        return Ok(());
    }

    for i in 1..=plan.count {
        let name = plan.worker_name(i);
        // Best-effort: single domifaddr attempt.
        let ip_opt = conn.domifaddr(&name).ok().flatten();
        if let Some(ip) = ip_opt {
            let switch_cmd = cp_ssh_cmd(
                privkey_path,
                &ip.to_string(),
                "bootc switch ghcr.io/aatchison/hummingbird-k8s-worker:latest",
            );
            if let Err(e) = conn.exec_shell(&switch_cmd) {
                log(&format!(
                    "WARN: bootc switch failed for {name}: {e} (VM still tracks localhost:latest)"
                ));
            }
        } else {
            log(&format!(
                "WARN: could not resolve IP for {name}; skipping bootc switch"
            ));
        }
    }

    Ok(())
}

// ---- run entrypoint --------------------------------------------------------

#[tracing::instrument(level = "debug", skip(args), fields(config = ?args.config, count = args.count, dry_run = args.dry_run), err(Debug))]
pub fn run(args: SpawnWorkersArgs) -> Result<()> {
    if args.count == 0 {
        return Err(anyhow!(
            "--count must be > 0 (got {}); bash twin defaults to 2",
            args.count,
        ));
    }
    let config = hbird_config::parse(&args.config).map_err(|e| anyhow!("{e}"))?;
    let plan = Plan::from_args(&args, config);

    log(&format!("config: {}", plan.config_path.display()));
    log(&format!(
        "config OK: CP={}, count={}, pool_dir={}",
        plan.cp_name, plan.count, plan.pool_dir,
    ));

    let conn = build_connection(plan.kvm_host.as_deref());
    let privkey_path = derive_privkey_path(&plan.ssh_pubkey_file);

    let cp_ip = plan_cp_ip_resolve(&plan, &conn)?;
    let injector = plan_injector_detect(&plan, &conn)?;
    plan_worker_loop(&plan, &conn, &cp_ip, &privkey_path, injector)?;
    plan_bootc_switch(&plan, &conn, &privkey_path)?;

    if plan.dry_run {
        log("");
        log("==============================================================");
        log("DRY-RUN plan complete. No VMs were created.");
        log(&format!("  CP:        {} ({cp_ip})", plan.cp_name));
        log(&format!("  Count:     {} worker(s) to spawn", plan.count));
        log(&format!(
            "  Worker:    memory={} vcpus={}",
            plan.worker_memory, plan.worker_vcpus,
        ));
        log("==============================================================");
    }
    Ok(())
}

// ---- Guestfish / virt-customize command builders (pure, unit-testable) ------

/// Build the guestfish discovery command for the ostree stateroot.
pub(crate) fn guestfish_ls_stateroot_cmd(qcow: &str) -> String {
    format!(
        "guestfish --ro -a {} run : mount /dev/sda4 / : ls /ostree/deploy 2>/dev/null | grep -v '^$' | head -1",
        sh_quote(qcow)
    )
}

/// Build the guestfish discovery command for the ostree deploy hash.
pub(crate) fn guestfish_ls_deploy_cmd(qcow: &str, stateroot: &str) -> String {
    format!(
        "guestfish --ro -a {} run : mount /dev/sda4 / : ls /ostree/deploy/{}/deploy 2>/dev/null | grep -v '\\.origin$' | grep -v '^$' | head -1",
        sh_quote(qcow),
        sh_quote(stateroot)
    )
}

/// Build the guestfish write command (heredoc format) for injecting worker-join.env.
pub(crate) fn guestfish_inject_cmd(qcow: &str, etc_path: &str, tmpfile: &str) -> String {
    format!(
        "guestfish --rw -a {} <<'GUESTFISH_HBIRD_EOF'\nrun\nmount /dev/sda4 /\nmkdir-p {}\nupload {} {}/worker-join.env\nchmod 0600 {}/worker-join.env\nchown 0 0 {}/worker-join.env\nGUESTFISH_HBIRD_EOF",
        sh_quote(qcow),
        etc_path,
        sh_quote(tmpfile),
        etc_path,
        etc_path,
        etc_path
    )
}

/// Build the virt-customize inject command (fallback for non-bootc).
pub(crate) fn virt_customize_inject_cmd(qcow: &str, tmpfile: &str) -> String {
    format!(
        "virt-customize -a {} --mkdir /etc/hummingbird --upload '{}:/etc/hummingbird/worker-join.env' --run-command 'chmod 0600 /etc/hummingbird/worker-join.env' --run-command 'chown root:root /etc/hummingbird/worker-join.env'",
        sh_quote(qcow),
        tmpfile
    )
}

/// Build the command to write the join command to a tmpfile on the KVM host.
pub(crate) fn write_join_tmp_cmd(join_cmd: &str, tmpfile: &str) -> String {
    format!(
        "printf '%s\\n' {} > {}",
        sh_quote(join_cmd),
        sh_quote(tmpfile)
    )
}

// ---- inject_join_env --------------------------------------------------------

/// Inject the kubeadm join command into the qcow2 at the correct
/// bootc/ostree path (`/ostree/deploy/<stateroot>/deploy/<hash>/etc/hummingbird/`)
/// or `/etc/hummingbird/` for non-bootc images (fallback).
///
/// Mirrors `scripts/spawn-workers.sh::inject_join_env`.
fn inject_join_env(
    conn: &Connection,
    qcow: &str,
    join_cmd: &str,
    injector: Injector,
) -> Result<()> {
    let tmpfile = format!(
        "/tmp/hbird-join-{}-{}.env",
        std::process::id(),
        std::path::Path::new(qcow)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("worker")
    );

    // Write join cmd to tmpfile on the KVM host.
    conn.exec_shell(&write_join_tmp_cmd(join_cmd, &tmpfile))
        .map_err(|e| anyhow!("could not write join tmpfile {tmpfile}: {e}"))?;

    let result = match injector {
        Injector::Guestfish => {
            // Discover ostree layout.
            let stateroot = conn
                .exec_shell(&guestfish_ls_stateroot_cmd(qcow))
                .unwrap_or_default()
                .trim()
                .to_string();

            let deploy_hash = if !stateroot.is_empty() {
                conn.exec_shell(&guestfish_ls_deploy_cmd(qcow, &stateroot))
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            } else {
                String::new()
            };

            let etc_path = if !stateroot.is_empty() && !deploy_hash.is_empty() {
                format!("/ostree/deploy/{stateroot}/deploy/{deploy_hash}/etc/hummingbird")
            } else {
                "/etc/hummingbird".to_string()
            };

            conn.exec_shell(&guestfish_inject_cmd(qcow, &etc_path, &tmpfile))
                .map_err(|e| anyhow!("guestfish inject failed: {e}"))
        }
        Injector::VirtCustomize => conn
            .exec_shell(&virt_customize_inject_cmd(qcow, &tmpfile))
            .map_err(|e| anyhow!("virt-customize inject failed: {e}")),
    };

    // Cleanup tmpfile (best-effort).
    let _ = conn.exec_shell(&format!("rm -f -- {}", sh_quote(&tmpfile)));

    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbird_config::parse_str;

    fn args(dry_run: bool, count: u32) -> SpawnWorkersArgs {
        SpawnWorkersArgs {
            config: PathBuf::from("/dev/null"),
            count,
            kvm_host: None,
            no_sudo: false,
            dry_run,
            cp_ssh_retries: 5,
            cp_ssh_retry_sleep_secs: 10,
            token_ttl: "2h".to_string(),
            force_rebuild: false,
            force_switch: false,
        }
    }

    fn cfg() -> ClusterConfig {
        parse_str(
            "CP_NAME=hbird-cp1\n\
             SSH_PUBKEY_FILE=/k\n\
             WORKER_MEMORY=8192\n\
             WORKER_VCPUS=4\n\
             POOL_DIR=/mnt/pool\n",
        )
        .expect("parse")
    }

    #[test]
    fn worker_names_match_bash_twin() {
        let p = Plan::from_args(&args(true, 3), cfg());
        assert_eq!(p.worker_name(1), "hummingbird-k8s-worker-1");
        assert_eq!(p.worker_name(2), "hummingbird-k8s-worker-2");
        assert_eq!(p.worker_name(3), "hummingbird-k8s-worker-3");
    }

    #[test]
    fn template_path_under_pool_dir() {
        let p = Plan::from_args(&args(true, 1), cfg());
        assert_eq!(p.template_path(), "/mnt/pool/hummingbird-k8s-worker.qcow2");
    }

    // ---- guestfish command shape tests ----------------------------------------

    #[test]
    fn guestfish_ls_stateroot_cmd_shape() {
        let cmd = guestfish_ls_stateroot_cmd("/mnt/pool/hummingbird-k8s-worker-1.qcow2");
        assert!(cmd.contains("guestfish --ro"), "must use --ro: {cmd}");
        assert!(
            cmd.contains("/mnt/pool/hummingbird-k8s-worker-1.qcow2"),
            "must reference qcow: {cmd}"
        );
        assert!(
            cmd.contains("mount /dev/sda4 /"),
            "must mount partition: {cmd}"
        );
        assert!(
            cmd.contains("ls /ostree/deploy"),
            "must list ostree deploy: {cmd}"
        );
        assert!(cmd.contains("head -1"), "must take first result: {cmd}");
    }

    #[test]
    fn guestfish_ls_deploy_cmd_shape() {
        let cmd = guestfish_ls_deploy_cmd("/mnt/pool/worker-1.qcow2", "hummingbird-k8s");
        assert!(cmd.contains("guestfish --ro"), "must use --ro: {cmd}");
        assert!(
            cmd.contains("/ostree/deploy/hummingbird-k8s/deploy"),
            "must interpolate stateroot: {cmd}"
        );
        assert!(
            cmd.contains("grep -v '\\.origin$'"),
            "must filter .origin: {cmd}"
        );
        assert!(cmd.contains("head -1"), "must take first result: {cmd}");
    }

    #[test]
    fn guestfish_inject_cmd_shape() {
        let cmd = guestfish_inject_cmd(
            "/mnt/pool/worker-1.qcow2",
            "/ostree/deploy/hbird/deploy/abc.0/etc/hummingbird",
            "/tmp/hbird-join-42-worker-1.env",
        );
        assert!(cmd.contains("guestfish --rw"), "must use --rw: {cmd}");
        assert!(
            cmd.contains("<<'GUESTFISH_HBIRD_EOF'"),
            "must use heredoc marker: {cmd}"
        );
        assert!(cmd.contains("run\n"), "must have run directive: {cmd}");
        assert!(
            cmd.contains("mount /dev/sda4 /"),
            "must mount partition: {cmd}"
        );
        assert!(cmd.contains("mkdir-p"), "must mkdir-p: {cmd}");
        assert!(cmd.contains("upload"), "must upload: {cmd}");
        assert!(
            cmd.contains("worker-join.env"),
            "must reference worker-join.env: {cmd}"
        );
        assert!(cmd.contains("chmod 0600"), "must chmod 0600: {cmd}");
        assert!(cmd.contains("chown 0 0"), "must chown 0 0: {cmd}");
        assert!(
            cmd.contains("GUESTFISH_HBIRD_EOF"),
            "must close heredoc: {cmd}"
        );
    }

    #[test]
    fn virt_customize_inject_cmd_shape() {
        let cmd = virt_customize_inject_cmd(
            "/mnt/pool/worker-1.qcow2",
            "/tmp/hbird-join-42-worker-1.env",
        );
        assert!(
            cmd.contains("virt-customize"),
            "must use virt-customize: {cmd}"
        );
        assert!(
            cmd.contains("-a /mnt/pool/worker-1.qcow2")
                || cmd.contains("-a '/mnt/pool/worker-1.qcow2'"),
            "must specify qcow with -a: {cmd}"
        );
        assert!(
            cmd.contains("--mkdir /etc/hummingbird"),
            "must mkdir: {cmd}"
        );
        assert!(cmd.contains("--upload"), "must upload: {cmd}");
        assert!(
            cmd.contains("worker-join.env"),
            "must reference worker-join.env: {cmd}"
        );
        assert!(cmd.contains("chmod 0600"), "must chmod 0600: {cmd}");
        assert!(
            cmd.contains("chown root:root"),
            "must chown root:root: {cmd}"
        );
    }

    #[test]
    fn write_join_tmp_cmd_quotes_join_cmd() {
        let join_cmd = "kubeadm join 192.168.122.10:6443 --token abc.def --discovery-token-ca-cert-hash sha256:deadbeef";
        let tmpfile = "/tmp/hbird-join-42-worker-1.env";
        let cmd = write_join_tmp_cmd(join_cmd, tmpfile);
        assert!(cmd.starts_with("printf '%s\\n'"), "must use printf: {cmd}");
        assert!(
            cmd.contains("kubeadm join"),
            "must embed join command: {cmd}"
        );
        assert!(
            cmd.contains("/tmp/hbird-join-42-worker-1.env"),
            "must reference tmpfile: {cmd}"
        );
    }
}
