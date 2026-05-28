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
use crate::cp_resolve::resolve_cp_ip_via_ssh;

/// Round-2 lens L3 HIGH: when the remote `kubectl` exits non-zero,
/// the bash twin (`scripts/kubectl-k8s.sh` under `set -e`) propagates
/// `$?` verbatim — kubectl-not-found is 127, validation is 1,
/// resource-not-found is 1, etc. The default `Result<()>` → anyhow
/// path always exits 1, hiding the kubectl exit code.
///
/// This helper inspects the anyhow chain for an `hbird_ssh::Error::NonZeroExit`
/// — if found, emits its stdout/stderr verbatim and `std::process::exit`s
/// with the captured kubectl status code. Falls through to the normal
/// anyhow-error path for any other failure shape (SSH transport, metachar
/// guard, IdentityFileMissing, etc.) so operator diagnostics are preserved.
/// Pub re-export so sibling subcommands (`nodes`) can share the same
/// kubectl-exit-code propagation contract without copy-paste.
pub fn propagate_kubectl_exit_or_bail_pub(err: anyhow::Error) -> Result<()> {
    propagate_kubectl_exit_or_bail(err)
}

fn propagate_kubectl_exit_or_bail(err: anyhow::Error) -> Result<()> {
    if let Some(hbird_ssh::Error::NonZeroExit {
        status,
        stdout,
        stderr,
        ..
    }) = err.downcast_ref::<hbird_ssh::Error>()
    {
        let _ = io::stdout().write_all(stdout.as_bytes());
        let _ = io::stderr().write_all(stderr.as_bytes());
        std::process::exit(status.code().unwrap_or(1));
    }
    Err(err)
}

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
    match cp_kubectl_raw(&target, &command) {
        Ok(out) => {
            io::stdout()
                .write_all(&out.stdout)
                .context("write kubectl stdout")?;
            io::stderr()
                .write_all(&out.stderr)
                .context("write kubectl stderr")?;
            Ok(())
        }
        Err(e) => propagate_kubectl_exit_or_bail(e),
    }
}

/// Resolve a [`CpTarget`] — same defaulting order as
/// [`crate::commands::nodes::resolve_target`]. See that fn's docs for
/// the #306-avoidance rationale.
///
/// Defaulting order for `cp_ip`:
/// 1. `--cp-ip` / `CP_IP` env (explicit, wins).
/// 2. `cp_ip` from `--config`'s parsed cluster.local.conf.
/// 3. **Auto-resolution** via `ssh $KVM_HOST virsh -c qemu:///system
///    domifaddr $CP_NAME` (PR #366 round-2 H1 fix — closes the
///    chicken-egg where the 3 etcd scripts called
///    `${CP_IP:-$(hbird kubectl get nodes …)}` to discover CP_IP but
///    `hbird kubectl` itself hard-failed when CP_IP was unset).
///    Mirrors the bash twin `scripts/kubectl-k8s.sh` (removed in #353)
///    which did the same virsh-domifaddr lookup itself.
/// 4. If KVM_HOST is also unset, fail with a clear error pointing the
///    operator at either CP_IP or KVM_HOST.
///
/// Deferred originally in #289 (the Rust placeholder comment said
/// "virsh-domifaddr resolution is not yet wired in the Rust path —
/// operator must pin CP_IP= for now"); #366 round-2 closes that gap.
#[tracing::instrument(level = "debug", skip(args), err(Debug))]
fn resolve_target(args: &KubectlArgs) -> Result<CpTarget> {
    let config = match &args.config {
        Some(path) => Some(hbird_config::parse(path).map_err(|e| anyhow!("{e}"))?),
        None => None,
    };

    let cp_name = args
        .cp_name
        .clone()
        .or_else(|| config.as_ref().map(|c| c.cp_name.clone()))
        .ok_or_else(|| {
            anyhow!(
                "CP_NAME required (set in --config <cluster.local.conf>, \
                 or pass --cp-name / CP_NAME env)"
            )
        })?;

    // #306: ProxyJump defaulting happens AFTER config sourcing. Resolve
    // kvm_host first so the cp_ip auto-resolution fallback below can
    // use it.
    let kvm_host = args
        .kvm_host
        .clone()
        .or_else(|| config.as_ref().and_then(|c| c.kvm_host.clone()))
        .filter(|s| !s.is_empty());

    let cp_ip = if let Some(ip) = args.cp_ip.clone().filter(|s| !s.is_empty()) {
        ip
    } else if let Some(ip) = config
        .as_ref()
        .and_then(|c| c.cp_ip.clone())
        .filter(|s| !s.is_empty())
    {
        ip
    } else if let Some(host) = kvm_host.as_deref() {
        // PR #366 round-2 H1: virsh-domifaddr resolution via SSH. Mirrors
        // the deleted bash twin `scripts/kubectl-k8s.sh`'s
        // `ssh $KVM_HOST virsh ... domifaddr $CP_NAME | awk /ipv4/...`
        // pipeline. Uses the SshExec trait from #357 so unit tests can
        // mock the transport.
        let ssh_opts = hbird_ssh::SshOptions::new(host.to_string());
        let client = hbird_ssh::Client::new(ssh_opts);
        resolve_cp_ip_via_ssh(&client, host, &cp_name).with_context(|| {
            format!(
                "resolve CP_IP via virsh-domifaddr on KVM_HOST={host} \
                 for CP_NAME={cp_name}"
            )
        })?
    } else {
        bail!(
            "CP_IP required (set in --config <cluster.local.conf>, \
             or pass --cp-ip / CP_IP env). For workstation operators \
             without local libvirt, set KVM_HOST=<ssh-alias> so we can \
             query libvirt on the KVM host via SSH (mirrors the deleted \
             scripts/kubectl-k8s.sh)."
        );
    };

    Ok(CpTarget { cp_ip, kvm_host })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a [`KubectlArgs`] with the supplied fields and
    /// empty positional args (since `resolve_target` doesn't read them).
    fn args(cp_name: Option<&str>, cp_ip: Option<&str>, kvm_host: Option<&str>) -> KubectlArgs {
        KubectlArgs {
            config: None,
            cp_name: cp_name.map(str::to_string),
            cp_ip: cp_ip.map(str::to_string),
            kvm_host: kvm_host.map(str::to_string),
            args: Vec::new(),
        }
    }

    /// (a) Explicit `--cp-ip` wins; no SSH is attempted because the
    /// auto-resolution path is only taken when cp_ip is absent.
    #[test]
    fn resolve_target_returns_existing_cp_ip_unchanged() {
        let a = args(Some("hbird-cp1"), Some("10.0.0.7"), Some("geary"));
        let target = resolve_target(&a).expect("explicit cp_ip resolves without SSH");
        assert_eq!(target.cp_ip, "10.0.0.7");
        assert_eq!(target.kvm_host.as_deref(), Some("geary"));
    }

    /// (b) When CP_IP is unset AND KVM_HOST is unset, fail with a clear
    /// error pointing at both options. Bash twin `kubectl-k8s.sh`
    /// hard-required `KVM_HOST` too — operators on workstations without
    /// libvirt need a way to reach the CP.
    #[test]
    fn resolve_target_errors_when_no_cp_ip_and_no_kvm_host() {
        let a = args(Some("hbird-cp1"), None, None);
        let err = resolve_target(&a).expect_err("missing cp_ip + no kvm_host should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("CP_IP required"),
            "expected 'CP_IP required' wording: {msg}"
        );
        assert!(
            msg.contains("KVM_HOST"),
            "expected hint pointing at KVM_HOST: {msg}"
        );
    }

    /// CP_NAME is required regardless — the bash twin defaulted
    /// `CP_NAME=hummingbird-k8s`, but the Rust kubectl path keeps the
    /// requirement explicit so operator confusion doesn't slip through
    /// (mirrors nodes.rs precedent).
    #[test]
    fn resolve_target_errors_when_no_cp_name() {
        let a = args(None, Some("10.0.0.7"), None);
        let err = resolve_target(&a).expect_err("missing cp_name should fail");
        assert!(
            err.to_string().contains("CP_NAME required"),
            "expected 'CP_NAME required' wording: {err}"
        );
    }
}
