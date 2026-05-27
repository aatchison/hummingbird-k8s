//! `hbird deploy-cluster` — bash twin: `scripts/deploy-cluster.sh`.
//!
//! Behavior tracked by [#289]. For [#283] this file only locks in the
//! flag set so the operator-facing surface is stable.
//!
//! [#283]: https://github.com/aatchison/hummingbird-k8s/issues/283
//! [#289]: https://github.com/aatchison/hummingbird-k8s/issues/289

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;

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
    #[arg(long)]
    pub no_sudo: bool,
}

/// Dispatch — currently `Err("not yet implemented")`.
pub fn run(_args: DeployClusterArgs) -> Result<()> {
    Err(anyhow!(
        "hbird deploy-cluster: not yet implemented — tracked by #289 \
         (https://github.com/aatchison/hummingbird-k8s/issues/289). \
         Use `make deploy-cluster CONFIG=…` until then."
    ))
}
