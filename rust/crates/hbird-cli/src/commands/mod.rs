//! Subcommand modules — one per operator-facing `Makefile` target.
//!
//! Each module exports an `…Args` struct (clap-derive) and a
//! `run(args) -> anyhow::Result<()>` function. For [#283] every `run`
//! returns `Err(anyhow!("not yet implemented — tracked by #XXX"))`
//! pointing at the sub-issue that owns the real implementation.

pub mod deploy_cluster;
pub mod destroy_cluster;
pub mod export_argocd;
pub mod get_kubeconfig;
pub mod kubectl;
pub mod nodes;
pub mod update_cluster;
pub mod verify;
