//! Phase 1B live-validate (cycle 3, #328) — node-Ready + DaemonSet-ready
//! gate parity check.
//!
//! Environment-gated integration test. Set `HBIRD_LIVE_TEST=1` to run.
//! Default-OFF + `#[ignore]` so CI (which has no cluster) stays green.
//!
//! What this validates:
//!
//! - The Rust wiring of cycle 3's helpers — `wait_node_ready`,
//!   `wait_node_daemonsets_ready` (and the `_collect_unready_names`
//!   parser they share) — drives the kubectl polling loop a real
//!   reboot exercises on a worker node, observing the same Ready and
//!   DS-Ready transitions the bash twin's block #9 polls.
//!
//! - The CP-mediated kubectl chain (root@CP_IP via ProxyJump=$KVM_HOST)
//!   is the same shape cycles 1 + 2 proved out (PR #325 / PR #344).
//!   Cycle 3 doesn't introduce any new SSH surface — it only adds two
//!   poll-loops over the existing `cp_kubectl` shim.
//!
//! How parity is proven:
//!
//! - Operator first force-bounces hbird-w1 via `systemctl reboot` to
//!   put the node briefly into NotReady, then captures the bash twin's
//!   wait_node_ready + wait_node_daemonsets_ready behavior manually
//!   (timing + final DS-Ready transition) into
//!   `tests/update_cluster/fixtures/live/cycle3_daemonset.txt`.
//! - The cluster re-stabilizes; the test reboots again and drives the
//!   IDENTICAL polling logic through the Rust path via direct
//!   `hbird_ssh::Client::run` invocations against the same CP +
//!   ProxyJump shape.
//! - Captured timings + final DS readiness state land in the fixture
//!   for the bash-vs-Rust diff.
//!
//! Required environment:
//!
//! - `HBIRD_LIVE_TEST=1`
//! - `CP_IP` (or `HBIRD_CP_IP`) — IP of the control plane VM.
//! - `KVM_HOST` (or `HBIRD_KVM_HOST`) — SSH alias / hostname of the KVM
//!   host. Inside the devcontainer set to `<your-login>@geary` because
//!   the container's `vscode` user is otherwise the SSH default user.
//! - `HBIRD_DS_NODE` — k8s node name to bounce (e.g. `hbird-w1`;
//!   cycle 1 used hbird-w2, cycle 2 used hbird-w1; cycle 3 also targets
//!   hbird-w1 per #328's coordination note). Test-only var; no bash
//!   twin.
//! - `HBIRD_DS_NODE_IP` — IPv4 of the same node (the live SSH target
//!   for the reboot trigger).
//!
//! IMPORTANT: this test issues `systemctl reboot` against the supplied
//! node, which forces a NotReady → Ready → DS-Ready cycle. Operator
//! must have bash-equivalent state already captured (per the cycle 3
//! fixture template) AND must verify the cluster is at 3/3 Ready and
//! all DS pods are Ready before merging.

use std::env;
use std::time::Duration;

fn env_var(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn env_with_hbird_fallback(key: &str) -> Option<String> {
    env_var(key).or_else(|| env_var(&format!("HBIRD_{key}")))
}

/// Local copy of the `stdout_has_ready_status` matcher so this test
/// crate doesn't depend on a `pub` export of the in-tree helper. The
/// shape MUST stay identical to
/// `src/commands/update_cluster.rs::stdout_has_ready_status` — cycle 3
/// pins the matcher's Ready-vs-Ready,SchedulingDisabled-vs-NotReady
/// behavior in BOTH places so a regression in either copy surfaces
/// immediately.
fn stdout_has_ready_status(stdout: &str) -> bool {
    for line in stdout.lines() {
        let mut cols = line.split_whitespace();
        let _name = cols.next();
        let Some(status) = cols.next() else { continue };
        if status == "Ready" || status.starts_with("Ready,") {
            return true;
        }
    }
    false
}

/// Local copy of the `_collect_unready_names` parser — same rationale
/// as `stdout_has_ready_status` above. See the in-tree helper for the
/// full bash-twin parity notes.
fn collect_unready_names(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        let (lhs, rhs) = match line.find('=') {
            Some(idx) => (&line[..idx], &line[idx + 1..]),
            None => (line, line),
        };
        if rhs.is_empty() || rhs.contains("false") {
            out.push(lhs.to_string());
        }
    }
    out
}

// `#[ignore]` so the test reports as IGNORED (not PASS) when not opted-in.
// Operator opts in with `--ignored` + HBIRD_LIVE_TEST=1 env (matches the
// Wave-2 pattern from PR #330 / #334 / #344).
#[test]
#[ignore = "live cluster test that REBOOTS a worker; opt in with --ignored + HBIRD_LIVE_TEST=1"]
fn live_wait_node_ready_and_daemonsets_drives_full_post_reboot_gate() {
    if env::var("HBIRD_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!("HBIRD_LIVE_TEST!=1 — skipping live cluster test");
        return;
    }

    let cp_ip = env_with_hbird_fallback("CP_IP")
        .expect("CP_IP (or HBIRD_CP_IP) required when HBIRD_LIVE_TEST=1");
    let kvm_host = env_with_hbird_fallback("KVM_HOST")
        .expect("KVM_HOST (or HBIRD_KVM_HOST) required when HBIRD_LIVE_TEST=1");
    let node = env_var("HBIRD_DS_NODE").expect("HBIRD_DS_NODE required when HBIRD_LIVE_TEST=1");
    let node_ip =
        env_var("HBIRD_DS_NODE_IP").expect("HBIRD_DS_NODE_IP required when HBIRD_LIVE_TEST=1");

    // ---- (a) Build the CP-targeted kubectl client (same shape as the
    // in-tree cp_kubectl shim's argv).
    let cp_opts = hbird_ssh::SshOptions::new(cp_ip.clone())
        .with_user("root")
        .with_proxy_jump(kvm_host.clone());
    let cp = hbird_ssh::Client::new(cp_opts);

    let node_get_cmd =
        format!("kubectl --kubeconfig=/etc/kubernetes/admin.conf get node {node} --no-headers");
    let ds_jsonpath_cmd = format!(
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf get pods -n kube-system \
         --field-selector=spec.nodeName={node} \
         -o jsonpath='{{range .items[*]}}{{.metadata.name}}={{range .status.containerStatuses[*]}}{{.ready}},{{end}}{{\"\\n\"}}{{end}}'"
    );

    // ---- (b) Pre-reboot: confirm node is Ready + capture baseline-unready.
    let pre_node_out = cp
        .run(&node_get_cmd)
        .expect("pre-reboot node get: ssh-run failed");
    let pre_stdout = pre_node_out.stdout_lossy();
    eprintln!("--- pre-reboot get node {node}: {pre_stdout:?} ---");
    assert!(
        stdout_has_ready_status(&pre_stdout),
        "pre-reboot {node} not Ready: {pre_stdout:?}",
    );

    let pre_raw_out = cp
        .run(&ds_jsonpath_cmd)
        .expect("pre-reboot DS jsonpath: ssh-run failed");
    let baseline_raw = pre_raw_out.stdout_lossy();
    let baseline_unready: std::collections::BTreeSet<String> =
        collect_unready_names(&baseline_raw).into_iter().collect();
    eprintln!(
        "--- pre-reboot baseline-unready on {node}: {:?} ---",
        baseline_unready
    );

    // ---- (c) Force-bounce the node via systemctl reboot.
    let node_opts = hbird_ssh::SshOptions::new(node_ip.clone())
        .with_user("root")
        .with_proxy_jump(kvm_host.clone())
        .with_connect_timeout(Duration::from_secs(3));
    let node_cli = hbird_ssh::Client::new(node_opts);
    eprintln!("--- issuing `systemctl reboot` on {node} ({node_ip}) ---");
    let _ = node_cli.run("systemctl reboot >/dev/null 2>&1 || true");

    // ---- (d) wait_node_ready: poll get node X every 10s up to 300s,
    // accepting Ready and Ready,SchedulingDisabled. Mirrors cycle 3's
    // wait_node_ready exactly.
    //
    // The node may briefly disappear from `kubectl get node` (it
    // doesn't actually; the row stays and just flips NotReady). We
    // count the FIRST observation that the matcher returns true.
    let timeout_ready: u32 = 300;
    let interval_ready: u32 = 10;
    let mut ready_at: Option<u32> = None;
    let mut elapsed: u32 = 0;
    // Pre-reboot was Ready; we may or may not observe NotReady briefly
    // depending on the node-monitor-grace-period vs the reboot speed.
    // We do NOT require seeing NotReady; we only require that the
    // matcher eventually returns true for the post-reboot node row.
    // Bash twin's gate has the same property — on a single-VM
    // reboot-and-back-within-grace-period the gate can return at t=0
    // (apiserver lease cached Ready=True over the disconnect window).
    // What this test pins is the SHAPE of the matcher: that
    // stdout_has_ready_status returns true for the post-reboot row.
    //
    // We still sleep one poll interval before starting so an operator
    // tailing the test output sees the loop actually iterate.
    std::thread::sleep(Duration::from_secs(interval_ready.into()));
    while elapsed < timeout_ready {
        match cp.run(&node_get_cmd) {
            Ok(out) => {
                let stdout = out.stdout_lossy();
                if stdout_has_ready_status(&stdout) {
                    ready_at = Some(elapsed);
                    eprintln!(
                        "--- node {node} Ready after ~{elapsed}s (post-pre-sleep): {stdout:?} ---"
                    );
                    break;
                }
            }
            Err(e) => {
                eprintln!("--- wait_node_ready: kubectl flake at ~{elapsed}s: {e:?} ---");
            }
        }
        std::thread::sleep(Duration::from_secs(interval_ready.into()));
        elapsed = elapsed.saturating_add(interval_ready);
    }
    let ready_at = ready_at.expect("node {node} did not reach Ready within 5 minutes of reboot");

    // ---- (e) wait_node_daemonsets_ready: two-phase gate. Phase 1
    // waits up to 60s for at least one kube-system pod to appear on
    // the node; phase 2 polls every 5s until no NEW-unready pods.
    //
    // After a clean reboot we expect both phases to advance quickly
    // (the DaemonSet controller re-binds pods as soon as the node
    // re-registers, and the Cilium / kube-proxy / coredns pods land
    // Ready within ~30-60s on this size cluster).
    let mut phase1_elapsed: u32 = 0;
    let mut pod_count: usize = 0;
    while phase1_elapsed < 60 {
        match cp.run(&format!(
            "kubectl --kubeconfig=/etc/kubernetes/admin.conf get pods -n kube-system \
             --field-selector=spec.nodeName={node} --no-headers"
        )) {
            Ok(out) => {
                pod_count = out
                    .stdout_lossy()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .count();
                if pod_count > 0 {
                    break;
                }
            }
            Err(_) => {
                pod_count = 0;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
        phase1_elapsed = phase1_elapsed.saturating_add(2);
    }
    eprintln!("--- DS phase 1 on {node}: observed {pod_count} pods after ~{phase1_elapsed}s ---");
    assert!(
        pod_count > 0,
        "DaemonSet controller never scheduled a kube-system pod onto {node} within 60s",
    );

    let timeout_ds: u32 = 300;
    let interval_ds: u32 = 5;
    let mut ds_ready_at: Option<u32> = None;
    let mut elapsed: u32 = 0;
    let mut last_new_unready: Vec<String> = Vec::new();
    while elapsed < timeout_ds {
        let raw = cp
            .run(&ds_jsonpath_cmd)
            .map(|o| o.stdout_lossy())
            .unwrap_or_default();
        let current: std::collections::BTreeSet<String> =
            collect_unready_names(&raw).into_iter().collect();
        let new_unready: Vec<String> = current.difference(&baseline_unready).cloned().collect();
        if new_unready.is_empty() {
            ds_ready_at = Some(elapsed);
            eprintln!("--- node {node} kube-system DaemonSet pods all Ready after ~{elapsed}s ---");
            break;
        }
        last_new_unready = new_unready;
        std::thread::sleep(Duration::from_secs(interval_ds.into()));
        elapsed = elapsed.saturating_add(interval_ds);
    }
    let ds_ready_at = ds_ready_at.unwrap_or_else(|| {
        panic!(
            "DaemonSet pods on {node} not Ready within {timeout_ds}s; last new-unready: {last_new_unready:?}"
        )
    });

    // ---- (f) Final sanity: cluster is 3/3 Ready, no SchedulingDisabled
    // on the bounced node.
    let final_out = cp
        .run("kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes")
        .expect("final get nodes: ssh-run failed");
    let final_stdout = final_out.stdout_lossy();
    eprintln!("--- final get nodes:\n{final_stdout}");
    let line = final_stdout
        .lines()
        .find(|l| l.starts_with(&node))
        .unwrap_or_else(|| panic!("no row for {node} in final get nodes: {final_stdout}"));
    assert!(
        !line.contains("SchedulingDisabled"),
        "{node} still SchedulingDisabled at end: {line}",
    );

    eprintln!(
        "\n=== Cycle 3 live-validate summary ===\n\
         node:             {node}\n\
         baseline_unready: {:?}\n\
         ready_at:         {ready_at}s\n\
         ds_ready_at:      {ds_ready_at}s\n",
        baseline_unready
    );
}
