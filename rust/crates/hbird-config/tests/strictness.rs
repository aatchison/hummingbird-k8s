//! Integration coverage for #316 — the parser must surface
//! type-mismatch (hard error) and unknown-key (warning) cases that
//! the pre-#316 parser silently dropped. Unit tests in
//! `src/lib.rs::tests` exercise the same paths; this file double-locks
//! the *public API* shape (`Error::TypeMismatch`, `Warning::UnknownKey`,
//! `ClusterConfig.warnings`) so an accidental rename or visibility
//! tighten breaks here too.

use hbird_config::{Error, Warning, parse_str};

/// The classic typo from the issue: `WORKER_NAMES=foo` instead of
/// `WORKER_NAMES=(foo)`. Pre-#316 the result was a config built with
/// the legacy 2-worker default — operator typo turning into a wrong
/// cluster. Now it's a hard error.
#[test]
fn worker_names_scalar_typo_errors() {
    let err = parse_str(
        r"
        CP_NAME=cp
        SSH_PUBKEY_FILE=/k
        WORKER_NAMES=foo
        ",
    )
    .expect_err("WORKER_NAMES=foo (no parens) must surface as TypeMismatch");
    assert!(matches!(
        err,
        Error::TypeMismatch {
            field: "WORKER_NAMES",
            expected: "array",
            got: "scalar",
            ..
        }
    ));
}

/// The other classic from the issue: `CP_MEMORY=(1 2)` instead of
/// `CP_MEMORY=1024`. Pre-#316 the result was the default 8192.
#[test]
fn cp_memory_array_typo_errors() {
    let err = parse_str(
        r"
        CP_NAME=cp
        SSH_PUBKEY_FILE=/k
        CP_MEMORY=(1 2)
        ",
    )
    .expect_err("CP_MEMORY=(...) must surface as TypeMismatch");
    assert!(matches!(
        err,
        Error::TypeMismatch {
            field: "CP_MEMORY",
            expected: "scalar",
            got: "array",
            ..
        }
    ));
}

/// Operator misspells `WORKER_NAMES` as `WROKER_NAMES`. Pre-#316 the
/// typo was silently ignored and the legacy 2-worker default applied.
/// Now the typo surfaces as a `Warning::UnknownKey` while the parse
/// still succeeds (matches bash's silence-on-unknown-key contract).
#[test]
fn unknown_key_returns_warning_not_error() {
    let cfg = parse_str(
        r"
        CP_NAME=cp
        SSH_PUBKEY_FILE=/k
        WROKER_NAMES=(w1 w2)
        ",
    )
    .expect("unknown key must be a warning, not an error");
    assert_eq!(cfg.warnings.len(), 1, "expected exactly one warning");
    // `Warning` is #[non_exhaustive] (future variants are valid evolution
    // without a SemVer break), so external matches need the wildcard arm.
    match &cfg.warnings[0] {
        Warning::UnknownKey { key, line_no } => {
            assert_eq!(key, "WROKER_NAMES");
            assert!(*line_no > 0);
        }
        other => panic!("expected UnknownKey, got {other:?}"),
    }
}

/// Operator-facing tooling that wants fail-on-typo behavior can opt in
/// by treating a non-empty `warnings` vec as an error. Documents the
/// pattern the crate-level docs recommend — and double-checks the
/// `Warning` enum implements `Display` (via thiserror) so eprintln!
/// works directly.
#[test]
fn warnings_display_renders_for_operator_output() {
    let cfg =
        parse_str("CP_NAME=cp\nSSH_PUBKEY_FILE=/k\nFOO_TYPO=1\n").expect("warning, not error");
    let rendered: Vec<String> = cfg.warnings.iter().map(|w| w.to_string()).collect();
    assert_eq!(rendered.len(), 1);
    assert!(
        rendered[0].contains("FOO_TYPO"),
        "rendered warning must mention the offending key: {rendered:?}"
    );
}
