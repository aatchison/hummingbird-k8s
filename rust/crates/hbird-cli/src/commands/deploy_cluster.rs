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
use clap::builder::BoolishValueParser;

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
}

/// Dispatch — currently `Err("not yet implemented")`. The args echo
/// gives operators an at-a-glance confirmation that clap parsed what
/// they typed (config path + KVM host) before the stub bails. (PR #319
/// round-2 review L8 MEDIUM.)
pub fn run(args: DeployClusterArgs) -> Result<()> {
    Err(anyhow!(
        "hbird deploy-cluster: not yet implemented — tracked by #289 \
         (https://github.com/aatchison/hummingbird-k8s/issues/289). \
         Parsed: --config {} --kvm-host {}. \
         Use `make deploy-cluster CONFIG=…` until then.",
        args.config.display(),
        args.kvm_host.as_deref().unwrap_or("<unset>"),
    ))
}
