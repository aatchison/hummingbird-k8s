//! Integration tests for [`hbird_virt::Connection`] using a stub
//! [`SshClient`].
//!
//! The stub records each `(host, command)` call and returns a canned
//! `(stdout, exit)` pair the test pre-loaded. This lets us assert two
//! things without ever opening a real socket:
//!
//! 1. The exact `virsh` command string we send the remote (matches the
//!    bash twin's shape).
//! 2. The parsing of `virsh`'s stdout into typed `Domain` / `DomainInfo`
//!    values.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hbird_virt::{Connection, Error, QemuSshUri, SshClient, SshError};

/// Canned response keyed by the full remote command string.
#[derive(Clone)]
enum Reply {
    Ok(String),
    /// virsh ran and exited non-zero.
    NonZero {
        stderr: String,
        exit_code: i32,
    },
    /// SSH transport layer failed (never reached virsh).
    Transport(String),
}

#[derive(Default)]
struct StubSshClient {
    /// `(host, command)` -> canned reply.
    replies: Mutex<HashMap<(String, String), Reply>>,
    /// Ordered log of every call the SUT made, for assertion.
    calls: Mutex<Vec<(String, String)>>,
}

impl StubSshClient {
    fn new() -> Self {
        Self::default()
    }

    fn expect(&self, host: &str, command: &str, reply: Reply) {
        self.replies
            .lock()
            .unwrap()
            .insert((host.to_string(), command.to_string()), reply);
    }

    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl SshClient for StubSshClient {
    fn run(&self, host: &str, command: &str) -> Result<String, SshError> {
        self.calls
            .lock()
            .unwrap()
            .push((host.to_string(), command.to_string()));
        match self
            .replies
            .lock()
            .unwrap()
            .get(&(host.to_string(), command.to_string()))
            .cloned()
        {
            Some(Reply::Ok(out)) => Ok(out),
            Some(Reply::NonZero { stderr, exit_code }) => Err(SshError::RemoteExit {
                host: host.to_string(),
                command: command.to_string(),
                exit_code: Some(exit_code),
                stderr,
            }),
            Some(Reply::Transport(message)) => Err(SshError::Transport {
                host: host.to_string(),
                message,
            }),
            None => Err(SshError::Transport {
                host: host.to_string(),
                message: format!("StubSshClient: no canned reply for command {command:?}"),
            }),
        }
    }
}

fn make_conn(stub: Arc<StubSshClient>) -> Connection {
    let uri = QemuSshUri::parse("qemu+ssh://op@kvm.example/system").unwrap();
    Connection::new(uri, stub)
}

#[test]
fn domains_lists_running_and_stopped() {
    let stub = Arc::new(StubSshClient::new());
    // `virsh list --all --name` emits one name per line, with a
    // trailing blank line on real output.
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system list --all --name",
        Reply::Ok("hbird-cp1\nhbird-w1\nhbird-w2\n\n".to_string()),
    );
    let conn = make_conn(Arc::clone(&stub));
    let doms = conn.domains().expect("ok");
    let names: Vec<_> = doms.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["hbird-cp1", "hbird-w1", "hbird-w2"]);
    // And we hit the right host + command:
    assert_eq!(
        stub.calls(),
        vec![(
            "op@kvm.example".to_string(),
            "virsh -c qemu:///system list --all --name".to_string()
        )]
    );
}

#[test]
fn domains_returns_empty_on_no_domains() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system list --all --name",
        Reply::Ok("\n".to_string()),
    );
    let conn = make_conn(stub);
    assert!(conn.domains().unwrap().is_empty());
}

#[test]
fn domains_surfaces_virsh_failure_as_virshfailed() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system list --all --name",
        Reply::NonZero {
            stderr: "error: failed to connect to the hypervisor\n".to_string(),
            exit_code: 1,
        },
    );
    let conn = make_conn(stub);
    let err = conn.domains().expect_err("must fail");
    match err {
        Error::VirshFailed { command, stderr } => {
            assert!(command.contains("list --all --name"));
            assert!(stderr.contains("failed to connect"));
        }
        other => panic!("expected VirshFailed, got {other:?}"),
    }
}

#[test]
fn domains_surfaces_ssh_transport_failure() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system list --all --name",
        Reply::Transport("connection refused".to_string()),
    );
    let conn = make_conn(stub);
    let err = conn.domains().expect_err("must fail");
    assert!(matches!(err, Error::Ssh { .. }));
}

#[test]
fn domifaddr_parses_ipv4_lease() {
    let stub = Arc::new(StubSshClient::new());
    let out = " Name       MAC address          Protocol     Address\n\
               -------------------------------------------------------------------------------\n\
               vnet0      52:54:00:01:02:03    ipv4         192.168.122.42/24\n";
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system domifaddr hbird-cp1",
        Reply::Ok(out.to_string()),
    );
    let conn = make_conn(stub);
    let ip = conn.domifaddr("hbird-cp1").expect("ok").expect("some");
    assert_eq!(ip.to_string(), "192.168.122.42");
}

#[test]
fn domifaddr_returns_none_when_no_lease() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system domifaddr hbird-cp1",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(stub);
    assert_eq!(conn.domifaddr("hbird-cp1").unwrap(), None);
}

#[test]
fn domifaddr_shell_quotes_dangerous_domain_names() {
    // A domain name with shell metacharacters must be quoted so the
    // remote sh -c can't be tricked into running it. (Defensive — the
    // cluster.local.conf parser already constrains CP_NAME, but we
    // don't want to rely on every caller knowing that.)
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system domifaddr 'evil; rm -rf /'",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(Arc::clone(&stub));
    conn.domifaddr("evil; rm -rf /").unwrap();
    let calls = stub.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].1.contains("'evil; rm -rf /'"),
        "command should single-quote dangerous chars: {:?}",
        calls[0].1
    );
}

#[test]
fn dominfo_parses_expected_fields() {
    let stub = Arc::new(StubSshClient::new());
    let out = "Id:             7\n\
               Name:           hbird-w1\n\
               UUID:           00112233-4455-6677-8899-aabbccddeeff\n\
               OS Type:        hvm\n\
               State:          running\n\
               CPU(s):         2\n\
               Persistent:     yes\n\
               Autostart:      disable\n";
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system dominfo hbird-w1",
        Reply::Ok(out.to_string()),
    );
    let conn = make_conn(stub);
    let info = conn.dominfo("hbird-w1").expect("ok");
    assert_eq!(info.name, "hbird-w1");
    assert_eq!(info.state, "running");
    assert_eq!(info.os_type, "hvm");
    assert!(info.persistent);
}

#[test]
fn dominfo_surfaces_missing_domain_as_virshfailed() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system dominfo nonexistent",
        Reply::NonZero {
            stderr: "error: failed to get domain 'nonexistent'\n".to_string(),
            exit_code: 1,
        },
    );
    let conn = make_conn(stub);
    let err = conn.dominfo("nonexistent").expect_err("must fail");
    match err {
        Error::VirshFailed { stderr, .. } => assert!(stderr.contains("failed to get domain")),
        other => panic!("expected VirshFailed, got {other:?}"),
    }
}

#[test]
fn no_user_uri_skips_user_prefix_in_ssh_target() {
    let stub = Arc::new(StubSshClient::new());
    // No user — ssh_target is just the host.
    stub.expect(
        "kvm.example",
        "virsh -c qemu:///system list --all --name",
        Reply::Ok(String::new()),
    );
    let uri = QemuSshUri::parse("qemu+ssh://kvm.example/system").unwrap();
    let conn = Connection::new(uri, Arc::clone(&stub) as Arc<dyn SshClient>);
    conn.domains().unwrap();
    let calls = stub.calls();
    assert_eq!(calls[0].0, "kvm.example");
}

#[test]
fn session_instance_routes_to_qemu_session_uri() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "kvm.example",
        "virsh -c qemu:///session list --all --name",
        Reply::Ok(String::new()),
    );
    let uri = QemuSshUri::parse("qemu+ssh://kvm.example/session").unwrap();
    let conn = Connection::new(uri, stub);
    conn.domains().unwrap();
}

// =============================================================================
// PR #337 round-2 — coverage for the 5 new Connection methods + the
// `reject_destructive_path` safety guard. Round-2 lens L5 HIGH flagged
// that these landed without StubSshClient-pinned tests.
// =============================================================================

#[test]
fn destroy_domain_emits_virsh_destroy_with_quoted_name() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system destroy hbird-cp1",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(Arc::clone(&stub));
    conn.destroy_domain("hbird-cp1").expect("ok");
    let calls = stub.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "virsh -c qemu:///system destroy hbird-cp1");
}

#[test]
fn destroy_domain_quotes_hostile_domain_name() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system destroy 'evil; rm -rf /'",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(Arc::clone(&stub));
    conn.destroy_domain("evil; rm -rf /").expect("ok");
    let calls = stub.calls();
    assert_eq!(calls.len(), 1);
    // Verify the metachar payload was single-quoted (no bare ;)
    assert!(
        calls[0].1.contains("'evil; rm -rf /'"),
        "expected single-quoted payload. cmd: {}",
        calls[0].1
    );
}

#[test]
fn undefine_domain_emits_virsh_undefine_with_nvram() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "virsh -c qemu:///system undefine --nvram hbird-w1",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(Arc::clone(&stub));
    conn.undefine_domain("hbird-w1").expect("ok");
    let calls = stub.calls();
    assert_eq!(
        calls[0].1,
        "virsh -c qemu:///system undefine --nvram hbird-w1"
    );
}

#[test]
fn remote_rm_f_emits_rm_f_with_quoted_path() {
    let stub = Arc::new(StubSshClient::new());
    // Paths contain `/` which shell_quote's allowlist (`[A-Za-z0-9-_.]`)
    // doesn't include, so they get single-quoted defensively.
    stub.expect(
        "op@kvm.example",
        "rm -f -- '/mnt/mass2/vms/hbird-cp1.qcow2'",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(Arc::clone(&stub));
    conn.remote_rm_f("/mnt/mass2/vms/hbird-cp1.qcow2")
        .expect("ok");
    let calls = stub.calls();
    assert_eq!(calls[0].1, "rm -f -- '/mnt/mass2/vms/hbird-cp1.qcow2'");
}

#[test]
fn remote_rm_rf_emits_rm_rf_with_quoted_path() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "rm -rf -- '/mnt/mass2/vms/deploy-cluster'",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(Arc::clone(&stub));
    conn.remote_rm_rf("/mnt/mass2/vms/deploy-cluster")
        .expect("ok");
    let calls = stub.calls();
    assert_eq!(calls[0].1, "rm -rf -- '/mnt/mass2/vms/deploy-cluster'");
}

// ---- reject_destructive_path guard tests (round-2 lens L5#H2 + L1 MED) ----

#[test]
fn remote_rm_rf_refuses_root_slash() {
    let stub = Arc::new(StubSshClient::new());
    let conn = make_conn(Arc::clone(&stub));
    let err = conn.remote_rm_rf("/").expect_err("should refuse /");
    assert!(
        err.to_string().contains("top-level system directory")
            || err.to_string().contains("destructive"),
        "expected destructive-path refusal. err: {err}"
    );
    // CRITICAL: no SSH call should have been made.
    assert_eq!(stub.calls().len(), 0, "remote_rm_rf('/') must not call ssh");
}

#[test]
fn remote_rm_rf_refuses_top_level_dirs() {
    let stub = Arc::new(StubSshClient::new());
    let conn = make_conn(Arc::clone(&stub));
    for banned in ["/etc", "/home", "/var", "/usr", "/root", "/boot"] {
        let err = conn.remote_rm_rf(banned).expect_err(banned);
        assert!(
            err.to_string().contains("top-level system directory"),
            "expected refusal for {banned}. err: {err}"
        );
    }
    assert_eq!(stub.calls().len(), 0);
}

#[test]
fn remote_rm_rf_refuses_empty_path() {
    let stub = Arc::new(StubSshClient::new());
    let conn = make_conn(Arc::clone(&stub));
    let err = conn.remote_rm_rf("").expect_err("should refuse empty");
    assert!(err.to_string().contains("empty path"), "err: {err}");
    assert_eq!(stub.calls().len(), 0);
}

#[test]
fn remote_rm_rf_refuses_relative_path() {
    let stub = Arc::new(StubSshClient::new());
    let conn = make_conn(Arc::clone(&stub));
    let err = conn
        .remote_rm_rf("relative/path")
        .expect_err("should refuse relative");
    assert!(
        err.to_string().contains("non-absolute") || err.to_string().contains("must start with /"),
        "err: {err}"
    );
    assert_eq!(stub.calls().len(), 0);
}

#[test]
fn remote_rm_rf_refuses_dotdot_segments() {
    let stub = Arc::new(StubSshClient::new());
    let conn = make_conn(Arc::clone(&stub));
    let err = conn
        .remote_rm_rf("/mnt/mass2/../../../../etc")
        .expect_err("should refuse ..");
    assert!(err.to_string().contains(".."), "err: {err}");
    assert_eq!(stub.calls().len(), 0);
}

#[test]
fn remote_rm_rf_allows_pool_dir_subdir() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "rm -rf -- '/mnt/mass2/vms/deploy-cluster'",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(Arc::clone(&stub));
    // Legitimate caller path — guard must NOT trip.
    conn.remote_rm_rf("/mnt/mass2/vms/deploy-cluster")
        .expect("legitimate POOL_DIR subdir should be allowed");
}

// ---- remote_path_exists exit-code translation (round-2 lens L5 HIGH) ----

#[test]
fn remote_path_exists_returns_true_on_exit_0() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "test -e '/mnt/mass2/vms/hbird-cp1.qcow2'",
        Reply::Ok(String::new()),
    );
    let conn = make_conn(Arc::clone(&stub));
    assert!(
        conn.remote_path_exists("/mnt/mass2/vms/hbird-cp1.qcow2")
            .unwrap()
    );
}

#[test]
fn remote_path_exists_returns_false_on_exit_1() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "test -e '/mnt/mass2/vms/missing.qcow2'",
        Reply::NonZero {
            stderr: String::new(),
            exit_code: 1,
        },
    );
    let conn = make_conn(Arc::clone(&stub));
    assert!(
        !conn
            .remote_path_exists("/mnt/mass2/vms/missing.qcow2")
            .unwrap()
    );
}

#[test]
fn remote_path_exists_propagates_transport_error() {
    let stub = Arc::new(StubSshClient::new());
    stub.expect(
        "op@kvm.example",
        "test -e '/some/path'",
        Reply::Transport("connection refused".to_string()),
    );
    let conn = make_conn(Arc::clone(&stub));
    let err = conn
        .remote_path_exists("/some/path")
        .expect_err("transport failures must propagate, not collapse to Ok(false)");
    assert!(
        err.to_string().contains("connection refused") || err.to_string().contains("transport"),
        "expected transport error chain preserved. err: {err}"
    );
}
