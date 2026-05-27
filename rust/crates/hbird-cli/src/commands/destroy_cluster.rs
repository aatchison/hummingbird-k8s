//! `hbird destroy-cluster` — bash twin: `scripts/destroy-cluster.sh`.
//!
//! Behavior tracked by [#289].
//!
//! [#289]: https://github.com/aatchison/hummingbird-k8s/issues/289

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;

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
    /// path, #305).
    #[arg(long)]
    pub no_sudo: bool,
}

/// Dispatch — currently `Err("not yet implemented")`.
pub fn run(_args: DestroyClusterArgs) -> Result<()> {
    Err(anyhow!(
        "hbird destroy-cluster: not yet implemented — tracked by #289 \
         (https://github.com/aatchison/hummingbird-k8s/issues/289). \
         Use `make destroy-cluster CONFIG=…` until then."
    ))
}
