//! End-to-end `--dry-run` + usage-error tests for `hbird etcd …`.
//!
//! Bash twins: `scripts/backup-etcd.sh`, `scripts/restore-etcd.sh`,
//! `scripts/rotate-etcd-encryption-key.sh`. None of them has a
//! `--dry-run` flag, so these are Rust-side plan tests rather than a
//! bash-vs-Rust diff — the parity that IS pinned here is the exit-code
//! contract (`2` for usage errors, `1` for a missing snapshot) and the
//! operator-visible wording the bash twins print.
//!
//! Every case execs the real binary with the CP IP pinned, so no SSH
//! connection is ever attempted. `env_remove` keeps a developer's
//! exported `CONFIG` / `CP_IP` / `KVM_HOST` / `SNAP` out of the run.

use std::path::PathBuf;
use std::process::Command;

/// Path to the `hbird` binary the test harness built.
fn hbird_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

/// Run `hbird <args…>` with a scrubbed environment.
fn run(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let out = Command::new(hbird_bin())
        .args(args)
        .env_remove("CONFIG")
        .env_remove("CP_NAME")
        .env_remove("CP_IP")
        .env_remove("KVM_HOST")
        .env_remove("SNAP")
        .output()
        .expect("failed to spawn hbird binary");
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Keep only the plan lines so tracing output can't contaminate a diff.
fn plan_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|l| l.starts_with("DRY-RUN"))
        .collect()
}

#[test]
fn etcd_backup_dry_run_prints_the_full_plan_and_touches_nothing() {
    let (status, stdout, _stderr) = run(&[
        "etcd",
        "backup",
        "--dry-run",
        "--cp-ip",
        "10.0.0.5",
        "--kvm-host",
        "geary",
        "--outdir",
        "/tmp/hbird-test-backups",
        "--label",
        "pre cni swap",
    ]);
    assert!(status.success(), "dry-run must exit 0: {stdout}");
    let lines = plan_lines(&stdout);
    assert_eq!(lines.len(), 8, "unexpected plan shape: {stdout}");
    assert_eq!(lines[0], "DRY-RUN etcd backup");
    // Label sanitisation is visible in the planned filename.
    assert!(
        stdout.contains("/tmp/hbird-test-backups/etcd-snapshot-<UTC-timestamp>-pre-cni-swap.db"),
        "{stdout}"
    );
    // ProxyJump renders as a pasteable `ssh -J`.
    assert!(stdout.contains("ssh -J geary root@10.0.0.5"), "{stdout}");
    // Nothing may be created on disk by a dry run.
    assert!(
        !PathBuf::from("/tmp/hbird-test-backups").exists(),
        "dry-run must not create the output directory"
    );
}

/// Two runs of the same dry-run command must produce identical stdout:
/// no wall clock, no RNG, no network.
#[test]
fn etcd_backup_dry_run_is_deterministic() {
    let args = &["etcd", "backup", "--dry-run", "--cp-ip", "10.0.0.5"];
    let (_, first, _) = run(args);
    let (_, second, _) = run(args);
    assert_eq!(first, second, "dry-run output must be deterministic");
}

/// With no `--cp-ip`, no `--config` and no `KVM_HOST`, a dry run still
/// renders a plan (placeholder IP) instead of trying to reach libvirt.
#[test]
fn etcd_backup_dry_run_without_cp_ip_uses_a_placeholder() {
    let (status, stdout, _stderr) = run(&["etcd", "backup", "--dry-run"]);
    assert!(
        status.success(),
        "dry-run must not need a reachable CP: {stdout}"
    );
    assert!(stdout.contains("root@<cp-ip>"), "{stdout}");
}

#[test]
fn etcd_restore_dry_run_lists_the_destructive_steps() {
    // `--snapshot` must point at a real file; /etc/hostname always exists.
    let (status, stdout, _stderr) = run(&[
        "etcd",
        "restore",
        "--dry-run",
        "--cp-ip",
        "10.0.0.5",
        "--snapshot",
        "/etc/hostname",
    ]);
    assert!(status.success(), "dry-run must exit 0: {stdout}");
    let lines = plan_lines(&stdout);
    assert_eq!(lines.len(), 8, "unexpected plan shape: {stdout}");
    assert!(lines[0].contains("DESTRUCTIVE"), "{stdout}");
    assert!(
        stdout.contains("test ! -e /etc/kubernetes/manifests.disabled"),
        "the re-entrancy guard must show up in the plan: {stdout}"
    );
}

#[test]
fn etcd_rotate_key_dry_run_lists_all_four_stages() {
    let (status, stdout, _stderr) =
        run(&["etcd", "rotate-key", "--dry-run", "--cp-ip", "10.0.0.5"]);
    assert!(status.success(), "dry-run must exit 0: {stdout}");
    for stage in ["Stage 0", "Stage 1", "Stage 2", "Stage 3", "Stage 4"] {
        assert!(stdout.contains(stage), "plan missing {stage}: {stdout}");
    }
    assert!(
        plan_lines(&stdout).len() >= 12,
        "unexpected plan shape: {stdout}"
    );
}

/// Bash twin `backup-etcd.sh:115` prints this exact line and exits 2
/// (usage error). `anyhow` would have collapsed it to exit 1, so the
/// code is pinned here.
#[test]
fn etcd_backup_empty_label_exits_2_with_bash_twin_wording() {
    let (status, _stdout, stderr) = run(&["etcd", "backup", "--label", "///"]);
    assert_eq!(status.code(), Some(2), "expected the bash twin's exit 2");
    assert!(
        stderr.contains("--label resolved to empty after sanitize"),
        "{stderr}"
    );
}

/// Bash twin `restore-etcd.sh:71`: `Snapshot not found: <path>`, exit 1.
#[test]
fn etcd_restore_missing_snapshot_exits_1_with_bash_twin_wording() {
    let (status, _stdout, stderr) = run(&[
        "etcd",
        "restore",
        "--cp-ip",
        "10.0.0.5",
        "--snapshot",
        "/nonexistent/etcd-snapshot.db",
    ]);
    assert_eq!(status.code(), Some(1), "expected exit 1");
    assert!(
        stderr.contains("Snapshot not found: /nonexistent/etcd-snapshot.db"),
        "{stderr}"
    );
}

/// `--snapshot` is required (the Makefile passes `SNAP=`); a missing
/// one must be a clap usage error (exit 2), not a panic.
#[test]
fn etcd_restore_without_snapshot_fails_to_parse() {
    let (status, _stdout, stderr) = run(&["etcd", "restore", "--cp-ip", "10.0.0.5"]);
    assert_eq!(status.code(), Some(2), "clap usage errors exit 2");
    assert!(stderr.contains("--snapshot"), "{stderr}");
}

/// A destructive subcommand must never proceed on a non-TTY stdin
/// without `--yes` — CI and `make` under a pipe would otherwise sail
/// straight past the bash twin's `read` gate.
#[test]
fn etcd_rotate_key_refuses_to_run_unconfirmed_without_a_tty() {
    let (status, _stdout, stderr) = run(&["etcd", "rotate-key", "--cp-ip", "10.0.0.5"]);
    assert_eq!(status.code(), Some(1), "expected exit 1");
    assert!(
        stderr.contains("stdin is not a TTY"),
        "must refuse rather than guess: {stderr}"
    );
    assert!(
        stderr.contains("--dry-run"),
        "must point at the safe path: {stderr}"
    );
}
