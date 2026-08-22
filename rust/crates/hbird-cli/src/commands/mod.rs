//! Subcommand modules — one per operator-facing `Makefile` target.
//!
//! Each module exports an `…Args` struct (clap-derive) and a
//! `run(args) -> anyhow::Result<()>` function. [#283] landed these as
//! placeholder `Err(anyhow!("not yet implemented"))` stubs; every one has
//! since been given a real implementation, so no `run` returns a
//! not-implemented error.

pub mod clean_vms;
pub mod deploy_cluster;
pub mod destroy_cluster;
pub mod etcd;
pub mod export_argocd;
pub mod get_kubeconfig;
pub mod kube_bench;
pub mod kubectl;
pub mod nodes;
pub mod preflight;
pub mod spawn_workers;
pub mod switch_to_ghcr;
pub mod update_cluster;
pub mod verify;
