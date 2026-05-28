//! `hbird` — operator CLI for the hummingbird-k8s Rust rewrite.
//!
//! # What this binary does today
//!
//! This crate defines the clap-derive command tree that mirrors the
//! operator-facing `Makefile` targets (see `../../../Makefile`):
//!
//! ```text
//! hbird deploy-cluster      <-> make deploy-cluster
//! hbird destroy-cluster     <-> make destroy-cluster
//! hbird update-cluster      <-> make update-cluster / make update-workers / make update-node
//! hbird verify encryption   <-> make verify-encryption
//! hbird verify hardening    <-> make verify-hardening
//! hbird verify app-deploy   <-> make verify-app-deploy
//! hbird verify all          <-> make verify-all
//! hbird get-kubeconfig      <-> make get-kubeconfig
//! hbird export-argocd       <-> make export-argocd
//! hbird nodes               <-> make nodes
//! hbird kubectl             <-> make kubectl
//! ```
//!
//! For [#283] every subcommand returns
//! `Err(anyhow!("not yet implemented — tracked by #XXX"))`. Real bodies
//! land in [#286] (update-cluster), [#287] (verify-*), [#288] (kubeconfig
//! + export-argocd), and [#289] (deploy / destroy / spawn).
//!
//! # Operator-mental-model contract
//!
//! The flag names, help text, and exit codes are chosen to match the
//! bash twins (`../scripts/*.sh`) so an operator who already knows the
//! `make` surface can switch to the Rust binary without re-learning the
//! flag set. See the epic ([#279]) for the full contract.
//!
//! [#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
//! [#283]: https://github.com/aatchison/hummingbird-k8s/issues/283
//! [#286]: https://github.com/aatchison/hummingbird-k8s/issues/286
//! [#287]: https://github.com/aatchison/hummingbird-k8s/issues/287
//! [#288]: https://github.com/aatchison/hummingbird-k8s/issues/288
//! [#289]: https://github.com/aatchison/hummingbird-k8s/issues/289

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info_span;
use tracing_subscriber::EnvFilter;

mod commands;
mod cp_kubectl;
mod cp_resolve;

use commands::{
    deploy_cluster::DeployClusterArgs, destroy_cluster::DestroyClusterArgs,
    export_argocd::ExportArgocdArgs, get_kubeconfig::GetKubeconfigArgs, kubectl::KubectlArgs,
    nodes::NodesArgs, spawn_workers::SpawnWorkersArgs, update_cluster::UpdateClusterArgs,
    verify::VerifyArgs,
};

/// Top-level entry point. Parse argv, dispatch to the chosen subcommand.
///
/// Returns `Result<()>` so a subcommand body can `?` its errors up; the
/// `Display` impl on `anyhow::Error` produces operator-readable output
/// without a stack trace by default (the bash twins are similarly
/// quiet). Set `RUST_BACKTRACE=1` to opt into one.
///
/// # Logging
///
/// `tracing_subscriber` is initialized at the very top so spans /
/// events emitted by `hbird-ssh`, `hbird-virt`, and the subcommand
/// bodies have a destination. The default filter is `info`; operators
/// can override via `RUST_LOG` (e.g. `RUST_LOG=hbird_ssh=debug` to see
/// per-host SSH command spans). Output goes to stderr so the existing
/// `update-cluster` log lines (`[update-cluster] …` via `println!` to
/// stdout — pinned byte-for-byte by the dry-run fixtures from
/// PR #321) keep flowing through stdout untouched. The formatter is
/// configured without timestamps/target/ansi-color so any tracing
/// output reads like the bash-twin prefix style.
fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let _span = info_span!("hbird", subcommand = cli.command.name()).entered();
    cli.command.run()
}

/// Install the global `tracing_subscriber` for the lifetime of the
/// process.
///
/// Done early in `main` (before clap parsing) so any panic / error
/// emitted by argument parsing also has somewhere to surface its span
/// events. The filter honors `RUST_LOG`; absent that, it defaults to
/// `info`, which keeps the foundation crates quiet during normal runs
/// (their `#[tracing::instrument]` spans are recorded at `info`+ but
/// most events emitted inside them are at `debug`).
///
/// Writer is stderr to keep stdout dedicated to the bash-twin-shaped
/// `[update-cluster] …` lines that `commands::update_cluster::log`
/// still emits via `println!`. Switching subscribers to stdout would
/// corrupt the dry-run fixtures from PR #321, which compare captured
/// stdout byte-for-byte against pinned snapshots.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` instead of `init` so a future test or library that
    // happens to install a subscriber doesn't panic the second caller
    // with `SetGlobalDefaultError`. Round-2 lens L5 MEDIUM — binaries
    // that may be re-entered under test should always use try_init.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(true)
        // Emit a debug event when each `#[tracing::instrument]` span
        // closes, carrying span duration. Round-2 lens L8 MEDIUM —
        // operators chasing "drain took 47s" diagnostics now see the
        // close events under `RUST_LOG=hbird_cli=debug` (silent at
        // default `info`).
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .try_init();
}

/// `hbird` — operator CLI for hummingbird-k8s.
///
/// The Rust rewrite of the bash scripts under `scripts/`. Mirrors the
/// operator-facing `Makefile` targets one-for-one; see each subcommand's
/// `--help` for flag details.
#[derive(Debug, Parser)]
#[command(
    name = "hbird",
    version,
    about = "hummingbird-k8s operator CLI (Rust rewrite of scripts/*.sh; tracked by #279)",
    long_about = "Operator CLI for hummingbird-k8s.\n\
        \n\
        Mirrors the operator-facing Makefile targets (deploy-cluster,\n\
        destroy-cluster, update-cluster, verify-*, get-kubeconfig,\n\
        export-argocd, nodes, kubectl). The bash equivalents under\n\
        scripts/*.sh remain canonical until each Rust subcommand reaches\n\
        behavioral parity with its bash twin (operator-mental-model\n\
        contract — see epic #279)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// All operator-facing subcommands, in the order they appear in the
/// `Makefile`'s help output (PR-readable convention so an operator
/// reading `hbird --help` sees the same shape as `make help`).
#[derive(Debug, Subcommand)]
enum Command {
    /// Deploy a hybrid bib+cloud-init cluster from a `cluster.local.conf`.
    ///
    /// Bash twin: `scripts/deploy-cluster.sh` (via `make deploy-cluster`).
    /// Implementation tracked by #289.
    DeployCluster(DeployClusterArgs),

    /// Tear down a cluster (destroys VMs + qcow2s + seed ISOs).
    ///
    /// Bash twin: `scripts/destroy-cluster.sh` (via `make destroy-cluster`).
    /// Implementation tracked by #289.
    DestroyCluster(DestroyClusterArgs),

    /// Spawn N additional workers against an existing cluster.
    ///
    /// Bash twin: `scripts/spawn-workers.sh` (via `make spawn-workers COUNT=N`).
    /// Implementation tracked by #289; live execution slice tracked by #335.
    SpawnWorkers(SpawnWorkersArgs),

    /// Rolling bootc upgrade across CP + workers with bootID + daemonset gates.
    ///
    /// Bash twin: `scripts/update-cluster.sh` (via `make update-cluster`,
    /// `make update-workers`, `make update-node`). Implementation tracked
    /// by #286.
    UpdateCluster(UpdateClusterArgs),

    /// Post-deploy verifiers (encryption / hardening / app-deploy / all).
    ///
    /// Bash twins: `scripts/verify-encryption.sh`,
    /// `scripts/verify-hardening.sh`, `scripts/verify-app-deploy.sh`
    /// (via `make verify-encryption`, `make verify-hardening`,
    /// `make verify-app-deploy`, `make verify-all`). Implementation
    /// tracked by #287.
    Verify(VerifyArgs),

    /// Fetch a kubeconfig from the cluster, defaulting to a kubectl-shaped
    /// context name (companion to `export-argocd`; daily-use sibling).
    ///
    /// Bash twin: `scripts/export-argocd.sh` invoked with
    /// `--context-name=$CP_NAME` defaults (via `make get-kubeconfig`).
    /// Implementation tracked by #288.
    GetKubeconfig(GetKubeconfigArgs),

    /// Export an ArgoCD-registerable kubeconfig (`hummingbird-`-prefixed
    /// context).
    ///
    /// Bash twin: `scripts/export-argocd.sh` (via `make export-argocd`).
    /// Implementation tracked by #288.
    ExportArgocd(ExportArgocdArgs),

    /// `kubectl get nodes` via the SSH-tunnel wrapper.
    ///
    /// Bash twin: `scripts/kubectl-k8s.sh get nodes` (via `make nodes`).
    /// Implementation tracked by #288.
    Nodes(NodesArgs),

    /// kubectl pass-through via the SSH-tunnel wrapper.
    ///
    /// Bash twin: `scripts/kubectl-k8s.sh "$@"` (via `make kubectl`).
    /// Implementation tracked by #288.
    Kubectl(KubectlArgs),
}

impl Command {
    /// Bash-twin-style name of the top-level subcommand
    /// (`deploy-cluster`, `update-cluster`, `verify`, …). Used as the
    /// `subcommand=…` field on the root tracing span so structured-log
    /// consumers can filter / group by operator action. The strings
    /// mirror the `Makefile` target names rather than the Rust variant
    /// names (`deploy-cluster` over `DeployCluster`) so an operator
    /// reading logs sees the same word they typed.
    fn name(&self) -> &'static str {
        match self {
            Command::DeployCluster(_) => "deploy-cluster",
            Command::DestroyCluster(_) => "destroy-cluster",
            Command::SpawnWorkers(_) => "spawn-workers",
            Command::UpdateCluster(_) => "update-cluster",
            Command::Verify(_) => "verify",
            Command::GetKubeconfig(_) => "get-kubeconfig",
            Command::ExportArgocd(_) => "export-argocd",
            Command::Nodes(_) => "nodes",
            Command::Kubectl(_) => "kubectl",
        }
    }

    /// Dispatch to the chosen subcommand. Each delegate currently returns
    /// `Err(anyhow!("not yet implemented — tracked by #XXX"))`.
    fn run(self) -> Result<()> {
        match self {
            Command::DeployCluster(args) => commands::deploy_cluster::run(args),
            Command::DestroyCluster(args) => commands::destroy_cluster::run(args),
            Command::SpawnWorkers(args) => commands::spawn_workers::run(args),
            Command::UpdateCluster(args) => commands::update_cluster::run(args),
            Command::Verify(args) => commands::verify::run(args),
            Command::GetKubeconfig(args) => commands::get_kubeconfig::run(args),
            Command::ExportArgocd(args) => commands::export_argocd::run(args),
            Command::Nodes(args) => commands::nodes::run(args),
            Command::Kubectl(args) => commands::kubectl::run(args),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Smoke: the clap-derive command tree still parses after the tracing
    /// init was wired in. Catches simple regressions like a missing
    /// trait import dragging in a name collision on `Command`.
    #[test]
    fn cli_command_tree_builds() {
        Cli::command().debug_assert();
    }

    /// Every top-level `Command` variant maps to a stable,
    /// bash-twin-shaped name. The strings are what end up in the root
    /// tracing span's `subcommand=…` field, so structured-log consumers
    /// can filter / group by them — asserting each variant's mapping
    /// here means a future variant that forgets to extend
    /// `Command::name` (which would default to "" via Rust's `match`
    /// non-exhaustiveness compile error) gets caught at test time
    /// before it lands an empty-string subcommand in production logs.
    #[test]
    fn command_names_match_bash_twin() {
        // Parsed via clap with the same minimal flags `cli_smoke` uses —
        // every subcommand accepts `--config /dev/null` (or no flag at
        // all, since `CONFIG` env is optional). Kubectl needs a `--`
        // pass-through; verify needs a sub-sub.
        let cases: &[(&[&str], &str)] = &[
            (
                &["hbird", "deploy-cluster", "--config", "/dev/null"],
                "deploy-cluster",
            ),
            (
                &["hbird", "destroy-cluster", "--config", "/dev/null"],
                "destroy-cluster",
            ),
            (
                &["hbird", "spawn-workers", "--config", "/dev/null"],
                "spawn-workers",
            ),
            (
                &["hbird", "update-cluster", "--config", "/dev/null"],
                "update-cluster",
            ),
            (
                &["hbird", "verify", "all", "--config", "/dev/null"],
                "verify",
            ),
            (
                &["hbird", "get-kubeconfig", "--config", "/dev/null"],
                "get-kubeconfig",
            ),
            (
                &["hbird", "export-argocd", "--config", "/dev/null"],
                "export-argocd",
            ),
            (&["hbird", "nodes", "--config", "/dev/null"], "nodes"),
            (
                &[
                    "hbird",
                    "kubectl",
                    "--config",
                    "/dev/null",
                    "--",
                    "get",
                    "nodes",
                ],
                "kubectl",
            ),
        ];
        for (argv, expected) in cases {
            let cli = Cli::try_parse_from(*argv)
                .unwrap_or_else(|e| panic!("parse failed for {expected}: {e}"));
            assert_eq!(
                cli.command.name(),
                *expected,
                "Command::name mismatch for argv {argv:?}",
            );
        }
    }
}
