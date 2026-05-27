//! Phase 1B live-validate (cycle 1) — `cp_kubectl` shim parity check.
//!
//! Environment-gated integration test. Set `HBIRD_LIVE_TEST=1` to run.
//! Default-OFF so CI (which has no cluster) stays green.
//!
//! What this validates:
//!
//! - The Rust `cp_kubectl` shim (newly wired in #322 cycle 1) connects
//!   via `hbird_ssh::Client` to `root@$CP_IP` with `ProxyJump=$KVM_HOST`,
//!   runs `kubectl --kubeconfig=/etc/kubernetes/admin.conf <cmd>`, and
//!   emits stdout + stderr to the operator-visible log.
//! - Specifically: `get nodes`, `drain <NODE>`, `uncordon <NODE>` —
//!   the three kubectl invocations bash twin's `cp_kubectl` makes for
//!   the drain+uncordon block (block #5 + part of update_worker).
//!
//! How parity is proven:
//!
//! - Test runs the Rust shim against the live cluster (3 kubectl calls).
//! - Operator side captures the bash-equivalent commands manually via
//!   `ssh -J $KVM_HOST root@$CP_IP "kubectl ... <cmd>"` and diffs.
//! - This test does NOT itself run bash — it documents the bash shape
//!   in module docs so the diff is reproducible.
//!
//! Required environment:
//!
//! - `HBIRD_LIVE_TEST=1`
//! - `CP_IP` (or `HBIRD_CP_IP`) — IP of the control plane VM
//!   (e.g. `192.168.122.212`). Bash twin uses `CP_IP` verbatim
//!   (`scripts/deploy-cluster.sh:526`); we honor that first so operators
//!   with the env already set don't need a Rust-specific copy. Round-2
//!   lens L9 MEDIUM.
//! - `KVM_HOST` (or `HBIRD_KVM_HOST`) — SSH alias / hostname of the KVM
//!   host (e.g. `geary`). Bash twin uses `KVM_HOST`
//!   (`scripts/update-cluster.sh:129`); same fallback shape.
//! - `HBIRD_DRAIN_NODE` — k8s node name to drain+uncordon
//!   (e.g. `hbird-w2`). Test-only var; no bash twin.
//!
//! Devcontainer gotcha: the container effective user is `vscode`;
//! without an explicit `user@` prefix, `ssh -J` defaults to
//! `vscode@geary` and fails with `Permission denied`. From inside the
//! devcontainer, set `KVM_HOST=<your-login>@geary` (e.g.
//! `KVM_HOST=aatchison@geary`).
//!
//! Bash-equivalent for the drain assertion:
//! ```sh
//! ssh -J "$KVM_HOST" "root@$CP_IP" \
//!   "kubectl --kubeconfig=/etc/kubernetes/admin.conf drain $HBIRD_DRAIN_NODE \
//!    --ignore-daemonsets --delete-emptydir-data --timeout=5m"
//! ```

use std::env;

/// Read a required env var; skip the test (return None) if unset.
fn env_var(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Read `key` (bash-twin var) first, fall back to `HBIRD_<key>` for the
/// Rust-specific override. Round-2 lens L9 MEDIUM — preserves operator
/// muscle memory for operators who already export `CP_IP` / `KVM_HOST`.
fn env_with_hbird_fallback(key: &str) -> Option<String> {
    env_var(key).or_else(|| env_var(&format!("HBIRD_{key}")))
}

/// RAII guard: on Drop, runs `kubectl uncordon` against the target node
/// via the same SSH chain the test used. Ensures a panicked assertion
/// (e.g. drain returned unexpected output) doesn't leave the cluster
/// with a cordoned node. Round-2 lens L3 HIGH.
struct UncordonGuard<'a> {
    client: &'a hbird_ssh::Client,
    node: String,
    armed: bool,
}

impl<'a> UncordonGuard<'a> {
    fn new(client: &'a hbird_ssh::Client, node: String) -> Self {
        Self {
            client,
            node,
            armed: true,
        }
    }

    /// Disarm after a successful explicit uncordon. Suppresses the Drop
    /// hook so the happy path doesn't run uncordon twice.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for UncordonGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        eprintln!(
            "--- UncordonGuard firing on panic: re-uncordoning {} ---",
            self.node
        );
        let cmd = format!(
            "kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon {}",
            self.node
        );
        match self.client.run(&cmd) {
            Ok(out) => eprintln!("[recovery] uncordon stdout: {}", out.stdout_lossy()),
            Err(e) => eprintln!("[recovery] uncordon FAILED: {e:#}"),
        }
    }
}

#[test]
fn live_cp_kubectl_get_nodes_drain_uncordon_smoke() {
    if env::var("HBIRD_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!("HBIRD_LIVE_TEST!=1 — skipping live cluster test");
        return;
    }

    let cp_ip = env_with_hbird_fallback("CP_IP")
        .expect("CP_IP (or HBIRD_CP_IP) required when HBIRD_LIVE_TEST=1");
    let kvm_host = env_with_hbird_fallback("KVM_HOST")
        .expect("KVM_HOST (or HBIRD_KVM_HOST) required when HBIRD_LIVE_TEST=1");
    let node =
        env_var("HBIRD_DRAIN_NODE").expect("HBIRD_DRAIN_NODE required when HBIRD_LIVE_TEST=1");

    // Test bypasses the in-tree `cp_kubectl` shim (private to its module
    // today) and re-constructs the SshOptions chain inline using the
    // same shape `cp_ssh_opts` builds. Cycle 1 proves the SSH+kubectl
    // pattern works end-to-end; cycle 2 should expose `cp_kubectl` via
    // a `pub(crate)` re-export so the test exercises the shim directly
    // (round-2 lens L5 MEDIUM).
    let opts = hbird_ssh::SshOptions::new(cp_ip.clone())
        .with_user("root")
        .with_proxy_jump(kvm_host.clone());
    let client = hbird_ssh::Client::new(opts);

    // (a) get nodes — readonly sanity check. Tighter assertion than
    // `contains("Ready")` (which matches `NotReady` too) per CodeRabbit
    // comment: check that the *target node's* line ends in `Ready` (not
    // `NotReady`, `SchedulingDisabled`, or `Unknown`).
    let out = client
        .run("kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes")
        .expect("cp_kubectl get nodes: ssh-run failed");
    let stdout = out.stdout_lossy();
    let node_line = stdout
        .lines()
        .find(|l| l.starts_with(&node))
        .unwrap_or_else(|| {
            panic!("expected `{node}` row in `get nodes` output. stdout:\n{stdout}")
        });
    assert!(
        node_line.contains(" Ready ") || node_line.contains("\tReady\t"),
        "node {node} not in Ready state. row: {node_line:?}"
    );
    eprintln!("--- cp_kubectl get nodes ---\n{stdout}");

    // (b) drain — block #5 of bash twin's update_worker. ARM the
    // recovery guard immediately after drain succeeds, BEFORE the
    // assertion can panic, so even an unexpected output shape leaves
    // the cluster recoverable.
    let drain_cmd = format!(
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf drain {node} \
         --ignore-daemonsets --delete-emptydir-data --timeout=5m"
    );
    let out = client
        .run(&drain_cmd)
        .expect("cp_kubectl drain: ssh-run failed");
    let guard = UncordonGuard::new(&client, node.clone());

    let stdout = out.stdout_lossy();
    let stderr = out.stderr_lossy();
    let combined = format!("{stdout}\n{stderr}");
    // Two markers: `cordoned` (drain set unschedulable) AND `drained`
    // (drain successfully evicted pods). The `drained` marker is the
    // canonical success signal; CodeRabbit + L5 MEDIUM noted that
    // checking only `cordoned` would pass for a drain that hung on PDB.
    assert!(
        combined.contains("cordoned"),
        "expected `cordoned` marker in drain output. combined:\n{combined}"
    );
    assert!(
        combined.contains("drained"),
        "expected `drained` marker in drain output (drain may have \
         hung on PDB). combined:\n{combined}"
    );
    eprintln!("--- cp_kubectl drain {node} ---\nstdout:\n{stdout}\nstderr:\n{stderr}");

    // (c) uncordon — restores node so we leave the cluster in clean
    // state. Disarm the guard on success so the happy path doesn't
    // double-uncordon.
    let uncordon_cmd = format!("kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon {node}");
    let out = client
        .run(&uncordon_cmd)
        .expect("cp_kubectl uncordon: ssh-run failed");
    let stdout = out.stdout_lossy();
    assert!(
        stdout.contains("uncordoned") || stdout.contains("already uncordoned"),
        "expected `uncordoned` in uncordon output. stdout:\n{stdout}"
    );
    eprintln!("--- cp_kubectl uncordon {node} ---\n{stdout}");
    guard.disarm();

    // (d) Final sanity: target node specifically is back to schedulable.
    // Substring `SchedulingDisabled` against the whole output would
    // pass even if a DIFFERENT node is cordoned; check the row.
    let out = client
        .run("kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes")
        .expect("post-uncordon get nodes failed");
    let stdout = out.stdout_lossy();
    let node_line = stdout
        .lines()
        .find(|l| l.starts_with(&node))
        .unwrap_or_else(|| {
            panic!("expected `{node}` row in post-uncordon output. stdout:\n{stdout}")
        });
    assert!(
        !node_line.contains("SchedulingDisabled"),
        "node {node} still cordoned after uncordon. row: {node_line:?}"
    );
    eprintln!("--- cp_kubectl get nodes (post-uncordon) ---\n{stdout}");
}
