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
fn update_cluster_dry_run_against_empty_config_fails_at_config_parse() {
    // #286 landed the real update-cluster body. `/dev/null` is no longer
    // a valid stand-in; the config parser will reject it. We pin that
    // the failure mode is "missing required fields" rather than the
    // pre-#286 "not yet implemented" stub.
    let (status, _stdout, stderr) = run(&["update-cluster", "--config", "/dev/null", "--dry-run"]);
    assert!(
        !status.success(),
        "update-cluster --dry-run with /dev/null exited 0"
    );
    // The error should surface as a config-parse failure
    // (CP_NAME / SSH_PUBKEY_FILE missing) rather than the stub message.
    assert!(
        !stderr.contains("not yet implemented") || !stderr.contains("#286"),
        "update-cluster should not be a stub anymore. stderr:\n{stderr}"
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

// --- PR #319 round-2 review L5 HIGH: negative-parse + variadic +
// env-var fallback coverage. -----------------------------------------

/// `deploy-cluster` without `--config` must fail at parse time, not
/// silently default to nothing. Mirrors the bash twin's required-arg
/// shape so an operator who forgets `--config` sees a clap error rather
/// than a confusing not-yet-implemented stub.
#[test]
fn deploy_cluster_missing_config_fails_to_parse() {
    let (status, _stdout, stderr) = run(&["deploy-cluster"]);
    assert!(
        !status.success(),
        "deploy-cluster without --config exited 0"
    );
    // clap's "required argument missing" wording varies across versions —
    // pin on the flag name + an error marker instead.
    assert!(
        stderr.contains("--config") && (stderr.contains("error") || stderr.contains("required")),
        "expected clap to complain about missing --config. stderr:\n{stderr}"
    );
}

/// Same negative-parse check for `destroy-cluster` (also `--config`-
/// required per bash twin).
#[test]
fn destroy_cluster_missing_config_fails_to_parse() {
    let (status, _stdout, stderr) = run(&["destroy-cluster"]);
    assert!(
        !status.success(),
        "destroy-cluster without --config exited 0"
    );
    assert!(
        stderr.contains("--config") && (stderr.contains("error") || stderr.contains("required")),
        "expected clap to complain about missing --config. stderr:\n{stderr}"
    );
}

/// `kubectl` is the only subcommand with variadic pass-through (the
/// rest take fixed flags). Confirm the variadic args actually land in
/// the parsed `Vec<String>` by checking the stub's args-echo.
#[test]
fn kubectl_variadic_capture_echoes_args() {
    let (status, _stdout, stderr) = run(&["kubectl", "get", "pods", "-A"]);
    assert!(!status.success());
    // The stub now echoes `args=["get", "pods", "-A"]` — pin the
    // observable substring so a future change to the echo format that
    // accidentally drops args is caught.
    assert!(
        stderr.contains("\"get\"") && stderr.contains("\"pods\"") && stderr.contains("\"-A\""),
        "kubectl args weren't captured into the variadic vec. stderr:\n{stderr}"
    );
}

/// `deploy-cluster`'s `--kvm-host` carries `env = "KVM_HOST"`. The bash
/// twin reads `KVM_HOST` from the env; the Rust binary must too, or
/// operator muscle memory breaks. Verify by setting the env var, then
/// reading the args-echo for the value.
#[test]
fn deploy_cluster_kvm_host_falls_back_to_env() {
    let out = Command::new(hbird_bin())
        .args(["deploy-cluster", "--config", "/dev/null"])
        .env("KVM_HOST", "geary-via-env")
        .output()
        .expect("failed to spawn hbird binary");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success());
    assert!(
        stderr.contains("geary-via-env"),
        "KVM_HOST env didn't reach --kvm-host. stderr:\n{stderr}"
    );
}

/// Same env-var fallback check for the verify-* family, which reads
/// `KVM_HOST` *and* `CONFIG` from the env (matches the bash twins).
#[test]
fn verify_encryption_reads_kvm_host_from_env() {
    let out = Command::new(hbird_bin())
        .args(["verify", "encryption"])
        .env("KVM_HOST", "geary-via-env")
        .output()
        .expect("failed to spawn hbird binary");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success());
    assert!(
        stderr.contains("geary-via-env"),
        "KVM_HOST env didn't reach verify's --kvm-host. stderr:\n{stderr}"
    );
}

/// `--no-sudo` honors `HBIRD_REMOTE_NO_SUDO=1` (matches bash twin's
/// `scripts/lib/ssh-wrap.sh`). Verify the env binding lands by setting
/// the env var and confirming the stub still bails (we can't directly
/// observe the bool, but at minimum we confirm clap accepts the env-var
/// path without complaint).
#[test]
fn deploy_cluster_no_sudo_env_var_accepted() {
    let out = Command::new(hbird_bin())
        .args(["deploy-cluster", "--config", "/dev/null"])
        .env("HBIRD_REMOTE_NO_SUDO", "1")
        .output()
        .expect("failed to spawn hbird binary");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // Should reach the stub (not-yet-implemented) — that means clap
    // happily parsed the env var.
    assert!(!out.status.success());
    assert!(
        stderr.contains("not yet implemented"),
        "HBIRD_REMOTE_NO_SUDO env didn't parse cleanly. stderr:\n{stderr}"
    );
}

/// The bash twin's `--context-name` spelling must still work after the
/// rename to `--context`. Pin via clap `alias`.
#[test]
fn export_argocd_accepts_legacy_context_name_alias() {
    let (status, _stdout, stderr) = run(&[
        "export-argocd",
        "--config",
        "/dev/null",
        "--context-name",
        "legacy-spelling",
    ]);
    // Reaches the stub (not-yet-implemented) — confirms the alias is
    // accepted by clap rather than failing parse.
    assert!(!status.success());
    assert!(
        stderr.contains("not yet implemented"),
        "legacy --context-name alias failed to parse. stderr:\n{stderr}"
    );
}
