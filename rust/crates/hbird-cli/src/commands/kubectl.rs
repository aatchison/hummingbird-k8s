//! `hbird kubectl …` — bash twin: `scripts/kubectl-k8s.sh "$@"`
//! (via `make kubectl ARGS='get pods -A'`).
//!
//! Pure pass-through: every positional arg after `kubectl` is forwarded
//! to the real `kubectl` binary inside the SSH tunnel. The bash twin
//! reads `CONFIG` from the env for `CP_NAME` + `KVM_HOST`; the Rust
//! shape uses the same env-var fallback.
//!
//! Behavior tracked by [#288].
//!
//! [#288]: https://github.com/aatchison/hummingbird-k8s/issues/288

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;

/// Arguments for `hbird kubectl`.
#[derive(Debug, Args)]
pub struct KubectlArgs {
    /// Path to `cluster.local.conf` (for `CP_NAME` + `KVM_HOST` lookup).
    /// Bash twin reads it from `CONFIG` env when set.
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,

    /// Positional pass-through. Everything after the subcommand (and
    /// after `--`, if used to disambiguate flags) is forwarded verbatim
    /// to `kubectl`.
    ///
    /// `trailing_var_arg` makes clap accept dashed args without
    /// treating them as our own (so `hbird kubectl get pods -A` does
    /// what the operator means). `allow_hyphen_values` is the matching
    /// permission for individual leading-dash values.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub args: Vec<String>,
}

/// Dispatch — currently `Err("not yet implemented")`. Echoes the
/// parsed pass-through args so the operator can confirm clap captured
/// the dashed flags they typed before the stub bails. (PR #319 round-2
/// review L8 MEDIUM.)
pub fn run(args: KubectlArgs) -> Result<()> {
    let config = args
        .config
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unset>".into());
    Err(anyhow!(
        "hbird kubectl: not yet implemented — tracked by #288 \
         (https://github.com/aatchison/hummingbird-k8s/issues/288). \
         Parsed: --config {config} args={:?}. \
         Use `make kubectl ARGS='…'` until then.",
        args.args,
    ))
}
