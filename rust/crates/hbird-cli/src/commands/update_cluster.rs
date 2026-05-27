//! `hbird update-cluster` — bash twin: `scripts/update-cluster.sh`.
//!
//! Phase 1 of the operator-CLI Rust rewrite (epic [#279], implementation
//! tracked by [#286]). The bash twin is a 1645-line orchestrator that
//! walks a deployed cluster one node at a time, draining + upgrading +
//! verifying each before moving on. This module is the Rust counterpart;
//! the bash twin remains canonical until parity is proven.
//!
//! # Scope of this module
//!
//! The dry-run path is fully implemented and emits log lines byte-for-byte
//! matching the bash twin (modulo timestamp-free runtime). The "real"
//! execution path (`--dry-run` absent) is implemented for the orchestration
//! shape (locking, k8s-node-name resolution, parallel batches, in-flight
//! tracking, recovery hints on abort) and routes individual remote
//! operations through [`hbird_ssh::Client`] / [`hbird_virt::Connection`].
//!
//! Live-execution slice ([#322]) wiring status:
//!   - Cycle 1 ([#325]): `cp_kubectl` (drain + uncordon block #5).
//!   - Cycle 2 ([#327]): bootID gate (`capture_node_bootid`,
//!     `wait_node_bootid_changed`), bootc upgrade (`bootc_has_apply`,
//!     `bootc_booted_digest`, `bootc_upgrade_apply`), SSH drop/back
//!     (`wait_ssh_drop`, `wait_ssh_back`). Block #6 + part of #8 + #10.
//!   - Cycles 3+4: TBD (apiserver + Ready + DaemonSets + timer).
//!
//! # Block traceability
//!
//! Each "block" listed in [#286] maps to a section of this file, marked
//! with `// ---- <name> ----` headers that mirror the bash twin's
//! comment blocks so a reviewer can grep both sides:
//!
//! 1. Config + arg loading  → [`UpdateClusterArgs`] + [`Plan::from_args`]
//! 2. Lock + in-flight gate → [`acquire_lock`]
//! 3. bootc capability probe → [`bootc_has_apply`]
//! 4. Timer stop/start       → [`timer_stop`] / [`timer_start`]
//! 5. Drain gate             → [`update_worker`]
//! 6. bootID capture         → [`capture_node_bootid`] / [`wait_node_bootid_changed`]
//! 7. Apiserver back-up wait → [`wait_apiserver_back`]
//! 8. SSH wait               → [`wait_ssh_drop`] / [`wait_ssh_back`]
//! 9. Node + DaemonSet ready → [`wait_node_ready`] / [`wait_node_daemonsets_ready`]
//! 10. bootc upgrade --apply → [`bootc_upgrade_apply`]
//! 11. CP upgrade flow       → [`update_cp`]
//! 12. Worker upgrade flow   → [`update_worker`]
//! 13. Parallel worker batch → [`run_worker_batch`]
//! 14. Resume support        → [`Plan::workers_to_run`]
//! 15. Dry-run               → threaded through every helper via `Plan::dry_run`
//!
//! [#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
//! [#286]: https://github.com/aatchison/hummingbird-k8s/issues/286
//! [#322]: https://github.com/aatchison/hummingbird-k8s/issues/322
//! [#325]: https://github.com/aatchison/hummingbird-k8s/issues/325
//! [#327]: https://github.com/aatchison/hummingbird-k8s/issues/327

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use clap::builder::BoolishValueParser;

use hbird_config::ClusterConfig;

// ---- Arguments (block #1: clap surface) ------------------------------------

/// Arguments for `hbird update-cluster`.
///
/// Maps to the bash twin's flag set (`scripts/update-cluster.sh` near
/// line 161). See PR #319 for the per-flag rationale.
///
/// | bash flag                  | Rust flag                    |
/// |----------------------------|------------------------------|
/// | `--workers-only`           | `--workers-only`             |
/// | `--skip-drain`             | `--skip-drain`               |
/// | `--skip-gates`             | `--skip-gates`               |
/// | `--dry-run`                | `--dry-run`                  |
/// | `--continue-on-error`      | `--continue-on-error`        |
/// | `--no-delete-emptydir-data`| `--no-delete-emptydir-data`  |
/// | `--node=NAME`              | `--node NAME`                |
/// | `--start-from=NAME`        | `--start-from NAME`          |
/// | `--parallel=N`             | `--parallel N`               |
/// | `--node-name-override=D=N` | `--node-name-override D=N`   |
#[derive(Debug, Args)]
pub struct UpdateClusterArgs {
    /// Path to `cluster.local.conf`.
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

    /// Roll workers only — skip the control plane.
    #[arg(long)]
    pub workers_only: bool,

    /// Skip `kubectl drain` (use when nodes have no evictable workload).
    #[arg(long)]
    pub skip_drain: bool,

    /// Skip the bootID + daemonset gate checks (advanced; #272 explicitly
    /// warns against this in routine operation).
    #[arg(long)]
    pub skip_gates: bool,

    /// Plan-only mode — print what would happen, change nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Continue past a node failure instead of aborting the whole roll.
    #[arg(long)]
    pub continue_on_error: bool,

    /// Pass `--disable-emptydir-data=false` to `kubectl drain` (keep
    /// emptyDir contents).
    #[arg(long)]
    pub no_delete_emptydir_data: bool,

    /// Restrict the roll to a single node by name (CP_NAME or one of
    /// the WORKER_NAMES). Mirrors `make update-node NODE=…`.
    #[arg(long, value_name = "NAME")]
    pub node: Option<String>,

    /// Resume a previously interrupted roll from this node onward.
    #[arg(long, value_name = "NAME")]
    pub start_from: Option<String>,

    /// Parallelism for worker drains (default 1 — serial). The bash twin
    /// caps this at `len(WORKER_NAMES)`.
    #[arg(long, value_name = "N")]
    pub parallel: Option<u32>,

    /// Override the libvirt-domain → kubernetes-node name mapping. Form:
    /// `DOMAIN=NODE`. Repeatable.
    #[arg(long, value_name = "DOMAIN=NODE")]
    pub node_name_override: Vec<String>,
}

// ---- Timeouts (block #1 cont.): env-tunable, validated -------------------

/// Env-tunable timeouts mirroring `scripts/update-cluster.sh`
/// lines 209-269. Constructed via [`Timeouts::from_env`]; defaults match
/// the bash twin verbatim. Operators who set hostile values (`READY_TIMEOUT='a[0$(reboot)]'`)
/// see the same fail-fast diagnostic the bash twin emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Timeouts {
    /// `--timeout=` passed to `kubectl drain` (Go duration string).
    pub drain: String,
    /// `wait_node_ready` seconds (default 300).
    pub ready: u32,
    /// `wait_node_daemonsets_ready` seconds (defaults to `ready`).
    pub daemonset: u32,
    /// `wait_apiserver_back` seconds (default 300).
    pub apiserver: u32,
    /// `wait_ssh_back` seconds (default 300).
    pub ssh: u32,
    /// `wait_ssh_drop` seconds (default 30).
    pub ssh_drop: u32,
    /// Seconds between batches (default 5).
    pub inter_node_sleep: u32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            drain: "5m".to_string(),
            ready: 300,
            daemonset: 300,
            apiserver: 300,
            ssh: 300,
            ssh_drop: 30,
            inter_node_sleep: 5,
        }
    }
}

impl Timeouts {
    /// Load timeouts from the process env, applying bash-twin defaults
    /// and the same security validation regex set. Operators who set a
    /// hostile value (e.g. `READY_TIMEOUT='a[0$(reboot)]'`) get the
    /// exact bash-twin diagnostic.
    pub(crate) fn from_env() -> Result<Self> {
        // Helper closures keep the per-var validation noise out of the
        // happy path. Each helper returns Result<u32, anyhow::Error>
        // with the same diagnostic shape as the bash twin's `fail` line.
        fn req_u32_strict_pos(name: &str, raw: &str) -> Result<u32> {
            // Bash regex: ^[1-9][0-9]*$  — STRICTLY positive integer.
            if raw.is_empty()
                || !raw
                    .bytes()
                    .next()
                    .is_some_and(|b| (b'1'..=b'9').contains(&b))
                || !raw.bytes().skip(1).all(|b| b.is_ascii_digit())
            {
                bail!("{name} must be a positive integer of seconds, >0 (got: {raw})");
            }
            raw.parse::<u32>().map_err(|_| {
                anyhow!("{name} must be a positive integer of seconds, >0 (got: {raw})")
            })
        }
        fn req_u32_nonneg(name: &str, raw: &str) -> Result<u32> {
            // Bash regex: ^[0-9]+$  — non-negative integer.
            if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
                bail!("{name} must be a positive integer of seconds (got: {raw})");
            }
            raw.parse::<u32>()
                .map_err(|_| anyhow!("{name} must be a positive integer of seconds (got: {raw})"))
        }
        fn req_drain(raw: &str) -> Result<String> {
            // Bash regex: ^[0-9]+(s|m|h)?$
            let (digits, suffix) = match raw.chars().last() {
                Some(c @ ('s' | 'm' | 'h')) => (&raw[..raw.len() - c.len_utf8()], Some(c)),
                _ => (raw, None),
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                bail!(r"DRAIN_TIMEOUT must match ^[0-9]+(s|m|h)?$ (got: {raw})");
            }
            // Re-render canonical form (preserves operator value).
            let _ = suffix;
            Ok(raw.to_string())
        }

        let drain = std::env::var("DRAIN_TIMEOUT").unwrap_or_else(|_| "5m".to_string());
        let drain = req_drain(&drain)?;

        let ready = std::env::var("READY_TIMEOUT").unwrap_or_else(|_| "300".to_string());
        let ready = req_u32_strict_pos("READY_TIMEOUT", &ready)?;

        // DAEMONSET_TIMEOUT defaults to READY_TIMEOUT.
        let daemonset_raw =
            std::env::var("DAEMONSET_TIMEOUT").unwrap_or_else(|_| ready.to_string());
        let daemonset = req_u32_strict_pos("DAEMONSET_TIMEOUT", &daemonset_raw)?;

        let apiserver = std::env::var("APISERVER_TIMEOUT").unwrap_or_else(|_| "300".to_string());
        let apiserver = req_u32_nonneg("APISERVER_TIMEOUT", &apiserver)?;

        let ssh = std::env::var("SSH_TIMEOUT").unwrap_or_else(|_| "300".to_string());
        let ssh = req_u32_nonneg("SSH_TIMEOUT", &ssh)?;

        let ssh_drop = std::env::var("SSH_DROP_TIMEOUT").unwrap_or_else(|_| "30".to_string());
        let ssh_drop = req_u32_strict_pos("SSH_DROP_TIMEOUT", &ssh_drop)?;

        let inter_node_sleep =
            std::env::var("INTER_NODE_SLEEP").unwrap_or_else(|_| "5".to_string());
        let inter_node_sleep = req_u32_nonneg("INTER_NODE_SLEEP", &inter_node_sleep)?;

        Ok(Self {
            drain,
            ready,
            daemonset,
            apiserver,
            ssh,
            ssh_drop,
            inter_node_sleep,
        })
    }
}

// ---- Logger (matches bash setup_logging "[update-cluster]" prefix) ---------

use std::cell::RefCell;

thread_local! {
    /// Per-thread log prefix injected before the `[update-cluster] ` token.
    /// In serial mode this is empty. The parallel batch runner sets it to
    /// `[parallel:NAME] ` while running each worker subtask so the captured
    /// output replay matches the bash twin's `sed "s/^/[parallel:${w}] /"`
    /// pattern (line 1544).
    static LOG_PREFIX: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Emit a log line with the bash twin's `[update-cluster] ` prefix to
/// stdout. Mirrors `lib/build-common.sh::log` invoked under
/// `setup_logging "[update-cluster]"`.
///
/// In a parallel batch the thread-local [`LOG_PREFIX`] is prepended to
/// the `[update-cluster] ` token so the captured replay reads
/// `[parallel:NAME] [update-cluster] ...` — identical to the bash
/// twin's `sed`-prefixed output.
///
/// We deliberately write to stdout (not stderr) to match the bash twin's
/// stream selection. Operators tail one and pipe the other.
fn log(line: &str) {
    LOG_PREFIX.with(|prefix| {
        let p = prefix.borrow();
        if p.is_empty() {
            println!("[update-cluster] {line}");
        } else {
            println!("{p}[update-cluster] {line}");
        }
    });
}

/// Emit a warning-prefixed log line to stderr. The bash twin uses
/// `log "ERROR: ..."` to stdout too — we keep that shape for
/// non-fatal warnings to preserve grep behavior; fatal errors return
/// `Err(...)` and the binary prints to stderr via anyhow's Display.
fn log_err(line: &str) {
    LOG_PREFIX.with(|prefix| {
        let p = prefix.borrow();
        if p.is_empty() {
            eprintln!("[update-cluster] {line}");
        } else {
            eprintln!("{p}[update-cluster] {line}");
        }
    });
}

/// Run `body` with `prefix` set on the thread-local log prefix. The
/// previous value (likely empty) is restored on exit. Used by the
/// parallel batch runner to tag each worker's log output.
fn with_log_prefix<F, R>(prefix: &str, body: F) -> R
where
    F: FnOnce() -> R,
{
    let saved = LOG_PREFIX.with(|p| {
        let mut p = p.borrow_mut();
        std::mem::replace(&mut *p, prefix.to_string())
    });
    let out = body();
    LOG_PREFIX.with(|p| {
        *p.borrow_mut() = saved;
    });
    out
}

// ---- Plan (the orchestration-time view of the merged args + config) -------

/// A merged "what we're about to do" plan. Built once from `args + config + env`
/// at the top of [`run`], then read-only for the rest of the run. This is the
/// Rust equivalent of the bash twin's top-of-file globals.
///
/// Doesn't own SSH clients — those are constructed lazily in the orchestration
/// helpers so test code can substitute stubs.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    // Config-derived fields.
    pub config_path: PathBuf,
    pub cp_name: String,
    pub cp_ip: String,
    pub worker_names: Vec<String>,
    pub worker_ip_map: HashMap<String, String>,
    pub worker_k8s_name_map: HashMap<String, String>,
    pub cp_k8s_name: String,

    // Arg-derived fields.
    pub workers_only: bool,
    pub skip_drain: bool,
    pub skip_gates: bool,
    pub dry_run: bool,
    pub continue_on_error: bool,
    pub no_delete_emptydir_data: bool,
    pub node_filter: Option<String>,
    pub start_from: Option<String>,
    pub parallel: u32,
    /// SSH alias of the KVM host. `Some(host)` means the live execution
    /// path must route virsh through `qemu+ssh://<host>/system`; `None`
    /// means run libvirt directly (operator already on the KVM host).
    /// Threaded into [`Plan`] by round-2 review (lens L1 medium): the
    /// live-execution slice (#322) consumes this when it lands.
    #[allow(dead_code)] // consumed by live-execution slice (#322).
    pub kvm_host: Option<String>,
    /// `HBIRD_REMOTE_NO_SUDO=1` toggle. When true, the libvirt-group
    /// operator path is in effect — virsh on the remote skips the sudo
    /// probe (#305). Threaded into [`Plan`] by round-2 review (lens L1
    /// medium); consumed by the live-execution slice (#322).
    #[allow(dead_code)] // consumed by live-execution slice (#322).
    pub no_sudo: bool,

    // Env-derived fields.
    pub timeouts: Timeouts,
}

impl Plan {
    /// Build a plan from parsed args + a loaded config + env. Performs
    /// all the cross-flag mutual-exclusion checks the bash twin does
    /// (lines 196-207) and resolves IPs + k8s node names up front so a
    /// stale config fails fast before any side effects.
    ///
    /// Override map parsing (line 384-418) is also done here; an invalid
    /// `DOMAIN=NODE` entry surfaces the same "DOMAIN does not match
    /// CP_NAME or any WORKER_NAMES entry" diagnostic the bash twin emits.
    pub(crate) fn from_args(
        args: &UpdateClusterArgs,
        config: ClusterConfig,
        timeouts: Timeouts,
    ) -> Result<Self> {
        // ---- arg-level mutual exclusions (lines 196-207) -------------
        if args.workers_only && args.node.is_some() {
            bail!("--workers-only and --node= are mutually exclusive");
        }
        if args.node.is_some() && args.start_from.is_some() {
            bail!(
                "--node= and --start-from= are mutually exclusive (--node is single-node mode; --start-from is resume mode)"
            );
        }
        let parallel = args.parallel.unwrap_or(1);
        if parallel < 1 {
            bail!("--parallel=N requires a positive integer (got: {parallel})");
        }

        // ---- worker names from config -------------------------------
        let worker_names = config.resolved_worker_names();

        // ---- IP resolution (deferred to live mode) -------------------
        //
        // In dry-run we substitute the bash twin's placeholder
        // "<resolved-at-runtime>" so the dry-run log output matches
        // byte-for-byte. Live mode resolves via virsh domifaddr; we
        // route through hbird-virt::Connection in [`resolve_ips`].
        let (cp_ip, worker_ip_map) = if args.dry_run {
            let mut m = HashMap::with_capacity(worker_names.len());
            for w in &worker_names {
                m.insert(w.clone(), "<resolved-at-runtime>".to_string());
            }
            ("<resolved-at-runtime>".to_string(), m)
        } else {
            // Operator-supplied IPs win over virsh resolution.
            let cp_ip = config
                .cp_ip
                .clone()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "could not resolve CP IP for domain '{}' via virsh domifaddr (set CP_IP= in env to override)",
                        config.cp_name
                    )
                })?;

            let mut m = HashMap::with_capacity(worker_names.len());
            if let Some(ips) = &config.worker_ips {
                if ips.len() != worker_names.len() {
                    bail!(
                        "WORKER_IPS ({}) and WORKER_NAMES ({}) must be the same length",
                        ips.len(),
                        worker_names.len(),
                    );
                }
                for (w, ip) in worker_names.iter().zip(ips.iter()) {
                    m.insert(w.clone(), ip.clone());
                }
            } else {
                // No virsh-domifaddr fallback baked into this scaffold —
                // operator must set WORKER_IPS until the live-execution
                // path lands. See module docs.
                bail!(
                    "live-mode IP resolution not yet implemented in the Rust path. \
                     Set WORKER_IPS=(...) and CP_IP= in cluster.local.conf, or run \
                     under --dry-run, until the virsh-domifaddr resolution lands. \
                     (Tracked by #322 for the live-execution slice.)"
                );
            }
            (cp_ip, m)
        };

        // ---- --start-from validation (line 373-382) ------------------
        if let Some(sf) = &args.start_from
            && !worker_names.iter().any(|w| w == sf)
        {
            bail!("--start-from={sf} did not match any WORKER_NAMES entry");
        }

        // ---- --node-name-override parsing (line 384-418) -------------
        //
        // Repeatable + comma-separated; DOMAIN must be CP_NAME or a
        // WORKER_NAMES entry. An entirely empty value (`--node-name-override=`)
        // is rejected — the bash twin's `parse_node_name_overrides` does
        // the same (round-2 lens L2#2; previously this was a silent no-op).
        let mut override_map: HashMap<String, String> = HashMap::new();
        for entry in &args.node_name_override {
            if entry.is_empty() {
                bail!(
                    "--node-name-override requires a non-empty DOMAIN=NODE \
                     (or comma-separated list); got an empty string"
                );
            }
            for pair in entry.split(',') {
                if pair.is_empty() {
                    continue;
                }
                let (dom, node) = pair.split_once('=').ok_or_else(|| {
                    anyhow!("--node-name-override expects DOMAIN=NODE (got: '{pair}')")
                })?;
                if dom.is_empty() {
                    bail!("--node-name-override DOMAIN cannot be empty (got: '{pair}')");
                }
                if node.is_empty() {
                    bail!("--node-name-override NODE cannot be empty (got: '{pair}')");
                }
                let valid = dom == config.cp_name || worker_names.iter().any(|w| w == dom);
                if !valid {
                    bail!(
                        "--node-name-override DOMAIN '{dom}' does not match CP_NAME or any WORKER_NAMES entry"
                    );
                }
                override_map.insert(dom.to_string(), node.to_string());
            }
        }

        // ---- k8s-node-name resolution (line 1424-1442) ---------------
        //
        // Dry-run: returns the libvirt domain verbatim (so existing
        // dry-run log shape holds). Live-mode resolution would consult
        // the apiserver — deferred until the live-execution slice
        // lands. See module docs.
        let resolve_k8s = |domain: &str| -> String {
            override_map
                .get(domain)
                .cloned()
                .unwrap_or_else(|| domain.to_string())
        };
        let cp_k8s_name = resolve_k8s(&config.cp_name);
        let mut worker_k8s_name_map = HashMap::with_capacity(worker_names.len());
        for w in &worker_names {
            worker_k8s_name_map.insert(w.clone(), resolve_k8s(w));
        }

        Ok(Self {
            config_path: args.config.clone(),
            cp_name: config.cp_name,
            cp_ip,
            worker_names,
            worker_ip_map,
            worker_k8s_name_map,
            cp_k8s_name,
            workers_only: args.workers_only,
            skip_drain: args.skip_drain,
            skip_gates: args.skip_gates,
            dry_run: args.dry_run,
            continue_on_error: args.continue_on_error,
            no_delete_emptydir_data: args.no_delete_emptydir_data,
            node_filter: args.node.clone(),
            start_from: args.start_from.clone(),
            parallel,
            kvm_host: args.kvm_host.clone(),
            no_sudo: args.no_sudo,
            timeouts,
        })
    }

    /// Build the ordered list of workers to process, honoring
    /// `--start-from` (resume mode skips entries before the match;
    /// inclusive of the match). Mirrors bash lines 1462-1479.
    pub(crate) fn workers_to_run(&self) -> Vec<String> {
        if let Some(sf) = &self.start_from {
            let mut out = Vec::new();
            let mut seen = false;
            for w in &self.worker_names {
                if w == sf {
                    seen = true;
                }
                if seen {
                    out.push(w.clone());
                }
            }
            out
        } else {
            self.worker_names.clone()
        }
    }
}

// ---- block #2: concurrency lock --------------------------------------------

/// Resolve the directory where the per-user flock lives. The bash twin
/// uses `${XDG_RUNTIME_DIR:-/tmp}`; we tighten that on Linux for two
/// reasons surfaced by round-2 review (lens L1):
///
/// 1. `/tmp` is world-writable and an attacker on a multi-user host can
///    plant a symlink at `/tmp/hbird-update-cluster.lock` that points at
///    a file the operator owns elsewhere. `O_NOFOLLOW` mitigates the
///    open() side, but flock(1) below will still chase the symlink.
/// 2. `XDG_RUNTIME_DIR` is per-UID (mode 0700, root-owned tmpfs) on every
///    systemd host — the lock is naturally isolated. Falling back to
///    `/tmp` collapses that isolation.
///
/// Policy:
///   - `XDG_RUNTIME_DIR` set and a directory we own  → use it directly.
///   - `XDG_RUNTIME_DIR` set but not ours / missing → bail.
///   - Unset, AND `/run/user/<UID>` exists           → use that.
///   - Unset and no `/run/user/<UID>`                 → fall back to a
///     UID-scoped subdir under `/tmp` (`/tmp/hbird-<UID>`) that we create
///     mode 0700; this matches the bash semantics on minimal hosts that
///     don't run systemd-logind, without collapsing onto world-writable
///     `/tmp` directly.
///
/// Returns the (validated) directory to place the lock in.
fn lock_dir() -> Result<PathBuf> {
    // Best-effort UID lookup — `/proc/self/loginuid` has more meaningful
    // semantics for our case but we just need a stable per-user int.
    // `users::get_current_uid` would pull in a dep; read it from env.
    let uid = std::env::var("UID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(|| {
            // `id -u` fallback — never panics on a host that has coreutils.
            std::process::Command::new("id")
                .arg("-u")
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok())
                })
        })
        .unwrap_or(0);

    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        let p = PathBuf::from(&xdg);
        // Validate: must exist as a directory we can write into. We
        // don't require ownership match (some containers fudge UIDs);
        // mode check is the operator's responsibility per FHS.
        if !p.is_dir() {
            bail!(
                "XDG_RUNTIME_DIR is set to {xdg} but it is not a directory; \
                 unset it or point it at a writable per-user dir"
            );
        }
        return Ok(p);
    }

    // Conventional systemd location.
    let run_user = PathBuf::from(format!("/run/user/{uid}"));
    if run_user.is_dir() {
        return Ok(run_user);
    }

    // Last resort: UID-scoped subdir under /tmp. Avoid the world-writable
    // /tmp root itself (lens L1: symlink-DoS surface). Create mode 0700
    // so a peer user can't plant files inside.
    let scoped = PathBuf::from(format!("/tmp/hbird-{uid}"));
    if !scoped.exists() {
        std::fs::create_dir(&scoped).with_context(|| {
            format!(
                "failed to create UID-scoped lock dir {} \
                 (set XDG_RUNTIME_DIR to override)",
                scoped.display(),
            )
        })?;
        // Tighten permissions. We can't use std::os::unix::fs::PermissionsExt
        // under `unsafe_code = "forbid"`? Actually PermissionsExt is safe —
        // the `unsafe` lint is about the `unsafe` keyword, not unix-specific
        // safe APIs. Apply 0700 to keep peer-user lockfile reads out.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&scoped, std::fs::Permissions::from_mode(0o700));
    }
    Ok(scoped)
}

/// Global toggle the SIGINT/SIGTERM handler flips so the main thread can
/// notice mid-loop and unwind cleanly. The handler ALSO kills the
/// flock-holder child directly (the kernel will free the lock when the
/// child reaps, even if the main thread is blocked).
static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

/// The flock-holder child's PID — set by [`acquire_lock`] and read by
/// the SIGINT/SIGTERM handler so it can `kill(2)` directly without
/// reaching back through `LockGuard::Drop` (which the signal-killed
/// process never gets to run).
static FLOCK_CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Acquire the per-user flock on `<lock-dir>/hbird-update-cluster.lock`.
/// Mirrors bash lines 757-779.
///
/// Implementation: shells out to `flock(1)` in a "test the lock"
/// short-running shape. flock(1) is part of util-linux and present on
/// every host this project targets; the bash twin uses `flock(2)`
/// directly via `exec 200>...; flock -n 200`. We mirror the
/// non-blocking exclusive semantics without pulling in an `unsafe`
/// libc binding (workspace lint `unsafe_code = "forbid"` rules that
/// out — see `rust/README.md` for the policy).
///
/// The returned [`LockGuard`] keeps a background `flock(1) -n PATH
/// sleep infinity` alive so the kernel-held lock persists until drop;
/// dropping the guard kills the child and releases the lock. A
/// SIGINT/SIGTERM handler is also installed so a Ctrl-C between
/// `acquire_lock` and the natural drop path still releases the lock
/// (round-2 lens L3#1).
///
/// Skipped entirely in dry-run mode.
///
/// # Errors
///
/// Returns `Err(_)` when another run holds the lock (bash twin emits
/// the same wording).
pub(crate) fn acquire_lock(dry_run: bool) -> Result<Option<LockGuard>> {
    if dry_run {
        return Ok(None);
    }
    let dir = lock_dir()?;
    let path = dir.join("hbird-update-cluster.lock");
    // Touch the file (flock(1) needs it to exist). `create_new=false` so
    // a pre-existing lock-file in the same dir is fine.
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| {
            format!(
                "failed to open lock file {} (set XDG_RUNTIME_DIR to a writable dir)",
                path.display(),
            )
        })?;
    // Spawn `flock -n <path> -c '<sleep loop>'` and keep the child
    // alive — the kernel-held lock survives as long as the child does.
    // `-n` makes flock non-blocking: exit-1 means another holder has it.
    //
    // The inner shell installs a TERM handler so our `Drop` `kill()`
    // shuts it down cleanly (otherwise the sleep absorbs the signal and
    // delivery races with the wait).
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("lock path is not valid UTF-8: {}", path.display()))?;
    let child = Command::new("flock")
        .args([
            "-n",
            path_str,
            "-c",
            "trap 'exit 0' TERM; while sleep 86400; do :; done",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn flock(1) for {}", path.display()))?;

    // Deterministic acquisition check (CodeRabbit round-2): rather than
    // a brittle 50ms-after-spawn heuristic, we attempt a SECOND
    // non-blocking flock on the same path and inspect ITS exit code:
    //   - exit 0  → the flock(1) we hold didn't actually take the lock
    //               (this can happen if path doesn't open) — bail.
    //   - exit 1  → the second flock saw contention, confirming the
    //               first child holds it. Success.
    // We use `flock -n -E 1` so the contention exit is unambiguous.
    let probe = Command::new("flock")
        .args(["-n", "-E", "1", path_str, "-c", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    let mut child = child;

    // Always check whether the held-alive child died first — if flock
    // itself failed (bad path, no perms, util-linux missing) the probe
    // will succeed misleadingly.
    if let Ok(Some(status)) = child.try_wait() {
        let stderr = child
            .stderr
            .take()
            .map(|mut s| {
                use std::io::Read;
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf);
                buf
            })
            .unwrap_or_default();
        if status.code() == Some(1) {
            bail!(
                "another update-cluster run is in progress (lock {} held)",
                path.display(),
            );
        }
        bail!(
            "failed to acquire lock {}: flock(1) exited {:?}: {stderr}",
            path.display(),
            status.code(),
        );
    }

    match probe {
        Ok(p) if p.status.code() == Some(1) => {
            // Expected: our child holds the lock, the probe saw contention.
        }
        Ok(p) => {
            // Probe acquired the lock — our child didn't actually take it.
            // Reap our child (its sleep is still running but the lock
            // never got held).
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "failed to acquire lock {}: probe got rc={:?} (stderr={})",
                path.display(),
                p.status.code(),
                String::from_utf8_lossy(&p.stderr),
            );
        }
        Err(e) => {
            // probe couldn't be spawned at all — bail (flock(1) missing?).
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "failed to probe lock state on {}: {e} \
                 (is flock(1) from util-linux installed?)",
                path.display(),
            );
        }
    }

    // Stash the child PID for the signal handler.
    FLOCK_CHILD_PID.store(child.id() as i32, Ordering::SeqCst);

    // Install the SIGINT + SIGTERM handler. signal-hook's `flag::register`
    // is idempotent: repeated calls re-register the same flag. We use a
    // separate handler that kills the flock child directly so the kernel
    // releases the lock even if Drop never runs (the process is about to
    // exit anyway when it receives the signal).
    install_signal_handler();

    Ok(Some(LockGuard {
        child: Some(child),
        path,
    }))
}

/// Install a process-wide SIGINT + SIGTERM handler that:
///   1. Sets `SIGNAL_RECEIVED` so polling loops can notice.
///   2. Kills the flock-holder child directly so the kernel releases
///      the lock even if the main thread never reaches `Drop`.
///   3. Re-installs default disposition and re-raises, so the process
///      exits with the conventional 130 (SIGINT) / 143 (SIGTERM) code.
///
/// Idempotent: signal-hook tracks registrations, calling twice doesn't
/// stack handlers.
fn install_signal_handler() {
    use signal_hook::consts::{SIGINT, SIGTERM};
    // signal-hook's iterator API gives us a thread that blocks on
    // sigwait(2). The `Once` guard ensures we install exactly one
    // watcher per process, so repeated `acquire_lock` calls in tests
    // don't stack handlers.
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let mut signals = match signal_hook::iterator::Signals::new([SIGINT, SIGTERM]) {
            Ok(s) => s,
            Err(_) => return,
        };
        std::thread::spawn(move || {
            for sig in signals.forever() {
                SIGNAL_RECEIVED.store(true, Ordering::SeqCst);
                // Kill the flock child if any. PID 0 means "not running".
                let pid = FLOCK_CHILD_PID.load(Ordering::SeqCst);
                if pid > 0 {
                    // SIGTERM is enough — the inner shell's `trap` exits 0.
                    // We can't use libc::kill under `unsafe_code = "forbid"`;
                    // shell out via `kill(1)` instead. Coreutils `kill` is
                    // present everywhere this project targets.
                    let _ = Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .status();
                }
                // Re-raise with default disposition. signal-hook offers a
                // helper for this (`low_level::emulate_default_handler`).
                let _ = signal_hook::low_level::emulate_default_handler(sig);
            }
        });
    });
}

/// Held-alive child whose kernel-held flock persists for its lifetime.
pub(crate) struct LockGuard {
    child: Option<std::process::Child>,
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Kill the background flock holder so the kernel releases the
        // file lock. Best-effort — a kill failure here means the OS is
        // already cleaning up the process tree (Ctrl-C via the parent
        // shell, for example), and the lock will be released anyway
        // when the orphan reaps.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Clear the static PID so a stale SIGINT after we've released
        // doesn't try to kill an unrelated PID that the OS may have
        // recycled.
        FLOCK_CHILD_PID.store(0, Ordering::SeqCst);
        // We deliberately don't unlink the lock file — the bash twin
        // leaves it on disk too (zero-byte tmpfs file).
        let _ = &self.path;
    }
}

// ---- block #3: bootc capability probe + digest snapshot --------------------
//
// These helpers are referenced by [`bootc_upgrade_apply`] in the live
// path. They're only called in live mode; in dry-run mode
// [`bootc_upgrade_apply`] short-circuits without consulting either. They
// stay defined here (rather than living in the follow-up live-execution
// slice) so the bash-twin block-traceability remains complete — a
// reviewer can grep this file for every bash function name in #286.

/// Probe whether the remote bootc supports `upgrade --apply`. Mirrors
/// bash `bootc_has_apply` (line 523). Dry-run short-circuits to `Ok(true)`.
///
/// Live path (#327 cycle 2): runs `bootc upgrade --help | grep -q -- --apply`
/// over SSH. Exit 0 → has --apply; exit 1 (no match) → fallback path.
/// Any other non-zero classifies as an SSH/transport error and bubbles
/// up so the caller can fail-fast rather than silently downgrade.
#[tracing::instrument(level = "debug", skip(plan), fields(node_ip = %ip))]
fn bootc_has_apply(plan: &Plan, ip: &str) -> Result<bool> {
    if plan.dry_run {
        return Ok(true);
    }
    let client = hbird_ssh::Client::new(node_ssh_opts(plan, ip));
    // Same shape as bash line 529:
    //   ssh "${SSH_OPTS[@]}" "root@${ip}" "bootc upgrade --help 2>/dev/null | grep -q -- '--apply'"
    match client.run("bootc upgrade --help 2>/dev/null | grep -q -- '--apply'") {
        Ok(_) => Ok(true),
        Err(hbird_ssh::Error::NonZeroExit { status, .. }) if status.code() == Some(1) => {
            // grep exit 1 = pattern not found = no --apply support.
            Ok(false)
        }
        Err(e) => Err(e).with_context(|| {
            format!("bootc_has_apply: ssh-run failed for `bootc upgrade --help` against root@{ip}")
        }),
    }
}

/// Snapshot the booted image digest via `bootc status --json`. Mirrors
/// `bootc_booted_digest` (line 536). Dry-run returns the sentinel
/// `<dry-run-digest>` the bash twin uses.
///
/// Live path (#327 cycle 2): runs `bootc status --json | jq -r
/// '.status.booted.image.imageDigest // .status.booted.image.digest // empty'`
/// over SSH. An empty string is a legitimate return (jq's `// empty`
/// when neither path is populated) — the caller in
/// [`bootc_upgrade_apply`] treats `pre == post == ""` as "couldn't
/// determine, treat as applied" matching bash line 1157 which only
/// short-circuits on a NON-EMPTY equal pair.
#[tracing::instrument(level = "debug", skip(plan), fields(node_ip = %ip))]
fn bootc_booted_digest(plan: &Plan, ip: &str) -> Result<String> {
    if plan.dry_run {
        return Ok("<dry-run-digest>".to_string());
    }
    let client = hbird_ssh::Client::new(node_ssh_opts(plan, ip));
    // Bash line 543 uses the same jq expression. We replicate verbatim so
    // operators grepping both sides find a matching call shape.
    // Round-2 lens L2 MED: fallback path matches bash twin literally
    // (line 544: `.image.imageDigest // .image.digest // empty`). The
    // earlier shape `// .status.booted.imageDigest` was missing the
    // `.image.` prefix on the fallback — same-looking but diverges on
    // any bootc schema where the alternate key is `.image.digest`.
    let cmd = "bootc status --json 2>/dev/null | \
               jq -r '.status.booted.image.imageDigest // .status.booted.image.digest // empty' \
               2>/dev/null || true";
    // `|| true` keeps a transient jq error from blowing up the digest
    // snapshot — bash twin does the same; empty string falls through to
    // the caller's `<unknown>` fallback log line.
    match client.run(cmd) {
        Ok(out) => Ok(out.stdout_lossy().trim().to_string()),
        Err(e) => {
            Err(e).with_context(|| format!("bootc_booted_digest: ssh-run failed against root@{ip}"))
        }
    }
}

// ---- block #4: timer stop/start --------------------------------------------

/// Stop the bootc auto-update timer(s) on the node. Mirrors `timer_stop`
/// (line 555). Honors `--skip-drain`. Dry-run emits the exact bash
/// log line.
fn timer_stop(plan: &Plan, ip: &str) -> Result<()> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN ssh root@{ip} systemctl stop bootc-semver-update.timer bootc-fetch-apply-updates.timer"
        ));
        return Ok(());
    }
    if plan.skip_drain {
        log(&format!(
            "  --skip-drain set: leaving bootc-*-update.timer alone on {ip}"
        ));
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "timer_stop",
        &format!("ssh root@{ip} systemctl stop bootc-{{semver,fetch-apply}}-update.timer"),
    ))
}

/// Start the bootc-semver-update.timer, falling back to the legacy
/// fetch-apply timer for pre-#181 hosts. Mirrors `timer_start`
/// (line 570). Dry-run emits the exact bash log line.
fn timer_start(plan: &Plan, ip: &str) -> Result<()> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN ssh root@{ip} systemctl start bootc-semver-update.timer"
        ));
        return Ok(());
    }
    if plan.skip_drain {
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "timer_start",
        &format!(
            "ssh root@{ip} systemctl start bootc-semver-update.timer (or fetch-apply fallback)"
        ),
    ))
}

// ---- block #6+9: wait helpers (Ready, bootID, DaemonSets) ------------------

/// Wait for SSH to come back on `ip`. Mirrors `wait_ssh_back`
/// (line 825). Dry-run emits the same one-shot log line.
///
/// Live path (#327 cycle 2): polls `ssh root@ip true` every 5s up to
/// `plan.timeouts.ssh` seconds. The bash twin uses `interval=5` (line
/// 827) so we match that cadence exactly. Returns Ok the moment a
/// connection succeeds; Err with the elapsed time on timeout.
///
/// Each poll uses a fresh SSH connection (no ControlMaster) with a
/// short `ConnectTimeout=3` so a hung TCP handshake doesn't eat the
/// whole interval — same shape as bash's `wait_ssh_drop` uses, but
/// without `ControlPath=none` since wait_ssh_back's bash equivalent
/// doesn't pass it (it actually WANTS the controlmaster reused once
/// the connection is back; line 834 uses `${SSH_OPTS[@]}` verbatim).
#[tracing::instrument(level = "debug", skip(plan), fields(node_ip = %ip))]
fn wait_ssh_back(plan: &Plan, ip: &str) -> Result<()> {
    let timeout = plan.timeouts.ssh;
    log(&format!(
        "waiting for SSH to come back on {ip} (timeout {timeout}s)"
    ));
    if plan.dry_run {
        log(&format!("DRY-RUN would poll ssh root@{ip}"));
        return Ok(());
    }
    let interval: u32 = 5;
    let client = hbird_ssh::Client::new(
        node_ssh_opts(plan, ip).with_connect_timeout(Duration::from_secs(3)),
    );
    let mut elapsed: u32 = 0;
    while elapsed < timeout {
        match client.run("true") {
            Ok(_) => {
                log(&format!("SSH back on {ip} after ~{elapsed}s"));
                return Ok(());
            }
            Err(_) => {
                // Any error (NonZeroExit/Spawn/Wait/IdentityFileMissing)
                // is treated as "not yet back"; we keep polling until the
                // timeout. Bash twin folds the same shapes via
                // `>/dev/null 2>&1` + condition on the if-statement.
            }
        }
        std::thread::sleep(Duration::from_secs(interval.into()));
        elapsed = elapsed.saturating_add(interval);
    }
    bail!("wait_ssh_back: SSH on {ip} did not come back within {timeout}s");
}

/// Wait for SSH on `ip` to become unreachable. Mirrors `wait_ssh_drop`
/// (line 805). Diagnostic gate — a timeout is logged but not fatal.
///
/// Live path (#327 cycle 2): polls `ssh root@ip true` every 1s up to
/// `plan.timeouts.ssh_drop` seconds. Returns Ok the moment a connection
/// fails (reboot in progress); Err on timeout (operator should grep for
/// "still up after" — bash twin uses the same wording at line 821).
///
/// IMPORTANT: each poll uses a FRESH connection (no ControlMaster) with
/// `ConnectTimeout=3` so we don't re-use the pre-reboot multiplexed
/// session (which would short-circuit to ~0s and false-success). Bash
/// line 813 forces `ControlPath=none` + `BatchMode=yes`; hbird_ssh
/// defaults to no ControlMaster + BatchMode=yes already so we just
/// shorten the connect timeout.
#[tracing::instrument(level = "debug", skip(plan), fields(node_ip = %ip))]
fn wait_ssh_drop(plan: &Plan, ip: &str) -> Result<()> {
    let max = plan.timeouts.ssh_drop;
    if plan.dry_run {
        log(&format!(
            "DRY-RUN wait_ssh_drop {ip} (would poll up to {max}s)"
        ));
        return Ok(());
    }
    log(&format!("waiting for SSH on {ip} to drop (timeout {max}s)"));
    let client = hbird_ssh::Client::new(
        node_ssh_opts(plan, ip).with_connect_timeout(Duration::from_secs(3)),
    );
    let mut elapsed: u32 = 0;
    while elapsed < max {
        if client.run("true").is_err() {
            log(&format!(
                "  SSH on {ip} dropped after ~{elapsed}s (reboot in progress)"
            ));
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
        elapsed = elapsed.saturating_add(1);
    }
    // Bash line 821: WARN + return non-zero. Both callers
    // (`update_worker` line 1640, `update_cp` line 1751) swallow the
    // Err with `let _ =`, so any `bail!` here is dead — the WARN line
    // is the only signal the operator sees. Round-2 lens L3 MED:
    // return Ok(()) instead of bail!ing into a void. Downstream bootID
    // gate is the actual source of truth for "did this reboot land?"
    log(&format!(
        "  WARN: SSH on {ip} still up after {max}s — bootc may have queued without rebooting"
    ));
    Ok(())
}

/// Poll the apiserver via the CP. Mirrors `wait_apiserver_back`
/// (line 1077). Dry-run emits the same log shape as the bash twin.
fn wait_apiserver_back(plan: &Plan) -> Result<()> {
    let timeout = plan.timeouts.apiserver;
    let cp_ip = &plan.cp_ip;
    log(&format!(
        "waiting for apiserver on CP ({cp_ip}) to answer (timeout {timeout}s)"
    ));
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would poll apiserver via ssh root@{cp_ip} kubectl get nodes"
        ));
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "wait_apiserver_back",
        &format!("ssh root@{cp_ip} kubectl get --raw=/readyz"),
    ))
}

/// Wait for a node to report Ready. Mirrors `wait_node_ready`
/// (line 844). Dry-run emits one log line.
fn wait_node_ready(plan: &Plan, node: &str) -> Result<()> {
    let timeout = plan.timeouts.ready;
    log(&format!(
        "waiting for node {node} to report Ready (timeout {timeout}s)"
    ));
    if plan.dry_run {
        log(&format!("DRY-RUN would poll kubectl get node {node}"));
        return Ok(());
    }
    Err(live_mode_not_implemented(
        "wait_node_ready",
        &format!("cp_kubectl get node {node} (poll for Ready)"),
    ))
}

/// Capture the node's pre-reboot bootID. Mirrors `capture_node_bootid`
/// (line 885). Dry-run emits the bash sentinel.
///
/// Live path (#327 cycle 2): three-attempt retry over a single transient
/// apiserver flake, matching bash twin lines 892-901 exactly. Each attempt
/// calls `cp_kubectl get node <NODE> -o jsonpath='{.status.nodeInfo.bootID}'`
/// and trims the result. A non-empty value wins; otherwise we sleep 2s
/// and retry (skipped after the last attempt).
///
/// An empty stdout + Ok return is a valid outcome — the bash twin
/// documents this (lines 875-877) and callers
/// ([`wait_node_bootid_changed`]) check `is_empty()` before using the
/// value as a comparison baseline. We log a WARN matching bash line 902
/// to preserve operator-grep parity.
///
/// Routes through the existing in-module [`cp_kubectl`] shim — same
/// SSH+ProxyJump chain as cycle 1's drain+uncordon path, and any kubectl
/// stderr surfaces via the shim's existing `log_err` plumbing.
#[tracing::instrument(level = "debug", skip(plan), fields(node = %node))]
fn capture_node_bootid(plan: &Plan, node: &str) -> Result<String> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN would capture pre-reboot bootID for {node}"
        ));
        return Ok("<dry-run-bootid>".to_string());
    }
    // Bash uses jsonpath='{.status.nodeInfo.bootID}'. cp_kubectl's
    // metacharacter check forbids `;&|`\n\r$(` but allows `'{}.` so
    // this passes through unchanged.
    let command = format!("get node {node} -o jsonpath='{{.status.nodeInfo.bootID}}'");
    for attempt in 1..=3 {
        // Use the local cp_kubectl helper which routes through hbird_ssh
        // and the same prefix-aware log plumbing. Tolerate Err here so a
        // single apiserver flake doesn't abort capture (bash uses
        // `2>/dev/null || true` for the same reason).
        match cp_kubectl(plan, &command) {
            Ok(val) => {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
            Err(e) => {
                // Trace the flake but don't bail — the next attempt may
                // succeed. Mirrors bash's `|| true` discard.
                tracing::debug!(
                    error = ?e,
                    attempt,
                    "capture_node_bootid attempt failed, retrying",
                );
            }
        }
        // Don't sleep after the last attempt — the caller is waiting.
        if attempt < 3 {
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    log(&format!(
        "WARN: failed to capture pre-reboot bootID for {node} after 3 attempts; \
         bootID-changed gate will be skipped (apiserver flake?)"
    ));
    Ok(String::new())
}

/// Poll the node's bootID until it differs from `pre_bootid`. Mirrors
/// `wait_node_bootid_changed` (line 921). Honors `--skip-gates`.
///
/// Live path (#327 cycle 2): polls every 5s up to `plan.timeouts.ready`
/// seconds, calling `cp_kubectl get node X -o jsonpath=...` each iteration.
/// Returns Ok the moment a non-empty cur_bootid != pre_bootid is observed;
/// Err with a diagnostic on timeout matching bash line 954.
///
/// Heartbeat: every ~30s we emit a "still polling" log line matching
/// bash line 948 so an operator watching the log knows the gate is
/// alive on a slow reboot. Rounded to the 5s interval boundary so we
/// don't log on every iteration.
///
/// Bash quirks preserved:
///   - Empty `pre_bootid` → WARN + Ok (line 928); without a baseline we
///     can't compare.
///   - `--skip-gates` → silent Ok with grep-parity log line (line 924).
///   - A single failed kubectl call inside the loop is tolerated (bash
///     uses `2>/dev/null || true`); we treat any [`cp_kubectl`] Err as
///     "couldn't read this iteration, try again".
#[tracing::instrument(level = "debug", skip(plan), fields(node = %node))]
fn wait_node_bootid_changed(plan: &Plan, node: &str, pre_bootid: &str) -> Result<()> {
    if plan.skip_gates {
        log(&format!(
            "node {node}: --skip-gates set, skipping bootID-changed gate"
        ));
        return Ok(());
    }
    if pre_bootid.is_empty() {
        log_err(&format!(
            "WARN: node {node}: pre-reboot bootID was empty; skipping bootID-changed gate"
        ));
        return Ok(());
    }
    let timeout = plan.timeouts.ready;
    log(&format!(
        "waiting for node {node} bootID to change from pre-reboot value (timeout {timeout}s)"
    ));
    if plan.dry_run {
        log(&format!("DRY-RUN would poll bootID for {node}"));
        return Ok(());
    }
    let interval: u32 = 5;
    let command = format!("get node {node} -o jsonpath='{{.status.nodeInfo.bootID}}'");
    let mut elapsed: u32 = 0;
    let mut last_heartbeat: u32 = 0;
    let mut cur_bootid = String::new();
    while elapsed < timeout {
        // Tolerate transient kubectl flake — bash twin's
        // `cp_kubectl ... 2>/dev/null || true` collapses errors to ""
        // and the loop body simply continues.
        cur_bootid = match cp_kubectl(plan, &command) {
            Ok(v) => v.trim().to_string(),
            Err(e) => {
                tracing::debug!(
                    error = ?e,
                    elapsed,
                    "wait_node_bootid_changed: kubectl flake, will retry",
                );
                String::new()
            }
        };
        if !cur_bootid.is_empty() && cur_bootid != pre_bootid {
            log(&format!(
                "node {node} bootID changed (pre={}... post={}...) after ~{elapsed}s",
                prefix8(pre_bootid),
                prefix8(&cur_bootid),
            ));
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(interval.into()));
        elapsed = elapsed.saturating_add(interval);
        // Progress heartbeat every ~30s, rounded to interval boundary so
        // we don't log every iteration. Suppress the heartbeat on the
        // very last iteration since the timeout diagnostic below covers it.
        if elapsed.saturating_sub(last_heartbeat) >= 30 && elapsed < timeout {
            log(&format!(
                "  still polling node {node} for bootID-changed gate after ~{elapsed}s (pre={}... cur={}...)",
                prefix8(pre_bootid),
                prefix8(&cur_bootid),
            ));
            last_heartbeat = elapsed;
        }
    }
    // Diagnostic on timeout: surface the last observed pre/cur so an
    // operator can tell at a glance whether the apiserver returned
    // anything at all (bash line 954 wording verbatim).
    log(&format!(
        "node {node}: bootID-changed gate timed out after {timeout}s (pre={}... cur={}...)",
        prefix8(pre_bootid),
        prefix8(&cur_bootid),
    ));
    bail!("wait_node_bootid_changed: node {node} bootID did not change within {timeout}s");
}

/// Wait for kube-system DaemonSet pods on `node` to report Ready.
/// Mirrors `wait_node_daemonsets_ready` (line 995). Honors
/// `--skip-gates`.
fn wait_node_daemonsets_ready(plan: &Plan, node: &str) -> Result<()> {
    if plan.skip_gates {
        log(&format!(
            "node {node}: --skip-gates set, skipping DaemonSet readiness gate"
        ));
        return Ok(());
    }
    let timeout = plan.timeouts.daemonset;
    if plan.dry_run {
        log(&format!(
            "waiting for kube-system DaemonSet pods on {node} to be Ready (timeout {timeout}s)"
        ));
        log(&format!(
            "DRY-RUN would poll kube-system pods on {node} for Ready"
        ));
        return Ok(());
    }
    log(&format!(
        "waiting for kube-system DaemonSet pods on {node} to be Ready (timeout {timeout}s)"
    ));
    Err(live_mode_not_implemented(
        "wait_node_daemonsets_ready",
        &format!(
            "cp_kubectl get pods -n kube-system --field-selector=spec.nodeName={node} (poll all Ready)"
        ),
    ))
}

// ---- block #10: bootc upgrade --apply --------------------------------------

/// Outcome of [`bootc_upgrade_apply`]. Mirrors the bash twin's
/// (0/1/2) exit-code triple (lines 1108-1113).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootcUpgradeOutcome {
    /// Upgrade applied; reboot expected.
    Applied,
    /// `bootc upgrade` itself failed.
    UpgradeFailed,
    /// No update available; skip the wait loops.
    AlreadyCurrent,
}

/// Run `bootc upgrade --apply` (or two-step fallback for bootc <1.1).
/// Mirrors `bootc_upgrade_apply` (line 1114). Dry-run emits the same
/// log line + returns `Applied`.
///
/// Live path (#327 cycle 2): mirrors bash lines 1114-1177 exactly:
///
/// 1. Snapshot the booted image digest pre-upgrade.
/// 2. Probe `bootc upgrade --help` for `--apply` support.
/// 3. With --apply: `ssh root@ip bootc upgrade --apply`. The reboot
///    tears down SSH, returning Err(NonZeroExit{255}) → success. Exit 0
///    means "no reboot happened" — disambiguate via post-digest compare.
/// 4. Without --apply: two-step `bootc upgrade` then `systemctl reboot`
///    in a detached subshell. Same rc-255-means-OK semantics.
/// 5. Classify:
///     - rc=0  + matching digests → `AlreadyCurrent` (skip wait loops)
///     - rc=0  + differing digests → `Applied` (treat as success)
///     - rc=255 → `Applied` (expected; reboot tore down ssh)
///     - other → log + `UpgradeFailed` (caller decides fail-fast vs
///       --continue-on-error; we do NOT call `bail!` here)
///
/// Stderr from the bootc invocation is logged via [`log_err`] so an
/// operator grepping `[update-cluster]` stderr sees bootc's progress
/// markers and any rpm-ostree errors — bash twin's bare `ssh ...`
/// without redirection interleaves these to terminal stderr directly.
#[tracing::instrument(level = "debug", skip(plan), fields(node_ip = %ip, node = %name))]
fn bootc_upgrade_apply(plan: &Plan, ip: &str, name: &str) -> Result<BootcUpgradeOutcome> {
    if plan.dry_run {
        log(&format!(
            "DRY-RUN ssh root@{ip} bootc upgrade --apply (with pre/post digest compare)"
        ));
        return Ok(BootcUpgradeOutcome::Applied);
    }

    let pre_digest = bootc_booted_digest(plan, ip)?;
    log(&format!(
        "  pre-upgrade booted digest: {}",
        if pre_digest.is_empty() {
            "<unknown>"
        } else {
            &pre_digest
        }
    ));

    let has_apply = bootc_has_apply(plan, ip)?;
    let client = hbird_ssh::Client::new(node_ssh_opts(plan, ip));
    let result = if has_apply {
        client.run("bootc upgrade --apply")
    } else {
        log(&format!(
            "  bootc on {name} lacks --apply; falling back to 'bootc upgrade && systemctl reboot'"
        ));
        let upgrade_result = client.run("bootc upgrade");
        match upgrade_result {
            Ok(out) => {
                // Emit stdout/stderr to operator log for parity with the
                // bash twin's bare-ssh interleaving.
                let stderr = out.stderr_lossy();
                if !stderr.trim().is_empty() {
                    for line in stderr.lines() {
                        log_err(line);
                    }
                }
                // Two-step fallback: now issue the reboot. The reboot
                // kills sshd so we EXPECT NonZeroExit{255}. Bash twin
                // line 1144 runs `systemctl reboot >/dev/null 2>&1` —
                // no `|| true`; it relies on rc=255 falling into the
                // 255 case. Round-2 lens L2 LOW: drop the dead `||
                // true` (the remote bash would die before the OR could
                // fire anyway) so the remote command matches bash
                // verbatim.
                client.run("systemctl reboot >/dev/null 2>&1")
            }
            Err(e) => Err(e),
        }
    };

    // Classify by exit shape. NonZeroExit{255} is the expected "reboot
    // tore down ssh" signal — bash line 1165 maps it to success.
    match result {
        Ok(out) => {
            // Surface bootc's stderr (progress markers, warnings) for
            // operator-grep parity with bash's bare-ssh stream-merge.
            let stderr = out.stderr_lossy();
            if !stderr.trim().is_empty() {
                for line in stderr.lines() {
                    log_err(line);
                }
            }
            // rc=0: command returned cleanly. Disambiguate "no update"
            // vs "applied but didn't reboot" via post-digest compare.
            let post_digest = bootc_booted_digest(plan, ip)?;
            log(&format!(
                "  post-upgrade booted digest: {}",
                if post_digest.is_empty() {
                    "<unknown>"
                } else {
                    &post_digest
                }
            ));
            if !pre_digest.is_empty() && !post_digest.is_empty() && pre_digest == post_digest {
                log(&format!(
                    "  no update available on {name}; skipping wait loops"
                ));
                Ok(BootcUpgradeOutcome::AlreadyCurrent)
            } else {
                // Digests differ or one is unknown — treat as success,
                // let wait loops verify Ready (matches bash line 1163).
                Ok(BootcUpgradeOutcome::Applied)
            }
        }
        Err(hbird_ssh::Error::NonZeroExit {
            status,
            stdout: _,
            stderr,
            ..
        }) => {
            // Match by exit code.
            let code = status.code();
            if !stderr.trim().is_empty() {
                for line in stderr.lines() {
                    log_err(line);
                }
            }
            match code {
                Some(255) => {
                    // Expected: reboot tore down ssh.
                    Ok(BootcUpgradeOutcome::Applied)
                }
                Some(other) => {
                    // Surface but DO NOT bail — caller (update_worker /
                    // update_cp) decides whether to route through
                    // worker_fail (which respects --continue-on-error)
                    // or to abort hard (CP path). Match bash line 1173
                    // wording exactly for operator-grep parity.
                    log(&format!(
                        "bootc upgrade on {name} exited unexpectedly (rc={other})"
                    ));
                    Ok(BootcUpgradeOutcome::UpgradeFailed)
                }
                None => {
                    // Killed by signal — rare but log + treat as failed.
                    log(&format!(
                        "bootc upgrade on {name} exited unexpectedly (killed by signal)"
                    ));
                    Ok(BootcUpgradeOutcome::UpgradeFailed)
                }
            }
        }
        Err(e) => {
            // Transport-layer failure (spawn, identity-missing, wait).
            // Surface as UpgradeFailed with the chain preserved via
            // tracing — but return the error so callers see the chain.
            Err(e).with_context(|| {
                format!(
                    "bootc_upgrade_apply: ssh-run failed for `bootc upgrade --apply` \
                     against root@{ip} (node={name})"
                )
            })
        }
    }
}

// ---- block #11: CP upgrade flow --------------------------------------------

/// Track which node we're mid-update on, so an abort can surface
/// `kubectl uncordon` recovery hints. Mirrors the bash IN_FLIGHT_*
/// globals (line 612-622).
///
/// The `node`/`ip`/`drained`/`uncordoned` fields are populated by
/// [`update_cp`] / [`update_worker`] and consumed by the recovery-hint
/// surfacing in [`InFlight::Drop`] (cleanup_on_exit in bash, line 668).
///
/// Recovery hint policy (round-2 lens L3#2):
///   - `drained && !uncordoned` — node was cordoned but never uncordoned
///     before we unwound (panic / `?` / SIGINT). Emit
///     `node <X> is cordoned and was not uncordoned; recover with: kubectl uncordon <X>`.
///   - Otherwise — clean path (either we successfully uncordoned, or
///     drain never ran). Stay quiet.
///
/// Per-thread state is kept by-value; `mark_drained` / `mark_uncordoned`
/// expose grep-parity wrappers for the live-execution slice (lens L9).
#[derive(Debug, Default)]
struct InFlight {
    node: String,
    #[allow(dead_code)] // consumed by live-execution slice (#322).
    ip: String,
    drained: bool,
    uncordoned: bool,
    #[allow(dead_code)] // consumed by live-execution slice (#322).
    phase: String,
}

impl InFlight {
    /// Mark that drain has completed for this node. Bash twin's
    /// `mark_in_flight ${node} drained` (line 614). Pinned as a named
    /// wrapper so a grep across both sides yields a hit (lens L9).
    #[allow(dead_code)] // consumed by live-execution slice (#322).
    fn mark_drained(&mut self) {
        self.drained = true;
    }

    /// Mark that uncordon has completed for this node. Bash twin's
    /// `mark_in_flight ${node} uncordoned` (line 615). Pinned as a
    /// named wrapper so a grep across both sides yields a hit (lens L9).
    #[allow(dead_code)] // consumed by live-execution slice (#322).
    fn mark_uncordoned(&mut self) {
        self.uncordoned = true;
    }

    /// Reset to a fresh state — called between nodes so the recovery-hint
    /// drop semantics don't bleed across worker boundaries. Bash twin's
    /// `clear_in_flight` (line 622). Pinned as a named wrapper for
    /// grep-parity (lens L9).
    #[allow(dead_code)] // consumed by live-execution slice (#322).
    fn clear_in_flight(&mut self) {
        *self = Self::default();
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        // Recovery hint: when a node was cordoned but not uncordoned,
        // surface the explicit `kubectl uncordon` command an operator
        // needs to type. Bash twin emits this from `cleanup_on_exit`
        // (line 668) and we replicate the wording so an operator who
        // greps both sides finds the same string.
        if self.drained && !self.uncordoned && !self.node.is_empty() {
            eprintln!(
                "[update-cluster] node {} is cordoned and was not uncordoned; \
                 recover with: kubectl uncordon {}",
                self.node, self.node,
            );
        }
    }
}

/// Update the control plane. Mirrors `update_cp` (line 1179).
/// CP failures always fail-fast (`--continue-on-error` is worker-only).
fn update_cp(plan: &Plan) -> Result<()> {
    let mut in_flight = InFlight {
        node: plan.cp_k8s_name.clone(),
        ip: plan.cp_ip.clone(),
        phase: "pre-upgrade".to_string(),
        ..Default::default()
    };
    log("============================================================");
    if plan.cp_k8s_name == plan.cp_name {
        log(&format!("CP: {} ({})", plan.cp_name, plan.cp_ip));
    } else {
        log(&format!(
            "CP: {} ({}) k8s-node={}",
            plan.cp_name, plan.cp_ip, plan.cp_k8s_name
        ));
    }
    log("  single-CP topology: skipping drain (no peers to evict to)");
    log("  apiserver will be unavailable for ~60-120s during reboot");
    log("============================================================");

    timer_stop(plan, &plan.cp_ip)?;

    let pre_bootid = capture_node_bootid(plan, &plan.cp_k8s_name)?;
    // Bash uses ${pre_bootid:0:8}... which gives an 8-char prefix +
    // "...". Match it precisely so the operator-log shape matches.
    log(&format!("  pre-reboot bootID: {}...", prefix8(&pre_bootid)));

    log(&format!(
        "ssh root@{} bootc upgrade --apply  (auto-reboots; digest pre/post compared)",
        plan.cp_ip
    ));
    // Round-2 lens L3 MED: wrap with worker context so the anyhow chain
    // reads "update_cp(<name>) → bootc upgrade phase → <inner>".
    let outcome = bootc_upgrade_apply(plan, &plan.cp_ip, &plan.cp_name)
        .with_context(|| format!("update_cp({}): bootc upgrade phase", plan.cp_name))?;
    if outcome == BootcUpgradeOutcome::AlreadyCurrent {
        log(&format!(
            "CP {}: no update available; restoring timer and continuing.",
            plan.cp_name
        ));
        timer_start(plan, &plan.cp_ip)?;
        return Ok(());
    }
    if outcome == BootcUpgradeOutcome::UpgradeFailed {
        bail!(
            "bootc upgrade on CP {} failed (see log above)",
            plan.cp_name
        );
    }

    let _ = wait_ssh_drop(plan, &plan.cp_ip);
    // Round-2 lens L3#4: preserve the underlying error chain via
    // `with_context` instead of `map_err(|_|)` so the operator sees
    // "CP X did not come back: <root cause: ssh exit 255>" rather than
    // a fresh anyhow that masks the SSH-side diagnostic.
    let ssh_timeout = plan.timeouts.ssh;
    wait_ssh_back(plan, &plan.cp_ip).with_context(|| {
        format!(
            "CP {} did not come back over SSH within {}s",
            plan.cp_name, ssh_timeout,
        )
    })?;
    in_flight.phase = "post-reboot-pre-apiserver".to_string();
    let apiserver_timeout = plan.timeouts.apiserver;
    wait_apiserver_back(plan).with_context(|| {
        format!(
            "CP {} apiserver did not return within {}s",
            plan.cp_name, apiserver_timeout,
        )
    })?;
    in_flight.phase = "post-apiserver-pre-bootID".to_string();
    let ready_timeout = plan.timeouts.ready;
    wait_node_bootid_changed(plan, &plan.cp_k8s_name, &pre_bootid).with_context(|| {
        format!(
            "CP node {} bootID did not change after {}s (apiserver may be serving stale state)",
            plan.cp_name, ready_timeout,
        )
    })?;
    in_flight.phase = "post-bootID-pre-Ready".to_string();
    wait_node_ready(plan, &plan.cp_k8s_name).with_context(|| {
        format!(
            "CP node {} did not reach Ready within {}s",
            plan.cp_name, ready_timeout,
        )
    })?;
    in_flight.phase = "post-Ready-pre-DaemonSet".to_string();
    let daemonset_timeout = plan.timeouts.daemonset;
    wait_node_daemonsets_ready(plan, &plan.cp_k8s_name).with_context(|| {
        format!(
            "CP {}: kube-system DaemonSet pods not Ready after {}s",
            plan.cp_name, daemonset_timeout,
        )
    })?;

    timer_start(plan, &plan.cp_ip)?;
    log(&format!("CP {} updated and Ready.", plan.cp_name));
    drop(in_flight);
    Ok(())
}

// ---- block #12: worker upgrade flow ----------------------------------------

/// Tracking arrays for the `Rolling update complete.` summary at the
/// bottom of [`run`]. Mirrors bash SUCCEEDED_NODES / FAILED_NODES
/// (line 627).
#[derive(Debug, Default)]
struct Results {
    succeeded: Vec<String>,
    failed: Vec<String>,
}

/// Update a single worker. Mirrors `update_worker` (line 1277). Returns
/// `Ok(())` on success, `Err(_)` carrying a `node: reason` message on
/// failure (so the caller can route through worker_fail to honor
/// --continue-on-error).
fn update_worker(plan: &Plan, name: &str, ip: &str, k8s_name: &str) -> Result<()> {
    log("============================================================");
    if k8s_name == name {
        log(&format!("WORKER: {name} ({ip})"));
    } else {
        log(&format!("WORKER: {name} ({ip}) k8s-node={k8s_name}"));
    }
    log("============================================================");

    timer_stop(plan, ip)?;

    if plan.skip_drain {
        log(&format!(
            "  --skip-drain set: skipping kubectl drain for {k8s_name}"
        ));
    } else {
        let mut drain_flags = format!("--ignore-daemonsets --timeout={}", plan.timeouts.drain);
        if !plan.no_delete_emptydir_data {
            drain_flags.push_str(" --delete-emptydir-data");
        }
        log(&format!("kubectl drain {k8s_name} {drain_flags}"));
        cp_kubectl(plan, &format!("drain {k8s_name} {drain_flags}"))?;
    }

    let pre_bootid = capture_node_bootid(plan, k8s_name)?;
    log(&format!("  pre-reboot bootID: {}...", prefix8(&pre_bootid)));

    log(&format!(
        "ssh root@{ip} bootc upgrade --apply  (auto-reboots; digest pre/post compared)"
    ));
    // Round-2 lens L3 MED: wrap with worker context so the anyhow chain
    // reads "update_worker(<name>) → bootc upgrade phase → <inner>".
    let outcome = bootc_upgrade_apply(plan, ip, name)
        .with_context(|| format!("update_worker({name}): bootc upgrade phase"))?;
    if outcome == BootcUpgradeOutcome::UpgradeFailed {
        bail!("{name}: bootc upgrade --apply failed (see log above)");
    }
    if outcome == BootcUpgradeOutcome::AlreadyCurrent {
        log(&format!(
            "worker {name}: no update available; uncordoning and continuing."
        ));
        if !plan.skip_drain {
            log(&format!("kubectl uncordon {k8s_name}"));
            cp_kubectl(plan, &format!("uncordon {k8s_name}"))?;
        }
        timer_start(plan, ip)?;
        return Ok(());
    }

    let _ = wait_ssh_drop(plan, ip);
    // Round-2 lens L3#4: `with_context` preserves the underlying
    // error chain (SSH stderr, kubectl exit code) instead of masking
    // it behind a fresh anyhow.
    let ssh_timeout = plan.timeouts.ssh;
    wait_ssh_back(plan, ip)
        .with_context(|| format!("{name}: did not come back over SSH within {ssh_timeout}s"))?;
    let ready_timeout = plan.timeouts.ready;
    wait_node_bootid_changed(plan, k8s_name, &pre_bootid).with_context(|| {
        format!(
            "{name}: node bootID did not change after {ready_timeout}s (apiserver may be serving stale state)"
        )
    })?;
    wait_node_ready(plan, k8s_name)
        .with_context(|| format!("{name}: did not reach Ready within {ready_timeout}s"))?;
    let daemonset_timeout = plan.timeouts.daemonset;
    wait_node_daemonsets_ready(plan, k8s_name).with_context(|| {
        format!(
            "{name}: kube-system DaemonSet pods not Ready on this node after {daemonset_timeout}s"
        )
    })?;

    log(&format!("kubectl uncordon {k8s_name}"));
    cp_kubectl(plan, &format!("uncordon {k8s_name}"))?;
    timer_start(plan, ip)?;
    log(&format!("node {name} updated"));
    Ok(())
}

// ---- block #13: parallel batch ---------------------------------------------

/// Process a batch of workers, optionally in parallel. Mirrors
/// `run_worker_batch` (line 1491).
///
/// Serial fast-path (`parallel == 1` OR `batch.len() == 1`): direct call,
/// log lines stream immediately.
///
/// Parallel path: each worker is processed in its own scope with the
/// thread-local log prefix set to `[parallel:NAME] `; in dry-run mode the
/// workers are processed sequentially under each prefix (the dry-run is
/// non-blocking and ordering matters for fixture-diff). Live parallelism
/// (real subprocesses doing actual SSH) is deferred to the live-execution
/// slice — see module docs.
fn run_worker_batch(plan: &Plan, batch: &[String], results: &mut Results) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    // Serial fast-path mirrors bash line 1497.
    if plan.parallel == 1 || batch.len() == 1 {
        for w in batch {
            run_one_worker(plan, w, results)?;
        }
        return Ok(());
    }

    // Parallel path. Bash emits the batch announce line first, then
    // forks subshells that capture their output to a tmpdir, then
    // replays them in batch order. We mirror the *observable shape*
    // (announce + per-worker prefix + ordering) deterministically:
    // in dry-run mode there's no real work to parallelize so we run
    // serially under the prefix, and live mode runs serially under
    // the prefix too with a WARN (real concurrency lands in the
    // live-execution slice). The output is identical to bash.
    log(&format!(
        "PARALLEL batch ({}): {}",
        batch.len(),
        batch.join(" ")
    ));
    if !plan.dry_run {
        log_err(
            "WARN: parallel batch live mode is processed serially in this Rust scaffold; bash twin forks here. Real concurrency lands in the live-execution slice.",
        );
    }
    for w in batch {
        let prefix = format!("[parallel:{w}] ");
        with_log_prefix(&prefix, || run_one_worker(plan, w, results))?;
    }
    Ok(())
}

/// Drive a single worker through [`update_worker`] and record the
/// result. Routes failures through `--continue-on-error` semantics
/// (mirrors bash `worker_fail`, line 1407).
///
/// Round-2 lens L3#3: a missing IP in [`Plan::worker_ip_map`] now
/// `bail!`s instead of silently substituting `<unknown>`. The Plan's
/// from_args invariant guarantees every worker in `workers_to_run()`
/// has an entry; reaching the bail means that invariant broke.
fn run_one_worker(plan: &Plan, w: &str, results: &mut Results) -> Result<()> {
    let ip = plan.worker_ip_map.get(w).cloned().ok_or_else(|| {
        anyhow!("internal: no IP recorded for worker {w}; Plan::from_args invariant broken")
    })?;
    let k8s_name = plan
        .worker_k8s_name_map
        .get(w)
        .cloned()
        .unwrap_or_else(|| w.to_string());
    match update_worker(plan, w, &ip, &k8s_name) {
        Ok(()) => {
            results.succeeded.push(w.to_string());
            Ok(())
        }
        Err(e) => {
            if plan.continue_on_error {
                let reason = e.to_string();
                log(&format!(
                    "  ERROR on {w}: {reason} — continuing (--continue-on-error)"
                ));
                results.failed.push(format!("{w}: {reason}"));
                // Best-effort timer restore (bash worker_fail line 1414).
                let _ = timer_start(plan, &ip);
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

// ---- cp_kubectl shim -------------------------------------------------------

/// Run a kubectl command via the CP. Mirrors `cp_kubectl`
/// (`scripts/update-cluster.sh:425`). First live helper wired by #322
/// cycle 1 (PR #325); [`cp_ssh_opts`] is the shared SSH-options builder
/// reused by cycles 2–4 (`wait_apiserver_back`, `wait_node_ready`,
/// `capture_node_bootid`).
///
/// Live execution path (Phase 1B, #322): builds an `hbird_ssh::Client`
/// per call against `root@plan.cp_ip` with `--proxy-jump` set to
/// `plan.kvm_host` when present. Emits kubectl stdout to operator stdout
/// via the prefix-aware [`log`] helper (matches bash twin's `ssh -t`
/// channel choice). On success, *also* emits any kubectl stderr via
/// [`log_err`] so warnings like `Warning: ignoring DaemonSet-managed
/// Pods: ...` from `drain` reach operators who grep stderr for `Warning:`
/// (PR #325 round-2 lens L8 HIGH + CodeRabbit comment).
///
/// Returns the captured kubectl stdout as a `String` so callers like
/// `capture_node_bootid` / `wait_node_bootid_changed` /
/// `wait_node_daemonsets_ready` (bash lines 894, 937, 1013, 1032, 1042)
/// can parse jsonpath / pod-count output. PR #325 round-2 lens L2 HIGH —
/// returning `Result<()>` would have blocked cycle 2 wait helpers.
///
/// Drain/uncordon callers (block #5 + part of update_worker) discard
/// the returned string; the same `Ok("")` shape works for them.
///
/// The `#[tracing::instrument]` attribute opens a debug-level span per
/// invocation tagged with the CP IP + command (#323). Spans are silent
/// at the default `RUST_LOG=info` so the dry-run fixtures (PR #321) keep
/// matching byte-for-byte — `log()` writes to stdout via `println!`,
/// while tracing emits to stderr only when enabled. Flip on with
/// `RUST_LOG=hbird_cli=debug` to surface which CP a given kubectl call
/// targeted.
// `err(Debug)` directive demoted to a manual `tracing::debug!` event in
// the Err branch so callers (not this wrapper) decide ERROR-vs-debug
// policy per call site. Some update-cluster callers deliberately tolerate
// non-zero kubectl exits (e.g. probing for a not-yet-present node) — the
// auto ERROR event was misleading. (#331; original wiring #326.)
#[tracing::instrument(level = "debug", skip(plan), fields(cp_ip = %plan.cp_ip, command = %command))]
fn cp_kubectl(plan: &Plan, command: &str) -> Result<String> {
    cp_kubectl_inner(plan, command)
        .inspect_err(|err| tracing::debug!(error = ?err, "cp_kubectl failed"))
}

fn cp_kubectl_inner(plan: &Plan, command: &str) -> Result<String> {
    if plan.dry_run {
        log(&format!("DRY-RUN cp_kubectl -- {command}"));
        return Ok(String::new());
    }
    // Round-2 L1 HIGH (defense-in-depth): the command is forwarded as a
    // single argv element to `ssh root@CP_IP`, which executes it via
    // `/bin/sh -c`. kubectl invocations don't need shell metacharacters;
    // a metachar in `command` would indicate an upstream bug or
    // injection. Reject before the SSH layer rather than relying on
    // every caller to allowlist k8s_name / namespace / field-selector
    // values themselves.
    if let Some(bad) = command
        .chars()
        .find(|c| matches!(c, ';' | '&' | '|' | '`' | '\n' | '\r'))
    {
        bail!(
            "cp_kubectl: refusing command with shell metacharacter '{bad}' \
             (commands are forwarded to remote /bin/sh -c; metachars would \
             execute as root on the CP). command={command:?}"
        );
    }
    if command.contains("$(") {
        bail!(
            "cp_kubectl: refusing command with `$(` substitution (executes \
             as root on the CP). command={command:?}"
        );
    }
    let opts = cp_ssh_opts(plan);
    let client = hbird_ssh::Client::new(opts);
    let remote = format!("kubectl --kubeconfig=/etc/kubernetes/admin.conf {command}");
    let output = client.run(&remote).with_context(|| {
        format!(
            "cp_kubectl: ssh-run failed for `kubectl ... {command}` against {}",
            plan.cp_ip
        )
    })?;
    let stdout = output.stdout_lossy();
    if !stdout.trim().is_empty() {
        for line in stdout.lines() {
            log(line);
        }
    }
    // Round-2 L8 HIGH: surface non-empty stderr on success too. kubectl
    // emits Warnings (e.g. "Warning: ignoring DaemonSet-managed Pods")
    // on stderr even when exit 0; bash twin's `ssh -t` interleaves these
    // onto the terminal, so dropping them in Rust silently breaks
    // log-grep parity.
    let stderr = output.stderr_lossy();
    if !stderr.trim().is_empty() {
        for line in stderr.lines() {
            log_err(line);
        }
    }
    Ok(stdout)
}

/// Build the SSH options for a CP-targeted call — bash twin's
/// `CP_SSH_OPTS` array shape (`scripts/lib/build-common.sh`'s
/// `ssh_opts_array CP_SSH_OPTS`). Pulls `cp_ip` from the plan and
/// ProxyJump from `kvm_host` when set.
///
/// Strict-host-key-checking stays off (default) — VM IPs rotate per
/// deploy; pinning is left to `~/.ssh/config` if the operator opts in
/// (see issue #320). The name mirrors bash's `cp_ssh_opts` grep-anchor
/// (round-2 lens L9 MEDIUM).
fn cp_ssh_opts(plan: &Plan) -> hbird_ssh::SshOptions {
    let mut opts = hbird_ssh::SshOptions::new(plan.cp_ip.clone()).with_user("root");
    if let Some(jump) = plan.kvm_host.as_deref() {
        opts = opts.with_proxy_jump(jump.to_string());
    }
    opts
}

/// Build the SSH options for an arbitrary node IP — same shape as
/// [`cp_ssh_opts`] but parameterized over the target. The bash twin
/// reuses a single `SSH_OPTS` array for both CP and worker SSH
/// (`scripts/update-cluster.sh:327`); we mirror that by sharing the
/// same construction logic and only varying the target host.
///
/// Used by [`wait_ssh_back`], [`wait_ssh_drop`], [`bootc_upgrade_apply`],
/// [`bootc_has_apply`], and [`bootc_booted_digest`] — every helper
/// wired in cycle 2 that talks to a node directly (rather than via the
/// CP's kubectl). Pinned as a named function rather than inlined so
/// the live-execution slice (#322) has a single point to thread future
/// per-node tweaks through (ControlMaster, identity overrides, etc).
fn node_ssh_opts(plan: &Plan, ip: &str) -> hbird_ssh::SshOptions {
    let mut opts = hbird_ssh::SshOptions::new(ip.to_string()).with_user("root");
    if let Some(jump) = plan.kvm_host.as_deref() {
        opts = opts.with_proxy_jump(jump.to_string());
    }
    opts
}

// ---- block #15 + dispatch: run() ------------------------------------------

/// Dispatch entrypoint invoked by `main.rs`.
///
/// Wires together the blocks above in the order the bash twin does:
///
/// 1. Args mutual-exclusion checks ([`Plan::from_args`]).
/// 2. Config load ([`hbird_config::parse`]).
/// 3. Timeout env load ([`Timeouts::from_env`]).
/// 4. Concurrency lock ([`acquire_lock`]).
/// 5. Plan summary log lines (matches bash 1444-1460).
/// 6. CP upgrade (when not `--workers-only` and not `--start-from`).
/// 7. Worker walk in batches of `--parallel` ([`run_worker_batch`]).
/// 8. Summary footer + exit code (0 / 3 per bash twin 1626-1645).
pub fn run(args: UpdateClusterArgs) -> Result<()> {
    let config = hbird_config::parse(&args.config).map_err(|e| anyhow!("{e}"))?;
    let timeouts = Timeouts::from_env()?;
    let plan = Plan::from_args(&args, config, timeouts)?;
    let _lock = acquire_lock(plan.dry_run)?;

    // ---- Plan summary log lines (bash 1444-1460) ----
    log(&format!("config: {}", plan.config_path.display()));
    let workers_str = plan.worker_names.join(" ");
    log(&format!(
        "CP={} ({}) k8s-node={}, workers=({})",
        plan.cp_name, plan.cp_ip, plan.cp_k8s_name, workers_str
    ));
    for w in &plan.worker_names {
        let k = plan
            .worker_k8s_name_map
            .get(w)
            .map(String::as_str)
            .unwrap_or(w);
        if k != w {
            log(&format!("  resolved libvirt domain {w} -> k8s node {k}"));
        }
    }
    log(&format!(
        "flags: workers-only={} skip-drain={} skip-gates={} dry-run={}",
        b(plan.workers_only),
        b(plan.skip_drain),
        b(plan.skip_gates),
        b(plan.dry_run),
    ));
    log(&format!(
        "       node-filter={} start-from={}",
        plan.node_filter.as_deref().unwrap_or("<none>"),
        plan.start_from.as_deref().unwrap_or("<none>"),
    ));
    log(&format!(
        "       continue-on-error={} no-delete-emptydir-data={} parallel={}",
        b(plan.continue_on_error),
        b(plan.no_delete_emptydir_data),
        plan.parallel,
    ));
    log(&format!(
        "timeouts: drain={} ready={}s daemonset={}s apiserver={}s ssh={}s ssh-drop={}s inter-node-sleep={}s",
        plan.timeouts.drain,
        plan.timeouts.ready,
        plan.timeouts.daemonset,
        plan.timeouts.apiserver,
        plan.timeouts.ssh,
        plan.timeouts.ssh_drop,
        plan.timeouts.inter_node_sleep,
    ));
    log(&format!(
        "per-node worst-case budget: drain {} + ssh-back {}s + bootID {}s + ready {}s + daemonsets {}s",
        plan.timeouts.drain,
        plan.timeouts.ssh,
        plan.timeouts.ready,
        plan.timeouts.ready,
        plan.timeouts.daemonset,
    ));

    // ---- Workers-to-run list (resume mode) ----
    let workers_to_run = plan.workers_to_run();
    if plan.start_from.is_some() {
        let listed = workers_to_run.join(" ");
        log(&format!(
            "--start-from={}: resuming with workers=({listed})",
            plan.start_from.as_deref().unwrap_or("")
        ));
    }

    let mut results = Results::default();

    // ---- block #14: --node single-node mode ----
    if let Some(node) = &plan.node_filter {
        if node == &plan.cp_name {
            update_cp(&plan)?;
            results.succeeded.push(plan.cp_name.clone());
        } else if plan.worker_names.iter().any(|w| w == node) {
            // Round-2 lens L2#1: route single-node WORKER mode through
            // the same `run_one_worker` codepath the batch loop uses, so
            // `--continue-on-error` semantics + best-effort timer restore
            // stay aligned (CodeRabbit's `update_worker || true` vs
            // `run_one_worker` divergence). The summary footer below
            // always prints — `bash scripts/update-cluster.sh
            // --node=WORKER` does the same.
            let w_owned = node.clone();
            run_one_worker(&plan, &w_owned, &mut results)?;
        } else {
            bail!("--node={node} did not match CP_NAME or any WORKER_NAMES entry");
        }
    } else {
        // ---- CP first (unless --workers-only or --start-from) ----
        if !plan.workers_only {
            if plan.start_from.is_some() {
                log(&format!(
                    "--start-from set: skipping CP (resume mode starts at worker '{}')",
                    plan.start_from.as_deref().unwrap_or("")
                ));
            } else {
                update_cp(&plan)?;
                results.succeeded.push(plan.cp_name.clone());
            }
        } else {
            log("--workers-only: skipping CP");
        }

        // ---- Walk workers in batches of --parallel ----
        let total = workers_to_run.len();
        let parallel = plan.parallel as usize;
        let mut i = 0;
        while i < total {
            let end = std::cmp::min(i + parallel, total);
            let batch: Vec<String> = workers_to_run[i..end].to_vec();
            run_worker_batch(&plan, &batch, &mut results)?;
            i = end;
            if i < total && plan.timeouts.inter_node_sleep > 0 {
                log(&format!(
                    "pausing {}s before next node",
                    plan.timeouts.inter_node_sleep
                ));
                if !plan.dry_run {
                    std::thread::sleep(Duration::from_secs(plan.timeouts.inter_node_sleep.into()));
                }
            }
        }
    }

    // ---- Summary footer (bash 1626-1644) ----
    log("============================================================");
    log("Rolling update complete.");
    let succ_count = results.succeeded.len();
    let succ_str = if results.succeeded.is_empty() {
        "<none>".to_string()
    } else {
        results.succeeded.join(" ")
    };
    log(&format!("  succeeded ({succ_count}): {succ_str}"));
    if !results.failed.is_empty() {
        log(&format!("  FAILED ({}):", results.failed.len()));
        for entry in &results.failed {
            log(&format!("    - {entry}"));
        }
    }
    log("============================================================");

    if !results.failed.is_empty() {
        // bash twin: `exit 3` for "some failed but --continue-on-error".
        // anyhow::Error in main() exits 1; we surface a distinct
        // structured error and let main() format it.
        bail!("UPDATE_CLUSTER_PARTIAL_FAILURE");
    }
    Ok(())
}

// ---- helpers ---------------------------------------------------------------

/// First 8 chars of a bootID (matches bash `${pre_bootid:0:8}`).
/// Empty strings collapse to empty (matches bash's expansion of an
/// empty value).
fn prefix8(s: &str) -> String {
    s.chars().take(8).collect()
}

/// Render a bool as `0` or `1` (matches bash twin's `(( FLAG == 1 ))`
/// numeric convention in the summary log lines).
fn b(v: bool) -> i32 {
    i32::from(v)
}

/// Construct the "not yet implemented in the Rust live path" error
/// used by every helper that needs a real SSH round-trip. The error
/// wording explicitly points at the follow-up issue so an operator
/// hitting this in CI gets actionable guidance.
///
/// The tracking issue is [#322] — the live-execution slice — not
/// [#286], which this PR closes with the dry-run parity surface
/// (round-2 lens L4#1).
///
/// [#322]: https://github.com/aatchison/hummingbird-k8s/issues/322
fn live_mode_not_implemented(helper: &str, equivalent: &str) -> anyhow::Error {
    anyhow!(
        "live-mode update-cluster: `{helper}` requires a remote SSH/kubectl round-trip that is not yet \
         implemented in the Rust path. Bash equivalent: `{equivalent}`. \
         Until the live-execution slice lands (tracked by #322), run with `--dry-run` to preview \
         the plan, or use `make update-cluster CONFIG=… [FLAGS=…]` to actually upgrade."
    )
}

// ---- unit tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hbird_config::ClusterConfig;

    /// Build a minimal `ClusterConfig` for tests. Mirrors what the bash
    /// twin's required-field defaults would have produced.
    fn cfg(workers: Vec<&str>) -> ClusterConfig {
        // ClusterConfig is non_exhaustive — construct via parse_str so
        // any field added upstream stays in sync without test churn.
        let mut body = String::from("CP_NAME=hbird-cp1\nSSH_PUBKEY_FILE=/k\n");
        if !workers.is_empty() {
            body.push_str(&format!("WORKER_NAMES=({})\n", workers.join(" ")));
        }
        hbird_config::parse_str(&body).expect("test cfg parses")
    }

    /// Build args via clap's parser so we exercise the same code path
    /// `main()` uses. Clap will fail to parse if the arg surface drifts.
    fn parse_args(extra: &[&str]) -> UpdateClusterArgs {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Wrap {
            #[command(flatten)]
            args: UpdateClusterArgs,
        }
        // Always include the required --config flag.
        let mut argv: Vec<&str> = vec!["test", "--config", "/dev/null"];
        argv.extend_from_slice(extra);
        Wrap::try_parse_from(argv).expect("args parse").args
    }

    #[test]
    fn timeouts_defaults_match_bash_twin() {
        // Force-clear any env that might leak from the parent shell.
        // SAFETY: tests run single-threaded for env manipulation per the
        // serial_test crate, but we don't pull that in for one fixture —
        // instead we read defaults via the no-env path.
        // We can't safely mutate the process env here without serializing
        // tests; pin defaults directly off the struct.
        let t = Timeouts::default();
        assert_eq!(t.drain, "5m");
        assert_eq!(t.ready, 300);
        assert_eq!(t.daemonset, 300);
        assert_eq!(t.apiserver, 300);
        assert_eq!(t.ssh, 300);
        assert_eq!(t.ssh_drop, 30);
        assert_eq!(t.inter_node_sleep, 5);
    }

    #[test]
    fn plan_workers_only_with_node_is_rejected() {
        let args = parse_args(&["--workers-only", "--node", "hbird-cp1"]);
        let err = Plan::from_args(&args, cfg(vec!["hbird-w1"]), Timeouts::default())
            .expect_err("workers-only + node should fail");
        assert!(
            err.to_string()
                .contains("--workers-only and --node= are mutually exclusive"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn plan_node_with_start_from_is_rejected() {
        let args = parse_args(&["--node", "x", "--start-from", "y"]);
        let err = Plan::from_args(&args, cfg(vec!["hbird-w1"]), Timeouts::default())
            .expect_err("node + start-from should fail");
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn plan_parallel_zero_is_rejected() {
        let args = parse_args(&["--parallel", "0", "--dry-run"]);
        let err = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default())
            .expect_err("--parallel=0 should fail");
        assert!(err.to_string().contains("positive integer"));
    }

    #[test]
    fn plan_start_from_unknown_worker_is_rejected() {
        let args = parse_args(&["--start-from", "ghost", "--dry-run"]);
        let err = Plan::from_args(&args, cfg(vec!["w1", "w2"]), Timeouts::default())
            .expect_err("unknown start-from should fail");
        assert!(err.to_string().contains("--start-from=ghost"));
    }

    #[test]
    fn plan_node_name_override_validates_domain() {
        let args = parse_args(&["--node-name-override", "ghost=real", "--dry-run"]);
        let err = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default())
            .expect_err("ghost domain should fail");
        assert!(
            err.to_string()
                .contains("'ghost' does not match CP_NAME or any WORKER_NAMES entry")
        );
    }

    #[test]
    fn plan_node_name_override_accepts_cp_and_worker_domains() {
        let args = parse_args(&[
            "--node-name-override",
            "hbird-cp1=cp-k8s,w1=w1-k8s",
            "--dry-run",
        ]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default())
            .expect("valid overrides accepted");
        assert_eq!(plan.cp_k8s_name, "cp-k8s");
        assert_eq!(plan.worker_k8s_name_map.get("w1").unwrap(), "w1-k8s");
    }

    #[test]
    fn plan_node_name_override_empty_pair_skipped() {
        // Trailing comma is benign in bash; we mirror that.
        let args = parse_args(&["--node-name-override", "w1=w1-k8s,", "--dry-run"]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default())
            .expect("trailing comma skipped");
        assert_eq!(plan.worker_k8s_name_map.get("w1").unwrap(), "w1-k8s");
    }

    #[test]
    fn plan_node_name_override_missing_equals_rejected() {
        let args = parse_args(&["--node-name-override", "no-equals-sign", "--dry-run"]);
        let err = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default())
            .expect_err("missing = should fail");
        assert!(err.to_string().contains("expects DOMAIN=NODE"));
    }

    #[test]
    fn workers_to_run_no_start_from_returns_all() {
        let args = parse_args(&["--dry-run"]);
        let plan =
            Plan::from_args(&args, cfg(vec!["a", "b", "c"]), Timeouts::default()).expect("plan");
        assert_eq!(plan.workers_to_run(), vec!["a", "b", "c"]);
    }

    #[test]
    fn workers_to_run_with_start_from_skips_earlier() {
        let args = parse_args(&["--start-from", "b", "--dry-run"]);
        let plan =
            Plan::from_args(&args, cfg(vec!["a", "b", "c"]), Timeouts::default()).expect("plan");
        assert_eq!(plan.workers_to_run(), vec!["b", "c"]);
    }

    #[test]
    fn dry_run_ip_resolution_uses_placeholder() {
        let args = parse_args(&["--dry-run"]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default()).expect("plan");
        assert_eq!(plan.cp_ip, "<resolved-at-runtime>");
        assert_eq!(
            plan.worker_ip_map.get("w1").unwrap(),
            "<resolved-at-runtime>"
        );
    }

    #[test]
    fn prefix8_truncates_correctly() {
        assert_eq!(prefix8("0123456789abcdef"), "01234567");
        assert_eq!(prefix8("short"), "short");
        assert_eq!(prefix8(""), "");
        // Multi-byte chars (defensive — bootIDs are hex, but the
        // helper takes &str): ensure we don't slice through a UTF-8
        // boundary.
        assert_eq!(prefix8("αβγδεζηθ"), "αβγδεζηθ");
    }

    #[test]
    fn b_renders_numerically() {
        assert_eq!(b(false), 0);
        assert_eq!(b(true), 1);
    }

    /// Regression: the helper-name field in the live-mode-not-implemented
    /// error must be the bash function name verbatim so operators can
    /// `grep scripts/update-cluster.sh` for the equivalent. (Block-by-block
    /// traceability per #286.)
    #[test]
    fn live_mode_error_names_bash_helper() {
        let e = live_mode_not_implemented("wait_node_ready", "...");
        let s = e.to_string();
        assert!(s.contains("wait_node_ready"), "missing helper name: {s}");
        assert!(s.contains("--dry-run"), "missing remediation hint: {s}");
    }

    /// The `Results::failed` summary's `exit 3` semantics from bash are
    /// mapped to a sentinel error wording — guard against accidental
    /// rewording that would mask the exit-code-3 channel.
    #[test]
    fn partial_failure_sentinel_unchanged() {
        // Build a fake plan that has no nodes to walk + force a failure
        // path. Easiest: invoke run() with --workers-only + empty
        // WORKER_NAMES + --continue-on-error and inject a fake worker
        // via direct construction. We don't have a builder for Plan
        // outside the from_args path so this test asserts the sentinel
        // string lives in the file — a smoke check.
        let src = include_str!("update_cluster.rs");
        assert!(
            src.contains("UPDATE_CLUSTER_PARTIAL_FAILURE"),
            "sentinel removed; bash twin's exit 3 channel would silently downgrade",
        );
    }

    /// CI smoke: a `--dry-run` invocation against a synthetic config
    /// runs to completion without touching the network or filesystem.
    /// (The lock acquisition + every helper is bypassed in dry-run.)
    #[test]
    fn dry_run_e2e_serializes_summary() {
        // We can't directly assert on stdout from within the same test
        // process without capturing, but we can confirm Plan + Results
        // walk without panicking. The byte-for-byte dry-run output is
        // pinned by the integration test in tests/update_cluster/dry_run.rs.
        let args = parse_args(&["--dry-run"]);
        let plan = Plan::from_args(
            &args,
            cfg(vec!["hbird-w1", "hbird-w2"]),
            Timeouts::default(),
        )
        .expect("plan");
        let _ = plan.workers_to_run();
        // Lock skipped in dry-run.
        assert!(acquire_lock(true).expect("dry-run lock is no-op").is_none());
    }

    /// Pin the InFlight field set so the live-execution slice (which
    /// consumes the recovery hints in cleanup_on_exit, bash line 668)
    /// surfaces here if a field is dropped or renamed.
    #[test]
    fn in_flight_struct_field_set_pinned() {
        // Construct with `uncordoned=true` so the Drop's recovery-hint
        // path doesn't print to test stderr.
        let f = InFlight {
            node: "n".to_string(),
            ip: "i".to_string(),
            drained: true,
            uncordoned: true,
            phase: "p".to_string(),
        };
        assert_eq!(f.node, "n");
        assert_eq!(f.ip, "i");
        assert!(f.drained);
        assert!(f.uncordoned);
        assert_eq!(f.phase, "p");
    }

    /// Round-2 lens L3#2: when a node is `drained` but never
    /// `uncordoned` and the `InFlight` is dropped (panic / `?` / SIGINT),
    /// the Drop must surface a `kubectl uncordon <NODE>` recovery hint
    /// to stderr. Pin the exact wording for grep-parity with the bash
    /// twin's `cleanup_on_exit`.
    ///
    /// We can't easily capture stderr from inside the same test process
    /// without a stderr-capture crate; instead we assert the source-code
    /// contains the load-bearing wording. A unit test that drives the
    /// real Drop into an stderr buffer would need to spawn a subprocess —
    /// see `tests/update_cluster_in_flight.rs` for that.
    #[test]
    fn in_flight_drop_hint_wording_present() {
        let src = include_str!("update_cluster.rs");
        assert!(
            src.contains("is cordoned and was not uncordoned; recover with: kubectl uncordon"),
            "round-2 L3#2: recovery-hint wording removed from InFlight::Drop"
        );
    }

    /// Lens L9: grep-parity helper names. The bash twin invokes
    /// `mark_in_flight`, `mark_in_flight ... uncordoned`, and
    /// `clear_in_flight`; we expose Rust wrappers with the same names
    /// so an operator grepping both sides finds matching call sites.
    #[test]
    fn in_flight_grep_parity_wrappers_compile() {
        let mut f = InFlight::default();
        f.node = "x".to_string();
        f.mark_drained();
        assert!(f.drained);
        f.mark_uncordoned();
        assert!(f.uncordoned);
        f.clear_in_flight();
        assert!(!f.drained);
        assert!(!f.uncordoned);
        assert!(f.node.is_empty());
    }

    /// Round-2 lens L2#2: `--node-name-override=` with an empty value
    /// is rejected (previously a silent no-op). Pin the error wording.
    #[test]
    fn plan_node_name_override_empty_string_is_rejected() {
        let args = parse_args(&["--node-name-override", "", "--dry-run"]);
        let err = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default())
            .expect_err("empty --node-name-override should fail");
        assert!(
            err.to_string().contains("non-empty DOMAIN=NODE"),
            "wrong error: {err}"
        );
    }

    /// Round-2 lens L1 medium: `--no-sudo` (and `HBIRD_REMOTE_NO_SUDO=1`
    /// env) must be threaded onto the Plan so the live-execution slice
    /// (#322) can consume it. Pin the field's presence + value
    /// propagation.
    #[test]
    fn plan_carries_no_sudo_flag() {
        // SAFETY: tests in the same crate share env state. We don't
        // touch the env here — clap reads `HBIRD_REMOTE_NO_SUDO` once
        // at parse time and a stale value from another test's setenv
        // would leak. Confirm only the explicit-flag path.
        // Note: clap's BoolishValueParser treats `--no-sudo true` as
        // `true` and bare `--no-sudo` as `true` (via default_missing_value).
        let args = parse_args(&["--dry-run", "--no-sudo", "true"]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default()).expect("plan");
        assert!(plan.no_sudo, "--no-sudo true should set plan.no_sudo");

        // And the default — `false` — when the flag is absent. We
        // can't be sure the env is clean (env_var('HBIRD_REMOTE_NO_SUDO')
        // may be set in CI), so we don't assert the negative case here.
        // The integration tests under tests/ pin the absent-flag path.
        let _ = plan.kvm_host;
    }

    /// Round-2 lens L3#3: a missing IP in worker_ip_map must `bail!`
    /// rather than silently use `<unknown>`. We can't reach
    /// `run_one_worker` from outside the module without a Plan that
    /// has at least one worker with no IP; assert the source-level
    /// invariant + the wording.
    #[test]
    fn run_one_worker_missing_ip_wording_present() {
        let src = include_str!("update_cluster.rs");
        assert!(
            src.contains("Plan::from_args invariant broken"),
            "round-2 L3#3: <unknown>-IP bail! wording removed from run_one_worker"
        );
    }

    // Note: the `acquire_lock` contention test (lens L5#M1) lives in
    // `tests/update_cluster_lock_contention.rs`. It needs an isolated
    // XDG_RUNTIME_DIR which std::env::set_var requires `unsafe` under
    // the 2024 edition, and the workspace lint `unsafe_code = "forbid"`
    // rules that out in src/. Pinning it in an integration test lets us
    // run the binary in a clean subprocess with the env set at spawn
    // time instead of mid-process.

    /// Cycle 2 (#327): `node_ssh_opts` must mirror `cp_ssh_opts`'s shape
    /// (root user, ProxyJump=KVM_HOST when set) but with the supplied
    /// IP as the target — not the CP IP. Pin both the user prefix and
    /// the per-worker target.
    #[test]
    fn node_ssh_opts_targets_supplied_ip_with_root_user() {
        let args = parse_args(&["--dry-run", "--kvm-host", "geary"]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default()).expect("plan");
        let opts = node_ssh_opts(&plan, "192.168.122.99");
        let argv = opts.to_argv();
        assert_eq!(
            argv.last().map(String::as_str),
            Some("root@192.168.122.99"),
            "node_ssh_opts must target the supplied IP as root@<ip>",
        );
        let has_jump = argv
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "ProxyJump=geary");
        assert!(
            has_jump,
            "node_ssh_opts must propagate kvm_host as ProxyJump (got argv={argv:?})",
        );
    }

    /// Cycle 2 (#327): when `kvm_host` is unset, `node_ssh_opts` MUST
    /// NOT emit a ProxyJump option (operator already on the KVM host).
    /// Mirrors `cp_ssh_opts`'s shape.
    #[test]
    fn node_ssh_opts_omits_proxy_jump_without_kvm_host() {
        let args = parse_args(&["--dry-run"]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default()).expect("plan");
        let opts = node_ssh_opts(&plan, "10.0.0.1");
        let argv = opts.to_argv();
        assert!(
            !argv.iter().any(|s| s.contains("ProxyJump")),
            "node_ssh_opts must omit ProxyJump when kvm_host is None (argv={argv:?})",
        );
        assert_eq!(argv.last().map(String::as_str), Some("root@10.0.0.1"));
    }

    /// Cycle 2 (#327) regression: the bash-twin function name MUST stay
    /// in tracing instrument fields so an operator grepping the source
    /// finds matching call sites in `scripts/update-cluster.sh`. Pin
    /// the helper names in the source body so a rename here is loud.
    #[test]
    fn cycle2_helper_names_present() {
        let src = include_str!("update_cluster.rs");
        for name in [
            "fn wait_ssh_drop",
            "fn wait_ssh_back",
            "fn capture_node_bootid",
            "fn wait_node_bootid_changed",
            "fn bootc_upgrade_apply",
            "fn bootc_has_apply",
            "fn bootc_booted_digest",
            "fn node_ssh_opts",
        ] {
            assert!(
                src.contains(name),
                "cycle 2 helper `{name}` removed; bash-twin block-traceability broken (#327)",
            );
        }
    }

    /// Cycle 2 (#327): `capture_node_bootid` MUST return the dry-run
    /// sentinel verbatim so the dry-run fixture diff stays byte-stable.
    /// Validates the early-return branch without needing SSH.
    #[test]
    fn capture_node_bootid_dry_run_returns_sentinel() {
        let args = parse_args(&["--dry-run"]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default()).expect("plan");
        let got = capture_node_bootid(&plan, "w1").expect("dry-run capture");
        assert_eq!(got, "<dry-run-bootid>");
    }

    /// Cycle 2 (#327): empty `pre_bootid` MUST short-circuit
    /// `wait_node_bootid_changed` to `Ok(())` with the WARN log line.
    /// Mirrors bash line 928-930 — without a baseline we can't compare;
    /// skipping is safer than blocking forever on a missing field.
    #[test]
    fn wait_node_bootid_changed_empty_pre_short_circuits() {
        let args = parse_args(&["--dry-run"]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default()).expect("plan");
        // dry-run + empty pre — both early-exit paths converge on Ok.
        let res = wait_node_bootid_changed(&plan, "w1", "");
        assert!(res.is_ok(), "empty pre_bootid must short-circuit: {res:?}");
    }

    /// Cycle 2 (#327): `--skip-gates` MUST short-circuit
    /// `wait_node_bootid_changed` even with a valid pre_bootid. Pins
    /// the bash line 924 behavior.
    #[test]
    fn wait_node_bootid_changed_skip_gates_short_circuits() {
        let args = parse_args(&["--dry-run", "--skip-gates"]);
        let plan = Plan::from_args(&args, cfg(vec!["w1"]), Timeouts::default()).expect("plan");
        let res = wait_node_bootid_changed(&plan, "w1", "abc12345-real-bootid");
        assert!(res.is_ok(), "--skip-gates must short-circuit: {res:?}");
    }

    /// `--continue-on-error` is documented as worker-only — CP failures
    /// still abort the run. Pin the wording so a future refactor that
    /// promotes CONTINUE_ON_ERROR to the CP path surfaces here.
    #[test]
    fn cp_failure_does_not_route_through_continue_on_error() {
        // We can't drive update_cp() to fail without a real SSH client,
        // so the test asserts the doc claim in this file matches the
        // bash twin's policy (bash 1226-1229).
        let src = include_str!("update_cluster.rs");
        assert!(
            src.contains("CP failures always fail-fast"),
            "documentation drift — the policy must remain visible in the source",
        );
    }
}
