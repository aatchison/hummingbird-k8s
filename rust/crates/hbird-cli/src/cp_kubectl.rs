//! Shared SSH-to-CP shim — runs commands (kubectl or arbitrary shell)
//! on the control plane via SSH. Used by both `update-cluster` (#322
//! cycle 1) and the Phase-3 subcommands (#288: `export-argocd`,
//! `get-kubeconfig`, `nodes`, `kubectl`). Extracted from
//! `commands/update_cluster.rs` so the post-Phase-1 subcommands can
//! reuse the proven SSH+metacharacter-defense wiring instead of
//! copy-pasting it. (PR #325 round-2 lens L5 MEDIUM requested a
//! `pub(crate)` re-export; #288 promotes it to a sibling module.)
//!
//! Shape coordinated with the parallel #287 (verify-*) dispatch: both
//! PRs need the same module surface, the first to land wins, the
//! second rebases. The API here covers BOTH consumers' needs so a
//! rebase is a no-op:
//!
//! - [`cp_kubectl_raw`] — kubectl wrapper that errors on non-zero exit
//!   (the update-cluster default).
//! - [`cp_kubectl_with_stdin_lenient`] — kubectl + stdin pipe, folds
//!   non-zero into [`CpExecOutput::success = false`] for
//!   verify-hardening's PSA-rejection check.
//! - [`cp_ssh_lenient`] — arbitrary shell command on the CP host
//!   (NOT wrapped in kubectl), tolerates non-zero. Used by
//!   verify-hardening's audit-log + kubelet probes.
//! - [`cp_ssh_capture`] — kubectl-free SSH that returns raw bytes for
//!   binary payloads (e.g. `sudo cat /etc/kubernetes/admin.conf` —
//!   the export-argocd / get-kubeconfig fetch path).
//!
//! Bash twin: `cp_kubectl` in `scripts/update-cluster.sh:425`. The
//! verify-hardening / verify-app-deploy bash twins reach the same
//! kubectl through the `scripts/kubectl-k8s.sh` wrapper, which has the
//! same SSH-ProxyJump shape as this shim. `scripts/export-argocd.sh`
//! issues `sudo cat /etc/kubernetes/admin.conf` directly via `cp_ssh()`.
//!
//! # Design notes
//!
//! The pre-extraction shape took an `&update_cluster::Plan` directly
//! and did its own log formatting. That coupled the shim to
//! update-cluster's much larger orchestration struct AND embedded
//! update-cluster's `[update-cluster] ` log-prefix + parallel-batch-tag
//! behavior. Verify-* and the Phase-3 subcommands don't share either
//! of those concerns.
//!
//! The split keeps responsibility narrow:
//!
//! - This module owns SSH options construction, the
//!   shell-metacharacter defense, and the kubectl-prefix attached
//!   to every command (`kubectl --kubeconfig=/etc/kubernetes/admin.conf
//!   …`). Returns the raw [`hbird_ssh::RunOutput`] / lossy
//!   [`CpExecOutput`] so each caller can format/log/inspect on its
//!   own terms.
//! - Each caller owns its own log prefix
//!   (`[update-cluster] `/`[export-argocd] `/`[verify-…] `).
//! - Each caller owns its own non-zero-exit policy.
//!
//! # #307 — non-TTY SSH (deliberate divergence from bash)
//!
//! [`hbird_ssh::Options::new`] defaults `BatchMode=yes` (no TTY). The
//! bash twin's `cp_ssh()` in `scripts/export-argocd.sh:326` uses
//! `ssh -t`, which allocates a remote PTY. Combined with `sudo cat
//! /etc/kubernetes/admin.conf`, modern sudo emits an OSC session-start
//! escape (`ESC ]3008;...ESC \\`) at the head of stdout — that escape
//! lands in the captured kubeconfig and breaks the downstream
//! `grep -q '^apiVersion:'` sanity check. The Rust path is the
//! correct shape; the bash twin's regression is tracked by #307.

use anyhow::{Context, Result, bail};

/// Minimal target descriptor for `cp_kubectl` — CP IP + optional
/// KVM-host ProxyJump alias. Constructed by each subcommand at the
/// top of its `run()` from clap args + config.
#[derive(Debug, Clone)]
pub(crate) struct CpTarget {
    /// IPv4 (or DNS-resolvable hostname) of the control-plane VM. Bash
    /// twin's `CP_IP` env var. Used as the SSH target after the
    /// optional KVM-host ProxyJump.
    pub cp_ip: String,
    /// SSH alias of the KVM host to ProxyJump through. `None` = direct
    /// SSH to `root@cp_ip` (operator already routes to the libvirt NAT
    /// subnet). Bash twin's `KVM_HOST` env var.
    pub kvm_host: Option<String>,
}

impl CpTarget {
    /// Construct an SSH options bundle (host + ProxyJump) targeting
    /// `root@cp_ip`. Mirrors bash `lib/build-common.sh`'s
    /// `ssh_opts_array CP_SSH_OPTS` + `--proxy-jump=$KVM_HOST` shape.
    pub(crate) fn cp_ssh_opts(&self) -> hbird_ssh::SshOptions {
        let mut opts = hbird_ssh::SshOptions::new(self.cp_ip.clone()).with_user("root");
        if let Some(jump) = self.kvm_host.as_deref() {
            opts = opts.with_proxy_jump(jump.to_string());
        }
        opts
    }
}

/// Defense-in-depth: reject command strings that would be interpreted
/// as shell metacharacters by the remote `/bin/sh -c`. kubectl
/// invocations don't need any of them; a metacharacter here would
/// indicate an upstream bug or injection attempt. PR #325 round-2
/// lens L1 HIGH.
///
/// Returns `Err` for `; & | ` `` ` `` `\n \r` or `$(`. `&&` / `||` are
/// caught by the single-char `&` and `|` rules.
fn reject_shell_metachars(context: &str, command: &str) -> Result<()> {
    if let Some(bad) = command
        .chars()
        .find(|c| matches!(c, ';' | '&' | '|' | '`' | '\n' | '\r'))
    {
        bail!(
            "{context}: refusing command with shell metacharacter '{bad}' \
             (commands are forwarded to remote /bin/sh -c; metachars would \
             execute as root on the CP). command={command:?}"
        );
    }
    if command.contains("$(") {
        bail!(
            "{context}: refusing command with `$(` substitution (executes \
             as root on the CP). command={command:?}"
        );
    }
    Ok(())
}

/// Run a kubectl command on the CP via SSH and return the captured
/// [`hbird_ssh::RunOutput`] verbatim. Bash twin: `cp_kubectl` in
/// `scripts/update-cluster.sh:425`.
///
/// The shim wraps `command` in
/// `kubectl --kubeconfig=/etc/kubernetes/admin.conf <command>` and
/// forwards it through SSH (ProxyJump=$KVM_HOST when set). It does
/// NOT log — each caller is responsible for emitting stdout/stderr
/// with its own bash-twin log prefix. It DOES reject shell
/// metacharacters before spawning SSH (see [`reject_shell_metachars`]).
///
/// # Errors
///
/// - `cp_kubectl_raw: refusing command with shell metacharacter '<c>'`
///   when the command contains `; & | ` `` ` `` `\n \r`.
/// - `cp_kubectl_raw: refusing command with $( substitution` when the
///   command contains `$(`.
/// - `cp_kubectl_raw: ssh-run failed for ...` for any
///   [`hbird_ssh::Error`] including [`hbird_ssh::Error::NonZeroExit`].
///   Callers that need to inspect non-zero exits (e.g. PSA admission)
///   should call [`cp_kubectl_with_stdin_lenient`] instead.
// `err(Debug)` directive demoted to a manual `tracing::debug!` event in
// the Err branch so callers (not this wrapper) decide ERROR-vs-debug
// policy per call site. `verify-hardening`'s PSA enforcement expects
// `kubectl apply` to exit non-zero — the auto ERROR event was noise.
// (#331; original wiring #326.)
#[tracing::instrument(
    level = "debug",
    skip(target),
    fields(cp_ip = %target.cp_ip, command = %command),
)]
pub(crate) fn cp_kubectl_raw(target: &CpTarget, command: &str) -> Result<hbird_ssh::RunOutput> {
    cp_kubectl_raw_inner(target, command)
        .inspect_err(|err| tracing::debug!(error = ?err, "cp_kubectl_raw failed"))
}

fn cp_kubectl_raw_inner(target: &CpTarget, command: &str) -> Result<hbird_ssh::RunOutput> {
    reject_shell_metachars("cp_kubectl_raw", command)?;
    let client = hbird_ssh::Client::new(target.cp_ssh_opts());
    let remote = format!("kubectl --kubeconfig=/etc/kubernetes/admin.conf {command}");
    client.run(&remote).with_context(|| {
        format!(
            "cp_kubectl_raw: ssh-run failed for `kubectl ... {command}` against {}",
            target.cp_ip
        )
    })
}

/// Run a kubectl command on the CP via SSH, piping `stdin` to it.
/// Tolerates non-zero exit — verify-hardening's PSA-rejection check
/// relies on `kubectl apply -f -` returning exit 1 with `violates
/// PodSecurity` on stderr.
///
/// Bash twin: the `apply -f - <<EOF` heredoc invocations in
/// `verify-hardening.sh:153` and `verify-app-deploy.sh:99`.
///
/// # Errors
///
/// - Same metacharacter rejection as [`cp_kubectl_raw`].
/// - `cp_kubectl_with_stdin_lenient: ssh-run failed for ...` only for
///   transport-level SSH failures (auth, ProxyJump, DNS). Non-zero
///   remote exit is folded into [`CpExecOutput::success = false`] so
///   the caller can inspect the stderr/stdout.
#[allow(dead_code)] // consumed by #287 verify-* dispatch (PSA-rejection check).
#[tracing::instrument(
    level = "debug",
    skip(target, stdin),
    fields(cp_ip = %target.cp_ip, command = %command, stdin_bytes = stdin.len()),
)]
pub(crate) fn cp_kubectl_with_stdin_lenient(
    target: &CpTarget,
    command: &str,
    stdin: &[u8],
) -> Result<CpExecOutput> {
    cp_kubectl_with_stdin_lenient_inner(target, command, stdin)
        .inspect_err(|err| tracing::debug!(error = ?err, "cp_kubectl_with_stdin_lenient failed"))
}

fn cp_kubectl_with_stdin_lenient_inner(
    target: &CpTarget,
    command: &str,
    stdin: &[u8],
) -> Result<CpExecOutput> {
    reject_shell_metachars("cp_kubectl_with_stdin_lenient", command)?;
    let client = hbird_ssh::Client::new(target.cp_ssh_opts());
    let remote = format!("kubectl --kubeconfig=/etc/kubernetes/admin.conf {command}");
    match client.run_with_stdin(&remote, stdin) {
        Ok(out) => Ok(CpExecOutput {
            success: true,
            stdout: out.stdout_lossy(),
            stderr: out.stderr_lossy(),
        }),
        Err(hbird_ssh::Error::NonZeroExit { stdout, stderr, .. }) => Ok(CpExecOutput {
            success: false,
            stdout,
            stderr,
        }),
        Err(e) => Err(e).with_context(|| {
            format!(
                "cp_kubectl_with_stdin_lenient: ssh-run failed for \
                 `kubectl ... {command}` against {}",
                target.cp_ip
            )
        }),
    }
}

/// Run an arbitrary shell command on the CP via SSH (not wrapped in
/// kubectl). Tolerates non-zero exit. Used by verify-hardening's
/// audit-log + kubelet checks, which need to `ps -ef | grep …` and
/// `[ -s /var/log/.../k8s-audit.log ]` on the CP host itself rather
/// than going through kubectl.
///
/// Metacharacter rejection does NOT apply here — verify-hardening's
/// checks use pipes (`ps -ef | grep`) which the bash twin runs
/// verbatim. Callers are responsible for the safety of their own
/// command strings; the public API surface is `pub(crate)` so only
/// in-tree code can call it.
///
/// # Errors
///
/// `cp_ssh_lenient: ssh-run failed for ...` only when the SSH
/// transport itself fails. Non-zero remote exit is folded into the
/// returned [`CpExecOutput::success = false`] so the caller can
/// inspect stdout/stderr — verify-hardening keys off "non-empty
/// stdout from `ps -ef | grep`" rather than exit code.
#[allow(dead_code)] // consumed by #287 verify-* dispatch (audit-log + kubelet probes).
#[tracing::instrument(
    level = "debug",
    skip(target),
    fields(cp_ip = %target.cp_ip, command = %command),
)]
pub(crate) fn cp_ssh_lenient(target: &CpTarget, command: &str) -> Result<CpExecOutput> {
    cp_ssh_lenient_inner(target, command)
        .inspect_err(|err| tracing::debug!(error = ?err, "cp_ssh_lenient failed"))
}

fn cp_ssh_lenient_inner(target: &CpTarget, command: &str) -> Result<CpExecOutput> {
    let client = hbird_ssh::Client::new(target.cp_ssh_opts());
    match client.run(command) {
        Ok(out) => Ok(CpExecOutput {
            success: true,
            stdout: out.stdout_lossy(),
            stderr: out.stderr_lossy(),
        }),
        Err(hbird_ssh::Error::NonZeroExit { stdout, stderr, .. }) => Ok(CpExecOutput {
            success: false,
            stdout,
            stderr,
        }),
        Err(e) => Err(e).with_context(|| {
            format!(
                "cp_ssh_lenient: ssh-run failed for `{command}` against {}",
                target.cp_ip
            )
        }),
    }
}

/// Run an arbitrary shell command on the CP via SSH and return the
/// raw [`hbird_ssh::RunOutput`] (errs on non-zero). Used by
/// export-argocd / get-kubeconfig to `sudo cat
/// /etc/kubernetes/admin.conf` — those callers need the raw bytes
/// (so they can detect a missing `apiVersion:` header before the
/// kubeconfig is written to disk) rather than the lossy [`String`]
/// shape [`cp_ssh_lenient`] produces.
///
/// Bash twin: `cp_ssh()` in `scripts/export-argocd.sh:326`. The Rust
/// path deliberately diverges from `ssh -t` to non-TTY (BatchMode=yes
/// is the hbird-ssh default) so modern sudo doesn't emit OSC
/// session-start escapes into the captured stdout — see module docs
/// for the full #307 rationale.
///
/// No metacharacter rejection: the export-argocd path issues a
/// single fixed `sudo cat /etc/kubernetes/admin.conf` literal under
/// its own crate's control. The `pub(crate)` surface limits exposure
/// to in-tree callers.
///
/// # Errors
///
/// - `cp_ssh_capture: ssh-run failed for ...` for any
///   [`hbird_ssh::Error`] including [`hbird_ssh::Error::NonZeroExit`].
#[tracing::instrument(
    level = "debug",
    skip(target),
    fields(cp_ip = %target.cp_ip, command = %command),
)]
pub(crate) fn cp_ssh_capture(target: &CpTarget, command: &str) -> Result<hbird_ssh::RunOutput> {
    let client = hbird_ssh::Client::new(target.cp_ssh_opts());
    client
        .run(command)
        .with_context(|| {
            format!(
                "cp_ssh_capture: ssh-run failed for `{command}` against {}",
                target.cp_ip
            )
        })
        .inspect_err(|err| tracing::debug!(error = ?err, "cp_ssh_capture failed"))
}

/// Lenient exec output — returned by [`cp_kubectl_with_stdin_lenient`]
/// and [`cp_ssh_lenient`] which fold non-zero remote exit into a
/// successful Result (so the caller can inspect stdout/stderr for
/// operator-grepped markers without an early-Err).
#[allow(dead_code)] // consumed by #287 verify-* dispatch.
#[derive(Debug, Default, Clone)]
pub(crate) struct CpExecOutput {
    /// Whether the remote command returned exit 0.
    pub success: bool,
    /// Captured stdout, UTF-8 lossy.
    pub stdout: String,
    /// Captured stderr, UTF-8 lossy. PSA-denial messages from
    /// `kubectl apply` land here.
    pub stderr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_ssh_opts_includes_proxy_jump_when_set() {
        let target = CpTarget {
            cp_ip: "192.168.122.42".into(),
            kvm_host: Some("geary".into()),
        };
        let opts = target.cp_ssh_opts();
        let argv = opts.to_argv();
        // hbird_ssh emits ProxyJump as `-o ProxyJump=<host>` (verified
        // by the tests/options_pin_argv_shape.rs pin in hbird-ssh).
        let pj_present = argv.windows(2).any(|w| {
            (w[0] == "-o" && w[1].starts_with("ProxyJump="))
                || w[0] == "-J"
                || w[0].starts_with("-J=")
        });
        assert!(pj_present, "expected ProxyJump in argv: {argv:?}");
    }

    #[test]
    fn target_ssh_opts_omits_proxy_jump_when_none() {
        let target = CpTarget {
            cp_ip: "192.168.122.42".into(),
            kvm_host: None,
        };
        let opts = target.cp_ssh_opts();
        let argv = opts.to_argv();
        assert!(
            !argv.iter().any(|s| s.contains("ProxyJump")),
            "expected no ProxyJump: {argv:?}"
        );
    }

    /// #307 — the bash twin's `cp_ssh` uses `ssh -t` which makes
    /// modern sudo emit OSC escape codes; the Rust path defaults to
    /// `BatchMode=yes` (no TTY) which avoids the trap. Verify the
    /// argv carries `BatchMode=yes` rather than `-t`.
    #[test]
    fn target_ssh_opts_uses_batch_mode_not_tty() {
        let target = CpTarget {
            cp_ip: "192.168.122.42".into(),
            kvm_host: Some("geary".into()),
        };
        let argv = target.cp_ssh_opts().to_argv();
        let has_batch = argv
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "BatchMode=yes");
        let has_tty = argv.iter().any(|a| a == "-t" || a == "-tt");
        assert!(has_batch, "expected `-o BatchMode=yes` in argv: {argv:?}");
        assert!(
            !has_tty,
            "argv must NOT request a remote TTY (#307 bash bug): {argv:?}"
        );
    }

    #[test]
    fn cp_kubectl_raw_rejects_shell_metachar() {
        let target = CpTarget {
            cp_ip: "192.168.122.42".into(),
            kvm_host: None,
        };
        for bad in ["get nodes; rm -rf /", "x && y", "a | b", "`whoami`"] {
            let err = cp_kubectl_raw(&target, bad)
                .expect_err(&format!("expected metachar rejection for: {bad}"));
            assert!(
                err.to_string().contains("shell metacharacter"),
                "wrong error for {bad}: {err}"
            );
        }
    }

    #[test]
    fn cp_kubectl_raw_rejects_command_substitution() {
        let target = CpTarget {
            cp_ip: "192.168.122.42".into(),
            kvm_host: None,
        };
        let err = cp_kubectl_raw(&target, "get $(echo nodes)").expect_err("expected $( rejection");
        assert!(err.to_string().contains("$("), "wrong error: {err}");
    }

    #[test]
    fn cp_kubectl_with_stdin_lenient_rejects_metachar() {
        let target = CpTarget {
            cp_ip: "192.168.122.42".into(),
            kvm_host: None,
        };
        let err = cp_kubectl_with_stdin_lenient(&target, "apply -f - ; rm /", b"")
            .expect_err("metachar must be rejected even with stdin");
        assert!(
            err.to_string().contains("shell metacharacter"),
            "wrong error: {err}"
        );
    }
}
