//! Typed parser for `cluster.local.conf` — the operator-edited file that
//! drives `scripts/deploy-cluster.sh`, `scripts/update-cluster.sh`, and
//! `scripts/destroy-cluster.sh`.
//!
//! # Why this crate exists
//!
//! The bash twin `source`s `cluster.local.conf` as a shell file and reads
//! shell variables. That's fine for bash — but the Rust rewrite (epic
//! [#279]) needs the same data as typed Rust values so subcommands can
//! validate, diff, and round-trip the config without shelling out.
//!
//! This crate intentionally accepts ONLY the declarative subset of bash
//! that the existing config files use:
//!
//! - `KEY=value`                              (bare scalar)
//! - `KEY="value with spaces"`                (double-quoted scalar)
//! - `KEY='value with spaces'`                (single-quoted scalar)
//! - `KEY=(a b "c d" e)`                      (array literal)
//! - `# comment lines` and blank lines        (ignored)
//!
//! Bash command substitution (`$(...)`), parameter expansion
//! (`${VAR:-default}`), conditionals, here-docs, function definitions —
//! none of those are evaluated. If the parser sees a line it doesn't
//! recognize, it returns [`Error::UnrecognizedLine`] pointing at the
//! offending line number. The operator can either rewrite the line into
//! the supported subset OR keep using the bash front-end until the Rust
//! subcommand catches up.
//!
//! # Three-state `WORKER_NAMES`
//!
//! `scripts/deploy-cluster.sh` distinguishes three cases (see the
//! `declare -p WORKER_NAMES` block):
//!
//! 1. **Unset** → legacy default of `(${CP_NAME}-w1 ${CP_NAME}-w2)`.
//! 2. **Explicit empty** (`WORKER_NAMES=()`) → CP-only deploy.
//! 3. **Populated** → use as-is.
//!
//! This crate exposes the raw three-state distinction as
//! [`ClusterConfig::worker_names`] (an `Option<Vec<String>>`) plus a
//! convenience [`ClusterConfig::resolved_worker_names`] that applies the
//! legacy default. Callers that care about CP-only intent must use the
//! raw `Option`; callers that just want a list can use the resolver.
//!
//! [#279]: https://github.com/aatchison/hummingbird-k8s/issues/279

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

mod error;
mod parser;

pub use error::{Error, Warning};

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Typed view of a parsed `cluster.local.conf`.
///
/// Field names match the bash variable names verbatim (lowercased) so
/// `grep`-ability between the bash twin and the Rust API stays trivial.
/// Defaults match `scripts/deploy-cluster.sh`'s `${VAR:=default}` lines.
///
/// Fields with no default in the bash twin (`KVM_HOST`, the
/// `BOOTC_UPDATE_*` overrides, `CP_IP`, `WORKER_IPS`) are `Option<...>`
/// so the caller can distinguish "operator left it blank" from "operator
/// set it to the empty string". The bash twin collapses the two via
/// `${VAR:=}` — Rust callers get the distinction back.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClusterConfig {
    // ---- Required scalars --------------------------------------------------
    /// Libvirt domain name of the control-plane VM. REQUIRED.
    pub cp_name: String,
    /// Path to the SSH public key baked into the bib image's default user
    /// AND embedded in each VM's cloud-init seed. REQUIRED.
    pub ssh_pubkey_file: String,

    // ---- Optional scalars with bash-twin defaults --------------------------
    /// KVM host alias (SSH-reachable from the kubectl client). Unset when
    /// the operator runs kubectl from the KVM host directly.
    pub kvm_host: Option<String>,
    /// Image source: `"ghcr"` (default, pull published images) or
    /// `"local"` (build fresh from `containers/k8s` + `containers/k8s-worker`).
    pub image_source: String,
    /// GHCR tag to pull (when `image_source == "ghcr"`).
    pub ghcr_tag: String,
    /// Whether cloud-init is enabled. The deploy script requires `1`.
    pub enable_cloud_init: u32,
    /// Emit a cloud-init runcmd that enables the semver-aware bootc updater
    /// on the CP. See `cluster.example.conf` for the full rationale.
    pub auto_update_cp: bool,
    /// Emit `bootc switch ghcr.io/...` runcmds so the auto-update timer has
    /// a remote ref to pull from.
    pub switch_to_ghcr: bool,
    /// CP memory in MiB.
    pub cp_memory: u32,
    /// CP vCPU count.
    pub cp_vcpus: u32,
    /// Worker memory in MiB (per worker).
    pub worker_memory: u32,
    /// Worker vCPU count (per worker).
    pub worker_vcpus: u32,
    /// Libvirt storage pool directory.
    pub pool_dir: String,
    /// Whether `scripts/verify-app-deploy.sh` runs after the cluster
    /// reaches Ready.
    pub run_verify: bool,
    /// Optional `OnCalendar=` override for the bootc-semver-update timer.
    /// `None` (not present in config) and `Some("")` (explicit blank) are
    /// both treated as "use the image default" by the bash twin; we
    /// preserve the distinction in case future tooling needs it.
    pub bootc_update_schedule: Option<String>,
    /// Optional override for the CP bootc-semver-update repo.
    pub bootc_update_repo_k8s: Option<String>,
    /// Optional override for the worker bootc-semver-update repo.
    pub bootc_update_repo_worker: Option<String>,

    // ---- Arrays + IP overrides ---------------------------------------------
    /// Worker VM names. `None` = operator never set the variable (legacy
    /// 2-worker default applies); `Some(vec![])` = explicit CP-only;
    /// `Some(vec!["w1", "w2"])` = use as-is. See the module docs.
    pub worker_names: Option<Vec<String>>,
    /// Optional static CP IP override. Bypasses `virsh domifaddr` resolution.
    pub cp_ip: Option<String>,
    /// Optional static worker IP overrides. When set, must be parallel to
    /// `resolved_worker_names()`.
    pub worker_ips: Option<Vec<String>>,

    // ---- Non-fatal parser diagnostics --------------------------------------
    /// Warnings emitted while parsing — keys present in the input that
    /// no `ClusterConfig` field consumed. Empty on a clean parse.
    ///
    /// Bash silently ignores unknown `KEY=value` lines (`source` accepts
    /// them), so the Rust parser stays a warning rather than an error to
    /// preserve the bash-twin parity contract. Operator tooling that
    /// wants fail-on-typo behavior can opt in:
    ///
    /// ```no_run
    /// let cfg = hbird_config::parse("cluster.local.conf")?;
    /// if !cfg.warnings.is_empty() {
    ///     for w in &cfg.warnings { eprintln!("warning: {w}"); }
    ///     std::process::exit(1);
    /// }
    /// # Ok::<(), hbird_config::Error>(())
    /// ```
    ///
    /// Tracking: [#316](https://github.com/aatchison/hummingbird-k8s/issues/316).
    pub warnings: Vec<Warning>,
}

impl ClusterConfig {
    /// Apply the bash twin's legacy 2-worker default for an unset
    /// `WORKER_NAMES`. Returns the explicit value when set (including the
    /// empty vec for CP-only).
    ///
    /// Mirrors `scripts/deploy-cluster.sh`:
    ///
    /// ```text
    /// if ! declare -p WORKER_NAMES >/dev/null 2>&1; then
    ///   WORKER_NAMES=("${CP_NAME}-w1" "${CP_NAME}-w2")
    /// fi
    /// ```
    #[must_use]
    pub fn resolved_worker_names(&self) -> Vec<String> {
        match &self.worker_names {
            Some(names) => names.clone(),
            None => vec![
                format!("{}-w1", self.cp_name),
                format!("{}-w2", self.cp_name),
            ],
        }
    }
}

/// Parse a `cluster.local.conf` file at `path`.
///
/// Reads the file into memory (these files are small — kilobytes) and
/// delegates to [`parse_str`]. Required-field errors carry the supplied
/// `path` so the diagnostic matches the bash twin's
/// `${VAR:?... is required in $CONFIG_PATH}` shape.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file can't be read, plus any parse or
/// validation error from [`parse_str`].
pub fn parse(path: impl AsRef<Path>) -> Result<ClusterConfig> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path).map_err(|source| Error::Io {
        path: PathBuf::from(path),
        source,
    })?;
    parser::parse_with_source(&raw, &path.display().to_string())
}

/// Parse a `cluster.local.conf` from an in-memory string.
///
/// Useful for tests and for callers that have already loaded the file
/// (e.g. from stdin or a network source). Required-field errors will
/// report the source path as `<inline>`.
///
/// # Errors
///
/// Returns a parser or validation error if the input doesn't match the
/// supported subset of bash. See [`Error`] for the full variant list.
pub fn parse_str(input: &str) -> Result<ClusterConfig> {
    parser::parse_with_source(input, "<inline>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_required_only() {
        let cfg = parse_str(
            r#"
            # comment
            CP_NAME=hbird-cp1
            SSH_PUBKEY_FILE=/home/op/.ssh/id_ed25519.pub
            "#,
        )
        .expect("minimal config parses");
        assert_eq!(cfg.cp_name, "hbird-cp1");
        assert_eq!(cfg.ssh_pubkey_file, "/home/op/.ssh/id_ed25519.pub");
        // Defaults applied:
        assert_eq!(cfg.image_source, "ghcr");
        assert_eq!(cfg.ghcr_tag, "latest");
        assert_eq!(cfg.enable_cloud_init, 0);
        assert!(cfg.auto_update_cp);
        assert!(cfg.switch_to_ghcr);
        assert_eq!(cfg.cp_memory, 8192);
        assert_eq!(cfg.cp_vcpus, 4);
        assert_eq!(cfg.worker_memory, 4096);
        assert_eq!(cfg.worker_vcpus, 2);
        assert_eq!(cfg.pool_dir, "/var/lib/libvirt/images");
        assert!(!cfg.run_verify);
        assert_eq!(cfg.kvm_host, None);
        // Three-state WORKER_NAMES: unset → None → legacy default applies.
        assert_eq!(cfg.worker_names, None);
        assert_eq!(
            cfg.resolved_worker_names(),
            vec!["hbird-cp1-w1".to_string(), "hbird-cp1-w2".to_string()]
        );
    }

    #[test]
    fn cp_name_required() {
        let err = parse_str("SSH_PUBKEY_FILE=/x").expect_err("CP_NAME missing must fail");
        match err {
            Error::MissingRequired { field, .. } => assert_eq!(field, "CP_NAME"),
            other => panic!("expected MissingRequired CP_NAME, got {other:?}"),
        }
    }

    #[test]
    fn ssh_pubkey_required() {
        let err = parse_str("CP_NAME=foo").expect_err("SSH_PUBKEY_FILE missing must fail");
        match err {
            Error::MissingRequired { field, .. } => {
                assert_eq!(field, "SSH_PUBKEY_FILE");
            }
            other => panic!("expected MissingRequired SSH_PUBKEY_FILE, got {other:?}"),
        }
    }

    #[test]
    fn empty_array_means_cp_only() {
        let cfg = parse_str(
            r"
            CP_NAME=hbird-cp1
            SSH_PUBKEY_FILE=/k
            WORKER_NAMES=()
            ",
        )
        .expect("CP-only config parses");
        // Explicit empty: Some(vec![]) — distinct from None.
        assert_eq!(cfg.worker_names, Some(vec![]));
        assert_eq!(cfg.resolved_worker_names(), Vec::<String>::new());
    }

    #[test]
    fn populated_array() {
        let cfg = parse_str(
            r"
            CP_NAME=hbird-cp1
            SSH_PUBKEY_FILE=/k
            WORKER_NAMES=(hbird-w1 hbird-w2 hbird-w3)
            ",
        )
        .expect("populated array parses");
        assert_eq!(
            cfg.worker_names,
            Some(vec![
                "hbird-w1".to_string(),
                "hbird-w2".to_string(),
                "hbird-w3".to_string(),
            ])
        );
    }

    #[test]
    fn array_with_quoted_segments() {
        let cfg = parse_str(
            r#"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            WORKER_NAMES=("worker one" "worker two" plain)
            "#,
        )
        .expect("quoted array elements parse");
        assert_eq!(
            cfg.worker_names,
            Some(vec![
                "worker one".to_string(),
                "worker two".to_string(),
                "plain".to_string(),
            ])
        );
    }

    #[test]
    fn double_quoted_scalar() {
        let cfg = parse_str(
            r#"
            CP_NAME="hbird cp"
            SSH_PUBKEY_FILE="/path with spaces/key.pub"
            "#,
        )
        .expect("quoted scalars parse");
        assert_eq!(cfg.cp_name, "hbird cp");
        assert_eq!(cfg.ssh_pubkey_file, "/path with spaces/key.pub");
    }

    #[test]
    fn single_quoted_scalar() {
        let cfg = parse_str(
            r"
            CP_NAME='hbird-cp1'
            SSH_PUBKEY_FILE='/k.pub'
            ",
        )
        .expect("single-quoted scalars parse");
        assert_eq!(cfg.cp_name, "hbird-cp1");
        assert_eq!(cfg.ssh_pubkey_file, "/k.pub");
    }

    #[test]
    fn trailing_inline_comment_stripped_from_bare_scalar() {
        // Bash treats `#` as a comment only when preceded by whitespace.
        // We follow the same rule so values like `tag#v1` parse as-is.
        let cfg = parse_str(
            r"
            CP_NAME=cp # inline comment
            SSH_PUBKEY_FILE=/k
            ",
        )
        .expect("inline trailing comment strips");
        assert_eq!(cfg.cp_name, "cp");
    }

    #[test]
    fn hash_inside_bare_scalar_preserved() {
        // `tag#v1` — no whitespace before `#` — keeps the `#` per bash.
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            GHCR_TAG=tag#v1
            ",
        )
        .expect("embedded # in bare scalar preserved");
        assert_eq!(cfg.ghcr_tag, "tag#v1");
    }

    #[test]
    fn boolean_truthy_variants() {
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            AUTO_UPDATE_CP=true
            SWITCH_TO_GHCR=false
            RUN_VERIFY=1
            ",
        )
        .expect("boolean variants parse");
        assert!(cfg.auto_update_cp);
        assert!(!cfg.switch_to_ghcr);
        assert!(cfg.run_verify);
    }

    #[test]
    fn integer_fields() {
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            CP_MEMORY=16384
            CP_VCPUS=8
            WORKER_MEMORY=8192
            WORKER_VCPUS=4
            ENABLE_CLOUD_INIT=1
            ",
        )
        .expect("integer fields parse");
        assert_eq!(cfg.cp_memory, 16384);
        assert_eq!(cfg.cp_vcpus, 8);
        assert_eq!(cfg.worker_memory, 8192);
        assert_eq!(cfg.worker_vcpus, 4);
        assert_eq!(cfg.enable_cloud_init, 1);
    }

    #[test]
    fn invalid_integer_surfaces_field_and_value() {
        let err = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            CP_MEMORY=notanumber
            ",
        )
        .expect_err("invalid integer must fail");
        match err {
            Error::InvalidInteger { field, raw, .. } => {
                assert_eq!(field, "CP_MEMORY");
                assert_eq!(raw, "notanumber");
            }
            other => panic!("expected InvalidInteger, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_line_reports_line_number() {
        let err = parse_str("CP_NAME=cp\nSSH_PUBKEY_FILE=/k\nfunction foo() { :; }\n")
            .expect_err("function defs are not supported");
        match err {
            Error::UnrecognizedLine { line_no, raw } => {
                assert_eq!(line_no, 3);
                assert!(raw.contains("function foo"));
            }
            other => panic!("expected UnrecognizedLine, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_array() {
        let err = parse_str("CP_NAME=cp\nSSH_PUBKEY_FILE=/k\nWORKER_NAMES=(a b\n")
            .expect_err("missing close paren must fail");
        match err {
            Error::UnterminatedArray { key, line_no } => {
                assert_eq!(key, "WORKER_NAMES");
                assert_eq!(line_no, 3);
            }
            other => panic!("expected UnterminatedArray, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_quote() {
        let err = parse_str(
            r#"CP_NAME="open
"#,
        )
        .expect_err("missing close quote must fail");
        match err {
            Error::UnterminatedQuote { line_no, .. } => assert_eq!(line_no, 1),
            other => panic!("expected UnterminatedQuote, got {other:?}"),
        }
    }

    #[test]
    fn multi_line_array() {
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            WORKER_NAMES=(
              w1
              w2
              w3
            )
            ",
        )
        .expect("multi-line array parses");
        assert_eq!(
            cfg.worker_names,
            Some(vec!["w1".to_string(), "w2".to_string(), "w3".to_string(),])
        );
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let cfg = parse_str(
            r"

            # a comment

              # indented comment
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k

            ",
        )
        .expect("comments + blanks ignored");
        assert_eq!(cfg.cp_name, "cp");
    }

    #[test]
    fn last_assignment_wins() {
        // Bash assigns the latest value. Mirror that — operators editing
        // cluster.local.conf occasionally leave a default above an override.
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            CP_MEMORY=4096
            CP_MEMORY=16384
            ",
        )
        .expect("last assignment wins");
        assert_eq!(cfg.cp_memory, 16384);
    }

    #[test]
    fn worker_ips_parses_as_array() {
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            WORKER_NAMES=(w1 w2)
            WORKER_IPS=(192.168.122.11 192.168.122.12)
            CP_IP=192.168.122.10
            ",
        )
        .expect("WORKER_IPS + CP_IP parse");
        assert_eq!(cfg.cp_ip.as_deref(), Some("192.168.122.10"));
        assert_eq!(
            cfg.worker_ips,
            Some(vec![
                "192.168.122.11".to_string(),
                "192.168.122.12".to_string(),
            ])
        );
    }

    #[test]
    fn kvm_host_optional_blank_stays_blank() {
        // KVM_HOST=$VAR style isn't supported; bare blank is.
        let cfg = parse_str(
            r#"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            KVM_HOST=""
            "#,
        )
        .expect("empty KVM_HOST parses");
        // Explicit empty string survives — bash twin collapses to unset via
        // `${KVM_HOST:=}`, but we keep the distinction so callers can see
        // "operator typed `KVM_HOST=`" vs. "operator omitted the line".
        assert_eq!(cfg.kvm_host.as_deref(), Some(""));
    }

    // ---- PR #315 round-2 review additions ---------------------------------

    /// PR #315 round-2 review Lens 2 HIGH: AUTO_UPDATE_CP and
    /// SWITCH_TO_GHCR are strict-validated by the bash twin
    /// (`case "$X" in true|false) ;; *) fail`). Rust must mirror that
    /// failure rather than silently fall back to the default.
    #[test]
    fn strict_bool_rejects_non_literal_for_auto_update_cp() {
        let err = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            AUTO_UPDATE_CP=truue
            ",
        )
        .expect_err("typo'd boolean should error, not silently default");
        match err {
            Error::InvalidBool {
                field,
                line_no,
                raw,
            } => {
                assert_eq!(field, "AUTO_UPDATE_CP");
                assert!(line_no > 0);
                assert_eq!(raw, "truue");
            }
            other => panic!("expected InvalidBool, got {other:?}"),
        }
    }

    #[test]
    fn strict_bool_rejects_truthy_synonyms_for_switch_to_ghcr() {
        // The lenient parser would accept 1/yes/on/0/no/off; bash hard-
        // rejects everything but literal true/false for this field.
        for bad in ["1", "yes", "on", "0", "no", "off", "TRUE", "True"] {
            let conf = format!("CP_NAME=cp\nSSH_PUBKEY_FILE=/k\nSWITCH_TO_GHCR={bad}\n");
            let err = parse_str(&conf).expect_err(&format!(
                "expected InvalidBool for SWITCH_TO_GHCR={bad}, got Ok"
            ));
            assert!(
                matches!(
                    err,
                    Error::InvalidBool {
                        field: "SWITCH_TO_GHCR",
                        ..
                    }
                ),
                "wrong error variant for SWITCH_TO_GHCR={bad}: {err:?}"
            );
        }
    }

    /// PR #315 round-2 review Lens 2 HIGH: strict bool with empty
    /// string must fall through to the caller's default, NOT to false
    /// (the pre-round-2 lenient code mapped "" -> false unconditionally).
    #[test]
    fn strict_bool_empty_uses_default_not_false() {
        let cfg = parse_str(
            r#"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            AUTO_UPDATE_CP=""
            "#,
        )
        .expect("empty string falls through to default");
        // Default for AUTO_UPDATE_CP is true (mirrors bash `:=true`).
        assert!(
            cfg.auto_update_cp,
            "AUTO_UPDATE_CP=\"\" must default to true"
        );
    }

    /// PR #315 round-2 review Lens 2 HIGH: lenient bool with empty
    /// string must also fall through to default (same bug class).
    #[test]
    fn lenient_bool_empty_uses_default_not_false() {
        let cfg = parse_str(
            r#"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            RUN_VERIFY=""
            "#,
        )
        .expect("empty RUN_VERIFY falls through to default");
        // Default for RUN_VERIFY is false; the contract is "empty == default",
        // not "empty == false". With default=false, observed=false, the test
        // would tautologically pass — flip the default expectation by also
        // asserting unset-RUN_VERIFY behaves the same.
        let cfg2 = parse_str("CP_NAME=cp\nSSH_PUBKEY_FILE=/k\n").expect("unset RUN_VERIFY parses");
        assert_eq!(cfg.run_verify, cfg2.run_verify);
    }

    /// PR #315 round-2 review Lens 2 HIGH: trailing content after a
    /// closing quote (e.g. `CP_NAME="hbird" garbage`) used to silently
    /// drop everything past the quote, masking missing-`#` typos.
    /// Round-2 errors instead.
    #[test]
    fn trailing_garbage_after_double_quote_errors() {
        let err = parse_str(
            r#"
            CP_NAME="hbird" garbage
            SSH_PUBKEY_FILE=/k
            "#,
        )
        .expect_err("trailing content after close quote should error");
        match err {
            Error::TrailingContent { line_no, raw } => {
                assert!(line_no > 0);
                assert!(
                    raw.contains("garbage"),
                    "raw should include the offending text, got: {raw:?}"
                );
            }
            other => panic!("expected TrailingContent, got {other:?}"),
        }
    }

    #[test]
    fn trailing_garbage_after_single_quote_errors() {
        let err = parse_str("CP_NAME='hbird' garbage\nSSH_PUBKEY_FILE=/k\n")
            .expect_err("trailing content after close quote should error (single quoted)");
        assert!(matches!(err, Error::TrailingContent { .. }));
    }

    #[test]
    fn trailing_comment_after_close_quote_is_allowed() {
        // A `#` comment after the close quote IS legitimate bash;
        // verify the round-2 check doesn't false-positive on it.
        let cfg = parse_str(
            r#"
            CP_NAME="hbird"  # this is fine
            SSH_PUBKEY_FILE=/k
            "#,
        )
        .expect("trailing # comment is legal");
        assert_eq!(cfg.cp_name, "hbird");
    }

    #[test]
    fn trailing_whitespace_after_close_quote_is_allowed() {
        let cfg = parse_str("CP_NAME=\"hbird\"   \nSSH_PUBKEY_FILE=/k\n")
            .expect("trailing whitespace after close quote is legal");
        assert_eq!(cfg.cp_name, "hbird");
    }

    // ---- #316: type mismatches surface as errors --------------------------

    /// `WORKER_NAMES=foo` — scalar where an array is expected. Pre-#316
    /// silently fell through to `None` and applied the legacy 2-worker
    /// default, building a wrong cluster from a typo. Now errors.
    #[test]
    fn type_mismatch_scalar_for_array_field_errors() {
        let err = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            WORKER_NAMES=foo
            ",
        )
        .expect_err("scalar assigned to array field must error");
        match err {
            Error::TypeMismatch {
                field,
                expected,
                got,
                line_no,
            } => {
                assert_eq!(field, "WORKER_NAMES");
                assert_eq!(expected, "array");
                assert_eq!(got, "scalar");
                assert!(line_no > 0, "line_no must be 1-based, got {line_no}");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// `WORKER_IPS=192.168.1.10` — same pattern as WORKER_NAMES, but for
    /// the optional array. Ensures the helper covers both array fields
    /// rather than just the first one we wired.
    #[test]
    fn type_mismatch_scalar_for_worker_ips_errors() {
        let err = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            WORKER_IPS=192.168.1.10
            ",
        )
        .expect_err("scalar assigned to WORKER_IPS must error");
        assert!(matches!(
            err,
            Error::TypeMismatch {
                field: "WORKER_IPS",
                expected: "array",
                got: "scalar",
                ..
            }
        ));
    }

    /// `CP_MEMORY=(1 2)` — array where a scalar is expected. Pre-#316
    /// silently fell through to the default 8192, hiding the typo.
    #[test]
    fn type_mismatch_array_for_scalar_int_field_errors() {
        let err = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            CP_MEMORY=(1 2)
            ",
        )
        .expect_err("array assigned to scalar int field must error");
        match err {
            Error::TypeMismatch {
                field,
                expected,
                got,
                ..
            } => {
                assert_eq!(field, "CP_MEMORY");
                assert_eq!(expected, "scalar");
                assert_eq!(got, "array");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// Required scalar field (`CP_NAME`) assigned an array must error
    /// with `TypeMismatch`, NOT `MissingRequired` — the operator clearly
    /// intended to set the value, just got the shape wrong, and the
    /// "field is missing" diagnostic would mislead them.
    #[test]
    fn type_mismatch_array_for_required_scalar_errors() {
        let err = parse_str("CP_NAME=(a b)\nSSH_PUBKEY_FILE=/k\n")
            .expect_err("array assigned to CP_NAME must error");
        assert!(
            matches!(
                err,
                Error::TypeMismatch {
                    field: "CP_NAME",
                    expected: "scalar",
                    got: "array",
                    ..
                }
            ),
            "expected TypeMismatch CP_NAME, got {err:?}"
        );
    }

    /// Optional scalar field assigned an array must error (not silently
    /// drop the value). `CP_IP` is the natural target since the bash
    /// twin's `virsh net-update` path would also get confused.
    #[test]
    fn type_mismatch_array_for_optional_scalar_errors() {
        let err = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            CP_IP=(192.168.1.10 192.168.1.11)
            ",
        )
        .expect_err("array assigned to CP_IP must error");
        assert!(matches!(
            err,
            Error::TypeMismatch {
                field: "CP_IP",
                expected: "scalar",
                got: "array",
                ..
            }
        ));
    }

    /// Strict-bool field assigned an array must error with
    /// `TypeMismatch`, not `InvalidBool` (the shape is wrong, not the
    /// value — keeping the diagnostics specific helps the operator).
    #[test]
    fn type_mismatch_array_for_strict_bool_errors() {
        let err = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            AUTO_UPDATE_CP=(true)
            ",
        )
        .expect_err("array assigned to AUTO_UPDATE_CP must error");
        assert!(matches!(
            err,
            Error::TypeMismatch {
                field: "AUTO_UPDATE_CP",
                expected: "scalar",
                got: "array",
                ..
            }
        ));
    }

    /// Lenient-bool field (`RUN_VERIFY`) assigned an array must also
    /// error, not silently fall through to the default. Same bug class
    /// as the strict-bool variant; covered separately because the
    /// lenient parser path is structurally different.
    #[test]
    fn type_mismatch_array_for_lenient_bool_errors() {
        let err = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            RUN_VERIFY=(1)
            ",
        )
        .expect_err("array assigned to RUN_VERIFY must error");
        assert!(matches!(
            err,
            Error::TypeMismatch {
                field: "RUN_VERIFY",
                expected: "scalar",
                got: "array",
                ..
            }
        ));
    }

    // ---- #316: unknown keys surface as warnings ---------------------------

    /// A typo like `WROKER_NAMES=` should not be silently ignored.
    /// Bash's `source` would happily accept it (and never read it back),
    /// but the Rust parser collects it as `Warning::UnknownKey` so
    /// operator tooling can flag it near the source line.
    #[test]
    fn unknown_key_surfaces_as_warning() {
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            WROKER_NAMES=(w1 w2)
            ",
        )
        .expect("parse succeeds — unknown keys are warnings, not errors");
        // Config still builds with the legacy 2-worker default — the
        // typo'd assignment doesn't influence the cluster shape.
        assert_eq!(cfg.worker_names, None);
        // But the warning IS surfaced.
        assert_eq!(cfg.warnings.len(), 1);
        match &cfg.warnings[0] {
            Warning::UnknownKey { key, line_no } => {
                assert_eq!(key, "WROKER_NAMES");
                assert!(*line_no > 0, "line_no must be 1-based");
            }
        }
    }

    /// Multiple unknown keys must all surface, in source-order (by
    /// line_no) so operator tooling can print them top-to-bottom.
    #[test]
    fn multiple_unknown_keys_surface_in_line_order() {
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            ZZZ_UNKNOWN_A=1
            AAA_UNKNOWN_B=2
            MIDDLE_UNKNOWN_C=3
            ",
        )
        .expect("parse succeeds with multiple unknowns");
        let keys: Vec<&str> = cfg
            .warnings
            .iter()
            .map(|w| match w {
                Warning::UnknownKey { key, .. } => key.as_str(),
            })
            .collect();
        assert_eq!(
            keys,
            vec!["ZZZ_UNKNOWN_A", "AAA_UNKNOWN_B", "MIDDLE_UNKNOWN_C"]
        );
    }

    /// A clean config (no unknown keys) must produce an empty `warnings`
    /// vec — operator tooling that checks `warnings.is_empty()` as a
    /// gate relies on this invariant.
    #[test]
    fn clean_config_has_no_warnings() {
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            WORKER_NAMES=(w1 w2)
            CP_MEMORY=8192
            AUTO_UPDATE_CP=true
            ",
        )
        .expect("clean config parses");
        assert!(
            cfg.warnings.is_empty(),
            "expected zero warnings, got: {:?}",
            cfg.warnings
        );
    }

    /// Unknown keys with array values must still warn (not error). The
    /// shape of the offending value is irrelevant — it's unknown either
    /// way, so the diagnostic is the same.
    #[test]
    fn unknown_key_with_array_value_still_warns() {
        let cfg = parse_str(
            r"
            CP_NAME=cp
            SSH_PUBKEY_FILE=/k
            UNKNOWN_ARRAY=(a b c)
            ",
        )
        .expect("unknown key with array value parses (warning only)");
        assert_eq!(cfg.warnings.len(), 1);
        assert!(matches!(
            &cfg.warnings[0],
            Warning::UnknownKey { key, .. } if key == "UNKNOWN_ARRAY"
        ));
    }
}
