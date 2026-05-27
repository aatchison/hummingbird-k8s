//! `hbird export-argocd` — bash twin: `scripts/export-argocd.sh`.
//!
//! Bash flag set (see `scripts/export-argocd.sh` near line 188):
//! `--output`, `--server`, `--context-name`, `--proxy-jump`, `--force`.
//!
//! Behavior tracked by [#288].
//!
//! [#288]: https://github.com/aatchison/hummingbird-k8s/issues/288

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;

/// Arguments for `hbird export-argocd`.
#[derive(Debug, Args)]
pub struct ExportArgocdArgs {
    /// Path to `cluster.local.conf`.
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,

    /// Output path. Bash-twin default: `argocd-kubeconfig.yaml`.
    #[arg(long, value_name = "PATH", default_value = "argocd-kubeconfig.yaml")]
    pub output: PathBuf,

    /// Override the API-server URL written into the kubeconfig.
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,

    /// Context name. Bash-twin default: `hummingbird-$CP_NAME` (prefix
    /// avoids colliding with whatever ArgoCD already has registered).
    /// CLI flag renamed from `--context-name` to `--context` for
    /// consistency with the rest of the binary; the bash twin retains
    /// `--context-name`.
    #[arg(long, value_name = "NAME")]
    pub context: Option<String>,

    /// SSH ProxyJump host inserted via the kubeconfig's exec plugin.
    #[arg(long, value_name = "HOST")]
    pub proxy_jump: Option<String>,

    /// Overwrite `--output` if it already exists.
    #[arg(long)]
    pub force: bool,
}

/// Dispatch — currently `Err("not yet implemented")`.
pub fn run(_args: ExportArgocdArgs) -> Result<()> {
    Err(anyhow!(
        "hbird export-argocd: not yet implemented — tracked by #288 \
         (https://github.com/aatchison/hummingbird-k8s/issues/288). \
         Use `make export-argocd CONFIG=…` until then."
    ))
}
