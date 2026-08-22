//! `hbird preflight <sub>` — pre-deploy checks that run BEFORE (or
//! independent of) a deploy.
//!
//! Bash twin: `scripts/check-cilium-k8s-compat.sh` (via
//! `make check-cilium-k8s-compat`).
//!
//! # Why `preflight` and not `verify`
//!
//! `hbird verify <sub>` is the POST-deploy family: it talks to a live
//! cluster over SSH and asserts the deployed state. `preflight` is
//! deliberately a separate family because these checks read
//! *committed repo pins* and need no cluster at all — they are meant to
//! run in CI, in a PR gate, or on a laptop before anything is built.
//!
//! # Exit-code contract (the whole point of the bash twin)
//!
//! | situation                                   | default | `--strict` |
//! |---------------------------------------------|---------|------------|
//! | pinned Cilium supports pinned K8s minor      | 0       | 0          |
//! | mismatch (K8s minor outside Cilium window)   | 0 + WARN| 1          |
//! | Cilium minor missing from embedded matrix    | 0 + WARN| 1          |
//! | pin file unreadable / pin not extractable    | 2       | 2          |
//! | unknown argument                             | 2       | 2          |
//!
//! The default-warn behaviour is load-bearing: a planned Cilium+K8s
//! bump lands as two PRs, and the window between them must not block
//! every other build. `--strict` is for the pre-merge gate.
//!
//! # Matrix source
//!
//! Upstream Cilium docs, per-version compatibility pages (one URL per
//! [`MatrixRow`]). The matrix is EMBEDDED rather than fetched so
//! preflight works offline and adds no network dependency; see
//! [`MATRIX_REVIEWED_AS_OF`] for the freshness signal.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};

// ---- exit codes ------------------------------------------------------------

/// Mismatch (or stale matrix) under `--strict`. Bash twin: `exit 1`.
const EXIT_STRICT_MISMATCH: i32 = 1;

/// Input error — pin file unreadable, or the pin could not be extracted
/// from it. Bash twin: `exit 2` (also clap's own usage-error code, so
/// the two surfaces agree on "operator gave me something unusable").
const EXIT_INPUT_ERROR: i32 = 2;

// ---- clap surface ----------------------------------------------------------

/// Top-level `hbird preflight` — dispatches to one of the pre-deploy checks.
#[derive(Debug, Args)]
pub struct PreflightArgs {
    /// Which preflight check to run.
    #[command(subcommand)]
    pub command: PreflightSubcommand,
}

/// The pre-deploy checks. Today there is exactly one; the subcommand
/// level exists so the next preflight (there will be one) does not
/// force an operator-visible rename.
#[derive(Debug, Subcommand)]
pub enum PreflightSubcommand {
    /// Warn (or, with `--strict`, fail) when the pinned Cilium version
    /// does not cover the pinned/target Kubernetes minor.
    ///
    /// Bash twin: `scripts/check-cilium-k8s-compat.sh`
    /// (via `make check-cilium-k8s-compat`).
    Cilium(CiliumArgs),
}

/// Arguments for `hbird preflight cilium`.
///
/// With no flags at all the check reads the currently-committed pins
/// out of the repo, exactly like the bash twin's no-argument mode.
#[derive(Debug, Args)]
pub struct CiliumArgs {
    /// Override the Cilium version (any patch level, e.g. `1.17.16`).
    /// Bash twin: `--cilium=X.Y.Z`.
    #[arg(long, value_name = "X.Y.Z")]
    pub cilium: Option<String>,

    /// Override the K8s minor (`v1.32` or `1.32`; a patch component is
    /// ignored — the matrix is minor-scoped). Bash twin: `--k8s=vX.Y`.
    #[arg(long = "k8s", value_name = "vX.Y")]
    pub k8s: Option<String>,

    /// Exit 1 on mismatch instead of warning. Bash twin: `--strict`.
    ///
    /// No `env =` binding on purpose: the twin's `STRICT=1` knob is a
    /// Makefile-level variable that expands to `--strict`, and a
    /// clap flag bound to an env var would also fire on `STRICT=0`.
    #[arg(long)]
    pub strict: bool,

    /// Repository root that holds `containers/k8s/{k8s-init.sh,Containerfile}`.
    /// Defaults to the nearest ancestor of the current directory that
    /// contains `containers/k8s/Containerfile`.
    ///
    /// Deliberate divergence: the bash twin resolved the root from its
    /// own `$0` path. A compiled binary has no such anchor, so the
    /// search walks up from the working directory instead.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Print the resolved inputs and the verdict that WOULD be emitted,
    /// then exit 0 regardless. Never changes the process exit code.
    #[arg(long)]
    pub dry_run: bool,
}

// ---- embedded compatibility matrix -----------------------------------------

/// Freshness signal — update when a human re-checks every
/// [`MatrixRow::source_url`] against the live upstream docs. A long
/// stale date is a hint to re-validate before trusting the verdict.
/// Carried over verbatim from the bash twin's `MATRIX_REVIEWED_AS_OF`.
const MATRIX_REVIEWED_AS_OF: &str = "2026-05-27";

/// One row of the embedded Cilium→K8s compatibility matrix.
///
/// Schema note (from the bash twin): the matrix is minor×minor, not
/// patch×patch. Cilium ships patch-level fixes against the same K8s
/// window, so a `1.16.x` patch bump does not move the window.
#[derive(Debug, Clone, Copy)]
struct MatrixRow {
    /// Cilium minor, e.g. `"1.17"`.
    cilium_minor: &'static str,
    /// K8s minors this Cilium minor e2e-tests against, in upstream order.
    k8s_minors: &'static [&'static str],
    /// Upstream page this row was transcribed from.
    source_url: &'static str,
}

/// The embedded matrix. Refresh from the per-row `source_url` when
/// bumping the Cilium pin past the highest known minor here.
const COMPAT_MATRIX: &[MatrixRow] = &[
    MatrixRow {
        cilium_minor: "1.14",
        k8s_minors: &[
            "1.19", "1.20", "1.21", "1.22", "1.23", "1.24", "1.25", "1.26", "1.27",
        ],
        source_url: "https://docs.cilium.io/en/v1.14/network/kubernetes/compatibility/",
    },
    MatrixRow {
        cilium_minor: "1.15",
        k8s_minors: &["1.26", "1.27", "1.28", "1.29"],
        source_url: "https://docs.cilium.io/en/v1.15/network/kubernetes/compatibility/",
    },
    MatrixRow {
        cilium_minor: "1.16",
        k8s_minors: &["1.27", "1.28", "1.29", "1.30"],
        source_url: "https://docs.cilium.io/en/v1.16/network/kubernetes/compatibility/",
    },
    MatrixRow {
        cilium_minor: "1.17",
        k8s_minors: &["1.29", "1.30", "1.31", "1.32"],
        source_url: "https://docs.cilium.io/en/v1.17/network/kubernetes/compatibility/",
    },
    MatrixRow {
        cilium_minor: "1.18",
        k8s_minors: &["1.30", "1.31", "1.32", "1.33"],
        source_url: "https://docs.cilium.io/en/v1.18/network/kubernetes/compatibility/",
    },
];

/// Look up the row for a Cilium minor. `None` = the matrix is stale
/// relative to the pin (bash twin: empty `lookup_supported_k8s` output).
fn lookup_row(cilium_minor: &str) -> Option<&'static MatrixRow> {
    COMPAT_MATRIX
        .iter()
        .find(|row| row.cilium_minor == cilium_minor)
}

// ---- version parsing / comparison ------------------------------------------

/// Parse a `X.Y` (or `X.Y.Z…`) version into its `(major, minor)` pair.
/// Returns `None` when either component is missing or non-numeric.
///
/// This is the comparison primitive the bash twin got only half right —
/// see [`lowest_supported_minor`].
fn parse_minor_pair(s: &str) -> Option<(u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Lowest K8s minor in a row, by NUMERIC `(major, minor)` order.
///
/// # Divergence from the bash twin (bug fix)
///
/// The bash twin computed `sup_min`/`sup_max` with
/// `awk 'BEGIN{exit !(a+0 < b+0 …)}'`, which coerces `"1.9"` → `1.9`
/// and `"1.10"` → `1.10` and therefore ranks `1.10` BELOW `1.9`.
/// Confusingly, the very next block (`below_range`) compares correctly
/// by splitting on `.`, so the two halves of the same diagnostic could
/// disagree. Any row mixing single- and double-digit minors (`1.9`
/// alongside `1.10`) would print the wrong "lowest:" hint. We compare
/// component-wise everywhere. Pinned by
/// `lowest_supported_minor_orders_1_9_below_1_10`.
///
/// The bash twin also computed `sup_max` and never used it; that dead
/// value is not ported.
fn lowest_supported_minor(k8s_minors: &[&'static str]) -> Option<&'static str> {
    k8s_minors
        .iter()
        .copied()
        .min_by(|a, b| match (parse_minor_pair(a), parse_minor_pair(b)) {
            (Some(pa), Some(pb)) => pa.cmp(&pb),
            // Unparseable entries sort last so a typo in the embedded
            // table can never masquerade as the window's floor.
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.cmp(b),
        })
}

/// `true` when `k8s_minor` sits strictly BELOW `lowest`.
///
/// Mirrors the bash twin's `below_range` awk block, including its
/// fail-soft shape: an unparseable input yields `false` ("not below"),
/// which routes the operator to the generic "bump Cilium" hint rather
/// than a bogus "downgrade Cilium" one.
fn is_below_window(k8s_minor: &str, lowest: &str) -> bool {
    match (parse_minor_pair(k8s_minor), parse_minor_pair(lowest)) {
        (Some(k), Some(lo)) => k < lo,
        _ => false,
    }
}

// ---- pin extraction --------------------------------------------------------

/// Extract the Cilium pin from `k8s-init.sh` content.
///
/// Mirrors the bash twin's
/// `grep -E '^[[:space:]]*--version[[:space:]=]+v?[0-9]+\.[0-9]+\.[0-9]+' | head -n1`
/// plus the `sed` that strips a leading `v`. Anchoring on `--version`
/// (rather than on "cilium") is what keeps the Containerfile's
/// `CILIUM_CLI_VERSION` build-arg from matching.
///
/// Returns `None` when no line matches — the caller turns that into the
/// bash twin's exit-2 diagnostic.
fn extract_cilium_pin(content: &str) -> Option<String> {
    for line in content.lines() {
        let rest = line.trim_start();
        let Some(rest) = rest.strip_prefix("--version") else {
            continue;
        };
        // `[[:space:]=]+` — at least one separator, then the version.
        let trimmed = rest.trim_start_matches([' ', '\t', '=']);
        if trimmed.len() == rest.len() {
            continue; // no separator at all, e.g. `--versionfoo`
        }
        let value = trimmed.strip_prefix('v').unwrap_or(trimmed);
        if let Some(v) = leading_dotted_number(value, 3) {
            return Some(v);
        }
    }
    None
}

/// Extract the K8s pin from `Containerfile` content, as a bare `X.Y`.
///
/// Mirrors the bash twin's
/// `grep -E '^ARG[[:space:]]+K8S_VERSION[[:space:]]*=[[:space:]]*v?[0-9]+\.[0-9]+' | head -n1`.
/// The `^ARG` anchor is literal: an indented `ARG` line does NOT match
/// (Containerfile directives are column-0), and that is preserved.
fn extract_k8s_pin(content: &str) -> Option<String> {
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("ARG") else {
            continue;
        };
        let after_arg = rest.trim_start_matches([' ', '\t']);
        if after_arg.len() == rest.len() {
            continue; // `[[:space:]]+` requires at least one space
        }
        let Some(rest) = after_arg.strip_prefix("K8S_VERSION") else {
            continue;
        };
        let rest = rest.trim_start_matches([' ', '\t']);
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start_matches([' ', '\t']);
        let value = rest.strip_prefix('v').unwrap_or(rest);
        if let Some(v) = leading_dotted_number(value, 2) {
            return Some(v);
        }
    }
    None
}

/// Take the leading `N` dot-separated numeric components of `s`
/// (e.g. `leading_dotted_number("1.16.5 \\", 3) == Some("1.16.5")`).
/// Returns `None` if fewer than `N` numeric components are present.
///
/// This is the shared engine behind the twin's two `sed -E` extractions
/// and behind [`normalize_k8s_minor`].
fn leading_dotted_number(s: &str, components: usize) -> Option<String> {
    let mut out = String::new();
    let mut rest = s;
    for i in 0..components {
        if i > 0 {
            rest = rest.strip_prefix('.')?;
            out.push('.');
        }
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        rest = &rest[digits.len()..];
        out.push_str(&digits);
    }
    Some(out)
}

/// Normalize an operator-supplied K8s version to a bare `X.Y`: drop a
/// leading `v`, drop any patch component.
///
/// Faithful to the bash twin's `sed -E 's/^v?([0-9]+\.[0-9]+).*/\1/'`,
/// INCLUDING the pass-through on no match: `sed` prints the line
/// unchanged when the pattern does not match, so `--k8s=garbage` flows
/// on as `garbage` and lands in the mismatch branch rather than erroring.
fn normalize_k8s_minor(raw: &str) -> String {
    let stripped = raw.strip_prefix('v').unwrap_or(raw);
    leading_dotted_number(stripped, 2).unwrap_or_else(|| raw.to_string())
}

/// Derive the Cilium minor (`1.16.5` → `1.16`) for matrix lookup.
/// Same `sed` pass-through semantics as [`normalize_k8s_minor`].
fn cilium_minor_of(version: &str) -> String {
    leading_dotted_number(version, 2).unwrap_or_else(|| version.to_string())
}

// ---- repo-root discovery ---------------------------------------------------

/// Relative path of the file that carries the Cilium pin.
const K8S_INIT_REL: &str = "containers/k8s/k8s-init.sh";
/// Relative path of the file that carries the K8s pin.
const CONTAINERFILE_REL: &str = "containers/k8s/Containerfile";

/// Walk up from `start` looking for a directory that contains
/// `containers/k8s/Containerfile`. Returns `None` when the filesystem
/// root is reached without a hit; the caller then falls back to `start`
/// so the error message names a concrete path.
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(CONTAINERFILE_REL).is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

// ---- resolved inputs -------------------------------------------------------

/// The two versions the verdict is computed from, plus how we got them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pins {
    /// Full Cilium version, e.g. `1.17.16`.
    cilium_version: String,
    /// Cilium minor derived from it, e.g. `1.17`.
    cilium_minor: String,
    /// Bare K8s minor, e.g. `1.31`.
    k8s_minor: String,
    /// Operator-visible provenance of the Cilium pin (a path, or `--cilium`).
    cilium_source: String,
    /// Operator-visible provenance of the K8s pin (a path, or `--k8s`).
    k8s_source: String,
}

/// Failure to resolve the pins. Every variant is an exit-2 condition;
/// the payload is the bash twin's verbatim `ERROR:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PinError(String);

/// Resolve both pins from the flags, falling back to the committed
/// repo files. Mirrors the bash twin's override-then-file order and
/// its four distinct exit-2 diagnostics.
///
/// `read_file` is injected so the unreadable-file and
/// malformed-file branches are unit-testable without touching a real
/// filesystem (the production caller passes [`std::fs::read_to_string`]).
fn resolve_pins<F>(args: &CiliumArgs, repo_root: &Path, read_file: F) -> Result<Pins, PinError>
where
    F: Fn(&Path) -> std::io::Result<String>,
{
    let k8s_init = repo_root.join(K8S_INIT_REL);
    let containerfile = repo_root.join(CONTAINERFILE_REL);

    // Bash treats `--cilium=` (empty) as "not supplied" because it
    // tests `[ -n "$CILIUM_OVERRIDE" ]`; `.filter(non-empty)` matches.
    let cilium_override = args.cilium.clone().filter(|s| !s.is_empty());
    let (cilium_version, cilium_source) = match cilium_override {
        Some(v) => (v, "--cilium".to_string()),
        None => {
            let content = read_file(&k8s_init).map_err(|_| {
                // Verbatim bash wording (`ERROR: cannot read … to
                // extract Cilium pin`) — operators grep for it.
                PinError(format!(
                    "ERROR: cannot read {} to extract Cilium pin",
                    k8s_init.display()
                ))
            })?;
            let v = extract_cilium_pin(&content).ok_or_else(|| {
                PinError("ERROR: could not extract Cilium --version from k8s-init.sh".to_string())
            })?;
            (v, k8s_init.display().to_string())
        }
    };

    let k8s_override = args.k8s.clone().filter(|s| !s.is_empty());
    let (k8s_minor, k8s_source) = match k8s_override {
        Some(v) => (normalize_k8s_minor(&v), "--k8s".to_string()),
        None => {
            let content = read_file(&containerfile).map_err(|_| {
                PinError(format!(
                    "ERROR: cannot read {} to extract K8S_VERSION pin",
                    containerfile.display()
                ))
            })?;
            let v = extract_k8s_pin(&content).ok_or_else(|| {
                PinError("ERROR: could not extract K8S_VERSION from Containerfile".to_string())
            })?;
            (v, containerfile.display().to_string())
        }
    };

    Ok(Pins {
        cilium_minor: cilium_minor_of(&cilium_version),
        cilium_version,
        k8s_minor,
        cilium_source,
        k8s_source,
    })
}

// ---- verdict ---------------------------------------------------------------

/// What the check concluded. Kept separate from printing so both the
/// wording and the exit code can be unit-tested without capturing
/// process output.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    /// K8s minor is inside the pinned Cilium minor's window.
    Supported {
        /// Space-separated window, as printed to the operator.
        supported: String,
    },
    /// The Cilium minor has no row at all — the embedded matrix is
    /// stale relative to the pin.
    MatrixStale,
    /// The row exists but does not list this K8s minor.
    Mismatch {
        /// Space-separated window, as printed to the operator.
        supported: String,
        /// Lowest minor in the window (the `lowest:` hint).
        lowest: String,
        /// `true` when the K8s minor is below the window, which flips
        /// the remediation hint from "bump Cilium" to "downgrade Cilium".
        below_window: bool,
        /// The row's own [`MatrixRow::source_url`]. The bash twin
        /// re-derived this URL from the minor; using the transcribed
        /// one keeps the pointer honest if upstream ever moves a page.
        source_url: &'static str,
    },
}

/// Classify a resolved pin pair against the embedded matrix.
fn classify(pins: &Pins) -> Verdict {
    let Some(row) = lookup_row(&pins.cilium_minor) else {
        return Verdict::MatrixStale;
    };
    let supported = row.k8s_minors.join(" ");
    if row.k8s_minors.contains(&pins.k8s_minor.as_str()) {
        return Verdict::Supported { supported };
    }
    // A row always has at least one minor; the empty-row fallback keeps
    // the function total without an unwrap.
    let lowest = lowest_supported_minor(row.k8s_minors).unwrap_or("");
    Verdict::Mismatch {
        supported,
        lowest: lowest.to_string(),
        below_window: is_below_window(&pins.k8s_minor, lowest),
        source_url: row.source_url,
    }
}

/// Process exit code for a verdict under the current strictness.
/// This IS the operator contract — see the table in the module docs.
fn verdict_exit_code(verdict: &Verdict, strict: bool) -> i32 {
    match verdict {
        Verdict::Supported { .. } => 0,
        Verdict::MatrixStale | Verdict::Mismatch { .. } => {
            if strict {
                EXIT_STRICT_MISMATCH
            } else {
                0
            }
        }
    }
}

/// Render the verdict as the operator sees it: `(stdout_lines, stderr_lines)`.
///
/// Every string here is grep-anchored by the bash twin's bats suite
/// (`tests/scripts/check-cilium-k8s-compat.bats`) and by operators, so
/// the wording is preserved verbatim. The ONE deliberate change is the
/// "Refresh the matrix in …" pointer, which now names this Rust module
/// instead of the retired shell script — pointing an operator at a file
/// that no longer holds the table would be worse than a wording diff.
fn render_verdict(pins: &Pins, verdict: &Verdict) -> (Vec<String>, Vec<String>) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    match verdict {
        Verdict::Supported { supported } => {
            out.push(format!(
                "OK: Cilium {} supports K8s {} (supported: {supported}).",
                pins.cilium_version, pins.k8s_minor
            ));
        }
        Verdict::MatrixStale => {
            err.push(format!(
                "WARN: Cilium minor {} (pinned {}) is not in the embedded compat matrix.",
                pins.cilium_minor, pins.cilium_version
            ));
            err.push(
                "      Refresh the matrix in \
                 rust/crates/hbird-cli/src/commands/preflight.rs from:"
                    .to_string(),
            );
            err.push(format!(
                "      https://docs.cilium.io/en/v{}/network/kubernetes/compatibility/",
                pins.cilium_minor
            ));
            err.push(format!("      K8s pin: {}", pins.k8s_minor));
        }
        Verdict::Mismatch {
            supported,
            lowest,
            below_window,
            source_url,
        } => {
            err.push(format!(
                "WARN: Cilium {} does NOT list K8s {} as a supported minor.",
                pins.cilium_version, pins.k8s_minor
            ));
            err.push(format!(
                "      Cilium {}.x supported K8s minors: {supported}",
                pins.cilium_minor
            ));
            if *below_window {
                err.push(format!(
                    "      K8s {} is BELOW Cilium {}.x's window (lowest: {lowest}).",
                    pins.k8s_minor, pins.cilium_minor
                ));
                err.push(format!(
                    "      Downgrade Cilium to a minor that supports K8s {}, OR upgrade K8s into range.",
                    pins.k8s_minor
                ));
            } else {
                err.push(
                    "      Bump Cilium first (see docs/cilium-migration.md) OR pick a K8s minor in range."
                        .to_string(),
                );
            }
            err.push(format!("      Upstream matrix: {source_url}"));
            err.push(
                "      See also: docs/k8s-version-upgrade.md \"Pre-flight checklist\".".to_string(),
            );
        }
    }
    (out, err)
}

/// Deterministic `--dry-run` plan. Reads the pin files (a read is not a
/// side effect) and reports the verdict + exit code it WOULD produce,
/// without ever adopting that exit code itself.
fn render_dry_run_plan(pins: &Pins, verdict: &Verdict, strict: bool) -> Vec<String> {
    let verdict_name = match verdict {
        Verdict::Supported { .. } => "OK",
        Verdict::MatrixStale => "WARN (matrix stale)",
        Verdict::Mismatch { .. } => "WARN (mismatch)",
    };
    vec![
        format!(
            "[preflight-cilium] DRY-RUN cilium pin: {} (minor {}) from {}",
            pins.cilium_version, pins.cilium_minor, pins.cilium_source
        ),
        format!(
            "[preflight-cilium] DRY-RUN k8s pin:    {} from {}",
            pins.k8s_minor, pins.k8s_source
        ),
        format!("[preflight-cilium] DRY-RUN strict: {strict}"),
        format!(
            "[preflight-cilium] DRY-RUN matrix rows: {} (reviewed as of {MATRIX_REVIEWED_AS_OF})",
            COMPAT_MATRIX
                .iter()
                .map(|r| r.cilium_minor)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        format!(
            "[preflight-cilium] DRY-RUN would emit: {verdict_name} and exit {}",
            verdict_exit_code(verdict, strict)
        ),
    ]
}

// ---- dispatch --------------------------------------------------------------

/// Dispatch `hbird preflight <sub>`.
pub fn run(args: PreflightArgs) -> Result<()> {
    match args.command {
        PreflightSubcommand::Cilium(a) => run_cilium(a),
    }
}

/// `hbird preflight cilium` body.
///
/// Never returns `Err`: the bash twin's contract is expressed purely in
/// exit codes (0 / 1 / 2), and routing a mismatch through `anyhow`
/// would prepend an "Error: " line the operator's grep does not expect.
/// Non-zero outcomes call [`std::process::exit`] directly, matching
/// `commands::kubectl`'s precedent for exit-code passthrough.
fn run_cilium(args: CiliumArgs) -> Result<()> {
    let repo_root = match args.repo_root.clone() {
        Some(p) => p,
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            find_repo_root(&cwd).unwrap_or(cwd)
        }
    };

    let pins = match resolve_pins(&args, &repo_root, |p| std::fs::read_to_string(p)) {
        Ok(p) => p,
        Err(PinError(msg)) => {
            eprintln!("{msg}");
            std::process::exit(EXIT_INPUT_ERROR);
        }
    };

    let verdict = classify(&pins);

    if args.dry_run {
        for line in render_dry_run_plan(&pins, &verdict, args.strict) {
            println!("{line}");
        }
        return Ok(());
    }

    let (stdout_lines, stderr_lines) = render_verdict(&pins, &verdict);
    for line in stdout_lines {
        println!("{line}");
    }
    for line in stderr_lines {
        eprintln!("{line}");
    }

    match verdict_exit_code(&verdict, args.strict) {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `CiliumArgs` with both overrides set — the shape every
    /// matrix test uses (no filesystem involved).
    fn args_override(cilium: &str, k8s: &str, strict: bool) -> CiliumArgs {
        CiliumArgs {
            cilium: Some(cilium.to_string()),
            k8s: Some(k8s.to_string()),
            strict,
            repo_root: None,
            dry_run: false,
        }
    }

    /// Resolve pins from overrides only; the reader must never be called.
    fn pins_from(cilium: &str, k8s: &str) -> Pins {
        resolve_pins(
            &args_override(cilium, k8s, false),
            Path::new("/nonexistent"),
            |p| panic!("reader must not be called when both overrides are set: {p:?}"),
        )
        .expect("overrides always resolve")
    }

    // ---- leading_dotted_number ------------------------------------------

    #[test]
    fn leading_dotted_number_takes_requested_components() {
        assert_eq!(
            leading_dotted_number("1.16.5 \\", 3),
            Some("1.16.5".to_string())
        );
        assert_eq!(leading_dotted_number("1.16.5", 2), Some("1.16".to_string()));
        assert_eq!(leading_dotted_number("1.31", 2), Some("1.31".to_string()));
    }

    #[test]
    fn leading_dotted_number_rejects_too_few_components() {
        assert_eq!(leading_dotted_number("1.16", 3), None);
        assert_eq!(leading_dotted_number("1", 2), None);
        assert_eq!(leading_dotted_number("", 2), None);
        assert_eq!(leading_dotted_number("abc", 2), None);
        // Trailing dot with no digits after it is NOT a third component.
        assert_eq!(leading_dotted_number("1.16.", 3), None);
    }

    // ---- version comparison (the subtle-bug zone) ------------------------

    #[test]
    fn parse_minor_pair_handles_v_prefix_and_patch() {
        assert_eq!(parse_minor_pair("1.31"), Some((1, 31)));
        assert_eq!(parse_minor_pair("v1.31"), Some((1, 31)));
        assert_eq!(parse_minor_pair("1.31.4"), Some((1, 31)));
        assert_eq!(parse_minor_pair("garbage"), None);
        assert_eq!(parse_minor_pair("1"), None);
        assert_eq!(parse_minor_pair("1.x"), None);
    }

    /// BUG FIX vs the bash twin. `awk`'s `a+0` coercion ranks `1.10`
    /// (10 decimal-ish → 1.10) BELOW `1.9` (1.9), so the twin's
    /// `sup_min` loop would report `1.10` as the window floor for a row
    /// spanning `1.9 … 1.11`. Component-wise comparison gets it right.
    #[test]
    fn lowest_supported_minor_orders_1_9_below_1_10() {
        let row: &[&'static str] = &["1.10", "1.9", "1.11"];
        assert_eq!(lowest_supported_minor(row), Some("1.9"));
        // The naive numeric coercion the bash twin used would answer
        // "1.10" here; assert we are NOT doing that.
        assert_ne!(lowest_supported_minor(row), Some("1.10"));
    }

    #[test]
    fn lowest_supported_minor_matches_bash_on_uniform_rows() {
        // Rows in the live matrix are all two-digit minors, where the
        // twin's coercion happened to be right. Same answer here.
        assert_eq!(
            lowest_supported_minor(&["1.29", "1.30", "1.31", "1.32"]),
            Some("1.29")
        );
        assert_eq!(lowest_supported_minor(&[]), None);
    }

    #[test]
    fn lowest_supported_minor_sorts_unparseable_entries_last() {
        assert_eq!(lowest_supported_minor(&["nonsense", "1.30"]), Some("1.30"));
    }

    #[test]
    fn is_below_window_compares_component_wise() {
        assert!(is_below_window("1.9", "1.10"));
        assert!(is_below_window("1.26", "1.29"));
        assert!(!is_below_window("1.29", "1.29"));
        assert!(!is_below_window("1.33", "1.29"));
        assert!(is_below_window("0.99", "1.0"));
    }

    #[test]
    fn is_below_window_is_fail_soft_on_garbage() {
        // Bash's awk yields 0 ("not below") for non-numeric input; we
        // match so the remediation hint stays the generic one.
        assert!(!is_below_window("garbage", "1.29"));
        assert!(!is_below_window("1.29", "garbage"));
    }

    // ---- extraction ------------------------------------------------------

    #[test]
    fn extract_cilium_pin_reads_the_install_line() {
        let content = "#!/bin/bash\n\
                       KUBECONFIG=/etc/kubernetes/admin.conf cilium install \\\n\
                       \x20 --version 1.17.16 \\\n\
                       \x20 --set kubeProxyReplacement=true \\\n";
        assert_eq!(
            extract_cilium_pin(content),
            Some("1.17.16".to_string()),
            "must match the indented `--version X.Y.Z` line"
        );
    }

    #[test]
    fn extract_cilium_pin_accepts_equals_and_v_prefix() {
        assert_eq!(
            extract_cilium_pin("  --version=v1.16.5\n"),
            Some("1.16.5".to_string())
        );
        assert_eq!(
            extract_cilium_pin("\t--version\tv1.14.10 \\\n"),
            Some("1.14.10".to_string())
        );
    }

    #[test]
    fn extract_cilium_pin_takes_the_first_match_only() {
        let content = "  --version 1.16.5\n  --version 1.18.0\n";
        assert_eq!(extract_cilium_pin(content), Some("1.16.5".to_string()));
    }

    #[test]
    fn extract_cilium_pin_ignores_cilium_cli_version_build_arg() {
        // The Containerfile's `ARG CILIUM_CLI_VERSION=v0.16.0` must not
        // match — the twin anchors on `--version`, and so do we.
        let content = "ARG CILIUM_CLI_VERSION=v0.16.0\nRUN cilium status\n";
        assert_eq!(extract_cilium_pin(content), None);
    }

    #[test]
    fn extract_cilium_pin_requires_three_components_and_a_separator() {
        assert_eq!(extract_cilium_pin("  --version 1.16\n"), None);
        assert_eq!(extract_cilium_pin("  --version1.16.5\n"), None);
        assert_eq!(extract_cilium_pin(""), None);
    }

    #[test]
    fn extract_k8s_pin_reads_the_arg_line() {
        let content = "FROM quay.io/example/base:latest\n\
                       ARG K8S_VERSION=v1.31\n\
                       ARG POD_CIDR=10.244.0.0/16\n";
        assert_eq!(extract_k8s_pin(content), Some("1.31".to_string()));
    }

    #[test]
    fn extract_k8s_pin_tolerates_spaces_and_missing_v() {
        assert_eq!(
            extract_k8s_pin("ARG   K8S_VERSION = 1.32\n"),
            Some("1.32".to_string())
        );
    }

    #[test]
    fn extract_k8s_pin_drops_a_patch_component() {
        // Matrix is minor-scoped; `v1.31.4` still looks up the 1.31 row.
        assert_eq!(
            extract_k8s_pin("ARG K8S_VERSION=v1.31.4\n"),
            Some("1.31".to_string())
        );
    }

    #[test]
    fn extract_k8s_pin_requires_column_zero_arg() {
        // `^ARG` in the twin's grep is a literal line anchor.
        assert_eq!(extract_k8s_pin("  ARG K8S_VERSION=v1.31\n"), None);
        assert_eq!(extract_k8s_pin("ARGK8S_VERSION=v1.31\n"), None);
        assert_eq!(extract_k8s_pin("ARG OTHER_VERSION=v1.31\n"), None);
        assert_eq!(extract_k8s_pin("ARG K8S_VERSION\n"), None);
        assert_eq!(extract_k8s_pin(""), None);
    }

    #[test]
    fn normalize_k8s_minor_accepts_both_forms() {
        assert_eq!(normalize_k8s_minor("v1.30"), "1.30");
        assert_eq!(normalize_k8s_minor("1.30"), "1.30");
        assert_eq!(normalize_k8s_minor("v1.30.4"), "1.30");
    }

    #[test]
    fn normalize_k8s_minor_passes_garbage_through_like_sed() {
        // `sed` prints a non-matching line unchanged; the twin then
        // fails the matrix membership test and warns. Same here.
        assert_eq!(normalize_k8s_minor("garbage"), "garbage");
        assert_eq!(normalize_k8s_minor(""), "");
    }

    #[test]
    fn cilium_minor_of_truncates_to_two_components() {
        assert_eq!(cilium_minor_of("1.17.16"), "1.17");
        assert_eq!(cilium_minor_of("1.17"), "1.17");
        assert_eq!(cilium_minor_of("nonsense"), "nonsense");
    }

    // ---- matrix membership ----------------------------------------------

    #[test]
    fn matrix_rows_are_unique_and_nonempty() {
        for row in COMPAT_MATRIX {
            assert!(
                !row.k8s_minors.is_empty(),
                "row {} has no K8s minors",
                row.cilium_minor
            );
            assert!(
                row.source_url.starts_with("https://docs.cilium.io/en/v"),
                "row {} has a non-upstream source URL",
                row.cilium_minor
            );
            assert_eq!(
                COMPAT_MATRIX
                    .iter()
                    .filter(|r| r.cilium_minor == row.cilium_minor)
                    .count(),
                1,
                "duplicate row for {}",
                row.cilium_minor
            );
        }
    }

    /// Pin the embedded table cell-for-cell against the bash twin's
    /// heredoc, so a refresh that flips a row by accident is loud.
    #[test]
    fn matrix_matches_the_bash_twin_table() {
        let expected: &[(&str, &[&str])] = &[
            (
                "1.14",
                &[
                    "1.19", "1.20", "1.21", "1.22", "1.23", "1.24", "1.25", "1.26", "1.27",
                ],
            ),
            ("1.15", &["1.26", "1.27", "1.28", "1.29"]),
            ("1.16", &["1.27", "1.28", "1.29", "1.30"]),
            ("1.17", &["1.29", "1.30", "1.31", "1.32"]),
            ("1.18", &["1.30", "1.31", "1.32", "1.33"]),
        ];
        assert_eq!(COMPAT_MATRIX.len(), expected.len());
        for (minor, minors) in expected {
            let row = lookup_row(minor).unwrap_or_else(|| panic!("missing row {minor}"));
            assert_eq!(row.k8s_minors, *minors, "row {minor} drifted");
        }
    }

    #[test]
    fn lookup_row_misses_unknown_minor() {
        assert!(lookup_row("1.99").is_none());
        assert!(lookup_row("").is_none());
    }

    // ---- classify + exit codes (the operator contract) -------------------

    #[test]
    fn classify_supported_pair() {
        // Bats twin: "known supported pair returns OK" (1.16.5 + v1.29).
        let v = classify(&pins_from("1.16.5", "v1.29"));
        assert_eq!(
            v,
            Verdict::Supported {
                supported: "1.27 1.28 1.29 1.30".to_string()
            }
        );
    }

    #[test]
    fn classify_mismatch_above_window() {
        // Bats twin: 1.16.5 + v1.31 — above the 1.27-1.30 window.
        let v = classify(&pins_from("1.16.5", "v1.31"));
        match v {
            Verdict::Mismatch {
                lowest,
                below_window,
                ..
            } => {
                assert_eq!(lowest, "1.27");
                assert!(!below_window, "1.31 is ABOVE the 1.16 window");
            }
            other => panic!("expected mismatch, got {other:?}"),
        }
    }

    #[test]
    fn classify_mismatch_below_window() {
        // 1.18's window starts at 1.30; K8s 1.28 is below it, which
        // flips the remediation hint.
        let v = classify(&pins_from("1.18.0", "v1.28"));
        match v {
            Verdict::Mismatch {
                lowest,
                below_window,
                ..
            } => {
                assert_eq!(lowest, "1.30");
                assert!(below_window, "1.28 is BELOW the 1.18 window");
            }
            other => panic!("expected mismatch, got {other:?}"),
        }
    }

    #[test]
    fn classify_unknown_cilium_minor_is_matrix_stale() {
        assert_eq!(
            classify(&pins_from("1.99.0", "v1.31")),
            Verdict::MatrixStale
        );
    }

    #[test]
    fn classify_patch_level_does_not_move_the_window() {
        // Minor×minor schema: every 1.17.x patch resolves the same row.
        for patch in ["1.17.0", "1.17.2", "1.17.16", "1.17.999"] {
            assert!(
                matches!(
                    classify(&pins_from(patch, "v1.31")),
                    Verdict::Supported { .. }
                ),
                "{patch} should resolve the 1.17 row"
            );
        }
    }

    /// The whole point of the script: warn-by-default, fail on strict.
    #[test]
    fn exit_code_contract_default_vs_strict() {
        let ok = Verdict::Supported {
            supported: "1.29".to_string(),
        };
        let stale = Verdict::MatrixStale;
        let mismatch = Verdict::Mismatch {
            supported: "1.29".to_string(),
            lowest: "1.29".to_string(),
            below_window: false,
            source_url: "https://docs.cilium.io/en/v1.17/network/kubernetes/compatibility/",
        };

        // Default mode: everything exits 0 (preflight must not block).
        assert_eq!(verdict_exit_code(&ok, false), 0);
        assert_eq!(verdict_exit_code(&stale, false), 0);
        assert_eq!(verdict_exit_code(&mismatch, false), 0);

        // Strict mode: OK stays 0, both warn classes become 1.
        assert_eq!(verdict_exit_code(&ok, true), 0);
        assert_eq!(verdict_exit_code(&stale, true), EXIT_STRICT_MISMATCH);
        assert_eq!(verdict_exit_code(&mismatch, true), EXIT_STRICT_MISMATCH);
    }

    /// End-to-end over the pin pairs the bats suite pins, checking the
    /// exit code in BOTH modes for each.
    #[test]
    fn exit_codes_for_every_bats_pinned_pair() {
        // (cilium, k8s, default_code, strict_code)
        let cases: &[(&str, &str, i32, i32)] = &[
            ("1.16.5", "v1.29", 0, 0),
            ("1.16.5", "v1.31", 0, 1),
            ("1.17.2", "v1.31", 0, 0),
            ("1.99.0", "v1.31", 0, 1),
            ("1.16.5", "1.30", 0, 0),
            ("1.16.5", "v1.30.4", 0, 0),
            ("1.14.10", "v1.27", 0, 0),
            ("1.15.0", "v1.29", 0, 0),
            ("1.15.0", "v1.30", 0, 1),
            ("1.17.0", "v1.32", 0, 0),
            ("1.18.0", "v1.33", 0, 0),
        ];
        for (cilium, k8s, default_code, strict_code) in cases {
            let verdict = classify(&pins_from(cilium, k8s));
            assert_eq!(
                verdict_exit_code(&verdict, false),
                *default_code,
                "default-mode exit code for {cilium} + {k8s}"
            );
            assert_eq!(
                verdict_exit_code(&verdict, true),
                *strict_code,
                "strict-mode exit code for {cilium} + {k8s}"
            );
        }
    }

    /// The live repo pins MUST agree, or the default `make
    /// check-cilium-k8s-compat` is red. Uses the same table the command
    /// consults; the pins themselves are asserted by the caller-side
    /// integration test.
    #[test]
    fn live_pin_pair_is_supported() {
        // containers/k8s/k8s-init.sh: --version 1.17.16
        // containers/k8s/Containerfile: ARG K8S_VERSION=v1.31
        let v = classify(&pins_from("1.17.16", "v1.31"));
        assert!(
            matches!(v, Verdict::Supported { .. }),
            "committed pins must be compatible, got {v:?}"
        );
    }

    // ---- rendering -------------------------------------------------------

    #[test]
    fn render_supported_goes_to_stdout_with_bash_wording() {
        let pins = pins_from("1.16.5", "v1.29");
        let (out, err) = render_verdict(&pins, &classify(&pins));
        assert!(
            err.is_empty(),
            "OK verdict must not write to stderr: {err:?}"
        );
        assert_eq!(
            out,
            vec![
                "OK: Cilium 1.16.5 supports K8s 1.29 (supported: 1.27 1.28 1.29 1.30).".to_string()
            ]
        );
    }

    #[test]
    fn render_mismatch_goes_to_stderr_with_bash_wording() {
        let pins = pins_from("1.16.5", "v1.31");
        let (out, err) = render_verdict(&pins, &classify(&pins));
        assert!(
            out.is_empty(),
            "WARN verdict must not write to stdout: {out:?}"
        );
        let joined = err.join("\n");
        assert!(
            joined.contains("WARN: Cilium 1.16.5 does NOT list K8s 1.31 as a supported minor.")
        );
        assert!(joined.contains("Cilium 1.16.x supported K8s minors: 1.27 1.28 1.29 1.30"));
        // Above-window case takes the "bump Cilium" hint.
        assert!(joined.contains("docs/cilium-migration.md"));
        assert!(!joined.contains("BELOW"));
        assert!(joined.contains(
            "      Upstream matrix: https://docs.cilium.io/en/v1.16/network/kubernetes/compatibility/"
        ));
        assert!(joined.contains("docs/k8s-version-upgrade.md"));
    }

    #[test]
    fn render_below_window_mismatch_flips_the_hint() {
        let pins = pins_from("1.18.0", "v1.28");
        let (_out, err) = render_verdict(&pins, &classify(&pins));
        let joined = err.join("\n");
        assert!(joined.contains("K8s 1.28 is BELOW Cilium 1.18.x's window (lowest: 1.30)."));
        assert!(joined.contains("Downgrade Cilium to a minor that supports K8s 1.28"));
        assert!(
            !joined.contains("Bump Cilium first"),
            "below-window must NOT give the backwards advice: {joined}"
        );
    }

    #[test]
    fn render_matrix_stale_names_the_rust_module() {
        let pins = pins_from("1.99.0", "v1.31");
        let (out, err) = render_verdict(&pins, &classify(&pins));
        assert!(out.is_empty());
        let joined = err.join("\n");
        // Grep anchor preserved verbatim from the bash twin.
        assert!(joined.contains("not in the embedded compat matrix"));
        assert!(joined.contains("1.99"));
        assert!(joined.contains("K8s pin: 1.31"));
        // Deliberate divergence: point at the file that now holds the table.
        assert!(
            joined.contains("commands/preflight.rs"),
            "refresh hint must name the Rust module: {joined}"
        );
    }

    // ---- resolve_pins ----------------------------------------------------

    #[test]
    fn resolve_pins_prefers_overrides_over_files() {
        let pins = pins_from("1.17.0", "v1.31");
        assert_eq!(pins.cilium_version, "1.17.0");
        assert_eq!(pins.cilium_minor, "1.17");
        assert_eq!(pins.k8s_minor, "1.31");
        assert_eq!(pins.cilium_source, "--cilium");
        assert_eq!(pins.k8s_source, "--k8s");
    }

    #[test]
    fn resolve_pins_treats_empty_override_as_absent() {
        // Bash tests `[ -n "$CILIUM_OVERRIDE" ]`, so `--cilium=` falls
        // back to the file.
        let args = CiliumArgs {
            cilium: Some(String::new()),
            k8s: Some("v1.31".to_string()),
            strict: false,
            repo_root: None,
            dry_run: false,
        };
        let pins = resolve_pins(&args, Path::new("/repo"), |_| {
            Ok("  --version 1.18.0 \\\n".to_string())
        })
        .expect("file fallback resolves");
        assert_eq!(pins.cilium_version, "1.18.0");
        assert!(pins.cilium_source.ends_with(K8S_INIT_REL));
    }

    #[test]
    fn resolve_pins_reads_both_files_when_no_overrides() {
        let args = CiliumArgs {
            cilium: None,
            k8s: None,
            strict: false,
            repo_root: None,
            dry_run: false,
        };
        let pins = resolve_pins(&args, Path::new("/repo"), |p| {
            if p.ends_with(K8S_INIT_REL) {
                Ok("  --version 1.17.16 \\\n".to_string())
            } else {
                Ok("ARG K8S_VERSION=v1.31\n".to_string())
            }
        })
        .expect("both files resolve");
        assert_eq!(pins.cilium_version, "1.17.16");
        assert_eq!(pins.k8s_minor, "1.31");
    }

    #[test]
    fn resolve_pins_errors_when_k8s_init_unreadable() {
        let args = CiliumArgs {
            cilium: None,
            k8s: Some("v1.31".to_string()),
            strict: false,
            repo_root: None,
            dry_run: false,
        };
        let err = resolve_pins(&args, Path::new("/repo"), |_| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
        })
        .expect_err("unreadable k8s-init.sh must error");
        assert!(
            err.0.starts_with("ERROR: cannot read") && err.0.contains("Cilium pin"),
            "bash wording must be preserved: {}",
            err.0
        );
    }

    #[test]
    fn resolve_pins_errors_when_cilium_pin_not_extractable() {
        // Regression guard for the twin's `set -o pipefail` bug: an
        // empty grep match used to abort with a bare exit 1 BEFORE the
        // diagnostic. Reaching this ERROR line is the fix.
        let args = CiliumArgs {
            cilium: None,
            k8s: Some("v1.31".to_string()),
            strict: false,
            repo_root: None,
            dry_run: false,
        };
        let err = resolve_pins(&args, Path::new("/repo"), |_| {
            Ok("# no version pin here at all\n".to_string())
        })
        .expect_err("malformed k8s-init.sh must error");
        assert_eq!(
            err.0,
            "ERROR: could not extract Cilium --version from k8s-init.sh"
        );
    }

    #[test]
    fn resolve_pins_errors_when_containerfile_unreadable() {
        let args = CiliumArgs {
            cilium: Some("1.17.0".to_string()),
            k8s: None,
            strict: false,
            repo_root: None,
            dry_run: false,
        };
        let err = resolve_pins(&args, Path::new("/repo"), |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "nope",
            ))
        })
        .expect_err("unreadable Containerfile must error");
        assert!(
            err.0.starts_with("ERROR: cannot read") && err.0.contains("K8S_VERSION pin"),
            "bash wording must be preserved: {}",
            err.0
        );
    }

    #[test]
    fn resolve_pins_errors_when_k8s_pin_not_extractable() {
        let args = CiliumArgs {
            cilium: Some("1.17.0".to_string()),
            k8s: None,
            strict: false,
            repo_root: None,
            dry_run: false,
        };
        let err = resolve_pins(&args, Path::new("/repo"), |_| {
            Ok("FROM scratch\n".to_string())
        })
        .expect_err("malformed Containerfile must error");
        assert_eq!(
            err.0,
            "ERROR: could not extract K8S_VERSION from Containerfile"
        );
    }

    // ---- repo-root discovery --------------------------------------------

    #[test]
    fn find_repo_root_walks_up_to_the_containerfile() {
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("hbird-preflight-root-{pid}"));
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(root.join("containers/k8s")).expect("mkdir containers");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        std::fs::write(root.join(CONTAINERFILE_REL), "ARG K8S_VERSION=v1.31\n")
            .expect("write Containerfile");

        assert_eq!(find_repo_root(&nested).as_deref(), Some(root.as_path()));
        assert_eq!(find_repo_root(&root).as_deref(), Some(root.as_path()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn find_repo_root_returns_none_without_a_marker() {
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("hbird-preflight-noroot-{pid}"));
        std::fs::create_dir_all(&root).expect("mkdir");
        // /tmp itself must not contain containers/k8s/Containerfile for
        // this to be meaningful; assert the precondition rather than
        // silently passing.
        if find_repo_root(&root).is_some() {
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        assert!(find_repo_root(&root).is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    // ---- dry-run plan ----------------------------------------------------

    #[test]
    fn dry_run_plan_is_deterministic_and_reports_the_would_be_exit() {
        let pins = pins_from("1.16.5", "v1.31");
        let verdict = classify(&pins);
        let plan = render_dry_run_plan(&pins, &verdict, true);
        assert_eq!(plan.len(), 5);
        assert!(plan[0].contains("cilium pin: 1.16.5 (minor 1.16) from --cilium"));
        assert!(plan[1].contains("k8s pin:    1.31 from --k8s"));
        assert_eq!(plan[2], "[preflight-cilium] DRY-RUN strict: true");
        assert!(plan[3].contains("1.14 1.15 1.16 1.17 1.18"));
        assert!(plan[3].contains(MATRIX_REVIEWED_AS_OF));
        assert_eq!(
            plan[4],
            "[preflight-cilium] DRY-RUN would emit: WARN (mismatch) and exit 1"
        );
        // Re-rendering is byte-identical (no timestamps / no env reads).
        assert_eq!(plan, render_dry_run_plan(&pins, &verdict, true));
    }

    #[test]
    fn dry_run_plan_reports_exit_0_for_supported_pair() {
        let pins = pins_from("1.17.16", "v1.31");
        let verdict = classify(&pins);
        let plan = render_dry_run_plan(&pins, &verdict, true);
        assert_eq!(
            plan[4],
            "[preflight-cilium] DRY-RUN would emit: OK and exit 0"
        );
    }

    #[test]
    fn dry_run_plan_reports_matrix_stale() {
        let pins = pins_from("1.99.0", "v1.31");
        let verdict = classify(&pins);
        let plan = render_dry_run_plan(&pins, &verdict, false);
        assert_eq!(
            plan[4],
            "[preflight-cilium] DRY-RUN would emit: WARN (matrix stale) and exit 0"
        );
    }
}
