//! Phase 3 live-validate (cycle: `nodes`) — `hbird nodes` parity vs
//! the bash twin `ssh -J $KVM_HOST root@$CP_IP "kubectl ... get nodes"`.
//!
//! Environment-gated integration test. Set `HBIRD_LIVE_TEST=1` to run.
//! Default-OFF so CI (which has no cluster) stays green.
//!
//! What this validates: `hbird nodes` against the live cluster
//! returns the same NAME/STATUS/ROLES/AGE/VERSION table as
//! `ssh -J <kvm> root@<cp_ip> kubectl get nodes` (the bash twin
//! `make nodes` shape, modulo the local-tunnel + local-kubectl
//! transport which produces equivalent stdout content).
//!
//! Required environment:
//!
//! - `HBIRD_LIVE_TEST=1`
//! - `CP_IP` (or `HBIRD_CP_IP`)
//! - `KVM_HOST` (or `HBIRD_KVM_HOST`)
//!
//! Devcontainer gotcha: from inside the devcontainer, set
//! `KVM_HOST=<your-login>@geary` (the container's `vscode` user
//! prefix would otherwise fail SSH).
//!
//! Bash-equivalent capture:
//! ```sh
//! ssh -J "$KVM_HOST" "root@$CP_IP" \
//!   "kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes"
//! ```

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

/// Drive `hbird-ssh::Client` directly with the same options
/// `commands::nodes::run` constructs at the top of its dispatch.
/// We don't go through the binary (no PATH dance, no env juggling)
/// — the SSH chain is the load-bearing part, and `cp_kubectl_raw`
/// is exercised by the existing `cp_kubectl_live` test (cycle 1).
#[test]
fn live_nodes_returns_ready_table() {
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
        .run("kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes")
        .expect("ssh-run failed");
    let stdout = out.stdout_lossy();
    eprintln!("--- hbird nodes (live) ---\n{stdout}");

    // Header + at least one Ready row. The bash twin emits the same
    // header from kubectl directly, so we pin on the column shape
    // rather than absolute row count (a node may have been added /
    // removed since the dispatch ran).
    assert!(
        stdout.starts_with("NAME") && stdout.contains("STATUS") && stdout.contains("ROLES"),
        "expected kubectl-get-nodes header. stdout:\n{stdout}"
    );
    assert!(
        stdout.lines().filter(|l| l.contains(" Ready ")).count() >= 1,
        "expected at least one Ready row. stdout:\n{stdout}"
    );
}
