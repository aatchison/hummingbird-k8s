//! `hbird etcd <sub>` — bash twins: `scripts/backup-etcd.sh`,
//! `scripts/restore-etcd.sh`, `scripts/rotate-etcd-encryption-key.sh`
//! (via `make backup-etcd LABEL=…`, `make restore-etcd SNAP=…`,
//! `make rotate-etcd-key`).
//!
//! The three bash twins share one shape: resolve the control-plane IP,
//! SSH to `root@$CP_IP`, and drive `etcdctl` (through `crictl exec` for
//! the running static pod, or through `podman run` for the offline
//! restore). This module keeps that shape and reuses the proven
//! [`crate::cp_kubectl`] / [`crate::cp_resolve`] / [`crate::virt_bridge`]
//! plumbing instead of re-implementing SSH option handling.
//!
//! # Mapping from bash twin → Rust function
//!
//! | bash twin                                    | Rust fn                          |
//! |----------------------------------------------|----------------------------------|
//! | `backup-etcd.sh` snapshot block              | [`backup_with_exec`]             |
//! | `backup-etcd.sh` `--label` sanitize          | [`sanitize_label`]               |
//! | `restore-etcd.sh` confirmation banner        | [`restore_banner`]               |
//! | `restore-etcd.sh` remote heredoc             | [`restore_apply_with_exec`]      |
//! | `restore-etcd.sh` `ETCD_IMG=` awk pipeline   | [`pick_etcd_image`]              |
//! | `rotate-…-key.sh` Stage 0 fetch              | [`rotate_fetch_config_with_exec`]|
//! | `rotate-…-key.sh` Stage 1 python3/PyYAML     | [`insert_primary_key`]           |
//! | `rotate-…-key.sh` Stage 2 re-encrypt         | [`rotate_reencrypt_with_exec`]   |
//! | `rotate-…-key.sh` Stage 3 python3/PyYAML     | [`drop_non_primary_keys`]        |
//! | `rotate-…-key.sh` Stage 4 verify             | [`rotate_verify_with_exec`]      |
//!
//! Every helper that classifies an exit code or parses remote output has
//! a `…_with_exec(exec: &impl hbird_ssh::SshExec, …)` twin so the branch
//! is unit-testable without a live cluster (the [`crate::commands::update_cluster`]
//! `timer_stop` / `timer_start` pattern).
//!
//! # Deliberate divergences from the bash twins
//!
//! 1. **No `scp`.** `hbird-ssh` has no scp wrapper and adding one is out
//!    of scope, so the snapshot bytes ride the same SSH channel:
//!    `cat <remote>` to fetch (raw bytes via [`hbird_ssh::RunOutput::stdout`])
//!    and `cat > <remote>` with piped stdin to push. Same bytes, one
//!    transport, no second auth round-trip.
//! 2. **Bug fix — snapshot destination (`backup-etcd.sh`).** The bash
//!    twin runs `crictl exec … etcdctl snapshot save /tmp/snapshot.db`
//!    (which writes inside the *etcd container's* mount namespace) and
//!    then `scp root@$CP_IP:/tmp/snapshot.db` (which reads the *host's*
//!    `/tmp`). The kubeadm etcd static pod does not bind-mount the host
//!    `/tmp` — its own comment admits the uncertainty. We write to
//!    `/var/lib/etcd/…`, which *is* a real hostPath mount of the etcd
//!    pod, so the file the fetch reads is the file etcdctl wrote. The
//!    scratch file is removed afterwards.
//! 3. **Bug fix — re-entrancy guard (`restore-etcd.sh`).** The bash twin
//!    runs `mv /etc/kubernetes/manifests /etc/kubernetes/manifests.disabled`
//!    unconditionally. After an aborted restore, `manifests.disabled`
//!    already exists and `mv` silently nests the live directory *inside*
//!    it (`manifests.disabled/manifests`), which no later step undoes.
//!    We refuse to start when `manifests.disabled` already exists.
//! 4. **Bug fix — no rollback (`restore-etcd.sh`).** The bash twin's
//!    remote block runs under `set -euo pipefail`; if `podman run …
//!    snapshot restore` fails, the script dies with the manifests
//!    directory still moved aside and kubelet still stopped — the CP
//!    stays down with no hint. We attempt a best-effort rollback
//!    (move the manifests back, start kubelet) and say so before
//!    failing.
//! 5. **`--dry-run`** exists on all three subcommands; the bash twins have
//!    no equivalent. Dry-run never opens an SSH connection, so the plan
//!    is deterministic (runtime-resolved values render as `<cp-ip>` /
//!    `<UTC-timestamp>` / `<etcd-container-id>` placeholders).
//! 6. **Confirmation.** `restore-etcd.sh` accepts a bare Enter
//!    (`read -r _`); we require an explicit `y` (the wording + `[y/N]`
//!    shape `rotate-etcd-encryption-key.sh::confirm` already uses) and
//!    add `--yes` for non-interactive callers. A non-TTY stdin without
//!    `--yes` is refused rather than silently proceeding.
//! 7. **`command -v hbird` preflight** is not ported: we *are* `hbird`.
//!
//! Operator-visible wording that the bash twins print (and operators
//! grep for) is preserved verbatim; each such site is marked with a
//! `bash twin wording` comment.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};

use crate::cp_kubectl::{CpTarget, cp_kubectl_raw};
use crate::cp_resolve::resolve_cp_ip_via_ssh;
use crate::virt_bridge::build_connection;

// ---- clap surface ---------------------------------------------------------

/// Top-level `hbird etcd` — dispatches to one of three sub-subcommands.
/// Nested exactly like `hbird verify <encryption|hardening|app-deploy>`.
#[derive(Debug, Args)]
pub struct EtcdArgs {
    /// The chosen etcd operation.
    #[command(subcommand)]
    pub command: EtcdSubcommand,
}

/// The three etcd `Makefile` targets.
#[derive(Debug, Subcommand)]
pub enum EtcdSubcommand {
    /// Take an etcd snapshot from the running control plane.
    ///
    /// Bash twin: `scripts/backup-etcd.sh` (`make backup-etcd LABEL=…`).
    Backup(BackupArgs),

    /// Restore etcd from a snapshot taken by `hbird etcd backup`.
    ///
    /// DESTRUCTIVE. Bash twin: `scripts/restore-etcd.sh`
    /// (`make restore-etcd SNAP=…`).
    Restore(RestoreArgs),

    /// Rotate the etcd at-rest encryption key (#120).
    ///
    /// DESTRUCTIVE. Bash twin:
    /// `scripts/rotate-etcd-encryption-key.sh` (`make rotate-etcd-key`).
    RotateKey(RotateKeyArgs),
}

/// Flags shared by all three etcd subcommands. The bash twins take the
/// same values from the environment (`CONFIG`, `CP_NAME`, `CP_IP`,
/// `KVM_HOST`); clap's `env = …` keeps those spellings working.
#[derive(Debug, Args, Clone)]
pub struct EtcdCommonArgs {
    /// Path to `cluster.local.conf` (for `CP_NAME` / `CP_IP` /
    /// `KVM_HOST` lookup).
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,

    /// libvirt domain name of the control plane. Overrides `--config`.
    #[arg(long, value_name = "NAME", env = "CP_NAME")]
    pub cp_name: Option<String>,

    /// Static IP of the control plane. Overrides virsh resolution.
    #[arg(long, value_name = "IP", env = "CP_IP")]
    pub cp_ip: Option<String>,

    /// SSH alias of the KVM host (ProxyJump). Optional — when unset we
    /// run `virsh` locally, so `hbird` works *on* the KVM host too.
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// Plan-only mode — print what would happen, change nothing and
    /// open no SSH connection.
    #[arg(long)]
    pub dry_run: bool,
}

/// `hbird etcd backup` — bash twin `scripts/backup-etcd.sh`.
#[derive(Debug, Args)]
pub struct BackupArgs {
    /// Shared connection flags.
    #[command(flatten)]
    pub common: EtcdCommonArgs,

    /// Directory the snapshot is written to (bash twin's optional
    /// positional `[outdir]`, default `./backups`).
    #[arg(long, value_name = "DIR")]
    pub outdir: Option<PathBuf>,

    /// Bash-twin-compatible positional form: `hbird etcd backup ~/backups`.
    #[arg(value_name = "OUTDIR")]
    pub outdir_positional: Option<PathBuf>,

    /// Append `-<text>` to the snapshot filename so the reason for the
    /// snapshot is obvious on disk. Sanitized to `[A-Za-z0-9._-]`.
    /// `make backup-etcd LABEL=<text>`.
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,
}

/// `hbird etcd restore` — bash twin `scripts/restore-etcd.sh`.
#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Shared connection flags.
    #[command(flatten)]
    pub common: EtcdCommonArgs,

    /// Snapshot file to restore (`make restore-etcd SNAP=path.db`).
    #[arg(long, value_name = "PATH", env = "SNAP")]
    pub snapshot: PathBuf,

    /// Skip the interactive confirmation. Required for non-TTY callers
    /// (CI, `make` under a pipe) — see module docs, divergence 6.
    #[arg(long)]
    pub yes: bool,
}

/// `hbird etcd rotate-key` — bash twin
/// `scripts/rotate-etcd-encryption-key.sh`.
#[derive(Debug, Args)]
pub struct RotateKeyArgs {
    /// Shared connection flags.
    #[command(flatten)]
    pub common: EtcdCommonArgs,

    /// Skip every stage confirmation. The bash twin gates each of its
    /// four stages on `read`; this is the non-interactive escape hatch.
    #[arg(long)]
    pub yes: bool,
}

// ---- constants (bash twin literals) ---------------------------------------

/// TLS flag triple the bash twin passes to `etcdctl` inside the etcd
/// container (`backup-etcd.sh:141-144`). Kept as one literal so the two
/// surfaces stay diffable.
const ETCDCTL_TLS: &str = "--cacert=/etc/kubernetes/pki/etcd/ca.crt \
     --cert=/etc/kubernetes/pki/etcd/server.crt \
     --key=/etc/kubernetes/pki/etcd/server.key";

/// Where the snapshot is written on the CP before it is fetched.
/// `/var/lib/etcd` is a genuine hostPath mount of the kubeadm etcd
/// static pod — see module docs, divergence 2 (bash twin used
/// `/tmp/snapshot.db`, which is NOT shared with the host).
const REMOTE_SNAPSHOT_DIR: &str = "/var/lib/etcd";

/// Where `restore` uploads the snapshot on the CP. Matches the bash
/// twin verbatim (`restore-etcd.sh:97`); the restore runs `podman` on
/// the host, so the host's `/tmp` is the right place here.
const REMOTE_RESTORE_SNAPSHOT: &str = "/tmp/restore-snapshot.db";

/// Fallback etcd image when `crictl images` shows none. Bash twin:
/// `restore-etcd.sh:113`.
const ETCD_IMAGE_FALLBACK: &str = "registry.k8s.io/etcd:3.5.15-0";

/// On-CP path of the encryption config the apiserver reads.
const ENCRYPTION_CONFIG: &str = "/etc/kubernetes/encryption-config.yaml";

/// Seconds the bash twin waits for the apiserver to come back after a
/// static-pod manifest touch (`rotate-etcd-encryption-key.sh:172,229`)
/// and after a restore (`restore-etcd.sh:127`).
const APISERVER_WARMUP_SECS: u64 = 30;

/// `[rotate-etcd-key] ` — bash twin's `log()` prefix
/// (`rotate-etcd-encryption-key.sh:83`). Operators grep for it.
const ROTATE_PREFIX: &str = "[rotate-etcd-key]";

// ---- log helpers ----------------------------------------------------------

/// Bash twin's `log()` — `printf '[rotate-etcd-key] %s\n' "$*" >&2`.
/// Stderr, prefix preserved verbatim.
fn rotate_log(line: &str) {
    eprintln!("{ROTATE_PREFIX} {line}");
}

// ---- pure helpers: timestamps ---------------------------------------------

/// Split a Unix epoch second count into `(year, month, day, hour,
/// minute, second)` UTC. Howard Hinnant's `civil_from_days` algorithm —
/// no `chrono` dependency (the workspace ships none and this port adds
/// none).
fn civil_from_epoch(epoch_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = i64::try_from(epoch_secs / 86_400).unwrap_or(0);
    let rem = epoch_secs % 86_400;
    let hh = u32::try_from(rem / 3600).unwrap_or(0);
    let mm = u32::try_from((rem % 3600) / 60).unwrap_or(0);
    let ss = u32::try_from(rem % 60).unwrap_or(0);

    // Shift the epoch to 0000-03-01 so leap days land at the end of the
    // 400-year era and the month/day arithmetic becomes branch-free.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, hh, mm, ss)
}

/// `date -u +%Y%m%dT%H%M%SZ` — the snapshot-filename timestamp
/// (`backup-etcd.sh:108`, `restore-etcd.sh:99`).
fn format_utc_timestamp(epoch_secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil_from_epoch(epoch_secs);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// `key-$(date -u +%Y%m%d%H%M%S)` — the rotation key name
/// (`rotate-etcd-encryption-key.sh:135`). Note the *absence* of the
/// `T`/`Z` separators the snapshot timestamp carries; the two formats
/// are deliberately different in the bash twins and callers grep for
/// `key-2026…` in the apiserver's `k8s:enc:aesgcm:v1:<name>:` prefix.
fn rotation_key_name(epoch_secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil_from_epoch(epoch_secs);
    format!("key-{y:04}{m:02}{d:02}{hh:02}{mm:02}{ss:02}")
}

/// Seconds since the Unix epoch, or 0 if the clock is before 1970.
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- pure helpers: label sanitize -----------------------------------------

/// Bash twin (`backup-etcd.sh:113`):
///
/// ```sh
/// printf '%s' "$LABEL" | tr -c 'A-Za-z0-9._-' '-' | sed 's/^-*//; s/-*$//'
/// ```
///
/// Every char outside `[A-Za-z0-9._-]` becomes `-`, then leading and
/// trailing `-` runs are stripped. Returns `None` when nothing is left,
/// which the caller reports as the bash twin's
/// `--label resolved to empty after sanitize` (exit 2).
fn sanitize_label(raw: &str) -> Option<String> {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('-');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `etcd-snapshot-<ts>[-<label>].db` — bash twin `backup-etcd.sh:118`.
fn snapshot_file_name(timestamp: &str, label: Option<&str>) -> String {
    match label {
        Some(l) => format!("etcd-snapshot-{timestamp}-{l}.db"),
        None => format!("etcd-snapshot-{timestamp}.db"),
    }
}

// ---- pure helpers: remote-output parsing ----------------------------------

/// `crictl ps --name etcd -q | head -1` — first non-empty line, trimmed.
/// `None` when etcd is not running (bash twin then prints
/// `no etcd container found` and exits 1).
fn parse_etcd_container_id(stdout: &str) -> Option<&str> {
    stdout.lines().map(str::trim).find(|l| !l.is_empty())
}

/// The container ID is interpolated into a command string that the
/// remote `/bin/sh -c` evaluates, so it must not carry shell syntax.
/// crictl IDs are hex; we accept the wider `[0-9A-Za-z]` set and reject
/// everything else. Defense-in-depth twin of
/// [`crate::cp_kubectl`]'s metacharacter guard.
fn validate_container_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!(
            "refusing to use etcd container id {id:?}: expected an \
             alphanumeric crictl id (the id is interpolated into a \
             command evaluated by the CP's /bin/sh)"
        );
    }
    Ok(())
}

/// An image reference reaches the remote `podman run` the same way, so
/// it gets the same treatment. Accepts the registry/name:tag character
/// set (`[A-Za-z0-9._/:@-]`).
fn validate_image_ref(image: &str) -> Result<()> {
    let ok = !image.is_empty()
        && image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '-' | '@'));
    if !ok {
        bail!(
            "refusing to use etcd image {image:?}: unexpected characters \
             (the reference is interpolated into a command evaluated by \
             the CP's /bin/sh)"
        );
    }
    Ok(())
}

/// Bash twin (`restore-etcd.sh:110-113`):
///
/// ```sh
/// crictl images 2>/dev/null | awk '/registry\.k8s\.io\/etcd/{print $1":"$2; exit}'
/// ```
///
/// Returns the first `IMAGE:TAG` pair whose repository mentions
/// `registry.k8s.io/etcd`. `None` when no row matches — the caller
/// substitutes [`ETCD_IMAGE_FALLBACK`], exactly like the bash twin's
/// `[[ -n "$ETCD_IMG" ]] || ETCD_IMG=…`.
fn pick_etcd_image(images_stdout: &str) -> Option<String> {
    for line in images_stdout.lines() {
        if !line.contains("registry.k8s.io/etcd") {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if let (Some(repo), Some(tag)) = (cols.first(), cols.get(1)) {
            return Some(format!("{repo}:{tag}"));
        }
    }
    None
}

/// Approximate `du -h … | cut -f1` for the `Saved: … (<size>)` line
/// (`backup-etcd.sh:164`). GNU `du -h` reports *block* usage and rounds
/// up; we report apparent size with the same 1-decimal-below-10 shape,
/// so a 1 048 576-byte snapshot prints `1.0M` in both languages.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["", "K", "M", "G", "T"];
    let mut unit = 0usize;
    let mut scale = 1u64;
    while unit + 1 < UNITS.len() && bytes >= scale * 1024 {
        scale *= 1024;
        unit += 1;
    }
    if unit == 0 {
        return bytes.to_string();
    }
    // Round up to the nearest tenth, like `du -h`. The multiply happens
    // in u128 so `scale` stays exact (`scale / 10` would truncate and
    // report 1024 bytes as `1.1K`).
    let tenths = u64::try_from((u128::from(bytes) * 10).div_ceil(u128::from(scale))).unwrap_or(0);
    if tenths < 100 {
        format!("{}.{}{}", tenths / 10, tenths % 10, UNITS[unit])
    } else {
        format!("{}{}", bytes.div_ceil(scale), UNITS[unit])
    }
}

// ---- pure helpers: base64 + key material ----------------------------------

/// Standard RFC 4648 base64 with padding — the encoding
/// `head -c 32 /dev/urandom | base64 -w0` produces
/// (`rotate-etcd-encryption-key.sh:134`, `containers/k8s/k8s-init.sh:120`).
/// Hand-rolled because the workspace has no base64 dependency and this
/// port is not allowed to add one.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[((n >> 18) & 0x3f) as usize]));
        out.push(char::from(ALPHABET[((n >> 12) & 0x3f) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[((n >> 6) & 0x3f) as usize]));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[(n & 0x3f) as usize]));
        } else {
            out.push('=');
        }
    }
    out
}

/// `head -c 32 /dev/urandom` — 32 bytes of kernel CSPRNG output.
/// Reading `/dev/urandom` directly keeps the dependency count at zero
/// and matches what the bash twin (and `k8s-init.sh`) do.
fn random_key_bytes() -> Result<[u8; 32]> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom for a new etcd encryption key")?;
    f.read_exact(&mut buf)
        .context("read 32 bytes from /dev/urandom for a new etcd encryption key")?;
    Ok(buf)
}

// ---- pure helpers: EncryptionConfiguration rewriting ----------------------
//
// The bash twin shells out to `python3` + PyYAML to rewrite the
// providers' `keys:` array (Stage 1 prepends the new key, Stage 3 drops
// everything but the new key). Neither python3 nor PyYAML is a
// dependency this Rust port may take, and the workspace ships no YAML
// crate, so we edit the file line-wise instead.
//
// That is not a general YAML implementation and does not pretend to be
// one: it targets the exact document `containers/k8s/k8s-init.sh:130`
// writes (and the documents this very code emits), i.e.
//
//     resources:
//       - resources: [...]
//         providers:
//           - aesgcm:
//               keys:
//                 - name: bootstrap
//                   secret: <base64>
//           - identity: {}
//
// Any shape it cannot recognise is an error, never a silent no-op —
// mangling this file makes every existing Secret unreadable. As a bonus
// over the bash twin, the line-wise edit preserves comments and key
// order in the rest of the document (PyYAML's `safe_dump` round-trip
// discards both).

/// Locations of interest inside an `EncryptionConfiguration`'s aesgcm
/// `keys:` list.
#[derive(Debug, PartialEq, Eq)]
struct KeysBlock {
    /// Index of the `keys:` line itself.
    keys_line: usize,
    /// Column the `- name:` entries start at.
    entry_indent: usize,
    /// Line index of each `- …` entry under `keys:`, in document order.
    entries: Vec<usize>,
    /// First line index *after* the `keys:` block (exclusive end).
    end: usize,
}

/// Leading-space count of `line`.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Locate the `keys:` list of the `aesgcm` provider.
///
/// # Errors
///
/// - `no aesgcm provider found in encryption config` — bash twin's
///   verbatim `SystemExit` message (`rotate-etcd-encryption-key.sh:151`),
///   preserved because operators grep for it.
/// - A named error when `aesgcm:` exists but carries no `keys:` list or
///   no entries under it.
fn find_aesgcm_keys_block(lines: &[&str]) -> Result<KeysBlock> {
    let aes_idx = lines
        .iter()
        .position(|l| {
            let t = l.trim_start();
            let t = t.strip_prefix("- ").unwrap_or(t);
            t.starts_with("aesgcm:")
        })
        // bash twin wording (python3 SystemExit), preserved verbatim.
        .ok_or_else(|| anyhow!("no aesgcm provider found in encryption config"))?;
    let aes_indent = indent_of(lines[aes_idx]);

    let mut keys_line = None;
    for (i, line) in lines.iter().enumerate().skip(aes_idx + 1) {
        if line.trim().is_empty() {
            continue;
        }
        if indent_of(line) <= aes_indent {
            // Dedented back to the provider list — e.g. `- identity: {}`.
            break;
        }
        if line.trim() == "keys:" {
            keys_line = Some(i);
            break;
        }
    }
    let keys_line = keys_line
        .ok_or_else(|| anyhow!("aesgcm provider in encryption config has no `keys:` list"))?;
    let keys_indent = indent_of(lines[keys_line]);

    let mut entries = Vec::new();
    let mut entry_indent = None;
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(keys_line + 1) {
        if line.trim().is_empty() {
            continue;
        }
        let ind = indent_of(line);
        if ind <= keys_indent {
            end = i;
            break;
        }
        let is_entry = line.trim_start().starts_with("- ");
        match entry_indent {
            None if is_entry => {
                entry_indent = Some(ind);
                entries.push(i);
            }
            Some(e) if is_entry && ind == e => entries.push(i),
            _ => {}
        }
    }
    let entry_indent = entry_indent
        .ok_or_else(|| anyhow!("aesgcm provider's `keys:` list in encryption config is empty"))?;
    Ok(KeysBlock {
        keys_line,
        entry_indent,
        entries,
        end,
    })
}

/// Stage 1 (`rotate-etcd-encryption-key.sh:139-160`): prepend
/// `{name, secret}` to the aesgcm provider's `keys:` list so the NEW key
/// is primary (index 0 — the apiserver encrypts with it) while every
/// OLD key stays behind it (so existing rows still decrypt). The
/// trailing `identity` provider is untouched.
///
/// # Errors
///
/// Propagates [`find_aesgcm_keys_block`]'s diagnostics.
fn insert_primary_key(config: &str, name: &str, secret: &str) -> Result<String> {
    let lines: Vec<&str> = config.lines().collect();
    let block = find_aesgcm_keys_block(&lines)?;
    let at = *block
        .entries
        .first()
        .ok_or_else(|| anyhow!("aesgcm provider's `keys:` list in encryption config is empty"))?;
    let pad = " ".repeat(block.entry_indent);
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 2);
    out.extend(lines[..at].iter().map(|s| (*s).to_string()));
    out.push(format!("{pad}- name: {name}"));
    out.push(format!("{pad}  secret: {secret}"));
    out.extend(lines[at..].iter().map(|s| (*s).to_string()));
    Ok(joined(&out, config))
}

/// Stage 3 (`rotate-etcd-encryption-key.sh:203-217`): keep only the
/// first (new, primary) key entry and drop every older one, so the old
/// key material is no longer in use. Idempotent when only one key is
/// present.
///
/// # Errors
///
/// Propagates [`find_aesgcm_keys_block`]'s diagnostics.
fn drop_non_primary_keys(config: &str) -> Result<String> {
    let lines: Vec<&str> = config.lines().collect();
    let block = find_aesgcm_keys_block(&lines)?;
    let Some(&second) = block.entries.get(1) else {
        // Already single-key: nothing to drop (bash twin's `[:1]` slice
        // is a no-op in the same situation).
        return Ok(config.to_string());
    };
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    out.extend(lines[..second].iter().map(|s| (*s).to_string()));
    out.extend(lines[block.end..].iter().map(|s| (*s).to_string()));
    Ok(joined(&out, config))
}

/// Re-join edited lines, preserving whether the source ended with a
/// newline. `str::lines` drops the trailing terminator, and a
/// kubernetes config file without its final newline is ugly (though
/// harmless), so restore it when it was there.
fn joined(lines: &[String], original: &str) -> String {
    let mut s = lines.join("\n");
    if original.ends_with('\n') {
        s.push('\n');
    }
    s
}

// ---- confirmation ---------------------------------------------------------

/// Bash twin's `confirm()` predicate:
/// `[[ "$ans" =~ ^[Yy]$ ]]` (`rotate-etcd-encryption-key.sh:87`).
/// Trailing newline/whitespace is stripped before the test, so `y` and
/// `Y` pass and everything else (including `yes`) does not.
fn is_affirmative(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y")
}

/// Prompt on stderr, read one line from stdin, and return `Ok(())` only
/// on `y`/`Y`. `--yes` short-circuits.
///
/// # Errors
///
/// - `aborted by operator` — bash twin's verbatim wording
///   (`rotate-etcd-encryption-key.sh:88`); exit 1, matching bash.
/// - A refusal when stdin is not a TTY and `--yes` was not passed: the
///   bash twin would consume EOF as "not y" and abort anyway, but the
///   explicit message tells CI callers what to do.
fn confirm(prompt: &str, assume_yes: bool) -> Result<()> {
    if assume_yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "{prompt} [y/N] — stdin is not a TTY, refusing to guess. \
             Re-run with --yes to confirm non-interactively, or with \
             --dry-run to preview."
        );
    }
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("read confirmation from stdin")?;
    if is_affirmative(&answer) {
        Ok(())
    } else {
        // bash twin wording (`log "aborted by operator"`), exit 1.
        bail!("aborted by operator");
    }
}

// ---- plan: resolved CP target ---------------------------------------------

/// Resolved etcd-command target: CP IP + optional KVM-host ProxyJump,
/// plus the dry-run flag. Built once at the top of each subcommand.
#[derive(Debug, Clone)]
struct EtcdPlan {
    /// Forwarded into every [`crate::cp_kubectl`] call.
    target: CpTarget,
    /// Plan-only mode: no SSH connection is opened.
    dry_run: bool,
}

impl EtcdPlan {
    /// The ssh target as an operator would type it — `root@<ip>`, or
    /// `-J <kvm-host> root@<ip>` when a ProxyJump is in play. Rendered
    /// straight into the `DRY-RUN ssh …` plan lines so the printed
    /// command is one an operator can paste.
    fn ssh_target(&self) -> String {
        match self.target.kvm_host.as_deref() {
            Some(h) => format!("-J {h} root@{}", self.target.cp_ip),
            None => format!("root@{}", self.target.cp_ip),
        }
    }

    /// Build an SSH client for the CP. Live path only.
    fn exec(&self) -> hbird_ssh::Client {
        hbird_ssh::Client::new(self.target.cp_ssh_opts())
    }
}

/// Placeholder used for the CP IP in `--dry-run` when it would
/// otherwise have to be resolved over the network. Keeps the printed
/// plan deterministic (and the dry-run hermetic).
const DRY_RUN_CP_IP: &str = "<cp-ip>";

/// Resolve the plan from the shared flags.
///
/// CP_IP precedence (highest first), mirroring
/// [`crate::commands::kubectl::run`]'s resolver:
///
/// 1. `--cp-ip` / `CP_IP`.
/// 2. `CP_IP=` from `--config`.
/// 3. `ssh $KVM_HOST virsh -c qemu:///system domifaddr $CP_NAME`
///    (via [`resolve_cp_ip_via_ssh`]) when `KVM_HOST` is set.
/// 4. **Local** `virsh -c qemu:///system domifaddr $CP_NAME` when it is
///    not — so running `hbird` *on* the KVM host works without setting
///    `KVM_HOST` to itself (which would ProxyJump through the local
///    sshd and hang on root-login denial). Uses
///    [`crate::virt_bridge::build_connection`].
///
/// Under `--dry-run` steps 3 and 4 are skipped entirely and the IP
/// renders as [`DRY_RUN_CP_IP`].
///
/// # Errors
///
/// - config parse failures from `hbird_config::parse`.
/// - `CP_NAME required …` when resolution is needed but no domain name
///   is known.
/// - The resolver's own `Could not find … IP via ssh …` /
///   `has no IPv4 lease` diagnostics.
fn plan_from_common(common: &EtcdCommonArgs) -> Result<EtcdPlan> {
    let config = match &common.config {
        Some(path) => Some(hbird_config::parse(path).map_err(|e| anyhow!("{e}"))?),
        None => None,
    };

    let kvm_host = common
        .kvm_host
        .clone()
        .or_else(|| config.as_ref().and_then(|c| c.kvm_host.clone()))
        .filter(|s| !s.is_empty());

    let pinned = common
        .cp_ip
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| config.as_ref().and_then(|c| c.cp_ip.clone()))
        .filter(|s| !s.is_empty());

    let cp_ip = match (pinned, common.dry_run) {
        (Some(ip), _) => ip,
        (None, true) => DRY_RUN_CP_IP.to_string(),
        (None, false) => {
            let cp_name = common
                .cp_name
                .clone()
                .or_else(|| config.as_ref().map(|c| c.cp_name.clone()))
                .ok_or_else(|| {
                    anyhow!(
                        "CP_NAME required to resolve the control-plane IP \
                         (set in --config <cluster.local.conf>, or pass \
                         --cp-name / CP_NAME env, or pin --cp-ip / CP_IP)"
                    )
                })?;
            match kvm_host.as_deref() {
                Some(host) => {
                    let client =
                        hbird_ssh::Client::new(hbird_ssh::SshOptions::new(host.to_string()));
                    resolve_cp_ip_via_ssh(&client, host, &cp_name).with_context(|| {
                        format!(
                            "resolve CP_IP via virsh-domifaddr on KVM_HOST={host} \
                             for CP_NAME={cp_name}"
                        )
                    })?
                }
                // No KVM_HOST: we are (or claim to be) on the KVM host.
                None => resolve_cp_ip_local(&build_connection(None), &cp_name)?,
            }
        }
    };

    Ok(EtcdPlan {
        target: CpTarget { cp_ip, kvm_host },
        dry_run: common.dry_run,
    })
}

/// Resolve the CP IP with a **local** `virsh domifaddr` through
/// [`hbird_virt::Connection`]. Split out from [`plan_from_common`] so a
/// canned connection can drive both branches in a unit test.
///
/// # Errors
///
/// - `has no IPv4 lease yet` when libvirt knows the domain but has no
///   DHCP lease for it (VM still booting, or never started).
/// - The underlying `virsh` failure (binary missing, not in the
///   `libvirt` group, domain undefined) with context.
fn resolve_cp_ip_local(conn: &hbird_virt::Connection, cp_name: &str) -> Result<String> {
    match conn.domifaddr(cp_name) {
        Ok(Some(ip)) => Ok(ip.to_string()),
        Ok(None) => bail!(
            "libvirt domain '{cp_name}' has no IPv4 lease yet (queried via \
             local virsh). Pin CP_IP= in cluster.local.conf, or set \
             KVM_HOST=<ssh-alias> if libvirt runs on another host."
        ),
        Err(e) => Err(anyhow!(e)).with_context(|| {
            format!(
                "resolve CP_IP via local `virsh -c qemu:///system domifaddr \
                 {cp_name}` (set KVM_HOST=<ssh-alias> if libvirt runs on \
                 another host, or pin CP_IP=)"
            )
        }),
    }
}

// ---- backup ---------------------------------------------------------------

/// Everything the backup plan renderer needs. A struct (rather than six
/// positional params) keeps the renderer under the workspace's
/// `clippy::too_many_arguments` bar and lets the unit tests construct
/// cases by name.
#[derive(Debug)]
struct BackupPlanView<'a> {
    /// `root@<ip>[ via <host>]`.
    ssh_target: &'a str,
    /// Local destination path of the snapshot.
    dst: &'a str,
    /// Remote scratch path the snapshot is written to on the CP.
    remote_snapshot: &'a str,
}

/// Render the `--dry-run` plan for `hbird etcd backup`. Pure so the
/// exact line set is pinned by unit tests. Style matches
/// `update-cluster`'s `DRY-RUN ssh root@<ip> <command>` lines.
fn backup_dry_run_lines(v: &BackupPlanView<'_>) -> Vec<String> {
    let t = v.ssh_target;
    let remote = v.remote_snapshot;
    vec![
        "DRY-RUN etcd backup".to_string(),
        format!("DRY-RUN   control plane : {t}"),
        format!("DRY-RUN   snapshot file : {}", v.dst),
        format!("DRY-RUN ssh {t} crictl ps --name etcd -q"),
        format!(
            "DRY-RUN ssh {t} crictl exec <etcd-container-id> etcdctl {ETCDCTL_TLS} snapshot save {remote}"
        ),
        format!(
            "DRY-RUN ssh {t} crictl exec <etcd-container-id> etcdctl --write-out=table snapshot status {remote}"
        ),
        format!("DRY-RUN ssh {t} cat {remote} > {}", v.dst),
        format!("DRY-RUN ssh {t} rm -f {remote}"),
    ]
}

/// Snapshot the running etcd and return the snapshot bytes.
///
/// Bash twin: the `ssh … <<EOF` block plus the `scp` at
/// `backup-etcd.sh:135-160`, with the container-vs-host `/tmp` bug
/// fixed (module docs, divergence 2) and `scp` replaced by `cat` over
/// the same SSH channel (divergence 1).
///
/// # Errors
///
/// - `no etcd container found` (bash twin's verbatim wording, exit 1)
///   when `crictl ps --name etcd -q` prints nothing.
/// - A refusal when the container id is not alphanumeric
///   ([`validate_container_id`]).
/// - Context-wrapped SSH errors for the save / status / fetch / cleanup
///   steps, each naming the step that failed.
fn backup_with_exec(exec: &impl hbird_ssh::SshExec, remote_snapshot: &str) -> Result<Vec<u8>> {
    let ps = exec
        .run("crictl ps --name etcd -q")
        .context("etcd backup: `crictl ps --name etcd -q` failed on the CP")?;
    let stdout = ps.stdout_lossy();
    // bash twin wording: `echo 'no etcd container found' >&2; exit 1`.
    let id = parse_etcd_container_id(&stdout).ok_or_else(|| anyhow!("no etcd container found"))?;
    validate_container_id(id)?;

    exec.run(&format!(
        "crictl exec {id} etcdctl {ETCDCTL_TLS} snapshot save {remote_snapshot}"
    ))
    .context("etcd backup: `etcdctl snapshot save` failed inside the etcd container")?;

    // The bash twin sends the status table to /dev/null; surfacing it on
    // stderr is strictly additive (stdout stays clean for the
    // `Snapshotting …` / `Saved: …` lines operators grep).
    match exec.run(&format!(
        "crictl exec {id} etcdctl --write-out=table snapshot status {remote_snapshot}"
    )) {
        Ok(out) => {
            let table = out.stdout_lossy();
            if !table.trim().is_empty() {
                eprint!("{table}");
            }
        }
        Err(e) => bail!("etcd backup: `etcdctl snapshot status` failed: {e}"),
    }

    let fetched = exec
        .run(&format!("cat {remote_snapshot}"))
        .with_context(|| format!("etcd backup: could not fetch {remote_snapshot} from the CP"))?;
    if fetched.stdout.is_empty() {
        bail!(
            "etcd backup: {remote_snapshot} came back empty from the CP \
             (etcdctl reported success but wrote no bytes)"
        );
    }

    exec.run(&format!("rm -f {remote_snapshot}"))
        .with_context(|| format!("etcd backup: could not remove {remote_snapshot} on the CP"))?;

    Ok(fetched.stdout)
}

/// `hbird etcd backup` — bash twin `scripts/backup-etcd.sh`.
///
/// # Errors
///
/// Config / resolution failures, plus everything
/// [`backup_with_exec`] can raise. A `--label` that sanitizes to the
/// empty string exits 2 (bash twin's usage-error code) rather than
/// returning an error, because `anyhow` would collapse it to exit 1.
fn run_backup(args: BackupArgs) -> Result<()> {
    let label = match args.label.as_deref() {
        Some(raw) => match sanitize_label(raw) {
            Some(clean) => Some(clean),
            None => {
                // bash twin wording + exit code (`backup-etcd.sh:115`).
                eprintln!("--label resolved to empty after sanitize");
                std::process::exit(2);
            }
        },
        None => None,
    };

    let outdir = args
        .outdir
        .clone()
        .or_else(|| args.outdir_positional.clone())
        .unwrap_or_else(|| PathBuf::from("./backups"));

    let plan = plan_from_common(&args.common)?;

    if plan.dry_run {
        // Deterministic: the timestamp renders as a placeholder rather
        // than the wall clock.
        let dst = outdir.join(snapshot_file_name("<UTC-timestamp>", label.as_deref()));
        let remote = format!("{REMOTE_SNAPSHOT_DIR}/hbird-snapshot-<UTC-timestamp>.db");
        for line in backup_dry_run_lines(&BackupPlanView {
            ssh_target: &plan.ssh_target(),
            dst: &dst.display().to_string(),
            remote_snapshot: &remote,
        }) {
            println!("{line}");
        }
        return Ok(());
    }

    let ts = format_utc_timestamp(now_epoch_secs());
    let dst = outdir.join(snapshot_file_name(&ts, label.as_deref()));
    let remote = format!("{REMOTE_SNAPSHOT_DIR}/hbird-snapshot-{ts}.db");

    // bash: `mkdir -p "$OUTDIR"` before contacting the CP, so an
    // unwritable destination fails fast.
    std::fs::create_dir_all(&outdir)
        .with_context(|| format!("create snapshot output directory {}", outdir.display()))?;

    // bash twin wording (`backup-etcd.sh:134`).
    println!(
        "Snapshotting etcd on {} -> {}",
        plan.target.cp_ip,
        dst.display()
    );
    let bytes = backup_with_exec(&plan.exec(), &remote)?;
    std::fs::write(&dst, &bytes)
        .with_context(|| format!("write etcd snapshot to {}", dst.display()))?;

    // bash twin wording (`backup-etcd.sh:164`), including the two
    // spaces before the parenthesised size.
    println!(
        "Saved: {}  ({})",
        dst.display(),
        human_size(bytes.len() as u64)
    );
    Ok(())
}

// ---- restore --------------------------------------------------------------

/// The operator-facing warning block, verbatim from
/// `restore-etcd.sh:86-99` (minus its final "Press Enter" line, which
/// [`confirm`] replaces with an explicit `[y/N]` prompt — module docs,
/// divergence 6). Operators grep for "About to restore etcd on".
fn restore_banner(cp_ip: &str, snapshot: &str) -> String {
    format!(
        "About to restore etcd on {cp_ip} from {snapshot}.\n\
         \n\
         This will:\n\
         \x20 1. Move /etc/kubernetes/manifests aside so the apiserver+etcd static\n\
         \x20    pods stop.\n\
         \x20 2. Stop kubelet.\n\
         \x20 3. Rename /var/lib/etcd to /var/lib/etcd.before-restore.<ts>.\n\
         \x20 4. Run 'etcdctl snapshot restore' into a fresh /var/lib/etcd.\n\
         \x20 5. Restore the manifests directory and start kubelet so the apiserver\n\
         \x20    comes back up against the restored etcd.\n"
    )
}

/// Inputs to the remote restore script.
#[derive(Debug)]
struct RestoreApplyParams<'a> {
    /// `date -u +%Y%m%dT%H%M%SZ`; names the `/var/lib/etcd.before-restore.<ts>`
    /// keep-dir so a botched restore can be reverted by hand.
    timestamp: &'a str,
    /// Image `podman run` uses for `etcdctl snapshot restore`.
    etcd_image: &'a str,
}

/// Build the remote restore script. Byte-for-byte the bash twin's
/// heredoc (`restore-etcd.sh:101-124`) except that the etcd image is
/// resolved by [`detect_etcd_image_with_exec`] on this side (so the
/// operator sees which image was picked before anything is destroyed).
/// Pure, so the script text is pinned by a unit test.
fn restore_script(p: &RestoreApplyParams<'_>) -> String {
    let ts = p.timestamp;
    let img = p.etcd_image;
    format!(
        "set -euo pipefail\n\
         mv /etc/kubernetes/manifests /etc/kubernetes/manifests.disabled\n\
         sleep 10  # give kubelet time to notice and tear the static pods down\n\
         systemctl stop kubelet || true\n\
         mv /var/lib/etcd \"/var/lib/etcd.before-restore.{ts}\"\n\
         podman run --rm --network host \\\n\
         \x20 -v /var/lib:/var/lib -v /tmp:/tmp \\\n\
         \x20 \"{img}\" \\\n\
         \x20 etcdctl snapshot restore {REMOTE_RESTORE_SNAPSHOT} \\\n\
         \x20   --data-dir=/var/lib/etcd\n\
         mv /etc/kubernetes/manifests.disabled /etc/kubernetes/manifests\n\
         systemctl start kubelet\n\
         rm -f {REMOTE_RESTORE_SNAPSHOT}\n\
         echo 'Restore complete. Apiserver will come back up in ~30s.'\n"
    )
}

/// Best-effort undo for a restore that died partway. NOT in the bash
/// twin — see module docs, divergence 4. Only moves the manifests
/// directory back when it is actually missing, so it can never clobber
/// a live one, and always tries to bring kubelet back.
const RESTORE_ROLLBACK_SCRIPT: &str = "if [ -d /etc/kubernetes/manifests.disabled ] && \
     [ ! -e /etc/kubernetes/manifests ]; then \
     mv /etc/kubernetes/manifests.disabled /etc/kubernetes/manifests; fi; \
     systemctl start kubelet || true";

/// Refuse to start when a previous restore left
/// `/etc/kubernetes/manifests.disabled` behind. See module docs,
/// divergence 3 — without this the bash twin's unconditional `mv`
/// nests the live manifests directory inside the stale one.
///
/// # Errors
///
/// - A named error when the path exists (remote `test` exits non-zero).
/// - Context-wrapped SSH transport failures.
fn restore_preflight_with_exec(exec: &impl hbird_ssh::SshExec) -> Result<()> {
    match exec.run("test ! -e /etc/kubernetes/manifests.disabled") {
        Ok(_) => Ok(()),
        Err(hbird_ssh::Error::NonZeroExit { .. }) => bail!(
            "/etc/kubernetes/manifests.disabled already exists on the CP — \
             a previous restore was interrupted. Move the static-pod \
             manifests back by hand (`mv /etc/kubernetes/manifests.disabled \
             /etc/kubernetes/manifests`) and confirm the apiserver is up \
             before retrying; restoring now would nest the live manifests \
             directory inside the stale one."
        ),
        Err(e) => Err(e)
            .context("etcd restore: could not check /etc/kubernetes/manifests.disabled on the CP"),
    }
}

/// Upload the snapshot to the CP. Replaces the bash twin's
/// `scp "$SNAP" root@$CP_IP:/tmp/restore-snapshot.db`
/// (`restore-etcd.sh:97`) with a piped `cat >` over the same SSH
/// channel — module docs, divergence 1.
///
/// # Errors
///
/// Context-wrapped SSH failures (including a non-zero remote `cat`,
/// e.g. a full `/tmp`).
fn push_snapshot_with_exec(exec: &impl hbird_ssh::SshExec, bytes: &[u8]) -> Result<()> {
    exec.run_with_stdin(&format!("cat > {REMOTE_RESTORE_SNAPSHOT}"), bytes)
        .with_context(|| {
            format!("etcd restore: could not upload the snapshot to {REMOTE_RESTORE_SNAPSHOT}")
        })?;
    Ok(())
}

/// Detect the etcd image already present on the CP. Bash twin:
/// `ETCD_IMG=$(crictl images 2>/dev/null | awk …)` with the
/// `[[ -n … ]] || ETCD_IMG=<fallback>` default
/// (`restore-etcd.sh:110-113`).
///
/// The bash twin's `2>/dev/null` + pipeline means a failing `crictl`
/// still yields the fallback rather than aborting, so a non-zero remote
/// exit is *not* an error here either — only a transport failure is.
///
/// # Errors
///
/// Context-wrapped SSH transport failures (auth, ProxyJump, DNS).
fn detect_etcd_image_with_exec(exec: &impl hbird_ssh::SshExec) -> Result<String> {
    let stdout = match exec.run("crictl images") {
        Ok(out) => out.stdout_lossy(),
        // crictl missing / CRI socket down: bash swallows this too.
        Err(hbird_ssh::Error::NonZeroExit { stdout, .. }) => stdout,
        Err(e) => {
            return Err(e).context("etcd restore: `crictl images` failed on the CP");
        }
    };
    Ok(pick_etcd_image(&stdout).unwrap_or_else(|| ETCD_IMAGE_FALLBACK.to_string()))
}

/// Run the destructive restore, rolling back on failure.
///
/// The script goes over stdin to a remote `bash -s`, exactly like the
/// bash twin's `ssh … "TS=$TS bash -s" <<'REMOTE'` — no quoting layer,
/// and `set -euo pipefail` is guaranteed to be interpreted by bash.
///
/// Returns the remote stdout (which carries the bash twin's
/// `Restore complete. Apiserver will come back up in ~30s.` line).
///
/// # Errors
///
/// - A refusal when the image reference carries unexpected characters.
/// - The remote failure, annotated with whether the automatic rollback
///   (manifests back + kubelet started) succeeded. Module docs,
///   divergence 4.
fn restore_apply_with_exec(
    exec: &impl hbird_ssh::SshExec,
    params: &RestoreApplyParams<'_>,
) -> Result<String> {
    validate_image_ref(params.etcd_image)?;
    let script = restore_script(params);
    match exec.run_with_stdin("bash -s", script.as_bytes()) {
        Ok(out) => Ok(out.stdout_lossy()),
        Err(e) => {
            let rollback = exec.run_with_stdin("bash -s", RESTORE_ROLLBACK_SCRIPT.as_bytes());
            let note = match rollback {
                Ok(_) => "rollback attempted: static-pod manifests moved back and kubelet started",
                Err(ref re) => {
                    eprintln!("etcd restore: rollback itself failed: {re}");
                    "rollback FAILED — the CP may still have \
                     /etc/kubernetes/manifests.disabled and a stopped kubelet; \
                     fix by hand before retrying"
                }
            };
            Err(e).context(format!(
                "etcd restore: remote restore failed ({note}). The previous \
                 data dir was kept at /var/lib/etcd.before-restore.{}",
                params.timestamp
            ))
        }
    }
}

/// Render the `--dry-run` plan for `hbird etcd restore`.
fn restore_dry_run_lines(ssh_target: &str, snapshot: &str) -> Vec<String> {
    vec![
        "DRY-RUN etcd restore (DESTRUCTIVE — no confirmation prompt in dry-run)".to_string(),
        format!("DRY-RUN   control plane : {ssh_target}"),
        format!("DRY-RUN   snapshot file : {snapshot}"),
        format!("DRY-RUN ssh {ssh_target} test ! -e /etc/kubernetes/manifests.disabled"),
        format!("DRY-RUN ssh {ssh_target} cat > {REMOTE_RESTORE_SNAPSHOT}  < {snapshot}"),
        format!(
            "DRY-RUN ssh {ssh_target} crictl images   # pick the etcd image, else {ETCD_IMAGE_FALLBACK}"
        ),
        format!(
            "DRY-RUN ssh {ssh_target} bash -s   # mv manifests aside; stop kubelet; keep /var/lib/etcd as /var/lib/etcd.before-restore.<UTC-timestamp>; podman run <etcd-image> etcdctl snapshot restore; manifests back; start kubelet"
        ),
        format!(
            "DRY-RUN ssh {ssh_target} kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes   # after a {APISERVER_WARMUP_SECS}s warm-up"
        ),
    ]
}

/// `hbird etcd restore` — bash twin `scripts/restore-etcd.sh`.
///
/// # Errors
///
/// - `Snapshot not found: <path>` (bash twin's verbatim wording, exit 1).
/// - `aborted by operator` when the confirmation is declined.
/// - Everything [`restore_preflight_with_exec`] / [`push_snapshot_with_exec`]
///   / [`detect_etcd_image_with_exec`] / [`restore_apply_with_exec`] raise.
fn run_restore(args: RestoreArgs) -> Result<()> {
    let snap = args.snapshot.display().to_string();
    if !args.snapshot.is_file() {
        // bash twin wording (`restore-etcd.sh:71`), exit 1.
        bail!("Snapshot not found: {snap}");
    }

    let plan = plan_from_common(&args.common)?;
    if plan.dry_run {
        for line in restore_dry_run_lines(&plan.ssh_target(), &snap) {
            println!("{line}");
        }
        return Ok(());
    }

    let bytes =
        std::fs::read(&args.snapshot).with_context(|| format!("read etcd snapshot {snap}"))?;

    print!("{}", restore_banner(&plan.target.cp_ip, &snap));
    confirm("Proceed with the restore?", args.yes)?;

    let exec = plan.exec();
    restore_preflight_with_exec(&exec)?;
    push_snapshot_with_exec(&exec, &bytes)?;

    let image = detect_etcd_image_with_exec(&exec)?;
    // bash twin wording (`restore-etcd.sh:115`, emitted remotely there).
    println!("Using etcd image: {image}");

    let ts = format_utc_timestamp(now_epoch_secs());
    let out = restore_apply_with_exec(
        &exec,
        &RestoreApplyParams {
            timestamp: &ts,
            etcd_image: &image,
        },
    )?;
    print!("{out}");

    // bash twin wording (`restore-etcd.sh:126-128`).
    println!("Verifying cluster (after a 30s warm-up)...");
    sleep(Duration::from_secs(APISERVER_WARMUP_SECS));
    let nodes = cp_kubectl_raw(&plan.target, "get nodes")
        .context("etcd restore: post-restore `kubectl get nodes` failed")?;
    print!("{}", nodes.stdout_lossy());
    Ok(())
}

// ---- rotate-key -----------------------------------------------------------

/// Stage 0 (`rotate-etcd-encryption-key.sh:121-124`): fetch the current
/// `/etc/kubernetes/encryption-config.yaml` from the CP.
///
/// # Errors
///
/// - `fetched encryption-config is empty` — bash twin's verbatim
///   wording (exit 1) when the file is absent or zero-length.
/// - Context-wrapped SSH failures.
fn rotate_fetch_config_with_exec(exec: &impl hbird_ssh::SshExec) -> Result<String> {
    let out = exec
        .run(&format!("cat {ENCRYPTION_CONFIG}"))
        .with_context(|| {
            format!("rotate-etcd-key: could not read {ENCRYPTION_CONFIG} on the CP")
        })?;
    let text = out.stdout_lossy();
    if text.trim().is_empty() {
        // bash twin wording (`[[ -s "$BEFORE" ]] || { log …; exit 1; }`).
        bail!("fetched encryption-config is empty");
    }
    Ok(text)
}

/// Install a new EncryptionConfiguration on the CP and trigger an
/// apiserver reload. Bash twin: the `scp` + `ssh` pair at
/// `rotate-etcd-encryption-key.sh:162-171` (and again at 220-228).
///
/// Divergence: the staging file is written under `umask 077` instead of
/// arriving via `scp` (which would briefly leave key material in a
/// 0644 file). `install -m 0600` still sets the final mode, exactly
/// like the bash twin.
///
/// # Errors
///
/// Context-wrapped SSH failures from either the upload or the install.
fn rotate_install_config_with_exec(exec: &impl hbird_ssh::SshExec, yaml: &str) -> Result<()> {
    exec.run_with_stdin(
        &format!("umask 077; cat > {ENCRYPTION_CONFIG}.new"),
        yaml.as_bytes(),
    )
    .with_context(|| {
        format!("rotate-etcd-key: could not upload the new config to {ENCRYPTION_CONFIG}.new")
    })?;
    // Touching the static-pod manifest is the documented way to make the
    // kubelet re-create the apiserver pod, which re-reads the encryption
    // config file. The manifest contents do not need to change.
    let script = format!(
        "set -euo pipefail\n\
         install -m 0600 -o root -g root \\\n\
         \x20 {ENCRYPTION_CONFIG}.new \\\n\
         \x20 {ENCRYPTION_CONFIG}\n\
         rm -f {ENCRYPTION_CONFIG}.new\n\
         touch /etc/kubernetes/manifests/kube-apiserver.yaml\n"
    );
    exec.run_with_stdin("bash -s", script.as_bytes())
        .context("rotate-etcd-key: could not install the new encryption config on the CP")?;
    Ok(())
}

/// Post-reload health gate. Bash twin:
/// `KUBECONFIG=… kubectl get --raw=/healthz >/dev/null ||
///  { log "apiserver healthz failed after Stage N reload"; exit 1; }`
/// (`rotate-etcd-encryption-key.sh:174-177`, `231-234`).
///
/// # Errors
///
/// `apiserver healthz failed after Stage <n> reload` — bash twin's
/// verbatim wording, for BOTH a non-zero remote exit and a transport
/// failure (the bash `||` cannot tell them apart either).
fn rotate_healthz_with_exec(exec: &impl hbird_ssh::SshExec, stage: u8) -> Result<()> {
    let cmd = "KUBECONFIG=/etc/kubernetes/admin.conf kubectl get --raw=/healthz";
    match exec.run(cmd) {
        Ok(_) => Ok(()),
        Err(e) => bail!("apiserver healthz failed after Stage {stage} reload: {e}"),
    }
}

/// Stage 2 (`rotate-etcd-encryption-key.sh:185-197`): rewrite every
/// Secret and ConfigMap so each row is re-encrypted under the new
/// primary key.
///
/// `--force` is deliberately NOT used: it would delete-and-recreate the
/// object, changing its UID and breaking selectors.
///
/// # Errors
///
/// Context-wrapped SSH failures; a non-zero `kubectl replace` aborts
/// the rotation before Stage 3 drops the old key (which is the safe
/// order — the old key must stay readable while rows still use it).
fn rotate_reencrypt_with_exec(exec: &impl hbird_ssh::SshExec) -> Result<()> {
    let script = "set -euo pipefail\n\
         export KUBECONFIG=/etc/kubernetes/admin.conf\n\
         kubectl get secrets -A -o json | kubectl replace -f -\n\
         kubectl get configmaps -A -o json | kubectl replace -f -\n";
    exec.run_with_stdin("bash -s", script.as_bytes()).context(
        "rotate-etcd-key: Stage 2 re-encrypt failed. The new key is already \
         primary, so the old key MUST stay in the config — do not run Stage 3 \
         by hand until every Secret/ConfigMap has been rewritten.",
    )?;
    Ok(())
}

/// Stage 4 (`rotate-etcd-encryption-key.sh:242-244`): prove a Secret
/// reads back as `k8s:enc:aesgcm:v1:<new-key-name>:` by running the
/// on-image verifier with the stricter `EXPECTED_PREFIX`.
///
/// # Errors
///
/// Context-wrapped SSH failures, including a non-zero exit from
/// `/usr/libexec/verify-encryption.sh` (whose own
/// `[verify-encryption] FAIL:` lines land on stderr).
fn rotate_verify_with_exec(exec: &impl hbird_ssh::SshExec, key_name: &str) -> Result<()> {
    let prefix = format!("k8s:enc:aesgcm:v1:{key_name}:");
    let cmd = format!(
        "EXPECTED_PREFIX={} /usr/libexec/verify-encryption.sh",
        crate::cp_resolve::shell_single_quote(&prefix)
    );
    exec.run(&cmd).context(
        "rotate-etcd-key: Stage 4 verification failed — \
         /usr/libexec/verify-encryption.sh did not confirm the new key",
    )?;
    Ok(())
}

/// Render the `--dry-run` plan for `hbird etcd rotate-key`.
fn rotate_dry_run_lines(ssh_target: &str) -> Vec<String> {
    vec![
        "DRY-RUN etcd rotate-key (DESTRUCTIVE — no confirmation prompts in dry-run)".to_string(),
        format!("DRY-RUN   control plane : {ssh_target}"),
        "DRY-RUN   pre-flight    : take a labelled snapshot first (`hbird etcd backup --label pre-key-rotation`)".to_string(),
        format!("DRY-RUN ssh {ssh_target} cat {ENCRYPTION_CONFIG}   # Stage 0"),
        "DRY-RUN local  generate a 32-byte key from /dev/urandom, named key-<UTC-timestamp>".to_string(),
        format!("DRY-RUN ssh {ssh_target} cat > {ENCRYPTION_CONFIG}.new   # Stage 1: new key primary, old key kept"),
        format!("DRY-RUN ssh {ssh_target} bash -s   # Stage 1: install -m 0600 the config; touch /etc/kubernetes/manifests/kube-apiserver.yaml"),
        format!("DRY-RUN ssh {ssh_target} kubectl get --raw=/healthz   # after a {APISERVER_WARMUP_SECS}s wait"),
        format!("DRY-RUN ssh {ssh_target} bash -s   # Stage 2: kubectl get secrets/configmaps -A -o json | kubectl replace -f -"),
        format!("DRY-RUN ssh {ssh_target} cat > {ENCRYPTION_CONFIG}.new   # Stage 3: old key dropped"),
        format!("DRY-RUN ssh {ssh_target} bash -s   # Stage 3: install -m 0600 the config; touch /etc/kubernetes/manifests/kube-apiserver.yaml"),
        format!("DRY-RUN ssh {ssh_target} kubectl get --raw=/healthz   # after a {APISERVER_WARMUP_SECS}s wait"),
        format!("DRY-RUN ssh {ssh_target} EXPECTED_PREFIX='k8s:enc:aesgcm:v1:<new-key-name>:' /usr/libexec/verify-encryption.sh   # Stage 4"),
    ]
}

/// `hbird etcd rotate-key` — bash twin
/// `scripts/rotate-etcd-encryption-key.sh`.
///
/// Operator-driven: every destructive stage gates on a confirmation,
/// exactly like the bash twin (which never runs end-to-end without
/// prompts). `--yes` is the non-interactive escape hatch.
///
/// # Errors
///
/// - `aborted by operator` on a declined confirmation (exit 1).
/// - `fetched encryption-config is empty`,
///   `no aesgcm provider found in encryption config`,
///   `apiserver healthz failed after Stage <n> reload` — all bash-twin
///   wording.
fn run_rotate_key(args: RotateKeyArgs) -> Result<()> {
    let plan = plan_from_common(&args.common)?;
    if plan.dry_run {
        for line in rotate_dry_run_lines(&plan.ssh_target()) {
            println!("{line}");
        }
        return Ok(());
    }

    let exec = plan.exec();
    // bash twin wording (`rotate-etcd-encryption-key.sh:107-109`).
    rotate_log(&format!("control plane: {}", plan.target.cp_ip));
    rotate_log("pre-flight: did you 'make backup-etcd LABEL=pre-key-rotation' already?");
    rotate_log("            (see docs/backup-restore.md 'When to snapshot')");
    confirm("Continue with rotation?", args.yes)?;

    // ---- Stage 0: capture the current config ----
    rotate_log(&format!("fetching current {ENCRYPTION_CONFIG} from CP"));
    let before = rotate_fetch_config_with_exec(&exec)?;

    // ---- Stage 1: dual-key config (NEW primary, OLD secondary) ----
    let key_secret = base64_encode(&random_key_bytes()?);
    let key_name = rotation_key_name(now_epoch_secs());
    rotate_log(&format!(
        "generated new key (base64, 32 bytes) named '{key_name}'"
    ));
    let dual = insert_primary_key(&before, &key_name, &key_secret)?;

    rotate_log("Stage 1: install dual-key config (new=primary, old=secondary) + reload apiserver");
    confirm("Proceed with Stage 1?", args.yes)?;
    rotate_install_config_with_exec(&exec, &dual)?;
    rotate_log("waiting 30s for apiserver to come back with the new config");
    sleep(Duration::from_secs(APISERVER_WARMUP_SECS));
    rotate_healthz_with_exec(&exec, 1)?;

    // ---- Stage 2: re-encrypt every Secret and ConfigMap ----
    rotate_log("Stage 2: re-encrypt every existing Secret and ConfigMap");
    rotate_log("         (kubectl get -A -o json | kubectl replace -f -)");
    confirm("Proceed with Stage 2?", args.yes)?;
    rotate_reencrypt_with_exec(&exec)?;

    // ---- Stage 3: drop the OLD key ----
    let final_cfg = drop_non_primary_keys(&dual)?;
    rotate_log("Stage 3: drop old key from config + reload apiserver");
    confirm("Proceed with Stage 3?", args.yes)?;
    rotate_install_config_with_exec(&exec, &final_cfg)?;
    rotate_log("waiting 30s for apiserver to come back with the single-key config");
    sleep(Duration::from_secs(APISERVER_WARMUP_SECS));
    rotate_healthz_with_exec(&exec, 3)?;

    // ---- Stage 4: verify a Secret round-trips through the new key ----
    rotate_log("Stage 4: verify via /usr/libexec/verify-encryption.sh");
    rotate_verify_with_exec(&exec, &key_name)?;

    rotate_log(&format!(
        "rotation complete; new key name on CP: {key_name}"
    ));
    rotate_log("post-flight: consider 'make backup-etcd LABEL=post-key-rotation'");
    Ok(())
}

// ---- dispatch -------------------------------------------------------------

/// Dispatch `hbird etcd <backup|restore|rotate-key>`.
///
/// # Errors
///
/// Whatever the chosen subcommand raises; see [`run_backup`],
/// [`run_restore`], [`run_rotate_key`].
pub fn run(args: EtcdArgs) -> Result<()> {
    match args.command {
        EtcdSubcommand::Backup(a) => run_backup(a),
        EtcdSubcommand::Restore(a) => run_restore(a),
        EtcdSubcommand::RotateKey(a) => run_rotate_key(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hbird_ssh::{Error as SshErr, RunOutput, SshExec};
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::Mutex;

    // ---- fixtures ---------------------------------------------------------

    /// The EncryptionConfiguration `containers/k8s/k8s-init.sh:130`
    /// writes at cluster-init time. Every rewrite test starts here.
    const BOOTSTRAP_CONFIG: &str = "apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources:
      - secrets
      - configmaps
    providers:
      - aesgcm:
          keys:
            - name: bootstrap
              secret: Ym9vdHN0cmFwLWtleS0zMi1ieXRlcy1iYXNlNjQtLQ==
      - identity: {}
";

    /// Test-only [`SshExec`] returning canned responses from a FIFO
    /// queue, capturing both the command and any piped stdin. Same
    /// shape as `update_cluster::tests::MockSshExec`, plus stdin
    /// capture (the etcd port pipes scripts + snapshot bytes).
    struct MockSshExec {
        responses: Mutex<std::collections::VecDeque<Result<RunOutput, SshErr>>>,
        observed: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl MockSshExec {
        fn new(responses: Vec<Result<RunOutput, SshErr>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                observed: Mutex::new(Vec::new()),
            }
        }
        fn commands(&self) -> Vec<String> {
            self.observed
                .lock()
                .unwrap()
                .iter()
                .map(|(c, _)| c.clone())
                .collect()
        }
        fn stdins(&self) -> Vec<Vec<u8>> {
            self.observed
                .lock()
                .unwrap()
                .iter()
                .map(|(_, s)| s.clone())
                .collect()
        }
        fn pop(&self, command: &str, stdin: &[u8]) -> Result<RunOutput, SshErr> {
            self.observed
                .lock()
                .unwrap()
                .push((command.to_string(), stdin.to_vec()));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockSshExec: ran out of canned responses — extend the script")
        }
    }

    impl SshExec for MockSshExec {
        fn run(&self, command: &str) -> Result<RunOutput, SshErr> {
            self.pop(command, b"")
        }
        fn run_with_stdin(&self, command: &str, stdin: &[u8]) -> Result<RunOutput, SshErr> {
            self.pop(command, stdin)
        }
    }

    /// rc=0 with the given stdout.
    fn ok_stdout(s: &str) -> Result<RunOutput, SshErr> {
        Ok(RunOutput {
            status: ExitStatus::from_raw(0),
            stdout: s.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    /// rc=0 with raw (possibly non-UTF8) stdout — the snapshot fetch.
    fn ok_bytes(b: &[u8]) -> Result<RunOutput, SshErr> {
        Ok(RunOutput {
            status: ExitStatus::from_raw(0),
            stdout: b.to_vec(),
            stderr: Vec::new(),
        })
    }

    /// Non-zero remote exit (POSIX wait-status shape: code in the high
    /// byte), carrying optional stdout/stderr.
    fn nonzero_exit(code: i32, stderr: &str) -> Result<RunOutput, SshErr> {
        Err(SshErr::NonZeroExit {
            host: "test-host".to_string(),
            status: ExitStatus::from_raw((code & 0xff) << 8),
            stdout: String::new(),
            stderr: stderr.to_string(),
        })
    }

    /// Non-zero remote exit that still produced stdout (the
    /// `crictl images` case the bash twin swallows with `2>/dev/null`).
    fn nonzero_exit_with_stdout(code: i32, stdout: &str) -> Result<RunOutput, SshErr> {
        Err(SshErr::NonZeroExit {
            host: "test-host".to_string(),
            status: ExitStatus::from_raw((code & 0xff) << 8),
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    /// Transport-level failure (no remote exit code at all).
    fn transport_err() -> Result<RunOutput, SshErr> {
        Err(SshErr::Spawn {
            host: "test-host".to_string(),
            program: "ssh".to_string(),
            kind: hbird_ssh::SpawnKind::SshBinaryMissing,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "ssh: not found"),
        })
    }

    // ---- timestamps -------------------------------------------------------

    #[test]
    fn format_utc_timestamp_matches_date_u_format() {
        // Cross-checked against `date -u -d @<epoch> +%Y%m%dT%H%M%SZ`.
        assert_eq!(format_utc_timestamp(0), "19700101T000000Z");
        assert_eq!(format_utc_timestamp(1_700_000_000), "20231114T221320Z");
        assert_eq!(format_utc_timestamp(1_767_225_600), "20260101T000000Z");
    }

    /// Leap-day handling is the classic hand-rolled-calendar bug; 2000
    /// is both a leap year AND a century (the case a naive `% 4` rule
    /// gets wrong).
    #[test]
    fn format_utc_timestamp_handles_leap_day() {
        assert_eq!(format_utc_timestamp(951_782_400), "20000229T000000Z");
    }

    /// The rotation key name uses a DIFFERENT format from the snapshot
    /// timestamp (no `T`/`Z`) — pin both so a future refactor can't
    /// unify them by accident and break the `k8s:enc:aesgcm:v1:<name>:`
    /// prefix operators grep for.
    #[test]
    fn rotation_key_name_has_no_t_or_z_separators() {
        assert_eq!(rotation_key_name(1_700_000_000), "key-20231114221320");
        assert!(!rotation_key_name(1_700_000_000).contains('T'));
        assert!(!rotation_key_name(1_700_000_000).contains('Z'));
    }

    // ---- label sanitize ---------------------------------------------------

    #[test]
    fn sanitize_label_keeps_allowed_charset() {
        assert_eq!(
            sanitize_label("pre-cni-swap").as_deref(),
            Some("pre-cni-swap")
        );
        assert_eq!(sanitize_label("v1.2_3").as_deref(), Some("v1.2_3"));
    }

    #[test]
    fn sanitize_label_replaces_disallowed_with_dash() {
        // `tr -c 'A-Za-z0-9._-' '-'`
        assert_eq!(
            sanitize_label("pre cni swap").as_deref(),
            Some("pre-cni-swap")
        );
        assert_eq!(sanitize_label("a/b:c").as_deref(), Some("a-b-c"));
    }

    #[test]
    fn sanitize_label_strips_leading_and_trailing_dashes() {
        // `sed 's/^-*//; s/-*$//'`
        assert_eq!(sanitize_label("  spaced  ").as_deref(), Some("spaced"));
        assert_eq!(sanitize_label("---x---").as_deref(), Some("x"));
    }

    /// Empty-after-sanitize is the exit-2 branch in `run_backup`.
    #[test]
    fn sanitize_label_returns_none_when_nothing_survives() {
        assert_eq!(sanitize_label(""), None);
        assert_eq!(sanitize_label("   "), None);
        assert_eq!(sanitize_label("///"), None);
        assert_eq!(sanitize_label("---"), None);
    }

    #[test]
    fn snapshot_file_name_matches_bash_twin_shape() {
        assert_eq!(
            snapshot_file_name("20260522T180000Z", None),
            "etcd-snapshot-20260522T180000Z.db"
        );
        assert_eq!(
            snapshot_file_name("20260522T180000Z", Some("pre-cni-swap")),
            "etcd-snapshot-20260522T180000Z-pre-cni-swap.db"
        );
    }

    // ---- remote-output parsing -------------------------------------------

    #[test]
    fn parse_etcd_container_id_takes_first_non_empty_line() {
        // `crictl ps --name etcd -q | head -1`
        assert_eq!(parse_etcd_container_id("abc123\ndef456\n"), Some("abc123"));
        assert_eq!(parse_etcd_container_id("\n\n  abc123  \n"), Some("abc123"));
    }

    #[test]
    fn parse_etcd_container_id_none_when_no_container() {
        assert_eq!(parse_etcd_container_id(""), None);
        assert_eq!(parse_etcd_container_id("\n  \n"), None);
    }

    #[test]
    fn validate_container_id_accepts_hex_ids() {
        validate_container_id("1a2b3c4d5e6f").expect("hex id is fine");
        validate_container_id("ABC123").expect("alnum id is fine");
    }

    #[test]
    fn validate_container_id_rejects_shell_syntax_and_empty() {
        for bad in ["", "abc; rm -rf /", "$(whoami)", "a b", "abc`id`"] {
            let err =
                validate_container_id(bad).expect_err(&format!("must reject container id {bad:?}"));
            assert!(
                err.to_string()
                    .contains("refusing to use etcd container id"),
                "wrong error for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn validate_image_ref_accepts_registry_refs_and_rejects_shell_syntax() {
        validate_image_ref("registry.k8s.io/etcd:3.5.15-0").expect("normal ref");
        validate_image_ref("registry.k8s.io/etcd@sha256:abc").expect("digest ref");
        for bad in ["", "etcd:latest; reboot", "$(id)", "a b"] {
            let err = validate_image_ref(bad).expect_err(&format!("must reject image ref {bad:?}"));
            assert!(
                err.to_string().contains("refusing to use etcd image"),
                "wrong error for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn pick_etcd_image_takes_first_matching_row() {
        // `crictl images` output shape: IMAGE TAG IMAGE-ID SIZE.
        let raw = "IMAGE                                TAG        IMAGE ID       SIZE\n\
                   registry.k8s.io/coredns/coredns      v1.11.1    abc123         59MB\n\
                   registry.k8s.io/etcd                 3.5.15-0   def456         149MB\n\
                   registry.k8s.io/etcd                 3.5.12-0   999999         148MB\n";
        assert_eq!(
            pick_etcd_image(raw).as_deref(),
            Some("registry.k8s.io/etcd:3.5.15-0")
        );
    }

    #[test]
    fn pick_etcd_image_none_when_no_etcd_row() {
        let raw = "IMAGE  TAG  IMAGE ID  SIZE\nregistry.k8s.io/pause  3.9  aaa  700kB\n";
        assert_eq!(pick_etcd_image(raw), None);
        assert_eq!(pick_etcd_image(""), None);
    }

    /// A truncated row (repository but no tag column) must not panic
    /// and must not yield a half-formed `repo:` reference.
    #[test]
    fn pick_etcd_image_none_when_tag_column_missing() {
        assert_eq!(pick_etcd_image("registry.k8s.io/etcd\n"), None);
    }

    #[test]
    fn human_size_matches_du_h_shape() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(512), "512");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1_048_576), "1.0M");
        assert_eq!(human_size(1_572_864), "1.5M");
        // >= 10 units drops the decimal, like `du -h`.
        assert_eq!(human_size(12 * 1_048_576), "12M");
        assert_eq!(human_size(3 * 1_073_741_824), "3.0G");
    }

    // ---- base64 -----------------------------------------------------------

    /// RFC 4648 section 10 test vectors.
    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// `head -c 32 /dev/urandom | base64 -w0` is always 44 chars with a
    /// single `=` pad — the shape the apiserver's aesgcm provider
    /// requires (32 bytes exactly).
    #[test]
    fn base64_encode_of_32_bytes_is_44_chars() {
        let out = base64_encode(&[0xABu8; 32]);
        assert_eq!(out.len(), 44, "unexpected length: {out}");
        assert!(out.ends_with('='), "expected one pad char: {out}");
        assert!(!out.ends_with("=="), "expected exactly one pad char: {out}");
        assert!(!out.contains('\n'), "base64 -w0 emits no newlines: {out}");
    }

    // ---- EncryptionConfiguration rewriting --------------------------------

    #[test]
    fn find_aesgcm_keys_block_locates_the_bootstrap_key() {
        let lines: Vec<&str> = BOOTSTRAP_CONFIG.lines().collect();
        let block = find_aesgcm_keys_block(&lines).expect("bootstrap config parses");
        assert_eq!(lines[block.keys_line].trim(), "keys:");
        assert_eq!(block.entry_indent, 12);
        assert_eq!(block.entries.len(), 1);
        assert_eq!(lines[block.entries[0]].trim(), "- name: bootstrap");
        // The block ends at the trailing `- identity: {}` provider.
        assert_eq!(lines[block.end].trim(), "- identity: {}");
    }

    /// bash twin's python3 raises `SystemExit("no aesgcm provider found
    /// in encryption config")`; operators grep for that string.
    #[test]
    fn find_aesgcm_keys_block_errors_without_aesgcm_provider() {
        let cfg = "resources:\n  - providers:\n      - identity: {}\n";
        let lines: Vec<&str> = cfg.lines().collect();
        let err = find_aesgcm_keys_block(&lines).expect_err("must reject");
        assert_eq!(
            err.to_string(),
            "no aesgcm provider found in encryption config"
        );
    }

    #[test]
    fn find_aesgcm_keys_block_errors_when_keys_list_missing() {
        let cfg = "providers:\n  - aesgcm: {}\n  - identity: {}\n";
        let lines: Vec<&str> = cfg.lines().collect();
        let err = find_aesgcm_keys_block(&lines).expect_err("must reject");
        assert!(
            err.to_string().contains("no `keys:` list"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn find_aesgcm_keys_block_errors_when_keys_list_is_empty() {
        let cfg = "providers:\n  - aesgcm:\n      keys:\n  - identity: {}\n";
        let lines: Vec<&str> = cfg.lines().collect();
        let err = find_aesgcm_keys_block(&lines).expect_err("must reject");
        assert!(err.to_string().contains("is empty"), "wrong error: {err}");
    }

    #[test]
    fn insert_primary_key_puts_the_new_key_first() {
        let out = insert_primary_key(BOOTSTRAP_CONFIG, "key-20260101000000", "TkVXS0VZ")
            .expect("rewrite succeeds");
        let names: Vec<&str> = out
            .lines()
            .filter(|l| l.trim().starts_with("- name:"))
            .collect();
        assert_eq!(names.len(), 2, "expected two keys, got: {out}");
        assert_eq!(names[0].trim(), "- name: key-20260101000000");
        assert_eq!(names[1].trim(), "- name: bootstrap");
        // Indentation must match the existing entries or the apiserver
        // rejects the file at startup.
        assert!(
            out.contains("\n            - name: key-20260101000000\n"),
            "bad indent: {out}"
        );
        assert!(
            out.contains("\n              secret: TkVXS0VZ\n"),
            "bad indent: {out}"
        );
        // The read-fallback provider must survive untouched.
        assert!(
            out.contains("      - identity: {}"),
            "identity dropped: {out}"
        );
        // And the old key must still be there so old rows decrypt.
        assert!(out.contains("secret: Ym9vdHN0cmFwLWtleS0zMi1ieXRlcy1iYXNlNjQtLQ=="));
    }

    #[test]
    fn insert_primary_key_propagates_missing_provider_error() {
        let err = insert_primary_key("providers:\n  - identity: {}\n", "k", "s")
            .expect_err("must reject");
        assert_eq!(
            err.to_string(),
            "no aesgcm provider found in encryption config"
        );
    }

    #[test]
    fn drop_non_primary_keys_keeps_only_the_first_entry() {
        let dual = insert_primary_key(BOOTSTRAP_CONFIG, "key-20260101000000", "TkVXS0VZ")
            .expect("stage 1 rewrite");
        let out = drop_non_primary_keys(&dual).expect("stage 3 rewrite");
        let names: Vec<&str> = out
            .lines()
            .filter(|l| l.trim().starts_with("- name:"))
            .collect();
        assert_eq!(names.len(), 1, "expected one key left, got: {out}");
        assert_eq!(names[0].trim(), "- name: key-20260101000000");
        assert!(!out.contains("bootstrap"), "old key survived: {out}");
        assert!(
            out.contains("      - identity: {}"),
            "identity dropped: {out}"
        );
    }

    /// Stage 3 on an already-single-key config is a no-op, mirroring
    /// the bash twin's `keys[:1]` slice.
    #[test]
    fn drop_non_primary_keys_is_a_noop_on_single_key_config() {
        let out = drop_non_primary_keys(BOOTSTRAP_CONFIG).expect("no-op rewrite");
        assert_eq!(out, BOOTSTRAP_CONFIG);
    }

    /// Full Stage 1 → Stage 3 round trip: the document must still be
    /// the bootstrap document with exactly one (new) key.
    #[test]
    fn rotate_rewrites_round_trip_to_a_valid_single_key_document() {
        let dual = insert_primary_key(BOOTSTRAP_CONFIG, "key-A", "S1").expect("stage 1");
        let fin = drop_non_primary_keys(&dual).expect("stage 3");
        assert_eq!(
            fin,
            BOOTSTRAP_CONFIG
                .replace("- name: bootstrap", "- name: key-A")
                .replace(
                    "secret: Ym9vdHN0cmFwLWtleS0zMi1ieXRlcy1iYXNlNjQtLQ==",
                    "secret: S1"
                ),
        );
        assert!(fin.ends_with('\n'), "trailing newline must survive");
    }

    #[test]
    fn joined_preserves_absence_of_trailing_newline() {
        let no_nl = "a:\n  - aesgcm:\n      keys:\n        - name: x\n          secret: y";
        let out = drop_non_primary_keys(no_nl).expect("single key no-op");
        assert!(!out.ends_with('\n'), "must not invent a trailing newline");
    }

    // ---- confirmation -----------------------------------------------------

    #[test]
    fn is_affirmative_matches_bash_y_only_regex() {
        // bash: [[ "$ans" =~ ^[Yy]$ ]]
        assert!(is_affirmative("y"));
        assert!(is_affirmative("Y"));
        assert!(is_affirmative("y\n"));
        assert!(!is_affirmative("yes"));
        assert!(!is_affirmative("n"));
        assert!(!is_affirmative(""));
        assert!(!is_affirmative("\n"));
    }

    /// `--yes` short-circuits before any TTY probe, which is what makes
    /// the destructive paths usable from `make` / CI.
    #[test]
    fn confirm_with_assume_yes_never_reads_stdin() {
        confirm("Proceed?", true).expect("--yes must short-circuit");
    }

    // ---- dry-run plans ----------------------------------------------------

    #[test]
    fn backup_dry_run_lines_are_deterministic_and_name_every_remote_step() {
        let lines = backup_dry_run_lines(&BackupPlanView {
            ssh_target: "-J geary root@10.0.0.5",
            dst: "./backups/etcd-snapshot-<UTC-timestamp>-pre-cni-swap.db",
            remote_snapshot: "/var/lib/etcd/hbird-snapshot-<UTC-timestamp>.db",
        });
        let text = lines.join("\n");
        assert!(text.starts_with("DRY-RUN etcd backup"), "{text}");
        for needle in [
            "crictl ps --name etcd -q",
            "snapshot save /var/lib/etcd/hbird-snapshot-<UTC-timestamp>.db",
            "--write-out=table snapshot status",
            "cat /var/lib/etcd/hbird-snapshot-<UTC-timestamp>.db > ./backups/",
            "rm -f /var/lib/etcd/hbird-snapshot-<UTC-timestamp>.db",
            "-J geary root@10.0.0.5",
        ] {
            assert!(
                text.contains(needle),
                "dry-run plan missing {needle:?}:\n{text}"
            );
        }
        // Every line must be prefixed so an operator can grep the plan
        // out of a mixed log — same contract as update-cluster's.
        assert!(lines.iter().all(|l| l.starts_with("DRY-RUN")), "{text}");
        // Second render must be byte-identical (no clock, no RNG).
        let again = backup_dry_run_lines(&BackupPlanView {
            ssh_target: "-J geary root@10.0.0.5",
            dst: "./backups/etcd-snapshot-<UTC-timestamp>-pre-cni-swap.db",
            remote_snapshot: "/var/lib/etcd/hbird-snapshot-<UTC-timestamp>.db",
        });
        assert_eq!(lines, again);
    }

    #[test]
    fn restore_dry_run_lines_flag_the_destructive_nature() {
        let lines = restore_dry_run_lines("root@10.0.0.5", "/tmp/snap.db");
        let text = lines.join("\n");
        assert!(text.contains("DESTRUCTIVE"), "{text}");
        assert!(
            text.contains("test ! -e /etc/kubernetes/manifests.disabled"),
            "{text}"
        );
        assert!(text.contains("/tmp/restore-snapshot.db"), "{text}");
        assert!(text.contains("registry.k8s.io/etcd:3.5.15-0"), "{text}");
        assert!(lines.iter().all(|l| l.starts_with("DRY-RUN")), "{text}");
    }

    #[test]
    fn rotate_dry_run_lines_cover_all_four_stages() {
        let lines = rotate_dry_run_lines("root@10.0.0.5");
        let text = lines.join("\n");
        for needle in ["Stage 0", "Stage 1", "Stage 2", "Stage 3", "Stage 4"] {
            assert!(
                text.contains(needle),
                "dry-run plan missing {needle}:\n{text}"
            );
        }
        assert!(text.contains("DESTRUCTIVE"), "{text}");
        assert!(lines.iter().all(|l| l.starts_with("DRY-RUN")), "{text}");
    }

    // ---- restore banner + script -----------------------------------------

    #[test]
    fn restore_banner_preserves_bash_twin_wording() {
        let b = restore_banner("10.0.0.5", "/tmp/snap.db");
        assert!(
            b.starts_with("About to restore etcd on 10.0.0.5 from /tmp/snap.db.\n"),
            "{b}"
        );
        for needle in [
            "  1. Move /etc/kubernetes/manifests aside",
            "  2. Stop kubelet.",
            "  3. Rename /var/lib/etcd to /var/lib/etcd.before-restore.<ts>.",
            "  4. Run 'etcdctl snapshot restore' into a fresh /var/lib/etcd.",
            "  5. Restore the manifests directory and start kubelet",
        ] {
            assert!(b.contains(needle), "banner missing {needle:?}:\n{b}");
        }
    }

    #[test]
    fn restore_script_matches_bash_twin_steps_in_order() {
        let s = restore_script(&RestoreApplyParams {
            timestamp: "20260101T000000Z",
            etcd_image: "registry.k8s.io/etcd:3.5.15-0",
        });
        let steps = [
            "set -euo pipefail",
            "mv /etc/kubernetes/manifests /etc/kubernetes/manifests.disabled",
            "sleep 10",
            "systemctl stop kubelet || true",
            "mv /var/lib/etcd \"/var/lib/etcd.before-restore.20260101T000000Z\"",
            "podman run --rm --network host",
            "\"registry.k8s.io/etcd:3.5.15-0\"",
            "etcdctl snapshot restore /tmp/restore-snapshot.db",
            "--data-dir=/var/lib/etcd",
            "mv /etc/kubernetes/manifests.disabled /etc/kubernetes/manifests",
            "systemctl start kubelet",
            "rm -f /tmp/restore-snapshot.db",
            "Restore complete. Apiserver will come back up in ~30s.",
        ];
        let mut cursor = 0usize;
        for step in steps {
            let at = s[cursor..].find(step).unwrap_or_else(|| {
                panic!("restore script missing (or out of order) {step:?}:\n{s}")
            });
            cursor += at + step.len();
        }
    }

    // ---- backup_with_exec -------------------------------------------------

    #[test]
    fn backup_with_exec_happy_path_runs_the_five_remote_steps() {
        let exec = MockSshExec::new(vec![
            ok_stdout("deadbeef01\n"),
            ok_stdout(""),
            ok_stdout("+------+\n| HASH |\n"),
            ok_bytes(&[0x00, 0x01, 0x02, 0xff]),
            ok_stdout(""),
        ]);
        let bytes = backup_with_exec(&exec, "/var/lib/etcd/snap.db").expect("happy path");
        assert_eq!(bytes, vec![0x00, 0x01, 0x02, 0xff]);
        let cmds = exec.commands();
        assert_eq!(cmds.len(), 5, "expected five ssh calls: {cmds:?}");
        assert_eq!(cmds[0], "crictl ps --name etcd -q");
        assert!(
            cmds[1].starts_with(
                "crictl exec deadbeef01 etcdctl --cacert=/etc/kubernetes/pki/etcd/ca.crt"
            ),
            "snapshot save must reuse the bash twin's TLS flags: {}",
            cmds[1]
        );
        assert!(
            cmds[1].ends_with("snapshot save /var/lib/etcd/snap.db"),
            "{}",
            cmds[1]
        );
        assert_eq!(
            cmds[2],
            "crictl exec deadbeef01 etcdctl --write-out=table snapshot status /var/lib/etcd/snap.db"
        );
        assert_eq!(cmds[3], "cat /var/lib/etcd/snap.db");
        assert_eq!(cmds[4], "rm -f /var/lib/etcd/snap.db");
    }

    /// bash twin: `[[ -n "$ETCD" ]] || { echo 'no etcd container found'
    /// >&2; exit 1; }`. Operators grep for that exact string.
    #[test]
    fn backup_with_exec_errors_when_no_etcd_container() {
        let exec = MockSshExec::new(vec![ok_stdout("\n")]);
        let err = backup_with_exec(&exec, "/var/lib/etcd/snap.db").expect_err("must fail");
        assert_eq!(err.to_string(), "no etcd container found");
        assert_eq!(exec.commands().len(), 1, "must stop after `crictl ps`");
    }

    #[test]
    fn backup_with_exec_surfaces_crictl_ps_failure() {
        let exec = MockSshExec::new(vec![nonzero_exit(127, "crictl: not found")]);
        let err = backup_with_exec(&exec, "/var/lib/etcd/snap.db").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("crictl ps --name etcd -q"),
            "error should name the failing step: {err:#}"
        );
    }

    #[test]
    fn backup_with_exec_rejects_a_bogus_container_id_before_exec() {
        let exec = MockSshExec::new(vec![ok_stdout("abc; rm -rf /\n")]);
        let err = backup_with_exec(&exec, "/var/lib/etcd/snap.db").expect_err("must fail");
        assert!(
            err.to_string()
                .contains("refusing to use etcd container id"),
            "wrong error: {err}"
        );
        assert_eq!(
            exec.commands().len(),
            1,
            "no further command may run after the id is rejected"
        );
    }

    #[test]
    fn backup_with_exec_surfaces_snapshot_save_failure() {
        let exec = MockSshExec::new(vec![
            ok_stdout("deadbeef01\n"),
            nonzero_exit(1, "etcdctl: context deadline exceeded"),
        ]);
        let err = backup_with_exec(&exec, "/var/lib/etcd/snap.db").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("snapshot save"),
            "error should name the failing step: {err:#}"
        );
    }

    #[test]
    fn backup_with_exec_surfaces_snapshot_status_failure() {
        let exec = MockSshExec::new(vec![
            ok_stdout("deadbeef01\n"),
            ok_stdout(""),
            nonzero_exit(1, "etcdctl: bad snapshot"),
        ]);
        let err = backup_with_exec(&exec, "/var/lib/etcd/snap.db").expect_err("must fail");
        assert!(
            err.to_string().contains("snapshot status"),
            "wrong error: {err}"
        );
    }

    /// A zero-byte fetch means etcdctl claimed success but nothing
    /// landed on the host — writing that to `./backups/` would give the
    /// operator a snapshot that cannot restore.
    #[test]
    fn backup_with_exec_rejects_an_empty_snapshot() {
        let exec = MockSshExec::new(vec![
            ok_stdout("deadbeef01\n"),
            ok_stdout(""),
            ok_stdout("table"),
            ok_bytes(b""),
        ]);
        let err = backup_with_exec(&exec, "/var/lib/etcd/snap.db").expect_err("must fail");
        assert!(
            err.to_string().contains("came back empty"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn backup_with_exec_surfaces_cleanup_failure() {
        let exec = MockSshExec::new(vec![
            ok_stdout("deadbeef01\n"),
            ok_stdout(""),
            ok_stdout("table"),
            ok_bytes(b"snapshot-bytes"),
            nonzero_exit(1, "rm: read-only file system"),
        ]);
        let err = backup_with_exec(&exec, "/var/lib/etcd/snap.db").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("could not remove"),
            "wrong error: {err:#}"
        );
    }

    // ---- restore helpers --------------------------------------------------

    #[test]
    fn restore_preflight_passes_when_manifests_disabled_is_absent() {
        let exec = MockSshExec::new(vec![ok_stdout("")]);
        restore_preflight_with_exec(&exec).expect("absent path is the happy case");
        assert_eq!(
            exec.commands(),
            vec!["test ! -e /etc/kubernetes/manifests.disabled"]
        );
    }

    /// The bug the bash twin has: a second run would `mv` the live
    /// manifests directory INSIDE the stale `manifests.disabled`.
    #[test]
    fn restore_preflight_refuses_when_manifests_disabled_exists() {
        let exec = MockSshExec::new(vec![nonzero_exit(1, "")]);
        let err = restore_preflight_with_exec(&exec).expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("already exists on the CP"),
            "wrong error: {msg}"
        );
        assert!(
            msg.contains("mv /etc/kubernetes/manifests.disabled"),
            "must tell the operator how to recover: {msg}"
        );
    }

    #[test]
    fn restore_preflight_distinguishes_transport_failure() {
        let exec = MockSshExec::new(vec![transport_err()]);
        let err = restore_preflight_with_exec(&exec).expect_err("must fail");
        assert!(
            format!("{err:#}").contains("could not check"),
            "transport failure must not be reported as `already exists`: {err:#}"
        );
    }

    #[test]
    fn push_snapshot_pipes_the_bytes_to_a_remote_cat() {
        let exec = MockSshExec::new(vec![ok_stdout("")]);
        push_snapshot_with_exec(&exec, b"snapshot-bytes").expect("upload ok");
        assert_eq!(exec.commands(), vec!["cat > /tmp/restore-snapshot.db"]);
        assert_eq!(exec.stdins(), vec![b"snapshot-bytes".to_vec()]);
    }

    #[test]
    fn push_snapshot_surfaces_remote_failure() {
        let exec = MockSshExec::new(vec![nonzero_exit(1, "No space left on device")]);
        let err = push_snapshot_with_exec(&exec, b"x").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("could not upload the snapshot"),
            "wrong error: {err:#}"
        );
    }

    #[test]
    fn detect_etcd_image_uses_the_image_already_on_the_cp() {
        let exec = MockSshExec::new(vec![ok_stdout(
            "registry.k8s.io/etcd  3.5.15-0  abc  149MB\n",
        )]);
        let img = detect_etcd_image_with_exec(&exec).expect("detect ok");
        assert_eq!(img, "registry.k8s.io/etcd:3.5.15-0");
        assert_eq!(exec.commands(), vec!["crictl images"]);
    }

    #[test]
    fn detect_etcd_image_falls_back_when_no_row_matches() {
        let exec = MockSshExec::new(vec![ok_stdout("registry.k8s.io/pause 3.9 aaa 700kB\n")]);
        let img = detect_etcd_image_with_exec(&exec).expect("fallback ok");
        assert_eq!(img, ETCD_IMAGE_FALLBACK);
    }

    /// The bash twin swallows a failing `crictl images` (`2>/dev/null`
    /// inside a pipeline whose exit status is awk's), so a non-zero
    /// exit must NOT abort the restore — it falls back like an empty
    /// listing does.
    #[test]
    fn detect_etcd_image_tolerates_nonzero_crictl_exit() {
        let exec = MockSshExec::new(vec![nonzero_exit_with_stdout(1, "")]);
        let img = detect_etcd_image_with_exec(&exec).expect("non-zero must not abort");
        assert_eq!(img, ETCD_IMAGE_FALLBACK);

        // …and if it still printed a usable row, that row wins.
        let exec = MockSshExec::new(vec![nonzero_exit_with_stdout(
            1,
            "registry.k8s.io/etcd  3.6.0-1  abc  150MB\n",
        )]);
        assert_eq!(
            detect_etcd_image_with_exec(&exec).expect("non-zero must not abort"),
            "registry.k8s.io/etcd:3.6.0-1"
        );
    }

    #[test]
    fn detect_etcd_image_propagates_transport_failure() {
        let exec = MockSshExec::new(vec![transport_err()]);
        let err = detect_etcd_image_with_exec(&exec).expect_err("transport failure must abort");
        assert!(format!("{err:#}").contains("crictl images"), "{err:#}");
    }

    #[test]
    fn restore_apply_sends_the_script_over_stdin_to_bash_s() {
        let exec = MockSshExec::new(vec![ok_stdout(
            "Restore complete. Apiserver will come back up in ~30s.\n",
        )]);
        let out = restore_apply_with_exec(
            &exec,
            &RestoreApplyParams {
                timestamp: "20260101T000000Z",
                etcd_image: "registry.k8s.io/etcd:3.5.15-0",
            },
        )
        .expect("apply ok");
        assert!(out.contains("Restore complete."), "{out}");
        assert_eq!(exec.commands(), vec!["bash -s"]);
        let script = String::from_utf8(exec.stdins()[0].clone()).expect("utf8 script");
        assert!(script.starts_with("set -euo pipefail\n"), "{script}");
        assert!(
            script.contains("/var/lib/etcd.before-restore.20260101T000000Z"),
            "{script}"
        );
    }

    /// Divergence 4: the bash twin leaves the CP down when the restore
    /// dies mid-flight. We roll back and say so.
    #[test]
    fn restore_apply_rolls_back_when_the_remote_script_fails() {
        let exec = MockSshExec::new(vec![
            nonzero_exit(125, "podman: image not known"),
            ok_stdout(""),
        ]);
        let err = restore_apply_with_exec(
            &exec,
            &RestoreApplyParams {
                timestamp: "20260101T000000Z",
                etcd_image: "registry.k8s.io/etcd:3.5.15-0",
            },
        )
        .expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("rollback attempted"), "{msg}");
        assert!(
            msg.contains("/var/lib/etcd.before-restore.20260101T000000Z"),
            "operator must be told where the old data dir is: {msg}"
        );
        assert_eq!(exec.commands(), vec!["bash -s", "bash -s"]);
        let rollback = String::from_utf8(exec.stdins()[1].clone()).expect("utf8");
        assert!(
            rollback.contains("mv /etc/kubernetes/manifests.disabled"),
            "{rollback}"
        );
        assert!(rollback.contains("systemctl start kubelet"), "{rollback}");
    }

    #[test]
    fn restore_apply_reports_a_failed_rollback_distinctly() {
        let exec = MockSshExec::new(vec![nonzero_exit(1, "boom"), transport_err()]);
        let err = restore_apply_with_exec(
            &exec,
            &RestoreApplyParams {
                timestamp: "20260101T000000Z",
                etcd_image: "registry.k8s.io/etcd:3.5.15-0",
            },
        )
        .expect_err("must fail");
        assert!(format!("{err:#}").contains("rollback FAILED"), "{err:#}");
    }

    #[test]
    fn restore_apply_rejects_a_hostile_image_ref_before_touching_the_cp() {
        let exec = MockSshExec::new(vec![ok_stdout("")]);
        let err = restore_apply_with_exec(
            &exec,
            &RestoreApplyParams {
                timestamp: "20260101T000000Z",
                etcd_image: "etcd:latest; reboot",
            },
        )
        .expect_err("must reject");
        assert!(
            err.to_string().contains("refusing to use etcd image"),
            "{err}"
        );
        assert!(
            exec.commands().is_empty(),
            "nothing may run: {:?}",
            exec.commands()
        );
    }

    // ---- rotate helpers ---------------------------------------------------

    #[test]
    fn rotate_fetch_config_returns_the_remote_document() {
        let exec = MockSshExec::new(vec![ok_stdout(BOOTSTRAP_CONFIG)]);
        let cfg = rotate_fetch_config_with_exec(&exec).expect("fetch ok");
        assert_eq!(cfg, BOOTSTRAP_CONFIG);
        assert_eq!(
            exec.commands(),
            vec!["cat /etc/kubernetes/encryption-config.yaml"]
        );
    }

    /// bash twin wording: `[[ -s "$BEFORE" ]] || { log "fetched
    /// encryption-config is empty"; exit 1; }`.
    #[test]
    fn rotate_fetch_config_errors_on_empty_document() {
        let exec = MockSshExec::new(vec![ok_stdout("   \n")]);
        let err = rotate_fetch_config_with_exec(&exec).expect_err("must fail");
        assert_eq!(err.to_string(), "fetched encryption-config is empty");
    }

    #[test]
    fn rotate_fetch_config_surfaces_ssh_failure() {
        let exec = MockSshExec::new(vec![nonzero_exit(1, "cat: No such file or directory")]);
        let err = rotate_fetch_config_with_exec(&exec).expect_err("must fail");
        assert!(
            format!("{err:#}").contains("could not read /etc/kubernetes/encryption-config.yaml"),
            "{err:#}"
        );
    }

    #[test]
    fn rotate_install_config_uploads_then_installs_0600_and_touches_the_manifest() {
        let exec = MockSshExec::new(vec![ok_stdout(""), ok_stdout("")]);
        rotate_install_config_with_exec(&exec, "new-config\n").expect("install ok");
        let cmds = exec.commands();
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        // Key material must never sit in a world-readable staging file.
        assert_eq!(
            cmds[0],
            "umask 077; cat > /etc/kubernetes/encryption-config.yaml.new"
        );
        assert_eq!(exec.stdins()[0], b"new-config\n".to_vec());
        assert_eq!(cmds[1], "bash -s");
        let script = String::from_utf8(exec.stdins()[1].clone()).expect("utf8");
        assert!(
            script.contains("install -m 0600 -o root -g root"),
            "{script}"
        );
        assert!(
            script.contains("rm -f /etc/kubernetes/encryption-config.yaml.new"),
            "{script}"
        );
        assert!(
            script.contains("touch /etc/kubernetes/manifests/kube-apiserver.yaml"),
            "the apiserver only re-reads the config when the static-pod \
             manifest is touched: {script}"
        );
    }

    #[test]
    fn rotate_install_config_surfaces_upload_failure_before_installing() {
        let exec = MockSshExec::new(vec![nonzero_exit(1, "No space left on device")]);
        let err = rotate_install_config_with_exec(&exec, "cfg").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("could not upload the new config"),
            "{err:#}"
        );
        assert_eq!(
            exec.commands().len(),
            1,
            "install must not run after a failed upload"
        );
    }

    #[test]
    fn rotate_healthz_passes_on_rc_zero() {
        let exec = MockSshExec::new(vec![ok_stdout("ok")]);
        rotate_healthz_with_exec(&exec, 1).expect("healthz ok");
        assert_eq!(
            exec.commands(),
            vec!["KUBECONFIG=/etc/kubernetes/admin.conf kubectl get --raw=/healthz"]
        );
    }

    /// bash twin wording: `apiserver healthz failed after Stage N
    /// reload`. Both stages must be able to say which one they are.
    #[test]
    fn rotate_healthz_uses_bash_twin_wording_per_stage() {
        for (stage, code) in [(1u8, 1i32), (3u8, 7i32)] {
            let exec = MockSshExec::new(vec![nonzero_exit(code, "connection refused")]);
            let err = rotate_healthz_with_exec(&exec, stage).expect_err("must fail");
            assert!(
                err.to_string().starts_with(&format!(
                    "apiserver healthz failed after Stage {stage} reload"
                )),
                "wrong wording for stage {stage}: {err}"
            );
        }
    }

    /// A transport failure is also a failed health gate — the bash `||`
    /// cannot tell the two apart, and neither should we (a CP we cannot
    /// reach is not a CP we may drop the old key from).
    #[test]
    fn rotate_healthz_treats_transport_failure_as_failure() {
        let exec = MockSshExec::new(vec![transport_err()]);
        let err = rotate_healthz_with_exec(&exec, 1).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("apiserver healthz failed after Stage 1 reload"),
            "{err}"
        );
    }

    #[test]
    fn rotate_reencrypt_replaces_secrets_then_configmaps() {
        let exec = MockSshExec::new(vec![ok_stdout("")]);
        rotate_reencrypt_with_exec(&exec).expect("re-encrypt ok");
        assert_eq!(exec.commands(), vec!["bash -s"]);
        let script = String::from_utf8(exec.stdins()[0].clone()).expect("utf8");
        let secrets = script.find("kubectl get secrets -A -o json | kubectl replace -f -");
        let cms = script.find("kubectl get configmaps -A -o json | kubectl replace -f -");
        assert!(secrets.is_some() && cms.is_some(), "{script}");
        assert!(secrets < cms, "secrets must be rewritten first: {script}");
        assert!(
            !script.contains("--force"),
            "`replace --force` would change UIDs: {script}"
        );
    }

    #[test]
    fn rotate_reencrypt_failure_warns_against_running_stage_3() {
        let exec = MockSshExec::new(vec![nonzero_exit(1, "Operation cannot be fulfilled")]);
        let err = rotate_reencrypt_with_exec(&exec).expect_err("must fail");
        assert!(
            format!("{err:#}").contains("do not run Stage 3"),
            "the ordering hazard must be spelled out: {err:#}"
        );
    }

    #[test]
    fn rotate_verify_passes_the_new_key_name_as_expected_prefix() {
        let exec = MockSshExec::new(vec![ok_stdout("PASS")]);
        rotate_verify_with_exec(&exec, "key-20260101000000").expect("verify ok");
        assert_eq!(
            exec.commands(),
            vec![
                "EXPECTED_PREFIX='k8s:enc:aesgcm:v1:key-20260101000000:' \
                 /usr/libexec/verify-encryption.sh"
            ]
        );
    }

    #[test]
    fn rotate_verify_surfaces_a_failing_on_image_verifier() {
        let exec = MockSshExec::new(vec![nonzero_exit(1, "[verify-encryption] FAIL: plaintext")]);
        let err = rotate_verify_with_exec(&exec, "key-A").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("Stage 4 verification failed"),
            "{err:#}"
        );
    }

    // ---- plan / target resolution ----------------------------------------

    /// Canned [`hbird_virt::SshClient`] so the local-virsh branch runs
    /// without a libvirt daemon (same fixture shape as
    /// `update_cluster::tests::CannedVirt`).
    struct CannedVirt(String);
    impl hbird_virt::SshClient for CannedVirt {
        fn run(&self, _host: &str, _command: &str) -> Result<String, hbird_virt::SshError> {
            Ok(self.0.clone())
        }
    }

    fn local_conn(stdout: &str) -> hbird_virt::Connection {
        hbird_virt::Connection::new_local_with_client(std::sync::Arc::new(CannedVirt(
            stdout.to_string(),
        )))
    }

    #[test]
    fn resolve_cp_ip_local_parses_the_virsh_lease() {
        let table = " Name       MAC address          Protocol     Address\n\
                     -------------------------------------------------------\n\
                     vnet0      52:54:00:ab:cd:ef    ipv4         192.168.122.47/24\n";
        let ip = resolve_cp_ip_local(&local_conn(table), "hummingbird-k8s").expect("resolves");
        assert_eq!(ip, "192.168.122.47");
    }

    #[test]
    fn resolve_cp_ip_local_errors_without_a_lease() {
        let err = resolve_cp_ip_local(&local_conn(" Name  MAC  Protocol  Address\n"), "cp1")
            .expect_err("no lease must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("cp1"), "must name the domain: {msg}");
        assert!(msg.contains("CP_IP"), "must offer the pin hatch: {msg}");
        assert!(
            msg.contains("KVM_HOST"),
            "must offer the remote hatch: {msg}"
        );
    }

    fn common(cp_ip: Option<&str>, kvm_host: Option<&str>, dry_run: bool) -> EtcdCommonArgs {
        EtcdCommonArgs {
            config: None,
            cp_name: Some("hummingbird-k8s".to_string()),
            cp_ip: cp_ip.map(str::to_string),
            kvm_host: kvm_host.map(str::to_string),
            dry_run,
        }
    }

    #[test]
    fn plan_from_common_prefers_the_pinned_cp_ip() {
        let plan = plan_from_common(&common(Some("10.0.0.7"), Some("geary"), false))
            .expect("pinned ip needs no resolution");
        assert_eq!(plan.target.cp_ip, "10.0.0.7");
        assert_eq!(plan.target.kvm_host.as_deref(), Some("geary"));
        assert_eq!(plan.ssh_target(), "-J geary root@10.0.0.7");
    }

    /// Running ON the KVM host must work: no `KVM_HOST`, no ProxyJump,
    /// no `ssh root@<myself>` loop.
    #[test]
    fn plan_from_common_without_kvm_host_has_no_proxy_jump() {
        let plan = plan_from_common(&common(Some("10.0.0.7"), None, false)).expect("resolves");
        assert!(plan.target.kvm_host.is_none());
        assert_eq!(plan.ssh_target(), "root@10.0.0.7");
        let argv = plan.target.cp_ssh_opts().to_argv();
        assert!(
            !argv.iter().any(|a| a.contains("ProxyJump")),
            "no ProxyJump expected: {argv:?}"
        );
    }

    /// `--dry-run` must never open a connection, so an unresolvable CP
    /// still renders a plan (with the placeholder IP).
    #[test]
    fn plan_from_common_dry_run_uses_a_placeholder_instead_of_resolving() {
        let plan = plan_from_common(&common(None, None, true)).expect("dry-run must not resolve");
        assert_eq!(plan.target.cp_ip, DRY_RUN_CP_IP);
        assert!(plan.dry_run);
    }

    /// Live mode with neither a pinned IP nor a CP name has nothing to
    /// resolve from; the error must say which knobs exist.
    #[test]
    fn plan_from_common_errors_without_cp_name_or_cp_ip() {
        let args = EtcdCommonArgs {
            config: None,
            cp_name: None,
            cp_ip: None,
            kvm_host: None,
            dry_run: false,
        };
        let err = plan_from_common(&args).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("CP_NAME required"), "{msg}");
        assert!(msg.contains("CP_IP"), "{msg}");
    }
}
