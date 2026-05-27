//! `hbird verify <sub>` — bash twins: `scripts/verify-encryption.sh`,
//! `scripts/verify-hardening.sh`, `scripts/verify-app-deploy.sh`.
//!
//! Phase 2 of the operator-CLI Rust rewrite (epic [#279], tracked by
//! [#287]). The three bash twins take their input through env vars
//! (`CONFIG`, `CP_NAME`, `KVM_HOST`, `KUBECTL`) rather than positional
//! args; the Rust shape promotes them to flags while keeping the
//! env-var fallbacks via clap's `env = …` attribute.
//!
//! # Mapping from bash twin → Rust function
//!
//! Each Rust verifier function carries the bash twin's grep-anchor
//! name to keep the operator-mental-model contract from the epic.
//!
//! | bash twin                          | Rust fn                              |
//! |------------------------------------|--------------------------------------|
//! | `verify-encryption.sh` remote path | [`run_verify_encryption`]            |
//! | `verify-hardening.sh::check 1/3`   | [`check_podsecurity_rejects_privileged`] |
//! | `verify-hardening.sh::check 2/3`   | [`check_apiserver_audit_log_nonempty`] |
//! | `verify-hardening.sh::check 3/4`   | [`check_kubelet_protect_kernel_defaults`] |
//! | `verify-hardening.sh::check 4/4`   | [`check_kubelet_rotate_certificates`] |
//! | `verify-app-deploy.sh` body        | [`run_verify_app_deploy`]            |
//!
//! # Live execution
//!
//! All four verifiers use the shared [`crate::cp_kubectl`] shim, which
//! runs `kubectl --kubeconfig=/etc/kubernetes/admin.conf` on the CP via
//! `ssh -J $KVM_HOST root@$CP_IP`. The bash twins reach the same
//! kubectl via the `scripts/kubectl-k8s.sh` wrapper (port-forward +
//! local `kubectl`); the Rust path collapses the wrapper into the SSH
//! call directly — functionally equivalent (kubeconfig is sourced from
//! `/etc/kubernetes/admin.conf` on the CP itself, no tunnel needed).
//!
//! Bash error wording is preserved verbatim where the bash twin emits
//! operator-grepped strings (`PASS:`, `FAIL:`, `OK:`, `[verify-*]`
//! prefix lines).
//!
//! [#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
//! [#287]: https://github.com/aatchison/hummingbird-k8s/issues/287

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::cp_kubectl::{CpTarget, cp_kubectl_raw, cp_kubectl_with_stdin_lenient, cp_ssh_lenient};
use hbird_config::ClusterConfig;

/// Top-level `hbird verify` — dispatches to one of four sub-subcommands.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    #[command(subcommand)]
    pub command: VerifySubcommand,
}

/// The four `verify-*` Makefile targets, plus `all` (which chains the
/// other three in sequence — bash twin: `make verify-all`).
#[derive(Debug, Subcommand)]
pub enum VerifySubcommand {
    /// Verify etcd encryption-at-rest on the control plane.
    ///
    /// Bash twin: `scripts/verify-encryption.sh`.
    Encryption(VerifyCommonArgs),

    /// Verify PSA + audit + kubelet protect-kernel-defaults.
    ///
    /// Bash twin: `scripts/verify-hardening.sh`.
    Hardening(VerifyCommonArgs),

    /// End-to-end PSA-restricted nginx + pod-to-pod connectivity test.
    ///
    /// Bash twin: `scripts/verify-app-deploy.sh`.
    AppDeploy(VerifyCommonArgs),

    /// Run all three verifiers in sequence (encryption → hardening →
    /// app-deploy). Bash twin: `make verify-all`.
    All(VerifyCommonArgs),
}

/// Shared flags for every `verify` sub-subcommand. The bash twins all
/// take the same set of env vars; one struct keeps the surface uniform.
#[derive(Debug, Args)]
pub struct VerifyCommonArgs {
    /// Path to `cluster.local.conf`. The bash scripts read `CP_NAME` +
    /// `KVM_HOST` from this file; the Rust shape keeps the same lookup.
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,

    /// libvirt domain name of the control plane. Overrides the value
    /// pulled from `--config`.
    #[arg(long, value_name = "NAME", env = "CP_NAME")]
    pub cp_name: Option<String>,

    /// SSH alias of the KVM host. Overrides the value pulled from
    /// `--config` / `KVM_HOST` env.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// Explicit CP IP, bypassing config lookup + libvirt resolution.
    /// Bash twin: `CP_IP` env var (`verify-encryption.sh:75`,
    /// `verify-hardening.sh:131`).
    #[arg(long, value_name = "IP", env = "CP_IP")]
    pub cp_ip: Option<String>,

    /// Path to a `kubectl` binary or wrapper. The Rust path ignores
    /// this — kubectl runs on the CP via SSH (the wrapper exists for
    /// the bash path only). Kept on the surface for env-var
    /// compatibility with operators who set `KUBECTL=…` from their
    /// shell rc. Bash twin: `KUBECTL` env var.
    #[arg(long, value_name = "PATH", env = "KUBECTL")]
    pub kubectl: Option<PathBuf>,
}

/// Stdout log helper. Mirrors `lib/build-common.sh::log` invoked under
/// `setup_logging "[verify-<sub>]"`. The bash twin writes to stderr;
/// we match (Rust callers preserve grep parity).
fn log(prefix: &str, line: &str) {
    eprintln!("{prefix} {line}");
}

// ---- Plan: resolved CP target + log prefix --------------------------------

/// Resolved verify-time target: CP IP + optional KVM-host ProxyJump.
/// Built once at the top of [`dispatch`] from clap args + sourced
/// config, then read-only for the rest of the run.
#[derive(Debug, Clone)]
struct VerifyPlan {
    /// Forwarded into [`CpTarget`] for every shim call.
    target: CpTarget,
    /// CP libvirt-domain name (operator-visible diagnostic; not used
    /// by the SSH chain itself).
    cp_name: String,
}

impl VerifyPlan {
    /// Build the plan from the clap-parsed common args. Source order
    /// for each field (highest precedence first):
    ///
    /// 1. Explicit flag (`--cp-name`, `--kvm-host`, `--cp-ip`) — clap
    ///    also honors the matching env var.
    /// 2. `--config` file (parsed via `hbird_config::parse`).
    /// 3. None → either default (`CP_NAME=hummingbird-k8s` per bash
    ///    twin) or a fail-with-diagnostic if no fallback exists.
    ///
    /// Bash twin: the env-var precedence sourced from `CONFIG` then
    /// shell env, applied across `verify-encryption.sh:82-83`,
    /// `verify-hardening.sh:62-72`, `verify-app-deploy.sh:61-78`.
    fn from_args(args: &VerifyCommonArgs) -> Result<Self> {
        let config: Option<ClusterConfig> =
            if let Some(path) = args.config.as_ref() {
                Some(hbird_config::parse(path).with_context(|| {
                    format!("verify: failed to parse --config {}", path.display())
                })?)
            } else {
                None
            };

        // CP_NAME resolution — explicit flag > config > bash twin default.
        let cp_name = args
            .cp_name
            .clone()
            .or_else(|| config.as_ref().map(|c| c.cp_name.clone()))
            .unwrap_or_else(|| "hummingbird-k8s".to_string());

        // KVM_HOST resolution — explicit flag > config.
        let kvm_host = args
            .kvm_host
            .clone()
            .or_else(|| config.as_ref().and_then(|c| c.kvm_host.clone()))
            .filter(|s| !s.is_empty());

        // CP_IP resolution — explicit flag > config > virsh-via-ssh.
        // Bash twin: `verify-hardening.sh:131-148`.
        let cp_ip = if let Some(ip) = args.cp_ip.clone().filter(|s| !s.is_empty()) {
            ip
        } else if let Some(ip) = config
            .as_ref()
            .and_then(|c| c.cp_ip.clone())
            .filter(|s| !s.is_empty())
        {
            ip
        } else {
            resolve_cp_ip_via_kvm_host(&cp_name, kvm_host.as_deref())?
        };

        Ok(Self {
            target: CpTarget { cp_ip, kvm_host },
            cp_name,
        })
    }
}

/// Resolve a CP IP by SSH'ing the KVM host and running
/// `virsh -c qemu:///system domifaddr <cp_name>`. Mirrors bash twin's
/// `resolve_cp_ip` (`lib/build-common.sh:524`).
///
/// # Errors
///
/// Surfaces a verbatim copy of the bash twin's failure diagnostic when
/// every resolution path is exhausted, so operators grepping for
/// `resolve_cp_ip:` keep getting hits across both languages.
fn resolve_cp_ip_via_kvm_host(cp_name: &str, kvm_host: Option<&str>) -> Result<String> {
    let Some(host) = kvm_host else {
        // Bash twin (`lib/build-common.sh:583`) emits this exact
        // wording. Operators grep for `resolve_cp_ip:` to find the
        // failure across logs from either language.
        bail!(
            "resolve_cp_ip: could not resolve IPv4 for domain '{cp_name}'. \
             Set CP_IP=<ip> in your CONFIG, or export KVM_HOST=<ssh-alias> \
             so we can query libvirt on the KVM host via SSH (workstation \
             operators without local libvirt), or run from the KVM host \
             with virsh installed."
        );
    };
    // ssh "$KVM_HOST" "virsh -c qemu:///system domifaddr <vm>"
    let ssh_opts = hbird_ssh::SshOptions::new(host.to_string());
    let client = hbird_ssh::Client::new(ssh_opts);
    let cmd = format!(
        "virsh -c qemu:///system domifaddr {}",
        shell_single_quote(cp_name)
    );
    let raw = match client.run(&cmd) {
        Ok(out) => out.stdout_lossy(),
        Err(hbird_ssh::Error::NonZeroExit { stdout, .. }) => stdout,
        Err(e) => bail!("resolve_cp_ip: ssh-to-KVM-host failed for `{cmd}` against {host}: {e}"),
    };
    if let Some(ip) = parse_first_domifaddr_ipv4(&raw) {
        Ok(ip)
    } else {
        bail!(
            "resolve_cp_ip: could not resolve IPv4 for domain '{cp_name}'. \
             Set CP_IP=<ip> in your CONFIG, or export KVM_HOST=<ssh-alias> \
             so we can query libvirt on the KVM host via SSH (workstation \
             operators without local libvirt), or run from the KVM host \
             with virsh installed."
        )
    }
}

/// Wrap `s` in single quotes for safe inclusion in a remote shell
/// command. Mirrors bash twin's `'${vm}'` quoting in `resolve_cp_ip`.
/// Embedded single quotes are escaped via the standard
/// `'\'` + `'` + `'` dance.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Parse the first IPv4 lease from `virsh domifaddr` output.
/// Mirrors bash twin's awk pipeline:
/// `awk '/ipv4/{split($4,a,"/"); print a[1]; exit}'`
/// (`lib/build-common.sh:482`).
fn parse_first_domifaddr_ipv4(raw: &str) -> Option<String> {
    for line in raw.lines() {
        if !line.contains("ipv4") {
            continue;
        }
        // 4-field split on whitespace; take field[3] (0-indexed) → CIDR
        // → split on `/` → first segment.
        let cols: Vec<&str> = line.split_whitespace().collect();
        if let Some(cidr) = cols.get(3)
            && let Some(ip) = cidr.split('/').next()
            && !ip.is_empty()
        {
            return Some(ip.to_string());
        }
    }
    None
}

// ---- verify encryption ----------------------------------------------------

/// `hbird verify encryption` — bash twin: `scripts/verify-encryption.sh`
/// (remote mode, lines 75-105).
///
/// The bash twin in remote mode just SSHes into `root@$CP_IP` and runs
/// the baked-in copy at `/usr/libexec/verify-encryption.sh` on the CP
/// (with `EXPECTED_PREFIX` forwarded so rotation-time callers keep
/// their stricter `aesgcm:v1:<new-key>:` assertion). The Rust path is
/// the same: ssh + invoke the on-image script.
///
/// # Errors
///
/// Surfaces the bash twin's `[verify-encryption] FAIL: ...` diagnostic
/// (whose exact wording operators grep for) and returns non-zero exit
/// when the on-image verifier returns non-zero.
#[tracing::instrument(level = "debug", skip(plan), fields(cp_ip = %plan.target.cp_ip), err(Debug))]
fn run_verify_encryption(plan: &VerifyPlan) -> Result<()> {
    let prefix = "[verify-encryption]";
    tracing::debug!(cp_name = %plan.cp_name, cp_ip = %plan.target.cp_ip, "verify-encryption start");
    log(
        prefix,
        &format!(
            "remote mode: ssh root@{}{}",
            plan.target.cp_ip,
            plan.target
                .kvm_host
                .as_deref()
                .map(|h| format!(" via {h}"))
                .unwrap_or_default(),
        ),
    );

    // Honor an EXPECTED_PREFIX in the operator's env so rotation tests
    // can keep their stricter `aesgcm:v1:<keyname>:` assertion. Bash
    // twin defaults to `k8s:enc:aesgcm:`; we forward only when set.
    let expected_prefix = std::env::var("EXPECTED_PREFIX").ok();
    let remote_cmd = if let Some(p) = expected_prefix.as_deref() {
        // Single-quote the prefix for safe interpolation into the
        // remote `/bin/sh -c`. EXPECTED_PREFIX is operator-controlled
        // env; the bash twin does the same single-quote wrap.
        format!(
            "EXPECTED_PREFIX={} /usr/libexec/verify-encryption.sh",
            shell_single_quote(p),
        )
    } else {
        "/usr/libexec/verify-encryption.sh".to_string()
    };

    let out = cp_ssh_lenient(&plan.target, &remote_cmd).with_context(|| {
        format!(
            "verify-encryption: ssh to root@{} failed",
            plan.target.cp_ip
        )
    })?;
    // The on-image script logs to stderr with the `[verify-encryption]`
    // prefix already; forward both streams to operator stderr verbatim.
    if !out.stderr.is_empty() {
        for line in out.stderr.lines() {
            eprintln!("{line}");
        }
    }
    if !out.stdout.is_empty() {
        for line in out.stdout.lines() {
            println!("{line}");
        }
    }
    if !out.success {
        bail!(
            "verify-encryption: remote /usr/libexec/verify-encryption.sh \
             exited non-zero on {}",
            plan.target.cp_ip
        );
    }
    Ok(())
}

// ---- verify hardening -----------------------------------------------------

/// Check 1/4 of `verify-hardening.sh` (lines 151-179): apply a
/// privileged pod manifest into `default`; PodSecurity Admission must
/// reject it with `violates PodSecurity` on stderr.
///
/// Returns `true` when the rejection fires (PASS), `false` otherwise
/// (FAIL). SSH-transport failures bubble up via `?` — only k8s-layer
/// outcomes return bool.
#[tracing::instrument(level = "debug", skip(plan), fields(cp_ip = %plan.target.cp_ip), err(Debug))]
fn check_podsecurity_rejects_privileged(plan: &VerifyPlan) -> Result<bool> {
    let prefix = "[verify-hardening]";
    log(
        prefix,
        "check 1/3: PodSecurity restricted rejects a privileged pod",
    );
    // Bash twin's exact manifest (`verify-hardening.sh:154-167`).
    let manifest = b"apiVersion: v1
kind: Pod
metadata:
  name: verify-hardening-privileged-probe
  namespace: default
spec:
  hostPID: true
  containers:
  - name: x
    image: busybox
    securityContext:
      privileged: true
";
    let out = cp_kubectl_with_stdin_lenient(&plan.target, "apply -f -", manifest)?;
    // Best-effort cleanup in case it somehow got admitted (it shouldn't).
    // Bash twin: line 170. We ignore the result.
    let _ = cp_kubectl_raw(
        &plan.target,
        "-n default delete pod verify-hardening-privileged-probe \
         --ignore-not-found=true --wait=false",
    );
    let combined = format!("{}\n{}", out.stdout, out.stderr);
    if combined.contains("violates PodSecurity") {
        log(prefix, "  PASS: privileged pod rejected by PodSecurity");
        Ok(true)
    } else {
        log(
            prefix,
            "  FAIL: privileged pod was not rejected by PodSecurity",
        );
        log(prefix, &format!("  apiserver output: {combined}"));
        Ok(false)
    }
}

/// Check 2/4 of `verify-hardening.sh` (lines 181-197): assert the
/// apiserver audit log on the CP host is non-empty. Tries the
/// post-#50 path first (`/var/log/kubernetes/k8s-audit.log`), then
/// the legacy path (`/var/log/k8s-audit.log`).
#[tracing::instrument(level = "debug", skip(plan), fields(cp_ip = %plan.target.cp_ip), err(Debug))]
fn check_apiserver_audit_log_nonempty(plan: &VerifyPlan) -> Result<bool> {
    let prefix = "[verify-hardening]";
    log(
        prefix,
        "check 2/3: apiserver audit log is non-empty on the CP host",
    );
    // Bash twin's verbatim command (`verify-hardening.sh:187-190`).
    let cmd = "for p in /var/log/kubernetes/k8s-audit.log /var/log/k8s-audit.log; do \
       if [ -s \"$p\" ]; then printf \"OK %s\\n\" \"$p\"; exit 0; fi; \
       done; exit 1";
    let out = cp_ssh_lenient(&plan.target, cmd)?;
    if out.success
        && let Some(line) = out.stdout.lines().next()
    {
        let path = line.strip_prefix("OK ").unwrap_or(line);
        log(prefix, &format!("  PASS: audit log present ({path})"));
        return Ok(true);
    }
    log(prefix, "  FAIL: audit log missing or empty");
    log(
        prefix,
        "  expected /var/log/kubernetes/k8s-audit.log (post-#50) \
         or /var/log/k8s-audit.log",
    );
    Ok(false)
}

/// Check 3/4 of `verify-hardening.sh` (lines 199-211): assert kubelet
/// is running with `--protect-kernel-defaults=true`. Bash twin's
/// `ps -ef | grep | head -1`.
#[tracing::instrument(level = "debug", skip(plan), fields(cp_ip = %plan.target.cp_ip), err(Debug))]
fn check_kubelet_protect_kernel_defaults(plan: &VerifyPlan) -> Result<bool> {
    let prefix = "[verify-hardening]";
    log(
        prefix,
        "check 3/4: kubelet running with --protect-kernel-defaults=true",
    );
    let cmd = "ps -ef | grep -- '--protect-kernel-defaults=true' | grep -v grep | head -1";
    let out = cp_ssh_lenient(&plan.target, cmd)?;
    // Bash twin keys off "stdout non-empty" — `ps -ef | grep | head`
    // exits 0 even when nothing matches (grep matched nothing, head's
    // 0). The verifier semantics are "did we see a kubelet argv with
    // the flag?", which is the stdout content.
    if out.success && !out.stdout.trim().is_empty() {
        log(prefix, "  PASS: kubelet has --protect-kernel-defaults=true");
        Ok(true)
    } else {
        log(
            prefix,
            "  FAIL: kubelet is not running with --protect-kernel-defaults=true",
        );
        Ok(false)
    }
}

/// Check 4/4 of `verify-hardening.sh` (lines 213-224): assert kubelet
/// is running with `--rotate-certificates=true`. Bash twin's
/// `ps -ef | grep | head -1`. (#121)
#[tracing::instrument(level = "debug", skip(plan), fields(cp_ip = %plan.target.cp_ip), err(Debug))]
fn check_kubelet_rotate_certificates(plan: &VerifyPlan) -> Result<bool> {
    let prefix = "[verify-hardening]";
    log(
        prefix,
        "check 4/4: kubelet running with --rotate-certificates=true (#121)",
    );
    let cmd = "ps -ef | grep -- '--rotate-certificates=true' | grep -v grep | head -1";
    let out = cp_ssh_lenient(&plan.target, cmd)?;
    if out.success && !out.stdout.trim().is_empty() {
        log(prefix, "  PASS: kubelet has --rotate-certificates=true");
        Ok(true)
    } else {
        log(
            prefix,
            "  FAIL: kubelet is not running with --rotate-certificates=true",
        );
        Ok(false)
    }
}

/// `hbird verify hardening` — bash twin: `scripts/verify-hardening.sh`.
///
/// Runs all four checks (PSA / audit log / kubelet protect-kernel /
/// kubelet rotate-certs) regardless of intermediate failures, mirrors
/// bash twin's tracking-then-summary shape so operators see partial
/// state on FAIL.
///
/// # Errors
///
/// Returns `Err` only for SSH-transport failures (the individual
/// checks return `bool`). A FAIL on any check produces a non-`Err`
/// "one or more checks FAILED" exit-1 (matches bash twin lines
/// 236-241).
#[tracing::instrument(level = "debug", skip(plan), fields(cp_ip = %plan.target.cp_ip), err(Debug))]
fn run_verify_hardening(plan: &VerifyPlan) -> Result<()> {
    let prefix = "[verify-hardening]";
    // Bash twin emits `CP_IP=$CP_IP` verbatim (verify-hardening.sh:149).
    // Keep the wording so operators grepping the log find the same
    // string. cp_name is preserved on the plan for diagnostic logging
    // when RUST_LOG=hbird_cli=debug is set; it isn't in the bash
    // twin's stdout line so we don't add it here.
    log(prefix, &format!("CP_IP={}", plan.target.cp_ip));
    tracing::debug!(cp_name = %plan.cp_name, cp_ip = %plan.target.cp_ip, "verify-hardening start");

    let ps_ok = check_podsecurity_rejects_privileged(plan)?;
    let audit_ok = check_apiserver_audit_log_nonempty(plan)?;
    let kubelet_ok = check_kubelet_protect_kernel_defaults(plan)?;
    let rotate_ok = check_kubelet_rotate_certificates(plan)?;

    let label = |ok: bool| if ok { "PASS" } else { "FAIL" };
    // Bash twin's summary block (lines 230-234) uses bare printf — no
    // log prefix on the summary header. We match.
    println!();
    println!("[verify-hardening] summary");
    println!("  PodSecurity restricted    : {}", label(ps_ok));
    println!("  apiserver audit log       : {}", label(audit_ok));
    println!("  kubelet protect-kernel    : {}", label(kubelet_ok));
    println!("  kubelet rotate-certs      : {}", label(rotate_ok));

    if ps_ok && audit_ok && kubelet_ok && rotate_ok {
        println!("[verify-hardening] all checks PASSED");
        Ok(())
    } else {
        println!("[verify-hardening] one or more checks FAILED");
        bail!("verify-hardening: one or more checks FAILED");
    }
}

// ---- verify app-deploy ----------------------------------------------------

/// `hbird verify app-deploy` — bash twin:
/// `scripts/verify-app-deploy.sh`.
///
/// End-to-end smoke test:
/// 1. Create `smoketest-<epoch>` namespace.
/// 2. Apply a PSA-restricted-compliant nginx Deployment + Service.
/// 3. Wait up to 2m for `deployment/nginx` to become Available.
/// 4. Run a PSA-restricted busybox probe pod that `wget`s
///    `http://nginx:8080` and assert the response contains
///    "Welcome to nginx".
///
/// Cleanup (delete the namespace) is best-effort and runs even on
/// failure — mirrors bash twin's `trap cleanup EXIT`.
#[tracing::instrument(level = "debug", skip(plan), fields(cp_ip = %plan.target.cp_ip), err(Debug))]
fn run_verify_app_deploy(plan: &VerifyPlan) -> Result<()> {
    let prefix = "[verify-app-deploy]";
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ns = format!("smoketest-{epoch}");
    tracing::debug!(cp_name = %plan.cp_name, cp_ip = %plan.target.cp_ip, namespace = %ns, "verify-app-deploy start");

    // RAII cleanup: best-effort `kubectl delete ns <ns>` on Drop,
    // mirroring bash twin's `trap cleanup EXIT`. Captures `plan` by
    // reference; we own the namespace string so the closure outlives
    // any intermediate `?` return.
    struct NamespaceCleanup<'a> {
        target: &'a CpTarget,
        ns: String,
    }
    impl Drop for NamespaceCleanup<'_> {
        fn drop(&mut self) {
            eprintln!(
                "[verify-app-deploy] cleanup: deleting namespace {}",
                self.ns
            );
            let _ = cp_kubectl_raw(
                self.target,
                &format!("delete ns {} --wait=false --ignore-not-found", self.ns),
            );
        }
    }
    let _cleanup = NamespaceCleanup {
        target: &plan.target,
        ns: ns.clone(),
    };

    log(prefix, &format!("creating namespace {ns}"));
    let _ = cp_kubectl_raw(&plan.target, &format!("create ns {ns}"))
        .with_context(|| format!("verify-app-deploy: failed to create namespace {ns}"))?;

    log(
        prefix,
        &format!("applying nginx Deployment + Service in {ns}"),
    );
    // Bash twin's verbatim manifest (`verify-app-deploy.sh:99-138`).
    let manifest = b"apiVersion: apps/v1
kind: Deployment
metadata:
  name: nginx
spec:
  replicas: 1
  selector:
    matchLabels:
      app: nginx
  template:
    metadata:
      labels:
        app: nginx
    spec:
      automountServiceAccountToken: false
      containers:
      - name: nginx
        image: nginxinc/nginx-unprivileged:stable
        ports:
        - containerPort: 8080
        securityContext:
          runAsNonRoot: true
          allowPrivilegeEscalation: false
          capabilities:
            drop: [ALL]
          seccompProfile:
            type: RuntimeDefault
---
apiVersion: v1
kind: Service
metadata:
  name: nginx
spec:
  selector:
    app: nginx
  ports:
  - port: 8080
    targetPort: 8080
";
    let apply_out =
        cp_kubectl_with_stdin_lenient(&plan.target, &format!("apply -n {ns} -f -"), manifest)?;
    if !apply_out.success {
        bail!(
            "verify-app-deploy: kubectl apply failed.\n--- stdout ---\n{}\n--- stderr ---\n{}",
            apply_out.stdout,
            apply_out.stderr
        );
    }

    log(
        prefix,
        "waiting up to 2m for deployment/nginx to become Available",
    );
    let wait_out = cp_kubectl_raw(
        &plan.target,
        &format!("-n {ns} wait --for=condition=available --timeout=2m deployment/nginx"),
    );
    if wait_out.is_err() {
        log(prefix, "FAIL: deployment/nginx did not become Available");
        log(prefix, "  recent events:");
        if let Ok(events) = cp_kubectl_raw(
            &plan.target,
            &format!("-n {ns} get events --sort-by=.lastTimestamp"),
        ) {
            eprint!("{}", events.stdout_lossy());
        }
        bail!("verify-app-deploy: deployment/nginx did not become Available");
    }

    log(
        prefix,
        "probing http://nginx:8080 from an in-cluster busybox pod",
    );
    // The bash twin's `kubectl run --rm -i --restart=Never` blocks
    // until the pod terminates and surfaces its stdout. We replicate
    // by running the same kubectl from the CP. `--rm` removes the pod
    // on exit; `-i` keeps stdin attached so `wget` doesn't fail with
    // a closed pipe. The overrides JSON applies the PSA-restricted
    // security context.
    //
    // Note: `--overrides=` carries a JSON blob which contains `:`,
    // `{`, `}` etc. — those aren't in `cp_kubectl_raw`'s
    // metachar-deny list (`; & | ` `` ` `` `\n \r $(`), so the
    // command passes through. We single-quote the JSON for the
    // remote `/bin/sh -c`.
    // JSON kept on a single line so `cp_kubectl_with_stdin_lenient`'s
    // metacharacter guard (which rejects raw `\n`) doesn't trip — the
    // entire `--overrides=…` value flows through as one argv element
    // that we single-quote for the remote /bin/sh -c. Functionally
    // identical to the bash twin's heredoc; JSON parsing is
    // whitespace-insensitive. See `crate::cp_kubectl::reject_shell_metachars`.
    let overrides = r#"{"spec":{"automountServiceAccountToken":false,"containers":[{"name":"probe","image":"busybox:stable","stdin":true,"command":["sh","-c","wget -qO- http://nginx:8080"],"securityContext":{"runAsNonRoot":true,"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"seccompProfile":{"type":"RuntimeDefault"}}}]}}"#;
    let probe_cmd = format!(
        "run probe -n {ns} --rm -i --restart=Never --image=busybox:stable --overrides={}",
        shell_single_quote(overrides),
    );
    let probe_out = cp_kubectl_with_stdin_lenient(&plan.target, &probe_cmd, b"")?;
    let combined = format!("{}\n{}", probe_out.stdout, probe_out.stderr);
    if !probe_out.success {
        log(prefix, "FAIL: probe pod exited non-zero");
        eprintln!("{combined}");
        bail!("verify-app-deploy: probe pod exited non-zero");
    }
    if combined.contains("Welcome to nginx") {
        log(
            prefix,
            "PASS: nginx returned the welcome page over ClusterIP",
        );
    } else {
        log(prefix, "FAIL: probe did not see the nginx welcome page");
        log(prefix, "  probe output:");
        eprintln!("{combined}");
        bail!("verify-app-deploy: probe did not see the nginx welcome page");
    }

    log(prefix, "verify-app-deploy: PASS");
    Ok(())
}

// ---- dispatch -------------------------------------------------------------

/// Dispatch — builds the [`VerifyPlan`] from the common args, then
/// delegates to the chosen verifier. For `all`, chains
/// encryption → hardening → app-deploy and short-circuits on the first
/// failure (bash twin: `make verify-all` — recipe is the three Make
/// targets chained, which inherit Make's first-failure semantics).
pub fn run(args: VerifyArgs) -> Result<()> {
    match args.command {
        VerifySubcommand::Encryption(c) => {
            let plan = VerifyPlan::from_args(&c)?;
            run_verify_encryption(&plan)
        }
        VerifySubcommand::Hardening(c) => {
            let plan = VerifyPlan::from_args(&c)?;
            run_verify_hardening(&plan)
        }
        VerifySubcommand::AppDeploy(c) => {
            let plan = VerifyPlan::from_args(&c)?;
            run_verify_app_deploy(&plan)
        }
        VerifySubcommand::All(c) => {
            let plan = VerifyPlan::from_args(&c)?;
            run_verify_encryption(&plan)?;
            run_verify_hardening(&plan)?;
            run_verify_app_deploy(&plan)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_single_quote_no_quotes_inside() {
        assert_eq!(shell_single_quote("hbird-cp1"), "'hbird-cp1'");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quote() {
        // Bash idiom: '\'' (close, escape, open) — `o'brien` → `'o'\''brien'`.
        assert_eq!(shell_single_quote("o'brien"), "'o'\\''brien'");
    }

    #[test]
    fn parse_first_domifaddr_ipv4_picks_first_ipv4_row() {
        // Synthetic virsh-domifaddr output. Real shape per
        // lib/build-common.sh:482's awk pipeline expectation.
        let raw = " Name       MAC address          Protocol     Address\n\
                   -------------------------------------------------------------------------------\n\
                    vnet0      52:54:00:11:22:33    ipv4         192.168.122.42/24\n";
        assert_eq!(
            parse_first_domifaddr_ipv4(raw),
            Some("192.168.122.42".to_string())
        );
    }

    #[test]
    fn parse_first_domifaddr_ipv4_returns_none_on_empty() {
        assert_eq!(parse_first_domifaddr_ipv4(""), None);
    }

    #[test]
    fn parse_first_domifaddr_ipv4_returns_none_when_no_ipv4_row() {
        // `ipv6` lease only — should return None (bash twin's awk
        // `/ipv4/` matches nothing).
        let raw = " vnet0      52:54:00:11:22:33    ipv6         fe80::1/64\n";
        assert_eq!(parse_first_domifaddr_ipv4(raw), None);
    }

    #[test]
    fn from_args_explicit_cp_ip_wins_over_config() {
        // Two-tier precedence: explicit --cp-ip beats a config file's
        // CP_IP. No SSH attempted because explicit IP is present.
        let args = VerifyCommonArgs {
            config: None,
            cp_name: Some("hbird-cp1".into()),
            kvm_host: Some("geary".into()),
            cp_ip: Some("10.0.0.1".into()),
            kubectl: None,
        };
        let plan = VerifyPlan::from_args(&args).expect("explicit cp_ip resolves");
        assert_eq!(plan.target.cp_ip, "10.0.0.1");
        assert_eq!(plan.target.kvm_host.as_deref(), Some("geary"));
        assert_eq!(plan.cp_name, "hbird-cp1");
    }

    #[test]
    fn from_args_missing_cp_ip_and_no_kvm_host_fails_with_bash_diagnostic() {
        // No config, no explicit IP, no KVM_HOST → fail with the
        // bash-twin's `resolve_cp_ip:` wording.
        let args = VerifyCommonArgs {
            config: None,
            cp_name: Some("hbird-cp1".into()),
            kvm_host: None,
            cp_ip: None,
            kubectl: None,
        };
        let err =
            VerifyPlan::from_args(&args).expect_err("missing cp_ip + no kvm_host should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("resolve_cp_ip"),
            "expected bash twin's resolve_cp_ip wording: {msg}"
        );
    }

    #[test]
    fn from_args_uses_default_cp_name_when_unset() {
        // No --cp-name, no --config → fall back to bash twin default
        // "hummingbird-k8s". Pair with --cp-ip so we don't hit virsh
        // resolution.
        let args = VerifyCommonArgs {
            config: None,
            cp_name: None,
            kvm_host: None,
            cp_ip: Some("10.0.0.1".into()),
            kubectl: None,
        };
        let plan = VerifyPlan::from_args(&args).expect("default cp_name resolves");
        assert_eq!(plan.cp_name, "hummingbird-k8s");
    }
}
