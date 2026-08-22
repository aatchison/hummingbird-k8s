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
//! S2b lands the live-execution path for image acquisition and bib qcow2
//! build. S2c completes the boot half (cp_seed, virt_install, cp_ready,
//! join_token, worker_spawn, cluster_ready, verify).
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

use anyhow::{Result, anyhow, bail};
use clap::Args;
use clap::builder::BoolishValueParser;

use hbird_config::ClusterConfig;
use hbird_virt::Connection;

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

    /// bootc-image-builder (BIB) container image reference. Overrides
    /// the `BIB` env var. Default: `quay.io/centos-bootc/bootc-image-builder:latest`.
    #[arg(
        long,
        env = "BIB",
        default_value = "quay.io/centos-bootc/bootc-image-builder:latest"
    )]
    pub bib: String,

    /// Podman storage root override (`--root`). Maps to `PODMAN_ROOT` env.
    #[arg(long, env = "PODMAN_ROOT")]
    pub podman_root: Option<String>,

    /// Podman runroot override (`--runroot`). Maps to `PODMAN_RUNROOT` env.
    #[arg(long, env = "PODMAN_RUNROOT")]
    pub podman_runroot: Option<String>,

    /// Podman storage driver override (`--storage-driver`). Maps to
    /// `STORAGE_DRIVER` env.
    #[arg(long, env = "STORAGE_DRIVER")]
    pub storage_driver: Option<String>,

    /// Force-rebuild qcow2 templates even when the cache sidecar matches.
    /// Maps to `FORCE_REBUILD` env.
    #[arg(
        long,
        env = "FORCE_REBUILD",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub force_rebuild: bool,

    /// Hard-fail if the qcow2 cache is confirmed stale (rather than
    /// auto-rebuilding). Maps to `STRICT_CACHE` env.
    #[arg(
        long,
        env = "STRICT_CACHE",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub strict_cache: bool,

    /// Repo root for `make image-*` targets (used when `IMAGE_SOURCE=local`).
    /// Maps to `REPO_ROOT` env. Defaults to the repo root resolved from cwd
    /// (`git rev-parse --show-toplevel`, or walk-up to `Makefile+containers/`).
    /// The config file's location is NOT used as the default — configs often
    /// live outside the repo (e.g. `~/cluster.local.conf`). Override with
    /// this flag or `REPO_ROOT=<path>` when running outside the repo tree.
    #[arg(long, env = "REPO_ROOT")]
    pub repo_root: Option<PathBuf>,

    /// CP-Ready poll retry count. Env: CP_READY_RETRIES. Default: 60.
    #[arg(long, env = "CP_READY_RETRIES", default_value = "60")]
    pub cp_ready_retries: u32,

    /// Sleep seconds between CP-Ready poll attempts. Env: CP_READY_SLEEP. Default: 10.
    #[arg(long, env = "CP_READY_SLEEP", default_value = "10")]
    pub cp_ready_sleep_secs: u64,

    /// kubeadm join-token TTL. Env: TOKEN_TTL. Default: "2h".
    #[arg(long, env = "TOKEN_TTL", default_value = "2h")]
    pub token_ttl: String,
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
    ssh_pubkey_file: String,
    /// `KVM_HOST` SSH alias (`None` = local libvirt).
    kvm_host: Option<String>,
    /// When `true`, remote commands skip `sudo` (operator is in the `libvirt`
    /// group and the pool dir is group-writable). Consumed by live-execution
    /// helpers; not yet plumbed through every command string in S2c.
    #[allow(dead_code)]
    no_sudo: bool,
    dry_run: bool,
    // S2b fields
    bib: String,
    podman_root: Option<String>,
    podman_runroot: Option<String>,
    storage_driver: Option<String>,
    force_rebuild: bool,
    strict_cache: bool,
    repo_root: PathBuf,
    // S2c fields
    bootc_update_schedule: Option<String>,
    bootc_update_repo_k8s: Option<String>,
    bootc_update_repo_worker: Option<String>,
    /// Per-cluster CIDR overrides (#404). Emitted into
    /// /etc/hummingbird/k8s-init-local.env on the CP only.
    pod_cidr: Option<String>,
    service_cidr: Option<String>,
    /// Primary-NIC identity (#409): optional static IPs that become DHCP
    /// reservations on the `default` network, plus optional MAC overrides.
    /// The MAC is ALWAYS pinned at virt-install time (operator-set or
    /// name-derived) so a rebuilt VM keeps its DHCP lease.
    cp_ip: Option<String>,
    worker_ips: Option<Vec<String>>,
    cp_mac: Option<String>,
    worker_macs: Option<Vec<String>>,
    /// Optional second NIC (#405-#408): pre-existing libvirt network +
    /// per-VM MAC/CIDR family. `extra_network=None` = single-NIC deploy.
    extra_network: Option<String>,
    extra_net_cp_mac: Option<String>,
    extra_net_cp_ip: Option<String>,
    extra_net_worker_macs: Option<Vec<String>>,
    extra_net_worker_ips: Option<Vec<String>>,
    cp_ready_retries: u32,
    cp_ready_sleep_secs: u64,
    token_ttl: String,
}

impl Plan {
    fn from_args(args: &DeployClusterArgs, config: ClusterConfig) -> Result<Self> {
        let worker_names = config.resolved_worker_names();
        // Repo root: explicit --repo-root/REPO_ROOT flag wins; otherwise
        // derive from cwd (not the config file's directory — configs often
        // live outside the repo, e.g. ~/cluster.local.conf → that would set
        // repo_root=HOME and break `make -C HOME image-*`). Fixed by #33.
        let repo_root = if let Some(ref root) = args.repo_root {
            root.clone()
        } else if config.image_source == "local" {
            // `find_repo_root` falls back to "." when it can find neither a
            // git toplevel nor the repo markers by walking up from cwd.
            // Silently accepting that produced `make -C .` in whatever
            // directory the operator happened to be in, and a baffling
            // "No rule to make target 'image-k8s-with-cloud-init'" —
            // observed live on the KVM host when invoking hbird from $HOME
            // with an absolute --config path. Fail with the actionable
            // remedy instead. (S4 live validation, #289.)
            let root = find_repo_root();
            if !path_has_repo_markers(&root) {
                bail!(
                    "IMAGE_SOURCE=local needs the hummingbird-k8s repo to run \
                     `make image-*`, but the repo root could not be located from \
                     the current directory ({}). Either run hbird from inside a \
                     checkout, or pass --repo-root <path> (env: REPO_ROOT).",
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("?"))
                        .display(),
                );
            }
            root
        } else {
            // For registry pulls, repo_root is not used for image acquisition,
            // but we provide a sane default (cwd) to avoid failing the plan.
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        };
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
            ssh_pubkey_file: config.ssh_pubkey_file,
            kvm_host: args.kvm_host.clone(),
            no_sudo: args.no_sudo,
            dry_run: args.dry_run,
            bib: args.bib.clone(),
            podman_root: args.podman_root.clone(),
            podman_runroot: args.podman_runroot.clone(),
            storage_driver: args.storage_driver.clone(),
            force_rebuild: args.force_rebuild,
            strict_cache: args.strict_cache,
            repo_root,
            bootc_update_schedule: config.bootc_update_schedule,
            bootc_update_repo_k8s: config.bootc_update_repo_k8s,
            bootc_update_repo_worker: config.bootc_update_repo_worker,
            pod_cidr: config.pod_cidr.clone(),
            service_cidr: config.service_cidr.clone(),
            cp_ip: config.cp_ip.clone(),
            worker_ips: config.worker_ips.clone(),
            cp_mac: config.cp_mac.clone(),
            worker_macs: config.worker_macs.clone(),
            extra_network: config.extra_network.clone().filter(|s| !s.is_empty()),
            extra_net_cp_mac: config.extra_net_cp_mac.clone(),
            extra_net_cp_ip: config.extra_net_cp_ip.clone(),
            extra_net_worker_macs: config.extra_net_worker_macs.clone(),
            extra_net_worker_ips: config.extra_net_worker_ips.clone(),
            cp_ready_retries: args.cp_ready_retries,
            cp_ready_sleep_secs: args.cp_ready_sleep_secs,
            token_ttl: args.token_ttl.clone(),
        })
    }

    /// The forgejo registry host embedded in image refs.
    const FORGEJO_REGISTRY: &'static str = "forgejo.atchison.io";
    /// The GHCR registry host embedded in image refs.
    const GHCR_REGISTRY: &'static str = "ghcr.io";
}

// ---- Block #4: image acquisition -------------------------------------------

/// Plan the image-acquisition step. Mirrors `deploy-cluster.sh` line 560-600.
///
/// Returns `(cp_ref, worker_ref)` image references used as inputs to BIB.
fn plan_image_acquisition(plan: &Plan, conn: &Connection) -> Result<(String, String)> {
    let (cp_ref, worker_ref) = match plan.image_source.as_str() {
        "ghcr" => (
            format!(
                "{}/aatchison/hummingbird-k8s:{}",
                Plan::GHCR_REGISTRY,
                plan.ghcr_tag
            ),
            format!(
                "{}/aatchison/hummingbird-k8s-worker:{}",
                Plan::GHCR_REGISTRY,
                plan.ghcr_tag
            ),
        ),
        "forgejo" => (
            format!(
                "{}/aatchison/hummingbird-k8s:{}",
                Plan::FORGEJO_REGISTRY,
                plan.ghcr_tag
            ),
            format!(
                "{}/aatchison/hummingbird-k8s-worker:{}",
                Plan::FORGEJO_REGISTRY,
                plan.ghcr_tag
            ),
        ),
        "local" => (
            "localhost/hummingbird-k8s:latest".to_string(),
            "localhost/hummingbird-k8s-worker:latest".to_string(),
        ),
        other => {
            return Err(anyhow!(
                "IMAGE_SOURCE must be 'ghcr', 'forgejo', or 'local' (got '{other}')"
            ));
        }
    };

    if plan.dry_run {
        match plan.image_source.as_str() {
            "ghcr" | "forgejo" => {
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

    // Live path: pull or build.
    match plan.image_source.as_str() {
        "ghcr" | "forgejo" => {
            let cp_cmd = podman_pull_cmd(plan, &cp_ref);
            let worker_cmd = podman_pull_cmd(plan, &worker_ref);
            conn.exec_shell(&cp_cmd)
                .map_err(|e| anyhow!("podman pull {cp_ref} failed: {e}"))?;
            conn.exec_shell(&worker_cmd)
                .map_err(|e| anyhow!("podman pull {worker_ref} failed: {e}"))?;
            // #373: confirm the PULLED image actually reflects the on-disk
            // Containerfile. Without this a `IMAGE_SOURCE=ghcr` deploy
            // silently boot-tests the PUBLISHED image while the operator
            // believes they are testing their local edit. The bash twin
            // (the deleted lib/cache-utils.sh::hbird_assess_ghcr_image) did this; the
            // Rust ghcr path shipped without it.
            //
            // Freshness needs git, so on a checkout-free host this is
            // Unverifiable — which is normal here and only fatal under
            // STRICT_CACHE=1, where the operator has explicitly asked for
            // proof.
            for (image, label, containerfile) in [
                (&cp_ref, "CP image", "containers/k8s/Containerfile"),
                (
                    &worker_ref,
                    "worker image",
                    "containers/k8s-worker/Containerfile",
                ),
            ] {
                let vcs = crate::cache::image_vcs_ref(image).unwrap_or_default();
                let freshness = if vcs.is_empty() {
                    crate::cache::ImageFreshness::Unverifiable
                } else {
                    crate::cache::containerfile_changed_since(
                        &plan.repo_root,
                        &vcs,
                        &[containerfile],
                    )
                };
                match crate::cache::assess_ghcr_image(
                    freshness,
                    label,
                    &vcs,
                    containerfile,
                    plan.strict_cache,
                ) {
                    crate::cache::GhcrAssessResult::Fresh => {}
                    crate::cache::GhcrAssessResult::Warn(msg) => log(&msg),
                    crate::cache::GhcrAssessResult::StrictFail(msg) => bail!("{msg}"),
                }
            }
        }
        "local" => {
            let cp_cmd = make_image_cmd(&plan.repo_root, "image-k8s-with-cloud-init");
            let worker_cmd = make_image_cmd(&plan.repo_root, "image-worker-with-cloud-init");
            conn.exec_shell(&cp_cmd)
                .map_err(|e| anyhow!("make image-k8s-with-cloud-init failed: {e}"))?;
            conn.exec_shell(&worker_cmd)
                .map_err(|e| anyhow!("make image-worker-with-cloud-init failed: {e}"))?;
        }
        _ => unreachable!("matched above"),
    }

    Ok((cp_ref, worker_ref))
}

// ---- Block #5: bib qcow2 per flavor ----------------------------------------

/// Plan the bib qcow2 build step. Mirrors `deploy-cluster.sh` line 600-680
/// and `lib/build-common.sh::build_qcow2`.
///
/// Returns `(cp_template, worker_template)` paths.
fn plan_build_qcow2(
    plan: &Plan,
    cp_ref: &str,
    worker_ref: &str,
    conn: &Connection,
) -> Result<(String, String)> {
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

    // Live path: build CP and worker qcow2 templates.
    build_one_qcow2(plan, cp_ref, &cp_template, conn)?;
    build_one_qcow2(plan, worker_ref, &worker_template, conn)?;

    Ok((cp_template, worker_template))
}

/// Build (or reuse) one qcow2 template via BIB.
///
/// Mirrors `build_qcow2` from `lib/build-common.sh`.
fn build_one_qcow2(plan: &Plan, image_ref: &str, qcow_path: &str, conn: &Connection) -> Result<()> {
    use crate::cache::{CacheAssessResult, assess_qcow2_cache, read_sidecar};

    let qcow = std::path::Path::new(qcow_path);

    // Determine expected build ID from the image source.
    let expected_id = compute_expected_build_id(plan, image_ref, conn);

    // Cache assessment: skip-if-exists unless force_rebuild or confirmed stale.
    if qcow.exists() && !plan.force_rebuild {
        let cached_id = read_sidecar(qcow);
        let result = assess_qcow2_cache(
            cached_id.as_deref(),
            expected_id.as_deref(),
            plan.strict_cache,
        );
        match result {
            CacheAssessResult::Reuse => {
                log(&format!(
                    "cache: reusing {qcow_path} (build-ref matches or unverifiable)"
                ));
                return Ok(());
            }
            CacheAssessResult::Rebuild => {
                log(&format!(
                    "WARN: cache: {qcow_path} is stale — rebuilding (STRICT_CACHE=0)"
                ));
                // Fall through to rebuild.
            }
            CacheAssessResult::StrictFail => {
                return Err(anyhow!(
                    "cache: {qcow_path} is confirmed stale and STRICT_CACHE=1. \
                     Set FORCE_REBUILD=1 to override, or clear the qcow2 manually."
                ));
            }
        }
    }

    // Read pubkey for bib config.
    let pubkey_contents = conn
        .exec_shell(&format!("cat -- {}", sh_quote(&plan.ssh_pubkey_file)))
        .map_err(|e| {
            anyhow!(
                "could not read SSH_PUBKEY_FILE {}: {e}",
                plan.ssh_pubkey_file
            )
        })?;

    // Write bib TOML config to a temp file on the remote host.
    let bib_config_content = render_bib_config(pubkey_contents.trim());
    let bib_config_tmp = format!("/tmp/hbird-bib-config-{}.toml", std::process::id());
    conn.exec_shell(&format!(
        "cat > {} << 'HBIRD_BIB_EOF'\n{}\nHBIRD_BIB_EOF",
        sh_quote(&bib_config_tmp),
        bib_config_content
    ))
    .map_err(|e| anyhow!("could not write bib config to {bib_config_tmp}: {e}"))?;

    // Run BIB.
    let bib_cmd = bib_run_cmd(plan, image_ref, &bib_config_tmp);
    conn.exec_shell(&bib_cmd)
        .map_err(|e| anyhow!("BIB build failed for {image_ref}: {e}"))?;

    // Move disk.qcow2 → final path.
    let staging_qcow = format!("{}/qcow2/disk.qcow2", plan.pool_dir);
    conn.exec_shell(&format!(
        "mv -- {} {}",
        sh_quote(&staging_qcow),
        sh_quote(qcow_path)
    ))
    .map_err(|e| anyhow!("could not move {staging_qcow} → {qcow_path}: {e}"))?;

    // Remove staging dir.
    conn.exec_shell(&format!(
        "rm -rf -- {}",
        sh_quote(&format!("{}/qcow2", plan.pool_dir))
    ))
    .map_err(|e| anyhow!("could not remove BIB staging dir: {e}"))?;

    // Cleanup bib config temp file.
    let _ = conn.exec_shell(&format!("rm -f -- {}", sh_quote(&bib_config_tmp)));

    // Pool refresh (best-effort, mirrors bash twin's `|| true`).
    let _ = conn.exec_shell("virsh pool-refresh default 2>/dev/null || true");
    let _ = conn.exec_shell("virsh pool-refresh mass2 2>/dev/null || true");

    // Write sidecar (best-effort).
    if let Some(ref id) = expected_id {
        // Write sidecar via SSH (the qcow lives on the remote host).
        let sidecar_path = format!("{qcow_path}.build-ref");
        let _ = conn.exec_shell(&format!(
            "printf '%s\\n' {} > {}",
            sh_quote(id),
            sh_quote(&sidecar_path)
        ));
    }

    Ok(())
}

/// Compute the expected build ID for the given image reference.
///
/// For `ghcr`/`forgejo`: try `podman image inspect` to get the OCI
/// revision label. On failure (image not yet pulled or inspect unsupported),
/// returns `None`.
///
/// For `local`: FNV-1a hash of the relevant Containerfile.
fn compute_expected_build_id(plan: &Plan, image_ref: &str, conn: &Connection) -> Option<String> {
    match plan.image_source.as_str() {
        "ghcr" | "forgejo" => {
            let cmd = podman_inspect_vcs_ref_cmd(plan, image_ref);
            let out = conn.exec_shell(&cmd).ok()?;
            let revision = out.trim().to_string();
            crate::cache::build_id(&plan.image_source, &revision)
        }
        "local" => {
            // Determine which Containerfile to hash based on whether this is
            // the CP or worker image.
            let cf_path = if image_ref.contains("worker") {
                plan.repo_root.join("containers/k8s-worker/Containerfile")
            } else {
                plan.repo_root.join("containers/k8s/Containerfile")
            };
            let hash = crate::cache::containerfile_hash(&cf_path)?;
            crate::cache::build_id("local", &hash)
        }
        _ => None,
    }
}

// ---- Block #6+7: CP cloud-init seed + virt-install --------------------------

/// Plan the CP cloud-init user-data + seed ISO step. Mirrors lines 465-478.
fn plan_cp_seed(
    plan: &Plan,
    conn: &Connection,
    pubkey_contents: &str,
    net2: Option<&Net2>,
) -> Result<String> {
    let cp_seed = format!("{}/{}-seed.iso", plan.pool_dir, plan.cp_name);
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would render CP cloud-init user-data (auto-update-cp={}, switch-to-ghcr={}, ghcr-tag={})",
            plan.auto_update_cp, plan.switch_to_ghcr, plan.ghcr_tag,
        ));
        // Second NIC: the seed grows a network-config file so
        // cloud-init-local configures both NICs BEFORE NetworkManager
        // starts (no DHCP race, no second default route).
        if let Some(n) = net2 {
            log(&format!(
                "DRY-RUN would render net2 network-config for {} (mac={}, ip={}) into the seed",
                plan.cp_name, n.mac, n.ip,
            ));
        }
        log(&format!("DRY-RUN would build CP cloud-init seed {cp_seed}"));
        return Ok(cp_seed);
    }

    let user_data = render_cp_user_data(
        &plan.cp_name,
        pubkey_contents,
        plan.switch_to_ghcr,
        &plan.ghcr_tag,
        plan.auto_update_cp,
        CpOverrides {
            bootc_update_schedule: plan.bootc_update_schedule.as_deref(),
            bootc_update_repo_k8s: plan.bootc_update_repo_k8s.as_deref(),
            pod_cidr: plan.pod_cidr.as_deref(),
            service_cidr: plan.service_cidr.as_deref(),
            net2_mac: net2.map(|n| n.mac.as_str()),
        },
    );

    let ud_tmp = format!("/tmp/hbird-cp-ud-{}.yaml", std::process::id());
    let write_cmd = format!(
        "cat > '{}' << 'HBIRD_CI_EOF'\n{}\nHBIRD_CI_EOF",
        ud_tmp, user_data
    );
    conn.exec_shell(&write_cmd)
        .map_err(|e| anyhow!("could not write CP user-data to {ud_tmp}: {e}"))?;

    let nc_tmp = write_net2_config_tmp(conn, net2, &format!("cp-{}", std::process::id()))?;

    let seed_cmd = cloud_init_seed_cmd(&plan.cp_name, &ud_tmp, &cp_seed, nc_tmp.as_deref());
    conn.exec_shell(&seed_cmd)
        .map_err(|e| anyhow!("cloud-init seed build for CP failed: {e}"))?;

    Ok(cp_seed)
}

/// Write the rendered net2 network-config to a remote tmpfile; returns
/// its path (`None` when the second NIC is off).
fn write_net2_config_tmp(
    conn: &Connection,
    net2: Option<&Net2>,
    tag: &str,
) -> Result<Option<String>> {
    let Some(n) = net2 else {
        return Ok(None);
    };
    let nc_tmp = format!("/tmp/hbird-netcfg-{tag}.yaml");
    let content = render_net2_network_config(&n.mac, &n.ip);
    let write_cmd = format!("cat > '{nc_tmp}' << 'HBIRD_CI_EOF'\n{content}\nHBIRD_CI_EOF");
    conn.exec_shell(&write_cmd)
        .map_err(|e| anyhow!("could not write net2 network-config to {nc_tmp}: {e}"))?;
    Ok(Some(nc_tmp))
}

/// Plan the CP virt-install step. Mirrors lines 480-508 (+#409 MAC pin
/// and DHCP reservation, bash lines 1019-1036).
fn plan_cp_virt_install(
    plan: &Plan,
    conn: &Connection,
    cp_template: &str,
    cp_seed: &str,
    macs: &PrimaryMacs,
    net2: Option<&Net2>,
) -> Result<String> {
    let cp_qcow = format!("{}/{}.qcow2", plan.pool_dir, plan.cp_name);
    let cp_ip = plan.cp_ip.as_deref().filter(|s| !s.is_empty());
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would refuse to overwrite if CP VM '{}' already defined",
            plan.cp_name,
        ));
        log(&format!(
            "DRY-RUN would clone {cp_template} -> {cp_qcow} (reflink=auto)"
        ));
        // Reservation line only when CP_IP is configured — bash's
        // `ensure_dhcp_reservation` early-returns on an empty ip.
        if let Some(ip) = cp_ip {
            log(&format!(
                "DRY-RUN would ensure DHCP reservation on 'default': {} {} -> {ip}",
                plan.cp_name, macs.cp,
            ));
        }
        // INVARIANT (#409 port): a config without CP_MAC renders the
        // pre-#409 plan line byte-for-byte (pinned by the phase4 dry-run
        // fixture). The live path ALWAYS pins the (derived) MAC; the plan
        // only surfaces it when the operator set it explicitly.
        let mac_note = if plan.cp_mac.as_deref().is_some_and(|m| !m.is_empty()) {
            format!(", primary mac={}", macs.cp)
        } else {
            String::new()
        };
        if let Some(n) = net2 {
            log(&format!(
                "DRY-RUN would attach second NIC: network={},mac={}",
                n.network, n.mac,
            ));
        }
        log(&format!(
            "DRY-RUN would virt-install {} (memory={} vcpus={}{mac_note}) attaching {cp_qcow} + {cp_seed}",
            plan.cp_name, plan.cp_memory, plan.cp_vcpus,
        ));
        return Ok(cp_qcow);
    }

    // Guard: fail if CP VM already defined.
    match conn.dominfo(&plan.cp_name) {
        Ok(_) => {
            return Err(anyhow!(
                "CP VM '{}' is already defined — refusing to overwrite",
                plan.cp_name
            ));
        }
        Err(hbird_virt::Error::VirshFailed { .. }) => {} // not defined, proceed
        Err(e) => return Err(anyhow!("dominfo probe for CP failed: {e}")),
    }

    log(&format!("cloning CP qcow2 -> {cp_qcow}"));
    conn.remote_cp_reflink(cp_template, &cp_qcow)
        .map_err(|e| anyhow!("reflink clone {cp_template} -> {cp_qcow} failed: {e}"))?;

    // Primary NIC MAC: deterministic from the domain name so a rebuilt VM
    // keeps its DHCP lease (#409). Overridable per-cluster via CP_MAC.
    if let Some(ip) = cp_ip {
        ensure_dhcp_reservation(conn, "default", &macs.cp, ip, &plan.cp_name);
    }

    // Operator-visible wording mirrors bash line 1025 (incl. primary mac).
    log(&format!(
        "virt-install {} (memory={} vcpus={}, primary mac={})",
        plan.cp_name, plan.cp_memory, plan.cp_vcpus, macs.cp,
    ));
    conn.virt_install_vm(&hbird_virt::VmSpec {
        name: &plan.cp_name,
        memory_mib: plan.cp_memory as u64,
        vcpus: plan.cp_vcpus,
        disk_path: &cp_qcow,
        cdrom: Some(cp_seed),
        primary_mac: Some(&macs.cp),
        // mac= pins the guest-visible MAC so the seed's network-config
        // (matched by MAC) deterministically finds the right interface.
        extra_nic: net2.map(|n| hbird_virt::ExtraNic {
            network: &n.network,
            mac: &n.mac,
        }),
    })
    .map_err(|e| anyhow!("virt-install {} failed: {e}", plan.cp_name))?;

    Ok(cp_qcow)
}

// ---- Block #8+9: CP Ready + kubeadm token ----------------------------------

/// Plan the CP IP discovery + Ready poll. Mirrors lines 510-539.
/// Returns the CP IP address string.
fn plan_cp_ready(plan: &Plan, conn: &Connection, privkey_path: &str) -> Result<String> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would resolve CP IP via 'virsh domifaddr {}' (timeout ~5min)",
            plan.cp_name,
        ));
        log("DRY-RUN would poll 'kubectl get nodes' on CP until Ready (timeout ~600s)");
        return Ok("<resolved-at-runtime>".to_string());
    }

    // Poll domifaddr until CP gets an IP.
    log("waiting for CP IP to appear via DHCP...");
    let cp_ip_cell = std::cell::Cell::new(None::<std::net::Ipv4Addr>);
    let found =
        hbird_virt::poll::retry(
            plan.cp_ready_retries,
            plan.cp_ready_sleep_secs,
            || match conn.domifaddr(&plan.cp_name) {
                Ok(Some(ip)) => {
                    cp_ip_cell.set(Some(ip));
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
            "could not resolve CP IP after ~{}min",
            plan.cp_ready_retries * plan.cp_ready_sleep_secs as u32 / 60
        ));
    }
    let cp_ip = cp_ip_cell.get().unwrap();
    log(&format!("CP IP: {cp_ip}"));

    // Poll kubectl Ready via SSH.
    let kubectl_ready_cmd = cp_ssh_cmd(
        privkey_path,
        &cp_ip.to_string(),
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes --no-headers 2>/dev/null \
         | awk '$2==\"Ready\"' | grep -q .",
    );

    log(&format!(
        "polling kubectl until CP Ready (max ~{}s)",
        plan.cp_ready_retries * plan.cp_ready_sleep_secs as u32
    ));
    let cp_ready =
        hbird_virt::poll::retry(
            plan.cp_ready_retries,
            plan.cp_ready_sleep_secs,
            || match conn.exec_shell(&kubectl_ready_cmd) {
                Ok(_) => Ok(true),
                Err(_) => Ok(false),
            },
        )
        .map_err(|e: anyhow::Error| e)?;

    if !cp_ready {
        return Err(anyhow!("CP never reached Ready"));
    }
    log("CP Ready");
    Ok(cp_ip.to_string())
}

/// Plan the kubeadm join-token mint. Mirrors lines 541-545.
/// Returns the full `kubeadm join ...` command string.
fn plan_join_token(
    plan: &Plan,
    conn: &Connection,
    cp_ip: &str,
    privkey_path: &str,
) -> Result<String> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would mint 2h-TTL kubeadm join token via 'ssh root@{cp_ip} kubeadm token create --print-join-command'"
        ));
        return Ok("<join-cmd-at-runtime>".to_string());
    }

    log(&format!(
        "minting {}-TTL kubeadm join token from CP",
        plan.token_ttl
    ));
    let kubeadm_cmd = format!(
        "kubeadm token create --ttl {} --print-join-command",
        plan.token_ttl
    );
    let ssh_cmd = cp_ssh_cmd(privkey_path, cp_ip, &kubeadm_cmd);
    let join_cmd = conn
        .exec_shell(&ssh_cmd)
        .map_err(|e| anyhow!("kubeadm token create failed: {e}"))?;
    let join_cmd = join_cmd.trim().to_string();

    if !join_cmd.starts_with("kubeadm join") {
        return Err(anyhow!(
            "expected 'kubeadm join ...' from CP, got: {:?}",
            join_cmd
        ));
    }
    Ok(join_cmd)
}

// ---- Block #10: per-worker seed + spawn ------------------------------------

/// Plan the per-worker seed + virt-install step. Mirrors lines 547-597
/// (+#409 MAC pin and DHCP reservation, bash lines 1114-1129).
fn plan_worker_spawn(
    plan: &Plan,
    conn: &Connection,
    worker_template: &str,
    join_cmd: &str,
    pubkey_contents: &str,
    macs: &PrimaryMacs,
    net2s: Option<&[Net2]>,
) -> Result<()> {
    if plan.worker_names.is_empty() {
        if plan.dry_run {
            log("DRY-RUN WORKER_NAMES=() — CP-only deploy, no workers to spawn");
        }
        return Ok(());
    }
    if plan.dry_run {
        for (i, w) in plan.worker_names.iter().enumerate() {
            let w_qcow = format!("{}/{}.qcow2", plan.pool_dir, w);
            let w_seed = format!("{}/{}-seed.iso", plan.pool_dir, w);
            log(&format!(
                "DRY-RUN would refuse to overwrite if worker VM '{w}' already defined"
            ));
            if let Some(n) = net2s.and_then(|v| v.get(i)) {
                log(&format!(
                    "DRY-RUN would render net2 network-config for {w} (mac={}, ip={}) into the seed",
                    n.mac, n.ip,
                ));
            }
            log(&format!(
                "DRY-RUN would render worker cloud-init user-data with join command + build seed {w_seed}"
            ));
            log(&format!(
                "DRY-RUN would clone {worker_template} -> {w_qcow} (reflink=auto)"
            ));
            // See the CP block: reservation line only when the matching
            // WORKER_IPS entry exists; mac note only when operator-set.
            if let Some(ip) = worker_ip(plan, i) {
                log(&format!(
                    "DRY-RUN would ensure DHCP reservation on 'default': {w} {} -> {ip}",
                    macs.workers[i],
                ));
            }
            let mac_note = if worker_mac_configured(plan, i) {
                format!(", primary mac={}", macs.workers[i])
            } else {
                String::new()
            };
            if let Some(n) = net2s.and_then(|v| v.get(i)) {
                log(&format!(
                    "DRY-RUN would attach second NIC: network={},mac={}",
                    n.network, n.mac,
                ));
            }
            log(&format!(
                "DRY-RUN would virt-install {w} (memory={} vcpus={}{mac_note}) attaching {w_qcow} + {w_seed} [parallel]",
                plan.worker_memory, plan.worker_vcpus,
            ));
        }
        log(&format!(
            "DRY-RUN would wait for {} parallel virt-install processes",
            plan.worker_names.len(),
        ));
        return Ok(());
    }

    for (i, w) in plan.worker_names.iter().enumerate() {
        // Guard: fail if worker VM already defined.
        match conn.dominfo(w) {
            Ok(_) => {
                return Err(anyhow!(
                    "worker VM '{w}' already defined — refusing to overwrite"
                ));
            }
            Err(hbird_virt::Error::VirshFailed { .. }) => {}
            Err(e) => return Err(anyhow!("dominfo probe for worker {w}: {e}")),
        }

        let w_qcow = format!("{}/{w}.qcow2", plan.pool_dir);
        let w_seed = format!("{}/{w}-seed.iso", plan.pool_dir);
        let ud_tmp = format!("/tmp/hbird-w-ud-{}-{w}.yaml", std::process::id());

        let w_net2 = net2s.and_then(|v| v.get(i));
        let user_data = render_worker_user_data(
            w,
            pubkey_contents,
            join_cmd,
            plan.switch_to_ghcr,
            &plan.ghcr_tag,
            WorkerOverrides {
                bootc_update_schedule: plan.bootc_update_schedule.as_deref(),
                bootc_update_repo_worker: plan.bootc_update_repo_worker.as_deref(),
                net2_mac: w_net2.map(|n| n.mac.as_str()),
            },
        );

        let write_cmd = format!(
            "cat > '{}' << 'HBIRD_CI_EOF'\n{}\nHBIRD_CI_EOF",
            ud_tmp, user_data
        );
        conn.exec_shell(&write_cmd)
            .map_err(|e| anyhow!("could not write worker user-data to {ud_tmp}: {e}"))?;

        let nc_tmp = write_net2_config_tmp(conn, w_net2, &format!("w-{}-{w}", std::process::id()))?;

        let seed_cmd = cloud_init_seed_cmd(w, &ud_tmp, &w_seed, nc_tmp.as_deref());
        conn.exec_shell(&seed_cmd)
            .map_err(|e| anyhow!("cloud-init seed build for {w} failed: {e}"))?;

        log(&format!("cloning worker qcow2 -> {w_qcow}"));
        conn.remote_cp_reflink(worker_template, &w_qcow)
            .map_err(|e| anyhow!("reflink clone {worker_template} -> {w_qcow} failed: {e}"))?;

        // Primary NIC MAC + optional reservation — see the CP block (#409).
        if let Some(ip) = worker_ip(plan, i) {
            ensure_dhcp_reservation(conn, "default", &macs.workers[i], ip, w);
        }

        // Operator-visible wording mirrors bash line 1118 (incl. primary
        // mac); `[bg]` is preserved even though the Rust port installs
        // sequentially — operators grep for the marker.
        log(&format!(
            "virt-install {w} (memory={} vcpus={}, primary mac={}) [bg]",
            plan.worker_memory, plan.worker_vcpus, macs.workers[i],
        ));
        conn.virt_install_vm(&hbird_virt::VmSpec {
            name: w,
            memory_mib: plan.worker_memory as u64,
            vcpus: plan.worker_vcpus,
            disk_path: &w_qcow,
            cdrom: Some(&w_seed),
            primary_mac: Some(&macs.workers[i]),
            extra_nic: w_net2.map(|n| hbird_virt::ExtraNic {
                network: &n.network,
                mac: &n.mac,
            }),
        })
        .map_err(|e| anyhow!("virt-install {w} failed: {e}"))?;
    }

    Ok(())
}

// ---- Block #11+12: cluster Ready + optional verify --------------------------

/// Plan the full-cluster Ready poll. Mirrors lines 599-616.
fn plan_cluster_ready(
    plan: &Plan,
    conn: &Connection,
    cp_ip: &str,
    privkey_path: &str,
) -> Result<()> {
    let expected_nodes: Vec<String> = std::iter::once(plan.cp_name.clone())
        .chain(plan.worker_names.iter().cloned())
        .collect();
    let expected = expected_nodes.len();

    if plan.dry_run {
        log(&format!(
            "DRY-RUN would poll cluster until {expected} nodes Ready (timeout ~600s)"
        ));
        return Ok(());
    }

    for node_name in &expected_nodes {
        log(&format!(
            "polling '{node_name}' Ready (max ~{}s)",
            plan.cp_ready_retries * plan.cp_ready_sleep_secs as u32
        ));
        let check_cmd = cp_ssh_cmd(
            privkey_path,
            cp_ip,
            &format!(
                "kubectl --kubeconfig=/etc/kubernetes/admin.conf get node '{}' \
                 --no-headers 2>/dev/null | awk '$2==\"Ready\"{{print \"yes\"}}'",
                node_name
            ),
        );

        let ready = hbird_virt::poll::retry(
            plan.cp_ready_retries,
            plan.cp_ready_sleep_secs,
            || match conn.exec_shell(&check_cmd) {
                Ok(out) if out.trim().contains("yes") => Ok(true),
                Ok(_) => Ok(false),
                Err(_) => Ok(false),
            },
        )
        .map_err(|e: anyhow::Error| e)?;

        if !ready {
            return Err(anyhow!("node '{}' never reached Ready", node_name));
        }
    }

    log(&format!("cluster Ready: all {expected} named nodes Ready"));
    Ok(())
}

/// Plan the optional verify step. Mirrors lines 618-627. After the
/// v0.1.0 cutover (#353) the bash twin's verify call is now
/// `hbird verify app-deploy` (the Rust twin replaced
/// `scripts/verify-app-deploy.sh`).
fn plan_verify(plan: &Plan, cp_ip: &str) -> Result<()> {
    if !plan.run_verify {
        return Ok(());
    }
    if plan.dry_run {
        log(
            "DRY-RUN RUN_VERIFY=true — would run 'hbird verify app-deploy' after Ready (post-#353)",
        );
        return Ok(());
    }

    let mut cmd = std::process::Command::new("hbird");
    cmd.args([
        "verify",
        "app-deploy",
        "--config",
        plan.config_path.to_str().unwrap_or(""),
        "--cp-ip",
        cp_ip,
    ]);
    if let Some(ref kvm_host) = plan.kvm_host {
        if !kvm_host.is_empty() {
            cmd.args(["--kvm-host", kvm_host]);
        }
    }

    let status = cmd.status();

    match status {
        Err(e) if plan.strict_cache => {
            return Err(anyhow!(
                "hbird verify app-deploy not found on PATH (required under STRICT_CACHE=1): {e}"
            ));
        }
        Err(_) => {
            log("hbird verify app-deploy not found on PATH; skipping");
        }
        Ok(s) if !s.success() => {
            if plan.strict_cache {
                return Err(anyhow!(
                    "hbird verify app-deploy exited non-zero — deploy fails under STRICT_CACHE=1"
                ));
            }
            log(
                "hbird verify app-deploy exited non-zero (cluster is up; verifier failure is informational)",
            );
        }
        Ok(_) => {}
    }
    Ok(())
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
    // Keep the parser's non-fatal diagnostics past the Plan handoff
    // (Plan::from_args consumes the config).
    let config_warnings = config.warnings.clone();
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
    // Bash `source`s the config and silently accepts unknown keys; the
    // Rust parser reports them. Print at plan time — silent typo-eating
    // is exactly how the #405-#410 knobs went missing unnoticed.
    for w in &config_warnings {
        log(&format!("WARN: config: {w}"));
    }
    // Primary-NIC MAC family (#409): malformed / colliding MACs are hard
    // failures before any side effects (bash extra-network-validation).
    let primary_macs = resolve_primary_macs(
        &plan.cp_name,
        plan.cp_mac.as_deref(),
        &plan.worker_names,
        plan.worker_macs.as_deref(),
    )?;
    // POD_CIDR / SERVICE_CIDR are printf'd into cloud-init YAML and
    // sourced as root by k8s-init.sh — validate whenever set,
    // independent of EXTRA_NETWORK (bash lines 730-734).
    for (knob, v) in [
        ("POD_CIDR", plan.pod_cidr.as_deref()),
        ("SERVICE_CIDR", plan.service_cidr.as_deref()),
    ] {
        if let Some(v) = v.filter(|s| !s.is_empty())
            && !is_valid_cidr(v)
        {
            return Err(anyhow!(
                "{knob} is malformed (need CIDR like 10.244.0.0/16): '{v}'"
            ));
        }
    }
    // Optional second NIC (#405-#408): validate the whole knob family up
    // front so a partial config fails here, not as a half-provisioned VM.
    let extra_net = validate_extra_network(&plan, &primary_macs)?;
    let (cp_net2, worker_net2s) = match &extra_net {
        Some((cp, workers)) => (Some(cp), Some(workers.as_slice())),
        None => (None, None),
    };
    let workers_str = if plan.worker_names.is_empty() {
        "<none>".to_string()
    } else {
        plan.worker_names.join(" ")
    };
    // `${EXTRA_NETWORK:+, extra-net=...}` suffix — byte-parity with bash 819.
    let extra_net_note = match &cp_net2 {
        Some(n) => format!(", extra-net={}", n.network),
        None => String::new(),
    };
    log(&format!(
        "config OK: CP={}, workers=({workers_str}), source={}, tag={}{extra_net_note}",
        plan.cp_name, plan.image_source, plan.ghcr_tag,
    ));

    // Build connection once; shared by image acquisition and bib.
    let conn = crate::virt_bridge::build_connection(plan.kvm_host.as_deref());

    // Host-side EXTRA_NETWORK preflight (bash runs it inside the
    // validation block): defined + active + VF capacity, BEFORE any
    // template build or clone can waste minutes.
    if let Some(n) = &cp_net2 {
        if plan.dry_run {
            log(&format!(
                "DRY-RUN would verify EXTRA_NETWORK '{}' is defined + active (and has enough VFs when it is a hostdev pool)",
                n.network,
            ));
        } else {
            check_extra_network_on_host(&plan, &conn, &n.network)?;
        }
    }

    // Read the SSH pubkey contents from the KVM host (needed for cloud-init).
    let pubkey_contents = if plan.dry_run {
        String::new()
    } else {
        conn.exec_shell(&format!("cat -- {}", sh_quote(&plan.ssh_pubkey_file)))
            .map_err(|e| anyhow!("cannot read SSH_PUBKEY_FILE {}: {e}", plan.ssh_pubkey_file))?
    };
    let privkey_path = derive_privkey_path(&plan.ssh_pubkey_file);

    let (cp_ref, worker_ref) = plan_image_acquisition(&plan, &conn)?;
    let (cp_template, worker_template) = plan_build_qcow2(&plan, &cp_ref, &worker_ref, &conn)?;
    let cp_seed = plan_cp_seed(&plan, &conn, &pubkey_contents, cp_net2)?;
    let _cp_qcow =
        plan_cp_virt_install(&plan, &conn, &cp_template, &cp_seed, &primary_macs, cp_net2)?;
    let cp_ip = plan_cp_ready(&plan, &conn, &privkey_path)?;
    let join_cmd = plan_join_token(&plan, &conn, &cp_ip, &privkey_path)?;
    plan_worker_spawn(
        &plan,
        &conn,
        &worker_template,
        &join_cmd,
        &pubkey_contents,
        &primary_macs,
        worker_net2s,
    )?;
    plan_cluster_ready(&plan, &conn, &cp_ip, &privkey_path)?;
    plan_verify(&plan, &cp_ip)?;
    plan_summary(&plan, &cp_ip);

    Ok(())
}

// ---- S2c helpers -----------------------------------------------------------

/// Strip `.pub` from pubkey path to get privkey path.
pub(crate) fn derive_privkey_path(pubkey_file: &str) -> String {
    pubkey_file
        .strip_suffix(".pub")
        .unwrap_or(pubkey_file)
        .to_string()
}

/// Build `ssh -i <privkey> -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
/// root@<ip> <remote_cmd>` command string.
pub(crate) fn cp_ssh_cmd(privkey_path: &str, cp_ip: &str, remote_cmd: &str) -> String {
    format!(
        "ssh -i {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@{} {}",
        sh_quote(privkey_path),
        cp_ip,
        sh_quote(remote_cmd),
    )
}

/// Render CP cloud-config YAML (bash twin: `render_cp_user_data`).
///
/// Pure function — takes all inputs, returns YAML string.
/// Cloud-init override inputs for the CP renderer.
///
/// Grouped into a struct rather than passed as four more positional
/// parameters: `render_cp_user_data` had grown to 9 arguments (clippy's
/// `too_many_arguments` limit is 7), and four consecutive
/// `Option<&str>` arguments are trivially transposable at a call site —
/// the compiler cannot catch swapping `pod_cidr` with `service_cidr`.
/// Named fields make that class of bug impossible.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CpOverrides<'a> {
    /// `BOOTC_UPDATE_SCHEDULE` — systemd `OnCalendar=` override.
    pub bootc_update_schedule: Option<&'a str>,
    /// `BOOTC_UPDATE_REPO_K8S` — registry the semver updater tracks.
    pub bootc_update_repo_k8s: Option<&'a str>,
    /// `POD_CIDR` — pod network passed to kubeadm/Cilium on first boot.
    pub pod_cidr: Option<&'a str>,
    /// `SERVICE_CIDR` — service network passed to kubeadm on first boot.
    pub service_cidr: Option<&'a str>,
    /// Second-NIC MAC (`EXTRA_NET_CP_MAC`) — when set, the runcmd block
    /// opens with the IPv6-off entry for that NIC (#407).
    pub net2_mac: Option<&'a str>,
}

pub(crate) fn render_cp_user_data(
    cp_name: &str,
    pubkey_contents: &str,
    switch_to_ghcr: bool,
    ghcr_tag: &str,
    auto_update_cp: bool,
    overrides: CpOverrides<'_>,
) -> String {
    let CpOverrides {
        bootc_update_schedule,
        bootc_update_repo_k8s,
        pod_cidr,
        service_cidr,
        net2_mac,
    } = overrides;
    let mut out = String::new();
    out.push_str("#cloud-config\n");
    out.push_str(&format!("hostname: {cp_name}\n"));
    out.push_str("disable_root: false\n");
    out.push_str("users:\n");
    out.push_str("  - name: root\n");
    out.push_str("    ssh_authorized_keys:\n");
    out.push_str(&format!("      - {pubkey_contents}\n"));
    // Bash treats an empty var as unset (`-n "${VAR:-}"`), so normalize
    // Some("") to None or we would emit a drop-in with a blank OnCalendar=
    // and silently disarm the timer.
    let bootc_update_schedule = bootc_update_schedule.filter(|s| !s.is_empty());
    let bootc_update_repo_k8s = bootc_update_repo_k8s.filter(|s| !s.is_empty());
    let pod_cidr = pod_cidr.filter(|s| !s.is_empty());
    let service_cidr = service_cidr.filter(|s| !s.is_empty());
    // write_files for bootc-semver-update + CIDR overrides. Only emit the
    // block when at least one override is set, otherwise the YAML stays
    // clean (no empty `write_files:` key). Mirrors deploy-cluster.sh:64-100.
    if bootc_update_schedule.is_some()
        || bootc_update_repo_k8s.is_some()
        || pod_cidr.is_some()
        || service_cidr.is_some()
    {
        out.push_str("write_files:\n");
        if let Some(schedule) = bootc_update_schedule {
            // The empty `OnCalendar=` FIRST clears the image-baked default;
            // without it systemd unions the two schedules instead of
            // replacing, and the node would still fire on the baked timer.
            out.push_str(
                "  - path: /etc/systemd/system/bootc-semver-update.timer.d/schedule.conf\n",
            );
            out.push_str("    owner: root:root\n");
            out.push_str("    permissions: '0644'\n");
            out.push_str("    content: |\n");
            out.push_str("      [Timer]\n");
            out.push_str("      OnCalendar=\n");
            out.push_str(&format!("      OnCalendar={schedule}\n"));
        }
        if let Some(repo) = bootc_update_repo_k8s {
            out.push_str("  - path: /etc/hummingbird/bootc-update.env\n");
            out.push_str("    owner: root:root\n");
            out.push_str("    permissions: '0644'\n");
            out.push_str("    content: |\n");
            out.push_str(&format!("      REPO={repo}\n"));
            out.push_str("      PREFIX=v\n");
        }
        if pod_cidr.is_some() || service_cidr.is_some() {
            // Deliberately a SEPARATE file from the image-baked
            // k8s-init.env: k8s-init.sh sources this one AFTER the baked
            // env, so operator values win and a bootc image update can
            // never clobber them (and vice versa). Mode 0600 matches the
            // bash twin.
            out.push_str("  - path: /etc/hummingbird/k8s-init-local.env\n");
            out.push_str("    owner: root:root\n");
            out.push_str("    permissions: '0600'\n");
            out.push_str("    content: |\n");
            if let Some(pod) = pod_cidr {
                out.push_str(&format!("      POD_CIDR={pod}\n"));
            }
            if let Some(svc) = service_cidr {
                out.push_str(&format!("      SERVICE_CIDR={svc}\n"));
            }
        }
    }
    out.push_str("runcmd:\n");
    // Before anything else: neutralise the second NIC's IPv6 (the v2
    // keys never reach NetworkManager — see net2_ipv6_off_runcmd).
    if let Some(mac) = net2_mac.filter(|s| !s.is_empty()) {
        out.push_str(&net2_ipv6_off_runcmd(mac));
    }
    if switch_to_ghcr {
        out.push_str(&format!(
            "  - [ bootc, switch, ghcr.io/aatchison/hummingbird-k8s:{ghcr_tag} ]\n"
        ));
    }
    if auto_update_cp {
        out.push_str("  - [ systemctl, enable, --now, bootc-semver-update.timer ]\n");
        out.push_str("  - [ systemctl, disable, --now, bootc-fetch-apply-updates.timer ]\n");
    } else {
        out.push_str("  - [ systemctl, disable, --now, bootc-semver-update.timer ]\n");
    }
    // Re-read the drop-in cloud-init just wrote so the override takes effect
    // THIS boot, not only the next one. Gated on auto_update_cp so the
    // false-branch's `disable` above stays sticky (bash line 136-143).
    if auto_update_cp && bootc_update_schedule.is_some() {
        out.push_str("  - [ systemctl, daemon-reload ]\n");
        out.push_str("  - [ systemctl, restart, bootc-semver-update.timer ]\n");
    }
    out
}

/// Cloud-init override inputs for the worker renderer. Same rationale
/// as [`CpOverrides`]: a params struct instead of positional
/// `Option<&str>`s (clippy `too_many_arguments` + transposition safety).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WorkerOverrides<'a> {
    /// `BOOTC_UPDATE_SCHEDULE` — systemd `OnCalendar=` override.
    pub bootc_update_schedule: Option<&'a str>,
    /// `BOOTC_UPDATE_REPO_WORKER` — registry the semver updater tracks.
    pub bootc_update_repo_worker: Option<&'a str>,
    /// Second-NIC MAC (`EXTRA_NET_WORKER_MACS[i]`) — when set, the
    /// runcmd block opens with the IPv6-off entry for that NIC (#407).
    pub net2_mac: Option<&'a str>,
}

/// Render worker cloud-config YAML (bash twin: `worker_user_data`).
pub(crate) fn render_worker_user_data(
    worker_name: &str,
    pubkey_contents: &str,
    join_cmd: &str,
    switch_to_ghcr: bool,
    ghcr_tag: &str,
    overrides: WorkerOverrides<'_>,
) -> String {
    let WorkerOverrides {
        bootc_update_schedule,
        bootc_update_repo_worker,
        net2_mac,
    } = overrides;
    let mut out = String::new();
    out.push_str("#cloud-config\n");
    out.push_str(&format!("hostname: {worker_name}\n"));
    out.push_str("disable_root: false\n");
    out.push_str("users:\n");
    out.push_str("  - name: root\n");
    out.push_str("    ssh_authorized_keys:\n");
    out.push_str(&format!("      - {pubkey_contents}\n"));
    out.push_str("write_files:\n");
    out.push_str("  - path: /etc/hummingbird/worker-join.env\n");
    out.push_str("    owner: root:root\n");
    out.push_str("    permissions: '0600'\n");
    out.push_str("    content: |\n");
    out.push_str(&format!("      {join_cmd}\n"));
    // bootc-semver-update overrides, appended to the join.env block that is
    // always present. Bash treats empty as unset, so normalize first.
    // Mirrors deploy-cluster.sh:177-194.
    let bootc_update_schedule = bootc_update_schedule.filter(|s| !s.is_empty());
    let bootc_update_repo_worker = bootc_update_repo_worker.filter(|s| !s.is_empty());
    let net2_mac = net2_mac.filter(|s| !s.is_empty());
    if let Some(schedule) = bootc_update_schedule {
        // Empty OnCalendar= first clears the image-baked default; see the
        // CP renderer for why the union semantics matter.
        out.push_str("  - path: /etc/systemd/system/bootc-semver-update.timer.d/schedule.conf\n");
        out.push_str("    owner: root:root\n");
        out.push_str("    permissions: '0644'\n");
        out.push_str("    content: |\n");
        out.push_str("      [Timer]\n");
        out.push_str("      OnCalendar=\n");
        out.push_str(&format!("      OnCalendar={schedule}\n"));
    }
    if let Some(repo) = bootc_update_repo_worker {
        out.push_str("  - path: /etc/hummingbird/bootc-update.env\n");
        out.push_str("    owner: root:root\n");
        out.push_str("    permissions: '0644'\n");
        out.push_str("    content: |\n");
        out.push_str(&format!("      REPO={repo}\n"));
        out.push_str("      PREFIX=v\n");
    }
    // Bash gate (line 195): SWITCH_TO_GHCR || BOOTC_UPDATE_SCHEDULE ||
    // net2_mac — the second NIC alone is enough to need a runcmd block.
    if switch_to_ghcr || bootc_update_schedule.is_some() || net2_mac.is_some() {
        out.push_str("runcmd:\n");
        // First: neutralise the second NIC's IPv6 (see the CP path).
        if let Some(mac) = net2_mac {
            out.push_str(&net2_ipv6_off_runcmd(mac));
        }
        if switch_to_ghcr {
            out.push_str(&format!(
                "  - [ bootc, switch, ghcr.io/aatchison/hummingbird-k8s-worker:{ghcr_tag} ]\n"
            ));
        }
        if bootc_update_schedule.is_some() {
            out.push_str("  - [ systemctl, daemon-reload ]\n");
            out.push_str("  - [ systemctl, restart, bootc-semver-update.timer ]\n");
        }
    }
    out
}

/// Build the shell script that writes a user-data YAML tmpfile and runs
/// cloud-localds / genisoimage / mkisofs to produce a seed ISO.
///
/// Mirrors `scripts/lib/cloud-init-seed.sh`.
///
/// `ud_tmp` is the path to the already-written user-data YAML on the remote.
fn cloud_init_seed_cmd(
    hostname: &str,
    ud_tmp: &str,
    out_iso: &str,
    net_cfg_tmp: Option<&str>,
) -> String {
    let hostname_q = sh_quote(hostname);
    let ud_q = sh_quote(ud_tmp);
    let iso_q = sh_quote(out_iso);
    // Optional network-config: NoCloud reads it from the ISO root as
    // `network-config`. cloud-localds takes it as a flag; the ISO tools
    // just get another file argument (bash twin: cloud-init-seed.sh's
    // nc_localds / nc_isofile arrays).
    let (stage_nc, nc_localds, nc_isofile, rm_nc) = match net_cfg_tmp {
        Some(nc) => {
            let nc_q = sh_quote(nc);
            (
                format!("cp {nc_q} \"$_tmp/network-config\"; "),
                " --network-config \"$_tmp/network-config\"".to_string(),
                " \"$_tmp/network-config\"".to_string(),
                format!("; rm -f -- {nc_q}"),
            )
        }
        None => (String::new(), String::new(), String::new(), String::new()),
    };
    format!(
        "set -euo pipefail; \
         _tmp=$(mktemp -d -t hbird-ci-XXXXXX); \
         cp {ud_q} \"$_tmp/user-data\"; \
         {stage_nc}printf 'instance-id: hbird-%s-%s\\nlocal-hostname: %s\\n' \
           \"$(date +%s)\" \"$$\" {hostname_q} > \"$_tmp/meta-data\"; \
         if command -v cloud-localds >/dev/null 2>&1; then \
           cloud-localds{nc_localds} {iso_q} \"$_tmp/user-data\" \"$_tmp/meta-data\"; \
         elif command -v genisoimage >/dev/null 2>&1; then \
           genisoimage -output {iso_q} -volid cidata -joliet -rock \
             \"$_tmp/user-data\" \"$_tmp/meta-data\"{nc_isofile} >/dev/null 2>&1; \
         elif command -v mkisofs >/dev/null 2>&1; then \
           mkisofs -output {iso_q} -volid cidata -joliet -rock \
             \"$_tmp/user-data\" \"$_tmp/meta-data\"{nc_isofile} >/dev/null 2>&1; \
         else \
           echo 'build_cloud_init_seed: need cloud-localds / genisoimage / mkisofs' >&2; exit 1; \
         fi; \
         rm -rf -- \"$_tmp\"; \
         rm -f -- {ud_q}{rm_nc}"
    )
}

// ---- Primary-NIC identity (#409) --------------------------------------------
//
// Bash twin: deploy-cluster.sh `derive_primary_mac` + `ensure_dhcp_reservation`
// plus the primary-MAC slice of the extra-network-validation block.

/// SHA-256 (FIPS 180-4), std-only. The workspace deliberately carries no
/// crypto crate; this is not a security boundary — it only has to produce
/// the SAME digest `sha256sum` produces so `derive_primary_mac` yields
/// byte-identical MACs to the bash twin (a rebuilt VM must land on the
/// lease the bash deploy reserved).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Padded message: data ++ 0x80 ++ zeros ++ 64-bit big-endian bit length.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 64];
    for chunk in msg.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// Deterministic MAC for a VM's PRIMARY NIC, derived from its domain
/// name. Bash twin: `derive_primary_mac` (deploy-cluster.sh #409).
///
/// WHY: the pinned kubelet `--node-ip` is a DHCP lease frozen at first
/// boot. Without a stable MAC a rebuilt VM gets a libvirt-random MAC,
/// hence a different lease, while the pinned `--node-ip` stays put —
/// leaving kubelet with an address on no interface. 52:54:00 is the
/// QEMU/KVM OUI libvirt itself uses; the low three bytes are the first
/// 6 hex digits of the name's sha256.
pub(crate) fn derive_primary_mac(name: &str) -> String {
    let h = sha256_hex(name.as_bytes());
    format!("52:54:00:{}:{}:{}", &h[0..2], &h[2..4], &h[4..6])
}

/// `aa:bb:cc:dd:ee:ff` shape check. Bash twin regex:
/// `^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$`.
pub(crate) fn is_valid_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Resolved primary-NIC MACs: operator override (`CP_MAC` /
/// `WORKER_MACS[i]`) or name-derived fallback, validated + collision-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrimaryMacs {
    /// CP primary MAC.
    pub cp: String,
    /// Worker primary MACs, parallel to `Plan::worker_names`.
    pub workers: Vec<String>,
}

/// Compute + validate the primary-NIC MAC family. Mirrors the
/// primary-MAC slice of deploy-cluster.sh's extra-network-validation
/// block (lines 713-726): malformed values and duplicate MACs are hard
/// failures BEFORE any side effects.
///
/// NOTE the bash twin does NOT length-validate WORKER_MACS against
/// WORKER_NAMES (unlike the EXTRA_NET_* arrays): a missing/empty entry
/// falls back to the name-derived MAC. Preserved here.
pub(crate) fn resolve_primary_macs(
    cp_name: &str,
    cp_mac: Option<&str>,
    worker_names: &[String],
    worker_macs: Option<&[String]>,
) -> Result<PrimaryMacs> {
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let cp = match cp_mac.filter(|s| !s.is_empty()) {
        Some(m) => m.to_string(),
        None => derive_primary_mac(cp_name),
    };
    if !is_valid_mac(&cp) {
        // Operator-visible wording mirrors the bash `fail` message.
        return Err(anyhow!(
            "CP_MAC is malformed (need aa:bb:cc:dd:ee:ff): '{cp}'"
        ));
    }
    seen.insert(cp.to_ascii_lowercase(), cp_name.to_string());

    let mut workers = Vec::with_capacity(worker_names.len());
    for (i, w) in worker_names.iter().enumerate() {
        let mac = match worker_macs.and_then(|m| m.get(i)).filter(|s| !s.is_empty()) {
            Some(m) => m.clone(),
            None => derive_primary_mac(w),
        };
        if !is_valid_mac(&mac) {
            return Err(anyhow!(
                "WORKER_MACS[{i}] is malformed (need aa:bb:cc:dd:ee:ff): '{mac}'"
            ));
        }
        if let Some(other) = seen.get(&mac.to_ascii_lowercase()) {
            return Err(anyhow!(
                "primary-NIC MAC collision: {w} and {other} would both use {mac} — set CP_MAC/WORKER_MACS explicitly to break the tie"
            ));
        }
        seen.insert(mac.to_ascii_lowercase(), w.clone());
        workers.push(mac);
    }
    Ok(PrimaryMacs { cp, workers })
}

/// The configured `WORKER_IPS[i]` entry, if present and non-empty.
/// Bash twin: `"${WORKER_IPS[$w_idx]:-}"` — a short/empty array entry
/// simply means "no reservation for this worker" (WORKER_IPS is not
/// length-validated, unlike the EXTRA_NET_* family).
fn worker_ip(plan: &Plan, i: usize) -> Option<&str> {
    plan.worker_ips
        .as_ref()
        .and_then(|v| v.get(i))
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}

/// Whether the operator explicitly set `WORKER_MACS[i]` (drives the
/// dry-run plan's `primary mac=` note; the live path always pins).
fn worker_mac_configured(plan: &Plan, i: usize) -> bool {
    plan.worker_macs
        .as_ref()
        .and_then(|v| v.get(i))
        .is_some_and(|s| !s.is_empty())
}

/// What `ensure_dhcp_reservation` should do, decided from the network's
/// current XML. Mirrors the two greps in the bash twin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReservationCheck {
    /// `mac='<mac>'` already present (case-insensitive, like `grep -qi`)
    /// — reservation exists, nothing to do.
    AlreadyPresent,
    /// `ip='<ip>'` present under a different MAC — reserving an address
    /// another host already holds is worse than no reservation; skip.
    IpTakenByOtherMac,
    /// Neither found — add the reservation.
    Absent,
}

/// Classify a `virsh net-dumpxml` payload for the (mac, ip) pair.
pub(crate) fn classify_dhcp_reservation(net_xml: &str, mac: &str, ip: &str) -> ReservationCheck {
    // Bash: `grep -qi "mac='${mac}'"` — case-insensitive.
    let xml_lower = net_xml.to_ascii_lowercase();
    if xml_lower.contains(&format!("mac='{}'", mac.to_ascii_lowercase())) {
        return ReservationCheck::AlreadyPresent;
    }
    // Bash: `grep -q "ip='${ip}'"` — case-sensitive (IPs have no case).
    if net_xml.contains(&format!("ip='{ip}'")) {
        return ReservationCheck::IpTakenByOtherMac;
    }
    ReservationCheck::Absent
}

/// Add a `<host mac ip>` entry to a libvirt network so a VM's primary
/// address is a RESERVATION, not merely a sticky lease. Idempotent, and
/// never fatal: a host that cannot net-update, or a network that already
/// carries the entry, must not abort a deploy. Bash twin:
/// `ensure_dhcp_reservation` (deploy-cluster.sh #409). Log wording is
/// preserved byte-for-byte — operators grep for these lines.
fn ensure_dhcp_reservation(conn: &Connection, net: &str, mac: &str, ip: &str, name: &str) {
    // Bash pipes net-dumpxml through `2>/dev/null | grep`: a failed dump
    // classifies as Absent and falls through to the add attempt (whose
    // failure is a WARN, not an abort). Same here.
    let net_xml = conn.net_dumpxml(net).unwrap_or_default();
    match classify_dhcp_reservation(&net_xml, mac, ip) {
        ReservationCheck::AlreadyPresent => {
            log(&format!(
                "DHCP reservation for {name} ({mac} -> {ip}) already present on network '{net}'"
            ));
        }
        ReservationCheck::IpTakenByOtherMac => {
            log(&format!(
                "WARN: {ip} is already reserved on network '{net}' under a different MAC — skipping reservation for {name}. Pick a free CP_IP/WORKER_IPS value or clear the stale entry."
            ));
        }
        ReservationCheck::Absent => match conn.net_update_add_ip_dhcp_host(net, mac, name, ip) {
            Ok(()) => log(&format!(
                "added DHCP reservation on '{net}': {name} {mac} -> {ip}"
            )),
            Err(_) => log(&format!(
                "WARN: could not add a DHCP reservation for {name} on '{net}' (continuing — the pinned MAC still makes the lease sticky)"
            )),
        },
    }
}

// ---- Optional second NIC (#405-#408) ----------------------------------------
//
// Bash twin: deploy-cluster.sh `render_net2_network_config`,
// `emit_net2_ipv6_off_runcmd`, and the extra-network-validation block.

/// Octet-range-accurate CIDR check. Bash twin `_cidr_re`: the loose
/// `^[0-9.]+/[0-9]+$` it replaced accepted `10.0.0.256/24`, `1.2.3/99`
/// and `10.0.0.241/244` — defeating the fail-early guarantee. No leading
/// zeros (matches the regex's alternation exactly).
pub(crate) fn is_valid_cidr(s: &str) -> bool {
    let Some((addr, prefix)) = s.split_once('/') else {
        return false;
    };
    let octets: Vec<&str> = addr.split('.').collect();
    if octets.len() != 4 || !octets.iter().all(|o| is_decimal_in_range(o, 255)) {
        return false;
    }
    is_decimal_in_range(prefix, 32)
}

/// 1-3 digit decimal, no leading zero (except `"0"` itself), `<= max`.
fn is_decimal_in_range(s: &str, max: u32) -> bool {
    if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if s.len() > 1 && s.starts_with('0') {
        return false;
    }
    s.parse::<u32>().is_ok_and(|v| v <= max)
}

/// `true` when `addr` (bare IPv4) falls inside `cidr`. Bash twin
/// `_ip_in_cidr`. Malformed inputs return `false` — callers validate
/// shape first.
pub(crate) fn ip_in_cidr(addr: &str, cidr: &str) -> bool {
    fn to_u32(a: &str) -> Option<u32> {
        let mut out: u32 = 0;
        let mut n = 0;
        for part in a.split('.') {
            out = (out << 8) | u32::from(part.parse::<u8>().ok()?);
            n += 1;
        }
        (n == 4).then_some(out)
    }
    let Some((base, prefix)) = cidr.split_once('/') else {
        return false;
    };
    let (Some(a), Some(c), Ok(p)) = (to_u32(addr), to_u32(base), prefix.parse::<u32>()) else {
        return false;
    };
    if p == 0 {
        return true;
    }
    if p > 32 {
        return false;
    }
    let mask: u32 = u32::MAX << (32 - p);
    (a & mask) == (c & mask)
}

/// Emit a cloud-init network-config (v2) YAML for a dual-NIC VM.
/// Byte-parity with bash `render_net2_network_config`.
///
/// Why network-config and not a write_files NM keyfile: this file is
/// rendered by cloud-init-local BEFORE NetworkManager starts. A keyfile
/// delivered via write_files (cloud-config stage) loses the race — NM
/// auto-defaults the new NIC with DHCP first, and if that network's DHCP
/// hands out a gateway the node grows a second default route and its
/// identity (apiserver advertise-address, kubelet node IP) can move. The
/// static stanza below has NO gateway and disables RA, so the second NIC
/// can never carry a default route. The primary NIC is matched by name
/// (enp1s0 — deterministic on the q35 machine type these VMs use) and
/// MUST be declared: providing ANY network-config disables cloud-init's
/// fallback DHCP config; omitting it would leave the primary NIC dead.
pub(crate) fn render_net2_network_config(mac: &str, ip: &str) -> String {
    let mut out = String::new();
    out.push_str("version: 2\n");
    out.push_str("ethernets:\n");
    out.push_str("  primary:\n");
    out.push_str("    match:\n");
    out.push_str("      name: enp1s0\n");
    out.push_str("    dhcp4: true\n");
    out.push_str("  net2:\n");
    out.push_str("    match:\n");
    out.push_str(&format!("      macaddress: \"{mac}\"\n"));
    out.push_str("    dhcp4: false\n");
    out.push_str("    dhcp6: false\n");
    out.push_str("    accept-ra: false\n");
    out.push_str("    addresses:\n");
    out.push_str(&format!("      - \"{ip}\"\n"));
    out
}

/// The runcmd entry that disables IPv6 on the second NIC. Byte-parity
/// with bash `emit_net2_ipv6_off_runcmd`.
///
/// WHY THIS EXISTS: `render_net2_network_config` emits `accept-ra: false`
/// and `dhcp6: false`, and cloud-init's NetworkManager renderer SILENTLY
/// DROPS BOTH — with no IPv6 subnet it never emits an `[ipv6]` section,
/// so NM normalizes the keyfile to ipv6.method=auto (verified on a live
/// node by the bash twin's author). Left alone, an EXTRA_NETWORK segment
/// carrying router advertisements would SLAAC an address onto the second
/// NIC and install an IPv6 default route there. Resolved MAC -> device ->
/// connection because the NM renderer names the connection after the
/// matched INTERFACE, not the network-config key. Non-fatal on lookup
/// failure: a missing NIC must not block the rest of first boot.
pub(crate) fn net2_ipv6_off_runcmd(mac: &str) -> String {
    let mut out = String::new();
    out.push_str("  - |\n");
    out.push_str(
        "    # Disable IPv6 on the second NIC (accept-ra/dhcp6 do not survive the NM renderer).\n",
    );
    out.push_str(&format!(
        "    dev=$(ip -o link | awk -v m={mac} 'tolower($0) ~ tolower(m) {{gsub(/:/,\"\",$2); print $2; exit}}')\n"
    ));
    out.push_str(&format!(
        "    [ -n \"$dev\" ] || {{ echo \"hbird: no interface with MAC {mac}\" >&2; exit 0; }}\n"
    ));
    out.push_str("    con=$(nmcli -g GENERAL.CONNECTION device show \"$dev\")\n");
    out.push_str(
        "    [ -n \"$con\" ] || { echo \"hbird: no NM connection on $dev\" >&2; exit 0; }\n",
    );
    out.push_str("    nmcli connection modify \"$con\" ipv6.method disabled\n");
    out.push_str("    nmcli connection up \"$con\" >/dev/null\n");
    out
}

/// The second-NIC identity for one VM, resolved from the validated
/// EXTRA_NET_* family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Net2 {
    /// Libvirt network name (`EXTRA_NETWORK`).
    pub network: String,
    /// Guest-visible MAC (`EXTRA_NET_*_MAC`).
    pub mac: String,
    /// Static address in CIDR form (`EXTRA_NET_*_IP`).
    pub ip: String,
}

/// Validate the whole EXTRA_NET_* knob family up front so a partial
/// config fails here, not as a half-provisioned VM. Pure (no libvirt
/// probes — see [`check_extra_network_on_host`] for those). Mirrors
/// deploy-cluster.sh lines 748-816; `fail` wording preserved.
///
/// Returns `(cp_net2, worker_net2s)` — `None` when EXTRA_NETWORK is off.
fn validate_extra_network(
    plan: &Plan,
    primary_macs: &PrimaryMacs,
) -> Result<Option<(Net2, Vec<Net2>)>> {
    let Some(net) = plan.extra_network.as_deref().filter(|s| !s.is_empty()) else {
        // Guard against a half-set family: worker arrays without the
        // master knob would be silently ignored — that silence is the
        // #373-family bug class.
        let half_set = plan
            .extra_net_cp_mac
            .as_deref()
            .is_some_and(|s| !s.is_empty())
            || plan
                .extra_net_cp_ip
                .as_deref()
                .is_some_and(|s| !s.is_empty())
            || plan
                .extra_net_worker_macs
                .as_ref()
                .is_some_and(|v| v.iter().any(|s| !s.is_empty()))
            || plan
                .extra_net_worker_ips
                .as_ref()
                .is_some_and(|v| v.iter().any(|s| !s.is_empty()));
        if half_set {
            return Err(anyhow!(
                "EXTRA_NET_* values are set but EXTRA_NETWORK is empty — set EXTRA_NETWORK=<libvirt network> or clear the family"
            ));
        }
        return Ok(None);
    };

    let cp_mac = plan.extra_net_cp_mac.as_deref().unwrap_or("");
    if !is_valid_mac(cp_mac) {
        return Err(anyhow!(
            "EXTRA_NETWORK is set but EXTRA_NET_CP_MAC is missing/malformed (need aa:bb:cc:dd:ee:ff): '{cp_mac}'"
        ));
    }
    let cp_ip = plan.extra_net_cp_ip.as_deref().unwrap_or("");
    if !is_valid_cidr(cp_ip) {
        return Err(anyhow!(
            "EXTRA_NETWORK is set but EXTRA_NET_CP_IP is missing/malformed (need CIDR like 10.0.0.241/24): '{cp_ip}'"
        ));
    }
    // Unset arrays count as 0 entries (bash defaults them to `()`), and
    // unlike WORKER_MACS these ARE length-validated: parallel or fail.
    let empty: Vec<String> = Vec::new();
    let w_macs = plan.extra_net_worker_macs.as_ref().unwrap_or(&empty);
    let w_ips = plan.extra_net_worker_ips.as_ref().unwrap_or(&empty);
    if w_macs.len() != plan.worker_names.len() {
        return Err(anyhow!(
            "EXTRA_NET_WORKER_MACS has {} entries but WORKER_NAMES has {} — the arrays must be parallel",
            w_macs.len(),
            plan.worker_names.len(),
        ));
    }
    if w_ips.len() != plan.worker_names.len() {
        return Err(anyhow!(
            "EXTRA_NET_WORKER_IPS has {} entries but WORKER_NAMES has {} — the arrays must be parallel",
            w_ips.len(),
            plan.worker_names.len(),
        ));
    }
    for (i, m) in w_macs.iter().enumerate() {
        if !is_valid_mac(m) {
            return Err(anyhow!("EXTRA_NET_WORKER_MACS[{i}] malformed: '{m}'"));
        }
    }
    for (i, a) in w_ips.iter().enumerate() {
        if !is_valid_cidr(a) {
            return Err(anyhow!("EXTRA_NET_WORKER_IPS[{i}] malformed: '{a}'"));
        }
    }

    // Uniqueness. A duplicate MAC or address is silently catastrophic:
    // two VMs on one L2 segment answering for the same identity.
    let mut primary_by_mac: std::collections::HashMap<String, &str> =
        std::collections::HashMap::new();
    primary_by_mac.insert(primary_macs.cp.to_ascii_lowercase(), &plan.cp_name);
    for (i, m) in primary_macs.workers.iter().enumerate() {
        primary_by_mac.insert(m.to_ascii_lowercase(), &plan.worker_names[i]);
    }
    let mut seen_mac: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in std::iter::once(cp_mac).chain(w_macs.iter().map(String::as_str)) {
        let mk = m.to_ascii_lowercase();
        if !seen_mac.insert(mk.clone()) {
            return Err(anyhow!(
                "duplicate MAC in the EXTRA_NET_* family: '{m}' — every NIC needs a unique MAC"
            ));
        }
        // Also cross-check against the primary NICs' MACs (#409).
        if let Some(owner) = primary_by_mac.get(&mk) {
            return Err(anyhow!(
                "EXTRA_NET MAC '{m}' collides with the primary-NIC MAC of {owner} — every NIC on the host needs a unique MAC"
            ));
        }
    }
    let mut seen_ip: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for a in std::iter::once(cp_ip).chain(w_ips.iter().map(String::as_str)) {
        let bare = a.split('/').next().unwrap_or(a);
        if !seen_ip.insert(bare) {
            return Err(anyhow!(
                "duplicate address in the EXTRA_NET_* family: '{bare}' — every NIC needs a unique address"
            ));
        }
    }

    // Overlap with the cluster's own ranges — the exact collision class
    // that motivated this work (Cilium's 10.0.0.0/8 default swallowing
    // the LAN).
    for a in std::iter::once(cp_ip).chain(w_ips.iter().map(String::as_str)) {
        let bare = a.split('/').next().unwrap_or(a);
        for (range_name, range) in [
            ("POD_CIDR", plan.pod_cidr.as_deref()),
            ("SERVICE_CIDR", plan.service_cidr.as_deref()),
        ] {
            let Some(range) = range.filter(|s| !s.is_empty()) else {
                continue;
            };
            if ip_in_cidr(bare, range) {
                return Err(anyhow!(
                    "EXTRA_NET address {bare} falls inside {range_name}={range} — traffic to it would be swallowed by the cluster network. Pick ranges that overlap neither."
                ));
            }
        }
    }

    let cp = Net2 {
        network: net.to_string(),
        mac: cp_mac.to_string(),
        ip: cp_ip.to_string(),
    };
    let workers = w_macs
        .iter()
        .zip(w_ips.iter())
        .map(|(m, a)| Net2 {
            network: net.to_string(),
            mac: m.clone(),
            ip: a.clone(),
        })
        .collect();
    Ok(Some((cp, workers)))
}

/// `Active:` line check on `virsh net-info` output. Bash twin:
/// `awk '/^Active:/{print $2}' | grep -qi '^yes$'`.
pub(crate) fn net_info_reports_active(net_info: &str) -> bool {
    net_info
        .lines()
        .find_map(|l| l.strip_prefix("Active:"))
        .map(str::trim)
        .is_some_and(|v| v.eq_ignore_ascii_case("yes"))
}

/// For a hostdev (SR-IOV VF pool) network, the number of VFs in the pool;
/// `None` for any other forward mode. Bash twin: the `grep -q "forward
/// mode='hostdev'"` + `grep -c "<address type='pci'"` pair.
pub(crate) fn hostdev_vf_count(net_xml: &str) -> Option<usize> {
    if !net_xml.contains("forward mode='hostdev'") {
        return None;
    }
    Some(net_xml.matches("<address type='pci'").count())
}

/// Live host-side EXTRA_NETWORK preflight: the named libvirt network
/// must exist, be active, and (for hostdev pools) have enough VFs for
/// 1 CP + N workers. Mirrors deploy-cluster.sh lines 795-809.
fn check_extra_network_on_host(plan: &Plan, conn: &Connection, net: &str) -> Result<()> {
    let info = conn.net_info(net).map_err(|_| {
        anyhow!(
            "EXTRA_NETWORK='{net}' is not a defined libvirt network on this host — define it first (see cluster.example.conf for a VF-pool example)"
        )
    })?;
    // Defined is not enough: an inactive network fails at virt-install
    // time, after the templates are built and the CP qcow2 is cloned.
    if !net_info_reports_active(&info) {
        return Err(anyhow!(
            "EXTRA_NETWORK='{net}' is defined but NOT active — run 'virsh net-start {net}' (and net-autostart) first"
        ));
    }
    // hostdev (SR-IOV VF) pool: ensure enough ports for 1 CP + N workers.
    // Running out otherwise surfaces as a cryptic virt-install failure on
    // the Nth VM, halfway through the deploy.
    let xml = conn.net_dumpxml(net).unwrap_or_default();
    if let Some(vf_count) = hostdev_vf_count(&xml) {
        let need = 1 + plan.worker_names.len();
        if vf_count < need {
            return Err(anyhow!(
                "EXTRA_NETWORK='{net}' is a hostdev pool with {vf_count} VF(s) but this deploy needs {need} (1 CP + {} worker(s))",
                plan.worker_names.len(),
            ));
        }
    }
    Ok(())
}

// ---- helpers ---------------------------------------------------------------

/// Derive the repo root from the current working directory.
///
/// Resolution order (the config file's path is intentionally NOT consulted —
/// it often lives outside the repo, which was the bug in #33):
///
/// 1. `git rev-parse --show-toplevel` in cwd — fast path when already inside
///    the repo checkout.
/// 2. Walk up from cwd looking for a directory that contains both `Makefile`
///    and a `containers/` subdirectory (the two hummingbird-k8s root markers).
/// 3. `"."` as a last resort — caller will get a clear `make` error rather
///    than a misleading path-resolution failure.
fn path_has_repo_markers(p: &std::path::Path) -> bool {
    p.join("Makefile").exists() && p.join("containers").is_dir()
}

pub(crate) fn find_repo_root() -> PathBuf {
    // Try git first — fastest and most authoritative.
    // Guard: only accept the git-returned toplevel if it actually has the
    // repo markers (Makefile + containers/). Running hbird from a different
    // or nested git repo would otherwise return the wrong toplevel.
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            let path = s.trim();
            if !path.is_empty() {
                let p = PathBuf::from(path);
                if path_has_repo_markers(&p) {
                    return p;
                }
            }
        }
    }

    // Walk up from cwd looking for Makefile + containers/ (repo root markers).
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir: &std::path::Path = &cwd;
        loop {
            if path_has_repo_markers(dir) {
                return dir.to_path_buf();
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
    }

    PathBuf::from(".")
}

// ---- Command-string builder functions (pure, unit-testable) ----------------

/// Single-quote a string for safe inclusion in a shell command.
///
/// Wraps `s` in `'...'`, escaping embedded single quotes via the
/// `'\''` idiom. Mirrors `lib/build-common.sh`'s quoting convention.
pub(crate) fn sh_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':'))
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

/// Build the optional podman storage flags prefix.
///
/// Returns `"--storage-driver <d> --root <r> --runroot <rr> "` (trailing
/// space) when any option is set, or `""` when all are absent.
/// Mirrors the bash twin's `$STORAGE_OPT_ARGS` construction.
fn podman_storage_prefix(plan: &Plan) -> String {
    let mut parts = Vec::new();
    if let Some(ref d) = plan.storage_driver {
        parts.push(format!("--storage-driver {}", sh_quote(d)));
    }
    if let Some(ref r) = plan.podman_root {
        parts.push(format!("--root {}", sh_quote(r)));
    }
    if let Some(ref rr) = plan.podman_runroot {
        parts.push(format!("--runroot {}", sh_quote(rr)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{} ", parts.join(" "))
    }
}

/// Build a `podman [storage-opts] pull <image_ref>` command string.
fn podman_pull_cmd(plan: &Plan, image_ref: &str) -> String {
    format!(
        "podman {}pull {}",
        podman_storage_prefix(plan),
        sh_quote(image_ref)
    )
}

/// Build the `ENABLE_CLOUD_INIT=1 make -C <repo_root> <target>` command.
pub(crate) fn make_image_cmd(repo_root: &std::path::Path, target: &str) -> String {
    format!(
        "ENABLE_CLOUD_INIT=1 make -C {} {}",
        sh_quote(&repo_root.display().to_string()),
        target
    )
}

/// Build the full `podman run` BIB command string.
///
/// Mirrors `lib/build-common.sh::build_qcow2` exactly (flag order, storage
/// opts, -e STORAGE_DRIVER when set, volume mounts, BIB args).
fn bib_run_cmd(plan: &Plan, image_ref: &str, bib_config_path: &str) -> String {
    let storage_opts = podman_storage_prefix(plan);
    // Determine the container storage source path: PODMAN_ROOT if set,
    // else the default `/var/lib/containers/storage`.
    let storage_src = plan
        .podman_root
        .as_deref()
        .unwrap_or("/var/lib/containers/storage");

    let mut cmd = format!(
        "podman {storage_opts}run --rm --privileged --pull=newer \
         --security-opt label=type:unconfined_t"
    );
    if let Some(ref d) = plan.storage_driver {
        cmd.push_str(&format!(" -e STORAGE_DRIVER={}", sh_quote(d)));
    }
    cmd.push_str(&format!(
        " -v {}:/config.toml:ro",
        sh_quote(bib_config_path)
    ));
    cmd.push_str(&format!(" -v {}:/output", sh_quote(&plan.pool_dir)));
    cmd.push_str(&format!(
        " -v {}:/var/lib/containers/storage",
        sh_quote(storage_src)
    ));
    cmd.push_str(&format!(" {}", sh_quote(&plan.bib)));
    cmd.push_str(&format!(
        " --type qcow2 --rootfs ext4 --local {}",
        sh_quote(image_ref)
    ));
    cmd
}

/// Build the `podman image inspect` command to read the OCI revision label.
fn podman_inspect_vcs_ref_cmd(plan: &Plan, image_ref: &str) -> String {
    format!(
        "podman {}image inspect --format \
         '{{{{ index .Config.Labels \"org.opencontainers.image.revision\" }}}}' {}",
        podman_storage_prefix(plan),
        sh_quote(image_ref)
    )
}

/// Build the `git diff --quiet` command used to probe if a Containerfile
/// has changed relative to `git_ref`.
///
/// Intended for future use in the cache-assess live path; exposed here
/// for unit-testability alongside the other command builders.
#[allow(dead_code)]
pub(crate) fn git_diff_cmd(
    repo_root: &std::path::Path,
    git_ref: &str,
    containerfile: &std::path::Path,
) -> String {
    format!(
        "git -C {} diff --quiet {} -- {}",
        sh_quote(&repo_root.display().to_string()),
        sh_quote(git_ref),
        sh_quote(&containerfile.display().to_string())
    )
}

/// Generate a bib TOML configuration with `core` and `root` users.
///
/// Emits two `[[customizations.user]]` stanzas, each with only the `key`
/// field set to `pubkey_contents`. Password, groups, and env-knobs
/// (VM_USER, ENABLE_ROOT_SSH) are not emitted — filed as a parity
/// follow-up.
///
/// Uses the `key` field (BIB's `UserCustomization.Key` — a single
/// authorized-key string). The earlier `ssh_authorized_keys = [...]` array
/// form caused BIB to reject the config with "unknown keys found"; `key`
/// is the correct field. Matches the bash twin's `_render_user_block`
/// which emits `key = """<pubkey>"""`. Fixed by S4-bug #33b.
///
/// `pubkey_contents` must not contain `"""` — true for all standard SSH
/// public key material.
pub(crate) fn render_bib_config(pubkey_contents: &str) -> String {
    // Triple-quoted TOML string (`"""..."""`) matches the bash twin's
    // `printf 'key = """%s"""\n'` output and tolerates embedded double-quotes
    // in unusual key material.
    format!(
        "[[customizations.user]]\nname = \"core\"\nkey = \"\"\"{pubkey_contents}\"\"\"\n\n[[customizations.user]]\nname = \"root\"\nkey = \"\"\"{pubkey_contents}\"\"\"\n"
    )
}

// ---- Image ref helpers (for tests + dry-run) --------------------------------

/// Return `(cp_ref, worker_ref)` image reference strings for a given source.
///
/// Extracted from `plan_image_acquisition` for independent unit-testing.
#[allow(dead_code)]
fn image_refs(image_source: &str, tag: &str) -> Option<(String, String)> {
    match image_source {
        "ghcr" => Some((
            format!("{}/aatchison/hummingbird-k8s:{tag}", Plan::GHCR_REGISTRY),
            format!(
                "{}/aatchison/hummingbird-k8s-worker:{tag}",
                Plan::GHCR_REGISTRY
            ),
        )),
        "forgejo" => Some((
            format!("{}/aatchison/hummingbird-k8s:{tag}", Plan::FORGEJO_REGISTRY),
            format!(
                "{}/aatchison/hummingbird-k8s-worker:{tag}",
                Plan::FORGEJO_REGISTRY
            ),
        )),
        "local" => Some((
            "localhost/hummingbird-k8s:latest".to_string(),
            "localhost/hummingbird-k8s-worker:latest".to_string(),
        )),
        _ => None,
    }
}

// ---- Tests -----------------------------------------------------------------

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

    fn default_args() -> DeployClusterArgs {
        DeployClusterArgs {
            config: PathBuf::from("/dev/null"),
            kvm_host: None,
            no_sudo: false,
            dry_run: true,
            bib: "quay.io/centos-bootc/bootc-image-builder:latest".to_string(),
            podman_root: None,
            podman_runroot: None,
            storage_driver: None,
            force_rebuild: false,
            strict_cache: false,
            repo_root: None,
            cp_ready_retries: 60,
            cp_ready_sleep_secs: 10,
            token_ttl: "2h".to_string(),
        }
    }

    fn minimal_plan() -> Plan {
        Plan::from_args(&default_args(), cfg(None)).expect("plan")
    }

    // ---- #409 primary-NIC identity tests ------------------------------------

    #[test]
    fn sha256_hex_matches_reference_vectors() {
        // FIPS 180-4 vectors — same digests `sha256sum` prints.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // >1 block (64+ bytes) exercises the multi-chunk path.
        assert_eq!(
            sha256_hex(&[b'a'; 100]),
            "2816597888e4a0d3a36b82b83316ab32680eb8f00f8cd3b904d681246d285a0e"
        );
    }

    #[test]
    fn derive_primary_mac_matches_bash_twin() {
        // bash: printf hbird-cp1 | sha256sum | cut -c1-6 -> f01dbd
        assert_eq!(derive_primary_mac("hbird-cp1"), "52:54:00:f0:1d:bd");
        assert_eq!(derive_primary_mac("hbird-w1"), "52:54:00:4e:09:78");
    }

    #[test]
    fn is_valid_mac_accepts_and_rejects() {
        assert!(is_valid_mac("52:54:00:aa:bb:cc"));
        assert!(is_valid_mac("52:54:00:AA:BB:CC"));
        assert!(!is_valid_mac("52:54:00:aa:bb"));
        assert!(!is_valid_mac("52:54:00:aa:bb:cc:dd"));
        assert!(!is_valid_mac("52:54:00:aa:bb:cg"));
        assert!(!is_valid_mac("525400aabbcc"));
        assert!(!is_valid_mac(""));
    }

    #[test]
    fn resolve_primary_macs_derives_when_unset() {
        let macs = resolve_primary_macs("hbird-cp1", None, &["hbird-w1".to_string()], None)
            .expect("derived family is valid");
        assert_eq!(macs.cp, "52:54:00:f0:1d:bd");
        assert_eq!(macs.workers, vec!["52:54:00:4e:09:78".to_string()]);
    }

    #[test]
    fn resolve_primary_macs_prefers_operator_overrides() {
        let worker_macs = vec!["02:00:00:00:00:02".to_string()];
        let macs = resolve_primary_macs(
            "hbird-cp1",
            Some("02:00:00:00:00:01"),
            &["hbird-w1".to_string()],
            Some(&worker_macs),
        )
        .expect("override family is valid");
        assert_eq!(macs.cp, "02:00:00:00:00:01");
        assert_eq!(macs.workers, vec!["02:00:00:00:00:02".to_string()]);
    }

    #[test]
    fn resolve_primary_macs_short_worker_array_falls_back_to_derived() {
        // Bash does NOT length-validate WORKER_MACS: `${WORKER_MACS[$i]:-derive}`.
        let worker_macs = vec!["02:00:00:00:00:02".to_string()];
        let macs = resolve_primary_macs(
            "hbird-cp1",
            None,
            &["hbird-w1".to_string(), "hbird-w2".to_string()],
            Some(&worker_macs),
        )
        .expect("short WORKER_MACS is allowed");
        assert_eq!(macs.workers[0], "02:00:00:00:00:02");
        assert_eq!(macs.workers[1], derive_primary_mac("hbird-w2"));
    }

    #[test]
    fn resolve_primary_macs_rejects_malformed_cp_mac() {
        let err = resolve_primary_macs("cp", Some("not-a-mac"), &[], None)
            .expect_err("malformed CP_MAC must fail");
        assert!(
            err.to_string()
                .contains("CP_MAC is malformed (need aa:bb:cc:dd:ee:ff): 'not-a-mac'"),
            "err: {err}"
        );
    }

    #[test]
    fn resolve_primary_macs_rejects_malformed_worker_mac_with_index() {
        let worker_macs = vec!["02:00:00:00:00:01".to_string(), "bogus".to_string()];
        let err = resolve_primary_macs(
            "cp",
            None,
            &["w1".to_string(), "w2".to_string()],
            Some(&worker_macs),
        )
        .expect_err("malformed WORKER_MACS[1] must fail");
        assert!(
            err.to_string()
                .contains("WORKER_MACS[1] is malformed (need aa:bb:cc:dd:ee:ff): 'bogus'"),
            "err: {err}"
        );
    }

    #[test]
    fn resolve_primary_macs_rejects_collision_case_insensitively() {
        // Same MAC, different case — one L2 segment, one identity.
        let worker_macs = vec!["02:AA:BB:CC:DD:EE".to_string()];
        let err = resolve_primary_macs(
            "hbird-cp1",
            Some("02:aa:bb:cc:dd:ee"),
            &["hbird-w1".to_string()],
            Some(&worker_macs),
        )
        .expect_err("duplicate MAC must fail");
        let msg = err.to_string();
        assert!(msg.contains("primary-NIC MAC collision"), "err: {msg}");
        assert!(msg.contains("hbird-w1"), "err: {msg}");
        assert!(msg.contains("hbird-cp1"), "err: {msg}");
        assert!(
            msg.contains("set CP_MAC/WORKER_MACS explicitly to break the tie"),
            "err: {msg}"
        );
    }

    // ---- classify_dhcp_reservation (bash ensure_dhcp_reservation greps) -----

    const NET_XML: &str = r#"<network>
  <name>default</name>
  <mac address='52:54:00:99:99:99'/>
  <ip address='192.168.122.1' netmask='255.255.255.0'>
    <dhcp>
      <range start='192.168.122.2' end='192.168.122.254'/>
      <host mac='52:54:00:F0:1D:BD' name='hbird-cp1' ip='192.168.122.10'/>
    </dhcp>
  </ip>
</network>"#;

    #[test]
    fn classify_reservation_same_mac_is_already_present_case_insensitive() {
        // Bash `grep -qi "mac='${mac}'"` — the XML stores the MAC
        // uppercase here, the config supplies lowercase.
        assert_eq!(
            classify_dhcp_reservation(NET_XML, "52:54:00:f0:1d:bd", "192.168.122.10"),
            ReservationCheck::AlreadyPresent,
        );
    }

    #[test]
    fn classify_reservation_ip_under_other_mac_is_conflict() {
        assert_eq!(
            classify_dhcp_reservation(NET_XML, "52:54:00:00:00:01", "192.168.122.10"),
            ReservationCheck::IpTakenByOtherMac,
        );
    }

    #[test]
    fn classify_reservation_absent_pair_wants_add() {
        assert_eq!(
            classify_dhcp_reservation(NET_XML, "52:54:00:00:00:01", "192.168.122.11"),
            ReservationCheck::Absent,
        );
    }

    #[test]
    fn classify_reservation_ip_match_is_exact_not_prefix() {
        // The grep pattern includes the closing quote: ip='192.168.122.1'
        // must NOT match the ...122.10 host entry (or the <ip address=...>
        // element, which uses a different attribute name).
        assert_eq!(
            classify_dhcp_reservation(NET_XML, "52:54:00:00:00:01", "192.168.122.1"),
            ReservationCheck::Absent,
        );
    }

    #[test]
    fn classify_reservation_empty_xml_wants_add() {
        // A failed `net-dumpxml` classifies as Absent — the add attempt's
        // failure is a WARN, never an abort (bash `2>/dev/null | grep`).
        assert_eq!(
            classify_dhcp_reservation("", "52:54:00:00:00:01", "10.0.0.5"),
            ReservationCheck::Absent,
        );
    }

    // ---- #405-#408 second-NIC tests ------------------------------------------

    #[test]
    fn render_net2_network_config_matches_bash_byte_for_byte() {
        // Expected block captured from the bash twin:
        //   render_net2_network_config 02:11:22:33:44:55 10.0.0.241/24
        let expected = "version: 2\n\
                        ethernets:\n\
                        \x20 primary:\n\
                        \x20   match:\n\
                        \x20     name: enp1s0\n\
                        \x20   dhcp4: true\n\
                        \x20 net2:\n\
                        \x20   match:\n\
                        \x20     macaddress: \"02:11:22:33:44:55\"\n\
                        \x20   dhcp4: false\n\
                        \x20   dhcp6: false\n\
                        \x20   accept-ra: false\n\
                        \x20   addresses:\n\
                        \x20     - \"10.0.0.241/24\"\n";
        assert_eq!(
            render_net2_network_config("02:11:22:33:44:55", "10.0.0.241/24"),
            expected,
        );
    }

    #[test]
    fn net2_ipv6_off_runcmd_matches_bash_byte_for_byte() {
        // Expected block captured from the bash twin:
        //   emit_net2_ipv6_off_runcmd 02:11:22:33:44:55
        let expected = concat!(
            "  - |\n",
            "    # Disable IPv6 on the second NIC (accept-ra/dhcp6 do not survive the NM renderer).\n",
            "    dev=$(ip -o link | awk -v m=02:11:22:33:44:55 'tolower($0) ~ tolower(m) {gsub(/:/,\"\",$2); print $2; exit}')\n",
            "    [ -n \"$dev\" ] || { echo \"hbird: no interface with MAC 02:11:22:33:44:55\" >&2; exit 0; }\n",
            "    con=$(nmcli -g GENERAL.CONNECTION device show \"$dev\")\n",
            "    [ -n \"$con\" ] || { echo \"hbird: no NM connection on $dev\" >&2; exit 0; }\n",
            "    nmcli connection modify \"$con\" ipv6.method disabled\n",
            "    nmcli connection up \"$con\" >/dev/null\n",
        );
        assert_eq!(net2_ipv6_off_runcmd("02:11:22:33:44:55"), expected);
    }

    #[test]
    fn cp_user_data_opens_runcmd_with_ipv6_off_when_net2_set() {
        let ud = render_cp_user_data(
            "hbird-cp1",
            "k",
            true,
            "v0.1.0",
            true,
            CpOverrides {
                net2_mac: Some("02:11:22:33:44:55"),
                ..Default::default()
            },
        );
        let runcmd_pos = ud.find("runcmd:\n").expect("runcmd block");
        // The IPv6-off entry must be the FIRST runcmd item (bash puts it
        // before the bootc switch so it runs before any network egress).
        assert_eq!(
            &ud[runcmd_pos + "runcmd:\n".len()..runcmd_pos + "runcmd:\n".len() + 6],
            "  - |\n",
        );
        assert!(ud.contains("no interface with MAC 02:11:22:33:44:55"));
        let switch_pos = ud.find("bootc, switch").expect("switch entry");
        assert!(
            runcmd_pos < switch_pos && ud.find("  - |").expect("ipv6 entry") < switch_pos,
            "ipv6-off must precede the bootc switch:\n{ud}"
        );
    }

    #[test]
    fn worker_user_data_net2_mac_alone_triggers_runcmd() {
        // Bash gate (line 195): net2_mac is a runcmd trigger on its own.
        let ud = render_worker_user_data(
            "hbird-w1",
            "k",
            "kubeadm join ...",
            false,
            "v0.1.0",
            WorkerOverrides {
                net2_mac: Some("02:11:22:33:44:56"),
                ..Default::default()
            },
        );
        assert!(ud.contains("runcmd:\n"), "runcmd must be emitted:\n{ud}");
        assert!(ud.contains("no interface with MAC 02:11:22:33:44:56"));
        // And without it (all overrides off, switch off) — no runcmd.
        let ud_off = render_worker_user_data(
            "hbird-w1",
            "k",
            "kubeadm join ...",
            false,
            "v0.1.0",
            WorkerOverrides::default(),
        );
        assert!(!ud_off.contains("runcmd:"), "no runcmd expected:\n{ud_off}");
    }

    #[test]
    fn cloud_init_seed_cmd_with_network_config_feeds_all_tool_branches() {
        let cmd = cloud_init_seed_cmd(
            "hbird-cp1",
            "/tmp/ud.yaml",
            "/mnt/pool/hbird-cp1-seed.iso",
            Some("/tmp/nc.yaml"),
        );
        // Staged into the ISO root as `network-config` (NoCloud contract).
        assert!(
            cmd.contains("cp /tmp/nc.yaml \"$_tmp/network-config\""),
            "cmd: {cmd}"
        );
        // cloud-localds takes it as a flag…
        assert!(
            cmd.contains("cloud-localds --network-config \"$_tmp/network-config\""),
            "cmd: {cmd}"
        );
        // …the ISO tools as another file argument.
        assert_eq!(
            cmd.matches("\"$_tmp/meta-data\" \"$_tmp/network-config\"")
                .count(),
            2,
            "genisoimage + mkisofs branches must both carry the file: {cmd}"
        );
        // And the remote tmpfile is cleaned up.
        assert!(
            cmd.trim_end()
                .ends_with("rm -f -- /tmp/ud.yaml; rm -f -- /tmp/nc.yaml"),
            "cmd: {cmd}"
        );
    }

    #[test]
    fn cloud_init_seed_cmd_without_network_config_is_unchanged() {
        let cmd = cloud_init_seed_cmd("h", "/tmp/ud.yaml", "/tmp/out.iso", None);
        assert!(!cmd.contains("network-config"), "cmd: {cmd}");
    }

    // ---- CIDR helpers --------------------------------------------------------

    #[test]
    fn is_valid_cidr_matches_bash_regex_semantics() {
        for ok in [
            "10.0.0.241/24",
            "0.0.0.0/0",
            "255.255.255.255/32",
            "192.168.1.0/9",
        ] {
            assert!(is_valid_cidr(ok), "{ok} should be valid");
        }
        for bad in [
            "10.0.0.256/24", // octet out of range — the exact bug the strict regex fixed
            "1.2.3/99",
            "10.0.0.241/244",
            "10.0.0.241",
            "10.0.0.01/24", // leading zero — regex alternation rejects
            "10.0.0.1/033",
            "a.b.c.d/24",
            "",
        ] {
            assert!(!is_valid_cidr(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn ip_in_cidr_matches_bash_arithmetic() {
        assert!(ip_in_cidr("10.244.3.7", "10.244.0.0/16"));
        assert!(!ip_in_cidr("10.245.0.1", "10.244.0.0/16"));
        assert!(ip_in_cidr("10.0.0.241", "0.0.0.0/0")); // p==0 → everything
        assert!(ip_in_cidr("192.168.122.10", "192.168.122.10/32"));
        assert!(!ip_in_cidr("192.168.122.11", "192.168.122.10/32"));
    }

    // ---- validate_extra_network branches --------------------------------------

    /// Plan with a full, coherent EXTRA_NET family for 1 worker.
    fn plan_with_extra_net() -> Plan {
        let mut plan = Plan::from_args(&default_args(), cfg(Some(vec!["hbird-w1"]))).expect("plan");
        plan.extra_network = Some("vf-pool".to_string());
        plan.extra_net_cp_mac = Some("02:11:22:33:44:55".to_string());
        plan.extra_net_cp_ip = Some("10.0.0.241/24".to_string());
        plan.extra_net_worker_macs = Some(vec!["02:11:22:33:44:56".to_string()]);
        plan.extra_net_worker_ips = Some(vec!["10.0.0.242/24".to_string()]);
        plan
    }

    fn macs_for(plan: &Plan) -> PrimaryMacs {
        resolve_primary_macs(
            &plan.cp_name,
            plan.cp_mac.as_deref(),
            &plan.worker_names,
            plan.worker_macs.as_deref(),
        )
        .expect("primary macs")
    }

    #[test]
    fn validate_extra_network_off_and_clean_returns_none() {
        let plan = minimal_plan();
        let macs = macs_for(&plan);
        assert!(validate_extra_network(&plan, &macs).expect("ok").is_none());
    }

    #[test]
    fn validate_extra_network_happy_path_returns_family() {
        let plan = plan_with_extra_net();
        let macs = macs_for(&plan);
        let (cp, workers) = validate_extra_network(&plan, &macs)
            .expect("valid family")
            .expect("family present");
        assert_eq!(cp.network, "vf-pool");
        assert_eq!(cp.mac, "02:11:22:33:44:55");
        assert_eq!(cp.ip, "10.0.0.241/24");
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].mac, "02:11:22:33:44:56");
    }

    #[test]
    fn validate_extra_network_half_set_family_fails() {
        let mut plan = minimal_plan();
        plan.extra_net_cp_mac = Some("02:11:22:33:44:55".to_string());
        let macs = macs_for(&plan);
        let err = validate_extra_network(&plan, &macs).expect_err("half-set must fail");
        assert!(
            err.to_string().contains(
                "EXTRA_NET_* values are set but EXTRA_NETWORK is empty — set EXTRA_NETWORK=<libvirt network> or clear the family"
            ),
            "err: {err}"
        );
    }

    #[test]
    fn validate_extra_network_missing_cp_mac_fails() {
        let mut plan = plan_with_extra_net();
        plan.extra_net_cp_mac = None;
        let macs = macs_for(&plan);
        let err = validate_extra_network(&plan, &macs).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("EXTRA_NETWORK is set but EXTRA_NET_CP_MAC is missing/malformed"),
            "err: {err}"
        );
    }

    #[test]
    fn validate_extra_network_malformed_cp_ip_fails() {
        let mut plan = plan_with_extra_net();
        plan.extra_net_cp_ip = Some("10.0.0.256/24".to_string());
        let macs = macs_for(&plan);
        let err = validate_extra_network(&plan, &macs).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("EXTRA_NET_CP_IP is missing/malformed (need CIDR like 10.0.0.241/24): '10.0.0.256/24'"),
            "err: {err}"
        );
    }

    #[test]
    fn validate_extra_network_unparallel_arrays_fail() {
        let mut plan = plan_with_extra_net();
        plan.extra_net_worker_macs = Some(vec![]);
        let macs = macs_for(&plan);
        let err = validate_extra_network(&plan, &macs).expect_err("must fail");
        assert!(
            err.to_string().contains(
                "EXTRA_NET_WORKER_MACS has 0 entries but WORKER_NAMES has 1 — the arrays must be parallel"
            ),
            "err: {err}"
        );
    }

    #[test]
    fn validate_extra_network_duplicate_mac_fails_case_insensitively() {
        let mut plan = plan_with_extra_net();
        plan.extra_net_worker_macs = Some(vec!["02:11:22:33:44:55".to_uppercase()]);
        let macs = macs_for(&plan);
        let err = validate_extra_network(&plan, &macs).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("duplicate MAC in the EXTRA_NET_* family"),
            "err: {err}"
        );
    }

    #[test]
    fn validate_extra_network_collision_with_primary_mac_fails() {
        let mut plan = plan_with_extra_net();
        // Point the CP's second NIC at the worker's (derived) primary MAC.
        plan.extra_net_cp_mac = Some(derive_primary_mac("hbird-w1"));
        let macs = macs_for(&plan);
        let err = validate_extra_network(&plan, &macs).expect_err("must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("collides with the primary-NIC MAC of hbird-w1"),
            "err: {msg}"
        );
    }

    #[test]
    fn validate_extra_network_duplicate_bare_ip_fails() {
        let mut plan = plan_with_extra_net();
        // Same bare address, different prefix — still a duplicate.
        plan.extra_net_worker_ips = Some(vec!["10.0.0.241/16".to_string()]);
        let macs = macs_for(&plan);
        let err = validate_extra_network(&plan, &macs).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("duplicate address in the EXTRA_NET_* family: '10.0.0.241'"),
            "err: {err}"
        );
    }

    #[test]
    fn validate_extra_network_overlap_with_pod_cidr_fails() {
        let mut plan = plan_with_extra_net();
        plan.pod_cidr = Some("10.0.0.0/8".to_string());
        let macs = macs_for(&plan);
        let err = validate_extra_network(&plan, &macs).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("EXTRA_NET address 10.0.0.241 falls inside POD_CIDR=10.0.0.0/8"),
            "err: {err}"
        );
    }

    // ---- host-side EXTRA_NETWORK probe parsers --------------------------------

    #[test]
    fn net_info_active_parses_yes_no_and_garbage() {
        assert!(net_info_reports_active(
            "Name:           vf-pool\nUUID:           x\nActive:         yes\n"
        ));
        assert!(!net_info_reports_active(
            "Name:           vf-pool\nActive:         no\n"
        ));
        assert!(!net_info_reports_active(""));
        assert!(!net_info_reports_active("Name: x\n"));
    }

    #[test]
    fn hostdev_vf_count_only_counts_hostdev_pools() {
        let hostdev = "<network>\n  <forward mode='hostdev' managed='yes'>\n    <address type='pci' domain='0x0000' bus='0x03' slot='0x10' function='0x0'/>\n    <address type='pci' domain='0x0000' bus='0x03' slot='0x10' function='0x2'/>\n  </forward>\n</network>";
        assert_eq!(hostdev_vf_count(hostdev), Some(2));
        let nat = "<network>\n  <forward mode='nat'/>\n</network>";
        assert_eq!(hostdev_vf_count(nat), None);
    }

    #[test]
    fn worker_ip_helper_treats_short_or_empty_entries_as_none() {
        let mut plan = minimal_plan();
        plan.worker_names = vec!["w1".to_string(), "w2".to_string()];
        plan.worker_ips = Some(vec!["192.168.122.21".to_string(), String::new()]);
        assert_eq!(worker_ip(&plan, 0), Some("192.168.122.21"));
        assert_eq!(worker_ip(&plan, 1), None); // empty entry
        plan.worker_ips = Some(vec!["192.168.122.21".to_string()]);
        assert_eq!(worker_ip(&plan, 1), None); // short array
        plan.worker_ips = None;
        assert_eq!(worker_ip(&plan, 0), None);
    }

    #[test]
    fn plan_carries_worker_default_when_unset() {
        let plan = Plan::from_args(&default_args(), cfg(None)).expect("plan");
        // CP_NAME=hbird-cp1 → workers default to (hbird-cp1-w1, hbird-cp1-w2)
        assert_eq!(plan.worker_names, vec!["hbird-cp1-w1", "hbird-cp1-w2"]);
    }

    #[test]
    fn plan_honors_explicit_empty_workers() {
        let plan = Plan::from_args(&default_args(), cfg(Some(vec![]))).expect("plan");
        assert!(plan.worker_names.is_empty());
    }

    // ---- image ref tests ---------------------------------------------------

    #[test]
    fn image_refs_ghcr_uses_ghcr_host() {
        let (cp, worker) = image_refs("ghcr", "v0.42.0").expect("ghcr refs");
        assert!(
            cp.starts_with("ghcr.io/"),
            "CP ref should use ghcr.io: {cp}"
        );
        assert!(
            worker.starts_with("ghcr.io/"),
            "worker ref should use ghcr.io: {worker}"
        );
        assert!(cp.contains("hummingbird-k8s:v0.42.0"));
        assert!(worker.contains("hummingbird-k8s-worker:v0.42.0"));
    }

    #[test]
    fn image_refs_forgejo_uses_forgejo_registry() {
        let (cp, worker) = image_refs("forgejo", "v1.0.0").expect("forgejo refs");
        assert!(
            cp.starts_with("forgejo.atchison.io/"),
            "CP ref should use forgejo.atchison.io: {cp}"
        );
        assert!(
            worker.starts_with("forgejo.atchison.io/"),
            "worker ref should use forgejo.atchison.io: {worker}"
        );
        assert!(cp.contains("hummingbird-k8s:v1.0.0"));
        assert!(worker.contains("hummingbird-k8s-worker:v1.0.0"));
    }

    #[test]
    fn image_refs_local_uses_localhost() {
        let (cp, worker) = image_refs("local", "ignored-tag").expect("local refs");
        assert_eq!(cp, "localhost/hummingbird-k8s:latest");
        assert_eq!(worker, "localhost/hummingbird-k8s-worker:latest");
    }

    // ---- command builder tests ---------------------------------------------

    #[test]
    fn podman_pull_cmd_no_storage_opts() {
        let plan = minimal_plan();
        let cmd = podman_pull_cmd(&plan, "ghcr.io/aatchison/hummingbird-k8s:v0.1.0");
        // No storage flags — just `podman pull <ref>`.
        assert_eq!(cmd, "podman pull ghcr.io/aatchison/hummingbird-k8s:v0.1.0");
    }

    #[test]
    fn podman_pull_cmd_with_storage_opts() {
        let args = DeployClusterArgs {
            storage_driver: Some("overlay".to_string()),
            podman_root: Some("/mnt/podman/root".to_string()),
            podman_runroot: Some("/mnt/podman/runroot".to_string()),
            ..default_args()
        };
        let plan = Plan::from_args(&args, cfg(None)).expect("plan");
        let cmd = podman_pull_cmd(&plan, "ghcr.io/aatchison/hummingbird-k8s:v0.1.0");
        assert!(cmd.contains("--storage-driver overlay"), "cmd: {cmd}");
        assert!(cmd.contains("--root /mnt/podman/root"), "cmd: {cmd}");
        assert!(cmd.contains("--runroot /mnt/podman/runroot"), "cmd: {cmd}");
        assert!(
            cmd.ends_with("ghcr.io/aatchison/hummingbird-k8s:v0.1.0"),
            "cmd: {cmd}"
        );
    }

    #[test]
    fn bib_run_cmd_shape_matches_bash_twin() {
        let plan = minimal_plan();
        let cmd = bib_run_cmd(
            &plan,
            "ghcr.io/aatchison/hummingbird-k8s:v0.1.0",
            "/tmp/bib-config.toml",
        );
        // Must contain the required BIB flags.
        assert!(cmd.contains("--rm --privileged --pull=newer"), "cmd: {cmd}");
        assert!(
            cmd.contains("--security-opt label=type:unconfined_t"),
            "cmd: {cmd}"
        );
        assert!(cmd.contains("/config.toml:ro"), "cmd: {cmd}");
        assert!(cmd.contains("/output"), "cmd: {cmd}");
        assert!(cmd.contains("/var/lib/containers/storage"), "cmd: {cmd}");
        assert!(
            cmd.contains("--type qcow2 --rootfs ext4 --local"),
            "cmd: {cmd}"
        );
        assert!(
            cmd.contains("ghcr.io/aatchison/hummingbird-k8s:v0.1.0"),
            "cmd: {cmd}"
        );
        assert!(
            cmd.contains("quay.io/centos-bootc/bootc-image-builder:latest"),
            "cmd: {cmd}"
        );
    }

    #[test]
    fn render_bib_config_contains_core_and_root_users() {
        let toml = render_bib_config("ssh-ed25519 AAAA... user@host");
        assert!(toml.contains("[[customizations.user]]"), "toml: {toml}");
        assert!(toml.contains("name = \"core\""), "toml: {toml}");
        assert!(toml.contains("name = \"root\""), "toml: {toml}");
        // Must use `key` (BIB schema field), NOT `ssh_authorized_keys`
        // (which BIB rejects as "unknown key" — S4 bug #33b).
        assert!(toml.contains("key = "), "must use 'key' field: {toml}");
        assert!(
            !toml.contains("ssh_authorized_keys"),
            "must NOT use ssh_authorized_keys (BIB rejects it): {toml}"
        );
    }

    #[test]
    fn render_bib_config_embeds_pubkey() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI test-key";
        let toml = render_bib_config(key);
        assert!(
            toml.contains(key),
            "pubkey must appear in rendered TOML; toml: {toml}"
        );
        // Should appear twice: once for core, once for root.
        assert_eq!(
            toml.matches(key).count(),
            2,
            "key must appear for both users"
        );
    }

    /// BIB config must parse as valid TOML and carry the correct `key` field
    /// per user entry. Before the fix, `ssh_authorized_keys` (an unknown BIB
    /// field) was used, causing BIB to reject the config with
    /// "unknown keys found: [customizations.user.ssh_authorized_keys
    ///  customizations.user.ssh_authorized_keys]". (S4 bug #33b.)
    #[test]
    fn render_bib_config_parses_as_valid_toml_with_key_per_user() {
        let pubkey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI testuser@testhost";
        let rendered = render_bib_config(pubkey);

        // Must parse as valid TOML without errors.
        let val: toml::Value =
            toml::from_str(&rendered).expect("render_bib_config must produce valid TOML");

        // Must have a customizations.user array with exactly two entries.
        let users = val
            .get("customizations")
            .and_then(|c| c.get("user"))
            .and_then(|u| u.as_array())
            .expect("customizations.user must be a TOML array");
        assert_eq!(
            users.len(),
            2,
            "must have exactly 2 user entries (core + root)"
        );

        // Each entry must have `key` (BIB's authorized-key field) and must NOT
        // have `ssh_authorized_keys` (the field BIB rejects as unknown).
        for user in users {
            let name = user
                .get("name")
                .and_then(|n| n.as_str())
                .expect("user entry must have a name");
            let key_val = user
                .get("key")
                .and_then(|k| k.as_str())
                .unwrap_or_else(|| panic!("user '{name}' must have a 'key' field"));
            assert!(
                key_val.contains(pubkey),
                "user '{name}' key must contain the pubkey"
            );
            assert!(
                user.get("ssh_authorized_keys").is_none(),
                "user '{name}' must NOT have ssh_authorized_keys (BIB rejects it as unknown)"
            );
        }
    }

    // ---- S2c helper tests --------------------------------------------------

    #[test]
    fn derive_privkey_path_strips_pub_suffix() {
        assert_eq!(
            derive_privkey_path("/home/user/.ssh/id_ed25519.pub"),
            "/home/user/.ssh/id_ed25519"
        );
        // Without .pub suffix: returned unchanged.
        assert_eq!(
            derive_privkey_path("/home/user/.ssh/id_ed25519"),
            "/home/user/.ssh/id_ed25519"
        );
        // Empty string.
        assert_eq!(derive_privkey_path(""), "");
    }

    #[test]
    fn cp_ssh_cmd_shape() {
        let cmd = cp_ssh_cmd(
            "/home/user/.ssh/id_ed25519",
            "192.168.122.42",
            "kubectl get nodes",
        );
        assert!(cmd.contains("ssh"), "must invoke ssh: {cmd}");
        assert!(cmd.contains("-i"), "must have identity flag: {cmd}");
        assert!(cmd.contains("id_ed25519"), "must contain privkey: {cmd}");
        assert!(
            cmd.contains("StrictHostKeyChecking=no"),
            "must disable strict host checking: {cmd}"
        );
        assert!(
            cmd.contains("root@192.168.122.42"),
            "must target root@ip: {cmd}"
        );
        assert!(
            cmd.contains("kubectl get nodes"),
            "must embed remote cmd: {cmd}"
        );
    }

    #[test]
    fn render_cp_user_data_hostname_and_root_user() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "ssh-ed25519 AAAA test-key",
            false,
            "v0.1.0",
            false,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: None,
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(
            yaml.starts_with("#cloud-config\n"),
            "must start with cloud-config: {yaml}"
        );
        assert!(
            yaml.contains("hostname: hbird-cp1\n"),
            "must set hostname: {yaml}"
        );
        assert!(
            yaml.contains("disable_root: false\n"),
            "must allow root: {yaml}"
        );
        assert!(
            yaml.contains("name: root\n"),
            "must configure root user: {yaml}"
        );
        assert!(
            yaml.contains("ssh-ed25519 AAAA test-key"),
            "must embed pubkey: {yaml}"
        );
    }

    #[test]
    fn render_cp_user_data_switch_to_ghcr_emits_runcmd() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "ssh-ed25519 AAAA test",
            true,
            "v0.42.0",
            false,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: None,
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(yaml.contains("runcmd:\n"), "must have runcmd: {yaml}");
        assert!(
            yaml.contains("ghcr.io/aatchison/hummingbird-k8s:v0.42.0"),
            "must have bootc switch runcmd: {yaml}"
        );
    }

    #[test]
    fn render_cp_user_data_auto_update_cp_false_disables_timer() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "ssh-ed25519 AAAA test",
            false,
            "v0.1.0",
            false,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: None,
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(
            yaml.contains("systemctl, disable, --now, bootc-semver-update.timer"),
            "auto_update_cp=false must disable timer: {yaml}"
        );
        assert!(
            !yaml.contains("systemctl, enable"),
            "auto_update_cp=false must NOT enable timer: {yaml}"
        );
    }

    #[test]
    fn render_cp_user_data_auto_update_cp_true_enables_timer() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "ssh-ed25519 AAAA test",
            false,
            "v0.1.0",
            true,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: None,
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(
            yaml.contains("systemctl, enable, --now, bootc-semver-update.timer"),
            "auto_update_cp=true must enable timer: {yaml}"
        );
        assert!(
            yaml.contains("systemctl, disable, --now, bootc-fetch-apply-updates.timer"),
            "auto_update_cp=true must disable legacy timer: {yaml}"
        );
    }

    /// #404 parity: POD_CIDR / SERVICE_CIDR were parsed by nothing and
    /// emitted by nothing in the Rust path, so a cluster deployed with
    /// `hbird` silently ignored the operator's CIDRs while the bash twin
    /// honored them. Same silent-drop class as the bootc_update_* bug.
    #[test]
    fn render_cp_user_data_emits_cidr_overrides() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "k",
            false,
            "v0.1.0",
            true,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: None,
                pod_cidr: Some("10.244.0.0/16"),
                service_cidr: Some("10.96.0.0/12"),
                net2_mac: None,
            },
        );
        assert!(yaml.contains("write_files:"), "{yaml}");
        assert!(
            yaml.contains("/etc/hummingbird/k8s-init-local.env"),
            "{yaml}"
        );
        assert!(yaml.contains("      POD_CIDR=10.244.0.0/16\n"), "{yaml}");
        assert!(yaml.contains("      SERVICE_CIDR=10.96.0.0/12\n"), "{yaml}");
        // 0600, not 0644: the bash twin is stricter for this file.
        let idx = yaml.find("k8s-init-local.env").unwrap();
        assert!(
            yaml[idx..].contains("permissions: '0600'"),
            "k8s-init-local.env must be 0600: {yaml}"
        );
    }

    /// Only one of the two set is legal — emit just that key.
    #[test]
    fn render_cp_user_data_emits_partial_cidr() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "k",
            false,
            "v0.1.0",
            true,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: None,
                pod_cidr: Some("10.244.0.0/16"),
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(yaml.contains("POD_CIDR=10.244.0.0/16"), "{yaml}");
        assert!(
            !yaml.contains("SERVICE_CIDR="),
            "must omit unset key: {yaml}"
        );
    }

    /// CIDRs alone must still open the write_files block, even with no
    /// bootc_update_* overrides set.
    #[test]
    fn render_cp_user_data_cidr_alone_opens_write_files() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "k",
            false,
            "v0.1.0",
            true,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: None,
                pod_cidr: None,
                service_cidr: Some("10.96.0.0/12"),
                net2_mac: None,
            },
        );
        assert!(yaml.contains("write_files:"), "{yaml}");
        assert!(yaml.contains("SERVICE_CIDR=10.96.0.0/12"), "{yaml}");
    }

    // ---- bootc_update_* write_files parity with the bash twin -------------

    /// The regression this closes: the operator sets BOOTC_UPDATE_SCHEDULE,
    /// the Rust planner parsed it, then threw it away (`let _ = (...)`),
    /// so the node silently kept the image-baked schedule while the bash
    /// twin honored it. Assert the drop-in is actually emitted.
    #[test]
    fn render_cp_user_data_emits_schedule_drop_in() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "ssh-ed25519 AAAA test",
            false,
            "v0.1.0",
            true,
            CpOverrides {
                bootc_update_schedule: Some("daily"),
                bootc_update_repo_k8s: None,
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(yaml.contains("write_files:"), "{yaml}");
        assert!(
            yaml.contains("/etc/systemd/system/bootc-semver-update.timer.d/schedule.conf"),
            "{yaml}"
        );
        // The blank OnCalendar= MUST precede the override, else systemd
        // unions the schedules instead of replacing the baked default.
        let blank = yaml
            .find("      OnCalendar=\n")
            .expect("missing clearing OnCalendar=");
        let set = yaml
            .find("      OnCalendar=daily\n")
            .expect("missing override");
        assert!(blank < set, "clearing OnCalendar= must come first: {yaml}");
        // And the timer must be re-read so it applies this boot.
        assert!(yaml.contains("systemctl, daemon-reload"), "{yaml}");
        assert!(
            yaml.contains("systemctl, restart, bootc-semver-update.timer"),
            "{yaml}"
        );
    }

    #[test]
    fn render_cp_user_data_emits_repo_env() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "k",
            false,
            "v0.1.0",
            true,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: Some("ghcr.io/example/repo"),
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(yaml.contains("/etc/hummingbird/bootc-update.env"), "{yaml}");
        assert!(yaml.contains("      REPO=ghcr.io/example/repo\n"), "{yaml}");
        assert!(yaml.contains("      PREFIX=v\n"), "{yaml}");
    }

    /// No overrides set => no `write_files:` key at all. The bash twin is
    /// careful not to emit an empty block, and cloud-init rejects a
    /// `write_files:` with no entries.
    #[test]
    fn render_cp_user_data_omits_write_files_when_unset() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "k",
            false,
            "v0.1.0",
            true,
            CpOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_k8s: None,
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(!yaml.contains("write_files:"), "must stay clean: {yaml}");
    }

    /// Bash treats an empty var as unset. Some("") must therefore behave
    /// exactly like None — emitting a blank OnCalendar= would disarm the
    /// timer instead of scheduling it.
    #[test]
    fn render_cp_user_data_treats_empty_string_as_unset() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "k",
            false,
            "v0.1.0",
            true,
            CpOverrides {
                bootc_update_schedule: Some(""),
                bootc_update_repo_k8s: Some(""),
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(!yaml.contains("write_files:"), "empty == unset: {yaml}");
        assert!(!yaml.contains("daemon-reload"), "empty == unset: {yaml}");
    }

    /// auto_update_cp=false must NOT get the daemon-reload/restart pair,
    /// or it would undo the sticky `disable` (bash line 136 gate).
    #[test]
    fn render_cp_user_data_no_restart_when_auto_update_off() {
        let yaml = render_cp_user_data(
            "hbird-cp1",
            "k",
            false,
            "v0.1.0",
            false,
            CpOverrides {
                bootc_update_schedule: Some("daily"),
                bootc_update_repo_k8s: None,
                pod_cidr: None,
                service_cidr: None,
                net2_mac: None,
            },
        );
        assert!(
            yaml.contains("schedule.conf"),
            "drop-in still written: {yaml}"
        );
        assert!(
            !yaml.contains("systemctl, restart, bootc-semver-update.timer"),
            "must not re-arm a deliberately disabled timer: {yaml}"
        );
    }

    #[test]
    fn render_worker_user_data_emits_schedule_and_repo() {
        let yaml = render_worker_user_data(
            "hbird-w1",
            "k",
            "kubeadm join ...",
            false,
            "v0.1.0",
            WorkerOverrides {
                bootc_update_schedule: Some("weekly"),
                bootc_update_repo_worker: Some("ghcr.io/example/worker"),
                net2_mac: None,
            },
        );
        assert!(yaml.contains("/etc/hummingbird/worker-join.env"), "{yaml}");
        assert!(
            yaml.contains("/etc/systemd/system/bootc-semver-update.timer.d/schedule.conf"),
            "{yaml}"
        );
        assert!(yaml.contains("      OnCalendar=weekly\n"), "{yaml}");
        assert!(
            yaml.contains("      REPO=ghcr.io/example/worker\n"),
            "{yaml}"
        );
        assert!(yaml.contains("      PREFIX=v\n"), "{yaml}");
        assert!(
            yaml.contains("systemctl, restart, bootc-semver-update.timer"),
            "{yaml}"
        );
    }

    #[test]
    fn render_worker_user_data_treats_empty_string_as_unset() {
        let yaml = render_worker_user_data(
            "hbird-w1",
            "k",
            "kubeadm join ...",
            false,
            "v0.1.0",
            WorkerOverrides {
                bootc_update_schedule: Some(""),
                bootc_update_repo_worker: Some(""),
                net2_mac: None,
            },
        );
        assert!(!yaml.contains("schedule.conf"), "empty == unset: {yaml}");
        assert!(!yaml.contains("bootc-update.env"), "empty == unset: {yaml}");
    }

    #[test]
    fn render_worker_user_data_contains_join_cmd() {
        let join_cmd = "kubeadm join 192.168.122.10:6443 --token abc.def --discovery-token-ca-cert-hash sha256:deadbeef";
        let yaml = render_worker_user_data(
            "hbird-w1",
            "ssh-ed25519 AAAA test",
            join_cmd,
            false,
            "v0.1.0",
            WorkerOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_worker: None,
                net2_mac: None,
            },
        );
        assert!(yaml.contains(join_cmd), "must embed join command: {yaml}");
        assert!(
            yaml.contains("/etc/hummingbird/worker-join.env"),
            "must set join env path: {yaml}"
        );
        assert!(
            yaml.contains("permissions: '0600'"),
            "must set 0600 permissions: {yaml}"
        );
    }

    #[test]
    fn render_worker_user_data_hostname_matches() {
        let yaml = render_worker_user_data(
            "hbird-w2",
            "ssh-ed25519 AAAA test",
            "kubeadm join ...",
            false,
            "v0.1.0",
            WorkerOverrides {
                bootc_update_schedule: None,
                bootc_update_repo_worker: None,
                net2_mac: None,
            },
        );
        assert!(
            yaml.contains("hostname: hbird-w2\n"),
            "hostname must match: {yaml}"
        );
    }

    #[test]
    fn cloud_init_seed_cmd_uses_cloud_localds() {
        let cmd = cloud_init_seed_cmd(
            "hbird-cp1",
            "/tmp/ud.yaml",
            "/mnt/pool/hbird-cp1-seed.iso",
            None,
        );
        assert!(
            cmd.contains("cloud-localds"),
            "must try cloud-localds first: {cmd}"
        );
        assert!(
            cmd.contains("genisoimage"),
            "must fall back to genisoimage: {cmd}"
        );
        assert!(cmd.contains("mkisofs"), "must fall back to mkisofs: {cmd}");
        assert!(
            cmd.contains("/mnt/pool/hbird-cp1-seed.iso"),
            "must embed iso path: {cmd}"
        );
        assert!(cmd.contains("hbird-cp1"), "must embed hostname: {cmd}");
        assert!(cmd.contains("meta-data"), "must create meta-data: {cmd}");
        assert!(cmd.contains("user-data"), "must copy user-data: {cmd}");
    }

    // ---- find_repo_root (#33 regression tests) --------------------------------

    /// `find_repo_root` must find the hummingbird-k8s repo root regardless of
    /// where the config file lives. When tests run (cwd is inside the repo),
    /// the repo root must contain both `Makefile` and `containers/`.
    ///
    /// This is the core bug from #33: the old default was `config.parent()`,
    /// so `~/cluster.local.conf` → repo_root=`~` → `make -C ~` failed.
    #[test]
    fn find_repo_root_resolves_to_dir_with_makefile_and_containers() {
        let root = find_repo_root();
        assert!(
            root.join("Makefile").exists(),
            "repo root must contain Makefile; find_repo_root returned: {root:?}"
        );
        assert!(
            root.join("containers").is_dir(),
            "repo root must contain containers/; find_repo_root returned: {root:?}"
        );
    }

    /// When `--repo-root` is absent, the Plan must use the repo root derived
    /// from cwd, NOT the config file's parent directory.
    #[test]
    fn plan_repo_root_defaults_to_cwd_derived_root_not_config_dir() {
        // Config lives in /tmp — definitely not the repo root.
        let tmp_conf = std::env::temp_dir().join("cluster-test-33.conf");
        let args = DeployClusterArgs {
            config: tmp_conf.clone(),
            repo_root: None,
            ..default_args()
        };
        // To test the "defaults to cwd-derived root" path, we must set IMAGE_SOURCE=local.
        let mut config = cfg(None);
        config.image_source = "local".to_string();
        let plan = Plan::from_args(&args, config).expect("plan");
        // The resolved repo_root must NOT be /tmp (the config's parent).
        let config_parent = tmp_conf
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        assert_ne!(
            plan.repo_root, config_parent,
            "#33: repo_root must not default to the config file's parent directory"
        );
        // It must look like a valid repo root (contains Makefile + containers/).
        assert!(
            plan.repo_root.join("Makefile").exists(),
            "plan.repo_root must be the actual repo root; got: {:?}",
            plan.repo_root
        );
    }

    /// The git fast-path in `find_repo_root` must validate markers before
    /// accepting the returned toplevel. A path without `Makefile` +
    /// `containers/` must be rejected so that a different/nested git repo
    /// does not produce a wrong root; the walk-up then finds the real one.
    ///
    /// Tests the `path_has_repo_markers` guard directly — the end-to-end
    /// "git returns wrong root → walk-up corrects it" scenario requires
    /// changing process cwd and is covered by the integration gate.
    #[test]
    fn find_repo_root_git_fast_path_requires_markers() {
        // A directory without the markers must be rejected by the guard.
        let tmp = std::env::temp_dir();
        assert!(
            !path_has_repo_markers(&tmp),
            "/tmp must not have repo markers — test precondition failed"
        );
        // A real hummingbird-k8s root must pass the guard.
        let real_root = find_repo_root();
        assert!(
            path_has_repo_markers(&real_root),
            "find_repo_root must return a path with Makefile+containers/; got {real_root:?}"
        );
    }

    /// An explicit `--repo-root` override always wins over the cwd-derived default.
    #[test]
    fn plan_repo_root_explicit_override_wins() {
        let args = DeployClusterArgs {
            config: PathBuf::from("/tmp/cluster.conf"),
            repo_root: Some(PathBuf::from("/explicit/path")),
            ..default_args()
        };
        let plan = Plan::from_args(&args, cfg(None)).expect("plan");
        assert_eq!(plan.repo_root, PathBuf::from("/explicit/path"));
    }
}
