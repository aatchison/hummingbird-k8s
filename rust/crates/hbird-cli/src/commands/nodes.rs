//! `hbird nodes` — bash twin: `scripts/kubectl-k8s.sh get nodes`
//! (via `make nodes`).
//!
//! Convenience shorthand for `hbird kubectl get nodes`. The bash
//! `make nodes` recipe reads `CONFIG=<path>` for `CP_NAME` + `KVM_HOST`;
//! the Rust shape uses the same lookup via clap's `env = "CONFIG"`
//! fallback.
//!
//! Behavior tracked by [#288].
//!
//! [#288]: https://github.com/aatchison/hummingbird-k8s/issues/288

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;

/// Arguments for `hbird nodes`.
#[derive(Debug, Args)]
pub struct NodesArgs {
    /// Path to `cluster.local.conf`. Optional — the bash twin works
    /// without one as long as `CP_NAME` + `KVM_HOST` are in the env.
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,
}

/// Dispatch — currently `Err("not yet implemented")`. Echoes parsed
/// args so the operator can confirm clap captured the right config
/// before the stub bails. (PR #319 round-2 review L8 MEDIUM.)
pub fn run(args: NodesArgs) -> Result<()> {
    let config = args
        .config
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unset>".into());
    Err(anyhow!(
        "hbird nodes: not yet implemented — tracked by #288 \
         (https://github.com/aatchison/hummingbird-k8s/issues/288). \
         Parsed: --config {config}. \
         Use `make nodes CONFIG=…` until then."
    ))
}
