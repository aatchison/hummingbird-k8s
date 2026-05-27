//! `hbird verify <sub>` — bash twins: `scripts/verify-encryption.sh`,
//! `scripts/verify-hardening.sh`, `scripts/verify-app-deploy.sh`.
//!
//! Behavior tracked by [#287]. The three bash scripts take their input
//! through env vars (`CONFIG`, `CP_NAME`, `KVM_HOST`, `KUBECTL`) rather
//! than positional args; the Rust shape promotes them to flags while
//! keeping the env-var fallbacks via clap's `env = …` attribute.
//!
//! [#287]: https://github.com/aatchison/hummingbird-k8s/issues/287

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};

/// Top-level `hbird verify` — dispatches to one of four sub-subcommands.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    #[command(subcommand)]
    pub command: VerifySubcommand,
}

/// The four `verify-*` Makefile targets, plus `all` (which chains the
/// other three in sequence — bash twin: `make verify-all`).
#[derive(Debug, Subcommand)]
pub enum VerifySubcommand {
    /// Verify etcd encryption-at-rest on the control plane.
    ///
    /// Bash twin: `scripts/verify-encryption.sh`.
    Encryption(VerifyCommonArgs),

    /// Verify PSA + audit + kubelet protect-kernel-defaults.
    ///
    /// Bash twin: `scripts/verify-hardening.sh`.
    Hardening(VerifyCommonArgs),

    /// End-to-end PSA-restricted nginx + pod-to-pod connectivity test.
    ///
    /// Bash twin: `scripts/verify-app-deploy.sh`.
    AppDeploy(VerifyCommonArgs),

    /// Run all three verifiers in sequence (encryption → hardening →
    /// app-deploy). Bash twin: `make verify-all`.
    All(VerifyCommonArgs),
}

/// Shared flags for every `verify` sub-subcommand. The bash twins all
/// take the same set of env vars; one struct keeps the surface uniform.
#[derive(Debug, Args)]
pub struct VerifyCommonArgs {
    /// Path to `cluster.local.conf`. The bash scripts read `CP_NAME` +
    /// `KVM_HOST` from this file; the Rust shape keeps the same lookup.
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,

    /// libvirt domain name of the control plane. Overrides the value
    /// pulled from `--config`.
    #[arg(long, value_name = "NAME", env = "CP_NAME")]
    pub cp_name: Option<String>,

    /// SSH alias of the KVM host. Overrides the value pulled from
    /// `--config` / `KVM_HOST` env.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// Path to a `kubectl` binary or wrapper. Default: the project's
    /// `scripts/kubectl-k8s.sh` SSH-tunnel wrapper. Bash twin reads
    /// `KUBECTL` from env.
    #[arg(long, value_name = "PATH", env = "KUBECTL")]
    pub kubectl: Option<PathBuf>,
}

/// Dispatch — currently `Err("not yet implemented")`.
pub fn run(args: VerifyArgs) -> Result<()> {
    let which = match args.command {
        VerifySubcommand::Encryption(_) => "verify-encryption",
        VerifySubcommand::Hardening(_) => "verify-hardening",
        VerifySubcommand::AppDeploy(_) => "verify-app-deploy",
        VerifySubcommand::All(_) => "verify-all",
    };
    Err(anyhow!(
        "hbird verify {which}: not yet implemented — tracked by #287 \
         (https://github.com/aatchison/hummingbird-k8s/issues/287). \
         Use `make {which}` until then."
    ))
}
