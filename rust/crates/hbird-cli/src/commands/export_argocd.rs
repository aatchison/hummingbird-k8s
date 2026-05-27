//! `hbird export-argocd` — bash twin: `scripts/export-argocd.sh`.
//!
//! Produces an ArgoCD-registerable kubeconfig by:
//!
//! 1. Loading `cluster.local.conf` via [`hbird_config::parse`].
//! 2. Resolving CP_IP / KVM_HOST in the post-#306 order (config first,
//!    then CLI flags / env override).
//! 3. SSHing to `root@$CP_IP` (ProxyJump=$KVM_HOST when set) and
//!    running `sudo cat /etc/kubernetes/admin.conf`. Uses
//!    [`hbird_ssh::Client`] with default `BatchMode=yes` (no TTY)
//!    so modern sudo doesn't emit OSC session-start escapes into
//!    stdout (avoiding the #307 bash bug).
//! 4. Rewriting the `server:` URL + cluster/context/user names so the
//!    file doesn't collide with the operator's other kubeconfigs and
//!    ArgoCD registers under a meaningful name.
//! 5. Writing to `--output` with mode `0600` (backup-on-overwrite when
//!    `--force` is set, same shape as bash twin lines 364-385).
//!
//! Bash flag set (see `scripts/export-argocd.sh:188`):
//! `--output`, `--server`, `--context-name`, `--proxy-jump`, `--force`.
//!
//! Behavior tracked by [#288]. Two known bash bugs the Rust impl
//! deliberately fixes (and thus diverges from bash on):
//!
//! - **[#306]** — bash sets `PROXY_JUMP="${KVM_HOST:-}"` BEFORE
//!   sourcing `$CONFIG`, so `KVM_HOST=geary` in CONFIG (the documented
//!   place) doesn't activate ProxyJump unless also exported in the
//!   operator's shell. Rust resolves AFTER config load — the intended
//!   shape.
//! - **[#307]** — bash `cp_ssh() { ssh -t ... }` allocates a remote
//!   PTY; combined with `sudo cat`, modern sudo emits an OSC
//!   session-start escape into stdout that breaks the downstream
//!   `grep -q '^apiVersion:'` sanity check. Rust uses non-TTY SSH
//!   (BatchMode=yes is the hbird-ssh default).
//!
//! [#288]: https://github.com/aatchison/hummingbird-k8s/issues/288
//! [#306]: https://github.com/aatchison/hummingbird-k8s/issues/306
//! [#307]: https://github.com/aatchison/hummingbird-k8s/issues/307

use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;

use crate::cp_kubectl::{CpTarget, cp_ssh_capture};

/// Arguments for `hbird export-argocd`.
#[derive(Debug, Args)]
pub struct ExportArgocdArgs {
    /// Path to `cluster.local.conf`.
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,

    /// Output path. Bash-twin default: `argocd-kubeconfig.yaml`.
    #[arg(long, value_name = "PATH", default_value = "argocd-kubeconfig.yaml")]
    pub output: PathBuf,

    /// Override the API-server URL written into the kubeconfig.
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,

    /// Context name. Bash-twin default: `hummingbird-$CP_NAME` (prefix
    /// avoids colliding with whatever ArgoCD already has registered).
    /// CLI flag renamed from `--context-name` to `--context` for
    /// consistency with the rest of the binary; `alias = "context-name"`
    /// keeps the bash twin's spelling working so operator muscle memory
    /// doesn't break. (PR #319 round-2 review L9 MEDIUM.)
    #[arg(long, alias = "context-name", value_name = "NAME")]
    pub context: Option<String>,

    /// SSH ProxyJump host. Defaults to CONFIG's `KVM_HOST` when unset
    /// (post-#306 resolution order). Pass `--proxy-jump=''` to
    /// explicitly disable ProxyJump on this invocation.
    #[arg(long, value_name = "HOST")]
    pub proxy_jump: Option<String>,

    /// Static CP IP. Overrides CONFIG's `CP_IP`. Required when CONFIG
    /// doesn't pin one — virsh-domifaddr resolution isn't wired in
    /// the Rust path yet (#322).
    #[arg(long, value_name = "IP", env = "CP_IP")]
    pub cp_ip: Option<String>,

    /// Overwrite `--output` if it already exists.
    #[arg(long)]
    pub force: bool,
}

/// Dispatch. Mirrors the bash twin's top-to-bottom flow but with the
/// #306 fix (config first, then CLI/env) and the #307 fix (non-TTY
/// SSH so sudo doesn't pollute stdout with OSC escapes).
pub fn run(args: ExportArgocdArgs) -> Result<()> {
    let opts = ExportOptions::from_args(args)?;
    export_kubeconfig(&opts)
}

/// Shared core invoked by both `hbird export-argocd` and
/// `hbird get-kubeconfig` (the latter passes operator-friendly
/// defaults via [`ExportOptions::for_get_kubeconfig`]).
#[derive(Debug, Clone)]
pub(crate) struct ExportOptions {
    #[allow(dead_code)] // surfaced in log lines + reserved for future flags.
    pub cp_name: String,
    pub cp_ip: String,
    pub kvm_host: Option<String>,
    pub output: PathBuf,
    pub server: String,
    pub context_name: String,
    pub force: bool,
}

impl ExportOptions {
    /// Build from `hbird export-argocd` args. Default context name is
    /// `hummingbird-$CP_NAME` (bash twin line 292).
    pub(crate) fn from_args(args: ExportArgocdArgs) -> Result<Self> {
        let config = hbird_config::parse(&args.config).map_err(|e| anyhow!("{e}"))?;
        let cp_name = config.cp_name.clone();

        // #306: CP_IP resolution AFTER config load (CLI/env wins, then
        // config). virsh-domifaddr resolution is deferred to the live
        // helper wired by #322.
        let cp_ip = args.cp_ip.or(config.cp_ip).ok_or_else(|| {
            anyhow!(
                "could not resolve CP_IP for '{cp_name}' (set CP_IP=<ip> in {}, \
                 or pass --cp-ip / CP_IP env; virsh-domifaddr resolution \
                 not yet wired in the Rust path)",
                args.config.display(),
            )
        })?;
        if cp_ip.is_empty() {
            bail!("CP_IP resolved to empty string");
        }

        // #306: ProxyJump defaulting. CLI flag wins (Some(...) — even
        // empty string means "explicitly disable"). Otherwise fall back
        // to CONFIG's KVM_HOST. Mirrors the bash twin's
        // PROXY_JUMP_SET sentinel — bash differentiates
        // `--proxy-jump=` (explicit empty: disable) from absent
        // (default to KVM_HOST). Clap's `Option<String>` gives us the
        // same distinction: `Some("")` = explicit empty, `None` =
        // absent.
        let kvm_host = match args.proxy_jump {
            Some(s) if s.is_empty() => None, // explicit `--proxy-jump=`
            Some(s) => Some(s),
            None => config.kvm_host.filter(|s| !s.is_empty()),
        };

        let server = args
            .server
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("https://{cp_ip}:6443"));

        let context_name = args
            .context
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("hummingbird-{cp_name}"));
        validate_context_name(&context_name)?;

        Ok(Self {
            cp_name,
            cp_ip,
            kvm_host,
            output: args.output,
            server,
            context_name,
            force: args.force,
        })
    }

    /// Build from `hbird get-kubeconfig` args. Default context name is
    /// `$CP_NAME` (without the `hummingbird-` prefix) — bash twin via
    /// the `make get-kubeconfig` rule.
    pub(crate) fn for_get_kubeconfig(
        config_path: &Path,
        output: PathBuf,
        cli_server: Option<String>,
        cli_context: Option<String>,
        cli_proxy_jump: Option<String>,
        cli_cp_ip: Option<String>,
        force: bool,
    ) -> Result<Self> {
        let config = hbird_config::parse(config_path).map_err(|e| anyhow!("{e}"))?;
        let cp_name = config.cp_name.clone();
        let cp_ip = cli_cp_ip.or(config.cp_ip).ok_or_else(|| {
            anyhow!(
                "could not resolve CP_IP for '{cp_name}' (set CP_IP=<ip> in {}, \
                 or pass --cp-ip / CP_IP env)",
                config_path.display(),
            )
        })?;
        if cp_ip.is_empty() {
            bail!("CP_IP resolved to empty string");
        }
        let kvm_host = match cli_proxy_jump {
            Some(s) if s.is_empty() => None,
            Some(s) => Some(s),
            None => config.kvm_host.filter(|s| !s.is_empty()),
        };
        let server = cli_server
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("https://{cp_ip}:6443"));
        let context_name = cli_context
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cp_name.clone());
        validate_context_name(&context_name)?;
        Ok(Self {
            cp_name,
            cp_ip,
            kvm_host,
            output,
            server,
            context_name,
            force,
        })
    }

    pub(crate) fn target(&self) -> CpTarget {
        CpTarget {
            cp_ip: self.cp_ip.clone(),
            kvm_host: self.kvm_host.clone(),
        }
    }
}

/// Validate context name — kubeconfig names can be reasonably
/// permissive but ArgoCD treats them as URL-safe IDs. Conservative
/// allowlist matches bash twin line 296.
fn validate_context_name(s: &str) -> Result<()> {
    if s.is_empty() {
        bail!("--context-name must not be empty");
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("--context-name must match [A-Za-z0-9._-]+ (got: '{s}')");
    }
    Ok(())
}

/// Top-level worker — invoked by both `export-argocd` and
/// `get-kubeconfig`. Bash twin: `scripts/export-argocd.sh` lines
/// 311-406 (everything after the SSH-helper definition).
#[tracing::instrument(level = "debug", skip(opts), fields(cp_ip = %opts.cp_ip), err(Debug))]
pub(crate) fn export_kubeconfig(opts: &ExportOptions) -> Result<()> {
    pre_check_output(&opts.output, opts.force)?;
    log(&format!(
        "fetching /etc/kubernetes/admin.conf from root@{}",
        opts.cp_ip
    ));
    let raw = fetch_admin_conf(opts).context("fetch admin.conf via SSH")?;
    sanity_check_kubeconfig(&raw)?;
    let rewritten = rewrite_kubeconfig(&raw, &opts.server, &opts.context_name)?;
    write_atomic_0600(&opts.output, &rewritten, opts.force)?;
    log(&format!("kubeconfig written to {}", opts.output.display()));
    log(&format!(
        "register with:  argocd cluster add {} --kubeconfig {}",
        opts.context_name,
        opts.output.display()
    ));
    log(&format!(
        "sanity check:   KUBECONFIG={} kubectl get nodes",
        opts.output.display()
    ));
    if opts.kvm_host.is_some() {
        log("note: ProxyJump used for fetch — direct 'kubectl get nodes' from");
        log("      this workstation may fail (apiserver isn't reachable here);");
        log("      that is expected and does not invalidate the kubeconfig.");
    }
    Ok(())
}

/// Pre-check: refuse to clobber an existing file (unless `--force`),
/// refuse a symlink in either case (bash twin lines 299-309).
fn pre_check_output(output: &Path, force: bool) -> Result<()> {
    let meta = match fs::symlink_metadata(output) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(anyhow!(e)).with_context(|| format!("stat {}", output.display()));
        }
    };
    if meta.file_type().is_symlink() {
        let target = fs::read_link(output).unwrap_or_default();
        bail!(
            "OUTPUT is a symlink ({} -> {}); re-run with --output={} or \
             remove the symlink first",
            output.display(),
            target.display(),
            target.display(),
        );
    }
    if !force {
        bail!(
            "output file already exists: {} (re-run with --force to overwrite)",
            output.display()
        );
    }
    Ok(())
}

/// SSH to `root@$CP_IP` (ProxyJump=$KVM_HOST when set) and run
/// `sudo cat /etc/kubernetes/admin.conf`. Returns the captured stdout
/// as raw bytes — we don't UTF-8-validate here, the rewrite step does
/// line-oriented work which tolerates an invalid byte by passing it
/// through verbatim.
///
/// Bash twin: `cp_ssh "sudo cat /etc/kubernetes/admin.conf" | tr -d '\r'`
/// at lines 347-349. The bash side uses `ssh -t` and then strips the
/// CR injection — both of which are the #307 bug. The Rust path uses
/// non-TTY SSH (BatchMode=yes default) so no CR injection happens and
/// no `tr -d '\r'` is needed.
fn fetch_admin_conf(opts: &ExportOptions) -> Result<Vec<u8>> {
    let target = opts.target();
    let out =
        cp_ssh_capture(&target, "sudo cat /etc/kubernetes/admin.conf").with_context(|| {
            format!(
                "ssh root@{} 'sudo cat /etc/kubernetes/admin.conf' failed — \
                 is the CP up and reachable?",
                opts.cp_ip
            )
        })?;
    if out.stdout.is_empty() {
        bail!("fetched admin.conf is empty");
    }
    Ok(out.stdout)
}

/// Sanity-check the fetched file looks like a kubeconfig. Bash twin
/// lines 352-356 — `grep -q '^apiVersion:' && grep -q '^kind: Config'`.
fn sanity_check_kubeconfig(raw: &[u8]) -> Result<()> {
    let s = std::str::from_utf8(raw)
        .context("fetched admin.conf was not UTF-8 (kubeconfigs are YAML, must be UTF-8)")?;
    let has_apiversion = s.lines().any(|l| l.starts_with("apiVersion:"));
    let has_kind = s.lines().any(|l| l.starts_with("kind: Config"));
    if !has_apiversion || !has_kind {
        bail!(
            "fetched file does not look like a kubeconfig (no 'kind: Config'). \
             Refusing to continue."
        );
    }
    Ok(())
}

/// Line-anchored rewrite of `server:` + the kubeadm-default
/// `kubernetes` / `kubernetes-admin` / `kubernetes-admin@kubernetes`
/// names. Mirrors the bash twin's sed-fallback path (`rewrite_kubeconfig`
/// at line 95) — same six anchored substitutions in the same order so
/// the composite `kubernetes-admin@kubernetes` is rewritten before the
/// bare `kubernetes-admin` and `kubernetes` patterns.
///
/// We don't depend on Go yq; the sed path is structurally equivalent
/// to the yq-edit path bash takes when Go yq is present (bash twin
/// `detect_yq_flavor` at line 71 picks the path; the sed branch is
/// what the bash twin runs in CI / minimal-tools environments).
fn rewrite_kubeconfig(raw: &[u8], server_url: &str, context_name: &str) -> Result<Vec<u8>> {
    let input = std::str::from_utf8(raw).context("kubeconfig was not UTF-8 in rewrite step")?;
    let mut out = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        // Capture trailing newline so we emit it verbatim. `split_inclusive`
        // keeps it; rewrite operates on the stripped form.
        let (body, newline) = match line.strip_suffix('\n') {
            Some(s) => (s, "\n"),
            None => (line, ""),
        };
        let rewritten = rewrite_line(body, server_url, context_name);
        out.push_str(&rewritten);
        out.push_str(newline);
    }
    Ok(out.into_bytes())
}

/// Apply the six anchored substitutions to a single line. Returns
/// the rewritten line (or the original when no pattern matched).
fn rewrite_line(line: &str, server_url: &str, context_name: &str) -> String {
    // ---- server: rewrite ----
    // Match `^([[:space:]]+server:[[:space:]]+).*$` then substitute the
    // capture group + new URL.
    if let Some(rest) = line.strip_prefix_with_indent("server:") {
        let (indent, _after) = rest;
        return format!("{indent}server: {server_url}");
    }

    // ---- kubernetes-admin@kubernetes composite ----
    // Two patterns from bash twin:
    //   ^([[:space:]]+| - )name: kubernetes-admin@kubernetes$
    //   ^(current-context: )kubernetes-admin@kubernetes$
    if let Some(prefix) = match_name_kv(line, "kubernetes-admin@kubernetes") {
        return format!("{prefix}{context_name}");
    }
    if let Some(prefix) = line.strip_suffix("kubernetes-admin@kubernetes")
        && prefix.starts_with("current-context: ")
        && prefix == "current-context: "
    {
        return format!("{prefix}{context_name}");
    }

    // ---- kubernetes-admin (bare user) ----
    if let Some(prefix) = match_name_kv(line, "kubernetes-admin") {
        return format!("{prefix}{context_name}");
    }
    if let Some(suffix) = line.strip_prefix_with_indent("user:")
        && let (indent, after) = suffix
        && after == " kubernetes-admin"
    {
        return format!("{indent}user: {context_name}");
    }

    // ---- kubernetes (bare cluster) ----
    if let Some(prefix) = match_name_kv(line, "kubernetes") {
        return format!("{prefix}{context_name}");
    }
    if let Some(suffix) = line.strip_prefix_with_indent("cluster:")
        && let (indent, after) = suffix
        && after == " kubernetes"
    {
        return format!("{indent}cluster: {context_name}");
    }

    line.to_string()
}

/// Strip-and-yield helper: if `line` is `<whitespace>?<key><rest>`,
/// returns `Some((leading_ws, rest_after_key))`. Otherwise None.
trait LineStripExt {
    fn strip_prefix_with_indent(&self, key: &str) -> Option<(&str, &str)>;
}

impl LineStripExt for str {
    fn strip_prefix_with_indent(&self, key: &str) -> Option<(&str, &str)> {
        // Find the first non-whitespace character — that's the start of
        // the key. If the prefix matches, split into (indent, rest_after_key).
        let trimmed = self.trim_start();
        if !trimmed.starts_with(key) {
            return None;
        }
        let indent_len = self.len() - trimmed.len();
        let indent = &self[..indent_len];
        let after = &trimmed[key.len()..];
        Some((indent, after))
    }
}

/// Match `^([[:space:]]+| - )name: <target>$`. Returns the
/// match-up-to-and-including-`name: ` prefix on success.
fn match_name_kv(line: &str, target: &str) -> Option<String> {
    // bash regex was `([[:space:]]+|- )name:[[:space:]]+`.
    // Two shapes: indented `    name: <target>` or list `- name: <target>`
    // (the latter is what kubeconfig YAML uses for `clusters: - cluster: ...
    // name: <name>` array entries).
    let (indent, rest) = if let Some(rest) = line.strip_prefix("- ") {
        ("- ", rest)
    } else {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("name:") {
            return None;
        }
        let indent_len = line.len() - trimmed.len();
        if indent_len == 0 {
            // bash anchors on `[[:space:]]+` — at least one ws char.
            return None;
        }
        (&line[..indent_len], trimmed)
    };
    let rest = rest.strip_prefix("name:")?;
    // Bash required `[[:space:]]+` after `name:` — at least one ws.
    if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        return None;
    }
    let value = rest.trim_start();
    if value != target {
        return None;
    }
    // Reconstruct the leading "<indent>name: " portion so the caller
    // can append the new name.
    let post_key = &rest[..rest.len() - value.len()];
    Some(format!("{indent}name:{post_key}"))
}

/// Write `contents` to `path` atomically at mode 0600. Mirrors
/// bash twin's `install -m 0600`. When `force` is set and the
/// destination exists, snapshot it to `<path>.bak-<UTC>` first (mode
/// 0600 inherited).
///
/// Implementation: write to `<path>.tmp-<rand>`, fsync, rename. The
/// rename is atomic on POSIX (same filesystem), and the temp file's
/// mode is set at `open(O_CREAT, 0o600)` time so the file is never
/// world-readable.
fn write_atomic_0600(path: &Path, contents: &[u8], force: bool) -> Result<()> {
    use std::io::Write;
    // Backup on overwrite (bash twin lines 373-386).
    if force && path.exists() {
        let bak = backup_path(path);
        if bak.exists() {
            bail!(
                "backup target already exists: {} (refusing to clobber)",
                bak.display()
            );
        }
        fs::copy(path, &bak)
            .with_context(|| format!("backup: copy {} to {}", path.display(), bak.display()))?;
        // Set mode 0600 on the backup (the install copy inherits whatever
        // the source had; we re-pin for safety).
        let mut perms = fs::metadata(&bak)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o600);
        fs::set_permissions(&bak, perms)?;
        log(&format!(
            "backup: copied {} to {} (will be overwritten next)",
            path.display(),
            bak.display()
        ));
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|f| f.to_string_lossy())
            .unwrap_or_else(|| "kubeconfig".into()),
        std::process::id(),
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("open temp file {}", tmp.display()))?;
        f.write_all(contents)
            .with_context(|| format!("write temp file {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync temp file {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "rename {} -> {} (atomic install)",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Compute backup path `<path>.bak-<UTC-YYYYMMDDTHHMMSSnnnnnnnnnUTC>`.
/// Bash twin uses GNU `date -u +%Y%m%dT%H%M%S%N%Z` (nanosecond
/// precision). We approximate via `SystemTime::now()` and emit the
/// same shape.
fn backup_path(path: &Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Convert epoch seconds to UTC YYYYMMDDTHHMMSS via chrono-free code
    // (we don't pull chrono into the workspace just for backup naming).
    // Use `time_t` decomposition: derive days since 1970 -> year/month/day.
    let secs = now.as_secs() as i64;
    let nsecs = now.subsec_nanos();
    let (y, mo, d, h, mi, s) = epoch_to_utc_ymdhms(secs);
    let stamp = format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}{nsecs:09}UTC");
    let mut bak = path.as_os_str().to_owned();
    bak.push(format!(".bak-{stamp}"));
    PathBuf::from(bak)
}

/// Minimal epoch → UTC (Y, Mo, D, H, M, S) decomposition. Civil
/// calendar algorithm from Howard Hinnant's "chrono-Compatible Low-Level
/// Date Algorithms" — public domain. Inlined to keep the workspace
/// dep-free for a one-call-site formatter. Year valid for 1970+.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn epoch_to_utc_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400) as u32;
    let h = secs_of_day / 3600;
    let mi = (secs_of_day / 60) % 60;
    let s = secs_of_day % 60;
    // Convert `days since 1970-01-01` to YMD.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

/// Operator-visible log line. Mirrors `setup_logging "[export-argocd]"`
/// in the bash twin. Writes to stdout (same channel bash's `log`
/// helper uses).
fn log(msg: &str) {
    println!("[export-argocd] {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_KUBECONFIG: &str = r#"apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: REDACTED
    server: https://10.0.0.5:6443
  name: kubernetes
contexts:
- context:
    cluster: kubernetes
    user: kubernetes-admin
  name: kubernetes-admin@kubernetes
current-context: kubernetes-admin@kubernetes
kind: Config
preferences: {}
users:
- name: kubernetes-admin
  user:
    client-certificate-data: REDACTED
    client-key-data: REDACTED
"#;

    #[test]
    fn rewrite_replaces_server_url() {
        let out = rewrite_kubeconfig(
            SAMPLE_KUBECONFIG.as_bytes(),
            "https://10.99.99.99:6443",
            "hummingbird-cp1",
        )
        .unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains("    server: https://10.99.99.99:6443"),
            "expected new server URL, got:\n{s}"
        );
        assert!(
            !s.contains("https://10.0.0.5:6443"),
            "old server URL still present:\n{s}"
        );
    }

    #[test]
    fn rewrite_renames_cluster_context_user() {
        let out = rewrite_kubeconfig(
            SAMPLE_KUBECONFIG.as_bytes(),
            "https://10.99.99.99:6443",
            "hummingbird-cp1",
        )
        .unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        // Six substitutions:
        //   cluster name, context name (composite), user name,
        //   contexts[0].context.cluster, contexts[0].context.user,
        //   current-context.
        assert!(s.contains("  name: hummingbird-cp1"), "cluster name:\n{s}");
        assert!(
            s.contains("    cluster: hummingbird-cp1"),
            "context.cluster:\n{s}"
        );
        assert!(
            s.contains("    user: hummingbird-cp1"),
            "context.user:\n{s}"
        );
        assert!(
            s.contains("current-context: hummingbird-cp1"),
            "current-context:\n{s}"
        );
        assert!(
            !s.contains("kubernetes-admin@kubernetes"),
            "composite still present:\n{s}"
        );
        assert!(
            !s.contains("name: kubernetes\n"),
            "bare 'kubernetes' name still present:\n{s}"
        );
        assert!(
            !s.contains("name: kubernetes-admin\n"),
            "bare 'kubernetes-admin' still present:\n{s}"
        );
    }

    /// The sed-fallback path must NOT rewrite the substring
    /// "kubernetes" inside base64 cert blobs or comments. We test the
    /// comment-rewrite case with a `# kubernetes` line — the
    /// line-anchored substitutions should leave it untouched.
    #[test]
    fn rewrite_does_not_touch_comments_or_non_key_lines() {
        let raw = r#"# kubernetes is the platform
apiVersion: v1
kind: Config
# kubernetes-admin is the kubeadm-default
clusters:
- cluster:
    server: https://10.0.0.5:6443
  name: kubernetes
"#;
        let out = rewrite_kubeconfig(raw.as_bytes(), "https://x:6443", "hbird").unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        assert!(
            s.contains("# kubernetes is the platform"),
            "comment was rewritten:\n{s}"
        );
        assert!(
            s.contains("# kubernetes-admin is the kubeadm-default"),
            "comment was rewritten:\n{s}"
        );
        assert!(s.contains("  name: hbird"), "cluster name rename:\n{s}");
    }

    #[test]
    fn sanity_check_accepts_real_kubeconfig() {
        sanity_check_kubeconfig(SAMPLE_KUBECONFIG.as_bytes()).unwrap();
    }

    #[test]
    fn sanity_check_rejects_missing_apiversion() {
        let s = "kind: Config\n";
        let err = sanity_check_kubeconfig(s.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("does not look like a kubeconfig"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn sanity_check_rejects_missing_kind() {
        let s = "apiVersion: v1\n";
        let err = sanity_check_kubeconfig(s.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("does not look like a kubeconfig"),
            "wrong error: {err}"
        );
    }

    /// #307 regression: sanity check would fail if the SSH layer
    /// prepended an OSC session-start escape to stdout. Verify the
    /// rejection fires (Rust never produces this byte sequence
    /// because BatchMode=yes — but if the bug ever regressed, this
    /// would catch it).
    #[test]
    fn sanity_check_rejects_osc_prefix() {
        let mut s = String::new();
        // ESC ]3008;1\u{07} — the OSC session-start sequence modern
        // sudo emits to the controlling terminal.
        s.push('\x1b');
        s.push_str("]3008;1\x07");
        s.push_str(SAMPLE_KUBECONFIG);
        let err = sanity_check_kubeconfig(s.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("does not look like a kubeconfig"),
            "expected OSC-prefixed input to be rejected: {err}"
        );
    }

    #[test]
    fn validate_context_name_accepts_normal() {
        validate_context_name("hummingbird-cp1").unwrap();
        validate_context_name("cp_1.test").unwrap();
        validate_context_name("a").unwrap();
    }

    #[test]
    fn validate_context_name_rejects_metachars() {
        for bad in ["foo bar", "x;y", "$(whoami)", "a/b", ""] {
            assert!(
                validate_context_name(bad).is_err(),
                "should reject: {bad:?}"
            );
        }
    }

    #[test]
    fn backup_path_carries_bak_suffix() {
        let p = PathBuf::from("/tmp/argocd-kubeconfig.yaml");
        let bak = backup_path(&p);
        let s = bak.to_string_lossy();
        assert!(s.contains(".bak-"), "no .bak- suffix: {s}");
        assert!(s.ends_with("UTC"), "no UTC marker: {s}");
        assert!(s.starts_with("/tmp/argocd-kubeconfig.yaml.bak-"));
    }

    #[test]
    fn epoch_to_utc_ymdhms_matches_known_value() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let (y, mo, d, h, mi, s) = epoch_to_utc_ymdhms(1_704_067_200);
        assert_eq!((y, mo, d, h, mi, s), (2024, 1, 1, 0, 0, 0));
        // 2026-05-25 10:34:56 UTC = 1779705296 (cross-checked via
        // `date -u -d @1779705296`).
        let (y, mo, d, h, mi, s) = epoch_to_utc_ymdhms(1_779_705_296);
        assert_eq!((y, mo, d, h, mi, s), (2026, 5, 25, 10, 34, 56));
        // Mid-month leap-year boundary — 2024-02-29 12:00:00 UTC =
        // 1709208000. Pins the Hinnant civil-calendar correctness for
        // the one date class easiest to get wrong.
        let (y, mo, d, h, mi, s) = epoch_to_utc_ymdhms(1_709_208_000);
        assert_eq!((y, mo, d, h, mi, s), (2024, 2, 29, 12, 0, 0));
    }
}
