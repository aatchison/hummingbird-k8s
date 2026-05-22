#!/usr/bin/env bash
# Copy to config.local.sh (gitignored) and adjust per-host. Sourced by build-*.sh.

# Initial user account inside the VM.
# export VM_USER=core
# export VM_USER_GROUPS=                # empty = no sudo via wheel (default)
                                        # set "wheel" if you want the user able to sudo
# export VM_PASSWORD=                   # empty = SSH-key-only auth (default; recommended)
# export ENABLE_ROOT_SSH=1              # bake same pubkeys into root@; PermitRootLogin
                                        # prohibit-password drop-in is in the image already

# Pubkey files to embed. Colon-separated.
# export SSH_PUBKEY_FILES=~/.ssh/id_ed25519.pub:~/.ssh/id_rsa.pub

# libvirt storage pool target directory on this host.
# export POOL_DIR=/var/lib/libvirt/images

# For the upstream-k8s flavor: extra SANs to bake into the apiserver cert.
# Add the hostname/IP of any client that will hit the cluster via SSH tunnel.
# export APISERVER_EXTRA_SANS=127.0.0.1,localhost

# For kubectl-k8s.sh (run from a client): the KVM host SSH alias to tunnel through.
# export KVM_HOST=kvm.example.com
