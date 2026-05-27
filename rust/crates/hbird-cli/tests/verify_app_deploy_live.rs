//! Phase 2 live-validate — `hbird verify app-deploy` parity check.
//!
//! Environment-gated integration test. Set `HBIRD_LIVE_TEST=1` to run.
//! Default-OFF so CI (which has no cluster) stays green.
//!
//! Validates the end-to-end nginx-under-PSA-restricted smoke against
//! the live geary cluster:
//!
//! 1. Creates a `smoketest-<epoch>` namespace.
//! 2. Applies a PSA-restricted-compliant nginx Deployment + Service.
//! 3. Waits for deployment/nginx to become Available.
//! 4. Runs a busybox probe pod that `wget`s the Service, asserts the
//!    response contains `Welcome to nginx`.
//! 5. Cleans up the namespace on EXIT.
//!
//! IMPORTANT: this test MUTATES the cluster (briefly). Run sequentially,
//! not in parallel, with any other live cluster work.
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

#[test]
fn live_verify_app_deploy_pass() {
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
            "app-deploy",
            "--cp-ip",
            &cp_ip,
            "--kvm-host",
            &kvm_host,
        ])
        .output()
        .expect("failed to spawn hbird");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    eprintln!("--- hbird verify app-deploy stdout ---\n{stdout}");
    eprintln!("--- hbird verify app-deploy stderr ---\n{stderr}");

    let combined = format!("{stdout}\n{stderr}");
    // Bash twin's grep anchors (verify-app-deploy.sh:185 + :195).
    for marker in [
        "PASS: nginx returned the welcome page over ClusterIP",
        "verify-app-deploy: PASS",
    ] {
        assert!(
            combined.contains(marker),
            "expected bash-twin marker `{marker}` in output. combined:\n{combined}"
        );
    }
    assert!(
        out.status.success(),
        "verify app-deploy exited non-zero. combined:\n{combined}"
    );
}
