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

mod commands;

use commands::{
    deploy_cluster::DeployClusterArgs, destroy_cluster::DestroyClusterArgs,
    export_argocd::ExportArgocdArgs, get_kubeconfig::GetKubeconfigArgs, kubectl::KubectlArgs,
    nodes::NodesArgs, update_cluster::UpdateClusterArgs, verify::VerifyArgs,
};

/// Top-level entry point. Parse argv, dispatch to the chosen subcommand.
///
/// Returns `Result<()>` so a subcommand body can `?` its errors up; the
/// `Display` impl on `anyhow::Error` produces operator-readable output
/// without a stack trace by default (the bash twins are similarly
/// quiet). Set `RUST_BACKTRACE=1` to opt into one.
fn main() -> Result<()> {
    // TODO(#286): wrap parse + dispatch in a `tracing_subscriber` init +
    // top-level `tracing::info_span!("hbird", subcommand = …)` once the
    // workspace picks a logging crate. The hbird-ssh / hbird-virt
    // `tracing::instrument` seams already exist — main is the missing
    // half. (PR #319 round-2 review L8 DISCUSS deferred to #286.)
    let cli = Cli::parse();
    cli.command.run()
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
    /// Dispatch to the chosen subcommand. Each delegate currently returns
    /// `Err(anyhow!("not yet implemented — tracked by #XXX"))`.
    fn run(self) -> Result<()> {
        match self {
            Command::DeployCluster(args) => commands::deploy_cluster::run(args),
            Command::DestroyCluster(args) => commands::destroy_cluster::run(args),
            Command::UpdateCluster(args) => commands::update_cluster::run(args),
            Command::Verify(args) => commands::verify::run(args),
            Command::GetKubeconfig(args) => commands::get_kubeconfig::run(args),
            Command::ExportArgocd(args) => commands::export_argocd::run(args),
            Command::Nodes(args) => commands::nodes::run(args),
            Command::Kubectl(args) => commands::kubectl::run(args),
        }
    }
}
