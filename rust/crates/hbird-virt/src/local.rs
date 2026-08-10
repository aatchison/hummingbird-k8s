//! Local libvirt transport — run virsh/cp/virt-install on this host via
//! `std::process` instead of routing through SSH.
//!
//! [`LocalClient`] implements [`crate::ssh::SshClient`] by spawning
//! `sh -c <command>` locally. The `host` argument is ignored — calls go
//! to the local machine's `qemu:///system` daemon.
//!
//! Used when `kvm_host` is absent or empty: the operator is running
//! `hbird` directly on the KVM host (i.e. Option A per #289 S2a decision).
//! The SSH-tunnelled path in [`crate::Connection`] is unchanged when
//! `kvm_host` is set.

use std::process::Command;

use crate::ssh::{SshClient, SshError};

/// Executes commands locally via `sh -c`, implementing the same
/// [`SshClient`] trait used by the SSH-tunnelled path.
///
/// The `host` argument to [`SshClient::run`] is ignored — every call
/// runs against the local machine.
pub struct LocalClient;

impl LocalClient {
    /// Create a new [`LocalClient`] that runs commands via `sh -c` on
    /// this machine.
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SshClient for LocalClient {
    fn run(&self, _host: &str, command: &str) -> Result<String, SshError> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .map_err(|e| SshError::Transport {
                host: "local".to_string(),
                message: format!("failed to spawn sh: {e}"),
            })?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(SshError::RemoteExit {
                host: "local".to_string(),
                command: command.to_string(),
                exit_code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }
}
