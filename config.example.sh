#!/usr/bin/env bash
# Copy to config.local.sh (gitignored) and adjust per-host.
#
# Sourced by scripts/build-k8s.sh and scripts/build-worker.sh (and the
# `image-*` Makefile targets that delegate to them) when
# HBIRD_AUTOLOAD_CONFIG_LOCAL=1 is set. This tunes image-build inputs only;
# cluster-topology knobs (CP_NAME, WORKER_NAMES, IMAGE_SOURCE, etc.) live in
# cluster.local.conf (see cluster.example.conf) and are consumed by
# scripts/deploy-cluster.sh / `hbird update-cluster` (Rust twin, post-#353).

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

# VM-sizing knobs (CP_MEMORY / CP_VCPUS / WORKER_MEMORY / WORKER_VCPUS) live in
# cluster.local.conf — see cluster.example.conf. They no longer belong here
# (the `define-vm*.sh` consumers were removed in #216).

# ---- spawn-workers retry tuning (#90) ----------------------------------------
# When deploy-cluster.sh / spawn-workers.sh comes up right after the CP boots,
# the CP may still be initializing the first time the script SSHes in to mint
# a join token. The script retries CP_SSH_RETRIES times, sleeping
# CP_SSH_RETRY_SLEEP seconds between attempts (each attempt has
# ConnectTimeout=10s).
# export CP_SSH_RETRIES=5
# export CP_SSH_RETRY_SLEEP=10

# For the upstream-k8s flavor: extra SANs to bake into the apiserver cert.
# Add the hostname/IP of any client that will hit the cluster via SSH tunnel.
# export APISERVER_EXTRA_SANS=127.0.0.1,localhost

# For `hbird kubectl` (run from a client; Rust twin of the removed
# scripts/kubectl-k8s.sh, v0.1.0 cutover #353): the KVM host SSH alias
# to tunnel through.
# export KVM_HOST=kvm.example.com

# ---- Restoring pre-#17 "classic lab" defaults ---------------------------------
# Older Hummingbird images shipped with a wheel-capable, password-authenticatable
# user. PR #17 flipped both defaults off. To get the old behavior back:
# export VM_USER_GROUPS=wheel
# export VM_PASSWORD=changeme

# ---- Optional cloud-init in the built image -----------------------------------
# Default OFF — the canonical Hummingbird customization path is the
# `[[customizations.user]]` blocks rendered into bib-config.toml at qcow2
# build time. Set ENABLE_CLOUD_INIT=1 to additionally bake the cloud-init
# package + NoCloud datasource into the image so operators can inject
# per-VM user-data (SSH keys, runcmd, packages) at `virt-install` time
# via `--cloud-init` or a libvirt seed ISO. See docs/cloud-init.md.
#
# Note: deploy-cluster.sh requires ENABLE_CLOUD_INIT=1 in cluster.local.conf;
# this setting controls the standalone image-* build targets only.
# export ENABLE_CLOUD_INIT=1
