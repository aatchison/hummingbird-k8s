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

/// Pin the exact `line_no` for `Error::TypeMismatch` rather than just
/// asserting it's non-zero. The other strictness tests use `..` in their
/// `matches!` macro to be tolerant of test-input rewrites; this one
/// trades that tolerance for the certainty that line counting hasn't
/// drifted (off-by-one in the parser would silently pass otherwise).
#[test]
fn type_mismatch_reports_exact_line_no() {
    // Build input so the offending line sits on a KNOWN row. Layout:
    //   line 1: blank (leading \n)
    //   line 2: CP_NAME=cp
    //   line 3: SSH_PUBKEY_FILE=/k
    //   line 4: WORKER_NAMES=foo   <-- the mismatch
    let input = "\nCP_NAME=cp\nSSH_PUBKEY_FILE=/k\nWORKER_NAMES=foo\n";
    let err = parse_str(input).expect_err("scalar-for-array must error");
    match err {
        Error::TypeMismatch { field, line_no, .. } => {
            assert_eq!(field, "WORKER_NAMES");
            assert_eq!(line_no, 4, "expected the mismatch on line 4 exactly");
        }
        other => panic!("expected TypeMismatch, got {other:?}"),
    }
}

/// Companion to `type_mismatch_reports_exact_line_no` — same pinning,
/// but for `Warning::UnknownKey` so an off-by-one in the warning path
/// (which goes through a different code path: HashMap collection +
/// post-build sort) also surfaces.
#[test]
fn unknown_key_reports_exact_line_no() {
    // Layout:
    //   line 1: blank (leading \n)
    //   line 2: CP_NAME=cp
    //   line 3: SSH_PUBKEY_FILE=/k
    //   line 4: WROKER_NAMES=foo   <-- the typo
    let input = "\nCP_NAME=cp\nSSH_PUBKEY_FILE=/k\nWROKER_NAMES=foo\n";
    let cfg = parse_str(input).expect("unknown key warns, not errors");
    assert_eq!(cfg.warnings.len(), 1);
    match &cfg.warnings[0] {
        Warning::UnknownKey { key, line_no } => {
            assert_eq!(key, "WROKER_NAMES");
            assert_eq!(*line_no, 4, "expected the typo on line 4 exactly");
        }
        other => panic!("expected UnknownKey, got {other:?}"),
    }
}

/// A duplicate unknown-key assignment (`UNKNOWN_FOO=first` then
/// `UNKNOWN_FOO=second`) must yield exactly ONE warning, with the
/// `line_no` of the LAST occurrence. Locks the current HashMap-insert-
/// overwrite semantics — same as `last_assignment_wins` for known
/// fields, applied to unknown ones. If the parser ever switches to
/// collecting both occurrences, this test forces the change to be
/// deliberate (and the doc on `ClusterConfig.warnings` to be updated).
#[test]
fn duplicate_unknown_key_collapses_to_last_occurrence() {
    // Layout:
    //   line 1: blank
    //   line 2: CP_NAME=cp
    //   line 3: SSH_PUBKEY_FILE=/k
    //   line 4: UNKNOWN_FOO=first
    //   line 5: UNKNOWN_FOO=second
    let input = "\nCP_NAME=cp\nSSH_PUBKEY_FILE=/k\nUNKNOWN_FOO=first\nUNKNOWN_FOO=second\n";
    let cfg = parse_str(input).expect("duplicate unknown key parses as one warning");
    assert_eq!(
        cfg.warnings.len(),
        1,
        "duplicate unknown key must collapse to one warning, got: {:?}",
        cfg.warnings
    );
    match &cfg.warnings[0] {
        Warning::UnknownKey { key, line_no } => {
            assert_eq!(key, "UNKNOWN_FOO");
            assert_eq!(
                *line_no, 5,
                "duplicate must report the last-occurrence line"
            );
        }
        other => panic!("expected UnknownKey, got {other:?}"),
    }
}

/// When a config has BOTH a type mismatch AND an unknown key, the
/// type-mismatch error wins and the unknown-key warning is discarded
/// (errors short-circuit the build). Locks current behavior so a
/// future change that would surface BOTH via `Result<(Config, Vec<Warning>)>`
/// must update this test deliberately.
#[test]
fn type_mismatch_discards_unknown_key_warning() {
    // CP_NAME=(should not be array) is a type mismatch for a required
    // scalar; UNKNOWN_KEY=value would normally yield a warning. With the
    // mismatch present, parse_str returns Err and the warning is
    // unreachable (the Vec is never built).
    let input = "\nCP_NAME=(should not be array)\nSSH_PUBKEY_FILE=/k\nUNKNOWN_KEY=value\n";
    let err = parse_str(input).expect_err("type mismatch on required scalar must error");
    match err {
        Error::TypeMismatch {
            field,
            expected,
            got,
            ..
        } => {
            assert_eq!(field, "CP_NAME");
            assert_eq!(expected, "scalar");
            assert_eq!(got, "array");
        }
        other => panic!("expected TypeMismatch for CP_NAME, got {other:?}"),
    }
    // The unknown-key warning is intentionally unreachable on the Err
    // path — there's no warnings vec to inspect when parse_str returns
    // Err. If the API ever evolves to surface warnings alongside errors,
    // this test must be updated to assert the unknown-key warning is
    // present too.
}
