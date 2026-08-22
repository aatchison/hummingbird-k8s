//! Process-level checks for `hbird kube-bench`
//! (bash twin: `scripts/run-kube-bench.sh`).
//!
//! Only the paths that need NO cluster are exercised here: the dry-run
//! planner and the argument-validation exit codes. Everything that
//! talks to a cluster is covered by the mocked `SshExec` unit tests in
//! `commands/kube_bench.rs`.

use std::path::PathBuf;
use std::process::Command;

/// Locate the `hbird` binary cargo built for this test run.
fn hbird_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

/// Run `hbird kube-bench <args…>` with every consumed env var scrubbed,
/// so a developer's exported `KVM_HOST` / `KUBE_BENCH_*` cannot leak in.
fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(hbird_bin())
        .arg("kube-bench")
        .args(args)
        .env_remove("CONFIG")
        .env_remove("CP_IP")
        .env_remove("CP_NAME")
        .env_remove("KVM_HOST")
        .env_remove("KUBE_BENCH_VERSION")
        .env_remove("KUBE_BENCH_TIMEOUT")
        .env_remove("KUBE_BENCH_NS")
        .env_remove("KUBE_BENCH_TARGETS")
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to spawn hbird");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn dry_run_needs_no_cluster_and_exits_zero() {
    let (code, stdout, stderr) = run(&["--dry-run", "--cp-ip=192.168.122.42"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stdout.is_empty(),
        "the plan belongs on stderr so a redirected stdout stays a clean baseline: {stdout}"
    );
    assert!(stderr.contains("DRY-RUN kube-bench version: v0.15.5"));
    assert!(stderr.contains("job-master.yaml"), "stderr: {stderr}");
    assert!(stderr.contains("job-node.yaml"), "stderr: {stderr}");
}

#[test]
fn dry_run_without_a_cp_ip_stays_symbolic() {
    // No CP_IP, no config, no libvirt — the planner must still print a
    // plan rather than trying to resolve an IP.
    let (code, _stdout, stderr) = run(&["--dry-run"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stderr.contains("<resolved-at-runtime>"), "stderr: {stderr}");
}

#[test]
fn dry_run_honours_the_target_subset() {
    let (code, _stdout, stderr) = run(&["--dry-run", "--cp-ip=1.2.3.4", "--targets=node"]);
    assert_eq!(code, Some(0));
    assert!(stderr.contains("job-node.yaml"));
    assert!(
        !stderr.contains("job-master.yaml"),
        "master must be skipped: {stderr}"
    );
}

#[test]
fn dry_run_threads_version_timeout_and_namespace() {
    let (code, _stdout, stderr) = run(&[
        "--dry-run",
        "--cp-ip=1.2.3.4",
        "--kube-bench-version=v0.16.0",
        "--timeout=90s",
        "--namespace=kube-bench",
    ]);
    assert_eq!(code, Some(0));
    assert!(
        stderr.contains("kube-bench/v0.16.0/job-master.yaml"),
        "{stderr}"
    );
    assert!(stderr.contains("--timeout=90s"), "{stderr}");
    assert!(stderr.contains("-n kube-bench"), "{stderr}");
}

#[test]
fn unknown_target_exits_two_without_contacting_the_cluster() {
    // Twin's `FAIL: unknown target …` + exit 2. No --cp-ip is supplied,
    // so reaching exit 2 also proves validation happens BEFORE any
    // resolution or kubectl call.
    let (code, _stdout, stderr) = run(&["--targets=master worker"]);
    assert_eq!(code, Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("FAIL: unknown target 'worker' (expected master or node)"),
        "stderr: {stderr}"
    );
}

#[test]
fn empty_target_list_exits_two() {
    let (code, _stdout, stderr) = run(&["--targets= "]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("FAIL: no targets requested"),
        "stderr: {stderr}"
    );
}

#[test]
fn dry_run_output_is_byte_stable_across_invocations() {
    let args = &["--dry-run", "--cp-ip=192.168.122.42"];
    let (_, _, a) = run(args);
    let (_, _, b) = run(args);
    assert_eq!(a, b, "dry-run plan must be deterministic");
}

#[test]
fn help_documents_the_env_var_surface() {
    let (code, stdout, _stderr) = run(&["--help"]);
    assert_eq!(code, Some(0));
    for marker in [
        "KUBE_BENCH_VERSION",
        "KUBE_BENCH_TIMEOUT",
        "KUBE_BENCH_NS",
        "KUBE_BENCH_TARGETS",
    ] {
        assert!(stdout.contains(marker), "help missing {marker}:\n{stdout}");
    }
}
