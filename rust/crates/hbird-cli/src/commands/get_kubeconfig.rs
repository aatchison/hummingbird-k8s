//! `hbird get-kubeconfig` — bash twin: `make get-kubeconfig`
//! (daily-use sibling of `export-argocd`, see issue #195).
//!
//! The bash twin calls `scripts/export-argocd.sh` with operator-friendly
//! defaults (`--output kubeconfig.yaml`, `--context-name=$CP_NAME`). The
//! Rust shape delegates to [`crate::commands::export_argocd`]'s shared
//! [`crate::commands::export_argocd::export_kubeconfig`] core, passing
//! those defaults via [`crate::commands::export_argocd::ExportOptions::for_get_kubeconfig`].
//!
//! Same #306/#307 fixes apply here — see the export-argocd module docs.
//!
//! Behavior tracked by [#288].
//!
//! [#288]: https://github.com/aatchison/hummingbird-k8s/issues/288

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::commands::export_argocd::{ExportOptions, export_kubeconfig};

/// Arguments for `hbird get-kubeconfig`.
#[derive(Debug, Args)]
pub struct GetKubeconfigArgs {
    /// Path to `cluster.local.conf`. Required (the bash twin sources it
    /// to read `CP_NAME`).
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,

    /// Output path for the kubeconfig. Bash-twin default:
    /// `kubeconfig.yaml`.
    #[arg(long, value_name = "PATH", default_value = "kubeconfig.yaml")]
    pub output: PathBuf,

    /// Override the API-server URL written into the kubeconfig (bash:
    /// `--server`).
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,

    /// Override the context name written into the kubeconfig (bash:
    /// `--context-name`). Default: `CP_NAME` from `--config`. The bash
    /// twin's `--context-name` spelling is preserved via clap `alias`
    /// so operator muscle memory keeps working. (PR #319 round-2 review
    /// L9 MEDIUM.)
    #[arg(long, alias = "context-name", value_name = "NAME")]
    pub context: Option<String>,

    /// Overwrite `--output` if it already exists (bash: `--force`).
    #[arg(long)]
    pub force: bool,

    /// SSH ProxyJump host. Defaults to CONFIG's `KVM_HOST` when unset
    /// (post-#306 resolution order). Pass `--proxy-jump=''` to disable.
    #[arg(long, value_name = "HOST")]
    pub proxy_jump: Option<String>,

    /// Static CP IP. Overrides CONFIG's `CP_IP`. Required when CONFIG
    /// doesn't pin one.
    #[arg(long, value_name = "IP", env = "CP_IP")]
    pub cp_ip: Option<String>,
}

/// Dispatch — builds an [`ExportOptions`] with the operator-friendly
/// `--context-name=$CP_NAME` default (vs. the `hummingbird-$CP_NAME`
/// default that `export-argocd` uses) and delegates to the shared
/// core.
pub fn run(args: GetKubeconfigArgs) -> Result<()> {
    let opts = ExportOptions::for_get_kubeconfig(
        &args.config,
        args.output,
        args.server,
        args.context,
        args.proxy_jump,
        args.cp_ip,
        args.force,
    )?;
    export_kubeconfig(&opts)
}
