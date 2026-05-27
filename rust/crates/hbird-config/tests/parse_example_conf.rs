//! Integration test that parses the real `cluster.example.conf` shipped
//! at the repo root. This is the load-bearing regression: the operator
//! docs (`docs/deploy-cluster.md`, `README.md`) point at that file as
//! the canonical template, so the Rust parser MUST accept whatever ships
//! there verbatim — even when future PRs add new optional knobs to it.
//!
//! If this test starts failing after an edit to `cluster.example.conf`,
//! the right fix is to teach `hbird-config` about the new field (and
//! update `ClusterConfig`), NOT to special-case the example file.

use std::path::PathBuf;

/// Locate `cluster.example.conf` from `CARGO_MANIFEST_DIR`. The crate
/// lives at `rust/crates/hbird-config/`, so the repo root is three
/// `..`s up.
fn example_conf_path() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .join("..")
        .join("..")
        .join("..")
        .join("cluster.example.conf")
}

#[test]
fn parses_repo_root_cluster_example_conf() {
    let path = example_conf_path();
    assert!(
        path.exists(),
        "cluster.example.conf must exist at repo root: {}",
        path.display()
    );

    let cfg = hbird_config::parse(&path).unwrap_or_else(|e| {
        panic!(
            "cluster.example.conf must parse cleanly (see {}): {e}",
            path.display()
        )
    });

    // Spot-check the fields the example sets explicitly. If the example
    // file changes these, the change is intentional and this test should
    // be updated alongside it.
    assert_eq!(cfg.kvm_host.as_deref(), Some("thenewhost"));
    assert_eq!(cfg.cp_name, "hbird-cp1");
    assert_eq!(
        cfg.worker_names,
        Some(vec!["hbird-w1".to_string(), "hbird-w2".to_string()])
    );
    assert_eq!(cfg.ssh_pubkey_file, "$HOME/.ssh/id_ed25519.pub");
    assert_eq!(cfg.image_source, "ghcr");
    assert_eq!(cfg.ghcr_tag, "latest");
    assert_eq!(cfg.enable_cloud_init, 1);
    assert!(cfg.auto_update_cp);
    assert!(cfg.switch_to_ghcr);

    // Commented-out fields fall through to the bash-twin defaults:
    assert_eq!(cfg.cp_memory, 8192);
    assert_eq!(cfg.cp_vcpus, 4);
    assert_eq!(cfg.worker_memory, 4096);
    assert_eq!(cfg.worker_vcpus, 2);
    assert_eq!(cfg.pool_dir, "/var/lib/libvirt/images");
    assert!(!cfg.run_verify);
    assert_eq!(cfg.cp_ip, None);
    assert_eq!(cfg.worker_ips, None);
    assert_eq!(cfg.bootc_update_schedule, None);
    assert_eq!(cfg.bootc_update_repo_k8s, None);
    assert_eq!(cfg.bootc_update_repo_worker, None);

    // #316: the canonical example file MUST parse with zero warnings.
    // If a future PR adds a new knob to cluster.example.conf without
    // teaching ClusterConfig about it, this test catches the drift
    // (unknown-key warning fires) before the divergence ships.
    assert!(
        cfg.warnings.is_empty(),
        "cluster.example.conf must parse with zero warnings — got: {:?}",
        cfg.warnings
    );
}

#[test]
fn resolved_worker_names_matches_explicit_array() {
    // The example sets WORKER_NAMES explicitly to (hbird-w1 hbird-w2), so
    // resolved_worker_names() must return that exact list — NOT the
    // legacy `${CP_NAME}-w1/-w2` default. Catches a regression where
    // resolution accidentally overrides explicit settings.
    let cfg = hbird_config::parse(example_conf_path()).expect("parses");
    assert_eq!(
        cfg.resolved_worker_names(),
        vec!["hbird-w1".to_string(), "hbird-w2".to_string()]
    );
}
