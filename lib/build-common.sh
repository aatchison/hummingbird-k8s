#!/usr/bin/env bash
# Shared helpers for build-*.sh scripts. Source it; don't run it directly.
#
# Environment variables (all optional, sensible defaults):
#   VM_USER           — Username of the initial account in the guest (default: core)
#   VM_USER_GROUPS    — Comma-separated groups for that user (default: empty — no wheel
#                       membership, no sudo. Set to "wheel" to opt back in.)
#   VM_PASSWORD       — Plaintext password for the initial user (default: unset = no password,
#                       key-only auth). Hashed with `openssl passwd -6` before embedding.
#   ENABLE_ROOT_SSH   — If "1" (default), bake the same pubkeys into root's authorized_keys
#                       and rely on PermitRootLogin=prohibit-password (set in the Containerfile)
#                       for admin access. Set to "0" to disable.
#   SSH_PUBKEY_FILES  — Colon-separated list of pubkey files to bake into authorized_keys.
#                       Default: ~/.ssh/id_ed25519.pub of the SUDO_USER.
#   POOL_DIR          — libvirt storage pool target directory.
#                       Default: /var/lib/libvirt/images
#   BASE_IMAGE        — Upstream bootc-os base.
#                       Default: quay.io/hummingbird-community/bootc-os:latest
#   BIB               — bootc-image-builder image.
#                       Default: quay.io/centos-bootc/bootc-image-builder:latest

set -euo pipefail

# A repo-local config.local.sh, if present, overrides any of the above. Gitignored.
_HC_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -r "${_HC_REPO_ROOT}/config.local.sh" ]]; then
  # shellcheck disable=SC1091
  source "${_HC_REPO_ROOT}/config.local.sh"
fi

: "${VM_USER:=core}"
: "${VM_USER_GROUPS:=}"
: "${ENABLE_ROOT_SSH:=1}"
: "${POOL_DIR:=/var/lib/libvirt/images}"
: "${BASE_IMAGE:=quay.io/hummingbird-community/bootc-os:latest}"
: "${BIB:=quay.io/centos-bootc/bootc-image-builder:latest}"

# Require root for the build (bootc-image-builder needs --privileged + loopback mounts).
require_root() {
  if [[ $EUID -ne 0 ]]; then
    echo "Run with sudo." >&2
    exit 1
  fi
  : "${SUDO_USER:?must be invoked via sudo so the qcow2 can be chowned to the calling user}"
}

# Resolve the list of pubkey files to embed.
ssh_pubkey_files() {
  if [[ -n "${SSH_PUBKEY_FILES:-}" ]]; then
    tr ':' '\n' <<<"$SSH_PUBKEY_FILES"
  else
    echo "/home/${SUDO_USER}/.ssh/id_ed25519.pub"
  fi
}

# Concatenate the pubkey file contents into one newline-separated blob.
ssh_pubkey_blob() {
  local f
  while read -r f; do
    [[ -r "$f" ]] || { echo "Pubkey file not readable: $f" >&2; exit 1; }
    cat "$f"
  done < <(ssh_pubkey_files)
}

# Render a single [[customizations.user]] block. $1 = username, $2 = include password (0/1).
_render_user_block() {
  local name="$1" want_pw="$2" keys groups
  keys="$(ssh_pubkey_blob)"

  printf '[[customizations.user]]\n'
  printf 'name = "%s"\n' "$name"
  if [[ "$want_pw" = 1 && -n "${VM_PASSWORD:-}" ]]; then
    printf 'password = "%s"\n' "$(openssl passwd -6 "$VM_PASSWORD")"
  fi
  if [[ -n "$VM_USER_GROUPS" && "$name" != "root" ]]; then
    # comma-separated → TOML array of quoted strings
    local groups
    groups="[$(awk -v RS=, '{printf "%s\"%s\"", (NR>1?", ":""), $0}' <<<"$VM_USER_GROUPS")]"
    printf 'groups = %s\n' "$groups"
  fi
  printf 'key = """%s"""\n' "$keys"
}

# Print a complete bib-config.toml on stdout. Always includes the VM_USER; also
# includes root if ENABLE_ROOT_SSH=1, sharing the same pubkeys.
render_bib_config() {
  _render_user_block "$VM_USER" 1
  if [[ "$ENABLE_ROOT_SSH" = 1 ]]; then
    printf '\n'
    _render_user_block root 0
  fi
}

# Run bib to turn a local OCI image into a qcow2 under $POOL_DIR/$NAME.qcow2
# Args: 1=local image ref, 2=qcow2 name (without .qcow2), 3=path to bib-config.toml
build_qcow2() {
  local local_image="$1" name="$2" cfg="$3"
  local qcow="${POOL_DIR}/${name}.qcow2"

  rm -rf "${POOL_DIR}/qcow2"
  mkdir -p "$POOL_DIR"

  podman run --rm --privileged --pull=newer \
    --security-opt label=type:unconfined_t \
    -v "${cfg}:/config.toml:ro" \
    -v "${POOL_DIR}:/output" \
    -v /var/lib/containers/storage:/var/lib/containers/storage \
    "$BIB" \
    --type qcow2 --rootfs ext4 \
    --local \
    "$local_image"

  mv -f "${POOL_DIR}/qcow2/disk.qcow2" "$qcow"
  rmdir "${POOL_DIR}/qcow2"
  chown root:root "$qcow"
  chmod 0644 "$qcow"

  # Pool refresh is best-effort — pool may not be defined under any name we know.
  virsh -c qemu:///system pool-refresh default >/dev/null 2>&1 || true
  virsh -c qemu:///system pool-refresh mass2   >/dev/null 2>&1 || true

  echo "Built: $qcow"
}
