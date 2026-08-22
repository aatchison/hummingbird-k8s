//! `hbird kube-bench` — bash twin: `scripts/run-kube-bench.sh`
//! (via `make kube-bench`).
//!
//! Runs the aquasecurity/kube-bench CIS Kubernetes Benchmark against
//! the live cluster and prints a combined report on stdout plus a
//! FAIL/WARN summary on stderr. Use the captured stdout to seed or
//! refresh `scripts/kube-bench-baseline.txt`.
//!
//! # Why two Jobs
//!
//! kube-bench must run INSIDE the cluster (it inspects host paths like
//! `/etc/kubernetes`, `/var/lib/etcd`, `/var/lib/kubelet` through
//! hostPath mounts and uses hostPID), so it is applied as a Job rather
//! than run locally. Upstream ships two manifests and we run both in
//! sequence:
//!
//! * `job-master.yaml` — nodeAffinity for
//!   `node-role.kubernetes.io/control-plane` plus a toleration for the
//!   master `NoSchedule` taint, so it covers sections 1.x (master),
//!   2.x (etcd), 3.x (control-plane config), 4.x (kubelet on the CP)
//!   and 5.x (policies).
//! * `job-node.yaml` — unconstrained, lands on a worker. Covers 4.x
//!   (worker) and 5.x again.
//!
//! Running only the combined `job.yaml` lets the scan land on whichever
//! node scheduling picks — usually a worker — and the control-plane
//! sections are silently lost. That is exactly how
//! `scripts/kube-bench-baseline.txt` came to be missing its 1.x/2.x/3.x
//! sections; the split-Job behaviour here is what fixes it going
//! forward, so do not collapse the two targets back into one.
//!
//! # Divergence from the bash twin: how kubectl is reached
//!
//! The twin shells out to `hbird kubectl` (or a plain `kubectl` if the
//! operator overrides `KUBECTL=`). Re-execing ourselves would be silly,
//! so the Rust path calls the shared [`crate::cp_kubectl`] shim
//! directly: `ssh -J $KVM_HOST root@$CP_IP kubectl
//! --kubeconfig=/etc/kubernetes/admin.conf …`. That is the same kubectl
//! `hbird kubectl` would have reached, one process shallower. The
//! `KUBECTL` env var is therefore NOT honoured — an operator who set it
//! to point at a private wrapper gets the CP's kubectl instead.
//!
//! # Exit codes (preserved from the twin)
//!
//! | situation                                       | exit |
//! |-------------------------------------------------|------|
//! | every requested target Job ran                   | 0    |
//! | cluster unreachable / unknown target requested   | 2    |
//! | a Job failed to complete or produced no logs     | 1    |
//!
//! Exit 0 does NOT mean kube-bench found no violations. The benchmark
//! output is informational, not gating; only infrastructure errors
//! (apply / wait / logs) fail the command.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;

use crate::cp_kubectl::{CpTarget, cp_kubectl_lenient_with_exec};
use crate::cp_resolve::resolve_cp_ip_via_ssh;
use hbird_config::ClusterConfig;

/// Cluster unreachable, or an unknown target was requested. Bash twin:
/// `exit 2`.
const EXIT_PRECONDITION: i32 = 2;

/// Upstream manifest host. Bash twin: `BASE_URL`.
const BASE_URL: &str = "https://raw.githubusercontent.com/aquasecurity/kube-bench";

// ---- clap surface ----------------------------------------------------------

/// Arguments for `hbird kube-bench`.
///
/// The bash twin took everything through env vars; the Rust shape
/// promotes them to flags and keeps the env fallbacks via clap's
/// `env = …`, so `KUBE_BENCH_VERSION=… hbird kube-bench` still works.
#[derive(Debug, Args)]
pub struct KubeBenchArgs {
    /// Path to `cluster.local.conf` (supplies `CP_NAME` / `KVM_HOST` /
    /// `CP_IP`).
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,

    /// libvirt domain name of the control plane. Overrides `--config`.
    #[arg(long, value_name = "NAME", env = "CP_NAME")]
    pub cp_name: Option<String>,

    /// SSH alias of the KVM host to ProxyJump through. Overrides `--config`.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// Explicit CP IP, bypassing config lookup + libvirt resolution.
    #[arg(long, value_name = "IP", env = "CP_IP")]
    pub cp_ip: Option<String>,

    /// kube-bench release tag. Bash twin: `KUBE_BENCH_VERSION`.
    ///
    /// Named `--kube-bench-version` rather than `--version` so it can
    /// never collide with clap's built-in version flag.
    #[arg(
        long = "kube-bench-version",
        value_name = "TAG",
        env = "KUBE_BENCH_VERSION",
        default_value = "v0.15.5"
    )]
    pub kube_bench_version: String,

    /// `kubectl wait` timeout for each Job. Bash twin: `KUBE_BENCH_TIMEOUT`.
    #[arg(
        long,
        value_name = "DURATION",
        env = "KUBE_BENCH_TIMEOUT",
        default_value = "5m"
    )]
    pub timeout: String,

    /// Namespace to run the Jobs in. Bash twin: `KUBE_BENCH_NS`.
    #[arg(
        long = "namespace",
        short = 'n',
        value_name = "NS",
        env = "KUBE_BENCH_NS",
        default_value = "default"
    )]
    pub namespace: String,

    /// Subset of `{master, node}` to run, space- or comma-separated.
    /// Bash twin: `KUBE_BENCH_TARGETS`.
    #[arg(
        long,
        value_name = "LIST",
        env = "KUBE_BENCH_TARGETS",
        default_value = "master node"
    )]
    pub targets: String,

    /// Print the exact kubectl calls that would run, then exit 0
    /// without touching the cluster.
    #[arg(long)]
    pub dry_run: bool,
}

// ---- logging ---------------------------------------------------------------

/// stderr log line. The `[run-kube-bench]` prefix is preserved verbatim
/// from the bash twin (`log()` at `scripts/run-kube-bench.sh`) because
/// operators grep captured runs for it, and because the twin's whole
/// stdout stream is meant to be redirected into a baseline file — every
/// diagnostic therefore has to stay on stderr.
fn log(line: &str) {
    eprintln!("[run-kube-bench] {line}");
}

// ---- targets ---------------------------------------------------------------

/// Which upstream kube-bench Job manifest to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchTarget {
    /// `job-master.yaml` — control-plane sections 1.x/2.x/3.x plus 4.x/5.x.
    Master,
    /// `job-node.yaml` — worker sections 4.x/5.x.
    Node,
}

impl BenchTarget {
    /// Operator-visible name, matching the twin's `$target`.
    fn as_str(self) -> &'static str {
        match self {
            BenchTarget::Master => "master",
            BenchTarget::Node => "node",
        }
    }

    /// Job name applied to the cluster. Twin: `kube-bench-${target}`.
    fn job_name(self) -> String {
        format!("kube-bench-{}", self.as_str())
    }

    /// Upstream manifest URL for `version`.
    /// Twin: `${BASE_URL}/job-${target}.yaml`.
    fn job_url(self, version: &str) -> String {
        format!("{BASE_URL}/{version}/job-{}.yaml", self.as_str())
    }
}

/// Parse the twin's `KUBE_BENCH_TARGETS` string. Accepts whitespace or
/// commas as separators; rejects anything outside `{master, node}`.
///
/// The twin validated inside the loop and `exit 2`-ed on the first bad
/// entry AFTER possibly having already run an earlier good one. We
/// validate the whole list up front instead, so a typo in the second
/// element cannot leave a half-finished scan behind — see
/// `parse_targets_rejects_unknown_before_running_anything`.
fn parse_targets(raw: &str) -> Result<Vec<BenchTarget>, String> {
    let mut out = Vec::new();
    for tok in raw.split([' ', '\t', '\n', ',']).filter(|s| !s.is_empty()) {
        match tok {
            "master" => out.push(BenchTarget::Master),
            "node" => out.push(BenchTarget::Node),
            // Wording preserved from the twin — operators grep it.
            other => {
                return Err(format!(
                    "FAIL: unknown target '{other}' (expected master or node)"
                ));
            }
        }
    }
    if out.is_empty() {
        return Err("FAIL: no targets requested (expected master and/or node)".to_string());
    }
    Ok(out)
}

// ---- plan ------------------------------------------------------------------

/// Everything one run needs, resolved once up front.
///
/// A params struct (rather than a long argument list) keeps
/// [`run_target_with_exec`] and friends at three arguments — the
/// workspace denies `clippy::too_many_arguments` and an `#[allow]`
/// would just hide the same readability problem.
#[derive(Debug, Clone)]
struct BenchPlan {
    /// CP IP + optional ProxyJump for every kubectl call.
    target: CpTarget,
    /// CP libvirt-domain name (diagnostic only).
    cp_name: String,
    /// kube-bench release tag used to build manifest URLs.
    version: String,
    /// `kubectl wait --timeout` value.
    timeout: String,
    /// Namespace the Jobs run in.
    namespace: String,
    /// Targets to run, in order.
    targets: Vec<BenchTarget>,
    /// Plan-only mode.
    dry_run: bool,
}

impl BenchPlan {
    /// Build the plan from clap args + `--config`.
    ///
    /// Resolution order per field mirrors [`crate::commands::verify`]:
    /// explicit flag (or its env var) > config file > default. In
    /// `--dry-run` the CP IP is left symbolic when it cannot be read
    /// from flags/config, so a plan can be printed from a laptop with
    /// no libvirt access at all.
    fn from_args(args: &KubeBenchArgs) -> Result<Self> {
        let targets = match parse_targets(&args.targets) {
            Ok(t) => t,
            Err(msg) => {
                log(&msg);
                std::process::exit(EXIT_PRECONDITION);
            }
        };

        let config: Option<ClusterConfig> = match args.config.as_ref() {
            Some(path) => Some(hbird_config::parse(path).map_err(|e| {
                anyhow::anyhow!(
                    "kube-bench: failed to parse --config {}: {e}",
                    path.display()
                )
            })?),
            None => None,
        };

        let cp_name = args
            .cp_name
            .clone()
            .or_else(|| config.as_ref().map(|c| c.cp_name.clone()))
            .unwrap_or_else(|| "hummingbird-k8s".to_string());

        let kvm_host = args
            .kvm_host
            .clone()
            .or_else(|| config.as_ref().and_then(|c| c.kvm_host.clone()))
            .filter(|s| !s.is_empty());

        let explicit_ip = args
            .cp_ip
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| config.as_ref().and_then(|c| c.cp_ip.clone()))
            .filter(|s| !s.is_empty());

        let cp_ip = match explicit_ip {
            Some(ip) => ip,
            None if args.dry_run => "<resolved-at-runtime>".to_string(),
            None => resolve_cp_ip(&cp_name, kvm_host.as_deref())?,
        };

        Ok(Self {
            target: CpTarget { cp_ip, kvm_host },
            cp_name,
            version: args.kube_bench_version.clone(),
            timeout: args.timeout.clone(),
            namespace: args.namespace.clone(),
            targets,
            dry_run: args.dry_run,
        })
    }

    /// `-n <ns>` prefix shared by every kubectl call.
    fn ns_flag(&self) -> String {
        format!("-n {}", self.namespace)
    }
}

/// Resolve the CP IP through `virsh domifaddr` on the KVM host, or
/// locally when no KVM host is configured.
///
/// Reuses [`crate::cp_resolve::resolve_cp_ip_via_ssh`] for the SSH case
/// and [`crate::virt_bridge::build_connection`] for the local case, so
/// running `hbird kube-bench` ON the KVM host works without
/// `KVM_HOST` being set at all.
fn resolve_cp_ip(cp_name: &str, kvm_host: Option<&str>) -> Result<String> {
    if let Some(host) = kvm_host {
        let client = hbird_ssh::Client::new(hbird_ssh::SshOptions::new(host.to_string()));
        return resolve_cp_ip_via_ssh(&client, host, cp_name);
    }
    // No KVM host: we are (or claim to be) on the hypervisor. Query the
    // local libvirt through the same bridge deploy/destroy use.
    let conn = crate::virt_bridge::build_connection(None);
    match conn.domifaddr(cp_name) {
        Ok(Some(ip)) => Ok(ip.to_string()),
        Ok(None) => bail!(
            "kube-bench: libvirt domain '{cp_name}' has no IPv4 lease yet (queried on this host). \
             Set CP_IP=<ip> in your CONFIG, or export KVM_HOST=<ssh-alias> to query libvirt \
             on the KVM host over SSH."
        ),
        Err(e) => bail!(
            "kube-bench: could not resolve IPv4 for domain '{cp_name}' via local \
             `virsh domifaddr`: {e}. Set CP_IP=<ip> in your CONFIG, or export \
             KVM_HOST=<ssh-alias>."
        ),
    }
}

// ---- report parsing --------------------------------------------------------

/// The stderr summary block the twin builds with two `grep -E` passes
/// over the combined logs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Summary {
    /// Lines starting with `[FAIL]` or `[WARN]`. Twin: `grep -E '^\[(FAIL|WARN)\]'`.
    findings: Vec<String>,
    /// Per-section tallies. Twin:
    /// `grep -E '(checks PASS|checks FAIL|checks WARN|checks INFO)'`.
    tallies: Vec<String>,
}

/// Extract the summary from a combined kube-bench report.
///
/// Faithful to the twin's two greps, including their asymmetry: the
/// findings grep is ANCHORED at column zero (`^\[`) while the tallies
/// grep is unanchored and therefore matches a tally phrase anywhere in
/// the line.
fn summarize(all_logs: &str) -> Summary {
    let mut summary = Summary::default();
    for line in all_logs.lines() {
        if line.starts_with("[FAIL]") || line.starts_with("[WARN]") {
            summary.findings.push(line.to_string());
        }
        if line.contains("checks PASS")
            || line.contains("checks FAIL")
            || line.contains("checks WARN")
            || line.contains("checks INFO")
        {
            summary.tallies.push(line.to_string());
        }
    }
    summary
}

/// Render the twin's `=== kube-bench summary ===` block. Emitted on
/// stderr so it never pollutes a stdout redirected into the baseline
/// file. The leading blank line and the `---` separator are part of the
/// twin's shape.
fn render_summary(summary: &Summary) -> Vec<String> {
    let mut lines = vec![String::new(), "=== kube-bench summary ===".to_string()];
    lines.extend(summary.findings.iter().cloned());
    lines.push("---".to_string());
    lines.extend(summary.tallies.iter().cloned());
    lines
}

/// Banner the twin prints ahead of each target's log block so the
/// combined baseline file stays readable. Reproduced byte-for-byte
/// (60 `#`).
fn target_banner(target: BenchTarget) -> String {
    let rule = "#".repeat(60);
    format!("{rule}\n# kube-bench target: {}\n{rule}", target.as_str())
}

/// First `-o name` line from `kubectl get nodes -l …`. Twin: `head -n1`.
/// Returns `None` when the CP label matched nothing.
fn first_node_name(raw: &str) -> Option<String> {
    raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

// ---- live steps ------------------------------------------------------------

/// Reachability probe. Twin: `kc version --request-timeout=10s`, whose
/// failure is the only exit-2 infrastructure condition.
fn cluster_reachable_with_exec(exec: &impl hbird_ssh::SshExec, plan: &BenchPlan) -> bool {
    match cp_kubectl_lenient_with_exec(exec, &plan.target.cp_ip, "version --request-timeout=10s") {
        Ok(out) => out.success,
        // A transport-level failure (auth, ProxyJump, DNS) is just as
        // much "can't reach the cluster" as a non-zero kubectl.
        Err(_) => false,
    }
}

/// Informational control-plane node lookup. Twin logs the node name, or
/// WARNs when the label matched nothing; neither outcome is fatal.
fn log_control_plane_node_with_exec(exec: &impl hbird_ssh::SshExec, plan: &BenchPlan) {
    let cmd = "get nodes -l node-role.kubernetes.io/control-plane -o name";
    let raw = match cp_kubectl_lenient_with_exec(exec, &plan.target.cp_ip, cmd) {
        Ok(out) if out.success => out.stdout,
        // Twin swallows errors here (`2>/dev/null … || true`).
        _ => String::new(),
    };
    match first_node_name(&raw) {
        Some(node) => log(&format!("control-plane node: {node}")),
        None => log("WARN: no node labeled node-role.kubernetes.io/control-plane found"),
    }
}

/// Run one target's Job and return its captured log block (banner
/// included), exactly like the twin's `run_target`.
///
/// # Errors
///
/// Returns `Err` (→ exit 1) when the Job does not reach
/// `condition=complete` within the timeout, or when it completes but
/// produces no log output. Both paths dump diagnostics to stderr first,
/// same as the twin.
fn run_target_with_exec(
    exec: &impl hbird_ssh::SshExec,
    plan: &BenchPlan,
    target: BenchTarget,
) -> Result<String> {
    let job = target.job_name();
    let url = target.job_url(&plan.version);
    let ns = plan.ns_flag();
    let name = target.as_str();

    // Wipe any leftover Job from a previous run. Best-effort, like the
    // twin's `|| true`.
    let _ = cp_kubectl_lenient_with_exec(
        exec,
        &plan.target.cp_ip,
        &format!("{ns} delete job {job} --ignore-not-found=true"),
    );

    log(&format!("[{name}] applying {url}"));
    // kubectl fetches the HTTP(S) manifest itself, which sidesteps
    // piping YAML through the SSH stdin channel.
    let applied =
        cp_kubectl_lenient_with_exec(exec, &plan.target.cp_ip, &format!("{ns} apply -f {url}"))?;
    if !applied.stdout.is_empty() {
        eprint!("{}", applied.stdout);
    }
    if !applied.success {
        eprint!("{}", applied.stderr);
        bail!("[{name}] FAIL: could not apply {url}");
    }

    log(&format!(
        "[{name}] waiting up to {} for Job/{job}",
        plan.timeout
    ));
    let waited = cp_kubectl_lenient_with_exec(
        exec,
        &plan.target.cp_ip,
        &format!(
            "{ns} wait --for=condition=complete --timeout={} job/{job}",
            plan.timeout
        ),
    )?;
    if !waited.success {
        // Wording preserved from the twin: operators grep captured runs
        // for "FAIL: Job did not complete".
        log(&format!(
            "[{name}] FAIL: Job did not complete within {}",
            plan.timeout
        ));
        log(&format!("[{name}] --- pod status ---"));
        if let Ok(pods) = cp_kubectl_lenient_with_exec(
            exec,
            &plan.target.cp_ip,
            &format!("{ns} get pods -l job-name={job} -o wide"),
        ) {
            eprint!("{}", pods.stdout);
        }
        log(&format!("[{name}] --- last logs ---"));
        if let Ok(tail) = cp_kubectl_lenient_with_exec(
            exec,
            &plan.target.cp_ip,
            &format!("{ns} logs job/{job} --tail=100"),
        ) {
            eprint!("{}", tail.stdout);
        }
        bail!(
            "[{name}] FAIL: Job did not complete within {}",
            plan.timeout
        );
    }

    log(&format!("[{name}] fetching logs"));
    let logs =
        cp_kubectl_lenient_with_exec(exec, &plan.target.cp_ip, &format!("{ns} logs job/{job}"))?;
    if logs.stdout.trim().is_empty() {
        log(&format!("[{name}] FAIL: kube-bench produced no log output"));
        bail!("[{name}] FAIL: kube-bench produced no log output");
    }

    Ok(format!(
        "{}\n{}\n\n",
        target_banner(target),
        logs.stdout.trim_end_matches('\n')
    ))
}

/// Delete every Job we created. Mirrors the twin's `trap cleanup EXIT`:
/// runs whether the scan succeeded or bailed partway through, and never
/// reports its own failures.
fn cleanup_with_exec(exec: &impl hbird_ssh::SshExec, plan: &BenchPlan, created: &[String]) {
    for job in created {
        log(&format!("cleaning up Job/{job} in ns/{}", plan.namespace));
        let _ = cp_kubectl_lenient_with_exec(
            exec,
            &plan.target.cp_ip,
            &format!(
                "{} delete job {job} --ignore-not-found=true --wait=false",
                plan.ns_flag()
            ),
        );
    }
}

/// Run every requested target, then clean up unconditionally.
/// Returns the combined report (what the twin writes to stdout).
fn run_all_with_exec(exec: &impl hbird_ssh::SshExec, plan: &BenchPlan) -> Result<String> {
    let mut created: Vec<String> = Vec::new();
    let mut all_logs = String::new();
    let mut result = Ok(());

    for target in &plan.targets {
        // Tracked BEFORE the apply, so a failed apply still gets a
        // delete attempt (a no-op if the Job never existed).
        created.push(target.job_name());
        match run_target_with_exec(exec, plan, *target) {
            Ok(block) => {
                print!("{block}");
                all_logs.push_str(&block);
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }

    cleanup_with_exec(exec, plan, &created);
    result?;
    Ok(all_logs)
}

// ---- dry run ---------------------------------------------------------------

/// Deterministic plan: every kubectl call, in order, with no cluster
/// contact at all.
fn render_dry_run_plan(plan: &BenchPlan) -> Vec<String> {
    let ns = plan.ns_flag();
    let mut lines = vec![
        format!("DRY-RUN kube-bench version: {}", plan.version),
        format!(
            "DRY-RUN cp: {} ({}){}",
            plan.target.cp_ip,
            plan.cp_name,
            plan.target
                .kvm_host
                .as_deref()
                .map(|h| format!(" via {h}"))
                .unwrap_or_default(),
        ),
        format!("DRY-RUN namespace: {}", plan.namespace),
        format!("DRY-RUN timeout:   {}", plan.timeout),
        format!(
            "DRY-RUN targets:   {}",
            plan.targets
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "DRY-RUN would run: kubectl version --request-timeout=10s".to_string(),
        "DRY-RUN would run: kubectl get nodes -l node-role.kubernetes.io/control-plane -o name"
            .to_string(),
    ];
    for target in &plan.targets {
        let job = target.job_name();
        let url = target.job_url(&plan.version);
        lines.push(format!(
            "DRY-RUN would run: kubectl {ns} delete job {job} --ignore-not-found=true"
        ));
        lines.push(format!("DRY-RUN would run: kubectl {ns} apply -f {url}"));
        lines.push(format!(
            "DRY-RUN would run: kubectl {ns} wait --for=condition=complete --timeout={} job/{job}",
            plan.timeout
        ));
        lines.push(format!("DRY-RUN would run: kubectl {ns} logs job/{job}"));
    }
    for target in &plan.targets {
        lines.push(format!(
            "DRY-RUN cleanup:   kubectl {ns} delete job {} --ignore-not-found=true --wait=false",
            target.job_name()
        ));
    }
    lines
}

// ---- dispatch --------------------------------------------------------------

/// `hbird kube-bench` entry point.
///
/// Exit-code mapping (see module docs): `Ok(())` → 0, `Err(_)` → 1 via
/// `main`'s anyhow return, and [`std::process::exit`] for the exit-2
/// precondition failures the twin owns.
pub fn run(args: KubeBenchArgs) -> Result<()> {
    let plan = BenchPlan::from_args(&args)?;

    log(&format!("kube-bench version: {}", plan.version));
    log(&format!(
        "kubectl:           ssh root@{} kubectl (via hbird cp_kubectl shim)",
        plan.target.cp_ip
    ));
    log(&format!("namespace:         {}", plan.namespace));
    log(&format!(
        "targets:           {}",
        plan.targets
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    ));

    if plan.dry_run {
        for line in render_dry_run_plan(&plan) {
            log(&line);
        }
        return Ok(());
    }

    let client = hbird_ssh::Client::new(plan.target.cp_ssh_opts());

    if !cluster_reachable_with_exec(&client, &plan) {
        // Wording preserved from the twin (operators grep
        // "FAIL: kubectl can't reach the cluster"); the hint is
        // retargeted at the Rust flag set.
        log(&format!(
            "FAIL: kubectl can't reach the cluster (cp_ip={})",
            plan.target.cp_ip
        ));
        log("      try: hbird kube-bench --config <conf> --kvm-host <host>");
        std::process::exit(EXIT_PRECONDITION);
    }

    log_control_plane_node_with_exec(&client, &plan);

    let all_logs = run_all_with_exec(&client, &plan)?;

    for line in render_summary(&summarize(&all_logs)) {
        eprintln!("{line}");
    }
    log("done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbird_ssh::{Error as SshErr, RunOutput, SshExec};
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::Mutex;

    // ---- mock exec (same shape as update_cluster / cp_kubectl tests) ----

    /// Scripted [`SshExec`]: pops one canned response per call and
    /// records every command it saw.
    struct MockSshExec {
        canned: Mutex<Vec<Result<RunOutput, SshErr>>>,
        observed: Mutex<Vec<String>>,
    }

    impl MockSshExec {
        fn new(canned: Vec<Result<RunOutput, SshErr>>) -> Self {
            Self {
                canned: Mutex::new(canned),
                observed: Mutex::new(Vec::new()),
            }
        }

        fn commands(&self) -> Vec<String> {
            self.observed.lock().unwrap().clone()
        }
    }

    impl SshExec for MockSshExec {
        fn run(&self, command: &str) -> Result<RunOutput, SshErr> {
            self.observed.lock().unwrap().push(command.to_string());
            let mut q = self.canned.lock().unwrap();
            if q.is_empty() {
                panic!("MockSshExec: no canned response left for `{command}`");
            }
            q.remove(0)
        }

        fn run_with_stdin(&self, command: &str, _stdin: &[u8]) -> Result<RunOutput, SshErr> {
            self.run(command)
        }
    }

    fn ok_stdout(s: &str) -> Result<RunOutput, SshErr> {
        Ok(RunOutput {
            status: ExitStatus::from_raw(0),
            stdout: s.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    fn nonzero_exit(code: i32, stderr: &str) -> Result<RunOutput, SshErr> {
        Err(SshErr::NonZeroExit {
            host: "192.168.122.42".to_string(),
            // `from_raw` takes a wait-status word; shift so `code()`
            // reports the intended exit code.
            status: ExitStatus::from_raw(code << 8),
            stdout: String::new(),
            stderr: stderr.to_string(),
        })
    }

    fn transport_error() -> Result<RunOutput, SshErr> {
        Err(SshErr::Spawn {
            program: "ssh".to_string(),
            host: "192.168.122.42".to_string(),
            kind: hbird_ssh::SpawnKind::SshBinaryMissing,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "ssh not found"),
        })
    }

    fn test_plan(targets: Vec<BenchTarget>) -> BenchPlan {
        BenchPlan {
            target: CpTarget {
                cp_ip: "192.168.122.42".to_string(),
                kvm_host: Some("geary".to_string()),
            },
            cp_name: "hummingbird-k8s".to_string(),
            version: "v0.15.5".to_string(),
            timeout: "5m".to_string(),
            namespace: "default".to_string(),
            targets,
            dry_run: false,
        }
    }

    // ---- target parsing --------------------------------------------------

    #[test]
    fn parse_targets_default_is_master_then_node() {
        assert_eq!(
            parse_targets("master node").expect("default parses"),
            vec![BenchTarget::Master, BenchTarget::Node]
        );
    }

    #[test]
    fn parse_targets_accepts_commas_and_extra_whitespace() {
        assert_eq!(
            parse_targets("  master ,, node  ").expect("parses"),
            vec![BenchTarget::Master, BenchTarget::Node]
        );
        assert_eq!(
            parse_targets("node").expect("single target parses"),
            vec![BenchTarget::Node]
        );
    }

    #[test]
    fn parse_targets_rejects_unknown_before_running_anything() {
        // Twin wording, preserved for grep parity.
        let err = parse_targets("master worker").expect_err("unknown target must be rejected");
        assert_eq!(
            err,
            "FAIL: unknown target 'worker' (expected master or node)"
        );
        // Divergence (bug fix): the twin validated INSIDE the loop, so
        // `KUBE_BENCH_TARGETS="master worker"` applied + waited on the
        // master Job and only then exited 2 — leaving a Job behind and
        // no cleanup for the typo'd one. Up-front validation means no
        // Job is ever created for a bad list.
        assert!(parse_targets("master worker").is_err());
    }

    #[test]
    fn parse_targets_rejects_empty_list() {
        assert!(parse_targets("").is_err());
        assert!(parse_targets("   ").is_err());
    }

    #[test]
    fn target_job_names_and_urls_match_upstream_layout() {
        assert_eq!(BenchTarget::Master.job_name(), "kube-bench-master");
        assert_eq!(BenchTarget::Node.job_name(), "kube-bench-node");
        assert_eq!(
            BenchTarget::Master.job_url("v0.15.5"),
            "https://raw.githubusercontent.com/aquasecurity/kube-bench/v0.15.5/job-master.yaml"
        );
        assert_eq!(
            BenchTarget::Node.job_url("v0.16.0"),
            "https://raw.githubusercontent.com/aquasecurity/kube-bench/v0.16.0/job-node.yaml"
        );
    }

    // ---- report parsing --------------------------------------------------

    const SAMPLE_REPORT: &str = "\
[INFO] 4 Worker Node Security Configuration
[PASS] 4.1.1 Ensure that the kubelet service file permissions are set
[FAIL] 4.1.2 Ensure that the kubelet service file ownership is set
[WARN] 4.2.6 Ensure that the --protect-kernel-defaults argument is set
  remediation text mentioning checks PASS should not be a finding
== Summary node ==
12 checks PASS
1 checks FAIL
3 checks WARN
0 checks INFO
";

    #[test]
    fn summarize_collects_fail_and_warn_findings_only() {
        let s = summarize(SAMPLE_REPORT);
        assert_eq!(
            s.findings,
            vec![
                "[FAIL] 4.1.2 Ensure that the kubelet service file ownership is set".to_string(),
                "[WARN] 4.2.6 Ensure that the --protect-kernel-defaults argument is set"
                    .to_string(),
            ],
            "PASS/INFO lines are not findings"
        );
    }

    #[test]
    fn summarize_findings_grep_is_anchored_at_column_zero() {
        // Twin's grep is `^\[(FAIL|WARN)\]`; an indented or embedded
        // marker must NOT be collected.
        let s = summarize("  [FAIL] indented\nnote: [WARN] embedded\n[FAIL] real\n");
        assert_eq!(s.findings, vec!["[FAIL] real".to_string()]);
    }

    #[test]
    fn summarize_tallies_grep_is_deliberately_unanchored() {
        // Twin's tally grep has no `^`, so it matches the phrase
        // anywhere — including inside remediation prose. Faithful port:
        // the prose line IS collected.
        let s = summarize(SAMPLE_REPORT);
        assert_eq!(
            s.tallies,
            vec![
                "  remediation text mentioning checks PASS should not be a finding".to_string(),
                "12 checks PASS".to_string(),
                "1 checks FAIL".to_string(),
                "3 checks WARN".to_string(),
                "0 checks INFO".to_string(),
            ]
        );
    }

    #[test]
    fn summarize_empty_report_yields_empty_summary() {
        assert_eq!(summarize(""), Summary::default());
    }

    #[test]
    fn render_summary_shape_matches_the_twin() {
        let s = summarize(SAMPLE_REPORT);
        let lines = render_summary(&s);
        assert_eq!(lines[0], "", "twin opens the block with a blank line");
        assert_eq!(lines[1], "=== kube-bench summary ===");
        // Findings, then the `---` separator, then the tallies.
        let sep = lines.iter().position(|l| l == "---").expect("separator");
        assert_eq!(sep, 2 + s.findings.len());
        assert_eq!(lines.len(), sep + 1 + s.tallies.len());
    }

    #[test]
    fn render_summary_keeps_separator_when_nothing_matched() {
        let lines = render_summary(&Summary::default());
        assert_eq!(lines, vec!["", "=== kube-bench summary ===", "---"]);
    }

    #[test]
    fn target_banner_is_sixty_hashes_around_the_target_name() {
        let banner = target_banner(BenchTarget::Master);
        let lines: Vec<&str> = banner.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), 60);
        assert!(lines[0].chars().all(|c| c == '#'));
        assert_eq!(lines[1], "# kube-bench target: master");
        assert_eq!(lines[2], lines[0]);
    }

    #[test]
    fn first_node_name_takes_the_first_nonblank_line() {
        assert_eq!(
            first_node_name("node/hbird-cp1\nnode/hbird-cp2\n").as_deref(),
            Some("node/hbird-cp1")
        );
        assert_eq!(
            first_node_name("\n  node/hbird-cp1  \n").as_deref(),
            Some("node/hbird-cp1")
        );
    }

    #[test]
    fn first_node_name_is_none_when_label_matched_nothing() {
        assert_eq!(first_node_name(""), None);
        assert_eq!(first_node_name("\n \n"), None);
    }

    // ---- exit-code classification ---------------------------------------

    #[test]
    fn cluster_reachable_true_on_exit_zero() {
        let exec = MockSshExec::new(vec![ok_stdout("Client Version: v1.31.0")]);
        assert!(cluster_reachable_with_exec(&exec, &test_plan(vec![])));
        assert_eq!(
            exec.commands(),
            vec![
                "kubectl --kubeconfig=/etc/kubernetes/admin.conf version --request-timeout=10s"
                    .to_string()
            ]
        );
    }

    #[test]
    fn cluster_reachable_false_on_nonzero_exit() {
        let exec = MockSshExec::new(vec![nonzero_exit(1, "connection refused")]);
        assert!(!cluster_reachable_with_exec(&exec, &test_plan(vec![])));
    }

    #[test]
    fn cluster_reachable_false_on_transport_error() {
        // ssh itself failed to spawn — indistinguishable from
        // "unreachable" for the twin's purposes.
        let exec = MockSshExec::new(vec![transport_error()]);
        assert!(!cluster_reachable_with_exec(&exec, &test_plan(vec![])));
    }

    #[test]
    fn control_plane_lookup_tolerates_a_failed_kubectl() {
        // Non-zero `get nodes` must not abort: the twin swallows it with
        // `2>/dev/null || true` and WARNs.
        let exec = MockSshExec::new(vec![nonzero_exit(1, "the server was unable")]);
        log_control_plane_node_with_exec(&exec, &test_plan(vec![]));
        assert_eq!(exec.commands().len(), 1);
    }

    // ---- run_target ------------------------------------------------------

    #[test]
    fn run_target_happy_path_emits_banner_and_logs() {
        let exec = MockSshExec::new(vec![
            ok_stdout(""),                                          // delete leftover
            ok_stdout("job.batch/kube-bench-master created"),       // apply
            ok_stdout("job.batch/kube-bench-master condition met"), // wait
            ok_stdout(SAMPLE_REPORT),                               // logs
        ]);
        let block = run_target_with_exec(&exec, &test_plan(vec![]), BenchTarget::Master)
            .expect("happy path");
        assert!(block.starts_with(&"#".repeat(60)));
        assert!(block.contains("# kube-bench target: master"));
        assert!(block.contains("[FAIL] 4.1.2"));
        assert!(
            block.ends_with("\n\n"),
            "twin appends a trailing blank line"
        );

        let cmds = exec.commands();
        assert_eq!(cmds.len(), 4, "delete, apply, wait, logs: {cmds:?}");
        assert!(cmds[0].contains("delete job kube-bench-master --ignore-not-found=true"));
        assert!(cmds[1].contains(
            "apply -f https://raw.githubusercontent.com/aquasecurity/kube-bench/v0.15.5/job-master.yaml"
        ));
        assert!(
            cmds[2].contains("wait --for=condition=complete --timeout=5m job/kube-bench-master")
        );
        assert!(cmds[3].ends_with("-n default logs job/kube-bench-master"));
    }

    #[test]
    fn run_target_fails_when_apply_returns_nonzero() {
        let exec = MockSshExec::new(vec![
            ok_stdout(""),                                // delete leftover
            nonzero_exit(1, "error: unable to read URL"), // apply
        ]);
        let err = run_target_with_exec(&exec, &test_plan(vec![]), BenchTarget::Node)
            .expect_err("failed apply must abort the target");
        assert!(err.to_string().contains("could not apply"), "{err}");
        assert_eq!(
            exec.commands().len(),
            2,
            "must not wait after a failed apply"
        );
    }

    #[test]
    fn run_target_fails_when_wait_times_out_and_dumps_diagnostics() {
        let exec = MockSshExec::new(vec![
            ok_stdout(""),                                          // delete leftover
            ok_stdout("job.batch/kube-bench-node created"),         // apply
            nonzero_exit(1, "timed out waiting for the condition"), // wait
            ok_stdout("kube-bench-node-abcde  0/1  Pending"),       // get pods
            ok_stdout("some pod logs"),                             // logs --tail=100
        ]);
        let err = run_target_with_exec(&exec, &test_plan(vec![]), BenchTarget::Node)
            .expect_err("wait timeout must fail the target");
        // Twin wording, preserved for grep parity.
        assert!(
            err.to_string()
                .contains("FAIL: Job did not complete within 5m"),
            "{err}"
        );
        let cmds = exec.commands();
        assert_eq!(cmds.len(), 5, "diagnostics must be collected: {cmds:?}");
        assert!(cmds[3].contains("get pods -l job-name=kube-bench-node -o wide"));
        assert!(cmds[4].contains("logs job/kube-bench-node --tail=100"));
    }

    #[test]
    fn run_target_fails_when_logs_are_empty() {
        for empty in ["", "   \n\n"] {
            let exec = MockSshExec::new(vec![
                ok_stdout(""),
                ok_stdout("created"),
                ok_stdout("condition met"),
                ok_stdout(empty),
            ]);
            let err = run_target_with_exec(&exec, &test_plan(vec![]), BenchTarget::Master)
                .expect_err("empty logs must fail the target");
            assert!(
                err.to_string()
                    .contains("FAIL: kube-bench produced no log output"),
                "{err}"
            );
        }
    }

    #[test]
    fn run_target_surfaces_a_transport_error_rather_than_masking_it() {
        let exec = MockSshExec::new(vec![
            ok_stdout(""),     // delete leftover
            transport_error(), // apply — ssh itself broke
        ]);
        let err = run_target_with_exec(&exec, &test_plan(vec![]), BenchTarget::Master)
            .expect_err("transport failure must not be swallowed");
        assert!(err.to_string().contains("ssh-run failed"), "{err}");
    }

    // ---- run_all + cleanup ----------------------------------------------

    #[test]
    fn run_all_runs_both_targets_then_cleans_up_both() {
        let mut canned = Vec::new();
        for _ in 0..2 {
            canned.push(ok_stdout("")); // delete leftover
            canned.push(ok_stdout("created")); // apply
            canned.push(ok_stdout("condition met")); // wait
            canned.push(ok_stdout(SAMPLE_REPORT)); // logs
        }
        canned.push(ok_stdout("")); // cleanup master
        canned.push(ok_stdout("")); // cleanup node
        let exec = MockSshExec::new(canned);
        let plan = test_plan(vec![BenchTarget::Master, BenchTarget::Node]);

        let all = run_all_with_exec(&exec, &plan).expect("both targets succeed");
        assert!(all.contains("# kube-bench target: master"));
        assert!(all.contains("# kube-bench target: node"));

        let cmds = exec.commands();
        assert_eq!(cmds.len(), 10, "8 scan calls + 2 cleanups: {cmds:?}");
        assert!(
            cmds[8].contains("delete job kube-bench-master --ignore-not-found=true --wait=false")
        );
        assert!(
            cmds[9].contains("delete job kube-bench-node --ignore-not-found=true --wait=false")
        );
    }

    #[test]
    fn run_all_cleans_up_even_when_a_target_fails() {
        // Master apply fails → node target never runs, but the master
        // Job is still deleted (twin: `trap cleanup EXIT`).
        let exec = MockSshExec::new(vec![
            ok_stdout(""),                   // delete leftover (master)
            nonzero_exit(1, "apply failed"), // apply (master)
            ok_stdout(""),                   // cleanup master
        ]);
        let plan = test_plan(vec![BenchTarget::Master, BenchTarget::Node]);
        let err = run_all_with_exec(&exec, &plan).expect_err("failed target propagates");
        assert!(err.to_string().contains("could not apply"), "{err}");

        let cmds = exec.commands();
        assert_eq!(cmds.len(), 3, "node target must not have started: {cmds:?}");
        assert!(cmds[2].contains("delete job kube-bench-master"));
        assert!(
            !cmds.iter().any(|c| c.contains("kube-bench-node")),
            "node Job must never be created after a master failure: {cmds:?}"
        );
    }

    // ---- dry run ---------------------------------------------------------

    #[test]
    fn dry_run_plan_lists_every_call_in_order() {
        let mut plan = test_plan(vec![BenchTarget::Master, BenchTarget::Node]);
        plan.dry_run = true;
        let lines = render_dry_run_plan(&plan);
        // 5 header lines + 2 preflight calls + 4 calls per target + 1
        // cleanup per target.
        assert_eq!(lines.len(), 5 + 2 + 4 * 2 + 2, "{lines:#?}");
        assert_eq!(lines[0], "DRY-RUN kube-bench version: v0.15.5");
        assert_eq!(
            lines[1],
            "DRY-RUN cp: 192.168.122.42 (hummingbird-k8s) via geary"
        );
        assert_eq!(lines[4], "DRY-RUN targets:   master node");
        assert!(lines.iter().any(|l| l.contains("job-master.yaml")));
        assert!(lines.iter().any(|l| l.contains("job-node.yaml")));
        // Deterministic: re-rendering is byte-identical.
        assert_eq!(lines, render_dry_run_plan(&plan));
    }

    #[test]
    fn dry_run_plan_shrinks_with_a_single_target() {
        let mut plan = test_plan(vec![BenchTarget::Node]);
        plan.dry_run = true;
        let lines = render_dry_run_plan(&plan);
        assert_eq!(lines.len(), 5 + 2 + 4 + 1);
        assert!(
            !lines.iter().any(|l| l.contains("job-master.yaml")),
            "master must not appear: {lines:#?}"
        );
    }

    #[test]
    fn dry_run_plan_omits_the_via_clause_without_a_kvm_host() {
        let mut plan = test_plan(vec![BenchTarget::Node]);
        plan.dry_run = true;
        plan.target.kvm_host = None;
        let lines = render_dry_run_plan(&plan);
        assert_eq!(lines[1], "DRY-RUN cp: 192.168.122.42 (hummingbird-k8s)");
    }

    #[test]
    fn ns_flag_is_threaded_through_every_command() {
        let mut plan = test_plan(vec![BenchTarget::Master]);
        plan.namespace = "kube-bench".to_string();
        plan.dry_run = true;
        for line in render_dry_run_plan(&plan) {
            if line.starts_with("DRY-RUN would run: kubectl -")
                || line.starts_with("DRY-RUN cleanup:")
            {
                assert!(
                    line.contains("-n kube-bench"),
                    "namespace missing from: {line}"
                );
            }
        }
    }
}
