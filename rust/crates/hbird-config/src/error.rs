//! Error types for the `cluster.local.conf` parser.
//!
//! Errors carry the offending line number (1-based) and the raw line text
//! whenever a parse failure refers to a specific input line. Operator-facing
//! tooling should surface both — the bash twin's failure mode ("source: line
//! 42: unexpected token") is the UX baseline this matches.
//!
//! Required-field errors carry the field name and the source path so the
//! operator sees the same "X is required in $CONFIG_PATH" shape that
//! `scripts/deploy-cluster.sh` emits via `${VAR:?...}`.

use std::path::PathBuf;

/// Parser + validation errors.
///
/// Constructed by [`crate::parse`] / [`crate::parse_str`]. Variants stay
/// flat (no nested enums) so consumers can `match` on the shape directly.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// I/O failure reading the config file (file not found, permission
    /// denied, etc.). Wraps the underlying [`std::io::Error`] and the path
    /// that was attempted, since the std error doesn't carry the path.
    #[error("could not read config at {path}: {source}")]
    Io {
        /// Path the parser tried to open.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A non-comment, non-blank line did not match any of the three
    /// supported shapes (`KEY=value`, `KEY="quoted"`, `KEY=(array)`). The
    /// parser intentionally does NOT evaluate shell — `${VAR}` expansion,
    /// command substitution, here-docs, and conditionals all fall into
    /// this bucket.
    #[error("unrecognized line {line_no} in cluster config: {raw:?}")]
    UnrecognizedLine {
        /// 1-based line number.
        line_no: usize,
        /// The raw line as it appeared in the file (trailing newline stripped).
        raw: String,
    },

    /// A `KEY=(...)` array opened but the file ended before the closing
    /// `)`. Multi-line arrays are accepted (bash allows them); an EOF
    /// before the close is a hard error rather than silent truncation.
    #[error("unterminated array for {key} starting at line {line_no}")]
    UnterminatedArray {
        /// Key whose array literal never closed.
        key: String,
        /// 1-based line number where the `KEY=(` opened.
        line_no: usize,
    },

    /// A quoted value (`KEY="..."` or `KEY='...'`) opened but did not
    /// close on the same line. The parser does not span quoted scalars
    /// across lines — bash does, but no existing `cluster.example.conf`
    /// value relies on multi-line scalars, and accepting them would
    /// complicate the grammar without operator-visible benefit.
    #[error("unterminated quoted value on line {line_no}: {raw:?}")]
    UnterminatedQuote {
        /// 1-based line number.
        line_no: usize,
        /// The raw line as it appeared in the file.
        raw: String,
    },

    /// A required field (`CP_NAME`, `SSH_PUBKEY_FILE`) was missing or
    /// empty. Matches the bash twin's `${VAR:?...}` failure shape so
    /// operators see the same diagnostic regardless of which front-end
    /// (bash vs. Rust) consumed the config.
    #[error("{field} is required in {path}")]
    MissingRequired {
        /// Name of the field that was unset or empty.
        field: &'static str,
        /// Source path the parser read from. `<inline>` for [`crate::parse_str`].
        path: String,
    },

    /// A field that must parse as a particular scalar type (currently:
    /// `CP_MEMORY`, `CP_VCPUS`, `WORKER_MEMORY`, `WORKER_VCPUS`,
    /// `ENABLE_CLOUD_INIT`) had a non-numeric value. The bash twin
    /// silently tolerates this until `virt-install` fails downstream;
    /// the Rust parser fails fast with the offending value visible.
    #[error("invalid integer for {field} at line {line_no}: {raw:?}")]
    InvalidInteger {
        /// Field name that failed to parse.
        field: &'static str,
        /// 1-based line number.
        line_no: usize,
        /// The raw value text (post-unquote).
        raw: String,
    },

    /// A boolean field whose bash twin hard-validates as exactly
    /// `true` / `false` (`AUTO_UPDATE_CP`, `SWITCH_TO_GHCR` — see the
    /// `case "$X" in true|false) ;; *) fail` blocks in
    /// `scripts/deploy-cluster.sh`) received something else. Mirrors the
    /// bash failure mode so a typo (`truue`, `TRUE`, `1`) doesn't
    /// silently fall back to a default in the Rust path while erroring
    /// in bash. (PR #315 round-2 review Lens 2 HIGH.)
    #[error(
        "invalid boolean for {field} at line {line_no}: {raw:?} (expected literal `true` or `false`)"
    )]
    InvalidBool {
        /// Field name that failed to parse.
        field: &'static str,
        /// 1-based line number.
        line_no: usize,
        /// The raw value text (post-unquote).
        raw: String,
    },

    /// A quoted scalar (`KEY="..."` / `KEY='...'`) closed cleanly but
    /// had non-comment non-whitespace text after the close quote — e.g.
    /// `CP_NAME="hbird" garbage`. Bash would tokenize that as two
    /// statements and error; the pre-#315-round-2 Rust parser silently
    /// dropped everything past the close quote, masking operator typos
    /// like a missing `#` before the comment. (PR #315 round-2 review
    /// Lens 2 HIGH.)
    #[error("trailing content after closing quote on line {line_no}: {raw:?}")]
    TrailingContent {
        /// 1-based line number.
        line_no: usize,
        /// The raw line as it appeared in the file.
        raw: String,
    },
}
