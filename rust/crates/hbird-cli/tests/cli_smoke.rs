//! Smoke tests for the `hbird` CLI's clap command tree (#283).
//!
//! These tests don't run real subcommands — they only assert that:
//!
//! 1. The CLI parses (a malformed command tree fails at compile time
//!    via `clap-derive`, so a build is itself the strongest check; we
//!    pin `--help` shape too so a future refactor doesn't quietly drop
//!    a subcommand).
//! 2. Every operator-facing subcommand listed in the `Makefile` exists.
//! 3. Every subcommand currently returns the documented
//!    "not yet implemented — tracked by #XXX" error.
//!
//! For (3) we exec the binary via `std::process::Command` and assert on
//! its stderr — that's the same path an operator's shell would take, so
//! the test catches breakage in `main()`'s error-formatting path too.

use std::path::PathBuf;
use std::process::Command;

/// Locate the `hbird` binary the test harness built for us. Cargo
/// exposes it via the `CARGO_BIN_EXE_<name>` env var at compile time;
/// `env!` lifts it into a `&'static str`.
fn hbird_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

/// Run `hbird <args…>` and return (status, stdout, stderr) as strings.
fn run(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let out = Command::new(hbird_bin())
        .args(args)
        .output()
        .expect("failed to spawn hbird binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status, stdout, stderr)
}

#[test]
fn help_lists_every_operator_facing_subcommand() {
    let (status, stdout, _stderr) = run(&["--help"]);
    assert!(status.success(), "hbird --help exited non-zero");

    // Mirror of the operator-facing Makefile targets — see Makefile's
    // .PHONY block. Any divergence here means either the Makefile or
    // the Rust binary drifted out from under the operator-mental-model
    // contract (#279). If you're adding a new operator-facing target,
    // add it to both.
    for sub in [
        "deploy-cluster",
        "destroy-cluster",
        "update-cluster",
        "verify",
        "get-kubeconfig",
        "export-argocd",
        "nodes",
        "kubectl",
    ] {
        assert!(
            stdout.contains(sub),
            "hbird --help missing subcommand `{sub}`. Output was:\n{stdout}"
        );
    }
}

#[test]
fn verify_help_lists_all_three_verifiers_plus_all() {
    let (status, stdout, _stderr) = run(&["verify", "--help"]);
    assert!(status.success(), "hbird verify --help exited non-zero");

    // Mirror of the `verify-*` Makefile targets: encryption, hardening,
    // app-deploy, all.
    for sub in ["encryption", "hardening", "app-deploy", "all"] {
        assert!(
            stdout.contains(sub),
            "hbird verify --help missing sub `{sub}`. Output was:\n{stdout}"
        );
    }
}

#[test]
fn version_flag_works() {
    // `--version` is wired by `#[command(version)]` in main.rs — this
    // catches a regression where someone removes that attribute.
    let (status, stdout, _stderr) = run(&["--version"]);
    assert!(status.success(), "hbird --version exited non-zero");
    assert!(
        stdout.contains("hbird"),
        "version output missing binary name: {stdout}"
    );
}

#[test]
fn deploy_cluster_returns_not_yet_implemented() {
    // Run with a dummy config path — the command should fail before
    // touching the filesystem because the body is the
    // not-yet-implemented stub. If a future PR (#289) wires real
    // behavior, this test will fail and the implementer can swap it
    // for a real integration test (or delete it).
    let (status, _stdout, stderr) = run(&["deploy-cluster", "--config", "/dev/null"]);
    assert!(!status.success(), "deploy-cluster stub exited zero");
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#289"),
        "deploy-cluster stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn destroy_cluster_returns_not_yet_implemented() {
    let (status, _stdout, stderr) = run(&["destroy-cluster", "--config", "/dev/null"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#289"),
        "destroy-cluster stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn update_cluster_returns_not_yet_implemented() {
    let (status, _stdout, stderr) = run(&["update-cluster", "--config", "/dev/null", "--dry-run"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#286"),
        "update-cluster stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn verify_encryption_returns_not_yet_implemented() {
    // `--config` is optional for verify-* (matches the bash twins),
    // so we can invoke without any flag and still reach the stub.
    let (status, _stdout, stderr) = run(&["verify", "encryption"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#287"),
        "verify encryption stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn verify_hardening_returns_not_yet_implemented() {
    let (status, _stdout, stderr) = run(&["verify", "hardening"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#287"),
        "verify hardening stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn verify_app_deploy_returns_not_yet_implemented() {
    let (status, _stdout, stderr) = run(&["verify", "app-deploy"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#287"),
        "verify app-deploy stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn verify_all_returns_not_yet_implemented() {
    let (status, _stdout, stderr) = run(&["verify", "all"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#287"),
        "verify all stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn get_kubeconfig_returns_not_yet_implemented() {
    let (status, _stdout, stderr) = run(&["get-kubeconfig", "--config", "/dev/null"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#288"),
        "get-kubeconfig stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn export_argocd_returns_not_yet_implemented() {
    let (status, _stdout, stderr) = run(&["export-argocd", "--config", "/dev/null"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#288"),
        "export-argocd stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn nodes_returns_not_yet_implemented() {
    let (status, _stdout, stderr) = run(&["nodes"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#288"),
        "nodes stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn kubectl_passthrough_returns_not_yet_implemented() {
    // The pass-through accepts arbitrary args — make sure clap doesn't
    // intercept them and they pile into args[].
    let (status, _stdout, stderr) = run(&["kubectl", "get", "pods", "-A"]);
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented") && stderr.contains("#288"),
        "kubectl stub error missing tracker. stderr:\n{stderr}"
    );
}

#[test]
fn unknown_subcommand_fails_to_parse() {
    // clap exits 2 on parse error. Don't pin the exact code, just
    // confirm it isn't 0 — different clap versions have used 2 / 64.
    let (status, _stdout, stderr) = run(&["definitely-not-a-real-subcommand"]);
    assert!(!status.success());
    assert!(
        stderr.contains("error") || stderr.contains("unrecognized"),
        "unknown-subcommand path didn't print clap-shaped error. stderr:\n{stderr}"
    );
}
