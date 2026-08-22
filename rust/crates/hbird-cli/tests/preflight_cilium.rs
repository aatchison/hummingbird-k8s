//! Process-level exit-code contract for `hbird preflight cilium`
//! (bash twin: `scripts/check-cilium-k8s-compat.sh`, issue #303).
//!
//! The module's own unit tests cover classification and wording. These
//! tests exist because the operator contract is stated in EXIT CODES,
//! and an exit code is only real at the process boundary — a `run()`
//! that returns `Ok(())` while `main` maps it to 1 would pass every
//! unit test and still break the pre-merge gate.
//!
//! Case list mirrors `tests/scripts/check-cilium-k8s-compat.bats` so the
//! bash and Rust surfaces can be diffed test-for-test.

use std::path::PathBuf;
use std::process::Command;

/// Locate the `hbird` binary cargo built for this test run.
fn hbird_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

/// Run `hbird preflight cilium <args…>` with a scrubbed environment.
///
/// Every env var the command (or its config lookup) reads is removed so
/// a developer with `CONFIG=` / `KVM_HOST=` exported in their shell
/// cannot change the result.
fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(hbird_bin())
        .arg("preflight")
        .arg("cilium")
        .args(args)
        .env_remove("CONFIG")
        .env_remove("CP_IP")
        .env_remove("CP_NAME")
        .env_remove("KVM_HOST")
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
fn supported_pair_exits_zero_and_prints_ok_on_stdout() {
    let (code, stdout, stderr) = run(&["--cilium=1.16.5", "--k8s=v1.29"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stdout.starts_with("OK: Cilium 1.16.5 supports K8s 1.29"),
        "stdout: {stdout}"
    );
}

#[test]
fn mismatch_warns_but_exits_zero_by_default() {
    // Default mode must never block a build: this is the whole reason
    // the twin exists as a *warn* by default.
    let (code, stdout, stderr) = run(&["--cilium=1.16.5", "--k8s=v1.31"]);
    assert_eq!(code, Some(0), "default mode must not fail");
    assert!(stdout.is_empty(), "warnings belong on stderr: {stdout}");
    assert!(stderr.contains("does NOT list"), "stderr: {stderr}");
    assert!(
        stderr.contains("docs/cilium-migration.md"),
        "stderr: {stderr}"
    );
}

#[test]
fn mismatch_under_strict_exits_one() {
    let (code, _stdout, stderr) = run(&["--cilium=1.16.5", "--k8s=v1.31", "--strict"]);
    assert_eq!(code, Some(1), "strict mode must fail on mismatch");
    assert!(stderr.contains("WARN"), "stderr: {stderr}");
}

#[test]
fn supported_pair_under_strict_still_exits_zero() {
    let (code, stdout, _stderr) = run(&["--cilium=1.17.2", "--k8s=v1.31", "--strict"]);
    assert_eq!(code, Some(0));
    assert!(stdout.contains("OK"), "stdout: {stdout}");
}

#[test]
fn unknown_cilium_minor_is_a_matrix_stale_warn() {
    let (code, _stdout, stderr) = run(&["--cilium=1.99.0", "--k8s=v1.31"]);
    assert_eq!(code, Some(0));
    assert!(
        stderr.contains("not in the embedded compat matrix"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("1.99"), "stderr: {stderr}");
}

#[test]
fn unknown_cilium_minor_under_strict_exits_one() {
    // A stale matrix must fail CLOSED in a pre-merge gate — otherwise
    // the gate silently stops checking after the next Cilium bump.
    let (code, _stdout, stderr) = run(&["--cilium=1.99.0", "--k8s=v1.31", "--strict"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("not in the embedded compat matrix"));
}

#[test]
fn k8s_override_accepts_bare_and_patch_forms() {
    for form in ["1.30", "v1.30", "v1.30.4"] {
        let (code, stdout, _stderr) = run(&["--cilium=1.16.5", &format!("--k8s={form}")]);
        assert_eq!(code, Some(0), "form {form}");
        assert!(stdout.contains("OK"), "form {form}: {stdout}");
    }
}

#[test]
fn unreadable_repo_root_exits_two() {
    // Bash twin: `ERROR: cannot read … to extract Cilium pin` + exit 2.
    let (code, _stdout, stderr) = run(&["--repo-root=/nonexistent-hbird-repo-root"]);
    assert_eq!(code, Some(2), "pin-read failure is an exit-2 input error");
    assert!(stderr.starts_with("ERROR: cannot read"), "stderr: {stderr}");
}

#[test]
fn unknown_flag_exits_two() {
    // Twin printed `ERROR: unknown argument: …` and exit 2; clap prints
    // its own usage error but the EXIT CODE (the contract) matches.
    let (code, _stdout, stderr) = run(&["--bogus"]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("Usage"),
        "stderr: {stderr}"
    );
}

#[test]
fn help_exits_zero_and_mentions_strict() {
    let (code, stdout, _stderr) = run(&["--help"]);
    assert_eq!(code, Some(0));
    assert!(stdout.contains("--strict"), "stdout: {stdout}");
}

/// Live-repo smoke, matching the twin's first bats case: with no flags
/// at all the command reads the committed pins and emits a verdict.
///
/// `cargo test` runs with CWD = the crate directory, so the repo-root
/// walk-up finds `containers/k8s/` a few levels above. Skipped (rather
/// than failed) if that is ever not true, so the test never becomes a
/// false alarm about someone's checkout layout.
#[test]
fn live_repo_pins_are_compatible() {
    let (code, stdout, stderr) = run(&[]);
    if code == Some(2) {
        eprintln!("skipping: repo pins not reachable from CWD ({stderr})");
        return;
    }
    assert_eq!(code, Some(0), "committed pins must not be a mismatch");
    assert!(
        stdout.starts_with("OK: Cilium "),
        "expected an OK verdict from the committed pins. stdout: {stdout} stderr: {stderr}"
    );
}

/// Same, under `--strict`: the committed pins are what a pre-merge gate
/// would evaluate, so they must be gate-clean.
#[test]
fn live_repo_pins_pass_strict_mode() {
    let (code, _stdout, stderr) = run(&["--strict"]);
    if code == Some(2) {
        eprintln!("skipping: repo pins not reachable from CWD ({stderr})");
        return;
    }
    assert_eq!(
        code,
        Some(0),
        "committed pins must survive the strict gate. stderr: {stderr}"
    );
}

#[test]
fn dry_run_never_adopts_the_would_be_exit_code() {
    // A mismatch under --strict would exit 1; --dry-run reports that in
    // its plan but still exits 0 itself.
    let (code, stdout, _stderr) = run(&["--cilium=1.16.5", "--k8s=v1.31", "--strict", "--dry-run"]);
    assert_eq!(code, Some(0), "dry-run must always exit 0");
    assert!(
        stdout.contains("DRY-RUN would emit: WARN (mismatch) and exit 1"),
        "stdout: {stdout}"
    );
}

#[test]
fn dry_run_output_is_byte_stable_across_invocations() {
    let args = &["--cilium=1.17.16", "--k8s=v1.31", "--dry-run"];
    let (code_a, a, _) = run(args);
    let (code_b, b, _) = run(args);
    assert_eq!(code_a, Some(0));
    assert_eq!(code_b, Some(0));
    assert_eq!(a, b, "dry-run plan must be deterministic");
}
