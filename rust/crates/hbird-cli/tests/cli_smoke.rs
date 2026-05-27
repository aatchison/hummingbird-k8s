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
        "spawn-workers",
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

/// #289 landed the real deploy-cluster body (dry-run parity). `/dev/null`
/// is no longer a valid stand-in; the config parser will reject it.
/// Confirm the failure mode is config-parse (CP_NAME missing) rather
/// than the pre-#289 stub.
#[test]
fn deploy_cluster_against_empty_config_fails_at_config_parse() {
    let (status, _stdout, stderr) = run(&["deploy-cluster", "--config", "/dev/null"]);
    assert!(!status.success(), "deploy-cluster with /dev/null exited 0");
    // Stub error from PR #319 was:
    //   "deploy-cluster not yet implemented — tracked by #289"
    // Both halves must be absent for the stub to be truly gone.
    let stub_marker = stderr.contains("not yet implemented") && stderr.contains("#289");
    assert!(
        !stub_marker,
        "deploy-cluster should not be a stub anymore. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("CP_NAME")
            || stderr.contains("SSH_PUBKEY_FILE")
            || stderr.contains("required")
            || stderr.contains("missing"),
        "deploy-cluster should fail at config-parse on empty config. stderr:\n{stderr}"
    );
}

/// Same shape for `destroy-cluster`: real body landed in #289, the
/// failure should be a config-parse error rather than a stub.
#[test]
fn destroy_cluster_against_empty_config_fails_at_config_parse() {
    let (status, _stdout, stderr) = run(&["destroy-cluster", "--config", "/dev/null"]);
    assert!(!status.success());
    let stub_marker = stderr.contains("not yet implemented") && stderr.contains("#289");
    assert!(
        !stub_marker,
        "destroy-cluster should not be a stub anymore. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("CP_NAME")
            || stderr.contains("SSH_PUBKEY_FILE")
            || stderr.contains("required")
            || stderr.contains("missing"),
        "destroy-cluster should fail at config-parse on empty config. stderr:\n{stderr}"
    );
}

/// `hbird spawn-workers` is new in #289. Confirm it parses + reaches
/// the config-parse failure with the same shape as deploy/destroy.
#[test]
fn spawn_workers_against_empty_config_fails_at_config_parse() {
    let (status, _stdout, stderr) = run(&["spawn-workers", "--config", "/dev/null"]);
    assert!(!status.success());
    let stub_marker = stderr.contains("not yet implemented") && stderr.contains("#289");
    assert!(
        !stub_marker,
        "spawn-workers should not be a stub anymore. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("CP_NAME")
            || stderr.contains("SSH_PUBKEY_FILE")
            || stderr.contains("required")
            || stderr.contains("missing"),
        "spawn-workers should fail at config-parse on empty config. stderr:\n{stderr}"
    );
}

#[test]
fn update_cluster_dry_run_against_empty_config_fails_at_config_parse() {
    // #286 landed the real update-cluster body. `/dev/null` is no longer
    // a valid stand-in; the config parser will reject it. We pin that
    // the failure mode is "missing required fields" rather than the
    // pre-#286 "not yet implemented" stub.
    //
    // Round-2 CodeRabbit: tighten the assertion. The previous
    // `!contains("not yet implemented") || !contains("#286")` could
    // still pass if only ONE stub marker remained (the OR short-circuits).
    // Require BOTH the absence of a stub error AND the presence of a
    // config-parse failure signature.
    let (status, _stdout, stderr) = run(&["update-cluster", "--config", "/dev/null", "--dry-run"]);
    assert!(
        !status.success(),
        "update-cluster --dry-run with /dev/null exited 0"
    );
    // Stub error format from PR #319 was:
    //   "update-cluster not yet implemented — tracked by #286"
    // Both halves must be absent for the stub to be truly gone.
    let stub_marker = stderr.contains("not yet implemented") && stderr.contains("#286");
    assert!(
        !stub_marker,
        "update-cluster should not be a stub anymore — stderr still carries the pre-#286 stub \
         markers. stderr:\n{stderr}"
    );
    // Positive signal: the error should mention either the missing
    // required-field name (config parser) or the path that didn't
    // produce a valid config. `/dev/null` lacks CP_NAME, so the
    // hbird-config error chain will name it.
    assert!(
        stderr.contains("CP_NAME")
            || stderr.contains("SSH_PUBKEY_FILE")
            || stderr.contains("required")
            || stderr.contains("missing"),
        "update-cluster --dry-run with /dev/null should fail at config-parse with a \
         missing-required-field diagnostic; stderr:\n{stderr}"
    );
}

// PR #287 landed the verify-* implementations (Phase 2 of the Rust
// rewrite). The four `returns_not_yet_implemented` tests that lived
// here previously asserted the stub `Err("not yet implemented —
// tracked by #287")` surface; they were replaced with the four tests
// below, which assert that — invoked without any cluster reachability
// — each subcommand now reaches the real implementation and exits
// with the bash-twin-equivalent `resolve_cp_ip:` failure diagnostic.
//
// Live-mode parity (real cluster touched) is covered by the env-gated
// `tests/verify_*_live.rs` suite. These smoke tests stay hermetic.

#[test]
fn verify_encryption_no_cluster_surfaces_bash_resolve_cp_ip_diagnostic() {
    // No --config, no --kvm-host, no --cp-ip → reaches the implementation,
    // tries to resolve CP IP, hits the "no KVM_HOST" branch of
    // resolve_cp_ip_via_kvm_host(), bails with the bash-twin diagnostic.
    let (status, _stdout, stderr) = run(&["verify", "encryption"]);
    assert!(!status.success());
    assert!(
        stderr.contains("resolve_cp_ip"),
        "expected bash-twin resolve_cp_ip diagnostic, got:\n{stderr}"
    );
}

#[test]
fn verify_hardening_no_cluster_surfaces_bash_resolve_cp_ip_diagnostic() {
    let (status, _stdout, stderr) = run(&["verify", "hardening"]);
    assert!(!status.success());
    assert!(
        stderr.contains("resolve_cp_ip"),
        "expected bash-twin resolve_cp_ip diagnostic, got:\n{stderr}"
    );
}

#[test]
fn verify_app_deploy_no_cluster_surfaces_bash_resolve_cp_ip_diagnostic() {
    let (status, _stdout, stderr) = run(&["verify", "app-deploy"]);
    assert!(!status.success());
    assert!(
        stderr.contains("resolve_cp_ip"),
        "expected bash-twin resolve_cp_ip diagnostic, got:\n{stderr}"
    );
}

#[test]
fn verify_all_no_cluster_surfaces_bash_resolve_cp_ip_diagnostic() {
    let (status, _stdout, stderr) = run(&["verify", "all"]);
    assert!(!status.success());
    assert!(
        stderr.contains("resolve_cp_ip"),
        "expected bash-twin resolve_cp_ip diagnostic, got:\n{stderr}"
    );
}

/// #288 landed the real get-kubeconfig body. `/dev/null` is no longer
/// a valid stand-in; the config parser will reject it. Pin that the
/// failure mode is config-parse (CP_NAME missing) rather than a stub.
#[test]
fn get_kubeconfig_against_empty_config_fails_at_config_parse() {
    let (status, _stdout, stderr) = run(&["get-kubeconfig", "--config", "/dev/null"]);
    assert!(!status.success(), "get-kubeconfig with /dev/null exited 0");
    let stub_marker = stderr.contains("not yet implemented") && stderr.contains("#288");
    assert!(
        !stub_marker,
        "get-kubeconfig should not be a stub anymore. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("CP_NAME")
            || stderr.contains("SSH_PUBKEY_FILE")
            || stderr.contains("required")
            || stderr.contains("missing"),
        "get-kubeconfig should fail at config-parse on empty config. stderr:\n{stderr}"
    );
}

/// Same shape for `export-argocd`: real body landed in #288, the
/// failure should be a config-parse error rather than a stub.
#[test]
fn export_argocd_against_empty_config_fails_at_config_parse() {
    let (status, _stdout, stderr) = run(&["export-argocd", "--config", "/dev/null"]);
    assert!(!status.success(), "export-argocd with /dev/null exited 0");
    let stub_marker = stderr.contains("not yet implemented") && stderr.contains("#288");
    assert!(
        !stub_marker,
        "export-argocd should not be a stub anymore. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("CP_NAME")
            || stderr.contains("SSH_PUBKEY_FILE")
            || stderr.contains("required")
            || stderr.contains("missing"),
        "export-argocd should fail at config-parse on empty config. stderr:\n{stderr}"
    );
}

/// `nodes` with no config / env returns a missing-CP_NAME error
/// (real body landed in #288). The failure is at target-resolution,
/// not the old not-yet-implemented stub.
#[test]
fn nodes_without_config_or_env_fails_at_resolve() {
    // Scrub CONFIG/CP_NAME/CP_IP/KVM_HOST so env doesn't satisfy the
    // resolver. The fmt-subscriber init in main() is idempotent so the
    // env-clean doesn't affect tracing.
    let out = Command::new(hbird_bin())
        .args(["nodes"])
        .env_remove("CONFIG")
        .env_remove("CP_NAME")
        .env_remove("CP_IP")
        .env_remove("KVM_HOST")
        .output()
        .expect("failed to spawn hbird binary");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "nodes with no config exited 0");
    let stub_marker = stderr.contains("not yet implemented") && stderr.contains("#288");
    assert!(
        !stub_marker,
        "nodes should not be a stub anymore. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("CP_NAME") || stderr.contains("CP_IP"),
        "nodes should fail at target-resolve with a CP_NAME/CP_IP \
         diagnostic. stderr:\n{stderr}"
    );
}

/// `kubectl` with positional args reaches target-resolve, fails on
/// missing CP_NAME/CP_IP (real body landed in #288). Confirms variadic
/// args still parse cleanly through the new dispatch.
#[test]
fn kubectl_passthrough_resolves_targets_and_fails_at_cp_lookup() {
    let out = Command::new(hbird_bin())
        .args(["kubectl", "get", "pods", "-A"])
        .env_remove("CONFIG")
        .env_remove("CP_NAME")
        .env_remove("CP_IP")
        .env_remove("KVM_HOST")
        .output()
        .expect("failed to spawn hbird binary");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success());
    let stub_marker = stderr.contains("not yet implemented") && stderr.contains("#288");
    assert!(
        !stub_marker,
        "kubectl should not be a stub anymore. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("CP_NAME") || stderr.contains("CP_IP"),
        "kubectl should fail at target-resolve with a CP_NAME/CP_IP \
         diagnostic. stderr:\n{stderr}"
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

/// `kubectl` with NO positional args is rejected by our own dispatch
/// (real body landed in #288). The bash twin's `kubectl-k8s.sh` runs
/// `kubectl` with no args (which prints its own help); we surface a
/// clearer error because the SSH spawn is non-trivial. The variadic
/// shape is verified by [`kubectl_passthrough_resolves_targets_and_fails_at_cp_lookup`]
/// — if the variadic args ever stopped reaching `args.join(" ")`,
/// that test would fail at target-resolve before kubectl ever runs.
#[test]
fn kubectl_no_args_surfaces_clear_error() {
    let out = Command::new(hbird_bin())
        .args(["kubectl"])
        .env_remove("CONFIG")
        .env_remove("CP_NAME")
        .env_remove("CP_IP")
        .env_remove("KVM_HOST")
        .output()
        .expect("failed to spawn hbird binary");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success());
    assert!(
        stderr.contains("no positional args"),
        "expected explicit no-args error. stderr:\n{stderr}"
    );
}

/// `deploy-cluster`'s `--kvm-host` carries `env = "KVM_HOST"`. The bash
/// twin reads `KVM_HOST` from the env; the Rust binary must too, or
/// operator muscle memory breaks. Verify by parsing `--help` with the
/// env var set — clap surfaces env defaults in the help output, so a
/// missing `env =` binding regresses the help line. Pre-#289 the
/// stub-echo could observe the env var directly; now the body reaches
/// the config-parse failure before the echo, so `--help` is the most
/// hermetic place to assert the env binding survived.
#[test]
fn deploy_cluster_kvm_host_env_binding_present_in_help() {
    let out = Command::new(hbird_bin())
        .args(["deploy-cluster", "--help"])
        .env("KVM_HOST", "geary-via-env")
        .output()
        .expect("failed to spawn hbird binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "--help exited non-zero");
    assert!(
        stdout.contains("KVM_HOST"),
        "deploy-cluster --help should mention KVM_HOST env binding. stdout:\n{stdout}"
    );
}

/// Regression for #331: at the default tracing level (no `RUST_LOG`
/// override), an SSH-layer failure must NOT emit an `ERROR` span event.
/// Pre-#331 `hbird_ssh::Client::run_inner` carried `#[instrument(...
/// err(Debug))]` which auto-fired an ERROR event on every Err return —
/// even when the caller deliberately expected a non-zero exit (the
/// `verify-hardening` PSA-PASS condition). The demotion to a manual
/// `tracing::debug!` event means a passing verify run now leaves a
/// quiet stderr.
#[test]
fn verify_at_default_log_level_does_not_emit_error_span_on_ssh_failure() {
    let out = Command::new(hbird_bin())
        .args(["verify", "encryption"])
        .env("KVM_HOST", "geary-via-env")
        // No RUST_LOG → INFO+ only; debug event from run_inner stays
        // suppressed.
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to spawn hbird binary");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success());
    // The bash-twin `resolve_cp_ip:` error line is expected (operators
    // grep for it). What MUST NOT appear is a tracing `ERROR` event from
    // the SSH layer.
    assert!(
        !stderr.contains("ERROR run_inner"),
        "demoted err(Debug) regressed — ERROR run_inner span event fired \
         at default level. stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("ERROR hbird_ssh"),
        "demoted err(Debug) regressed — ERROR hbird_ssh event fired at \
         default level. stderr:\n{stderr}"
    );
}

/// Same env-var fallback check for the verify-* family, which reads
/// `KVM_HOST` *and* `CONFIG` from the env (matches the bash twins).
///
/// `RUST_LOG=hbird_ssh=debug` is set so the demoted `tracing::debug!`
/// event in `hbird_ssh::Client::run_inner` (#331) surfaces the `host`
/// field. Before #331 the wrapper carried `err(Debug)` which fired an
/// auto ERROR span event at the default INFO+ level — that was the
/// cosmetic bug #331 fixed (operators saw an ERROR on a passing
/// `verify-hardening` PSA check). The demotion means we now have to
/// opt into the debug stream to observe the host field; the test's
/// original intent — pinning that `KVM_HOST` env binding reaches the
/// SSH layer's `host=…` field — is preserved.
#[test]
fn verify_encryption_reads_kvm_host_from_env() {
    let out = Command::new(hbird_bin())
        .args(["verify", "encryption"])
        .env("KVM_HOST", "geary-via-env")
        .env("RUST_LOG", "hbird_ssh=debug")
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
/// `scripts/lib/ssh-wrap.sh`). Pre-#289 this verified by reading the
/// stub echo; post-#289 the real body fails at config-parse before any
/// echo. Assert the env binding is wired by spotting `HBIRD_REMOTE_NO_SUDO`
/// in `--help` instead — clap surfaces env-var bindings in the per-flag
/// help line.
#[test]
fn deploy_cluster_no_sudo_env_binding_present_in_help() {
    let out = Command::new(hbird_bin())
        .args(["deploy-cluster", "--help"])
        .env("HBIRD_REMOTE_NO_SUDO", "1")
        .output()
        .expect("failed to spawn hbird binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "--help exited non-zero");
    assert!(
        stdout.contains("HBIRD_REMOTE_NO_SUDO"),
        "deploy-cluster --help should mention HBIRD_REMOTE_NO_SUDO env binding. stdout:\n{stdout}"
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
    // Reaches the config-parse failure — confirms the alias is
    // accepted by clap rather than failing parse. (Pre-#288 stub
    // marker is gone; the body now fails at config-parse because
    // /dev/null has no CP_NAME.)
    assert!(!status.success());
    assert!(
        !stderr.contains("error: unexpected argument") && !stderr.contains("unrecognized"),
        "legacy --context-name alias failed clap parse. stderr:\n{stderr}"
    );
}
