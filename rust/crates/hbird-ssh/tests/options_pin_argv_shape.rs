//! Pin the Rust [`SshOptions::to_argv`] output against the bash twin's
//! `ssh_opts_array{,_no_identity}` helpers in `lib/build-common.sh`.
//!
//! Drift detection — if either side adds/removes/reorders an option, this
//! test surfaces the mismatch in CI before any consumer crate ships a
//! divergent invocation.
//!
//! # What this test does NOT do
//!
//! Actually invoke bash. The bash array body is reproduced verbatim
//! below; reviewers updating `lib/build-common.sh` must also update the
//! expectations here (the inverse direction is enforced by clippy + the
//! per-field unit tests in `src/options.rs`). The constants live at the
//! top of the test so the diff is obvious in code review.
//!
//! # Bash reference (from `lib/build-common.sh`, function
//! `_ssh_opts_array_impl`)
//!
//! ```text
//! _opts=()
//! (( _with_identity == 1 )) && _opts+=( -i "$SSH_PRIVKEY_FILE" )
//! _opts+=(
//!   -o StrictHostKeyChecking=no
//!   -o UserKnownHostsFile=/dev/null
//!   -o LogLevel=ERROR
//!   -o ConnectTimeout=10
//!   -o BatchMode=yes
//! )
//! (( _with_cm == 1 )) && _opts+=(
//!   -o ControlMaster=auto
//!   -o "ControlPath=/tmp/hbird-ssh-${UID}-%r@%h:%p"
//!   -o ControlPersist=60s
//! )
//! [[ -n "$_proxy_jump" ]] && _opts+=( -o "ProxyJump=${_proxy_jump}" )
//! ```

use hbird_ssh::SshOptions;

/// `ssh_opts_array` (with identity) — no controlmaster, no proxy-jump.
/// Bash output:
/// `-i /keys/id -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
///  -o LogLevel=ERROR -o ConnectTimeout=10 -o BatchMode=yes`
#[test]
fn matches_ssh_opts_array_default() {
    let argv = SshOptions::new("kvm-host")
        .with_identity_file("/keys/id")
        .to_argv();

    // argv[0] is "ssh"; everything between that and the final target
    // must match the bash twin's array body exactly (order included).
    let expected = vec![
        "ssh",
        "-i",
        "/keys/id",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "BatchMode=yes",
        "kvm-host",
    ];
    assert_eq!(
        argv,
        expected.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "argv must match `ssh_opts_array` body verbatim"
    );
}

/// `ssh_opts_array_no_identity` — same as above but no `-i` line.
/// Used by `verify-hardening.sh` + `verify-encryption.sh`.
#[test]
fn matches_ssh_opts_array_no_identity() {
    let argv = SshOptions::new("kvm-host")
        .with_identity_file("/keys/id") // set, but...
        .without_identity() // ...suppressed.
        .to_argv();

    let expected = vec![
        "ssh",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "BatchMode=yes",
        "kvm-host",
    ];
    assert_eq!(
        argv,
        expected.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "without_identity() must reproduce `ssh_opts_array_no_identity` body"
    );
}

/// `ssh_opts_array --with-controlmaster` — appends ControlMaster=auto,
/// ControlPath template, ControlPersist=60s.
/// Used by `update-cluster.sh` to multiplex per-node operations over one
/// connection (saves ~3 SSH handshakes per node).
#[test]
fn matches_ssh_opts_array_with_controlmaster() {
    let argv = SshOptions::new("kvm-host")
        .with_identity_file("/keys/id")
        .with_controlmaster()
        .to_argv();

    // ControlPath includes the operator's UID; we can't pin the literal
    // UID (varies by runner) but we CAN pin the surrounding template.
    let cm_options: Vec<&String> = argv
        .iter()
        .skip_while(|a| a.as_str() != "ControlMaster=auto")
        .take(5)
        .collect();
    assert_eq!(
        cm_options.len(),
        5,
        "ControlMaster block must be 5 argv entries (ControlMaster=auto, -o, ControlPath=..., -o, ControlPersist=60s); got {cm_options:?}"
    );
    assert_eq!(cm_options[0], "ControlMaster=auto");
    assert_eq!(cm_options[1], "-o");
    assert!(
        cm_options[2].starts_with("ControlPath=/tmp/hbird-ssh-"),
        "ControlPath prefix must match bash twin; got {:?}",
        cm_options[2]
    );
    assert!(
        cm_options[2].ends_with("-%r@%h:%p"),
        "ControlPath suffix must match bash twin's `%r@%h:%p`; got {:?}",
        cm_options[2]
    );
    assert_eq!(cm_options[3], "-o");
    assert_eq!(cm_options[4], "ControlPersist=60s");
}

/// `ssh_opts_array --proxy-jump=HOST` — appends `-o ProxyJump=HOST`.
/// Used by every `verify-*` script when KVM_HOST is set.
#[test]
fn matches_ssh_opts_array_with_proxy_jump() {
    let argv = SshOptions::new("vm-host")
        .with_identity_file("/keys/id")
        .with_proxy_jump("kvm-host")
        .to_argv();

    // ProxyJump=... must appear AFTER the standard option block (matches
    // `_opts+=(...)` ordering in the bash function).
    let proxy_idx = argv
        .iter()
        .position(|a| a == "ProxyJump=kvm-host")
        .expect("ProxyJump option must be present");
    let connect_idx = argv
        .iter()
        .position(|a| a == "ConnectTimeout=10")
        .expect("ConnectTimeout sentinel must be present");
    assert!(
        proxy_idx > connect_idx,
        "ProxyJump must appear after the standard option block (matches bash twin ordering); proxy@{proxy_idx} ConnectTimeout@{connect_idx}, argv={argv:?}"
    );
}

/// Both --with-controlmaster AND --proxy-jump=HOST together — exercised
/// by the future `update-cluster` re-implementation (#286) which talks to
/// VMs via the KVM host and wants connection multiplexing.
#[test]
fn matches_ssh_opts_array_with_cm_and_proxy_jump() {
    let argv = SshOptions::new("vm-host")
        .with_identity_file("/keys/id")
        .with_controlmaster()
        .with_proxy_jump("kvm-host")
        .to_argv();

    // Order from the bash function: standard opts -> CM block -> ProxyJump.
    let cm_idx = argv
        .iter()
        .position(|a| a == "ControlMaster=auto")
        .expect("ControlMaster=auto present");
    let proxy_idx = argv
        .iter()
        .position(|a| a == "ProxyJump=kvm-host")
        .expect("ProxyJump=kvm-host present");
    let persist_idx = argv
        .iter()
        .position(|a| a == "ControlPersist=60s")
        .expect("ControlPersist=60s present");
    assert!(
        cm_idx < persist_idx && persist_idx < proxy_idx,
        "ordering must be CM-block then ProxyJump; got CM@{cm_idx} persist@{persist_idx} proxy@{proxy_idx}"
    );
}

/// Trailing positional argument is the SSH target — `user@host` when a
/// user was set, bare `host` otherwise. Bash uses `$KVM_HOST` directly
/// (no user prefix) since operators configure their user in
/// `~/.ssh/config`; the Rust crate exposes both shapes.
#[test]
fn target_is_last_positional_argument() {
    let bare = SshOptions::new("host-only").to_argv();
    assert_eq!(bare.last().map(String::as_str), Some("host-only"));

    let with_user = SshOptions::new("h").with_user("core").to_argv();
    assert_eq!(with_user.last().map(String::as_str), Some("core@h"));
}
