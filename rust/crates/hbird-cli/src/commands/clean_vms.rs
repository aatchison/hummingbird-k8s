//! `hbird clean-vms` — bash twin: `scripts/clean-vms.sh`.
//!
//! Sweeps every `hummingbird-*` libvirt domain off the KVM host, then
//! removes the stale qcow2 / seed-ISO / cloud-init-ISO artifacts those
//! domains leave behind under `POOL_DIR`, then `pool-refresh`es so
//! libvirt's volume catalog drops the removed files.
//!
//! # Why this is a Rust subcommand now
//!
//! The bash twin's first act is `source scripts/lib/ssh-wrap.sh` +
//! `hbird_ssh_wrap_maybe_reexec` — a 752-line shim whose entire job is
//! "decide local-vs-remote, re-exec on `$KVM_HOST`, forward an env
//! allowlist". [`crate::virt_bridge::build_connection`] already does the
//! local-vs-remote decision natively, and clap `env = "…"` attributes
//! replace the env allowlist with a declarative, `--help`-visible one.
//! So this module deliberately does NOT port the shim (see the reply in
//! the PR body for the two shim behaviours consciously dropped).
//!
//! # Block traceability
//!
//! Each `// ---- <name> ----` header matches a section of
//! `scripts/clean-vms.sh`:
//!
//! 1. `POOL_DIR` / `POOL_NAME` defaulting  → [`Plan::from_args`]
//! 2. Destroy + undefine `hummingbird-*`   → [`sweep_domains`]
//! 3. Straggler file sweep under POOL_DIR  → [`sweep_stragglers`]
//! 4. `virsh pool-refresh`                 → [`refresh_pool`]
//!
//! # Deliberate divergence from the bash twin
//!
//! * **Straggler globs are anchored.** The bash twin sweeps
//!   `"$POOL_DIR"/hummingbird-*.qcow2` but also the *unanchored*
//!   `"$POOL_DIR"/*-seed.iso` and `"$POOL_DIR"/*-cloud-init.iso`. On a
//!   host that also runs production VMs (`hbird-geary-*`,
//!   `hbird-forge-*`) out of the same pool, `make clean-vms` would
//!   delete their cloud-init seed ISOs — which a domain still references
//!   as a CDROM, so the next `virsh start` of an untouched production VM
//!   fails. That is inconsistent with step 2, which only ever destroys
//!   `hummingbird-*` domains. Treated as a bug in the twin and fixed:
//!   [`is_straggler_artifact`] requires the `hummingbird-` prefix on all
//!   three patterns, and [`tests::straggler_matcher_never_touches_production_vms`]
//!   pins it.
//! * **A fatal SSH/transport failure is an error, not "nothing to
//!   clean".** The bash twin `|| true`s every command, so a typo'd
//!   `KVM_HOST` prints `[clean-vms] done.` and exits 0 having cleaned
//!   nothing. Same rationale as `destroy_cluster.rs`'s round-2 L3#2 fix.
//!   Per-item failures stay non-fatal (WARN + exit 0), matching the
//!   twin's idempotency contract.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::Args;

use hbird_virt::{Connection, Error as VirtError};

// ---- Arguments -------------------------------------------------------------

/// Arguments for `hbird clean-vms`.
///
/// The bash twin takes no flags at all — every knob is an environment
/// variable forwarded by `scripts/lib/ssh-wrap.sh`'s
/// `HBIRD_SSH_WRAP_ALLOWED_ENV`. Each such variable becomes a clap flag
/// with a matching `env = "…"`, so the operator's existing
/// `POOL_DIR=… make clean-vms` muscle memory keeps working while the
/// knob also shows up in `hbird clean-vms --help`.
#[derive(Debug, Args)]
pub struct CleanVmsArgs {
    /// Path to `cluster.local.conf`. Optional — only `POOL_DIR` and
    /// `KVM_HOST` are read from it, and both have defaults. The bash
    /// twin never sources a config at all.
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,

    /// SSH alias of the KVM host. Absent/empty → run locally (operator
    /// is already on the KVM host). Replaces the bash twin's
    /// `hbird_ssh_wrap_maybe_reexec` re-exec.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// libvirt image pool directory to sweep. Bash twin:
    /// `: "${POOL_DIR:=/var/lib/libvirt/images}"`.
    #[arg(long, value_name = "DIR", env = "POOL_DIR")]
    pub pool_dir: Option<String>,

    /// libvirt storage-pool name to refresh afterwards. Bash twin:
    /// `: "${POOL_NAME:=default}"`.
    #[arg(long, value_name = "NAME", env = "POOL_NAME")]
    pub pool_name: Option<String>,

    /// Plan-only mode — print what would be destroyed / removed without
    /// touching anything. Discovery (`virsh list`, `ls`) still runs
    /// because the plan is derived from live host state; nothing
    /// mutating does. The bash twin has no dry-run.
    #[arg(long)]
    pub dry_run: bool,
}

// ---- Logger ----------------------------------------------------------------

/// Bash twin writes its progress lines to **stdout** via plain `echo`
/// (`echo "[clean-vms] destroying $d"`). Keep the stream and the
/// `[clean-vms] ` prefix identical so operator greps / CI log scrapers
/// that key on those lines keep matching.
fn log(line: &str) {
    println!("[clean-vms] {line}");
}

// ---- Defaults (block #1) ---------------------------------------------------

/// Bash twin: `: "${POOL_DIR:=/var/lib/libvirt/images}"`.
const DEFAULT_POOL_DIR: &str = "/var/lib/libvirt/images";

/// Bash twin: `: "${POOL_NAME:=default}"`.
const DEFAULT_POOL_NAME: &str = "default";

/// Domain-name prefix this command is allowed to touch.
///
/// Bash twin: `grep '^hummingbird-'`. The anchor matters — the same KVM
/// host runs production domains named `hbird-geary-*` and
/// `hbird-forge-*`, and an unanchored match would destroy them.
const HUMMINGBIRD_PREFIX: &str = "hummingbird-";

/// Filename suffixes swept out of `POOL_DIR`, in bash-twin order.
///
/// Bash twin (`scripts/clean-vms.sh::64-68`) uses the globs
/// `hummingbird-*.qcow2`, `*-seed.iso`, `*-cloud-init.iso`. The
/// `hummingbird-` anchor is applied to all three here — see the module
/// docs for why the twin's last two are treated as a bug.
const STRAGGLER_SUFFIXES: [&str; 3] = [".qcow2", "-seed.iso", "-cloud-init.iso"];

// ---- Matchers (pure; the safety-critical part) ------------------------------

/// Does this libvirt domain name belong to a Hummingbird cluster?
///
/// Bash twin: `virsh list --all --name | grep '^hummingbird-'`.
///
/// Anchored prefix match, byte-exact and case-sensitive — exactly what
/// `grep '^hummingbird-'` does. Production domains on the same host
/// (`hbird-geary-1`, `hbird-forge-runner-2`) MUST NOT match; see
/// [`tests::domain_matcher_never_touches_production_vms`].
pub(crate) fn is_hummingbird_domain(name: &str) -> bool {
    name.starts_with(HUMMINGBIRD_PREFIX)
}

/// Is this `POOL_DIR` entry a Hummingbird straggler artifact?
///
/// Requires BOTH the `hummingbird-` prefix and one of
/// [`STRAGGLER_SUFFIXES`]. `name` is a bare directory entry (no `/`),
/// as returned by [`hbird_virt::Connection::remote_ls`].
///
/// The bash twin's `*-seed.iso` / `*-cloud-init.iso` globs lack the
/// prefix requirement; that is the bug this function fixes.
pub(crate) fn is_straggler_artifact(name: &str) -> bool {
    if !name.starts_with(HUMMINGBIRD_PREFIX) {
        return false;
    }
    STRAGGLER_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

// ---- Plan ------------------------------------------------------------------

/// Merged view of args + config + env, built once at the top of [`run`].
#[derive(Debug, Clone)]
struct Plan {
    pool_dir: String,
    pool_name: String,
    kvm_host: Option<String>,
    dry_run: bool,
}

impl Plan {
    /// Resolution order for every knob: explicit flag (which clap has
    /// already filled from the matching env var) wins, then the config
    /// file, then the bash twin's literal default.
    fn from_args(args: &CleanVmsArgs, config: Option<hbird_config::ClusterConfig>) -> Self {
        let (cfg_pool_dir, cfg_kvm_host) = match config {
            Some(c) => (Some(c.pool_dir), c.kvm_host),
            None => (None, None),
        };
        Self {
            pool_dir: args
                .pool_dir
                .clone()
                .filter(|s| !s.is_empty())
                .or(cfg_pool_dir)
                .unwrap_or_else(|| DEFAULT_POOL_DIR.to_string()),
            pool_name: args
                .pool_name
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_POOL_NAME.to_string()),
            kvm_host: args
                .kvm_host
                .clone()
                .filter(|s| !s.is_empty())
                .or(cfg_kvm_host),
            dry_run: args.dry_run,
        }
    }

    /// Absolute path of a `POOL_DIR` entry.
    fn pool_path(&self, name: &str) -> String {
        format!("{}/{name}", self.pool_dir.trim_end_matches('/'))
    }
}

// ---- Block #2: destroy + undefine all hummingbird-* domains ----------------

/// Destroy + undefine every `hummingbird-*` domain.
///
/// Mirrors `scripts/clean-vms.sh::50-59`. Idempotent: a domain that is
/// already shut off makes `virsh destroy` exit non-zero, which the twin
/// swallows with `2>/dev/null || true` — we downgrade it to a WARN so
/// the operator can still see it, and keep going.
///
/// Returns the number of WARN lines emitted. Fatal SSH/transport
/// failures during discovery propagate as `Err`.
fn sweep_domains(conn: &Connection, plan: &Plan) -> Result<usize> {
    let all = conn.domains().map_err(|e| {
        anyhow::Error::new(e).context(
            "clean-vms: could not list libvirt domains — check --kvm-host reachability and \
             libvirt-group membership before re-running",
        )
    })?;

    let mut warnings = 0usize;
    for domain in all.iter().filter(|d| is_hummingbird_domain(&d.name)) {
        let name = &domain.name;
        if plan.dry_run {
            log(&format!(
                "DRY-RUN would destroy + undefine --remove-all-storage {name}"
            ));
            continue;
        }
        // Wording preserved verbatim from the bash twin (`echo
        // "[clean-vms] destroying $d"`); operators grep for it.
        log(&format!("destroying {name}"));
        if let Err(e) = conn.destroy_domain(name) {
            // Expected + benign when the domain was already shut off.
            log(&format!(
                "WARN: virsh destroy returned non-zero for {name} (already shut off?): {e}"
            ));
            warnings += 1;
        }
        if let Err(e) = conn.undefine_domain_remove_all_storage(name) {
            log(&format!(
                "WARN: virsh undefine --remove-all-storage returned non-zero for {name}: {e}"
            ));
            warnings += 1;
        }
    }
    Ok(warnings)
}

// ---- Block #3: straggler sweep under POOL_DIR (#221) -----------------------

/// Remove leftover `hummingbird-*` qcow2 / seed-ISO / cloud-init-ISO
/// files under `POOL_DIR`.
///
/// Mirrors `scripts/clean-vms.sh::62-72`. A missing `POOL_DIR` is not an
/// error — the twin's `shopt -s nullglob` yields an empty glob for the
/// same case — so an `ls` that exits non-zero downgrades to a WARN.
fn sweep_stragglers(conn: &Connection, plan: &Plan) -> Result<usize> {
    let entries = match conn.remote_ls(&plan.pool_dir) {
        Ok(v) => v,
        // `ls` ran and failed → directory missing / unreadable. Bash's
        // nullglob treats this as "nothing to sweep"; keep that, but say so.
        Err(VirtError::VirshFailed { stderr, .. }) => {
            log(&format!(
                "WARN: could not list {} ({}); skipping straggler sweep",
                plan.pool_dir,
                stderr.trim(),
            ));
            return Ok(1);
        }
        // Transport failure → the host is not reachable at all. Fatal.
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "clean-vms: transport failure listing {}",
                plan.pool_dir
            )));
        }
    };

    let mut warnings = 0usize;
    // Sort so the emitted plan is deterministic regardless of the order
    // `ls` / the filesystem happened to return entries in.
    let mut matched: Vec<&String> = entries
        .iter()
        .filter(|name| is_straggler_artifact(name))
        .collect();
    matched.sort();

    for name in matched {
        let path = plan.pool_path(name);
        if plan.dry_run {
            log(&format!("DRY-RUN would rm -f {path}"));
            continue;
        }
        // Wording preserved verbatim from the bash twin
        // (`echo "[clean-vms] removing $f"`).
        log(&format!("removing {path}"));
        if let Err(e) = conn.remote_rm_f(&path) {
            log(&format!("WARN: could not remove {path}: {e}"));
            warnings += 1;
        }
    }
    Ok(warnings)
}

// ---- Block #4: pool-refresh ------------------------------------------------

/// Refresh libvirt's volume catalog so it drops the removed files.
///
/// Mirrors `scripts/clean-vms.sh::77` —
/// `virsh pool-refresh "$POOL_NAME" >/dev/null 2>&1 || true`. Non-fatal:
/// a host with a differently-named pool still gets a clean sweep, it
/// just keeps a stale catalog until libvirt notices on its own.
fn refresh_pool(conn: &Connection, plan: &Plan) -> usize {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would virsh pool-refresh {}",
            plan.pool_name
        ));
        return 0;
    }
    if let Err(e) = conn.virsh_pool_refresh(&plan.pool_name) {
        log(&format!(
            "WARN: virsh pool-refresh {} returned non-zero: {e}",
            plan.pool_name
        ));
        return 1;
    }
    0
}

// ---- run entrypoint --------------------------------------------------------

/// Dispatch entrypoint invoked by `main.rs`.
///
/// # Exit codes
///
/// Mirrors the bash twin: **0** for a clean or partially-warned sweep
/// (missing domains and missing files are not an error — the twin's
/// stated idempotency contract). Non-zero only when the KVM host itself
/// could not be reached, which the twin cannot distinguish.
#[tracing::instrument(
    level = "debug",
    skip(args),
    fields(kvm_host = ?args.kvm_host, dry_run = args.dry_run),
    err(Debug)
)]
pub fn run(args: CleanVmsArgs) -> Result<()> {
    let config = match &args.config {
        Some(path) => Some(
            hbird_config::parse(path)
                .map_err(|e| anyhow!("{e}"))
                .with_context(|| format!("clean-vms: could not read config {}", path.display()))?,
        ),
        None => None,
    };
    let plan = Plan::from_args(&args, config);

    // build_connection runs virsh LOCALLY when kvm_host is None/empty —
    // i.e. the operator typed `hbird clean-vms` on the KVM host itself,
    // which the bash twin handled via the ssh-wrap shim's no-op path.
    let conn = crate::virt_bridge::build_connection(plan.kvm_host.as_deref());

    let mut warnings = 0usize;
    warnings += sweep_domains(&conn, &plan)?;
    warnings += sweep_stragglers(&conn, &plan)?;
    warnings += refresh_pool(&conn, &plan);

    if warnings > 0 {
        log(&format!(
            "{warnings} step(s) warned — see WARN lines above (non-fatal, matching the bash twin)"
        ));
    }
    // Wording preserved verbatim from the bash twin's final line.
    log("done.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> CleanVmsArgs {
        CleanVmsArgs {
            config: None,
            kvm_host: None,
            pool_dir: None,
            pool_name: None,
            dry_run: false,
        }
    }

    // ---- domain matcher --------------------------------------------------

    #[test]
    fn domain_matcher_accepts_deployed_hummingbird_names() {
        for name in [
            "hummingbird-k8s",
            "hummingbird-k8s-worker-1",
            "hummingbird-k8s-worker-27",
            // `grep '^hummingbird-'` matches the bare prefix too.
            "hummingbird-",
        ] {
            assert!(is_hummingbird_domain(name), "{name} should match");
        }
    }

    /// SAFETY PIN. The KVM host also runs production domains named
    /// `hbird-geary-*` and `hbird-forge-*`. `make clean-vms` destroys +
    /// undefines whatever this matcher accepts, with
    /// `--remove-all-storage`. If this test ever fails, the change under
    /// review deletes production VMs.
    #[test]
    fn domain_matcher_never_touches_production_vms() {
        for name in [
            "hbird-geary-1",
            "hbird-geary-cp",
            "hbird-geary-worker-3",
            "hbird-forge-runner",
            "hbird-forge-1",
            "hbird-cp1",
            "hbird-w1",
        ] {
            assert!(
                !is_hummingbird_domain(name),
                "PRODUCTION VM {name} must never match clean-vms' domain filter",
            );
        }
    }

    #[test]
    fn domain_matcher_is_anchored_and_case_sensitive() {
        // Unanchored substring match would accept these; `grep '^…'` does not.
        assert!(!is_hummingbird_domain("not-hummingbird-k8s"));
        assert!(!is_hummingbird_domain("xhummingbird-k8s"));
        assert!(!is_hummingbird_domain(" hummingbird-k8s"));
        // Case-sensitive, exactly like grep without -i.
        assert!(!is_hummingbird_domain("Hummingbird-k8s"));
        assert!(!is_hummingbird_domain("HUMMINGBIRD-K8S"));
        // Prefix without the separating dash.
        assert!(!is_hummingbird_domain("hummingbirds"));
        assert!(!is_hummingbird_domain(""));
    }

    // ---- straggler matcher -----------------------------------------------

    #[test]
    fn straggler_matcher_accepts_all_three_bash_suffixes() {
        for name in [
            "hummingbird-k8s.qcow2",
            "hummingbird-k8s-worker-1.qcow2",
            "hummingbird-k8s-seed.iso",
            "hummingbird-k8s-worker-2-seed.iso",
            "hummingbird-k8s-cloud-init.iso",
        ] {
            assert!(is_straggler_artifact(name), "{name} should match");
        }
    }

    /// SAFETY PIN + BUG FIX. The bash twin's unanchored `*-seed.iso` and
    /// `*-cloud-init.iso` globs match production seed ISOs sitting in the
    /// same pool. Deleting one breaks the next `virsh start` of an
    /// otherwise-untouched production VM, because the domain XML still
    /// references it as a CDROM source.
    #[test]
    fn straggler_matcher_never_touches_production_vms() {
        for name in [
            "hbird-geary-1-seed.iso",
            "hbird-geary-cp-cloud-init.iso",
            "hbird-geary-1.qcow2",
            "hbird-forge-runner-seed.iso",
            "hbird-forge-1-cloud-init.iso",
            "hbird-forge-1.qcow2",
        ] {
            assert!(
                !is_straggler_artifact(name),
                "PRODUCTION artifact {name} must never match clean-vms' file sweep \
                 (bash twin's unanchored *-seed.iso glob did — that is the bug)",
            );
        }
    }

    #[test]
    fn straggler_matcher_rejects_unrelated_extensions() {
        // Right prefix, wrong suffix — the twin's globs wouldn't match either.
        assert!(!is_straggler_artifact("hummingbird-k8s.iso"));
        assert!(!is_straggler_artifact("hummingbird-k8s.raw"));
        assert!(!is_straggler_artifact("hummingbird-k8s"));
        assert!(!is_straggler_artifact("hummingbird-k8s.qcow2.bak"));
        // Base images / templates the deploy path needs must survive.
        assert!(!is_straggler_artifact("fedora-bootc.qcow2"));
        assert!(!is_straggler_artifact("base.qcow2"));
        assert!(!is_straggler_artifact(""));
    }

    #[test]
    fn straggler_matcher_is_case_sensitive_like_the_glob() {
        assert!(!is_straggler_artifact("Hummingbird-k8s.qcow2"));
        assert!(!is_straggler_artifact("hummingbird-k8s.QCOW2"));
    }

    // ---- plan resolution -------------------------------------------------

    #[test]
    fn plan_defaults_match_bash_twin_when_nothing_is_set() {
        let p = Plan::from_args(&args(), None);
        assert_eq!(p.pool_dir, "/var/lib/libvirt/images");
        assert_eq!(p.pool_name, "default");
        assert_eq!(p.kvm_host, None);
        assert!(!p.dry_run);
    }

    #[test]
    fn plan_flag_beats_config_for_pool_dir() {
        let cfg = hbird_config::parse_str(
            "CP_NAME=hummingbird-k8s\nSSH_PUBKEY_FILE=/k\nPOOL_DIR=/from/config\n",
        )
        .expect("cfg parses");
        let mut a = args();
        a.pool_dir = Some("/from/flag".to_string());
        let p = Plan::from_args(&a, Some(cfg));
        assert_eq!(p.pool_dir, "/from/flag");
    }

    #[test]
    fn plan_falls_back_to_config_pool_dir_and_kvm_host() {
        let cfg = hbird_config::parse_str(
            "CP_NAME=hummingbird-k8s\nSSH_PUBKEY_FILE=/k\nPOOL_DIR=/mnt/mass2\nKVM_HOST=geary\n",
        )
        .expect("cfg parses");
        let p = Plan::from_args(&args(), Some(cfg));
        assert_eq!(p.pool_dir, "/mnt/mass2");
        assert_eq!(p.kvm_host.as_deref(), Some("geary"));
    }

    /// An exported-but-empty `POOL_DIR=` / `KVM_HOST=` must behave like
    /// unset. Bash's `: "${POOL_DIR:=…}"` uses `:=` (empty counts as
    /// unset); a plain `Option` would keep the empty string and produce
    /// `ls -1 -- '/'`.
    #[test]
    fn plan_treats_empty_env_values_as_unset() {
        let mut a = args();
        a.pool_dir = Some(String::new());
        a.pool_name = Some(String::new());
        a.kvm_host = Some(String::new());
        let p = Plan::from_args(&a, None);
        assert_eq!(p.pool_dir, "/var/lib/libvirt/images");
        assert_eq!(p.pool_name, "default");
        assert_eq!(p.kvm_host, None);
    }

    #[test]
    fn plan_pool_path_joins_without_double_slash() {
        let mut a = args();
        a.pool_dir = Some("/mnt/mass2/".to_string());
        let p = Plan::from_args(&a, None);
        assert_eq!(
            p.pool_path("hummingbird-k8s.qcow2"),
            "/mnt/mass2/hummingbird-k8s.qcow2"
        );
    }

    /// End-to-end matcher check against a realistic mixed pool listing:
    /// exactly the Hummingbird artifacts are selected, nothing else.
    #[test]
    fn mixed_pool_listing_selects_only_hummingbird_artifacts() {
        let listing = [
            "base-fedora-bootc.qcow2",
            "hbird-forge-1-cloud-init.iso",
            "hbird-forge-1.qcow2",
            "hbird-geary-1-seed.iso",
            "hbird-geary-1.qcow2",
            "hummingbird-k8s-cloud-init.iso",
            "hummingbird-k8s-worker-1.qcow2",
            "hummingbird-k8s.qcow2",
            "deploy-cluster",
            "lost+found",
        ];
        let picked: Vec<&str> = listing
            .iter()
            .copied()
            .filter(|n| is_straggler_artifact(n))
            .collect();
        assert_eq!(
            picked,
            vec![
                "hummingbird-k8s-cloud-init.iso",
                "hummingbird-k8s-worker-1.qcow2",
                "hummingbird-k8s.qcow2",
            ],
        );
    }

    /// Same listing, but for domains rather than files.
    #[test]
    fn mixed_domain_listing_selects_only_hummingbird_domains() {
        let domains = [
            "hbird-forge-1",
            "hbird-geary-1",
            "hbird-geary-cp",
            "hummingbird-k8s",
            "hummingbird-k8s-worker-1",
            "hummingbird-k8s-worker-2",
        ];
        let picked: Vec<&str> = domains
            .iter()
            .copied()
            .filter(|n| is_hummingbird_domain(n))
            .collect();
        assert_eq!(
            picked,
            vec![
                "hummingbird-k8s",
                "hummingbird-k8s-worker-1",
                "hummingbird-k8s-worker-2",
            ],
        );
    }
}
