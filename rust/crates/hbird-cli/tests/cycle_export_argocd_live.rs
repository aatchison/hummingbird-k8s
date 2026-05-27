//! Phase 3 live-validate (cycle: `export-argocd`) — `hbird export-argocd`
//! parity vs bash twin `scripts/export-argocd.sh`.
//!
//! Environment-gated; see `cycle_nodes_live.rs` for the env contract.
//!
//! What this validates: against the live cluster, the Rust path
//! produces a kubeconfig file (mode 0600) that
//!
//! - starts with `apiVersion: v1`
//! - contains `kind: Config`
//! - has the requested context name (`hummingbird-$CP_NAME`)
//! - has the bash-twin-equivalent server URL
//!
//! Intentional divergence from bash:
//!
//! - **[#306]** — bash sets `PROXY_JUMP="${KVM_HOST:-}"` BEFORE
//!   sourcing CONFIG. Rust resolves AFTER, applying ProxyJump from
//!   CONFIG's `KVM_HOST` correctly even when the operator hasn't
//!   exported it. This test pins the Rust behavior; the bash twin
//!   either matches (operator exported `KVM_HOST` ahead of time) or
//!   diverges (workstation operator who pinned `KVM_HOST` in CONFIG
//!   only).
//! - **[#307]** — bash `cp_ssh() { ssh -t ... }` allocates a remote
//!   PTY; modern sudo can emit OSC session-start escapes that
//!   pollute the captured stdout. Rust uses non-TTY SSH (the
//!   `hbird_ssh` default `BatchMode=yes`), so no escapes. The Rust
//!   sanity-check rejects any OSC-prefixed stdout via the unit test
//!   `commands::export_argocd::tests::sanity_check_rejects_osc_prefix`.
//!
//! [#306]: https://github.com/aatchison/hummingbird-k8s/issues/306
//! [#307]: https://github.com/aatchison/hummingbird-k8s/issues/307

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

/// RAII guard: cleans up the test's output kubeconfig + any backup
/// (`.bak-…UTC` suffix) on Drop so a panicked assertion doesn't leave
/// a 0600 admin.conf-derived file in /tmp.
struct OutputCleanup(PathBuf);

impl Drop for OutputCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let parent = self.0.parent().unwrap_or_else(|| std::path::Path::new("."));
        let prefix = self
            .0
            .file_name()
            .map(|f| {
                let mut s = f.to_os_string();
                s.push(".bak-");
                s
            })
            .unwrap_or_default();
        if let Ok(rd) = fs::read_dir(parent) {
            for entry in rd.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && let Some(pfx) = prefix.to_str()
                    && name.starts_with(pfx)
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[test]
fn live_export_argocd_emits_valid_kubeconfig() {
    if env::var("HBIRD_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!("HBIRD_LIVE_TEST!=1 — skipping live cluster test");
        return;
    }
    let cp_ip = env_with_hbird_fallback("CP_IP")
        .expect("CP_IP (or HBIRD_CP_IP) required when HBIRD_LIVE_TEST=1");
    let kvm_host = env_with_hbird_fallback("KVM_HOST")
        .expect("KVM_HOST (or HBIRD_KVM_HOST) required when HBIRD_LIVE_TEST=1");
    let cp_name = env_var("HBIRD_CP_NAME").unwrap_or_else(|| "hbird-cp1".to_string());

    // We exercise the SSH path directly (mirroring what
    // `commands::export_argocd::fetch_admin_conf` does) rather than
    // shelling out to the binary — same rationale as
    // `cycle_nodes_live.rs`. The kubeconfig rewrite logic is fully
    // unit-tested in the module's `#[cfg(test)]` block; here we
    // prove the SSH + sudo + fetch + sanity-check path works against
    // the live CP.
    let opts = hbird_ssh::SshOptions::new(cp_ip.clone())
        .with_user("root")
        .with_proxy_jump(kvm_host.clone());
    let client = hbird_ssh::Client::new(opts);
    let out = client
        .run("sudo cat /etc/kubernetes/admin.conf")
        .expect("ssh-run failed");
    let raw = out.stdout_lossy();
    assert!(!raw.is_empty(), "admin.conf came back empty");

    // #307 regression: the fetched bytes must NOT be prefixed with
    // an OSC session-start escape. The first byte must be either
    // `a` (apiVersion:) or whitespace.
    let first_byte = raw.as_bytes().first().copied().unwrap_or(0);
    assert!(
        first_byte == b'a' || first_byte.is_ascii_whitespace(),
        "fetched admin.conf starts with non-printable byte 0x{first_byte:02x} \
         — likely #307 OSC pollution. raw starts: {:?}",
        &raw[..raw.len().min(48)],
    );

    // Sanity-check matches the bash twin's `grep -q '^apiVersion:'`
    // + `grep -q '^kind: Config'`.
    assert!(
        raw.lines().any(|l| l.starts_with("apiVersion:")),
        "no apiVersion: line"
    );
    assert!(
        raw.lines().any(|l| l.starts_with("kind: Config")),
        "no kind: Config line"
    );

    // Write the rewritten kubeconfig to a temp path. The full
    // round-trip + mode-0600 check exercises `write_atomic_0600`
    // and `rewrite_kubeconfig` together. We pre-populate the temp
    // path with a sentinel so the `--force` overwrite path is also
    // exercised (matches the bash twin's backup-on-overwrite shape).
    let tmpdir = std::env::temp_dir();
    let out_path = tmpdir.join(format!("hbird-live-export-{}.yaml", std::process::id()));
    fs::write(&out_path, b"sentinel\n").expect("seed sentinel");
    let _cleanup = OutputCleanup(out_path.clone());

    // Build a minimal CONFIG file and invoke the binary. The binary
    // is the integration boundary — we want to prove the full clap
    // + dispatch path works against the cluster, not just the SSH
    // layer (which `cycle_nodes_live` already covers).
    let pubkey = env_var("HBIRD_SSH_PUBKEY")
        .or_else(|| {
            let home = env::var("HOME").ok()?;
            Some(format!("{home}/.ssh/id_ed25519.pub"))
        })
        .expect("could not derive SSH_PUBKEY_FILE for live config");
    let cfg_path = tmpdir.join(format!("hbird-live-cfg-{}.conf", std::process::id()));
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
    let _cfg_cleanup = OutputCleanup(cfg_path.clone());

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_hbird"));
    let status = std::process::Command::new(&bin)
        .args(["export-argocd", "--config"])
        .arg(&cfg_path)
        .arg("--output")
        .arg(&out_path)
        .arg("--force")
        .status()
        .expect("spawn hbird");
    assert!(status.success(), "hbird export-argocd exited non-zero");

    // Output exists, mode 0600.
    let meta = fs::metadata(&out_path).expect("stat output");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "kubeconfig mode is not 0600 (got 0o{mode:o})");

    let written = fs::read_to_string(&out_path).expect("read output");
    assert!(
        written.lines().any(|l| l.starts_with("apiVersion:")),
        "missing apiVersion: in output:\n{written}"
    );
    assert!(
        written.lines().any(|l| l.starts_with("kind: Config")),
        "missing kind: Config in output:\n{written}"
    );
    let expected_ctx = format!("hummingbird-{cp_name}");
    assert!(
        written.contains(&expected_ctx),
        "missing expected context name '{expected_ctx}' in output"
    );
    let expected_server = format!("server: https://{cp_ip}:6443");
    assert!(
        written.contains(&expected_server),
        "missing expected server URL in output"
    );
}
