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
//! returns `Err(live_mode_not_implemented)` pointing at [#335]: the
//! libguestfs/`guestfish` injection step needs a Rust binding decision
//! that's outside Phase 4's scope. Operators continue running
//! `make spawn-workers COUNT=N` for actual spawns until #335 lands.
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
    #[allow(dead_code)] // consumed by live-execution slice (#335).
    kvm_host: Option<String>,
    #[allow(dead_code)] // consumed by live-execution slice (#335).
    no_sudo: bool,
    dry_run: bool,
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
            // Bash twin hardcodes TOKEN_TTL=2h default; config doesn't
            // surface this knob. Keep parity.
            token_ttl: "2h".to_string(),
            kvm_host: args.kvm_host.clone().or(config.kvm_host),
            no_sudo: args.no_sudo,
            dry_run: args.dry_run,
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

// ---- Block #2: CP IP resolve ------------------------------------------------

fn plan_cp_ip_resolve(plan: &Plan) -> Result<String> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would resolve CP IP via 'virsh domifaddr {}'",
            plan.cp_name,
        ));
        return Ok("<resolved-at-runtime>".to_string());
    }
    Err(live_mode_not_implemented(
        "plan_cp_ip_resolve",
        &format!("virsh -c qemu:///system domifaddr {}", plan.cp_name),
    ))
}

// ---- Block #3: injector detection ------------------------------------------

fn plan_injector_detect(plan: &Plan) -> Result<()> {
    if plan.dry_run {
        log(
            "DRY-RUN would probe for guestfish (preferred for bootc/ostree) or virt-customize (fallback) on the KVM host",
        );
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "plan_injector_detect",
        "command -v guestfish || command -v virt-customize (auto-install via dnf if missing)",
    ))
}

// ---- Block #4+5: per-worker mint + inject + virt-install --------------------

fn plan_worker_loop(plan: &Plan, cp_ip: &str) -> Result<()> {
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
            return Err(live_mode_not_implemented(
                "plan_worker_loop",
                "mint_join_command + inject_join_env (guestfish) + virt-install --import",
            ));
        }
    }
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would virsh pool-refresh after spawning {} worker(s)",
            plan.count,
        ));
    }
    Ok(())
}

// ---- Block #6: bootc switch-to-ghcr -----------------------------------------

fn plan_bootc_switch(plan: &Plan) -> Result<()> {
    if plan.dry_run {
        for i in 1..=plan.count {
            let name = plan.worker_name(i);
            log(&format!(
                "DRY-RUN would run 'scripts/switch-to-ghcr.sh {name} ghcr.io/aatchison/hummingbird-k8s-worker:latest' (best-effort per worker)"
            ));
        }
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "plan_bootc_switch",
        "bash scripts/switch-to-ghcr.sh <worker> ghcr.io/.../hummingbird-k8s-worker:latest",
    ))
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

    let cp_ip = plan_cp_ip_resolve(&plan)?;
    plan_injector_detect(&plan)?;
    plan_worker_loop(&plan, &cp_ip)?;
    plan_bootc_switch(&plan)?;

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

fn live_mode_not_implemented(helper: &str, equivalent: &str) -> anyhow::Error {
    anyhow!(
        "live-mode spawn-workers: `{helper}` requires a remote libvirt / guestfish / SSH \
         round-trip that is not yet implemented in the Rust path. Bash equivalent: `{equivalent}`. \
         Until the live-execution slice lands (tracked by #335), run with `--dry-run` to preview \
         the plan, or use `make spawn-workers COUNT=N` to actually spawn workers."
    )
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

    #[test]
    fn live_mode_error_names_issue_and_bash_equivalent() {
        let e = live_mode_not_implemented("plan_x", "guestfish ...");
        let s = format!("{e}");
        assert!(s.contains("#335"));
        assert!(s.contains("plan_x"));
        assert!(s.contains("guestfish"));
    }
}
