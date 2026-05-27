//! Dry-run byte-for-byte fixtures for `hbird update-cluster` (#286).
//!
//! Each test below runs `hbird update-cluster --config <fixture-conf>
//! --dry-run [extra flags]` and compares the captured stdout to a
//! pre-captured bash-twin fixture under `tests/update_cluster/fixtures/`.
//!
//! The fixtures were captured by running the bash twin against a
//! synthetic config that names the same VMs the live cluster uses
//! (`hbird-cp1` + `hbird-w1`/`hbird-w2`). Both sides emit a deterministic
//! `<resolved-at-runtime>` placeholder for IPs in dry-run mode so the
//! diff is stable regardless of the operator's libvirt state.
//!
//! # Why a fixture per flag combination?
//!
//! Issue #286 calls out 15 behavioral blocks; the dry-run path exercises
//! every flag-driven branch (workers-only, single-node, start-from, etc).
//! One fixture per major flag combo catches drift in the per-block log
//! shape that a single end-to-end fixture would miss.
//!
//! # How to regenerate fixtures (after an intentional bash-twin change)
//!
//! From the repo root:
//!
//! ```bash
//! cp cluster.local.conf rust/cluster.local.conf  # one-time
//! cd rust
//! for fixture in dry_run_full_cluster dry_run_workers_only dry_run_node_cp \
//!                dry_run_node_worker dry_run_start_from dry_run_skip_drain \
//!                dry_run_skip_gates dry_run_no_empty_dir dry_run_parallel2 \
//!                dry_run_override; do
//!   # mirror the FLAGS column from the table in this file
//!   CONFIG=cluster.local.conf bash ../scripts/update-cluster.sh \
//!     <flags> > crates/hbird-cli/tests/update_cluster/fixtures/$fixture.txt 2>&1
//! done
//! ```

use std::path::PathBuf;
use std::process::Command;

/// Path to the `hbird` binary the test harness built.
fn hbird_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

/// Write a minimal `cluster.local.conf` to `path` matching the live
/// cluster config (`hbird-cp1` + `hbird-w1`/`hbird-w2`). All other
/// fields default — the dry-run path doesn't consult them.
fn write_fixture_config(path: &std::path::Path) {
    std::fs::write(
        path,
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         WORKER_NAMES=(hbird-w1 hbird-w2)\n",
    )
    .expect("write fixture config");
}

/// Locate the fixtures directory regardless of cwd. CARGO_MANIFEST_DIR
/// points at the crate root at test-build time.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("update_cluster")
        .join("fixtures")
}

/// Filter helper: keep only log lines that start with `[update-cluster]`
/// or `[parallel:`. The cargo warnings + `info:` lines that Cargo emits
/// at build time would otherwise contaminate the diff.
fn keep_log_lines(s: &str) -> String {
    s.lines()
        .filter(|l| l.starts_with("[update-cluster]") || l.starts_with("[parallel:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run `hbird update-cluster --config <conf> <flags…>` and return the
/// filtered stdout+stderr lines. We deliberately pass `--config
/// cluster.local.conf` (relative) so the dry-run log echoes the same
/// path the bash twin captured — `Path::display()` on an absolute path
/// would diverge from the bash twin's relative-path echo.
fn run_dry_run(extra_flags: &[&str]) -> String {
    // Use a per-test tempdir so concurrent test runs don't race on cwd.
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .arg("update-cluster")
        .arg("--config")
        .arg("cluster.local.conf")
        .arg("--dry-run")
        .args(extra_flags)
        .output()
        .expect("failed to spawn hbird binary");
    // Combine stdout + stderr for the filter (warnings on stderr,
    // log lines on stdout — both filtered to keep only [update-cluster]
    // / [parallel:] tagged lines).
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    keep_log_lines(&combined)
}

/// Lightweight TempDir without a `tempfile` dep — we only need a unique
/// per-test directory under `$TMPDIR`. Drop removes it.
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
    let dir = std::env::temp_dir().join(format!("hbird-uc-test-{pid}-{n}"));
    std::fs::create_dir_all(&dir).expect("mk tempdir");
    TempDir(dir)
}

/// Compare actual output to the fixture file, surfacing the first
/// diverging line for fast failure diagnosis.
#[track_caller]
fn assert_matches_fixture(name: &str, actual: &str) {
    let expected_path = fixtures_dir().join(format!("{name}.txt"));
    let expected_raw = std::fs::read_to_string(&expected_path)
        .unwrap_or_else(|e| panic!("read fixture {expected_path:?}: {e}"));
    let expected = keep_log_lines(&expected_raw);
    if expected.trim_end() == actual.trim_end() {
        return;
    }
    // Locate the first diverging line for a concise diagnostic.
    for (i, (a, e)) in actual.lines().zip(expected.lines()).enumerate() {
        if a != e {
            panic!(
                "fixture {name}: divergence at line {}\n--- actual:   {a}\n--- expected: {e}\n--- full actual:\n{actual}\n--- full expected:\n{expected}",
                i + 1,
            );
        }
    }
    // Lines compared so far match; one side is just longer.
    panic!(
        "fixture {name}: length mismatch (actual {} lines, expected {} lines)\n--- full actual:\n{actual}\n--- full expected:\n{expected}",
        actual.lines().count(),
        expected.lines().count(),
    );
}

#[test]
fn dry_run_full_cluster_matches_bash_twin() {
    let out = run_dry_run(&[]);
    assert_matches_fixture("dry_run_full_cluster", &out);
}

#[test]
fn dry_run_workers_only_matches_bash_twin() {
    let out = run_dry_run(&["--workers-only"]);
    assert_matches_fixture("dry_run_workers_only", &out);
}

#[test]
fn dry_run_node_cp_matches_bash_twin() {
    let out = run_dry_run(&["--node", "hbird-cp1"]);
    assert_matches_fixture("dry_run_node_cp", &out);
}

#[test]
fn dry_run_node_worker_matches_bash_twin() {
    let out = run_dry_run(&["--node", "hbird-w1"]);
    assert_matches_fixture("dry_run_node_worker", &out);
}

#[test]
fn dry_run_start_from_matches_bash_twin() {
    let out = run_dry_run(&["--start-from", "hbird-w2"]);
    assert_matches_fixture("dry_run_start_from", &out);
}

#[test]
fn dry_run_skip_drain_matches_bash_twin() {
    let out = run_dry_run(&["--skip-drain"]);
    assert_matches_fixture("dry_run_skip_drain", &out);
}

#[test]
fn dry_run_skip_gates_matches_bash_twin() {
    let out = run_dry_run(&["--skip-gates"]);
    assert_matches_fixture("dry_run_skip_gates", &out);
}

#[test]
fn dry_run_no_empty_dir_matches_bash_twin() {
    let out = run_dry_run(&["--no-delete-emptydir-data"]);
    assert_matches_fixture("dry_run_no_empty_dir", &out);
}

#[test]
fn dry_run_parallel2_matches_bash_twin() {
    let out = run_dry_run(&["--parallel", "2"]);
    assert_matches_fixture("dry_run_parallel2", &out);
}

#[test]
fn dry_run_node_name_override_matches_bash_twin() {
    let out = run_dry_run(&["--node-name-override", "hbird-w1=alt-w1"]);
    assert_matches_fixture("dry_run_override", &out);
}

#[test]
fn dry_run_continue_on_error_matches_bash_twin() {
    // Functionally identical to the full-cluster dry-run except the
    // flag echo line. Pin the fixture so a future flag-echo refactor
    // (e.g. coalescing the line) surfaces here.
    let out = run_dry_run(&["--continue-on-error"]);
    assert_matches_fixture("dry_run_continue_on_error", &out);
}

#[test]
fn dry_run_continue_on_error_with_clean_run_exits_zero() {
    // --continue-on-error is a no-op when no failures occur. Confirm
    // the dry-run still exits 0 with the flag set (i.e. the flag doesn't
    // accidentally force a failure path).
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args([
            "update-cluster",
            "--config",
            "cluster.local.conf",
            "--dry-run",
            "--continue-on-error",
        ])
        .output()
        .expect("failed to spawn");
    assert!(
        out.status.success(),
        "exit non-zero: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn live_mode_without_static_ips_surfaces_remediation_diagnostic() {
    // Confirm the live-mode failure surfaces a stable bash-twin-shaped
    // diagnostic. The current scaffold rejects at the CP_IP resolution
    // step (no virsh-domifaddr fallback yet); when WORKER_IPS lacks an
    // entry we surface the "live-mode IP resolution not yet implemented"
    // message. Either is acceptable evidence the live path is wired —
    // we pin both wording snippets so a future fixture-shape change
    // surfaces here.
    let tmp = tempdir_for_test();
    let conf_path = tmp.path().join("cluster.local.conf");
    write_fixture_config(&conf_path);

    let out = Command::new(hbird_bin())
        .current_dir(tmp.path())
        .args(["update-cluster", "--config", "cluster.local.conf"])
        .output()
        .expect("failed to spawn");
    assert!(
        !out.status.success(),
        "live mode should fail in this scaffold"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let acceptable_messages = [
        "live-mode IP resolution not yet implemented",
        "Set WORKER_IPS",
        // CP-IP-first failure path: bash twin's exact wording.
        "could not resolve CP IP for domain",
        "set CP_IP= in env to override",
    ];
    assert!(
        acceptable_messages.iter().any(|m| stderr.contains(m)),
        "missing remediation diagnostic; stderr was:\n{stderr}\n\
         expected one of: {acceptable_messages:?}",
    );
}
