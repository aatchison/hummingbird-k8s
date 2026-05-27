//! Integration tests for [`hbird_virt::QemuSshUri`].
//!
//! Validates the parser against the URI shapes libvirt documents
//! (<https://libvirt.org/uri.html>) plus the rejection cases that
//! distinguish "operator typo" from "valid-but-unsupported" URIs.

use hbird_virt::{Error, Instance, QemuSshUri};

#[test]
fn parses_minimal_uri() {
    let u = QemuSshUri::parse("qemu+ssh://kvm.example/system").expect("ok");
    assert_eq!(u.user, None);
    assert_eq!(u.host, "kvm.example");
    assert_eq!(u.port, None);
    assert_eq!(u.instance, Instance::System);
    assert_eq!(u.query, None);
    assert_eq!(u.remote_uri(), "qemu:///system");
    assert_eq!(u.ssh_target(), "kvm.example");
}

#[test]
fn parses_with_user_and_port() {
    let u = QemuSshUri::parse("qemu+ssh://op@kvm.example:2222/system").expect("ok");
    assert_eq!(u.user.as_deref(), Some("op"));
    assert_eq!(u.host, "kvm.example");
    assert_eq!(u.port, Some(2222));
    assert_eq!(u.ssh_target(), "op@kvm.example");
}

#[test]
fn defaults_to_system_when_path_empty() {
    // `qemu+ssh://kvm.example` — no path. Libvirt's docs say `/system`
    // is the default and operators leave the path off in practice.
    let u = QemuSshUri::parse("qemu+ssh://kvm.example").expect("ok");
    assert_eq!(u.instance, Instance::System);
}

#[test]
fn parses_session_instance() {
    let u = QemuSshUri::parse("qemu+ssh://kvm.example/session").expect("ok");
    assert_eq!(u.instance, Instance::Session);
    assert_eq!(u.remote_uri(), "qemu:///session");
}

#[test]
fn preserves_query_string_verbatim() {
    // libvirt-specific knobs like ?no_verify=1 stay opaque to us.
    let u = QemuSshUri::parse("qemu+ssh://op@host/system?no_verify=1&keyfile=/k").expect("ok");
    assert_eq!(u.query.as_deref(), Some("no_verify=1&keyfile=/k"));
}

#[test]
fn empty_query_collapses_to_none() {
    // `?` with nothing after it — bash callers sometimes emit this
    // when a shell variable expanded to empty.
    let u = QemuSshUri::parse("qemu+ssh://kvm.example/system?").expect("ok");
    assert_eq!(u.query, None);
}

#[test]
fn ipv6_bracketed_host_parses() {
    let u = QemuSshUri::parse("qemu+ssh://[2001:db8::1]:22/system").expect("ok");
    assert_eq!(u.host, "2001:db8::1");
    assert_eq!(u.port, Some(22));
}

#[test]
fn ipv6_bracketed_host_no_port() {
    let u = QemuSshUri::parse("qemu+ssh://[::1]/system").expect("ok");
    assert_eq!(u.host, "::1");
    assert_eq!(u.port, None);
}

#[test]
fn unbracketed_ipv6_with_port_is_rejected() {
    // ::1:22 is ambiguous (is the trailing :22 a port or another IPv6
    // group?). libvirt requires the brackets — we mirror that.
    let err = QemuSshUri::parse("qemu+ssh://::1:22/system").expect_err("must reject");
    assert!(matches!(err, Error::InvalidUri { .. }));
}

#[test]
fn rejects_missing_scheme() {
    let err = QemuSshUri::parse("ssh://kvm/system").expect_err("must reject");
    match err {
        Error::InvalidUri { reason, .. } => assert!(reason.contains("scheme")),
        other => panic!("expected InvalidUri, got {other:?}"),
    }
}

#[test]
fn rejects_uppercase_scheme() {
    // Round-trip stability: Display always emits lowercase scheme,
    // so accepting uppercase input would silently rewrite the URI.
    let err = QemuSshUri::parse("QEMU+SSH://kvm/system").expect_err("must reject");
    assert!(matches!(err, Error::InvalidUri { .. }));
}

#[test]
fn rejects_empty_host() {
    let err = QemuSshUri::parse("qemu+ssh:///system").expect_err("must reject");
    match err {
        Error::InvalidUri { reason, .. } => {
            assert!(
                reason.contains("authority") || reason.contains("host"),
                "reason: {reason}"
            );
        }
        other => panic!("expected InvalidUri, got {other:?}"),
    }
}

#[test]
fn rejects_empty_host_with_user() {
    // `qemu+ssh://op@/system` — user present, host blank.
    let err = QemuSshUri::parse("qemu+ssh://op@/system").expect_err("must reject");
    assert!(matches!(err, Error::InvalidUri { .. }));
}

#[test]
fn rejects_unknown_instance() {
    let err = QemuSshUri::parse("qemu+ssh://kvm/orchestra").expect_err("must reject");
    assert!(matches!(err, Error::InvalidUri { .. }));
}

#[test]
fn rejects_unsupported_transport() {
    // qemu+tcp, qemu+tls, plain qemu:/// — all out of scope.
    for raw in [
        "qemu+tcp://kvm/system",
        "qemu+tls://kvm/system",
        "qemu:///system",
    ] {
        let err = QemuSshUri::parse(raw).expect_err(&format!("{raw} should reject"));
        assert!(
            matches!(err, Error::InvalidUri { .. }),
            "wrong variant for {raw}: {err:?}"
        );
    }
}

#[test]
fn rejects_garbage_port() {
    let err = QemuSshUri::parse("qemu+ssh://kvm:notnum/system").expect_err("must reject");
    assert!(matches!(err, Error::InvalidPort { .. }));
}

#[test]
fn rejects_port_overflow() {
    // 65536 doesn't fit in u16.
    let err = QemuSshUri::parse("qemu+ssh://kvm:65536/system").expect_err("must reject");
    assert!(matches!(err, Error::InvalidPort { .. }));
}

#[test]
fn rejects_garbage_after_ipv6_bracket() {
    let err = QemuSshUri::parse("qemu+ssh://[::1]junk:22/system").expect_err("must reject");
    assert!(matches!(err, Error::InvalidUri { .. }));
}

#[test]
fn rejects_unclosed_ipv6_bracket() {
    let err = QemuSshUri::parse("qemu+ssh://[::1/system").expect_err("must reject");
    assert!(matches!(err, Error::InvalidUri { .. }));
}

#[test]
fn display_roundtrip_minimal() {
    let raw = "qemu+ssh://kvm.example/system";
    let u = QemuSshUri::parse(raw).unwrap();
    assert_eq!(u.to_string(), raw);
}

#[test]
fn display_roundtrip_full() {
    let raw = "qemu+ssh://op@kvm.example:2222/session?no_verify=1";
    let u = QemuSshUri::parse(raw).unwrap();
    assert_eq!(u.to_string(), raw);
}

#[test]
fn display_brackets_bare_ipv6_on_output() {
    // Parsed from bracketed input — output must re-bracket so the
    // round-trip is unambiguous.
    let u = QemuSshUri::parse("qemu+ssh://[2001:db8::1]/system").unwrap();
    assert_eq!(u.to_string(), "qemu+ssh://[2001:db8::1]/system");
}

#[test]
fn ssh_target_omits_port() {
    // Port lives on the URI; the SSH-target string mirrors the bash
    // twin's `ssh "$KVM_HOST"` shape where port comes from
    // ~/.ssh/config (or a future explicit override).
    let u = QemuSshUri::parse("qemu+ssh://op@kvm:2222/system").unwrap();
    assert_eq!(u.ssh_target(), "op@kvm");
}
