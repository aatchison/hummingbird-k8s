//! Phase 2 live-validate — `hbird verify encryption` parity check.
//!
//! Environment-gated integration test. Set `HBIRD_LIVE_TEST=1` to run.
//! Default-OFF so CI (which has no cluster) stays green.
//!
//! Validates that `hbird verify encryption` against the live geary
//! cluster exits 0 with the bash-twin's `OK: secret in etcd is
//! encrypted` marker on stderr. The Rust path SSHes into root@CP_IP
//! and invokes the baked-in `/usr/libexec/verify-encryption.sh`
//! verbatim — same code path as the bash twin's remote mode.
//!
//! Required environment:
//!
//! - `HBIRD_LIVE_TEST=1`
//! - `CP_IP` (or `HBIRD_CP_IP`) — IP of the control plane VM
//!   (e.g. `192.168.122.212`).
//! - `KVM_HOST` (or `HBIRD_KVM_HOST`) — SSH alias / hostname of the
//!   KVM host (e.g. `geary` or `aatchison@geary` from inside the
//!   devcontainer per the #320 gotcha).
//!
//! Bash-equivalent for the diff:
//! ```sh
//! ssh -J "$KVM_HOST" "root@$CP_IP" /usr/libexec/verify-encryption.sh
//! ```
//! Both sides should emit `[verify-encryption] OK: secret in etcd is
//! encrypted (prefix=k8s:enc:aesgcm:)` and exit 0.

use std::env;
use std::process::Command;

fn env_var(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Read `key` first, fall back to `HBIRD_<key>` for the Rust-specific
/// override. Mirrors the cycle1 test's helper.
fn env_with_hbird_fallback(key: &str) -> Option<String> {
    env_var(key).or_else(|| env_var(&format!("HBIRD_{key}")))
}

/// Locate the hbird binary built by the workspace. Mirrors the
/// `cli_smoke.rs::hbird_bin` helper.
fn hbird_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

#[test]
fn live_verify_encryption_pass() {
    if env::var("HBIRD_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!("HBIRD_LIVE_TEST!=1 — skipping live cluster test");
        return;
    }

    let cp_ip = env_with_hbird_fallback("CP_IP")
        .expect("CP_IP (or HBIRD_CP_IP) required when HBIRD_LIVE_TEST=1");
    let kvm_host = env_with_hbird_fallback("KVM_HOST")
        .expect("KVM_HOST (or HBIRD_KVM_HOST) required when HBIRD_LIVE_TEST=1");

    let out = Command::new(hbird_bin())
        .args([
            "verify",
            "encryption",
            "--cp-ip",
            &cp_ip,
            "--kvm-host",
            &kvm_host,
        ])
        .output()
        .expect("failed to spawn hbird");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    eprintln!("--- hbird verify encryption stdout ---\n{stdout}");
    eprintln!("--- hbird verify encryption stderr ---\n{stderr}");

    // Bash twin's marker string — operator grep anchor (verbatim).
    assert!(
        stderr.contains("OK: secret in etcd is encrypted")
            || stdout.contains("OK: secret in etcd is encrypted"),
        "expected bash twin's `OK: secret in etcd is encrypted` marker. \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "verify encryption exited non-zero. stderr:\n{stderr}"
    );
}
