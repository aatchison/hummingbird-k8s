//! Phase 3 live-validate (cycle: `get-kubeconfig`) — `hbird get-kubeconfig`
//! parity vs bash twin `make get-kubeconfig`. Same shape as
//! `cycle_export_argocd_live.rs` but with the operator-friendly
//! defaults (context = `$CP_NAME`, not `hummingbird-$CP_NAME`).
//!
//! See [`cycle_export_argocd_live.rs`] for the #306/#307 divergence
//! rationale.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn env_var(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn env_with_hbird_fallback(key: &str) -> Option<String> {
    env_var(key).or_else(|| env_var(&format!("HBIRD_{key}")))
}

struct OutputCleanup(PathBuf);

impl Drop for OutputCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// Round-2 lens L5 HIGH: `#[ignore]` so the test reports IGNORED (not
// PASS) when not opted-in.
#[test]
#[ignore = "live cluster test; opt in with --ignored + HBIRD_LIVE_TEST=1"]
fn live_get_kubeconfig_emits_valid_kubeconfig() {
    if env::var("HBIRD_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!("HBIRD_LIVE_TEST!=1 — skipping live cluster test");
        return;
    }
    let cp_ip = env_with_hbird_fallback("CP_IP")
        .expect("CP_IP (or HBIRD_CP_IP) required when HBIRD_LIVE_TEST=1");
    let kvm_host = env_with_hbird_fallback("KVM_HOST")
        .expect("KVM_HOST (or HBIRD_KVM_HOST) required when HBIRD_LIVE_TEST=1");
    let cp_name = env_var("HBIRD_CP_NAME").unwrap_or_else(|| "hbird-cp1".to_string());
    let pubkey = env_var("HBIRD_SSH_PUBKEY")
        .or_else(|| {
            let home = env::var("HOME").ok()?;
            Some(format!("{home}/.ssh/id_ed25519.pub"))
        })
        .expect("could not derive SSH_PUBKEY_FILE for live config");

    let tmpdir = std::env::temp_dir();
    let out_path = tmpdir.join(format!("hbird-live-getkc-{}.yaml", std::process::id()));
    let _out_cleanup = OutputCleanup(out_path.clone());
    let cfg_path = tmpdir.join(format!("hbird-live-getkc-cfg-{}.conf", std::process::id()));
    let _cfg_cleanup = OutputCleanup(cfg_path.clone());
    fs::write(
        &cfg_path,
        format!(
            "CP_NAME={cp_name}\n\
             SSH_PUBKEY_FILE={pubkey}\n\
             KVM_HOST={kvm_host}\n\
             CP_IP={cp_ip}\n",
        ),
    )
    .expect("seed live config");

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_hbird"));
    let status = std::process::Command::new(&bin)
        .args(["get-kubeconfig", "--config"])
        .arg(&cfg_path)
        .arg("--output")
        .arg(&out_path)
        .arg("--force")
        .status()
        .expect("spawn hbird");
    assert!(status.success(), "hbird get-kubeconfig exited non-zero");

    let meta = fs::metadata(&out_path).expect("stat output");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "kubeconfig mode is not 0600 (got 0o{mode:o})");

    let written = fs::read_to_string(&out_path).expect("read output");
    assert!(
        written.lines().any(|l| l.starts_with("apiVersion:")),
        "missing apiVersion:"
    );
    assert!(
        written.lines().any(|l| l.starts_with("kind: Config")),
        "missing kind: Config"
    );
    // get-kubeconfig default context: $CP_NAME (NOT hummingbird-prefixed).
    assert!(
        written.contains(&format!("name: {cp_name}\n"))
            || written.contains(&format!("- name: {cp_name}\n")),
        "missing default context name '{cp_name}' in output:\n{written}"
    );
    assert!(
        !written.contains(&format!("hummingbird-{cp_name}")),
        "get-kubeconfig should NOT use the hummingbird- prefix (that's export-argocd's default)"
    );
}
