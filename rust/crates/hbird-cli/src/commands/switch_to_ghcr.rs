//! `hbird switch-to-ghcr` — bash twin: `scripts/switch-to-ghcr.sh`.
//!
//! Switches deployed Hummingbird VMs from their locally-built image ref
//! (`localhost/hummingbird-<flavor>:latest`) to the GHCR-published
//! equivalent (`ghcr.io/aatchison/hummingbird-<flavor>:latest`), so the
//! `bootc-fetch-apply-updates` timer (and any manual `bootc upgrade`)
//! has a remote to pull from. See issue [#138].
//!
//! # Two modes (identical to the bash twin)
//!
//! * **all-VMs** — no positional args. Iterates every *running*
//!   `hummingbird-*` domain under `qemu:///system` and switches each.
//!   A VM already tracking the target ref is skipped. Any per-VM failure
//!   sets the process exit code to 1 but does NOT stop the walk (the
//!   [#138] design).
//! * **single-VM** — `hbird switch-to-ghcr <vm-name> [<ghcr-ref>]`. No
//!   discovery. Errors are non-fatal (exit 0): the caller's deploy
//!   already succeeded and the switch is a follow-on.
//!
//! # Why this is a Rust subcommand now
//!
//! The bash twin's first act is `source scripts/lib/ssh-wrap.sh` +
//! `hbird_ssh_wrap_maybe_reexec` — a 752-line shim that decides
//! local-vs-remote, re-execs on `$KVM_HOST`, and forwards an env
//! allowlist. [`crate::virt_bridge::build_connection`] already makes the
//! local-vs-remote decision natively (local `virsh` when `--kvm-host` is
//! absent, SSH-tunnelled when set), and clap `env = "…"` attributes turn
//! the shim's `HBIRD_SSH_WRAP_ALLOWED_ENV` list into a declarative,
//! `--help`-visible surface. The shim is deliberately NOT ported.
//!
//! Reaching the VM itself uses `ProxyJump=<kvm-host>` rather than the
//! twin's "re-exec on the KVM host, then ssh from there" — the same
//! shape `commands/update_cluster.rs::node_ssh_opts` already uses, so a
//! workstation operator needs only their existing SSH key.
//!
//! # Block traceability
//!
//! Each `// ---- <name> ----` header matches a section of
//! `scripts/switch-to-ghcr.sh`:
//!
//! 1. `BOOTC_SWITCH_TO_GHCR` escape hatch → [`run`]
//! 2. `flavor_for_vm`                     → [`flavor_for_vm`]
//! 3. `ip_for_vm` / `wait_for_ip`         → [`wait_for_ip_with`]
//! 4. `wait_for_ssh`                      → [`wait_for_ssh_with`]
//! 5. `current_image_ref`                 → [`current_image_ref_with_exec`] + [`parse_booted_image`]
//! 6. `switch_one`                        → [`switch_one_with_exec`]
//! 7. single-VM mode + `FORCE_REBUILD`    → [`run_single_vm`]
//! 8. all-VMs mode                        → [`run_all_vms`]
//!
//! # Deliberate divergences from the bash twin
//!
//! * **No `python3` on the KVM host.** The twin pipes `bootc status
//!   --json` into an inline `python3 -c` snippet to pluck
//!   `status.booted.image.image.image`. That silently makes `python3` a
//!   runtime dependency of the KVM host and swallows *every* error
//!   (`except Exception: pass`). [`minijson`] does the same extraction
//!   in-process with no new crate dependency.
//! * **Log lines go to stdout, not stderr.** The twin's `log()` writes
//!   to stderr; every Rust `hbird` subcommand writes operator progress
//!   to stdout and leaves stderr to the `tracing` subscriber. The
//!   *wording* of every shared line is preserved verbatim (operators
//!   grep for it); only the stream changed.
//!
//! [#138]: https://github.com/aatchison/hummingbird-k8s/issues/138

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use clap::builder::BoolishValueParser;

use hbird_ssh::SshExec;
use hbird_virt::Connection;

use crate::cp_resolve::shell_single_quote;

// ---- Arguments -------------------------------------------------------------

/// Arguments for `hbird switch-to-ghcr`.
///
/// Every environment variable the bash twin honors
/// (`BOOTC_SWITCH_TO_GHCR`, `GHCR_ORG`, `GHCR_TAG`, `FORCE_REBUILD`,
/// `FORCE_SWITCH`, `KVM_HOST`, `CONFIG`) is exposed as a flag with a
/// matching `env = "…"`, so existing `GHCR_TAG=v1 make switch-to-ghcr`
/// invocations keep working and the knob is discoverable via `--help`.
#[derive(Debug, Args)]
pub struct SwitchToGhcrArgs {
    /// VM (libvirt domain) name — single-VM mode. Omit to switch every
    /// running `hummingbird-*` VM.
    #[arg(value_name = "VM")]
    pub vm: Option<String>,

    /// Exact GHCR ref to switch to — single-VM mode only. Inferred from
    /// the VM's flavor when omitted.
    #[arg(value_name = "GHCR_REF")]
    pub ghcr_ref: Option<String>,

    /// Path to `cluster.local.conf`. Optional — only `GHCR_TAG` and
    /// `KVM_HOST` are read from it. The bash twin sources no config.
    #[arg(long, value_name = "PATH", env = "CONFIG")]
    pub config: Option<PathBuf>,

    /// SSH alias of the KVM host. Absent/empty → `virsh` runs locally
    /// (operator is already on the KVM host).
    #[arg(long, value_name = "HOST", env = "KVM_HOST")]
    pub kvm_host: Option<String>,

    /// GHCR org prefix. Bash twin: `GHCR_ORG="${GHCR_ORG:-ghcr.io/aatchison}"`.
    #[arg(long, value_name = "ORG", env = "GHCR_ORG")]
    pub ghcr_org: Option<String>,

    /// GHCR tag. Bash twin: `GHCR_TAG="${GHCR_TAG:-latest}"`.
    #[arg(long, value_name = "TAG", env = "GHCR_TAG")]
    pub ghcr_tag: Option<String>,

    /// Master escape hatch. `BOOTC_SWITCH_TO_GHCR=0` keeps the VMs
    /// tracking `localhost/…` on purpose (offline lab).
    #[arg(
        long,
        env = "BOOTC_SWITCH_TO_GHCR",
        num_args = 0..=1,
        default_value = "true",
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub bootc_switch_to_ghcr: bool,

    /// Operator rebuilt the image locally (#375). In single-VM mode this
    /// skips the switch so the VM keeps tracking the freshly-built
    /// install-time image that is being boot-tested. Env: `FORCE_REBUILD`.
    #[arg(
        long,
        env = "FORCE_REBUILD",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub force_rebuild: bool,

    /// Opt back in to the switch despite `--force-rebuild` (#375).
    /// Env: `FORCE_SWITCH`.
    #[arg(
        long,
        env = "FORCE_SWITCH",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new(),
    )]
    pub force_switch: bool,

    /// Plan-only mode — print the deterministic switch plan and change
    /// nothing. Read-only discovery (`virsh list --name`) still runs in
    /// all-VMs mode because the plan is derived from live host state.
    /// The bash twin has no dry-run.
    #[arg(long)]
    pub dry_run: bool,
}

// ---- Logger ----------------------------------------------------------------

/// Prefix preserved verbatim from the bash twin's
/// `printf '[switch-to-ghcr] %s\n'`; only the stream changed (see the
/// module docs).
fn log(line: &str) {
    println!("[switch-to-ghcr] {line}");
}

// ---- Constants -------------------------------------------------------------

/// Bash twin: `GHCR_ORG="${GHCR_ORG:-ghcr.io/aatchison}"`.
const DEFAULT_GHCR_ORG: &str = "ghcr.io/aatchison";

/// Bash twin: `GHCR_TAG="${GHCR_TAG:-latest}"`.
const DEFAULT_GHCR_TAG: &str = "latest";

/// Domain-name prefix `virsh list --name | grep '^hummingbird-'` selects.
const HUMMINGBIRD_PREFIX: &str = "hummingbird-";

/// Poll count for both wait loops. Bash twin: `wait_for_ip 30 …` /
/// `wait_for_ssh 30 …`.
const WAIT_TRIES: u32 = 30;

/// Seconds between polls. Bash twin: `sleep 2` inside both loops.
/// `WAIT_TRIES * WAIT_INTERVAL_SECS` is the "after 60s" in the SKIP
/// messages, which are formatted from these constants so the wording
/// cannot drift away from the behaviour.
const WAIT_INTERVAL_SECS: u32 = 2;

/// `ConnectTimeout` for the probe / status SSH calls. Bash twin:
/// `-o ConnectTimeout=5`.
const PROBE_CONNECT_TIMEOUT_SECS: u64 = 5;

/// `ConnectTimeout` for the `bootc switch` call. Bash twin:
/// `-o ConnectTimeout=10`.
const SWITCH_CONNECT_TIMEOUT_SECS: u64 = 10;

// ---- Block #2: flavor_for_vm -----------------------------------------------

/// Map a VM (domain) name to its image flavor.
///
/// Bash twin (`scripts/switch-to-ghcr.sh::99-106`):
///
/// ```sh
/// case "$name" in
///   hummingbird-k8s)          echo "hummingbird-k8s" ;;
///   hummingbird-k8s-worker-*) echo "hummingbird-k8s-worker" ;;
///   *)                        return 1 ;;
/// esac
/// ```
///
/// Case-arm order matters and is preserved: `hummingbird-k8s` is an
/// EXACT match, so `hummingbird-k8s-worker-1` falls through to the
/// worker arm. `None` maps to the twin's `return 1` ("unknown flavor").
pub(crate) fn flavor_for_vm(name: &str) -> Option<&'static str> {
    if name == "hummingbird-k8s" {
        Some("hummingbird-k8s")
    } else if name.starts_with("hummingbird-k8s-worker-") {
        Some("hummingbird-k8s-worker")
    } else {
        None
    }
}

/// Build the GHCR ref for a flavor. Bash twin:
/// `ref="${GHCR_ORG}/${flavor}:${GHCR_TAG}"`.
pub(crate) fn ghcr_ref_for(org: &str, flavor: &str, tag: &str) -> String {
    format!("{}/{flavor}:{tag}", org.trim_end_matches('/'))
}

// ---- Block #5: bootc status --json parsing ---------------------------------

/// Minimal, dependency-free JSON reader.
///
/// Replaces the bash twin's inline `python3 -c 'json.load(sys.stdin)'`
/// snippet. Only what `bootc status --json` needs: full structural
/// parsing (so unrelated sub-objects are *skipped* correctly) plus
/// string decoding for the one value we read. Numbers are kept as their
/// source text — nothing here consumes them, and round-tripping through
/// `f64` would lose precision for no benefit.
mod minijson {
    /// Maximum object/array nesting accepted. `bootc status --json` is
    /// ~6 levels deep; the cap keeps a malformed or hostile payload from
    /// recursing the parser into a stack overflow.
    const MAX_DEPTH: usize = 64;

    /// A parsed JSON value.
    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum Value {
        /// `null`.
        Null,
        /// `true` / `false`.
        Bool(bool),
        /// A number, kept as its source text (never interpreted here).
        Number(String),
        /// A string with all escapes decoded.
        Str(String),
        /// An array.
        Array(Vec<Value>),
        /// An object. Insertion-ordered; duplicate keys resolve to the
        /// first occurrence, matching Python's `json` only for
        /// well-formed input (bootc never emits duplicates).
        Object(Vec<(String, Value)>),
    }

    impl Value {
        /// Look up an object member. `None` for a missing key or a
        /// non-object receiver — which is what makes the `?`-chain in
        /// [`super::parse_booted_image`] read like the Python
        /// subscript chain it replaces.
        pub(super) fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Object(entries) => entries
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, value)| value),
                _ => None,
            }
        }

        /// Borrow the string payload, or `None` for any other variant.
        pub(super) fn as_str(&self) -> Option<&str> {
            match self {
                Value::Str(s) => Some(s),
                _ => None,
            }
        }
    }

    /// Parse a complete JSON document. Returns `None` for anything the
    /// grammar rejects, including trailing garbage after the top-level
    /// value.
    pub(super) fn parse(input: &str) -> Option<Value> {
        let mut parser = Parser {
            bytes: input.as_bytes(),
            pos: 0,
        };
        let value = parser.value(0)?;
        parser.skip_ws();
        if parser.pos == parser.bytes.len() {
            Some(value)
        } else {
            None
        }
    }

    /// Byte-cursor recursive-descent parser over the source text.
    struct Parser<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl Parser<'_> {
        fn peek(&self) -> Option<u8> {
            self.bytes.get(self.pos).copied()
        }

        fn skip_ws(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.pos += 1;
            }
        }

        fn eat(&mut self, expected: u8) -> Option<()> {
            if self.peek() == Some(expected) {
                self.pos += 1;
                Some(())
            } else {
                None
            }
        }

        fn keyword(&mut self, word: &str) -> Option<()> {
            if self.bytes[self.pos..].starts_with(word.as_bytes()) {
                self.pos += word.len();
                Some(())
            } else {
                None
            }
        }

        fn value(&mut self, depth: usize) -> Option<Value> {
            if depth > MAX_DEPTH {
                return None;
            }
            self.skip_ws();
            match self.peek()? {
                b'n' => self.keyword("null").map(|()| Value::Null),
                b't' => self.keyword("true").map(|()| Value::Bool(true)),
                b'f' => self.keyword("false").map(|()| Value::Bool(false)),
                b'"' => self.string().map(Value::Str),
                b'[' => self.array(depth),
                b'{' => self.object(depth),
                b'-' | b'0'..=b'9' => self.number().map(Value::Number),
                _ => None,
            }
        }

        fn array(&mut self, depth: usize) -> Option<Value> {
            self.eat(b'[')?;
            let mut items = Vec::new();
            self.skip_ws();
            if self.eat(b']').is_some() {
                return Some(Value::Array(items));
            }
            loop {
                items.push(self.value(depth + 1)?);
                self.skip_ws();
                if self.eat(b',').is_some() {
                    continue;
                }
                self.eat(b']')?;
                return Some(Value::Array(items));
            }
        }

        fn object(&mut self, depth: usize) -> Option<Value> {
            self.eat(b'{')?;
            let mut entries: Vec<(String, Value)> = Vec::new();
            self.skip_ws();
            if self.eat(b'}').is_some() {
                return Some(Value::Object(entries));
            }
            loop {
                self.skip_ws();
                let key = self.string()?;
                self.skip_ws();
                self.eat(b':')?;
                let value = self.value(depth + 1)?;
                entries.push((key, value));
                self.skip_ws();
                if self.eat(b',').is_some() {
                    continue;
                }
                self.eat(b'}')?;
                return Some(Value::Object(entries));
            }
        }

        fn number(&mut self) -> Option<String> {
            let start = self.pos;
            if self.peek() == Some(b'-') {
                self.pos += 1;
            }
            let digits_start = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == digits_start {
                return None;
            }
            if self.peek() == Some(b'.') {
                self.pos += 1;
                let frac_start = self.pos;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
                if self.pos == frac_start {
                    return None;
                }
            }
            if matches!(self.peek(), Some(b'e' | b'E')) {
                self.pos += 1;
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.pos += 1;
                }
                let exp_start = self.pos;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
                if self.pos == exp_start {
                    return None;
                }
            }
            // Slice is ASCII by construction, so from_utf8 cannot fail.
            std::str::from_utf8(&self.bytes[start..self.pos])
                .ok()
                .map(str::to_string)
        }

        fn string(&mut self) -> Option<String> {
            self.eat(b'"')?;
            let mut out = String::new();
            loop {
                match self.peek()? {
                    b'"' => {
                        self.pos += 1;
                        return Some(out);
                    }
                    b'\\' => {
                        self.pos += 1;
                        let escape = self.peek()?;
                        self.pos += 1;
                        match escape {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'/' => out.push('/'),
                            b'b' => out.push('\u{0008}'),
                            b'f' => out.push('\u{000c}'),
                            b'n' => out.push('\n'),
                            b'r' => out.push('\r'),
                            b't' => out.push('\t'),
                            b'u' => out.push(self.unicode_escape()?),
                            _ => return None,
                        }
                    }
                    // Control characters are not legal raw in a JSON string.
                    c if c < 0x20 => return None,
                    _ => {
                        // Copy one whole UTF-8 scalar, so multi-byte
                        // characters survive intact.
                        let rest = std::str::from_utf8(&self.bytes[self.pos..]).ok()?;
                        let ch = rest.chars().next()?;
                        self.pos += ch.len_utf8();
                        out.push(ch);
                    }
                }
            }
        }

        /// Decode the four hex digits after a `\u`, combining a
        /// surrogate pair with the `\uXXXX` escape that follows it.
        fn unicode_escape(&mut self) -> Option<char> {
            let first = self.hex4()?;
            if (0xD800..=0xDBFF).contains(&first) {
                self.eat(b'\\')?;
                self.eat(b'u')?;
                let second = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return None;
                }
                let combined = 0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                return char::from_u32(combined);
            }
            char::from_u32(first)
        }

        fn hex4(&mut self) -> Option<u32> {
            let end = self.pos.checked_add(4)?;
            let raw = self.bytes.get(self.pos..end)?;
            let text = std::str::from_utf8(raw).ok()?;
            let value = u32::from_str_radix(text, 16).ok()?;
            self.pos = end;
            Some(value)
        }
    }
}

/// Pluck the booted image ref out of `bootc status --json` output.
///
/// Bash twin (`scripts/switch-to-ghcr.sh::150-158`):
///
/// ```python
/// s = json.load(sys.stdin)
/// print(s["status"]["booted"]["image"]["image"]["image"])
/// ```
///
/// `None` for unparseable / unexpected-shape input, mirroring the twin's
/// `except Exception: pass` (which printed an empty line, and the caller
/// treated empty as "unknown"). The five-deep key chain is preserved
/// exactly.
pub(crate) fn parse_booted_image(raw: &str) -> Option<String> {
    let root = minijson::parse(raw.trim())?;
    let image = root
        .get("status")?
        .get("booted")?
        .get("image")?
        .get("image")?
        .get("image")?
        .as_str()?;
    if image.is_empty() {
        None
    } else {
        Some(image.to_string())
    }
}

// ---- Clock (injectable so the wait loops are testable) ---------------------

/// Sleep abstraction so the poll loops can be unit-tested without
/// burning 60 seconds of wall clock. Same shape as
/// `commands/update_cluster.rs`'s `Clock`.
pub(crate) trait Clock {
    /// Block for `duration`.
    fn sleep(&self, duration: Duration);
}

/// Production clock — delegates to [`std::thread::sleep`].
struct RealClock;

impl Clock for RealClock {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

// ---- Block #3: wait_for_ip -------------------------------------------------

/// Poll `virsh domifaddr <name>` until an IPv4 lease appears.
///
/// Bash twin `wait_for_ip` (`scripts/switch-to-ghcr.sh::118-129`): up to
/// `tries` iterations, `sleep 2` between them, empty result on timeout.
/// A `virsh` error is indistinguishable from "no lease yet" in the twin
/// (`2>/dev/null` + `|| true`), so we treat `Err` the same way.
///
/// No sleep happens after the final attempt — the twin sleeps there too,
/// but that trailing 2s is pure latency with no observable effect.
pub(crate) fn wait_for_ip_with(
    conn: &Connection,
    clock: &dyn Clock,
    name: &str,
    tries: u32,
) -> Option<Ipv4Addr> {
    for attempt in 0..tries {
        if let Ok(Some(ip)) = conn.domifaddr(name) {
            return Some(ip);
        }
        if attempt + 1 < tries {
            clock.sleep(Duration::from_secs(u64::from(WAIT_INTERVAL_SECS)));
        }
    }
    None
}

// ---- Block #4: wait_for_ssh ------------------------------------------------

/// Poll `ssh root@<ip> true` until it succeeds.
///
/// Bash twin `wait_for_ssh` (`scripts/switch-to-ghcr.sh::132-141`). Any
/// error — transport or non-zero exit — counts as "not up yet", exactly
/// as the twin's `if _ssh … >/dev/null 2>&1` does.
pub(crate) fn wait_for_ssh_with(exec: &impl SshExec, clock: &dyn Clock, tries: u32) -> bool {
    for attempt in 0..tries {
        if exec.run("true").is_ok() {
            return true;
        }
        if attempt + 1 < tries {
            clock.sleep(Duration::from_secs(u64::from(WAIT_INTERVAL_SECS)));
        }
    }
    false
}

// ---- Block #5: current_image_ref -------------------------------------------

/// Remote command the twin runs to read the booted image ref.
/// Preserved verbatim, including the `2>/dev/null` (bootc writes
/// progress chatter to stderr on some versions).
const BOOTC_STATUS_CMD: &str = "bootc status --json 2>/dev/null";

/// Read the currently-booted image ref from a VM.
///
/// Returns `None` when the VM is unreachable, `bootc` exits non-zero, or
/// the JSON does not have the expected shape — all three collapse to
/// "unknown" in the bash twin too (`|| true` around the pipeline plus
/// Python's bare `except`).
///
/// A non-zero exit still gets its stdout parsed: `bootc status` can
/// print valid JSON and then exit non-zero over an unrelated staged-image
/// warning, and the twin's pipeline would have parsed that stdout too
/// (the `|| true` applies to the whole pipeline, not to `bootc`).
pub(crate) fn current_image_ref_with_exec(exec: &impl SshExec) -> Option<String> {
    let stdout = match exec.run(BOOTC_STATUS_CMD) {
        Ok(out) => out.stdout_lossy(),
        Err(hbird_ssh::Error::NonZeroExit { stdout, .. }) => stdout,
        Err(_) => return None,
    };
    parse_booted_image(&stdout)
}

// ---- Block #6: switch_one --------------------------------------------------

/// What [`switch_one_with_exec`] did to a VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchOutcome {
    /// The VM already tracked the target ref; nothing was run.
    AlreadyTracking,
    /// `bootc switch` ran. `before` / `after` are `None` when the ref
    /// could not be read (the twin's `<unknown>` placeholder).
    Switched {
        /// Ref the VM tracked before the switch.
        before: Option<String>,
        /// Ref the VM reports after the switch.
        after: Option<String>,
    },
}

/// Render an optional ref the way the bash twin's `${cur:-<unknown>}`
/// parameter expansion does.
fn or_unknown(value: Option<&String>) -> &str {
    value.map_or("<unknown>", String::as_str)
}

/// Compare-then-switch a single VM, given an already-reachable
/// [`SshExec`] pointed at `root@<vm-ip>`.
///
/// Bash twin `switch_one` (`scripts/switch-to-ghcr.sh::162-197`), minus
/// the IP/sshd waits (those are [`wait_for_ip_with`] /
/// [`wait_for_ssh_with`], hoisted out so this function is pure SSH and
/// therefore fully unit-testable).
///
/// # Errors
///
/// Returns `Err` when `bootc switch` fails. The twin logs
/// "bootc switch failed (image may not exist on GHCR yet)." and
/// `return 1`; the wording is preserved and the caller decides whether
/// that is fatal (all-VMs mode: exit 1) or not (single-VM mode: exit 0).
pub(crate) fn switch_one_with_exec(
    exec: &impl SshExec,
    name: &str,
    ghcr_ref: &str,
) -> Result<SwitchOutcome> {
    let before = current_image_ref_with_exec(exec);
    if before.as_deref() == Some(ghcr_ref) {
        // Wording preserved verbatim from the bash twin.
        log(&format!(
            "{name}: already tracking {ghcr_ref}; nothing to do."
        ));
        return Ok(SwitchOutcome::AlreadyTracking);
    }
    log(&format!(
        "{name}: switching from '{}' to '{ghcr_ref}'...",
        or_unknown(before.as_ref()),
    ));

    // Twin: `bootc switch '${ref}'`. Same single-quoting, but built by
    // shell_single_quote so a ref containing a quote cannot break out.
    let cmd = format!("bootc switch {}", shell_single_quote(ghcr_ref));
    if let Err(e) = exec.run(&cmd) {
        // Wording preserved verbatim; the `: {e}` suffix is additive so
        // an existing `grep 'bootc switch failed'` still matches.
        log(&format!(
            "{name}: bootc switch failed (image may not exist on GHCR yet). {e}"
        ));
        bail!("{name}: bootc switch to {ghcr_ref} failed: {e}");
    }

    // Re-read so the operator sees the new staged ref.
    let after = current_image_ref_with_exec(exec);
    log(&format!(
        "{name}: now tracking '{}' (was '{}').",
        or_unknown(after.as_ref()),
        or_unknown(before.as_ref()),
    ));
    Ok(SwitchOutcome::Switched { before, after })
}

// ---- Plan ------------------------------------------------------------------

/// Merged view of args + config + env, built once at the top of [`run`].
///
/// Grouping these into one struct (rather than threading eight
/// parameters through the helpers) is what keeps
/// `clippy::too_many_arguments` quiet without an `#[allow]`.
#[derive(Debug, Clone)]
struct Plan {
    ghcr_org: String,
    ghcr_tag: String,
    kvm_host: Option<String>,
    force_rebuild: bool,
    force_switch: bool,
    dry_run: bool,
}

impl Plan {
    fn from_args(args: &SwitchToGhcrArgs, config: Option<hbird_config::ClusterConfig>) -> Self {
        let (cfg_tag, cfg_kvm_host) = match config {
            Some(c) => (Some(c.ghcr_tag), c.kvm_host),
            None => (None, None),
        };
        Self {
            ghcr_org: args
                .ghcr_org
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_GHCR_ORG.to_string()),
            ghcr_tag: args
                .ghcr_tag
                .clone()
                .filter(|s| !s.is_empty())
                .or(cfg_tag)
                .unwrap_or_else(|| DEFAULT_GHCR_TAG.to_string()),
            kvm_host: args
                .kvm_host
                .clone()
                .filter(|s| !s.is_empty())
                .or(cfg_kvm_host),
            force_rebuild: args.force_rebuild,
            force_switch: args.force_switch,
            dry_run: args.dry_run,
        }
    }

    /// SSH options for reaching a VM at `ip` as root.
    ///
    /// Replaces the twin's "re-exec on the KVM host, then plain `ssh`
    /// from there" with a ProxyJump through `--kvm-host`, matching
    /// `commands/update_cluster.rs::node_ssh_opts`. Absent `--kvm-host`
    /// the operator is already on the KVM host, so no jump is added.
    fn vm_ssh_opts(&self, ip: &str, connect_timeout_secs: u64) -> hbird_ssh::SshOptions {
        let mut opts = hbird_ssh::SshOptions::new(ip.to_string())
            .with_user("root")
            .with_connect_timeout(Duration::from_secs(connect_timeout_secs));
        if let Some(jump) = self.kvm_host.as_deref() {
            opts = opts.with_proxy_jump(jump.to_string());
        }
        opts
    }
}

// ---- Orchestration ---------------------------------------------------------

/// Resolve the VM's IP, wait for sshd, then switch it.
///
/// Bash twin `switch_one`'s outer half. Returns `Err` for every SKIP the
/// twin reports (no lease / no sshd / switch failed) so both callers can
/// apply their own fatality policy.
fn switch_vm(conn: &Connection, plan: &Plan, name: &str, ghcr_ref: &str) -> Result<SwitchOutcome> {
    if plan.dry_run {
        log(&format!(
            "{name}: DRY-RUN would resolve IP via virsh domifaddr, wait for sshd, \
             then run: bootc switch {}",
            shell_single_quote(ghcr_ref),
        ));
        return Ok(SwitchOutcome::AlreadyTracking);
    }

    // Wording preserved verbatim from the bash twin.
    log(&format!("{name}: resolving IP..."));
    let Some(ip) = wait_for_ip_with(conn, &RealClock, name, WAIT_TRIES) else {
        log(&format!(
            "{name}: no DHCP lease after {}s; SKIP.",
            WAIT_TRIES * WAIT_INTERVAL_SECS,
        ));
        bail!("{name}: no DHCP lease");
    };
    log(&format!("{name}: ip={ip}; waiting for sshd..."));

    let ip = ip.to_string();
    let probe = hbird_ssh::Client::new(plan.vm_ssh_opts(&ip, PROBE_CONNECT_TIMEOUT_SECS));
    if !wait_for_ssh_with(&probe, &RealClock, WAIT_TRIES) {
        log(&format!(
            "{name}: sshd never came up after {}s; SKIP.",
            WAIT_TRIES * WAIT_INTERVAL_SECS,
        ));
        bail!("{name}: sshd never came up");
    }

    // The twin uses ConnectTimeout=10 for the switch itself; the status
    // reads keep 5. One client per timeout keeps that split intact.
    let switcher = hbird_ssh::Client::new(plan.vm_ssh_opts(&ip, SWITCH_CONNECT_TIMEOUT_SECS));
    switch_one_with_exec(&switcher, name, ghcr_ref)
}

// ---- Block #7: single-VM mode ----------------------------------------------

/// Single-VM mode — `hbird switch-to-ghcr <vm> [<ref>]`.
///
/// Bash twin `scripts/switch-to-ghcr.sh::201-232`. Called by the
/// post-spawn path; every failure here is non-fatal (exit 0) because the
/// deploy that preceded it already succeeded.
///
/// The one fatal case is an un-inferrable ref for an unknown flavor,
/// which the twin also exits 1 on.
fn run_single_vm(
    conn: &Connection,
    plan: &Plan,
    vm: &str,
    explicit_ref: Option<&str>,
) -> Result<()> {
    // #375 opt-out. Checked BEFORE ref inference, exactly as the twin
    // orders it, so a FORCE_REBUILD run of an unknown-flavor VM exits 0
    // rather than 1.
    if plan.force_rebuild && !plan.force_switch {
        // Both WARN lines preserved verbatim — operators grep for #375.
        log(&format!(
            "WARN: FORCE_REBUILD=1 — skipping switch of '{vm}' to GHCR so it keeps tracking its freshly-built install-time image (#375)."
        ));
        log(
            "WARN: set FORCE_SWITCH=1 to switch anyway, or unset FORCE_REBUILD for normal GHCR-tracking behavior.",
        );
        return Ok(());
    }

    let ghcr_ref = match explicit_ref.filter(|s| !s.is_empty()) {
        Some(r) => r.to_string(),
        None => match flavor_for_vm(vm) {
            Some(flavor) => ghcr_ref_for(&plan.ghcr_org, flavor, &plan.ghcr_tag),
            None => {
                // Wording preserved verbatim from the bash twin.
                log(&format!(
                    "ERROR: cannot infer GHCR ref for VM '{vm}' (unknown flavor)."
                ));
                bail!("switch-to-ghcr: cannot infer GHCR ref for VM '{vm}' (unknown flavor)");
            }
        },
    };

    if switch_vm(conn, plan, vm, &ghcr_ref).is_err() {
        // Wording preserved verbatim. Non-fatal: deploy already
        // succeeded; the switch is best-effort.
        log(&format!(
            "WARN: switch failed for {vm}; VM still tracks its install-time image."
        ));
    }
    Ok(())
}

// ---- Block #8: all-VMs mode ------------------------------------------------

/// All-VMs mode — no positional args.
///
/// Bash twin `scripts/switch-to-ghcr.sh::236-259`. Iterates every
/// *running* `hummingbird-*` domain. Unknown flavors are skipped without
/// affecting the exit code; a failed switch sets it to 1 but the walk
/// continues (the [#138] design).
///
/// [#138]: https://github.com/aatchison/hummingbird-k8s/issues/138
fn run_all_vms(conn: &Connection, plan: &Plan) -> Result<()> {
    let running = conn.running_domains().map_err(|e| {
        anyhow::Error::new(e).context(
            "switch-to-ghcr: could not list running libvirt domains — check --kvm-host \
             reachability and libvirt-group membership before re-running",
        )
    })?;

    let vms: Vec<&str> = running
        .iter()
        .map(|d| d.name.as_str())
        .filter(|name| name.starts_with(HUMMINGBIRD_PREFIX))
        .collect();

    if vms.is_empty() {
        // Wording preserved verbatim from the bash twin.
        log("no running hummingbird-* VMs found.");
        return Ok(());
    }

    let mut failed = 0usize;
    for vm in vms {
        let Some(flavor) = flavor_for_vm(vm) else {
            // Wording preserved verbatim. Does NOT affect the exit code
            // in the twin either.
            log(&format!("{vm}: unknown flavor; SKIP."));
            continue;
        };
        let ghcr_ref = ghcr_ref_for(&plan.ghcr_org, flavor, &plan.ghcr_tag);
        if switch_vm(conn, plan, vm, &ghcr_ref).is_err() {
            // Per #138 design: record and STOP for this VM, continue
            // with the others.
            failed += 1;
        }
    }

    if failed > 0 {
        // Twin exits 1 here; anyhow's Err from main() produces the same.
        bail!("switch-to-ghcr: {failed} VM(s) failed to switch — see the SKIP/failure lines above");
    }
    Ok(())
}

// ---- run entrypoint --------------------------------------------------------

/// Dispatch entrypoint invoked by `main.rs`.
///
/// # Exit codes (preserved from the bash twin)
///
/// | situation                                    | exit |
/// |----------------------------------------------|------|
/// | `BOOTC_SWITCH_TO_GHCR=0`                     | 0    |
/// | single-VM, `FORCE_REBUILD=1` w/o `FORCE_SWITCH` | 0 |
/// | single-VM, unknown flavor and no explicit ref | 1   |
/// | single-VM, switch failed                     | 0    |
/// | all-VMs, no running `hummingbird-*` VMs      | 0    |
/// | all-VMs, one or more switches failed         | 1    |
/// | all-VMs, all switched / already tracking     | 0    |
#[tracing::instrument(
    level = "debug",
    skip(args),
    fields(vm = ?args.vm, kvm_host = ?args.kvm_host, dry_run = args.dry_run),
    err(Debug)
)]
pub fn run(args: SwitchToGhcrArgs) -> Result<()> {
    // Block #1: escape hatch, checked before anything else — the twin
    // exits before even sourcing the SSH-wrap shim.
    if !args.bootc_switch_to_ghcr {
        // Wording preserved verbatim from the bash twin.
        log("BOOTC_SWITCH_TO_GHCR=0; skipping.");
        return Ok(());
    }

    let config = match &args.config {
        Some(path) => Some(
            hbird_config::parse(path)
                .map_err(|e| anyhow!("{e}"))
                .with_context(|| {
                    format!("switch-to-ghcr: could not read config {}", path.display())
                })?,
        ),
        None => None,
    };
    let plan = Plan::from_args(&args, config);

    if plan.dry_run {
        log("DRY-RUN mode: no VM will be modified.");
    }

    // Local virsh when --kvm-host is absent (operator on the KVM host),
    // SSH-tunnelled when set. Replaces the twin's ssh-wrap re-exec.
    let conn = crate::virt_bridge::build_connection(plan.kvm_host.as_deref());

    let result = match args.vm.as_deref().filter(|s| !s.is_empty()) {
        Some(vm) => run_single_vm(&conn, &plan, vm, args.ghcr_ref.as_deref()),
        None => run_all_vms(&conn, &plan),
    };
    if plan.dry_run && result.is_ok() {
        log("DRY-RUN done.");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbird_ssh::{Error as SshErr, Result as SshResult, RunOutput};
    use hbird_virt::ssh::{SshClient, SshError as VirtSshError};
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // ---- fixtures --------------------------------------------------------

    /// Canned [`SshExec`] that replays pre-loaded responses in call
    /// order and records every command string. Same shape as the
    /// `MockSshExec` in `commands/update_cluster.rs`.
    struct MockSshExec {
        responses: Mutex<std::collections::VecDeque<SshResult<RunOutput>>>,
        observed: Mutex<Vec<String>>,
    }

    impl MockSshExec {
        fn new(responses: Vec<SshResult<RunOutput>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                observed: Mutex::new(Vec::new()),
            }
        }

        fn commands(&self) -> Vec<String> {
            self.observed.lock().unwrap().clone()
        }
    }

    impl SshExec for MockSshExec {
        fn run(&self, command: &str) -> SshResult<RunOutput> {
            self.observed.lock().unwrap().push(command.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockSshExec: ran out of canned responses — extend the script")
        }

        fn run_with_stdin(&self, command: &str, _stdin: &[u8]) -> SshResult<RunOutput> {
            self.run(command)
        }
    }

    /// rc=0 with the given stdout.
    fn ok_stdout(s: &str) -> SshResult<RunOutput> {
        Ok(RunOutput {
            status: ExitStatus::from_raw(0),
            stdout: s.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    /// Non-zero exit carrying stdout + stderr. `code` is shifted into
    /// the POSIX wait-status shape so `ExitStatus::code()` reports it.
    fn nonzero_exit(code: i32, stdout: &str, stderr: &str) -> SshResult<RunOutput> {
        Err(SshErr::NonZeroExit {
            host: "test-vm".to_string(),
            status: ExitStatus::from_raw((code & 0xff) << 8),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        })
    }

    /// SSH never reached the host at all.
    fn transport_err() -> SshResult<RunOutput> {
        Err(SshErr::NonZeroExit {
            host: "test-vm".to_string(),
            status: ExitStatus::from_raw(255 << 8),
            stdout: String::new(),
            stderr: "ssh: connect to host 192.168.122.9 port 22: Connection refused".to_string(),
        })
    }

    /// Clock that never sleeps but counts how often it was asked to.
    #[derive(Default)]
    struct CountingClock {
        sleeps: AtomicUsize,
    }

    impl CountingClock {
        fn sleeps(&self) -> usize {
            self.sleeps.load(Ordering::SeqCst)
        }
    }

    impl Clock for CountingClock {
        fn sleep(&self, _duration: Duration) {
            self.sleeps.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Canned [`hbird_virt::SshClient`] replaying stdout per call so
    /// `virsh domifaddr` polling can be exercised without libvirt.
    struct CannedVirtClient {
        replies: Mutex<std::collections::VecDeque<Result<String, VirtSshError>>>,
    }

    impl CannedVirtClient {
        fn new(replies: Vec<Result<String, VirtSshError>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
            }
        }
    }

    impl SshClient for CannedVirtClient {
        fn run(&self, _host: &str, command: &str) -> Result<String, VirtSshError> {
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("CannedVirtClient: no canned reply for {command}"))
        }
    }

    fn virt_conn(replies: Vec<Result<String, VirtSshError>>) -> Connection {
        Connection::new_local_with_client(Arc::new(CannedVirtClient::new(replies)))
    }

    /// A `virsh domifaddr` table carrying one IPv4 lease.
    fn domifaddr_ok(ip: &str) -> Result<String, VirtSshError> {
        Ok(format!(
            " Name       MAC address          Protocol     Address\n\
             -------------------------------------------------------------------------------\n\
             \x20vnet0      52:54:00:11:22:33    ipv4         {ip}/24\n"
        ))
    }

    /// Header-only output: domain is up but has no lease yet.
    fn domifaddr_no_lease() -> Result<String, VirtSshError> {
        Ok(" Name       MAC address          Protocol     Address\n".to_string())
    }

    /// Realistic `bootc status --json` payload, trimmed to the shape the
    /// bash twin's `s["status"]["booted"]["image"]["image"]["image"]`
    /// subscript chain walks.
    fn bootc_status_json(image: &str) -> String {
        format!(
            r#"{{
  "apiVersion": "org.containers.bootc/v1",
  "kind": "BootcHost",
  "metadata": {{ "name": "host" }},
  "spec": {{ "image": {{ "image": "{image}", "transport": "registry" }} }},
  "status": {{
    "staged": null,
    "booted": {{
      "image": {{
        "image": {{ "image": "{image}", "transport": "registry", "signature": "insecure" }},
        "version": "42.20250101.0",
        "timestamp": null,
        "imageDigest": "sha256:aaaa"
      }},
      "cachedUpdate": null,
      "incompatible": false,
      "pinned": false,
      "ostree": {{ "checksum": "abc", "deploySerial": 0 }}
    }},
    "rollback": null,
    "type": "bootcHost"
  }}
}}"#
        )
    }

    fn args() -> SwitchToGhcrArgs {
        SwitchToGhcrArgs {
            vm: None,
            ghcr_ref: None,
            config: None,
            kvm_host: None,
            ghcr_org: None,
            ghcr_tag: None,
            bootc_switch_to_ghcr: true,
            force_rebuild: false,
            force_switch: false,
            dry_run: false,
        }
    }

    // ---- block #2: flavor_for_vm ----------------------------------------

    #[test]
    fn flavor_for_vm_maps_control_plane_by_exact_name() {
        assert_eq!(flavor_for_vm("hummingbird-k8s"), Some("hummingbird-k8s"));
    }

    #[test]
    fn flavor_for_vm_maps_workers_by_prefix() {
        // Bash case-arm order matters: `hummingbird-k8s)` is exact, so
        // these fall through to `hummingbird-k8s-worker-*)`.
        for name in [
            "hummingbird-k8s-worker-1",
            "hummingbird-k8s-worker-2",
            "hummingbird-k8s-worker-17",
            // `*` matches the empty string in the bash glob too.
            "hummingbird-k8s-worker-",
        ] {
            assert_eq!(
                flavor_for_vm(name),
                Some("hummingbird-k8s-worker"),
                "{name} should map to the worker flavor",
            );
        }
    }

    #[test]
    fn flavor_for_vm_rejects_unknown_names() {
        for name in [
            // No trailing `-<N>`: the twin's glob requires the dash.
            "hummingbird-k8s-worker",
            "hummingbird-k8s-cp",
            "hummingbird-other",
            "hummingbird-k8s-extra",
            // Production VMs on the same host must never be classified.
            "hbird-geary-1",
            "hbird-forge-runner",
            "hbird-cp1",
            "",
        ] {
            assert_eq!(flavor_for_vm(name), None, "{name} must be unknown flavor");
        }
    }

    #[test]
    fn ghcr_ref_for_matches_bash_interpolation() {
        assert_eq!(
            ghcr_ref_for("ghcr.io/aatchison", "hummingbird-k8s", "latest"),
            "ghcr.io/aatchison/hummingbird-k8s:latest"
        );
        assert_eq!(
            ghcr_ref_for("ghcr.io/aatchison", "hummingbird-k8s-worker", "v1.2.3"),
            "ghcr.io/aatchison/hummingbird-k8s-worker:v1.2.3"
        );
    }

    #[test]
    fn ghcr_ref_for_tolerates_trailing_slash_on_org() {
        assert_eq!(
            ghcr_ref_for("ghcr.io/aatchison/", "hummingbird-k8s", "latest"),
            "ghcr.io/aatchison/hummingbird-k8s:latest",
            "a trailing slash must not produce a double slash in the ref",
        );
    }

    // ---- block #5: bootc status --json parsing --------------------------

    #[test]
    fn parse_booted_image_reads_the_python_subscript_chain() {
        let raw = bootc_status_json("ghcr.io/aatchison/hummingbird-k8s:latest");
        assert_eq!(
            parse_booted_image(&raw).as_deref(),
            Some("ghcr.io/aatchison/hummingbird-k8s:latest"),
        );
    }

    #[test]
    fn parse_booted_image_reads_a_localhost_ref() {
        let raw = bootc_status_json("localhost/hummingbird-k8s:latest");
        assert_eq!(
            parse_booted_image(&raw).as_deref(),
            Some("localhost/hummingbird-k8s:latest"),
        );
    }

    #[test]
    fn parse_booted_image_returns_none_for_empty_input() {
        // Twin: `json.load` raises, `except Exception: pass`, empty line.
        assert_eq!(parse_booted_image(""), None);
        assert_eq!(parse_booted_image("   \n "), None);
    }

    #[test]
    fn parse_booted_image_returns_none_for_non_json() {
        assert_eq!(parse_booted_image("error: not a bootc host"), None);
        assert_eq!(parse_booted_image("{"), None);
        // Trailing garbage after a complete value is rejected.
        assert_eq!(parse_booted_image(r#"{"a": 1} trailing"#), None);
    }

    #[test]
    fn parse_booted_image_returns_none_when_not_booted() {
        // Real shape for a host with no booted deployment recorded.
        assert_eq!(
            parse_booted_image(r#"{"status": {"booted": null, "staged": null}}"#),
            None
        );
        // Chain present but truncated one level early.
        assert_eq!(
            parse_booted_image(r#"{"status": {"booted": {"image": {"image": {}}}}}"#),
            None
        );
    }

    #[test]
    fn parse_booted_image_returns_none_when_image_is_empty_string() {
        // Twin would `print("")`, and the caller's `[[ -n "$cur" ]]`
        // guard treats empty as unknown — so must we.
        let raw = bootc_status_json("");
        assert_eq!(parse_booted_image(&raw), None);
    }

    #[test]
    fn parse_booted_image_returns_none_when_image_is_not_a_string() {
        let raw = r#"{"status":{"booted":{"image":{"image":{"image": 42}}}}}"#;
        assert_eq!(parse_booted_image(raw), None);
    }

    #[test]
    fn parse_booted_image_survives_escapes_elsewhere_in_the_document() {
        // A quote / backslash / newline inside an unrelated field must
        // not terminate string scanning early and desync the parser.
        let raw = r#"{
          "status": {
            "error": "failed: \"boom\" \\ path\nnext\tline \u00e9 \ud83d\ude00",
            "booted": {"image": {"image": {"image": "ghcr.io/x/y:latest"}}}
          }
        }"#;
        assert_eq!(
            parse_booted_image(raw).as_deref(),
            Some("ghcr.io/x/y:latest")
        );
    }

    // ---- minijson unit coverage -----------------------------------------

    #[test]
    fn minijson_parses_scalars_and_containers() {
        use minijson::Value;
        assert_eq!(minijson::parse("null"), Some(Value::Null));
        assert_eq!(minijson::parse(" true "), Some(Value::Bool(true)));
        assert_eq!(minijson::parse("false"), Some(Value::Bool(false)));
        assert_eq!(
            minijson::parse("-12.5e+3"),
            Some(Value::Number("-12.5e+3".to_string()))
        );
        assert_eq!(minijson::parse("[]"), Some(Value::Array(vec![])));
        assert_eq!(minijson::parse("{}"), Some(Value::Object(vec![])));
        assert_eq!(
            minijson::parse(r#"[1, "a", null]"#),
            Some(Value::Array(vec![
                Value::Number("1".to_string()),
                Value::Str("a".to_string()),
                Value::Null,
            ]))
        );
    }

    #[test]
    fn minijson_rejects_malformed_input() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\"}",
            "{\"a\": }",
            "tru",
            "01x",
            "1.",
            "1e",
            "-",
            // Raw control character inside a string is illegal JSON.
            "\"a\nb\"",
            // Unknown escape.
            r#""\q""#,
            // Lone high surrogate with no continuation.
            r#""\ud83d""#,
        ] {
            assert_eq!(minijson::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn minijson_decodes_surrogate_pairs_and_utf8() {
        use minijson::Value;
        assert_eq!(
            minijson::parse(r#""\ud83d\ude00""#),
            Some(Value::Str("\u{1F600}".to_string()))
        );
        assert_eq!(
            minijson::parse("\"caf\u{e9} \u{2713}\""),
            Some(Value::Str("caf\u{e9} \u{2713}".to_string()))
        );
    }

    #[test]
    fn minijson_depth_cap_rejects_runaway_nesting() {
        // 200 nested arrays — past MAX_DEPTH, so the parser refuses
        // rather than recursing into a stack overflow.
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(minijson::parse(&deep), None);
        // Well within the cap still parses.
        let shallow = format!("{}{}", "[".repeat(10), "]".repeat(10));
        assert!(minijson::parse(&shallow).is_some());
    }

    #[test]
    fn minijson_get_returns_none_for_non_objects() {
        use minijson::Value;
        assert_eq!(Value::Null.get("a"), None);
        assert_eq!(Value::Array(vec![]).get("a"), None);
        assert_eq!(Value::Str("x".to_string()).as_str(), Some("x"));
        assert_eq!(Value::Null.as_str(), None);
    }

    // ---- current_image_ref: exit-code classification --------------------

    #[test]
    fn current_image_ref_parses_rc0_output() {
        let raw = bootc_status_json("ghcr.io/aatchison/hummingbird-k8s:latest");
        let exec = MockSshExec::new(vec![ok_stdout(&raw)]);
        assert_eq!(
            current_image_ref_with_exec(&exec).as_deref(),
            Some("ghcr.io/aatchison/hummingbird-k8s:latest"),
        );
        assert_eq!(exec.commands(), vec![BOOTC_STATUS_CMD.to_string()]);
    }

    #[test]
    fn current_image_ref_still_parses_stdout_from_a_nonzero_exit() {
        // bootc can print valid JSON and exit non-zero over an unrelated
        // warning; the twin's `|| true` wraps the whole pipeline so the
        // stdout was parsed there too.
        let raw = bootc_status_json("localhost/hummingbird-k8s:latest");
        let exec = MockSshExec::new(vec![nonzero_exit(1, &raw, "warning: staged image gone")]);
        assert_eq!(
            current_image_ref_with_exec(&exec).as_deref(),
            Some("localhost/hummingbird-k8s:latest"),
        );
    }

    #[test]
    fn current_image_ref_is_none_when_bootc_is_absent() {
        // rc=127 with empty stdout — `bootc: command not found`.
        let exec = MockSshExec::new(vec![nonzero_exit(127, "", "bootc: command not found")]);
        assert_eq!(current_image_ref_with_exec(&exec), None);
    }

    #[test]
    fn current_image_ref_is_none_on_unparseable_output() {
        let exec = MockSshExec::new(vec![ok_stdout("not json at all")]);
        assert_eq!(current_image_ref_with_exec(&exec), None);
    }

    #[test]
    fn current_image_ref_is_none_on_ssh_transport_failure() {
        let exec = MockSshExec::new(vec![transport_err()]);
        assert_eq!(current_image_ref_with_exec(&exec), None);
    }

    // ---- block #6: switch_one -------------------------------------------

    #[test]
    fn switch_one_skips_a_vm_already_tracking_the_target_ref() {
        let target = "ghcr.io/aatchison/hummingbird-k8s:latest";
        let exec = MockSshExec::new(vec![ok_stdout(&bootc_status_json(target))]);
        let outcome =
            switch_one_with_exec(&exec, "hummingbird-k8s", target).expect("no-op must succeed");
        assert_eq!(outcome, SwitchOutcome::AlreadyTracking);
        // Exactly one call: the status read. No `bootc switch`.
        assert_eq!(exec.commands(), vec![BOOTC_STATUS_CMD.to_string()]);
    }

    #[test]
    fn switch_one_switches_a_vm_tracking_localhost() {
        let before = "localhost/hummingbird-k8s:latest";
        let after = "ghcr.io/aatchison/hummingbird-k8s:latest";
        let exec = MockSshExec::new(vec![
            ok_stdout(&bootc_status_json(before)),
            ok_stdout(""),
            ok_stdout(&bootc_status_json(after)),
        ]);
        let outcome = switch_one_with_exec(&exec, "hummingbird-k8s", after).expect("switch ok");
        assert_eq!(
            outcome,
            SwitchOutcome::Switched {
                before: Some(before.to_string()),
                after: Some(after.to_string()),
            }
        );
        let cmds = exec.commands();
        assert_eq!(cmds.len(), 3, "status, switch, status — got {cmds:?}");
        assert_eq!(
            cmds[1], "bootc switch 'ghcr.io/aatchison/hummingbird-k8s:latest'",
            "the switch command must match the bash twin's `bootc switch '${{ref}}'`",
        );
    }

    #[test]
    fn switch_one_switches_even_when_the_current_ref_is_unknown() {
        // First status read fails outright — the twin logs `<unknown>`
        // and switches anyway rather than giving up.
        let after = "ghcr.io/aatchison/hummingbird-k8s-worker:latest";
        let exec = MockSshExec::new(vec![
            transport_err(),
            ok_stdout(""),
            ok_stdout(&bootc_status_json(after)),
        ]);
        let outcome =
            switch_one_with_exec(&exec, "hummingbird-k8s-worker-1", after).expect("switch ok");
        assert_eq!(
            outcome,
            SwitchOutcome::Switched {
                before: None,
                after: Some(after.to_string()),
            }
        );
    }

    #[test]
    fn switch_one_reports_a_switched_vm_whose_post_read_fails() {
        // `bootc switch` succeeded; only the confirmation read failed.
        // That must NOT be reported as a failure.
        let after = "ghcr.io/aatchison/hummingbird-k8s:latest";
        let exec = MockSshExec::new(vec![
            ok_stdout(&bootc_status_json("localhost/hummingbird-k8s:latest")),
            ok_stdout(""),
            transport_err(),
        ]);
        let outcome = switch_one_with_exec(&exec, "hummingbird-k8s", after).expect("switch ok");
        assert!(matches!(
            outcome,
            SwitchOutcome::Switched { after: None, .. }
        ));
    }

    #[test]
    fn switch_one_fails_when_bootc_switch_exits_nonzero() {
        // The GHCR image does not exist yet — the twin's headline case.
        let exec = MockSshExec::new(vec![
            ok_stdout(&bootc_status_json("localhost/hummingbird-k8s:latest")),
            nonzero_exit(1, "", "error: failed to pull: manifest unknown"),
        ]);
        let err = switch_one_with_exec(
            &exec,
            "hummingbird-k8s",
            "ghcr.io/aatchison/hummingbird-k8s:latest",
        )
        .expect_err("a failed bootc switch must surface as Err");
        let msg = err.to_string();
        assert!(msg.contains("hummingbird-k8s"), "{msg}");
        assert!(msg.contains("bootc switch"), "{msg}");
        // No post-switch status read was attempted.
        assert_eq!(exec.commands().len(), 2);
    }

    #[test]
    fn switch_one_fails_when_the_vm_is_unreachable_for_the_switch() {
        let exec = MockSshExec::new(vec![ok_stdout("not json"), transport_err()]);
        assert!(
            switch_one_with_exec(&exec, "hummingbird-k8s", "ghcr.io/x/y:latest").is_err(),
            "an unreachable VM must not be reported as switched",
        );
    }

    #[test]
    fn switch_one_single_quotes_a_hostile_ref() {
        let exec = MockSshExec::new(vec![ok_stdout(""), ok_stdout(""), ok_stdout("")]);
        let _ = switch_one_with_exec(&exec, "hummingbird-k8s", "x'; rm -rf /; echo '");
        let cmds = exec.commands();
        assert_eq!(
            cmds[1], r#"bootc switch 'x'\''; rm -rf /; echo '\'''"#,
            "the ref must be single-quote-escaped, not interpolated raw",
        );
    }

    #[test]
    fn or_unknown_matches_bash_parameter_expansion() {
        assert_eq!(or_unknown(None), "<unknown>");
        let v = "ghcr.io/x/y:latest".to_string();
        assert_eq!(or_unknown(Some(&v)), "ghcr.io/x/y:latest");
    }

    // ---- block #4: wait_for_ssh -----------------------------------------

    #[test]
    fn wait_for_ssh_returns_immediately_when_sshd_is_up() {
        let exec = MockSshExec::new(vec![ok_stdout("")]);
        let clock = CountingClock::default();
        assert!(wait_for_ssh_with(&exec, &clock, 30));
        assert_eq!(clock.sleeps(), 0, "no sleep when the first probe succeeds");
        assert_eq!(exec.commands(), vec!["true".to_string()]);
    }

    #[test]
    fn wait_for_ssh_polls_until_sshd_accepts() {
        let exec = MockSshExec::new(vec![transport_err(), transport_err(), ok_stdout("")]);
        let clock = CountingClock::default();
        assert!(wait_for_ssh_with(&exec, &clock, 30));
        assert_eq!(clock.sleeps(), 2, "one 2s sleep between each failed probe");
    }

    #[test]
    fn wait_for_ssh_gives_up_after_the_try_budget() {
        let exec = MockSshExec::new(vec![transport_err(), transport_err(), transport_err()]);
        let clock = CountingClock::default();
        assert!(!wait_for_ssh_with(&exec, &clock, 3));
        assert_eq!(exec.commands().len(), 3, "exactly `tries` probes");
        assert_eq!(
            clock.sleeps(),
            2,
            "no trailing sleep after the final failed probe",
        );
    }

    #[test]
    fn wait_for_ssh_counts_a_nonzero_exit_as_not_up_yet() {
        // sshd answered but the command failed — the twin's
        // `>/dev/null 2>&1` test treats that as not-ready too.
        let exec = MockSshExec::new(vec![nonzero_exit(255, "", "Connection closed")]);
        let clock = CountingClock::default();
        assert!(!wait_for_ssh_with(&exec, &clock, 1));
    }

    // ---- block #3: wait_for_ip ------------------------------------------

    #[test]
    fn wait_for_ip_returns_the_first_lease() {
        let conn = virt_conn(vec![domifaddr_ok("192.168.122.42")]);
        let clock = CountingClock::default();
        assert_eq!(
            wait_for_ip_with(&conn, &clock, "hummingbird-k8s", 30),
            Some(Ipv4Addr::new(192, 168, 122, 42)),
        );
        assert_eq!(clock.sleeps(), 0);
    }

    #[test]
    fn wait_for_ip_polls_past_an_empty_lease_table() {
        let conn = virt_conn(vec![
            domifaddr_no_lease(),
            domifaddr_no_lease(),
            domifaddr_ok("10.0.0.7"),
        ]);
        let clock = CountingClock::default();
        assert_eq!(
            wait_for_ip_with(&conn, &clock, "hummingbird-k8s-worker-1", 30),
            Some(Ipv4Addr::new(10, 0, 0, 7)),
        );
        assert_eq!(clock.sleeps(), 2);
    }

    #[test]
    fn wait_for_ip_treats_a_virsh_error_as_not_yet() {
        // Twin: `virsh … 2>/dev/null` + `|| true` — an error and an
        // empty table are indistinguishable there.
        let conn = virt_conn(vec![
            Err(VirtSshError::RemoteExit {
                host: "local".to_string(),
                command: "virsh domifaddr".to_string(),
                exit_code: Some(1),
                stderr: "error: Domain not found".to_string(),
            }),
            domifaddr_ok("10.0.0.8"),
        ]);
        let clock = CountingClock::default();
        assert_eq!(
            wait_for_ip_with(&conn, &clock, "hummingbird-k8s", 30),
            Some(Ipv4Addr::new(10, 0, 0, 8)),
        );
    }

    #[test]
    fn wait_for_ip_gives_up_after_the_try_budget() {
        let conn = virt_conn(vec![
            domifaddr_no_lease(),
            domifaddr_no_lease(),
            domifaddr_no_lease(),
        ]);
        let clock = CountingClock::default();
        assert_eq!(wait_for_ip_with(&conn, &clock, "hummingbird-k8s", 3), None);
        assert_eq!(clock.sleeps(), 2, "no trailing sleep after the last poll");
    }

    /// The SKIP wording ("after 60s") is derived from the poll constants
    /// rather than hard-coded, so it cannot drift away from behaviour.
    /// Pin the product to the twin's 60 seconds.
    #[test]
    fn wait_budget_matches_the_bash_twins_60_seconds() {
        assert_eq!(WAIT_TRIES * WAIT_INTERVAL_SECS, 60);
    }

    // ---- Plan resolution -------------------------------------------------

    #[test]
    fn plan_defaults_match_bash_twin_when_nothing_is_set() {
        let p = Plan::from_args(&args(), None);
        assert_eq!(p.ghcr_org, "ghcr.io/aatchison");
        assert_eq!(p.ghcr_tag, "latest");
        assert_eq!(p.kvm_host, None);
        assert!(!p.force_rebuild);
        assert!(!p.force_switch);
    }

    #[test]
    fn plan_flags_beat_config_for_tag_and_kvm_host() {
        let cfg = hbird_config::parse_str(
            "CP_NAME=hummingbird-k8s\nSSH_PUBKEY_FILE=/k\nGHCR_TAG=from-config\nKVM_HOST=from-config-host\n",
        )
        .expect("cfg parses");
        let mut a = args();
        a.ghcr_tag = Some("from-flag".to_string());
        a.kvm_host = Some("from-flag-host".to_string());
        let p = Plan::from_args(&a, Some(cfg));
        assert_eq!(p.ghcr_tag, "from-flag");
        assert_eq!(p.kvm_host.as_deref(), Some("from-flag-host"));
    }

    #[test]
    fn plan_falls_back_to_config_ghcr_tag() {
        let cfg = hbird_config::parse_str(
            "CP_NAME=hummingbird-k8s\nSSH_PUBKEY_FILE=/k\nGHCR_TAG=v9\nKVM_HOST=geary\n",
        )
        .expect("cfg parses");
        let p = Plan::from_args(&args(), Some(cfg));
        assert_eq!(p.ghcr_tag, "v9");
        assert_eq!(p.kvm_host.as_deref(), Some("geary"));
    }

    /// Exported-but-empty env values must behave like unset, matching
    /// bash's `${GHCR_ORG:-…}` (`:-` treats empty as unset).
    #[test]
    fn plan_treats_empty_env_values_as_unset() {
        let mut a = args();
        a.ghcr_org = Some(String::new());
        a.ghcr_tag = Some(String::new());
        a.kvm_host = Some(String::new());
        let p = Plan::from_args(&a, None);
        assert_eq!(p.ghcr_org, "ghcr.io/aatchison");
        assert_eq!(p.ghcr_tag, "latest");
        assert_eq!(p.kvm_host, None);
    }

    // ---- SSH options ------------------------------------------------------

    #[test]
    fn vm_ssh_opts_target_root_at_ip_and_jump_via_kvm_host() {
        let mut a = args();
        a.kvm_host = Some("geary".to_string());
        let p = Plan::from_args(&a, None);
        let argv = p.vm_ssh_opts("192.168.122.42", 5).to_argv();
        assert_eq!(argv.last().map(String::as_str), Some("root@192.168.122.42"));
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ProxyJump=geary"),
            "argv={argv:?}",
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ConnectTimeout=5"),
            "the twin uses ConnectTimeout=5 for probe/status calls: argv={argv:?}",
        );
    }

    #[test]
    fn vm_ssh_opts_omits_proxy_jump_when_running_on_the_kvm_host() {
        // KVM_HOST unset must still work — the operator is on geary.
        let p = Plan::from_args(&args(), None);
        let argv = p.vm_ssh_opts("10.0.0.5", 10).to_argv();
        assert!(
            !argv.iter().any(|s| s.contains("ProxyJump")),
            "argv={argv:?}",
        );
        assert_eq!(argv.last().map(String::as_str), Some("root@10.0.0.5"));
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ConnectTimeout=10"),
            "the twin uses ConnectTimeout=10 for the switch itself: argv={argv:?}",
        );
    }

    #[test]
    fn vm_ssh_opts_keeps_batchmode_so_a_wedged_vm_cannot_prompt() {
        let p = Plan::from_args(&args(), None);
        let argv = p.vm_ssh_opts("10.0.0.5", 5).to_argv();
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "BatchMode=yes"),
            "argv={argv:?}",
        );
    }

    // ---- mode selection + guards ----------------------------------------

    /// The #375 opt-out is checked BEFORE ref inference, so a
    /// FORCE_REBUILD run against an unknown-flavor VM exits 0 rather
    /// than 1. Ordering pinned because reversing it changes the exit
    /// code of `make spawn-workers FORCE_REBUILD=1`.
    #[test]
    fn force_rebuild_skips_before_ref_inference() {
        let mut a = args();
        a.force_rebuild = true;
        let plan = Plan::from_args(&a, None);
        // No canned virsh replies: a skip must not touch the connection.
        let conn = virt_conn(vec![]);
        assert!(run_single_vm(&conn, &plan, "totally-unknown-vm", None).is_ok());
    }

    #[test]
    fn force_switch_re_enables_the_switch_under_force_rebuild() {
        let mut a = args();
        a.force_rebuild = true;
        a.force_switch = true;
        a.dry_run = true;
        let plan = Plan::from_args(&a, None);
        let conn = virt_conn(vec![]);
        // Dry-run so nothing is executed; the point is that we got past
        // the #375 guard and into ref inference.
        assert!(run_single_vm(&conn, &plan, "hummingbird-k8s", None).is_ok());
    }

    #[test]
    fn unknown_flavor_without_an_explicit_ref_is_fatal() {
        let plan = Plan::from_args(&args(), None);
        let conn = virt_conn(vec![]);
        let err = run_single_vm(&conn, &plan, "hbird-geary-1", None)
            .expect_err("unknown flavor must exit non-zero");
        assert!(
            err.to_string().contains("cannot infer GHCR ref"),
            "bash twin wording must survive: {err}",
        );
    }

    #[test]
    fn an_explicit_ref_bypasses_flavor_inference() {
        let mut a = args();
        a.dry_run = true;
        let plan = Plan::from_args(&a, None);
        let conn = virt_conn(vec![]);
        assert!(
            run_single_vm(&conn, &plan, "hbird-geary-1", Some("ghcr.io/x/y:latest")).is_ok(),
            "an operator-supplied ref must work for any VM name",
        );
    }

    #[test]
    fn all_vms_mode_is_a_no_op_when_nothing_hummingbird_is_running() {
        let mut a = args();
        a.dry_run = true;
        let plan = Plan::from_args(&a, None);
        // Only production VMs are up — they must be filtered out and the
        // command must exit 0 without touching them.
        let conn = virt_conn(vec![Ok(
            "hbird-geary-1\nhbird-forge-runner\nhbird-geary-cp\n\n".to_string(),
        )]);
        assert!(run_all_vms(&conn, &plan).is_ok());
    }

    #[test]
    fn all_vms_mode_selects_only_hummingbird_domains() {
        let mut a = args();
        a.dry_run = true;
        let plan = Plan::from_args(&a, None);
        let conn = virt_conn(vec![Ok("hbird-geary-1\n\
                                      hummingbird-k8s\n\
                                      hbird-forge-1\n\
                                      hummingbird-k8s-worker-1\n\
                                      hummingbird-strange\n"
            .to_string())]);
        // hummingbird-strange has an unknown flavor -> SKIP, not a
        // failure; the two known VMs plan cleanly under --dry-run.
        assert!(run_all_vms(&conn, &plan).is_ok());
    }

    #[test]
    fn all_vms_mode_surfaces_a_virsh_listing_failure() {
        let plan = Plan::from_args(&args(), None);
        let conn = virt_conn(vec![Err(VirtSshError::Transport {
            host: "geary".to_string(),
            message: "ssh: Could not resolve hostname geary".to_string(),
        })]);
        let err = run_all_vms(&conn, &plan).expect_err("unreachable KVM host must not exit 0");
        assert!(
            format!("{err:#}").contains("could not list running libvirt domains"),
            "{err:#}",
        );
    }

    #[test]
    fn escape_hatch_exits_zero_without_touching_libvirt() {
        let mut a = args();
        a.bootc_switch_to_ghcr = false;
        // No config, no kvm_host: run() must return before any I/O.
        assert!(run(a).is_ok());
    }
}
