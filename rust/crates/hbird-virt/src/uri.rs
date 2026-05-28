//! `qemu+ssh://[user@]host[:port]/[system|session][?params]` URI parser.
//!
//! Implements the subset of libvirt's URI grammar that the project
//! actually uses against a remote KVM host:
//!
//! - **Scheme:** always `qemu+ssh` (other transports like `tls`, `tcp`,
//!   `unix`, or in-process `qemu:///` rejected — those are out of
//!   scope for the Rust rewrite per the epic). The parser also rejects
//!   uppercase scheme variants so the round-trip is byte-identical.
//! - **Userinfo:** optional `user@` (no password — that field is
//!   ignored by libvirt for SSH anyway since auth goes through the SSH
//!   layer, and embedding a password in a URI is a security footgun).
//! - **Host:** required. Bracketed `[::1]` IPv6 form accepted alongside
//!   bare hostnames and IPv4 literals.
//! - **Port:** optional `:NNNNN`, must parse as a `u16`.
//! - **Path:** `/system` or `/session` (libvirt's only two driver
//!   instances). `/system` is the default — what `virt-install` and
//!   every hummingbird script targets.
//! - **Query params:** preserved as a raw opaque string so the operator
//!   can stash libvirt-specific knobs like `?no_verify=1` or
//!   `?keyfile=...` without this parser caring about them.
//!
//! Reference: <https://libvirt.org/uri.html>.

use std::fmt;

use crate::error::{Error, Result};

/// Libvirt instance flavor.
///
/// `qemu:///system` is the host-wide libvirt daemon (root-owned VMs,
/// shared networks); `qemu:///session` is the per-user daemon. Every
/// hummingbird-k8s deployment targets `/system` — the bash twin
/// hard-codes `virsh -c qemu:///system` everywhere. `/session` is
/// accepted by the parser for completeness, not as a supported
/// deployment target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Instance {
    /// Host-wide libvirt daemon. The hummingbird-k8s default.
    System,
    /// Per-user libvirt daemon. Accepted but not exercised by the
    /// project's scripts.
    Session,
}

impl fmt::Display for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::System => "system",
            Self::Session => "session",
        })
    }
}

/// Parsed `qemu+ssh://` URI.
///
/// Construct via [`QemuSshUri::parse`]; serialize via the [`fmt::Display`]
/// impl. Equality is structural — two URIs that differ only in optional
/// query params compare unequal, which is what callers want when using
/// the URI as a cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QemuSshUri {
    /// Optional SSH username (the `user@` prefix). When `None`, the
    /// underlying SSH layer falls back to the local user / `~/.ssh/config`.
    pub user: Option<String>,
    /// SSH-reachable hostname or IP literal. Always present.
    pub host: String,
    /// Optional TCP port for the SSH connection. When `None`, the SSH
    /// layer uses the default (22, or whatever `~/.ssh/config` says).
    pub port: Option<u16>,
    /// `/system` or `/session`. Defaults to [`Instance::System`] when
    /// the path is empty or `/`.
    pub instance: Instance,
    /// Raw query string (without the leading `?`). Preserved verbatim
    /// so libvirt-specific knobs (`?no_verify=1`, `?keyfile=...`) round-
    /// trip without this parser interpreting them.
    pub query: Option<String>,
}

impl QemuSshUri {
    /// Parse a `qemu+ssh://[user@]host[:port]/[system|session][?query]`
    /// URI.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidUri`] for any grammar violation
    /// (missing scheme, empty host, unknown instance suffix, etc.) and
    /// [`Error::InvalidPort`] if the port component fails to parse as
    /// a `u16`.
    pub fn parse(raw: &str) -> Result<Self> {
        // Scheme is case-sensitive per libvirt's URI grammar; we reject
        // `QEMU+SSH://` so the Display round-trip stays byte-identical.
        let after_scheme = raw.strip_prefix("qemu+ssh://").ok_or(Error::InvalidUri {
            raw: raw.to_string(),
            reason: "missing 'qemu+ssh://' scheme prefix",
        })?;

        // Split off the query string first so the authority + path parse
        // doesn't have to thread `?` past every component. An empty
        // query (`?`) collapses to `None` — bash callers occasionally
        // emit one when shell-expanding a missing variable.
        let (auth_and_path, query) = match after_scheme.split_once('?') {
            Some((lhs, q)) if !q.is_empty() => (lhs, Some(q.to_string())),
            Some((lhs, _)) => (lhs, None),
            None => (after_scheme, None),
        };

        // Authority is everything up to the first `/`; the rest is the
        // path. A URI with no `/` at all (e.g. `qemu+ssh://host`) is
        // treated as authority-only with an empty path — the path
        // defaulting to `/system` happens below.
        let (authority, path) = match auth_and_path.split_once('/') {
            Some((a, p)) => (a, p),
            None => (auth_and_path, ""),
        };

        if authority.is_empty() {
            return Err(Error::InvalidUri {
                raw: raw.to_string(),
                reason: "empty authority (host) component",
            });
        }

        // Reject any `@` in the user component. `rsplit_once('@')` would
        // otherwise let `user@inner@host` parse with `"user@inner"` as
        // the user — that string would then be passed to `ssh` verbatim
        // and could be misinterpreted as an option. (PR #318 round-2
        // review L1 LOW.)
        let (user, host_port) = match authority.rsplit_once('@') {
            Some(("", hp)) => (None, hp), // `@host` — ignore empty user
            Some((u, hp)) => {
                if u.contains('@') {
                    return Err(Error::InvalidUri {
                        raw: raw.to_string(),
                        reason: "user component must not contain '@'",
                    });
                }
                (Some(u.to_string()), hp)
            }
            None => (None, authority),
        };

        // Validate user token: SSH wrappers concatenate `user@host` into
        // argv. A leading `-` or shell metacharacters would let
        // `qemu+ssh://-oProxyCommand=evil@host/system` smuggle SSH
        // options through ssh_target(). Allowlist alphanumeric + `._-`,
        // require non-`-` first char. (PR #318 round-2 review L1 HIGH —
        // OpenSSH dash-option injection CVE family.)
        if let Some(u) = user.as_deref() {
            validate_safe_token(u, "user", raw)?;
        }

        let is_ipv6 = host_port.starts_with('[');
        let (host, port) = split_host_port(host_port, raw)?;

        if host.is_empty() {
            return Err(Error::InvalidUri {
                raw: raw.to_string(),
                reason: "empty host",
            });
        }

        // Validate host token unless it came from a bracketed IPv6
        // literal (whose charset is constrained by split_host_port's
        // bracket parser already). For bare hostnames + IPv4 literals,
        // enforce the same allowlist as user. (PR #318 round-2 review
        // L1 HIGH.)
        if !is_ipv6 {
            validate_safe_token(host, "host", raw)?;
        }

        let instance = match path {
            "" | "system" => Instance::System,
            "session" => Instance::Session,
            other => {
                // Distinguish "unknown instance" from "extra path
                // segments" so the diagnostic is useful. Both fall into
                // InvalidUri for now; the operator-facing message lives
                // in `reason`.
                let _ = other;
                return Err(Error::InvalidUri {
                    raw: raw.to_string(),
                    reason: "unsupported libvirt instance path (expected /system or /session)",
                });
            }
        };

        Ok(Self {
            user,
            host: host.to_string(),
            port,
            instance,
            query,
        })
    }

    /// The remote libvirt URI that this `qemu+ssh://` URI ultimately
    /// targets: `qemu:///system` or `qemu:///session`.
    ///
    /// This is the value passed to `virsh -c <here>` on the remote
    /// host — the `+ssh` transport is unwrapped on the SSH client
    /// side, so the remote `virsh` sees only the bare `qemu:///`
    /// URI. Matches the historical `scripts/kubectl-k8s.sh`
    /// `virsh -c qemu:///system domifaddr` invocation (kubectl-k8s.sh
    /// was removed in the v0.1.0 cutover #353; the Rust twin
    /// `hbird kubectl` is now canonical).
    #[must_use]
    pub fn remote_uri(&self) -> String {
        format!("qemu:///{}", self.instance)
    }

    /// The SSH target string — `user@host` or `host` — suitable for
    /// passing to [`crate::ssh::SshClient::run`]. Port is NOT included
    /// here (SSH targets are typically aliases from `~/.ssh/config`
    /// that already carry their own port); call sites that need the
    /// port should read [`Self::port`] directly and pass it to the
    /// underlying transport.
    #[must_use]
    pub fn ssh_target(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Split a `host[:port]` chunk, honoring `[::1]:22` IPv6 bracket
/// syntax. Returns `(host, port)` — host with brackets stripped if
/// they were present.
fn split_host_port<'a>(host_port: &'a str, raw: &str) -> Result<(&'a str, Option<u16>)> {
    if let Some(rest) = host_port.strip_prefix('[') {
        // IPv6 literal. Find the matching `]`, then maybe a `:port`.
        let close = rest.find(']').ok_or(Error::InvalidUri {
            raw: raw.to_string(),
            reason: "IPv6 literal missing closing bracket",
        })?;
        let host = &rest[..close];
        let tail = &rest[close + 1..];
        let port = if let Some(p) = tail.strip_prefix(':') {
            Some(p.parse::<u16>().map_err(|source| Error::InvalidPort {
                raw: raw.to_string(),
                source,
            })?)
        } else if tail.is_empty() {
            None
        } else {
            return Err(Error::InvalidUri {
                raw: raw.to_string(),
                reason: "garbage after IPv6 literal close bracket",
            });
        };
        // IPv6 literal — the bracket parser above already constrained
        // the charset to whatever appears between `[...]`. We don't
        // re-validate here; the caller skips validate_safe_token for
        // the bracketed case.
        Ok((host, port))
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        // Plain `host:port`. Only valid when the host part has no
        // colons (otherwise it's an unbracketed IPv6 literal — reject).
        if h.contains(':') {
            return Err(Error::InvalidUri {
                raw: raw.to_string(),
                reason: "IPv6 literal must be bracketed when paired with a port",
            });
        }
        let port = p.parse::<u16>().map_err(|source| Error::InvalidPort {
            raw: raw.to_string(),
            source,
        })?;
        Ok((h, Some(port)))
    } else {
        Ok((host_port, None))
    }
}

/// Validate that a URI token (user or non-bracketed host) is safe to
/// concatenate into an `ssh` argv slot. Rejects:
///
/// - Empty strings (already caught upstream for host; defensive here).
/// - Leading `-` — closes the OpenSSH dash-option-injection vulnerability
///   class. A host or user starting with `-` would be parsed by `ssh` as
///   a flag (e.g. `-oProxyCommand=evil`), bypassing the operator's
///   intended target.
/// - Anything outside the conservative `[A-Za-z0-9._-]` allowlist —
///   shell metas, whitespace, control chars, and `@`/`:` (the latter
///   would conflict with this crate's own URI grammar).
///
/// Bracketed IPv6 literals are exempt — they have their own constrained
/// charset enforced by `split_host_port`'s bracket parser. (PR #318
/// round-2 review L1 HIGH.)
fn validate_safe_token(s: &str, field: &'static str, raw: &str) -> Result<()> {
    if s.is_empty() {
        return Err(Error::InvalidUri {
            raw: raw.to_string(),
            reason: match field {
                "host" => "host token must not be empty",
                "user" => "user token must not be empty",
                _ => "token must not be empty",
            },
        });
    }
    if s.starts_with('-') {
        return Err(Error::InvalidUri {
            raw: raw.to_string(),
            reason: match field {
                "host" => "host must not start with '-' (would be parsed as an SSH option)",
                "user" => "user must not start with '-' (would be parsed as an SSH option)",
                _ => "token must not start with '-'",
            },
        });
    }
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';
        if !ok {
            return Err(Error::InvalidUri {
                raw: raw.to_string(),
                reason: match field {
                    "host" => "host contains characters outside [A-Za-z0-9._-]",
                    "user" => "user contains characters outside [A-Za-z0-9._-]",
                    _ => "token contains disallowed characters",
                },
            });
        }
    }
    Ok(())
}

impl fmt::Display for QemuSshUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("qemu+ssh://")?;
        if let Some(u) = &self.user {
            write!(f, "{u}@")?;
        }
        // Bracket bare IPv6 literals on output so the round-trip stays
        // unambiguous. Heuristic: contains a colon → IPv6.
        if self.host.contains(':') {
            write!(f, "[{}]", self.host)?;
        } else {
            f.write_str(&self.host)?;
        }
        if let Some(p) = self.port {
            write!(f, ":{p}")?;
        }
        write!(f, "/{}", self.instance)?;
        if let Some(q) = &self.query {
            write!(f, "?{q}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn host_with_leading_dash_rejected() {
        let err = QemuSshUri::parse("qemu+ssh://-oProxyCommand=evil/system")
            .expect_err("host starting with '-' must be rejected");
        match err {
            Error::InvalidUri { reason, .. } => {
                assert!(
                    reason.contains("host") && reason.contains("'-'"),
                    "diagnostic should name the host + the leading dash, got: {reason:?}"
                );
            }
            other => panic!("expected InvalidUri, got {other:?}"),
        }
    }

    #[test]
    fn user_with_leading_dash_rejected() {
        let err = QemuSshUri::parse("qemu+ssh://-oProxyCommand=evil@host/system")
            .expect_err("user starting with '-' must be rejected");
        assert!(matches!(err, Error::InvalidUri { .. }));
    }

    #[test]
    fn user_containing_at_sign_rejected() {
        let err = QemuSshUri::parse("qemu+ssh://user@inner@host/system")
            .expect_err("user containing '@' must be rejected");
        match err {
            Error::InvalidUri { reason, .. } => {
                assert!(reason.contains("'@'"), "got: {reason:?}");
            }
            other => panic!("expected InvalidUri, got {other:?}"),
        }
    }

    #[test]
    fn host_with_shell_metacharacter_rejected() {
        // `;rm` would be a classic shell-injection payload if a downstream
        // ssh wrapper concatenated host into a shell command. Even though
        // our SshClient impls don't (they use Command::arg), defense in
        // depth at the parser layer.
        let err = QemuSshUri::parse("qemu+ssh://host;rm/system")
            .expect_err("host with ';' must be rejected");
        assert!(matches!(err, Error::InvalidUri { .. }));
    }

    #[test]
    fn ipv6_literal_bypasses_token_validation() {
        // IPv6 hosts contain ':' which would fail the allowlist — but
        // bracket parsing already constrains the charset. Confirm
        // bracketed IPv6 still parses.
        let uri =
            QemuSshUri::parse("qemu+ssh://[::1]/system").expect("bracketed IPv6 must still parse");
        assert_eq!(uri.host, "::1");
    }

    #[test]
    fn normal_hostnames_still_parse() {
        // Sanity: don't break the happy path.
        for ok in ["kvm-host", "host.example.com", "h_1", "192.168.1.10"] {
            let uri = format!("qemu+ssh://{ok}/system");
            QemuSshUri::parse(&uri).unwrap_or_else(|e| panic!("{ok} should parse, got {e:?}"));
        }
    }
}
