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

/// Live mode for deploy-cluster bails with the #335-linked diagnostic
/// rather than the pre-#289 `not yet implemented — tracked by #289` stub.
#[test]
fn deploy_cluster_live_mode_surfaces_335_diagnostic() {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args(["deploy-cluster", "--config", "cluster.local.conf"])
        .output()
        .expect("spawn hbird");
    assert!(!out.status.success(), "live-mode deploy-cluster exited 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("#335"),
        "live-mode error should reference #335 follow-up; got:\n{stderr}"
    );
    assert!(
        stderr.contains("--dry-run") || stderr.contains("dry-run"),
        "live-mode error should point at --dry-run as the workaround; got:\n{stderr}"
    );
}

/// Live mode for spawn-workers bails the same way.
#[test]
fn spawn_workers_live_mode_surfaces_335_diagnostic() {
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args(["spawn-workers", "--config", "cluster.local.conf"])
        .output()
        .expect("spawn hbird");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("#335"),
        "live-mode spawn-workers should reference #335; got:\n{stderr}"
    );
}

/// `destroy-cluster` live mode is implemented but requires `--kvm-host`
/// to be set (the Rust path doesn't yet support on-host libvirt without
/// SSH). Assert the diagnostic is clear and points at `--dry-run` /
/// the bash twin as the workaround.
#[test]
fn destroy_cluster_live_mode_without_kvm_host_surfaces_clear_diagnostic() {
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
    assert!(
        !out.status.success(),
        "live-mode destroy-cluster without --kvm-host exited 0"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--kvm-host") || stderr.contains("KVM_HOST"),
        "destroy-cluster should mention --kvm-host in the diagnostic; got:\n{stderr}"
    );
    assert!(
        stderr.contains("dry-run") || stderr.contains("destroy-cluster"),
        "destroy-cluster should mention --dry-run or bash fallback; got:\n{stderr}"
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
