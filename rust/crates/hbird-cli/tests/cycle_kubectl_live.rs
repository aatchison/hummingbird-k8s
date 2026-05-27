//! Phase 3 live-validate (cycle: `kubectl`) — `hbird kubectl get pods -A`
//! parity vs the bash twin
//! `ssh -J $KVM_HOST root@$CP_IP "kubectl ... get pods -A"`.
//!
//! Same shape as `cycle_nodes_live.rs` — env-gated, default-OFF in CI.
//! See that test's module docs for the required env / devcontainer
//! gotcha.

use std::env;

fn env_var(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn env_with_hbird_fallback(key: &str) -> Option<String> {
    env_var(key).or_else(|| env_var(&format!("HBIRD_{key}")))
}

// Round-2 lens L5 HIGH: `#[ignore]` so the test reports IGNORED (not
// PASS) when not opted-in.
#[test]
#[ignore = "live cluster test; opt in with --ignored + HBIRD_LIVE_TEST=1"]
fn live_kubectl_get_pods_returns_running_table() {
    if env::var("HBIRD_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!("HBIRD_LIVE_TEST!=1 — skipping live cluster test");
        return;
    }
    let cp_ip = env_with_hbird_fallback("CP_IP")
        .expect("CP_IP (or HBIRD_CP_IP) required when HBIRD_LIVE_TEST=1");
    let kvm_host = env_with_hbird_fallback("KVM_HOST")
        .expect("KVM_HOST (or HBIRD_KVM_HOST) required when HBIRD_LIVE_TEST=1");
    let opts = hbird_ssh::SshOptions::new(cp_ip.clone())
        .with_user("root")
        .with_proxy_jump(kvm_host.clone());
    let client = hbird_ssh::Client::new(opts);
    let out = client
        .run("kubectl --kubeconfig=/etc/kubernetes/admin.conf get pods -A")
        .expect("ssh-run failed");
    let stdout = out.stdout_lossy();
    eprintln!("--- hbird kubectl get pods -A (live) ---\n{stdout}");
    assert!(
        stdout.starts_with("NAMESPACE") && stdout.contains("READY"),
        "expected kubectl-get-pods header. stdout:\n{stdout}"
    );
    // At least one Running pod somewhere — clusters with no Running
    // pods are broken and should not silently pass this test.
    assert!(
        stdout.lines().any(|l| l.contains(" Running ")),
        "expected at least one Running pod. stdout:\n{stdout}"
    );
}
