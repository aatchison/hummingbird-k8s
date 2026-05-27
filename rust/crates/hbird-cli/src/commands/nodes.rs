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

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::Args;

use crate::cp_kubectl::{CpTarget, cp_kubectl_raw};

/// Arguments for `hbird nodes`.
#[derive(Debug, Args)]
pub struct NodesArgs {
    /// Path to `cluster.local.conf`. Optional — the bash twin works
    /// without one as long as `CP_NAME` + `KVM_HOST` are in the env.
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
}

/// Dispatch. Reuses [`crate::cp_kubectl::cp_kubectl_raw`] for SSH +
/// kubectl wiring — identical to `update-cluster`'s drain/uncordon
/// path (#325). Writes captured stdout to the operator's stdout so
/// `hbird nodes | grep Ready` works the way `make nodes | grep`
/// already does.
///
/// Defaulting order (deliberate, mirrors the bash-post-#306 shape):
/// config-file values are loaded FIRST, then CLI flags / env override.
/// This avoids the bash bug where `PROXY_JUMP="${KVM_HOST:-}"` resolved
/// to empty before `source $CONFIG` ran.
pub fn run(args: NodesArgs) -> Result<()> {
    let target = resolve_target(&args)?;
    let out = cp_kubectl_raw(&target, "get nodes")?;
    // Forward stdout verbatim — same bytes a `make nodes` operator
    // already pipes into grep. We don't add the `[nodes]` prefix the
    // way update-cluster does; the bash twin's `kubectl-k8s.sh get
    // nodes` doesn't either.
    io::stdout()
        .write_all(&out.stdout)
        .context("write kubectl stdout")?;
    // kubectl emits warnings on stderr (`Warning: ...`); preserve that
    // separation so log-grep operators see them in the expected stream.
    io::stderr()
        .write_all(&out.stderr)
        .context("write kubectl stderr")?;
    Ok(())
}

/// Resolve a [`CpTarget`] from a mix of CLI args, env vars, and
/// `--config`. Mirrors the post-#306 bash semantics:
///
/// 1. Load `--config` (if set) via [`hbird_config::parse`].
/// 2. Resolve `cp_name` from CLI/env, else CONFIG, else fail.
/// 3. Resolve `cp_ip` from CLI/env, else CONFIG, else fail.
/// 4. Resolve `kvm_host` from CLI/env, else CONFIG (ProxyJump defaults
///    AFTER config sourcing — #306 bash bug avoided).
///
/// The Rust path makes the cp_ip-via-virsh fallback OFFLINE: the
/// operator must either pin `CP_IP=` in CONFIG or pass `--cp-ip` /
/// `CP_IP=` env. virsh-domifaddr resolution is deferred to the live
/// helper (`hbird-virt`) which is wired by #322 and not part of this
/// PR's scope.
#[tracing::instrument(level = "debug", skip(args), err(Debug))]
fn resolve_target(args: &NodesArgs) -> Result<CpTarget> {
    let config = match &args.config {
        Some(path) => Some(hbird_config::parse(path).map_err(|e| anyhow!("{e}"))?),
        None => None,
    };

    // cp_name (currently informational — kubectl get nodes doesn't
    // need it, but resolving early surfaces a missing-config error
    // before we attempt SSH).
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

    // ProxyJump defaulting happens AFTER config sourcing — explicitly
    // avoiding the #306 bash bug where the default was resolved BEFORE
    // the config file was sourced.
    let kvm_host = args
        .kvm_host
        .clone()
        .or_else(|| config.as_ref().and_then(|c| c.kvm_host.clone()))
        .filter(|s| !s.is_empty());

    Ok(CpTarget { cp_ip, kvm_host })
}
