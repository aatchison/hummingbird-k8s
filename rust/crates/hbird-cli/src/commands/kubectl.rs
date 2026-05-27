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

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;

use crate::cp_kubectl::{CpTarget, cp_kubectl_raw};

/// Arguments for `hbird kubectl`.
#[derive(Debug, Args)]
pub struct KubectlArgs {
    /// Path to `cluster.local.conf` (for `CP_NAME` + `KVM_HOST` lookup).
    /// Bash twin reads it from `CONFIG` env when set.
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,

    /// libvirt domain name of the control plane. Overrides the value
    /// pulled from `--config`. Bash twin reads `CP_NAME` from env.
    #[arg(long, value_name = "NAME", env = "CP_NAME")]
    pub cp_name: Option<String>,

    /// Static IP of the control plane. Overrides virsh resolution.
    /// Bash twin reads `CP_IP` from env / CONFIG.
    #[arg(long, value_name = "IP", env = "CP_IP")]
    pub cp_ip: Option<String>,

    /// SSH alias of the KVM host (ProxyJump). Overrides the value
    /// from `--config`. Bash twin reads `KVM_HOST` from env / CONFIG.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

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

/// Dispatch. Forwards `args.args` to `kubectl` on the CP via SSH +
/// ProxyJump (same path as `update-cluster`'s drain/uncordon). The
/// bash twin's `scripts/kubectl-k8s.sh "$@"` runs in interactive
/// shells too; the Rust shape mirrors the simple "execute one
/// command, print stdout/stderr, exit on err" loop because that's
/// the operator-facing surface `make kubectl ARGS=...` exposes.
///
/// Argument joining: clap captures pass-through args as `Vec<String>`
/// already shell-tokenized (the operator's shell did the splitting).
/// We rejoin with single spaces — same shape `cp_kubectl_raw` sees
/// from the in-tree callers — and let kubectl's own arg parser
/// re-tokenize on the remote side. Any value containing a space
/// would need shell-quoting on the operator's command line anyway,
/// just like `make kubectl ARGS='get pods --field-selector="a=b"'`.
///
/// # Errors
///
/// - Empty `args` (operator typed `hbird kubectl` with nothing after)
///   — bash twin: `scripts/kubectl-k8s.sh` with no args runs kubectl's
///   own help; we surface a clearer message instead because the SSH
///   spawn is non-trivial.
/// - `cp_kubectl_raw` propagates SSH / non-zero-exit / metacharacter
///   rejection errors verbatim.
pub fn run(args: KubectlArgs) -> Result<()> {
    if args.args.is_empty() {
        bail!(
            "hbird kubectl: no positional args supplied. \
             Usage: `hbird kubectl get pods -A` (everything after \
             `kubectl` is forwarded to the remote kubectl)."
        );
    }
    let target = resolve_target(&args)?;
    let command = args.args.join(" ");
    let out = cp_kubectl_raw(&target, &command)?;
    io::stdout()
        .write_all(&out.stdout)
        .context("write kubectl stdout")?;
    io::stderr()
        .write_all(&out.stderr)
        .context("write kubectl stderr")?;
    Ok(())
}

/// Resolve a [`CpTarget`] — same defaulting order as
/// [`crate::commands::nodes::resolve_target`]. See that fn's docs for
/// the #306-avoidance rationale.
#[tracing::instrument(level = "debug", skip(args), err(Debug))]
fn resolve_target(args: &KubectlArgs) -> Result<CpTarget> {
    let config = match &args.config {
        Some(path) => Some(hbird_config::parse(path).map_err(|e| anyhow!("{e}"))?),
        None => None,
    };

    let _cp_name = args
        .cp_name
        .clone()
        .or_else(|| config.as_ref().map(|c| c.cp_name.clone()))
        .ok_or_else(|| {
            anyhow!(
                "CP_NAME required (set in --config <cluster.local.conf>, \
                 or pass --cp-name / CP_NAME env)"
            )
        })?;

    let cp_ip = args
        .cp_ip
        .clone()
        .or_else(|| config.as_ref().and_then(|c| c.cp_ip.clone()))
        .ok_or_else(|| {
            anyhow!(
                "CP_IP required (set in --config <cluster.local.conf>, \
                 or pass --cp-ip / CP_IP env). virsh-domifaddr resolution \
                 is not yet wired in the Rust path — operator must pin \
                 CP_IP= for now."
            )
        })?;

    // #306: ProxyJump defaulting happens AFTER config sourcing.
    let kvm_host = args
        .kvm_host
        .clone()
        .or_else(|| config.as_ref().and_then(|c| c.kvm_host.clone()))
        .filter(|s| !s.is_empty());

    Ok(CpTarget { cp_ip, kvm_host })
}
