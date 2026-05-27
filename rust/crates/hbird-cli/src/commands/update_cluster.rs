//! `hbird update-cluster` — bash twin: `scripts/update-cluster.sh`.
//!
//! Behavior tracked by [#286]. Flag set mirrors the bash arg-parsing
//! block (see `scripts/update-cluster.sh` near line 161) so an operator
//! reading `make update-cluster FLAGS='--dry-run --parallel=2'` can move
//! to `hbird update-cluster --dry-run --parallel 2` without translation.
//!
//! [#286]: https://github.com/aatchison/hummingbird-k8s/issues/286

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;

/// Arguments for `hbird update-cluster`.
///
/// Maps to the bash twin's flag set:
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
    /// path, #272).
    #[arg(long)]
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

/// Dispatch — currently `Err("not yet implemented")`.
pub fn run(_args: UpdateClusterArgs) -> Result<()> {
    Err(anyhow!(
        "hbird update-cluster: not yet implemented — tracked by #286 \
         (https://github.com/aatchison/hummingbird-k8s/issues/286). \
         Use `make update-cluster CONFIG=… [FLAGS=…]` until then."
    ))
}
