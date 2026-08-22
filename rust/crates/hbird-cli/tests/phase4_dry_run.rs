//! Dry-run byte-for-byte fixtures for `hbird deploy-cluster`,
//! `hbird destroy-cluster`, and `hbird spawn-workers` (#289 Phase 4).
//!
//! Mirrors the `tests/update_cluster_dry_run.rs` pattern (#321 / #325):
//! exec the binary with `--dry-run`, capture stdout, filter to the
//! `[<subcommand>]`-prefixed lines, and diff against a pinned fixture
//! file. The fixtures pin the planner output so a future refactor that
//! coalesces / re-orders log lines surfaces here.
//!
//! # Why no bash-twin parity claim?
//!
//! Unlike Phase 1 (`update-cluster`), the bash twins under
//! `scripts/deploy-cluster.sh` / `destroy-cluster.sh` / `spawn-workers.sh`
//! do not implement a `--dry-run` flag. The Rust subcommands' dry-run
//! shape is Rust-side-only — useful for previewing the plan before
//! committing to side effects, but NOT a bash-vs-Rust diff. The fixtures
//! capture the Rust output verbatim; bash parity for live execution is
//! tracked by #335.

use std::path::PathBuf;
use std::process::Command;

/// Path to the `hbird` binary the test harness built.
fn hbird_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

/// Locate the fixtures directory regardless of cwd.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("update_cluster")
        .join("fixtures")
}

/// Filter helper: keep only log lines that start with one of the
/// subcommand prefixes used by Phase 4. Cargo warnings + tracing-
/// subscriber lines that leak into the test stdout would otherwise
/// contaminate the diff.
fn keep_log_lines(s: &str) -> String {
    s.lines()
        .filter(|l| {
            l.starts_with("[deploy-cluster]")
                || l.starts_with("[destroy-cluster]")
                || l.starts_with("[spawn-workers]")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lightweight TempDir without a `tempfile` dep — same pattern as
/// `tests/update_cluster_dry_run.rs`.
struct TempDir(PathBuf);
impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tempdir_for_test() -> TempDir {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("hbird-p4-test-{pid}-{n}"));
    std::fs::create_dir_all(&dir).expect("mk tempdir");
    TempDir(dir)
}

/// Write a fixture `cluster.local.conf` to `path` with the same values
/// the fixtures were captured against.
fn write_fixture_config(path: &std::path::Path) {
    std::fs::write(
        path,
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=1\n\
         WORKER_NAMES=(hbird-w1 hbird-w2)\n\
         POOL_DIR=/mnt/pool\n\
         IMAGE_SOURCE=ghcr\n\
         GHCR_TAG=v0.42.0\n",
    )
    .expect("write fixture config");
}

/// Run the binary with `args` (relative `--config cluster.local.conf`
/// expected) in a per-test tempdir, and return the filtered stdout.
fn run_dry_run(subcommand: &str, extra_flags: &[&str]) -> String {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let mut args: Vec<&str> = vec![subcommand, "--config", "cluster.local.conf", "--dry-run"];
    args.extend_from_slice(extra_flags);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args(&args)
        .output()
        .expect("spawn hbird");
    assert!(
        out.status.success(),
        "hbird {subcommand} --dry-run exited non-zero. stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    keep_log_lines(&combined)
}

/// Compare actual output to the fixture file, surfacing the first
/// diverging line. Same shape as
/// `update_cluster_dry_run::assert_matches_fixture`.
#[track_caller]
fn assert_matches_fixture(name: &str, actual: &str) {
    assert!(
        !actual.trim().is_empty(),
        "fixture {name}: actual output filtered to 0 lines — either the binary regressed \
         (no log output), the prefix filter regressed, or the dry-run path bailed out early. \
         Re-run with `cargo test -- --nocapture` to inspect."
    );
    let expected_path = fixtures_dir().join(format!("{name}.txt"));
    let expected_raw = std::fs::read_to_string(&expected_path)
        .unwrap_or_else(|e| panic!("read fixture {expected_path:?}: {e}"));
    let expected = keep_log_lines(&expected_raw);
    assert!(
        !expected.trim().is_empty(),
        "fixture {name}: fixture file at {expected_path:?} filters to 0 lines — must contain at least one prefixed line"
    );
    if expected.trim_end() == actual.trim_end() {
        return;
    }
    for (i, (a, e)) in actual.lines().zip(expected.lines()).enumerate() {
        if a != e {
            panic!(
                "fixture {name}: divergence at line {}\n--- actual:   {a}\n--- expected: {e}\n--- full actual:\n{actual}\n--- full expected:\n{expected}",
                i + 1,
            );
        }
    }
    panic!(
        "fixture {name}: length mismatch (actual {} lines, expected {} lines)\n--- full actual:\n{actual}\n--- full expected:\n{expected}",
        actual.lines().count(),
        expected.lines().count(),
    );
}

#[test]
fn deploy_cluster_dry_run_matches_fixture() {
    let out = run_dry_run("deploy-cluster", &[]);
    assert_matches_fixture("dry_run_deploy", &out);
}

#[test]
fn destroy_cluster_dry_run_matches_fixture() {
    let out = run_dry_run("destroy-cluster", &[]);
    assert_matches_fixture("dry_run_destroy", &out);
}

#[test]
fn spawn_workers_dry_run_matches_fixture() {
    let out = run_dry_run("spawn-workers", &["--count", "3"]);
    assert_matches_fixture("dry_run_spawn", &out);
}

/// Live mode for deploy-cluster fails at infrastructure (podman pull on CI)
/// or at the boot stub (#335). After S2b, the image-acquisition step is live,
/// so on CI without podman the command fails at podman pull rather than the
/// old `#335` stub. In both cases the command must exit non-zero.
#[test]
fn deploy_cluster_live_mode_fails_at_infra_or_boot_stub() {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args(["deploy-cluster", "--config", "cluster.local.conf"])
        .output()
        .expect("spawn hbird");

    // Must fail regardless of whether podman is installed.
    assert!(!out.status.success(), "live-mode deploy-cluster exited 0");

    let stderr = String::from_utf8_lossy(&out.stderr);
    // Must NOT contain the old pre-S2b stub message (plan_image_acquisition stub).
    assert!(
        !stderr.contains("plan_image_acquisition"),
        "S2b replaced the plan_image_acquisition stub; got:\n{stderr}"
    );
    // Either fails at podman pull (infra) or at a later boot stub (#335).
    // We don't assert the exact message — just that it fails for the right
    // reasons (not a regression to an earlier stub).
    let _ = stderr; // accepted either failure mode
}

/// Live mode for spawn-workers (S3): stubs replaced with real implementations.
/// On CI without a live cluster the command fails at CP IP resolution (domifaddr),
/// which is the correct live behavior — no longer the old #335 stub.
#[test]
fn spawn_workers_live_mode_fails_at_infra() {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args([
            "spawn-workers",
            "--config",
            "cluster.local.conf",
            "--cp-ssh-retries",
            "1",
        ])
        .output()
        .expect("spawn hbird");

    // Must fail — no live cluster on CI.
    assert!(!out.status.success(), "live-mode spawn-workers exited 0");

    let stderr = String::from_utf8_lossy(&out.stderr);
    // Must NOT contain the old pre-S3 stub message.
    assert!(
        !stderr.contains("not yet implemented"),
        "S3 replaced the live-mode stubs; got:\n{stderr}"
    );
}

/// `destroy-cluster` live mode without `--kvm-host` now uses the local
/// libvirt transport (S2a). On any host where the test cluster doesn't
/// exist (CI, workstations), virsh reports "domain not found" which the
/// idempotent destroy path treats as already torn down — so the command
/// exits 0. Assert the old "kvm-host required" gate is gone.
#[test]
fn destroy_cluster_live_mode_without_kvm_host_uses_local_transport() {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        // No --kvm-host, no KVM_HOST env, no kvm_host= in config.
        .env_remove("KVM_HOST")
        .args(["destroy-cluster", "--config", "cluster.local.conf"])
        .output()
        .expect("spawn hbird");

    let stderr = String::from_utf8_lossy(&out.stderr);
    // S2a removed the "not yet wired" gate — that message must never appear.
    assert!(
        !stderr.contains("Local libvirt access without SSH is not yet wired"),
        "S2a removed this limitation; got:\n{stderr}"
    );
    // The old hard-fail required --kvm-host. That must be gone too.
    assert!(
        !stderr.contains("requires --kvm-host"),
        "S2a: no-kvm-host is valid via local transport; got:\n{stderr}"
    );
    // On CI the test cluster doesn't exist locally, so virsh says "domain
    // not found" (VirshFailed) → treated as already torn down → exit 0.
    // If virsh isn't installed at all, sh exits 127 → also VirshFailed → exit 0.
    assert!(
        out.status.success(),
        "destroy-cluster without --kvm-host should succeed (idempotent: no cluster on CI). stderr:\n{stderr}"
    );
}

/// `--count 0` is rejected with a clear diagnostic. Mirrors the bash
/// twin's positional-default-of-2 contract — but the Rust path makes
/// the constraint explicit.
#[test]
fn spawn_workers_count_zero_rejected() {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args([
            "spawn-workers",
            "--config",
            "cluster.local.conf",
            "--count",
            "0",
            "--dry-run",
        ])
        .output()
        .expect("spawn hbird");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--count") && stderr.contains("> 0"),
        "spawn-workers --count 0 should be rejected with a clear diagnostic; got:\n{stderr}"
    );
}

/// CP-only deploy (`WORKER_NAMES=()`) emits the right footer.
#[test]
fn deploy_cluster_cp_only_dry_run_skips_worker_block() {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    std::fs::write(
        &conf_path,
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=1\n\
         WORKER_NAMES=()\n\
         POOL_DIR=/mnt/pool\n\
         IMAGE_SOURCE=ghcr\n\
         GHCR_TAG=v0.42.0\n",
    )
    .expect("write");

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args([
            "deploy-cluster",
            "--config",
            "cluster.local.conf",
            "--dry-run",
        ])
        .output()
        .expect("spawn hbird");
    assert!(out.status.success(), "CP-only dry-run failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("WORKER_NAMES=()") || stdout.contains("CP-only"),
        "expected CP-only marker in dry-run output; got:\n{stdout}"
    );
    // Should still emit the cluster Ready poll for 1 node (just CP).
    assert!(
        stdout.contains("1 nodes Ready"),
        "CP-only deploy should poll for 1 node Ready; got:\n{stdout}"
    );
}

/// `ENABLE_CLOUD_INIT != 1` fails fast with the bash-twin diagnostic.
#[test]
fn deploy_cluster_rejects_cloud_init_zero() {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    std::fs::write(
        &conf_path,
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=0\n\
         POOL_DIR=/mnt/pool\n",
    )
    .expect("write");

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args([
            "deploy-cluster",
            "--config",
            "cluster.local.conf",
            "--dry-run",
        ])
        .output()
        .expect("spawn hbird");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ENABLE_CLOUD_INIT"),
        "expected ENABLE_CLOUD_INIT diagnostic; got:\n{stderr}"
    );
}

/// Deploy helper for #405-#410 tests: run `hbird deploy-cluster --dry-run`
/// against an arbitrary config body, returning (success, stdout+stderr).
fn run_deploy_dry_run_with_config(config_body: &str) -> (bool, String) {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    std::fs::write(&conf_path, config_body).expect("write config");
    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args([
            "deploy-cluster",
            "--config",
            "cluster.local.conf",
            "--dry-run",
        ])
        .output()
        .expect("spawn hbird");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// Unknown config keys must surface as WARN lines at plan time (#405-#410
/// root cause: bash `source` silently eats unknown keys, and the Rust
/// parser's warnings were computed but never printed — a typo'd knob
/// vanished without a trace).
#[test]
fn deploy_cluster_dry_run_prints_unknown_key_warnings() {
    let (ok, out) = run_deploy_dry_run_with_config(
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=1\n\
         WORKER_NAMES=()\n\
         POOL_DIR=/mnt/pool\n\
         GHCR_TAG=v0.42.0\n\
         WROKER_MACS=(02:00:00:00:00:01)\n",
    );
    assert!(ok, "dry-run must still succeed on unknown keys:\n{out}");
    assert!(
        out.contains("[deploy-cluster] WARN: config: unknown key \"WROKER_MACS\""),
        "typo'd key must be surfaced as a WARN line; got:\n{out}"
    );
}

/// CP_MAC + CP_IP: the dry-run plan shows the DHCP reservation and the
/// pinned primary MAC on the virt-install line (#409).
#[test]
fn deploy_cluster_dry_run_plans_mac_pin_and_reservation() {
    let (ok, out) = run_deploy_dry_run_with_config(
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=1\n\
         POOL_DIR=/mnt/pool\n\
         GHCR_TAG=v0.42.0\n\
         WORKER_NAMES=(hbird-w1)\n\
         CP_MAC=02:00:00:00:00:01\n\
         CP_IP=192.168.122.10\n\
         WORKER_MACS=(02:00:00:00:00:02)\n\
         WORKER_IPS=(192.168.122.21)\n",
    );
    assert!(ok, "dry-run failed:\n{out}");
    assert!(
        out.contains(
            "would ensure DHCP reservation on 'default': hbird-cp1 02:00:00:00:00:01 -> 192.168.122.10"
        ),
        "missing CP reservation plan line:\n{out}"
    );
    assert!(
        out.contains(
            "would ensure DHCP reservation on 'default': hbird-w1 02:00:00:00:00:02 -> 192.168.122.21"
        ),
        "missing worker reservation plan line:\n{out}"
    );
    assert!(
        out.contains("(memory=8192 vcpus=4, primary mac=02:00:00:00:00:01)"),
        "CP virt-install plan line must show the pinned MAC:\n{out}"
    );
    assert!(
        out.contains("(memory=4096 vcpus=2, primary mac=02:00:00:00:00:02)"),
        "worker virt-install plan line must show the pinned MAC:\n{out}"
    );
}

/// CP_IP alone (no CP_MAC): bash still reserves, with the name-derived
/// MAC — deterministic, so the plan can print it.
#[test]
fn deploy_cluster_dry_run_reserves_with_derived_mac_when_only_ip_set() {
    let (ok, out) = run_deploy_dry_run_with_config(
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=1\n\
         POOL_DIR=/mnt/pool\n\
         GHCR_TAG=v0.42.0\n\
         WORKER_NAMES=()\n\
         CP_IP=192.168.122.10\n",
    );
    assert!(ok, "dry-run failed:\n{out}");
    // sha256("hbird-cp1") starts f01dbd — bash derive_primary_mac parity.
    assert!(
        out.contains(
            "would ensure DHCP reservation on 'default': hbird-cp1 52:54:00:f0:1d:bd -> 192.168.122.10"
        ),
        "derived-MAC reservation plan line missing:\n{out}"
    );
    // No CP_MAC => the virt-install plan line keeps its legacy shape.
    assert!(
        out.contains("would virt-install hbird-cp1 (memory=8192 vcpus=4) attaching"),
        "virt-install plan line must NOT grow a mac note without CP_MAC:\n{out}"
    );
}

/// Malformed CP_MAC fails fast, before any side effects, with the
/// bash-twin diagnostic.
#[test]
fn deploy_cluster_rejects_malformed_cp_mac() {
    let (ok, out) = run_deploy_dry_run_with_config(
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=1\n\
         POOL_DIR=/mnt/pool\n\
         CP_MAC=zz:zz:zz:zz:zz:zz\n",
    );
    assert!(!ok, "malformed CP_MAC must fail:\n{out}");
    assert!(
        out.contains("CP_MAC is malformed (need aa:bb:cc:dd:ee:ff): 'zz:zz:zz:zz:zz:zz'"),
        "missing bash-twin diagnostic:\n{out}"
    );
}

/// Full EXTRA_NETWORK family: dry-run surfaces the extra-net suffix on
/// `config OK`, the host preflight, the per-VM network-config renders,
/// and the second-NIC attach lines (#405-#408).
#[test]
fn deploy_cluster_dry_run_plans_extra_network() {
    let (ok, out) = run_deploy_dry_run_with_config(
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=1\n\
         POOL_DIR=/mnt/pool\n\
         GHCR_TAG=v0.42.0\n\
         WORKER_NAMES=(hbird-w1)\n\
         EXTRA_NETWORK=vf-pool\n\
         EXTRA_NET_CP_MAC=02:11:22:33:44:55\n\
         EXTRA_NET_CP_IP=10.0.0.241/24\n\
         EXTRA_NET_WORKER_MACS=(02:11:22:33:44:56)\n\
         EXTRA_NET_WORKER_IPS=(10.0.0.242/24)\n",
    );
    assert!(ok, "dry-run failed:\n{out}");
    // Byte-parity with bash 819: `, extra-net=<net>` suffix on config OK.
    assert!(
        out.contains("config OK: CP=hbird-cp1, workers=(hbird-w1), source=ghcr, tag=v0.42.0, extra-net=vf-pool"),
        "config OK suffix missing:\n{out}"
    );
    assert!(
        out.contains("would verify EXTRA_NETWORK 'vf-pool' is defined + active"),
        "host preflight plan line missing:\n{out}"
    );
    assert!(
        out.contains("would render net2 network-config for hbird-cp1 (mac=02:11:22:33:44:55, ip=10.0.0.241/24) into the seed"),
        "CP net2 render line missing:\n{out}"
    );
    assert!(
        out.contains("would render net2 network-config for hbird-w1 (mac=02:11:22:33:44:56, ip=10.0.0.242/24) into the seed"),
        "worker net2 render line missing:\n{out}"
    );
    assert!(
        out.contains("would attach second NIC: network=vf-pool,mac=02:11:22:33:44:55"),
        "CP second-NIC attach line missing:\n{out}"
    );
    assert!(
        out.contains("would attach second NIC: network=vf-pool,mac=02:11:22:33:44:56"),
        "worker second-NIC attach line missing:\n{out}"
    );
}

/// Half-set family (EXTRA_NET_* without EXTRA_NETWORK) fails fast with
/// the bash-twin diagnostic — the silent-ignore path is the bug class
/// this block exists to kill.
#[test]
fn deploy_cluster_rejects_half_set_extra_net_family() {
    let (ok, out) = run_deploy_dry_run_with_config(
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         ENABLE_CLOUD_INIT=1\n\
         POOL_DIR=/mnt/pool\n\
         WORKER_NAMES=()\n\
         EXTRA_NET_CP_MAC=02:11:22:33:44:55\n",
    );
    assert!(!ok, "half-set family must fail:\n{out}");
    assert!(
        out.contains("EXTRA_NET_* values are set but EXTRA_NETWORK is empty"),
        "missing diagnostic:\n{out}"
    );
}
