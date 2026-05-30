//! Shared SSH→libvirt bridge used by deploy-cluster and destroy-cluster.
//!
//! [`CliSshBridge`] bridges [`hbird_virt::SshClient`] onto
//! [`hbird_ssh::Client`], allowing [`hbird_virt::Connection`] to tunnel its
//! `virsh`/`cp`/`virt-install`/`podman` calls over the operator-supplied
//! `--kvm-host` SSH alias.
//!
//! [`build_connection`] centralises the "SSH vs. local" selection logic that
//! was previously duplicated inline in `destroy_cluster.rs` and will be
//! needed identically in `deploy_cluster.rs`.

use std::sync::Arc;

use hbird_ssh::{Client, SshOptions};
use hbird_virt::ssh::{SshClient, SshError};
use hbird_virt::{Connection, Instance, QemuSshUri};

// ---- CliSshBridge ----------------------------------------------------------

/// Bridge that lets [`hbird_virt::Connection`] call out via
/// [`hbird_ssh::Client`].
///
/// `hbird-virt` defines its own `SshClient` trait (so the crate stays
/// dep-free of an SSH backend); `hbird-ssh::Client` provides the real
/// OpenSSH-subprocess implementation. This struct is the seam.
///
/// The `host` argument passed to [`SshClient::run`] by `hbird_virt` is
/// intentionally ignored: the operator-supplied `--kvm-host` value was
/// captured at construction time into the `SshOptions`, and every call must
/// go to that host regardless of what the embedded `qemu+ssh://` URI says.
pub(crate) struct CliSshBridge {
    inner: Client,
}

impl CliSshBridge {
    pub(crate) fn new(options: SshOptions) -> Self {
        Self {
            inner: Client::new(options),
        }
    }
}

impl SshClient for CliSshBridge {
    fn run(&self, _host: &str, command: &str) -> std::result::Result<String, SshError> {
        // Use the captured options' host rather than the `host` arg.
        // hbird-virt::Connection passes `self.uri.ssh_target()` here,
        // but for cluster commands the operator-supplied `--kvm-host`
        // (or the on-host empty case) is the authoritative target.
        match self.inner.run(command) {
            Ok(out) => Ok(out.stdout_lossy()),
            Err(hbird_ssh::Error::NonZeroExit {
                host,
                status,
                stderr,
                ..
            }) => Err(SshError::RemoteExit {
                host,
                command: command.to_string(),
                exit_code: status.code(),
                stderr,
            }),
            Err(e) => Err(SshError::Transport {
                host: self.inner.options().host().to_string(),
                message: e.to_string(),
            }),
        }
    }
}

// ---- build_connection ------------------------------------------------------

/// Build a [`Connection`] for the given optional KVM host alias.
///
/// - `Some(host)` with a non-empty string → SSH-tunnelled connection via
///   [`CliSshBridge`], targeting `qemu+ssh://<host>/system`.
/// - `None` or an empty string → local [`Connection::new_local`] (operator
///   is running `hbird` directly on the KVM host; Option A of #289 S2a).
pub(crate) fn build_connection(kvm_host: Option<&str>) -> Connection {
    if let Some(host) = kvm_host.filter(|s| !s.is_empty()) {
        let ssh_options = SshOptions::new(host.to_string());
        let uri = QemuSshUri {
            user: None,
            host: host.to_string(),
            port: None,
            instance: Instance::System,
            query: None,
        };
        let bridge: Arc<dyn SshClient> = Arc::new(CliSshBridge::new(ssh_options));
        Connection::new(uri, bridge)
    } else {
        Connection::new_local()
    }
}
