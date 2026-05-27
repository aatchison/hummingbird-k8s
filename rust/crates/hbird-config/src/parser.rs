//! Hand-rolled parser for the declarative-bash subset described in the
//! crate-level docs. No regex, no third-party parser — the grammar is
//! small enough that a line-by-line walk is shorter, faster, and easier
//! to reason about than pulling in `nom` or `regex`.
//!
//! # Grammar
//!
//! ```text
//! file       := (line "\n")*
//! line       := blank | comment | assignment
//! comment    := WS* "#" .*
//! blank      := WS*
//! assignment := WS* KEY "=" rhs
//! rhs        := bare_scalar | quoted_scalar | array
//! array      := "(" (WS* element)* WS* ")"
//! element    := quoted_scalar | bare_scalar_no_ws
//! ```
//!
//! Trailing inline comments are stripped from bare scalars when a `#` is
//! preceded by whitespace — same rule as bash. `#` flush against
//! non-whitespace stays part of the value (so `GHCR_TAG=tag#v1` works).

use std::collections::HashMap;

use crate::error::Error;
use crate::{ClusterConfig, Result};

/// Maximum field name length the parser will accept on a line. Anything
/// longer is treated as not-an-assignment and falls through to
/// [`Error::UnrecognizedLine`]. Cheap guardrail against pathological input.
const MAX_KEY_LEN: usize = 128;

#[derive(Debug, Clone)]
enum Value {
    Scalar(String),
    Array(Vec<String>),
}

/// Top-level entry point. `source_path` shows up in
/// [`Error::MissingRequired`] so the diagnostic matches the bash twin's
/// `${VAR:?... is required in $CONFIG_PATH}` shape.
pub(crate) fn parse_with_source(input: &str, source_path: &str) -> Result<ClusterConfig> {
    let assignments = tokenize(input)?;
    build_config(assignments, source_path)
}

/// Walk the input line-by-line, producing a flat
/// `key -> (line_no_of_assignment, Value)` map. Later assignments
/// overwrite earlier ones — same as bash.
fn tokenize(input: &str) -> Result<HashMap<String, (usize, Value)>> {
    let mut out: HashMap<String, (usize, Value)> = HashMap::new();
    let lines: Vec<&str> = input.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        let line_no = idx + 1;
        let raw = lines[idx];
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }

        // Find the first `=`. Everything before is a candidate key; if it
        // isn't a valid shell identifier we bail with UnrecognizedLine so
        // operators see exactly where the parser got confused.
        let eq = match trimmed.find('=') {
            Some(pos) => pos,
            None => {
                return Err(Error::UnrecognizedLine {
                    line_no,
                    raw: raw.to_string(),
                });
            }
        };
        let key = &trimmed[..eq];
        if !is_valid_key(key) {
            return Err(Error::UnrecognizedLine {
                line_no,
                raw: raw.to_string(),
            });
        }
        let rhs = &trimmed[eq + 1..];

        let value = if let Some(rest) = rhs.strip_prefix('(') {
            // Array. Collect tokens until we see `)`, allowing the array
            // to span multiple lines.
            let (elements, consumed_extra_lines) =
                parse_array(key, line_no, rest, &lines[idx + 1..])?;
            idx += 1 + consumed_extra_lines;
            Value::Array(elements)
        } else {
            // Scalar — quoted or bare.
            let scalar = parse_scalar(line_no, raw, rhs)?;
            idx += 1;
            Value::Scalar(scalar)
        };

        out.insert(key.to_string(), (line_no, value));
    }
    Ok(out)
}

fn is_valid_key(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_KEY_LEN {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse a scalar RHS (everything after the `=`). Handles `"..."`,
/// `'...'`, and bare values with optional trailing whitespace-led `#`
/// comment. PR #315 round-2 review Lens 2 HIGH: after a closing quote,
/// only whitespace or a `#` comment is allowed; anything else
/// (`CP_NAME="hbird" garbage`) errors via `Error::TrailingContent`
/// rather than silently dropping the trailing text.
fn parse_scalar(line_no: usize, raw_line: &str, rhs: &str) -> Result<String> {
    let trimmed = rhs.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"') {
        match rest.find('"') {
            Some(end) => {
                check_trailing_after_quote(rest, end, line_no, raw_line)?;
                Ok(rest[..end].to_string())
            }
            None => Err(Error::UnterminatedQuote {
                line_no,
                raw: raw_line.to_string(),
            }),
        }
    } else if let Some(rest) = trimmed.strip_prefix('\'') {
        match rest.find('\'') {
            Some(end) => {
                check_trailing_after_quote(rest, end, line_no, raw_line)?;
                Ok(rest[..end].to_string())
            }
            None => Err(Error::UnterminatedQuote {
                line_no,
                raw: raw_line.to_string(),
            }),
        }
    } else {
        // Bare scalar. Strip a trailing `# comment` ONLY when the `#` is
        // preceded by whitespace (bash rule). `GHCR_TAG=tag#v1` keeps `#v1`.
        Ok(strip_trailing_comment(trimmed).trim_end().to_string())
    }
}

/// Helper for parse_scalar: after locating the close-quote at position
/// `end` in `rest` (the slice starting AFTER the open-quote), verify
/// the remainder is whitespace or a `#`-led comment. Anything else is
/// a `TrailingContent` error. (PR #315 round-2 review Lens 2 HIGH.)
fn check_trailing_after_quote(
    rest: &str,
    end: usize,
    line_no: usize,
    raw_line: &str,
) -> Result<()> {
    let tail = rest[end + 1..].trim_start();
    if tail.is_empty() || tail.starts_with('#') {
        Ok(())
    } else {
        Err(Error::TrailingContent {
            line_no,
            raw: raw_line.to_string(),
        })
    }
}

fn strip_trailing_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &s[..i];
        }
        i += 1;
    }
    s
}

/// Parse a `KEY=(...)` array starting from `first_line_rest` (the text
/// AFTER the opening `(`). Returns the parsed elements and the number of
/// EXTRA lines consumed beyond the opening line (so the caller can
/// advance its index).
fn parse_array(
    key: &str,
    open_line: usize,
    first_line_rest: &str,
    rest_lines: &[&str],
) -> Result<(Vec<String>, usize)> {
    // Concatenate all the array body lines until we see `)`, then
    // tokenize the body as whitespace-separated elements respecting
    // quoted segments.
    let mut body = String::new();
    let mut extra = 0;

    if let Some(close) = first_line_rest.find(')') {
        body.push_str(&first_line_rest[..close]);
        let elements = tokenize_array_body(&body, open_line)?;
        return Ok((elements, extra));
    }
    body.push_str(first_line_rest);
    body.push(' ');

    for line in rest_lines {
        extra += 1;
        let line_no = open_line + extra;
        if let Some(close) = line.find(')') {
            body.push_str(&line[..close]);
            let elements = tokenize_array_body(&body, line_no)?;
            return Ok((elements, extra));
        }
        // Skip blank / comment-only lines inside an array body — bash
        // allows comments inside `KEY=( ... )` as long as they're on
        // their own line. We strip them here so the body concat stays
        // clean.
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        body.push_str(line);
        body.push(' ');
    }

    Err(Error::UnterminatedArray {
        key: key.to_string(),
        line_no: open_line,
    })
}

/// Split an array body into elements. Whitespace-separated, quoted
/// segments preserved as single elements.
fn tokenize_array_body(body: &str, line_no: usize) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = body.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            chars.next(); // consume opening quote
            let start = i + 1;
            let mut end = None;
            for (j, ch) in chars.by_ref() {
                if ch == quote {
                    end = Some(j);
                    break;
                }
            }
            match end {
                Some(e) => out.push(body[start..e].to_string()),
                None => {
                    return Err(Error::UnterminatedQuote {
                        line_no,
                        raw: body.to_string(),
                    });
                }
            }
        } else {
            let start = i;
            let mut last = i + c.len_utf8();
            chars.next();
            while let Some(&(j, ch)) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                last = j + ch.len_utf8();
                chars.next();
            }
            out.push(body[start..last].to_string());
        }
    }
    Ok(out)
}

// ---- Field assembly --------------------------------------------------------

fn build_config(
    mut raw: HashMap<String, (usize, Value)>,
    source_path: &str,
) -> Result<ClusterConfig> {
    let cp_name = take_required_scalar(&mut raw, "CP_NAME", source_path)?;
    let ssh_pubkey_file = take_required_scalar(&mut raw, "SSH_PUBKEY_FILE", source_path)?;

    Ok(ClusterConfig {
        cp_name,
        ssh_pubkey_file,
        kvm_host: take_optional_scalar(&mut raw, "KVM_HOST"),
        image_source: take_scalar_or_default(&mut raw, "IMAGE_SOURCE", "ghcr"),
        ghcr_tag: take_scalar_or_default(&mut raw, "GHCR_TAG", "latest"),
        enable_cloud_init: take_u32_or_default(&mut raw, "ENABLE_CLOUD_INIT", 0)?,
        auto_update_cp: take_bool_strict_or_default(&mut raw, "AUTO_UPDATE_CP", true)?,
        switch_to_ghcr: take_bool_strict_or_default(&mut raw, "SWITCH_TO_GHCR", true)?,
        cp_memory: take_u32_or_default(&mut raw, "CP_MEMORY", 8192)?,
        cp_vcpus: take_u32_or_default(&mut raw, "CP_VCPUS", 4)?,
        worker_memory: take_u32_or_default(&mut raw, "WORKER_MEMORY", 4096)?,
        worker_vcpus: take_u32_or_default(&mut raw, "WORKER_VCPUS", 2)?,
        pool_dir: take_scalar_or_default(&mut raw, "POOL_DIR", "/var/lib/libvirt/images"),
        run_verify: take_bool_or_default(&mut raw, "RUN_VERIFY", false),
        bootc_update_schedule: take_optional_scalar(&mut raw, "BOOTC_UPDATE_SCHEDULE"),
        bootc_update_repo_k8s: take_optional_scalar(&mut raw, "BOOTC_UPDATE_REPO_K8S"),
        bootc_update_repo_worker: take_optional_scalar(&mut raw, "BOOTC_UPDATE_REPO_WORKER"),
        worker_names: take_optional_array(&mut raw, "WORKER_NAMES"),
        cp_ip: take_optional_scalar(&mut raw, "CP_IP"),
        worker_ips: take_optional_array(&mut raw, "WORKER_IPS"),
    })
}

fn take_required_scalar(
    raw: &mut HashMap<String, (usize, Value)>,
    key: &'static str,
    source_path: &str,
) -> Result<String> {
    match raw.remove(key) {
        Some((_, Value::Scalar(v))) if !v.is_empty() => Ok(v),
        _ => Err(Error::MissingRequired {
            field: key,
            path: source_path.to_string(),
        }),
    }
}

fn take_optional_scalar(
    raw: &mut HashMap<String, (usize, Value)>,
    key: &'static str,
) -> Option<String> {
    match raw.remove(key) {
        Some((_, Value::Scalar(v))) => Some(v),
        // Arrays here are silently dropped — none of the current optional
        // scalar fields would ever sensibly be set as an array; if an
        // operator does it, the bash twin would also misbehave.
        _ => None,
    }
}

fn take_scalar_or_default(
    raw: &mut HashMap<String, (usize, Value)>,
    key: &'static str,
    default: &str,
) -> String {
    match raw.remove(key) {
        Some((_, Value::Scalar(v))) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

fn take_u32_or_default(
    raw: &mut HashMap<String, (usize, Value)>,
    key: &'static str,
    default: u32,
) -> Result<u32> {
    match raw.remove(key) {
        Some((line_no, Value::Scalar(v))) if !v.is_empty() => {
            v.parse::<u32>().map_err(|_| Error::InvalidInteger {
                field: key,
                line_no,
                raw: v,
            })
        }
        _ => Ok(default),
    }
}

/// Lenient bool parser for fields whose bash twin uses `[[ "$X" = "true" ]]`
/// (truthy-string semantics). Currently only `RUN_VERIFY`. Empty string
/// falls through to the caller's default (mirrors bash `${VAR:=default}`
/// — empty-string explicit assignment re-applies the default). Unknown
/// non-empty spellings fall through to default rather than erroring,
/// matching the bash side's loose handling.
///
/// PR #315 round-2 review Lens 2 HIGH: the pre-round-2 code mapped `""`
/// to `false` regardless of the caller's default, so `RUN_VERIFY=""` and
/// `AUTO_UPDATE_CP=""` both returned `false` even when the default was
/// `true`. Fixed by removing the `""` arm so it falls through to default.
fn take_bool_or_default(
    raw: &mut HashMap<String, (usize, Value)>,
    key: &'static str,
    default: bool,
) -> bool {
    match raw.remove(key) {
        Some((_, Value::Scalar(v))) => match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            // Empty string AND unknown spellings fall through to the
            // caller's default — bash's `${VAR:=default}` would resolve
            // empty to the default, and lenient truthy-string semantics
            // tolerate unknown spellings.
            _ => default,
        },
        _ => default,
    }
}

/// Strict bool parser for fields whose bash twin hard-fails on anything
/// other than literal `true` / `false` (`AUTO_UPDATE_CP`, `SWITCH_TO_GHCR`;
/// see `case "$X" in true|false) ;; *) fail` blocks in
/// `scripts/deploy-cluster.sh`). Returns `Error::InvalidBool` so a typo
/// like `truue` errors in the Rust path the same way bash would, rather
/// than silently falling back to a default.
///
/// PR #315 round-2 review Lens 2 HIGH. Case-sensitive (matches bash's
/// case-sensitive comparison). Empty string falls through to the
/// caller's default (matches bash `:=`).
fn take_bool_strict_or_default(
    raw: &mut HashMap<String, (usize, Value)>,
    key: &'static str,
    default: bool,
) -> Result<bool> {
    match raw.remove(key) {
        Some((line_no, Value::Scalar(v))) => match v.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            "" => Ok(default),
            _ => Err(Error::InvalidBool {
                field: key,
                line_no,
                raw: v,
            }),
        },
        _ => Ok(default),
    }
}

fn take_optional_array(
    raw: &mut HashMap<String, (usize, Value)>,
    key: &'static str,
) -> Option<Vec<String>> {
    match raw.remove(key) {
        Some((_, Value::Array(v))) => Some(v),
        _ => None,
    }
}
