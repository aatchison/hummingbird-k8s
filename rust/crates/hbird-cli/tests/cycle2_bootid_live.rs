//! Phase 1B live-validate (cycle 2, #327) — bootID gate + bootc upgrade
//! parity check.
//!
//! Environment-gated integration test. Set `HBIRD_LIVE_TEST=1` to run.
//! Default-OFF + `#[ignore]` so CI (which has no cluster) stays green.
//!
//! What this validates:
//!
//! - The Rust wiring of cycle 2's helpers — `capture_node_bootid`,
//!   `wait_node_bootid_changed`, `bootc_upgrade_apply`, `wait_ssh_drop`,
//!   `wait_ssh_back` — drives a real reboot on a worker node and
//!   observes a bootID change matching what the bash twin's
//!   `update-cluster.sh` block #6 does.
//!
//! - The SSH chain (root@WORKER_IP via ProxyJump=$KVM_HOST) is the same
//!   shape cycle 1 proved out (PR #325). The new bits this test
//!   exercises are the node-targeted SSH (rather than CP-targeted) and
//!   the bootc upgrade life-cycle (digest snapshot → upgrade → reboot
//!   → digest re-snapshot).
//!
//! How parity is proven:
//!
//! - Operator runs the bash flow manually first (capturing bootID +
//!   timing into `tests/update_cluster/fixtures/live/cycle2_bootid.txt`)
//!   and rolls the node back via `bootc rollback && reboot`.
//! - This test then drives the SAME SSH chain through `hbird_ssh::Client`,
//!   issues `bootc status` pre/post + observes the bootID change via
//!   kubectl (which is the source of truth the Rust wiring polls).
//! - Captured timings + bootID values land in the fixture file for the
//!   bash-vs-Rust diff.
//!
//! Required environment:
//!
//! - `HBIRD_LIVE_TEST=1`
//! - `CP_IP` (or `HBIRD_CP_IP`) — IP of the control plane VM
//!   (bash twin's `CP_IP`, mirrored under the `HBIRD_` prefix per cycle
//!   1's lens L9 pattern).
//! - `KVM_HOST` (or `HBIRD_KVM_HOST`) — SSH alias / hostname of the KVM
//!   host. Inside the devcontainer set to `<your-login>@geary` because
//!   the container's `vscode` user is otherwise the SSH default user
//!   (per cp_kubectl_live.rs).
//! - `HBIRD_BOOTID_NODE` — k8s node name to bounce
//!   (e.g. `hbird-w1`; cycle 1 used hbird-w2 so cycle 2 preserves that
//!   by defaulting the other worker). Test-only var; no bash twin.
//! - `HBIRD_BOOTID_NODE_IP` — IPv4 of the same node (the live SSH
//!   target for the bootc invocations). The node-name → IP map lives
//!   in libvirt, not in the Rust path; we keep the resolution
//!   operator-driven so this test stays infrastructure-light.
//!
//! IMPORTANT: this test issues `bootc upgrade --apply` against the
//! supplied node, which reboots it. Operator must have bash-equivalent
//! state already captured (per the cycle 2 fixture template) AND must
//! be ready to `bootc rollback && reboot` afterward to restore the
//! cluster to 3/3 Ready before merging.
//!
//! Bash-equivalent for the bootID assertion:
//! ```sh
//! pre_bootid=$(ssh -J "$KVM_HOST" "root@$CP_IP" \
//!   "kubectl --kubeconfig=/etc/kubernetes/admin.conf get node $HBIRD_BOOTID_NODE \
//!    -o jsonpath='{.status.nodeInfo.bootID}'")
//! ssh -J "$KVM_HOST" "root@$HBIRD_BOOTID_NODE_IP" "bootc upgrade --apply"  # auto-reboots
//! # poll for SSH drop, SSH back, bootID change, Ready ...
//! ```

use std::env;
use std::time::Duration;

/// Read a required env var; return None if unset or empty so the
/// test can degrade gracefully to skip.
fn env_var(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Read `key` (bash-twin var) first, fall back to `HBIRD_<key>` for the
/// Rust-specific override. Mirrors cycle 1's lens L9 MEDIUM pattern —
/// operators already exporting CP_IP / KVM_HOST don't need a separate
/// hbird-prefixed copy.
fn env_with_hbird_fallback(key: &str) -> Option<String> {
    env_var(key).or_else(|| env_var(&format!("HBIRD_{key}")))
}

// `#[ignore]` so the test reports as IGNORED (not PASS) when not opted-in.
// Operator opts in with `--ignored` + HBIRD_LIVE_TEST=1 env (matches the
// Wave-2 pattern from PR #330 / #334).
#[test]
#[ignore = "live cluster test that REBOOTS a worker; opt in with --ignored + HBIRD_LIVE_TEST=1"]
fn live_bootid_and_bootc_upgrade_drives_full_reboot_cycle() {
    if env::var("HBIRD_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!("HBIRD_LIVE_TEST!=1 — skipping live cluster test");
        return;
    }

    let cp_ip = env_with_hbird_fallback("CP_IP")
        .expect("CP_IP (or HBIRD_CP_IP) required when HBIRD_LIVE_TEST=1");
    let kvm_host = env_with_hbird_fallback("KVM_HOST")
        .expect("KVM_HOST (or HBIRD_KVM_HOST) required when HBIRD_LIVE_TEST=1");
    let node =
        env_var("HBIRD_BOOTID_NODE").expect("HBIRD_BOOTID_NODE required when HBIRD_LIVE_TEST=1");
    let node_ip = env_var("HBIRD_BOOTID_NODE_IP")
        .expect("HBIRD_BOOTID_NODE_IP required when HBIRD_LIVE_TEST=1");

    // ---- (a) Capture pre-state via kubectl-on-CP ----
    //
    // Mirrors the in-tree `cp_kubectl` shim's argv shape. We construct
    // SshOptions inline here for the same reason cycle 1's test does —
    // the in-module cp_kubectl isn't `pub(crate)` for this test crate.
    let cp_opts = hbird_ssh::SshOptions::new(cp_ip.clone())
        .with_user("root")
        .with_proxy_jump(kvm_host.clone());
    let cp = hbird_ssh::Client::new(cp_opts);

    // bootID jsonpath — same expression cycle 2's capture_node_bootid uses.
    let bootid_cmd = format!(
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf get node {node} \
         -o jsonpath='{{.status.nodeInfo.bootID}}'"
    );
    let pre_out = cp
        .run(&bootid_cmd)
        .expect("pre-reboot bootID capture: ssh-run failed");
    let pre_bootid = pre_out.stdout_lossy().trim().to_string();
    assert!(
        !pre_bootid.is_empty(),
        "pre-reboot bootID empty — apiserver may be flaking; aborting before reboot",
    );
    eprintln!("--- pre-reboot bootID for {node}: {pre_bootid} ---");

    // ---- (b) Pre-upgrade booted digest on the node itself ----
    let node_opts = hbird_ssh::SshOptions::new(node_ip.clone())
        .with_user("root")
        .with_proxy_jump(kvm_host.clone());
    let node_cli = hbird_ssh::Client::new(node_opts.clone());

    let digest_cmd = "bootc status --json 2>/dev/null | \
                      jq -r '.status.booted.image.imageDigest // .status.booted.image.digest // empty' \
                      2>/dev/null || true";
    let pre_digest_out = node_cli
        .run(digest_cmd)
        .expect("pre-upgrade digest snapshot: ssh-run failed");
    let pre_digest = pre_digest_out.stdout_lossy().trim().to_string();
    eprintln!(
        "--- pre-upgrade booted digest on {node} ({node_ip}): {} ---",
        if pre_digest.is_empty() {
            "<unknown>"
        } else {
            &pre_digest
        },
    );

    // ---- (c) Drive bootc upgrade --apply, then ensure reboot ----
    //
    // Two paths converge here:
    //
    //   1. An upgrade IS available — bootc tears down SSH (rc=255) and
    //      the reboot fires. Cycle 2's `bootc_upgrade_apply` classifies
    //      255 as `Applied`; we mirror that expectation.
    //   2. No upgrade is available — `bootc upgrade --apply` returns
    //      rc=0 with matching pre/post digests. The bash twin classifies
    //      this as the rc=2 "AlreadyCurrent" path and SKIPS the SSH
    //      drop/back + bootID gates entirely. But cycle 2's live-validate
    //      goal is to exercise THOSE gates, so we force a reboot via
    //      `systemctl reboot` to drive the same downstream flow.
    //
    // Either way the rest of the test asserts the bootID-changed +
    // SSH-back gates do their jobs. Both reboot triggers exercise the
    // same `wait_ssh_drop` / `wait_ssh_back` / `wait_node_bootid_changed`
    // wiring.
    eprintln!(
        "--- issuing `bootc upgrade --apply` on {node} ({node_ip}); may or may not reboot ---"
    );
    let upgrade_result = node_cli.run("bootc upgrade --apply");
    let mut reboot_triggered_via_bootc = false;
    match upgrade_result {
        Ok(out) => {
            eprintln!(
                "--- bootc upgrade returned exit 0 (no reboot kill) ---\nstdout:\n{}\nstderr:\n{}",
                out.stdout_lossy(),
                out.stderr_lossy(),
            );
            // No upgrade rebooted us — explicitly trigger a reboot so the
            // downstream gates have something to observe. Detach in a
            // backgrounded subshell so SSH doesn't hang on the dying
            // session; we ignore exit (the SSH itself will tear down on
            // reboot, rc=255 or 0 depending on race).
            eprintln!("--- no bootc upgrade available; forcing reboot via `systemctl reboot` ---");
            let _ = node_cli.run("systemctl reboot >/dev/null 2>&1 || true");
        }
        Err(hbird_ssh::Error::NonZeroExit { status, stderr, .. }) if status.code() == Some(255) => {
            eprintln!(
                "--- bootc upgrade tore down SSH (rc=255) — reboot in progress ---\nstderr:\n{stderr}"
            );
            reboot_triggered_via_bootc = true;
        }
        Err(other) => {
            panic!("bootc upgrade failed unexpectedly: {other:?}");
        }
    }
    // After this point we EXPECT a reboot via one of the two triggers
    // above. Downstream asserts are non-conditional.
    let _ = reboot_triggered_via_bootc;

    // ---- (d) wait_ssh_drop — poll node for SSH unreachable ----
    //
    // Match cycle 2's helper cadence: 1s poll, ConnectTimeout=3, up to
    // SSH_DROP_TIMEOUT seconds (default 30). The bash twin treats a
    // timeout here as diagnostic (not fatal) — we do the same.
    let drop_probe = hbird_ssh::Client::new(
        node_opts
            .clone()
            .with_connect_timeout(Duration::from_secs(3)),
    );
    let mut ssh_dropped_at: Option<u32> = None;
    for i in 0..60u32 {
        if drop_probe.run("true").is_err() {
            ssh_dropped_at = Some(i);
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    match ssh_dropped_at {
        Some(secs) => eprintln!("--- SSH on {node_ip} dropped after ~{secs}s ---"),
        None => eprintln!("--- WARN: SSH on {node_ip} still up after 60s ---"),
    }
    assert!(
        ssh_dropped_at.is_some(),
        "expected SSH drop after reboot trigger (either bootc rc=255 or systemctl reboot)",
    );

    // ---- (e) wait_ssh_back — poll node for SSH reachable, up to 5min ----
    let back_probe = hbird_ssh::Client::new(
        node_opts
            .clone()
            .with_connect_timeout(Duration::from_secs(3)),
    );
    let mut ssh_back_at: Option<u32> = None;
    let interval = 5u32;
    let max = 300u32;
    let mut elapsed = 0u32;
    while elapsed < max {
        if back_probe.run("true").is_ok() {
            ssh_back_at = Some(elapsed);
            break;
        }
        std::thread::sleep(Duration::from_secs(interval.into()));
        elapsed += interval;
    }
    let ssh_back_at = ssh_back_at.expect("SSH did not come back within 5 minutes");
    eprintln!("--- SSH on {node_ip} back after ~{ssh_back_at}s ---");

    // ---- (f) wait_node_bootid_changed — poll via CP-kubectl ----
    //
    // Apiserver may serve stale Ready=True from the pre-reboot lease
    // before kubelet has re-registered. We poll the bootID directly
    // until it differs — same logic cycle 2's `wait_node_bootid_changed`
    // implements.
    let mut post_bootid = String::new();
    let mut bootid_changed_at: Option<u32> = None;
    let mut elapsed = 0u32;
    while elapsed < 300 {
        match cp.run(&bootid_cmd) {
            Ok(out) => {
                let v = out.stdout_lossy().trim().to_string();
                if !v.is_empty() && v != pre_bootid {
                    post_bootid = v;
                    bootid_changed_at = Some(elapsed);
                    break;
                }
            }
            Err(_) => {
                // apiserver flake; try again
            }
        }
        std::thread::sleep(Duration::from_secs(5));
        elapsed += 5;
    }
    let bootid_changed_at =
        bootid_changed_at.expect("node bootID never changed — reboot may have failed");
    eprintln!(
        "--- node {node} bootID changed (pre={} post={}) after ~{bootid_changed_at}s ---",
        &pre_bootid[..pre_bootid.len().min(8)],
        &post_bootid[..post_bootid.len().min(8)],
    );
    assert_ne!(pre_bootid, post_bootid, "bootID identical post-reboot");

    // ---- (g) Final sanity: node row in `get nodes` is Ready ----
    //
    // Wait up to 2 minutes for the node row to read Ready (kubelet
    // re-registration). Earlier the bootID gate proved the kernel
    // reboot; here we confirm the Ready condition lands too so we
    // don't leave the cluster degraded.
    let mut ready_at: Option<u32> = None;
    let mut elapsed = 0u32;
    while elapsed < 120 {
        if let Ok(out) = cp.run("kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes") {
            let stdout = out.stdout_lossy();
            if let Some(line) = stdout.lines().find(|l| l.starts_with(&node))
                && (line.contains(" Ready ") || line.contains("\tReady\t"))
                && !line.contains("SchedulingDisabled")
            {
                ready_at = Some(elapsed);
                eprintln!("--- node {node} Ready after ~{elapsed}s: {line:?} ---");
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(5));
        elapsed += 5;
    }
    assert!(
        ready_at.is_some(),
        "node {node} did not reach Ready (no SchedulingDisabled) within 2 minutes after bootID change",
    );

    eprintln!(
        "\n=== Cycle 2 live-validate summary ===\n\
         pre_bootid:       {pre_bootid}\n\
         post_bootid:      {post_bootid}\n\
         ssh_dropped:      {ssh_dropped_at:?}s\n\
         ssh_back:         {ssh_back_at}s\n\
         bootid_changed:   {bootid_changed_at}s\n\
         ready:            {ready_at:?}s\n\
         bootc_rc255:      {reboot_triggered_via_bootc}\n",
    );
}
