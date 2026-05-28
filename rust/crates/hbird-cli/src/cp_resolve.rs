//! Shared CP-IP resolution via `ssh $KVM_HOST virsh -c qemu:///system
//! domifaddr <cp_name>`.
//!
//! Extracted from [`crate::commands::verify`] so [`crate::commands::kubectl`]
//! (and [`crate::commands::nodes`]) can reuse the same logic — the
//! chicken-egg fix referenced in PR #366 round-2 H1. Before this module
//! existed, three etcd scripts (backup-etcd / restore-etcd /
//! rotate-etcd-encryption-key) called
//! `${CP_IP:-$(hbird kubectl get nodes …)}` to discover CP_IP, but
//! `hbird kubectl` itself hard-failed when CP_IP was unset — making the
//! `$(…)` substitution useless. The fix is to auto-resolve CP_IP from
//! the operator's KVM_HOST inside the Rust CLI, matching the behavior
//! of the deleted bash twin `scripts/kubectl-k8s.sh` (which did the
//! same `ssh $KVM_HOST virsh domifaddr` lookup itself).
//!
//! # Bash twin
//!
//! `scripts/kubectl-k8s.sh` (removed in #353):
//!
//! ```sh
//! VM_IP=$(ssh -4 "$KVM_HOST" "virsh -c qemu:///system domifaddr ${CP_NAME}" \
//!           | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}')
//! ```
//!
//! The Rust shape uses [`hbird_ssh::SshExec`] (issue #345 / PR #357) so
//! unit tests can mock the SSH transport without touching the network.
//!
//! # Cross-references
//!
//! Originally deferred in #289 ("Phase 4 destructive Rust impl") with
//! the placeholder error in `commands/nodes.rs` + `commands/kubectl.rs`
//! that said "virsh-domifaddr resolution is not yet wired in the Rust
//! path — operator must pin CP_IP= for now". This module makes that
//! comment obsolete — auto-resolution works from any host that can
//! ssh-into KVM_HOST.

use anyhow::{Result, bail};

use hbird_ssh::SshExec;

/// Wrap `s` in single quotes for safe inclusion in a remote shell
/// command. Mirrors the bash twin's `'${vm}'` quoting in `resolve_cp_ip`.
/// Embedded single quotes are escaped via the standard
/// `'\'` + `'` + `'` dance.
pub(crate) fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Parse the first IPv4 lease from `virsh domifaddr` output.
/// Mirrors the bash twin's awk pipeline:
/// `awk '/ipv4/{split($4,a,"/"); print a[1]; exit}'`
/// (`scripts/kubectl-k8s.sh` and `lib/build-common.sh:482`).
pub(crate) fn parse_first_domifaddr_ipv4(raw: &str) -> Option<String> {
    for line in raw.lines() {
        if !line.contains("ipv4") {
            continue;
        }
        // 4-field split on whitespace; take field[3] (0-indexed) → CIDR
        // → split on `/` → first segment.
        let cols: Vec<&str> = line.split_whitespace().collect();
        if let Some(cidr) = cols.get(3)
            && let Some(ip) = cidr.split('/').next()
            && !ip.is_empty()
        {
            return Some(ip.to_string());
        }
    }
    None
}

/// Resolve a CP IP by running `virsh -c qemu:///system domifaddr
/// <cp_name>` on `kvm_host` via the supplied [`SshExec`].
///
/// Returns the first IPv4 lease parsed from virsh's output. The
/// `SshExec` parameter is the production [`hbird_ssh::Client`] in the
/// CLI dispatch path; tests pass a canned implementor that returns
/// pre-built [`hbird_ssh::RunOutput`] values.
///
/// # Errors
///
/// - Surfaces the bash twin's "Could not find … IP via ssh" wording
///   (`scripts/kubectl-k8s.sh:64`) when virsh produces no IPv4 lease.
///   Operators grep for "Could not find" / "domifaddr" across both
///   languages so the wording is preserved verbatim where possible.
/// - SSH-transport failures bubble up via `anyhow` with the underlying
///   error chain intact.
pub(crate) fn resolve_cp_ip_via_ssh<S: SshExec>(
    ssh: &S,
    kvm_host: &str,
    cp_name: &str,
) -> Result<String> {
    let cmd = format!(
        "virsh -c qemu:///system domifaddr {}",
        shell_single_quote(cp_name)
    );
    let raw = match ssh.run(&cmd) {
        Ok(out) => out.stdout_lossy(),
        // Match verify.rs: even on non-zero exit, virsh sometimes
        // emits the table on stdout and an unrelated warning on
        // stderr; try the stdout we did get before giving up.
        Err(hbird_ssh::Error::NonZeroExit { stdout, .. }) => stdout,
        Err(e) => bail!(
            "Could not find {cp_name} IP via ssh {kvm_host} \
             'virsh -c qemu:///system domifaddr {cp_name}': {e}"
        ),
    };
    if let Some(ip) = parse_first_domifaddr_ipv4(&raw) {
        Ok(ip)
    } else {
        bail!(
            "Could not find {cp_name} IP via ssh {kvm_host} \
             'virsh -c qemu:///system domifaddr {cp_name}'. \
             Common causes: operator not in libvirt group on {kvm_host}; \
             virsh not installed there; domain not defined. \
             See docs/deploy-cluster.md#running-without-sudo-libvirt-group-operator-305."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbird_ssh::{Result as SshResult, RunOutput};
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    /// Canned [`SshExec`] for tests. Returns the configured stdout from
    /// [`SshExec::run`] regardless of command. Matches the test-fixture
    /// shape used in `cp_kubectl.rs`'s mod tests.
    struct CannedSsh {
        stdout: Vec<u8>,
    }

    impl CannedSsh {
        fn ok_stdout(stdout: &str) -> Self {
            Self {
                stdout: stdout.as_bytes().to_vec(),
            }
        }

        fn ok_empty() -> Self {
            Self { stdout: Vec::new() }
        }
    }

    impl SshExec for CannedSsh {
        fn run(&self, _command: &str) -> SshResult<RunOutput> {
            Ok(RunOutput {
                status: ExitStatus::from_raw(0),
                stdout: self.stdout.clone(),
                stderr: Vec::new(),
            })
        }

        fn run_with_stdin(&self, _command: &str, _stdin: &[u8]) -> SshResult<RunOutput> {
            unimplemented!("not exercised by cp_resolve tests")
        }
    }

    #[test]
    fn resolve_cp_ip_parses_valid_virsh_output() {
        // Synthetic virsh-domifaddr output. Real shape per
        // lib/build-common.sh:482's awk pipeline expectation.
        let raw = " Name       MAC address          Protocol     Address\n\
                   -------------------------------------------------------------------------------\n\
                    vnet0      52:54:00:11:22:33    ipv4         192.168.122.42/24\n";
        let ssh = CannedSsh::ok_stdout(raw);
        let ip =
            resolve_cp_ip_via_ssh(&ssh, "geary", "hbird-cp1").expect("valid virsh output resolves");
        assert_eq!(ip, "192.168.122.42");
    }

    #[test]
    fn resolve_cp_ip_errors_clearly_when_virsh_returns_no_lease() {
        // virsh ran but returned no IPv4 row (VM down, no DHCP lease yet).
        let ssh = CannedSsh::ok_empty();
        let err = resolve_cp_ip_via_ssh(&ssh, "geary", "hbird-cp1")
            .expect_err("empty virsh output should error");
        let msg = err.to_string();
        assert!(
            msg.contains("Could not find"),
            "expected bash twin's 'Could not find' wording: {msg}"
        );
        assert!(
            msg.contains("hbird-cp1"),
            "expected cp_name to surface in the error: {msg}"
        );
        assert!(
            msg.contains("geary"),
            "expected kvm_host to surface in the error: {msg}"
        );
    }

    #[test]
    fn shell_single_quote_no_quotes_inside() {
        assert_eq!(shell_single_quote("hbird-cp1"), "'hbird-cp1'");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quote() {
        // Bash idiom: '\'' (close, escape, open) — `o'brien` → `'o'\''brien'`.
        assert_eq!(shell_single_quote("o'brien"), "'o'\\''brien'");
    }

    #[test]
    fn parse_first_domifaddr_ipv4_picks_first_ipv4_row() {
        let raw = " Name       MAC address          Protocol     Address\n\
                   -------------------------------------------------------------------------------\n\
                    vnet0      52:54:00:11:22:33    ipv4         192.168.122.42/24\n";
        assert_eq!(
            parse_first_domifaddr_ipv4(raw),
            Some("192.168.122.42".to_string())
        );
    }

    #[test]
    fn parse_first_domifaddr_ipv4_returns_none_on_empty() {
        assert_eq!(parse_first_domifaddr_ipv4(""), None);
    }

    #[test]
    fn parse_first_domifaddr_ipv4_returns_none_when_no_ipv4_row() {
        let raw = " vnet0      52:54:00:11:22:33    ipv6         fe80::1/64\n";
        assert_eq!(parse_first_domifaddr_ipv4(raw), None);
    }
}
