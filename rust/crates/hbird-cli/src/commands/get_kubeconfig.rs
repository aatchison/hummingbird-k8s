//! `hbird get-kubeconfig` — bash twin: `make get-kubeconfig`
//! (daily-use sibling of `export-argocd`, see issue #195).
//!
//! The bash twin calls `scripts/export-argocd.sh` with operator-friendly
//! defaults (`--output kubeconfig.yaml`, `--context-name=$CP_NAME`). The
//! Rust shape mirrors those defaults.
//!
//! Behavior tracked by [#288].
//!
//! [#288]: https://github.com/aatchison/hummingbird-k8s/issues/288

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;

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

    /// SSH ProxyJump host inserted via the kubeconfig's exec plugin
    /// (bash: `--proxy-jump=HOST`). Useful when the kubeconfig will be
    /// consumed from a workstation that can't reach the CP directly.
    #[arg(long, value_name = "HOST")]
    pub proxy_jump: Option<String>,
}

/// Dispatch — currently `Err("not yet implemented")`. Echoes parsed
/// args so the operator can confirm clap captured the right config +
/// output before the stub bails. (PR #319 round-2 review L8 MEDIUM.)
pub fn run(args: GetKubeconfigArgs) -> Result<()> {
    Err(anyhow!(
        "hbird get-kubeconfig: not yet implemented — tracked by #288 \
         (https://github.com/aatchison/hummingbird-k8s/issues/288). \
         Parsed: --config {} --output {}. \
         Use `make get-kubeconfig CONFIG=…` until then.",
        args.config.display(),
        args.output.display(),
    ))
}
