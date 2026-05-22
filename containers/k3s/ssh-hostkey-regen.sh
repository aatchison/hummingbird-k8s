#!/bin/bash
# Regenerate SSH host keys on first boot so two k3s VMs built from the same
# image don't share identical host keys (#80). Idempotent via the marker file.
set -euo pipefail

MARKER=/var/lib/ssh-host-keys-regenerated
[[ -f "$MARKER" ]] && exit 0

rm -f /etc/ssh/ssh_host_*
ssh-keygen -A
systemctl restart sshd
touch "$MARKER"
