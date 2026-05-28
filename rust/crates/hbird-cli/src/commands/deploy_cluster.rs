//! `hbird deploy-cluster` — bash twin: `scripts/deploy-cluster.sh`.
//!
//! Phase 4 of the operator-CLI Rust rewrite (epic [#279], implementation
//! tracked by [#289]). The bash twin is a 649-line orchestrator that
//! pulls images, builds qcow2 templates via bib, spawns CP + workers,
//! and waits for kubeadm join + Ready.
//!
//! # Scope of this module
//!
//! The dry-run path is implemented: it emits a deterministic plan
//! describing every step the live execution would take (image pull,
//! qcow2 build, cloud-init seed, virt-install, CP IP probe, kubeadm
//! token mint, per-worker spawn, Ready poll, summary). Fixtures under
//! `tests/update_cluster/fixtures/dry_run_deploy_*.txt` pin the output.
//!
//! The live execution path remains an `Err(live_mode_not_implemented)`
//! pointing at the [#335] follow-up issue — the bash twin invokes
//! bootc-image-builder (rootful-podman-only, see [#311]), virt-install,
//! cloud-init seed generation, qcow2 cloning, and SSH-based kubeadm
//! token minting; rust-native parity is multiple hundred LOC of new
//! code plus a libvirt/bib decision that overlaps with #311 in flight.
//! Operators continue running `make deploy-cluster CONFIG=…` for
//! actual deployments until #335 lands.
//!
//! # Block traceability
//!
//! Each `// ---- <name> ----` header matches a section of
//! `scripts/deploy-cluster.sh`, so a reviewer can grep both sides:
//!
//! 1. Config + arg loading        → [`DeployClusterArgs`] + [`Plan::from_args`]
//! 2. Root + libvirt-group gate   → [`Plan::from_args`] (deferred to live)
//! 3. POOL_DIR write probe        → [`Plan::from_args`] (deferred to live)
//! 4. Image acquisition           → [`plan_image_acquisition`]
//! 5. bib qcow2 per flavor        → [`plan_build_qcow2`]
//! 6. CP cloud-init user-data     → [`plan_cp_seed`]
//! 7. CP virt-install             → [`plan_cp_virt_install`]
//! 8. CP IP discovery + Ready     → [`plan_cp_ready`]
//! 9. kubeadm join-token mint     → [`plan_join_token`]
//! 10. Per-worker seed + spawn    → [`plan_worker_spawn`]
//! 11. Cluster-Ready poll         → [`plan_cluster_ready`]
//! 12. Optional verify            → [`plan_verify`]
//! 13. Summary footer             → [`plan_summary`]
//!
//! [#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
//! [#289]: https://github.com/aatchison/hummingbird-k8s/issues/289
//! [#311]: https://github.com/aatchison/hummingbird-k8s/issues/311
//! [#335]: https://github.com/aatchison/hummingbird-k8s/issues/335

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;
use clap::builder::BoolishValueParser;

use hbird_config::ClusterConfig;

// ---- Arguments (block #1: clap surface) ------------------------------------

/// Arguments for `hbird deploy-cluster`.
///
/// Mirrors the bash twin: `scripts/deploy-cluster.sh` takes the config
/// path positionally and consults `KVM_HOST` from the environment. The
/// Rust shape promotes both to explicit flags so the operator can read
/// the invocation off the command line without checking `env`.
#[derive(Debug, Args)]
pub struct DeployClusterArgs {
    /// Path to `cluster.local.conf` (start from `cluster.example.conf`).
    ///
    /// Bash twin reads `CONFIG=<path>` (positional). Required.
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,

    /// SSH alias of the KVM host to re-exec onto. Overrides `KVM_HOST`
    /// in the env / config file.
    ///
    /// Bash twin uses the `KVM_HOST` env var via the
    /// `scripts/lib/ssh-wrap.sh` re-exec shim.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// Skip the `sudo` probe on the KVM host. Use when the operator is a
    /// member of the `libvirt` group (per #305) and the qcow2 pool dir
    /// is group-writable.
    ///
    /// Bash twin honors `HBIRD_REMOTE_NO_SUDO=1` (see
    /// `scripts/lib/ssh-wrap.sh`); the `env =` binding mirrors that, and
    /// `BoolishValueParser` accepts `1`/`0`/`yes`/`no` so the env-var
    /// path matches the bash twin's `[[ -n $HBIRD_REMOTE_NO_SUDO ]]`
    /// truthiness. (PR #319 round-2 review L2 + L5 + L9 convergent
    /// MEDIUM.)
    #[arg(
        long,
        env = "HBIRD_REMOTE_NO_SUDO",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub no_sudo: bool,

    /// Plan-only mode — print the deploy plan without invoking
    /// libvirt / bib / virt-install. Useful for confirming the config
    /// resolution and per-VM names before committing to a deploy.
    ///
    /// The bash twin has no `--dry-run` flag; this is a Rust-side
    /// addition. The plan output is pinned by fixtures under
    /// `tests/update_cluster/fixtures/dry_run_deploy_*.txt`. (#289.)
    #[arg(long)]
    pub dry_run: bool,
}

// ---- Logger ----------------------------------------------------------------

/// Emit a `[deploy-cluster]` prefixed log line to stdout. Mirrors
/// `lib/build-common.sh::log` invoked under
/// `setup_logging "[deploy-cluster]"`.
fn log(line: &str) {
    println!("[deploy-cluster] {line}");
}

// ---- Plan -----------------------------------------------------------------

/// Merged "what we're about to do" view of args + config. Built once
/// at the top of [`run`] and consumed by each `plan_*` step.
#[derive(Debug, Clone)]
struct Plan {
    config_path: PathBuf,
    cp_name: String,
    worker_names: Vec<String>,
    image_source: String,
    ghcr_tag: String,
    cp_memory: u32,
    cp_vcpus: u32,
    worker_memory: u32,
    worker_vcpus: u32,
    pool_dir: String,
    run_verify: bool,
    auto_update_cp: bool,
    switch_to_ghcr: bool,
    enable_cloud_init: u32,
    /// `KVM_HOST` SSH alias (`None` = local libvirt).
    #[allow(dead_code)] // consumed by live-execution slice (#335).
    kvm_host: Option<String>,
    #[allow(dead_code)] // consumed by live-execution slice (#335).
    no_sudo: bool,
    dry_run: bool,
}

impl Plan {
    fn from_args(args: &DeployClusterArgs, config: ClusterConfig) -> Result<Self> {
        let worker_names = config.resolved_worker_names();
        Ok(Self {
            config_path: args.config.clone(),
            cp_name: config.cp_name,
            worker_names,
            image_source: config.image_source,
            ghcr_tag: config.ghcr_tag,
            cp_memory: config.cp_memory,
            cp_vcpus: config.cp_vcpus,
            worker_memory: config.worker_memory,
            worker_vcpus: config.worker_vcpus,
            pool_dir: config.pool_dir,
            run_verify: config.run_verify,
            auto_update_cp: config.auto_update_cp,
            switch_to_ghcr: config.switch_to_ghcr,
            enable_cloud_init: config.enable_cloud_init,
            kvm_host: args.kvm_host.clone(),
            no_sudo: args.no_sudo,
            dry_run: args.dry_run,
        })
    }
}

// ---- Block #4: image acquisition -------------------------------------------

/// Plan the image-acquisition step. Mirrors `deploy-cluster.sh` line 410-437.
fn plan_image_acquisition(plan: &Plan) -> Result<(String, String)> {
    let (cp_ref, worker_ref) = match plan.image_source.as_str() {
        "ghcr" => (
            format!("ghcr.io/aatchison/hummingbird-k8s:{}", plan.ghcr_tag),
            format!("ghcr.io/aatchison/hummingbird-k8s-worker:{}", plan.ghcr_tag),
        ),
        "local" => (
            "localhost/hummingbird-k8s:latest".to_string(),
            "localhost/hummingbird-k8s-worker:latest".to_string(),
        ),
        other => {
            return Err(anyhow!(
                "IMAGE_SOURCE must be 'ghcr' or 'local' (got '{other}')"
            ));
        }
    };
    if plan.dry_run {
        match plan.image_source.as_str() {
            "ghcr" => {
                log(&format!("DRY-RUN would podman pull {cp_ref}"));
                log(&format!("DRY-RUN would podman pull {worker_ref}"));
            }
            "local" => {
                log(&format!(
                    "DRY-RUN would build local image {cp_ref} via 'make image-k8s-with-cloud-init'"
                ));
                log(&format!(
                    "DRY-RUN would build local image {worker_ref} via 'make image-worker-with-cloud-init'"
                ));
            }
            _ => unreachable!("matched above"),
        }
        return Ok((cp_ref, worker_ref));
    }
    Err(live_mode_not_implemented(
        "plan_image_acquisition",
        "podman pull / make image-* (deferred to bash twin until #335 lands)",
    ))
}

// ---- Block #5: bib qcow2 per flavor ----------------------------------------

/// Plan the bib qcow2 build step. Mirrors `deploy-cluster.sh` line 439-463.
fn plan_build_qcow2(plan: &Plan, cp_ref: &str, worker_ref: &str) -> Result<(String, String)> {
    let cp_template = format!("{}/hummingbird-k8s-deploy.qcow2", plan.pool_dir);
    let worker_template = format!("{}/hummingbird-k8s-worker-deploy.qcow2", plan.pool_dir);
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would render bib config + build {cp_template} from {cp_ref}"
        ));
        log(&format!(
            "DRY-RUN would render bib config + build {worker_template} from {worker_ref}"
        ));
        log(
            "  (bib invocation requires rootful podman — see #311; the live path will honor `FORCE_REBUILD=1` to override #311(d)'s skip-if-exists shortcut landed in PR #336)",
        );
        return Ok((cp_template, worker_template));
    }
    Err(live_mode_not_implemented(
        "plan_build_qcow2",
        "bootc-image-builder (#311 rootful-podman constraint)",
    ))
}

// ---- Block #6+7: CP cloud-init seed + virt-install --------------------------

/// Plan the CP cloud-init user-data + seed ISO step. Mirrors lines 465-478.
fn plan_cp_seed(plan: &Plan) -> Result<String> {
    let cp_seed = format!("{}/{}-seed.iso", plan.pool_dir, plan.cp_name);
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would render CP cloud-init user-data (auto-update-cp={}, switch-to-ghcr={}, ghcr-tag={})",
            plan.auto_update_cp, plan.switch_to_ghcr, plan.ghcr_tag,
        ));
        log(&format!("DRY-RUN would build CP cloud-init seed {cp_seed}"));
        return Ok(cp_seed);
    }
    Err(live_mode_not_implemented(
        "plan_cp_seed",
        "render_cp_user_data + build_cloud_init_seed",
    ))
}

/// Plan the CP virt-install step. Mirrors lines 480-508.
fn plan_cp_virt_install(plan: &Plan, cp_template: &str, cp_seed: &str) -> Result<String> {
    let cp_qcow = format!("{}/{}.qcow2", plan.pool_dir, plan.cp_name);
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would refuse to overwrite if CP VM '{}' already defined",
            plan.cp_name,
        ));
        log(&format!(
            "DRY-RUN would clone {cp_template} -> {cp_qcow} (reflink=auto)"
        ));
        log(&format!(
            "DRY-RUN would virt-install {} (memory={} vcpus={}) attaching {cp_qcow} + {cp_seed}",
            plan.cp_name, plan.cp_memory, plan.cp_vcpus,
        ));
        return Ok(cp_qcow);
    }
    Err(live_mode_not_implemented(
        "plan_cp_virt_install",
        "virsh dominfo + cp --reflink=auto + virt-install --import",
    ))
}

// ---- Block #8+9: CP Ready + kubeadm token ----------------------------------

/// Plan the CP IP discovery + Ready poll. Mirrors lines 510-539.
fn plan_cp_ready(plan: &Plan) -> Result<String> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would resolve CP IP via 'virsh domifaddr {}' (timeout ~5min)",
            plan.cp_name,
        ));
        log("DRY-RUN would poll 'kubectl get nodes' on CP until Ready (timeout ~600s)");
        return Ok("<resolved-at-runtime>".to_string());
    }
    Err(live_mode_not_implemented(
        "plan_cp_ready",
        "virsh domifaddr + ssh root@CP_IP kubectl get nodes (poll)",
    ))
}

/// Plan the kubeadm join-token mint. Mirrors lines 541-545.
fn plan_join_token(plan: &Plan, cp_ip: &str) -> Result<()> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would mint 2h-TTL kubeadm join token via 'ssh root@{cp_ip} kubeadm token create --print-join-command'"
        ));
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "plan_join_token",
        "ssh root@CP_IP kubeadm token create --print-join-command",
    ))
}

// ---- Block #10: per-worker seed + spawn ------------------------------------

/// Plan the per-worker seed + virt-install step. Mirrors lines 547-597.
fn plan_worker_spawn(plan: &Plan, worker_template: &str) -> Result<()> {
    if plan.worker_names.is_empty() {
        if plan.dry_run {
            log("DRY-RUN WORKER_NAMES=() — CP-only deploy, no workers to spawn");
        }
        return Ok(());
    }
    if plan.dry_run {
        for w in &plan.worker_names {
            let w_qcow = format!("{}/{}.qcow2", plan.pool_dir, w);
            let w_seed = format!("{}/{}-seed.iso", plan.pool_dir, w);
            log(&format!(
                "DRY-RUN would refuse to overwrite if worker VM '{w}' already defined"
            ));
            log(&format!(
                "DRY-RUN would render worker cloud-init user-data with join command + build seed {w_seed}"
            ));
            log(&format!(
                "DRY-RUN would clone {worker_template} -> {w_qcow} (reflink=auto)"
            ));
            log(&format!(
                "DRY-RUN would virt-install {w} (memory={} vcpus={}) attaching {w_qcow} + {w_seed} [parallel]",
                plan.worker_memory, plan.worker_vcpus,
            ));
        }
        log(&format!(
            "DRY-RUN would wait for {} parallel virt-install processes",
            plan.worker_names.len(),
        ));
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "plan_worker_spawn",
        "parallel virt-install loop with worker_user_data + seed ISO",
    ))
}

// ---- Block #11+12: cluster Ready + optional verify --------------------------

/// Plan the full-cluster Ready poll. Mirrors lines 599-616.
fn plan_cluster_ready(plan: &Plan) -> Result<()> {
    let expected = 1 + plan.worker_names.len();
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would poll cluster until {expected} nodes Ready (timeout ~600s)"
        ));
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "plan_cluster_ready",
        "ssh root@CP_IP kubectl get nodes (count Ready nodes)",
    ))
}

/// Plan the optional verify step. Mirrors lines 618-627. After the
/// v0.1.0 cutover (#353) the bash twin's verify call is now
/// `hbird verify app-deploy` (the Rust twin replaced
/// `scripts/verify-app-deploy.sh`).
fn plan_verify(plan: &Plan) -> Result<()> {
    if !plan.run_verify {
        return Ok(());
    }
    if plan.dry_run {
        log(
            "DRY-RUN RUN_VERIFY=true — would run 'hbird verify app-deploy' after Ready (post-#353)",
        );
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "plan_verify",
        "hbird verify app-deploy",
    ))
}

// ---- Block #13: summary footer ---------------------------------------------

fn plan_summary(plan: &Plan, cp_ip: &str) {
    log("");
    log("==============================================================");
    if plan.dry_run {
        log("DRY-RUN plan complete. No VMs were created.");
    } else {
        log("Cluster deployed.");
    }
    log(&format!("  CP:         {} ({cp_ip})", plan.cp_name));
    let workers = if plan.worker_names.is_empty() {
        "<none>".to_string()
    } else {
        plan.worker_names.join(" ")
    };
    log(&format!("  Workers:    {workers}"));
    log(&format!(
        "  Image src:  {} (tag={})",
        plan.image_source, plan.ghcr_tag,
    ));
    log(&format!(
        "  Kubeconfig: root@{cp_ip}:/etc/kubernetes/admin.conf",
    ));
    log("==============================================================");
}

// ---- run entrypoint --------------------------------------------------------

/// Dispatch entrypoint invoked by `main.rs`.
#[tracing::instrument(level = "debug", skip(args), fields(config = ?args.config, kvm_host = ?args.kvm_host, dry_run = args.dry_run), err(Debug))]
pub fn run(args: DeployClusterArgs) -> Result<()> {
    let config = hbird_config::parse(&args.config).map_err(|e| anyhow!("{e}"))?;
    let plan = Plan::from_args(&args, config)?;

    // Hard validation that the bash twin enforces before any side effects.
    if plan.enable_cloud_init != 1 {
        return Err(anyhow!(
            "ENABLE_CLOUD_INIT must be 1 for this flow (got '{}'). The deploy-cluster path requires cloud-init in the image to inject per-VM hostname + worker join + bootc switch.",
            plan.enable_cloud_init,
        ));
    }

    // ---- Plan summary header (bash 408) ----
    log(&format!("config: {}", plan.config_path.display()));
    let workers_str = if plan.worker_names.is_empty() {
        "<none>".to_string()
    } else {
        plan.worker_names.join(" ")
    };
    log(&format!(
        "config OK: CP={}, workers=({workers_str}), source={}, tag={}",
        plan.cp_name, plan.image_source, plan.ghcr_tag,
    ));

    let (cp_ref, worker_ref) = plan_image_acquisition(&plan)?;
    let (cp_template, worker_template) = plan_build_qcow2(&plan, &cp_ref, &worker_ref)?;
    let cp_seed = plan_cp_seed(&plan)?;
    let _cp_qcow = plan_cp_virt_install(&plan, &cp_template, &cp_seed)?;
    let cp_ip = plan_cp_ready(&plan)?;
    plan_join_token(&plan, &cp_ip)?;
    plan_worker_spawn(&plan, &worker_template)?;
    plan_cluster_ready(&plan)?;
    plan_verify(&plan)?;
    plan_summary(&plan, &cp_ip);

    Ok(())
}

// ---- helpers ---------------------------------------------------------------

/// Construct the "not yet implemented in the Rust live path" error
/// used by every helper that needs a real bib / virt-install /
/// SSH round-trip. The error wording explicitly points at the follow-up
/// issue so an operator hitting this in CI gets actionable guidance.
///
/// The tracking issue is [#335] — the live-execution slice for
/// deploy + spawn — not [#289], which this PR closes with the
/// dry-run parity surface.
///
/// [#335]: https://github.com/aatchison/hummingbird-k8s/issues/335
fn live_mode_not_implemented(helper: &str, equivalent: &str) -> anyhow::Error {
    anyhow!(
        "live-mode deploy-cluster: `{helper}` requires a remote libvirt / bib / SSH round-trip \
         that is not yet implemented in the Rust path. Bash equivalent: `{equivalent}`. \
         Until the live-execution slice lands (tracked by #335), run with `--dry-run` to preview \
         the plan, or use `make deploy-cluster CONFIG=…` to actually deploy."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbird_config::parse_str;

    fn cfg(workers: Option<Vec<&str>>) -> ClusterConfig {
        let mut body = String::from("CP_NAME=hbird-cp1\nSSH_PUBKEY_FILE=/k\nENABLE_CLOUD_INIT=1\n");
        if let Some(w) = workers {
            body.push_str(&format!("WORKER_NAMES=({})\n", w.join(" ")));
        }
        parse_str(&body).expect("test cfg parses")
    }

    #[test]
    fn plan_carries_worker_default_when_unset() {
        let args = DeployClusterArgs {
            config: PathBuf::from("/dev/null"),
            kvm_host: None,
            no_sudo: false,
            dry_run: true,
        };
        let plan = Plan::from_args(&args, cfg(None)).expect("plan");
        // CP_NAME=hbird-cp1 → workers default to (hbird-cp1-w1, hbird-cp1-w2)
        assert_eq!(plan.worker_names, vec!["hbird-cp1-w1", "hbird-cp1-w2"]);
    }

    #[test]
    fn plan_honors_explicit_empty_workers() {
        let args = DeployClusterArgs {
            config: PathBuf::from("/dev/null"),
            kvm_host: None,
            no_sudo: false,
            dry_run: true,
        };
        let plan = Plan::from_args(&args, cfg(Some(vec![]))).expect("plan");
        assert!(plan.worker_names.is_empty());
    }

    #[test]
    fn live_mode_not_implemented_names_issue_and_bash_equivalent() {
        let e = live_mode_not_implemented("plan_x", "ssh root@cp ...");
        let s = format!("{e}");
        assert!(s.contains("#335"), "must reference #335: {s}");
        assert!(s.contains("plan_x"));
        assert!(s.contains("ssh root@cp"));
        assert!(s.contains("--dry-run"));
    }
}
