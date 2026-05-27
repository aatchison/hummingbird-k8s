//! `hbird destroy-cluster` — bash twin: `scripts/destroy-cluster.sh`.
//!
//! Behavior tracked by [#289].
//!
//! [#289]: https://github.com/aatchison/hummingbird-k8s/issues/289

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;
use clap::builder::BoolishValueParser;

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
}

/// Dispatch — currently `Err("not yet implemented")`. Echoes parsed
/// args so the operator can confirm clap captured the right config +
/// host before the stub bails. (PR #319 round-2 review L8 MEDIUM.)
pub fn run(args: DestroyClusterArgs) -> Result<()> {
    Err(anyhow!(
        "hbird destroy-cluster: not yet implemented — tracked by #289 \
         (https://github.com/aatchison/hummingbird-k8s/issues/289). \
         Parsed: --config {} --kvm-host {}. \
         Use `make destroy-cluster CONFIG=…` until then.",
        args.config.display(),
        args.kvm_host.as_deref().unwrap_or("<unset>"),
    ))
}
