#!/usr/bin/env bash
# Copy to config.local.sh (gitignored) and adjust per-host. Sourced by scripts/build-*.sh.

# Initial user account inside the VM.
# export VM_USER=core
# export VM_USER_GROUPS=                # empty = no sudo via wheel (default)
                                        # set "wheel" if you want the user able to sudo
# export VM_PASSWORD=                   # empty = SSH-key-only auth (default; recommended)
export ENABLE_ROOT_SSH=1                # 1 = bake same pubkeys into root@ (default).
                                        # 0 = disable root SSH entirely.
                                        # PermitRootLogin prohibit-password drop-in is
                                        # baked into the image either way.

# Pubkey files to embed. Colon-separated.
# export SSH_PUBKEY_FILES=~/.ssh/id_ed25519.pub:~/.ssh/id_rsa.pub

# Additionally (or alternatively) fetch pubkeys from GitHub user profiles.
# Comma-separated usernames; each https://github.com/<user>.keys is fetched and
# appended to the embedded authorized_keys (deduped against the file set).
# Set SSH_PUBKEY_FILES= (empty) to use only GitHub keys.
# A 404 / bad username fails the build — see docs/ssh-keys.md.
# export SSH_PUBKEY_GH_USERS=aatchison,otheruser

# libvirt storage pool target directory on this host.
# export POOL_DIR=/var/lib/libvirt/images

# ---- VM resource knobs (#91) --------------------------------------------------
# Per-flavor virt-install --memory / --vcpus values. Defaults match the
# pre-knob hardcoded sizes, so unset = unchanged behavior. Override here
# for resource-constrained labs or larger node sizing.
#
# Control plane (scripts/define-vm-k8s.sh):
# export CP_MEMORY=8192          # MiB
# export CP_VCPUS=4
#
# k3s single-node (scripts/define-vm.sh):
# export K3S_MEMORY=6144
# export K3S_VCPUS=4
#
# Workers (scripts/spawn-workers.sh, per worker):
# export WORKER_MEMORY=4096
# export WORKER_VCPUS=2

# ---- spawn-workers retry tuning (#90) ----------------------------------------
# When chaining redo-k8s.sh -> redo-workers.sh, the CP may still be coming
# up the first time spawn-workers.sh SSHes in to mint a join token. The
# script retries CP_SSH_RETRIES times, sleeping CP_SSH_RETRY_SLEEP seconds
# between attempts (each attempt has ConnectTimeout=10s).
# export CP_SSH_RETRIES=5
# export CP_SSH_RETRY_SLEEP=10

# For the upstream-k8s flavor: extra SANs to bake into the apiserver cert.
# Add the hostname/IP of any client that will hit the cluster via SSH tunnel.
# export APISERVER_EXTRA_SANS=127.0.0.1,localhost

# For scripts/kubectl-k8s.sh (run from a client): the KVM host SSH alias to tunnel through.
# export KVM_HOST=kvm.example.com

# ---- Restoring pre-#17 "classic lab" defaults ---------------------------------
# Older Hummingbird images shipped with a wheel-capable, password-authenticatable
# user. PR #17 flipped both defaults off. To get the old behavior back:
# export VM_USER_GROUPS=wheel
# export VM_PASSWORD=changeme
