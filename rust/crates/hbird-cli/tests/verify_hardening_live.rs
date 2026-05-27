//! Phase 2 live-validate — `hbird verify hardening` parity check.
//!
//! Environment-gated integration test. Set `HBIRD_LIVE_TEST=1` to run.
//! Default-OFF so CI (which has no cluster) stays green.
//!
//! Validates the four checks of `verify-hardening.sh` against the live
//! geary cluster:
//!
//! - PSA restricted rejects a privileged pod (`violates PodSecurity`
//!   marker on stderr from `kubectl apply -f -`).
//! - apiserver audit log present + non-empty on the CP host.
//! - kubelet running with `--protect-kernel-defaults=true`.
//! - kubelet running with `--rotate-certificates=true`.
//!
//! All four must PASS for the test to pass. Mirrors the bash twin's
//! `[verify-hardening] all checks PASSED` exit-0 condition.
//!
//! Required environment: same as `verify_encryption_live.rs` —
//! `HBIRD_LIVE_TEST=1`, `CP_IP`, `KVM_HOST`.

use std::env;
use std::process::Command;

fn env_var(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn env_with_hbird_fallback(key: &str) -> Option<String> {
    env_var(key).or_else(|| env_var(&format!("HBIRD_{key}")))
}

fn hbird_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

// Round-2 lens L5 MEDIUM: `#[ignore]` so the test reports as IGNORED
// (not PASS) when not opted-in. Operator opts in with `--ignored` +
// `HBIRD_LIVE_TEST=1` env.
#[test]
#[ignore = "live cluster test; opt in with --ignored + HBIRD_LIVE_TEST=1"]
fn live_verify_hardening_all_checks_pass() {
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
            "hardening",
            "--cp-ip",
            &cp_ip,
            "--kvm-host",
            &kvm_host,
        ])
        .output()
        .expect("failed to spawn hbird");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    eprintln!("--- hbird verify hardening stdout ---\n{stdout}");
    eprintln!("--- hbird verify hardening stderr ---\n{stderr}");

    let combined = format!("{stdout}\n{stderr}");
    // Each bash-twin PASS marker — operator grep anchors.
    for marker in [
        "PASS: privileged pod rejected by PodSecurity",
        "PASS: audit log present",
        "PASS: kubelet has --protect-kernel-defaults=true",
        "PASS: kubelet has --rotate-certificates=true",
        "all checks PASSED",
    ] {
        assert!(
            combined.contains(marker),
            "expected bash-twin marker `{marker}` in output. combined:\n{combined}"
        );
    }
    assert!(
        out.status.success(),
        "verify hardening exited non-zero. combined:\n{combined}"
    );
}
