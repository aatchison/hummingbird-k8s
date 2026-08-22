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
fn plan_cp_seed(plan: &Plan, conn: &Connection, pubkey_contents: &str) -> Result<String> {
    let cp_seed = format!("{}/{}-seed.iso", plan.pool_dir, plan.cp_name);
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would render CP cloud-init user-data (auto-update-cp={}, switch-to-ghcr={}, ghcr-tag={})",
            plan.auto_update_cp, plan.switch_to_ghcr, plan.ghcr_tag,
        ));
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
        },
    );

    let ud_tmp = format!("/tmp/hbird-cp-ud-{}.yaml", std::process::id());
    let write_cmd = format!(
        "cat > '{}' << 'HBIRD_CI_EOF'\n{}\nHBIRD_CI_EOF",
        ud_tmp, user_data
    );
    conn.exec_shell(&write_cmd)
        .map_err(|e| anyhow!("could not write CP user-data to {ud_tmp}: {e}"))?;

    let seed_cmd = cloud_init_seed_cmd(&plan.cp_name, &ud_tmp, &cp_seed);
    conn.exec_shell(&seed_cmd)
        .map_err(|e| anyhow!("cloud-init seed build for CP failed: {e}"))?;

    Ok(cp_seed)
}

/// Plan the CP virt-install step. Mirrors lines 480-508.
fn plan_cp_virt_install(
    plan: &Plan,
    conn: &Connection,
    cp_template: &str,
    cp_seed: &str,
) -> Result<String> {
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

    log(&format!(
        "virt-install {} (memory={} vcpus={})",
        plan.cp_name, plan.cp_memory, plan.cp_vcpus
    ));
    conn.virt_install(
        &plan.cp_name,
        plan.cp_memory as u64,
        plan.cp_vcpus,
        &cp_qcow,
        Some(cp_seed),
    )
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

/// Plan the per-worker seed + virt-install step. Mirrors lines 547-597.
fn plan_worker_spawn(
    plan: &Plan,
    conn: &Connection,
    worker_template: &str,
    join_cmd: &str,
    cp_ip: &str,
    privkey_path: &str,
    pubkey_contents: &str,
) -> Result<()> {
    let _ = (cp_ip, privkey_path); // used in future cluster-ready polling; declared for signature parity
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

    for w in &plan.worker_names {
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

        let user_data = render_worker_user_data(
            w,
            pubkey_contents,
            join_cmd,
            plan.switch_to_ghcr,
            &plan.ghcr_tag,
            plan.bootc_update_schedule.as_deref(),
            plan.bootc_update_repo_worker.as_deref(),
        );

        let write_cmd = format!(
            "cat > '{}' << 'HBIRD_CI_EOF'\n{}\nHBIRD_CI_EOF",
            ud_tmp, user_data
        );
        conn.exec_shell(&write_cmd)
            .map_err(|e| anyhow!("could not write worker user-data to {ud_tmp}: {e}"))?;

        let seed_cmd = cloud_init_seed_cmd(w, &ud_tmp, &w_seed);
        conn.exec_shell(&seed_cmd)
            .map_err(|e| anyhow!("cloud-init seed build for {w} failed: {e}"))?;

        log(&format!("cloning worker qcow2 -> {w_qcow}"));
        conn.remote_cp_reflink(worker_template, &w_qcow)
            .map_err(|e| anyhow!("reflink clone {worker_template} -> {w_qcow} failed: {e}"))?;

        log(&format!(
            "virt-install {w} (memory={} vcpus={}) [bg]",
            plan.worker_memory, plan.worker_vcpus
        ));
        conn.virt_install(
            w,
            plan.worker_memory as u64,
            plan.worker_vcpus,
            &w_qcow,
            Some(&w_seed),
        )
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

    // Build connection once; shared by image acquisition and bib.
    let conn = crate::virt_bridge::build_connection(plan.kvm_host.as_deref());

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
    let cp_seed = plan_cp_seed(&plan, &conn, &pubkey_contents)?;
    let _cp_qcow = plan_cp_virt_install(&plan, &conn, &cp_template, &cp_seed)?;
    let cp_ip = plan_cp_ready(&plan, &conn, &privkey_path)?;
    let join_cmd = plan_join_token(&plan, &conn, &cp_ip, &privkey_path)?;
    plan_worker_spawn(
        &plan,
        &conn,
        &worker_template,
        &join_cmd,
        &cp_ip,
        &privkey_path,
        &pubkey_contents,
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

/// Render worker cloud-config YAML (bash twin: `worker_user_data`).
pub(crate) fn render_worker_user_data(
    worker_name: &str,
    pubkey_contents: &str,
    join_cmd: &str,
    switch_to_ghcr: bool,
    ghcr_tag: &str,
    bootc_update_schedule: Option<&str>,
    bootc_update_repo_worker: Option<&str>,
) -> String {
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
    if switch_to_ghcr || bootc_update_schedule.is_some() {
        out.push_str("runcmd:\n");
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
fn cloud_init_seed_cmd(hostname: &str, ud_tmp: &str, out_iso: &str) -> String {
    let hostname_q = sh_quote(hostname);
    let ud_q = sh_quote(ud_tmp);
    let iso_q = sh_quote(out_iso);
    format!(
        "set -euo pipefail; \
         _tmp=$(mktemp -d -t hbird-ci-XXXXXX); \
         cp {ud_q} \"$_tmp/user-data\"; \
         printf 'instance-id: hbird-%s-%s\\nlocal-hostname: %s\\n' \
           \"$(date +%s)\" \"$$\" {hostname_q} > \"$_tmp/meta-data\"; \
         if command -v cloud-localds >/dev/null 2>&1; then \
           cloud-localds {iso_q} \"$_tmp/user-data\" \"$_tmp/meta-data\"; \
         elif command -v genisoimage >/dev/null 2>&1; then \
           genisoimage -output {iso_q} -volid cidata -joliet -rock \
             \"$_tmp/user-data\" \"$_tmp/meta-data\" >/dev/null 2>&1; \
         elif command -v mkisofs >/dev/null 2>&1; then \
           mkisofs -output {iso_q} -volid cidata -joliet -rock \
             \"$_tmp/user-data\" \"$_tmp/meta-data\" >/dev/null 2>&1; \
         else \
           echo 'build_cloud_init_seed: need cloud-localds / genisoimage / mkisofs' >&2; exit 1; \
         fi; \
         rm -rf -- \"$_tmp\"; \
         rm -f -- {ud_q}"
    )
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
            Some("weekly"),
            Some("ghcr.io/example/worker"),
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
            Some(""),
            Some(""),
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
            None,
            None,
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
            None,
            None,
        );
        assert!(
            yaml.contains("hostname: hbird-w2\n"),
            "hostname must match: {yaml}"
        );
    }

    #[test]
    fn cloud_init_seed_cmd_uses_cloud_localds() {
        let cmd = cloud_init_seed_cmd("hbird-cp1", "/tmp/ud.yaml", "/mnt/pool/hbird-cp1-seed.iso");
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
