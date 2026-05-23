#!/usr/bin/env bash
# Shared helpers for build-*.sh and scripts/* shell scripts. Source it; don't
# run it directly.
#
# See docs/development.md and scripts/export-argocd.sh for the canonical
# pattern (source the lib + setup_logging + derive_ssh_privkey_file +
# ssh_opts_array). The "Adding a new shared helper" subsection in
# docs/development.md covers the two-test rule for additions.
#
# ──────────────────────────────────────────────────────────────────────────────
# Section: build-only helpers (bib + qcow2)
# Section: shared SSH / virsh / log helpers (issue #190)
# ──────────────────────────────────────────────────────────────────────────────
#
# Shared helpers (issue #190) — extracted from five callers
# (deploy-cluster, destroy-cluster, update-cluster, export-argocd,
# verify-hardening) that each hand-rolled the same primitives. Contracts:
#
#   setup_logging <prefix>
#       Defines log() and fail() in the caller's scope. log() prints
#       "<prefix> <msg>" to stderr; fail() prints "<prefix> ERROR: <msg>"
#       to stderr and exits 1. <prefix> is taken verbatim — pass it with
#       brackets if you want them, e.g. setup_logging '[deploy-cluster]'.
#       Contract: <prefix> MUST NOT contain `%`, `'`, or `\` — it is
#       interpolated into an eval'd function body. Bracketed labels like
#       "[deploy-cluster]" are safe; arbitrary user input is not.
#
#   ssh_opts_array <out_array_name> [--with-controlmaster] [--proxy-jump=HOST]
#       Populates the caller's array with the canonical SSH option set:
#         -i $SSH_PRIVKEY_FILE
#         -o StrictHostKeyChecking=no
#         -o UserKnownHostsFile=/dev/null
#         -o LogLevel=ERROR
#         -o ConnectTimeout=10
#         -o BatchMode=yes
#       With --with-controlmaster, appends ControlMaster/ControlPath/
#       ControlPersist for SSH multiplexing across many invocations
#       (used by update-cluster.sh which makes ~6-12 ssh calls per node).
#       With --proxy-jump=HOST, appends `-o ProxyJump=HOST` (used by
#       verify-hardening.sh tunneling via the KVM host).
#       Requires SSH_PRIVKEY_FILE to be set in the caller's env.
#       Variant: ssh_opts_array_no_identity <out_array_name> [flags]
#       Omits the `-i` line — used by verify-hardening which relies on
#       agent auth or ~/.ssh/config.
#
#   resolve_vm_ip <vm-name> [attempts] [interval-seconds]
#       Echoes the first IPv4 from `virsh -c qemu:///system domifaddr
#       <vm>` to stdout. Defaults: 1 attempt, 0s interval (no retry).
#       Caller can pass e.g. `resolve_vm_ip $vm 60 5` for the
#       deploy-cluster wait-loop (DHCP lease may take ~minutes).
#       Probes virsh up front and emits a distinct error if missing.
#       Warns on stderr if the VM reports multiple IPv4 addresses
#       (multi-NIC ambiguity; first is returned).
#       Returns non-zero with a diagnostic on stderr if no IP appears.
#
#   derive_ssh_privkey_file <pubkey-path>
#       Echoes "${pubkey%.pub}" — the conventional private-key path next
#       to the pubkey. Hard-fails (rc=2) if the input does not end in
#       ".pub" (so a typo or already-private path is rejected loudly
#       instead of silently feeding ssh a wrong identity). Also errors
#       if the resolved private key is unreadable.
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
#                       May be set to empty if SSH_PUBKEY_GH_USERS provides keys.
#   SSH_PUBKEY_GH_USERS — Comma-separated GitHub usernames. For each, fetch
#                       https://github.com/<user>.keys and append to the embedded
#                       authorized_keys (deduped against the file-based set).
#                       Default: empty (opt-in). A bad/404 username fails the build.
#   POOL_DIR          — libvirt storage pool target directory.
#                       Default: /var/lib/libvirt/images
#   BASE_IMAGE        — Upstream bootc-os base. Pinned by digest (OCI index,
#                       multi-arch) for reproducibility. Override only when
#                       deliberately bumping the base; keep it in lockstep with
#                       the `FROM` lines in containers/*/Containerfile.
#   BIB               — bootc-image-builder image.
#                       Default: quay.io/centos-bootc/bootc-image-builder:latest
#   ENABLE_CLOUD_INIT — "1" to opt into cloud-init in the built image (see
#                       docs/cloud-init.md). Default "0" — the resulting
#                       image is byte-identical to pre-cloud-init builds.
#                       Threaded through to `podman build` as a build-arg
#                       by each containers/*/Containerfile's conditional
#                       dnf install + preset block.

set -euo pipefail

# Optional opt-in autoload of a repo-local config.local.sh. Gated behind
# HBIRD_AUTOLOAD_CONFIG_LOCAL=1 so only the build-* scripts that have
# always relied on it inherit the file's side-effects (it can `export`
# arbitrary env or even run arbitrary commands — sourcing it from every
# orchestrator script would widen the trust boundary unnecessarily).
# Documented in docs/development.md.
_HC_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -r "${_HC_REPO_ROOT}/config.local.sh" && "${HBIRD_AUTOLOAD_CONFIG_LOCAL:-0}" == "1" ]]; then
  # shellcheck disable=SC1091
  source "${_HC_REPO_ROOT}/config.local.sh"
fi

: "${VM_USER:=core}"
: "${VM_USER_GROUPS:=}"
: "${ENABLE_ROOT_SSH:=1}"
: "${POOL_DIR:=/var/lib/libvirt/images}"
: "${BASE_IMAGE:=quay.io/hummingbird-community/bootc-os@sha256:3bed2fc1bd96ad56a3e4357270ff0f22286fb41c9e00b4f3c9a862696e3bfb84}"
: "${BIB:=quay.io/centos-bootc/bootc-image-builder:latest}"
: "${ENABLE_CLOUD_INIT:=0}"

# Require root for the build (bootc-image-builder needs --privileged + loopback mounts).
require_root() {
  if [[ $EUID -ne 0 ]]; then
    echo "${0##*/}: must be run as root (bootc-image-builder needs --privileged + loopback mounts; libvirt qemu:///system needs root). Try: sudo bash $0" >&2
    exit 1
  fi
  : "${SUDO_USER:?must be invoked via sudo so the qcow2 can be chowned to the calling user}"
}

# Resolve the list of pubkey files to embed. Honors an explicitly-empty
# SSH_PUBKEY_FILES (= no files; expect SSH_PUBKEY_GH_USERS to provide keys).
ssh_pubkey_files() {
  if [[ -n "${SSH_PUBKEY_FILES+x}" ]]; then
    # Set (possibly empty). Empty → emit nothing.
    if [[ -n "$SSH_PUBKEY_FILES" ]]; then
      tr ':' '\n' <<<"$SSH_PUBKEY_FILES"
    fi
  else
    # Resolve the SUDO_USER's actual home dir via getent — root's home is
    # /root, not /home/root, and operators with non-standard homes shouldn't
    # have to override SSH_PUBKEY_FILES just to use the default key.
    local home
    home="$(getent passwd "${SUDO_USER}" 2>/dev/null | cut -d: -f6)"
    [[ -n "$home" ]] || home="/home/${SUDO_USER}"
    echo "${home}/.ssh/id_ed25519.pub"
  fi
}

# Fetch pubkeys from each GitHub username in SSH_PUBKEY_GH_USERS (comma-separated).
# Uses curl -fsSL so a bad/404 username fails the whole build — better to fail
# loud than to silently embed the wrong set of keys.
ssh_pubkeys_from_github() {
  [[ -n "${SSH_PUBKEY_GH_USERS:-}" ]] || return 0
  local u
  for u in ${SSH_PUBKEY_GH_USERS//,/ }; do
    # Tolerate trailing comma / whitespace; skip empty tokens.
    u="${u// /}"
    [[ -z "$u" ]] && continue
    curl -fsSL "https://github.com/${u}.keys"
  done
}

# Concatenate file-based pubkeys with GitHub-sourced pubkeys into one
# newline-separated blob. File keys come first; GitHub keys are appended.
# Duplicate non-empty lines are removed while preserving first-seen order —
# `sort -u` would scramble ordering, which makes diffs noisier than they need
# to be.
ssh_pubkey_blob() {
  {
    local f
    while read -r f; do
      [[ -r "$f" ]] || { echo "ssh_pubkey_blob: pubkey file not readable: $f (set SSH_PUBKEY_FILES, or place ~/.ssh/id_ed25519.pub for SUDO_USER='${SUDO_USER:-<unset>}')" >&2; exit 1; }
      cat "$f"
    done < <(ssh_pubkey_files)
    ssh_pubkeys_from_github
  } | awk '!NF || !seen[$0]++'
}

# Render a single [[customizations.user]] block. $1 = username, $2 = include password (0/1).
_render_user_block() {
  local name="$1" want_pw="$2" keys
  keys="$(ssh_pubkey_blob)"

  printf '[[customizations.user]]\n'
  printf 'name = "%s"\n' "$name"
  if [[ "$want_pw" = 1 && -n "${VM_PASSWORD:-}" ]]; then
    printf 'password = "%s"\n' "$(openssl passwd -6 "$VM_PASSWORD")"
  fi
  if [[ -n "$VM_USER_GROUPS" && "$name" != "root" ]]; then
    # comma-separated → TOML array of quoted strings. Trim whitespace around each
    # token, and strip trailing newlines so we never emit `"wheel\n"`.
    local groups
    groups="$(awk -v RS=, '{
                gsub(/^[ \t\n]+|[ \t\n]+$/, "")
                printf "%s\"%s\"", (NR>1?", ":""), $0
              }' <<<"$VM_USER_GROUPS" | tr -d '\n')"
    printf 'groups = [%s]\n' "$groups"
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
  local stage="${POOL_DIR}/qcow2"
  local disk="${stage}/disk.qcow2"

  # Validate inputs up front so failures point at the real cause (issue #98 / #115).
  if [[ -z "$local_image" || -z "$name" || -z "$cfg" ]]; then
    echo "build_qcow2: usage: build_qcow2 <local-image-ref> <qcow2-name> <path-to-bib-config.toml>" >&2
    echo "build_qcow2: got local_image='${local_image}' name='${name}' cfg='${cfg}'" >&2
    return 2
  fi
  if [[ ! -r "$cfg" ]]; then
    echo "build_qcow2: bib config not readable: ${cfg}" >&2
    echo "build_qcow2: render one first via 'render_bib_config > bib-config.toml' (see lib/build-common.sh)." >&2
    return 2
  fi

  rm -rf "$stage"
  mkdir -p "$POOL_DIR"

  if ! podman run --rm --privileged --pull=newer \
    --security-opt label=type:unconfined_t \
    -v "${cfg}:/config.toml:ro" \
    -v "${POOL_DIR}:/output" \
    -v /var/lib/containers/storage:/var/lib/containers/storage \
    "$BIB" \
    --type qcow2 --rootfs ext4 \
    --local \
    "$local_image"; then
    echo "build_qcow2: bootc-image-builder (${BIB}) failed for image '${local_image}'." >&2
    echo "build_qcow2: common causes: missing --privileged, loopback mounts blocked, or the local image ref does not exist in podman storage. Re-run 'podman images' to confirm." >&2
    return 1
  fi

  # Promote the staged disk to its final path. Each step is explicitly checked so
  # that a failure cannot silently leave stale state in $stage that would poison
  # subsequent builds (issue #103).
  if [[ ! -f "$disk" ]]; then
    echo "build_qcow2: expected $disk to exist after bib run (image='${local_image}', stage='${stage}')" >&2
    echo "build_qcow2: bib appears to have exited 0 but produced no disk — inspect '${stage}' for partial output." >&2
    return 1
  fi
  if ! mv -f "$disk" "$qcow"; then
    echo "build_qcow2: failed to move $disk to $qcow (check ${POOL_DIR} permissions and free space: $(df -h "$POOL_DIR" 2>/dev/null | tail -1))" >&2
    return 1
  fi
  if ! rmdir "$stage"; then
    echo "build_qcow2: failed to remove staging dir $stage (non-empty? ls: $(ls -A "$stage" 2>/dev/null | head -5 | tr '\n' ' '))" >&2
    return 1
  fi
  chown root:root "$qcow"
  chmod 0644 "$qcow"

  # Pool refresh is best-effort — pool may not be defined under any name we know.
  virsh -c qemu:///system pool-refresh default >/dev/null 2>&1 || true
  virsh -c qemu:///system pool-refresh mass2   >/dev/null 2>&1 || true

  echo "Built: $qcow"
}

# ──────────────────────────────────────────────────────────────────────────────
# Shared SSH / virsh / log helpers (issue #190)
# ──────────────────────────────────────────────────────────────────────────────

# setup_logging <prefix> — define log()/fail() in the caller's scope with the
# given prefix. <prefix> is emitted verbatim (caller decides whether to wrap it
# in brackets, colons, etc). Output goes to stderr.
#
# Contract: <prefix> MUST NOT contain `%`, `'`, or `\` — it is spliced
# verbatim into a printf format string inside an eval'd function body.
# Bracketed labels like "[deploy-cluster]" are safe; arbitrary user
# input is not. Rewriting setup_logging without eval is tracked as a
# follow-up — out of scope for the issue #190 extraction.
setup_logging() {
  local _prefix="${1:?setup_logging requires a prefix (e.g. setup_logging \"[deploy-cluster]\")}"
  # Stash the prefix in a function-local global so each subsequent call to
  # setup_logging in the same process redefines log/fail with the new prefix.
  # We don't try to support "nested" loggers — callers only need one.
  eval "
    log()  { printf '${_prefix} %s\\n' \"\$*\" >&2; }
    fail() { printf '${_prefix} ERROR: %s\\n' \"\$*\" >&2; exit 1; }
  "
}

# _ssh_opts_array_impl <out_array_name> <include_identity:0|1> [flags...]
# Internal helper. Callers should use ssh_opts_array or
# ssh_opts_array_no_identity. Recognized flags:
#   --with-controlmaster      Add ControlMaster=auto + ControlPath + ControlPersist
#   --proxy-jump=HOST         Add -o ProxyJump=HOST
_ssh_opts_array_impl() {
  local _out="$1" _with_identity="$2"
  shift 2
  local _with_cm=0 _proxy_jump=""
  local _arg
  for _arg in "$@"; do
    case "$_arg" in
      --with-controlmaster) _with_cm=1 ;;
      --proxy-jump=*)       _proxy_jump="${_arg#--proxy-jump=}" ;;
      *) echo "ssh_opts_array: unknown flag '$_arg'" >&2; return 2 ;;
    esac
  done

  # Build the option list, then assign to the caller's array in one shot.
  local _opts=()
  if (( _with_identity == 1 )); then
    : "${SSH_PRIVKEY_FILE:?ssh_opts_array requires SSH_PRIVKEY_FILE to be set}"
    _opts+=( -i "$SSH_PRIVKEY_FILE" )
  fi
  _opts+=(
    -o StrictHostKeyChecking=no
    -o UserKnownHostsFile=/dev/null
    -o LogLevel=ERROR
    -o ConnectTimeout=10
    -o BatchMode=yes
  )
  if (( _with_cm == 1 )); then
    # ControlPath includes $UID so two operators on the same KVM host
    # running `make update-cluster` against the same remote do not
    # collide on the multiplex socket. Renamed from hbird-update-* now
    # that the helper is generic (not update-cluster-specific).
    _opts+=(
      -o ControlMaster=auto
      -o "ControlPath=/tmp/hbird-ssh-${UID}-%r@%h:%p"
      -o ControlPersist=60s
    )
  fi
  if [[ -n "$_proxy_jump" ]]; then
    _opts+=( -o "ProxyJump=${_proxy_jump}" )
  fi

  # Assign — use eval so the indirection works with arbitrary array names
  # (printf -v doesn't support arrays in bash <5.1).
  eval "${_out}=(\"\${_opts[@]}\")"
}

# ssh_opts_array <out_array_name> [--with-controlmaster] [--proxy-jump=HOST]
# Build the canonical SSH option array (with -i SSH_PRIVKEY_FILE).
ssh_opts_array() {
  local _out="${1:?ssh_opts_array requires an output array name}"
  shift
  _ssh_opts_array_impl "$_out" 1 "$@"
}

# ssh_opts_array_no_identity <out_array_name> [--with-controlmaster] [--proxy-jump=HOST]
# Same as ssh_opts_array but omits the `-i` line — for callers that
# rely on agent auth or ~/.ssh/config (e.g. verify-hardening.sh).
ssh_opts_array_no_identity() {
  local _out="${1:?ssh_opts_array_no_identity requires an output array name}"
  shift
  _ssh_opts_array_impl "$_out" 0 "$@"
}

# resolve_vm_ip <vm-name> [attempts] [interval-seconds]
# Echo the first IPv4 from `virsh -c qemu:///system domifaddr <vm>`.
# Defaults: 1 attempt, 0s interval (no retry). Pass higher attempts for
# the deploy-cluster boot-wait scenario (DHCP lease may take ~minutes).
# Probes virsh up front; warns on stderr when the VM exposes multiple
# IPv4 addresses (multi-NIC: first is returned). Returns non-zero with
# a diagnostic on stderr if no IP appears.
resolve_vm_ip() {
  local vm="${1:?resolve_vm_ip requires a VM name}"
  local attempts="${2:-1}" interval="${3:-0}"
  local attempt ip="" raw="" ipv4_count

  if ! command -v virsh >/dev/null 2>&1; then
    echo "resolve_vm_ip: virsh not installed or not on PATH; cannot resolve domain '${vm}'. Install libvirt-client or invoke from the KVM host." >&2
    return 1
  fi

  for (( attempt = 1; attempt <= attempts; attempt++ )); do
    raw="$(virsh -c qemu:///system domifaddr "$vm" 2>/dev/null || true)"
    ip="$(printf '%s\n' "$raw" \
      | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}')"
    if [[ -n "$ip" ]]; then
      # Multi-NIC ambiguity: emit a warning but still return the first
      # IP. A `--bridge` filter would be the proper fix; tracked as a
      # follow-up. Order is whatever `virsh domifaddr` prints, which is
      # not guaranteed stable across libvirt versions.
      ipv4_count="$(printf '%s\n' "$raw" | awk '/ipv4/' | wc -l)"
      if (( ipv4_count > 1 )); then
        echo "resolve_vm_ip: WARNING: domain '${vm}' has ${ipv4_count} IPv4 addresses; returning the first ('${ip}'). Multi-NIC VMs may surface a different NIC's IP across runs." >&2
      fi
      printf '%s\n' "$ip"
      return 0
    fi
    if (( attempt < attempts )) && (( interval > 0 )); then
      sleep "$interval"
    fi
  done

  echo "resolve_vm_ip: could not resolve IPv4 for domain '${vm}' via 'virsh -c qemu:///system domifaddr' after ${attempts} attempt(s). The DHCP lease may not be ready yet, or the domain may be powered off." >&2
  return 1
}

# derive_ssh_privkey_file <pubkey-path>
# Echo the conventional private-key path next to the pubkey. Hard-fails
# (rc=2) if the input does not end in ".pub" — otherwise `${pub%.pub}`
# would silently return the input unchanged, the readability check would
# pass (since the input WAS readable), and the caller would feed ssh the
# wrong identity, producing a confusing auth failure downstream.
# Also errors (rc=1) if the resolved private key is unreadable.
derive_ssh_privkey_file() {
  local pub="${1:?derive_ssh_privkey_file requires a public key path}"
  if [[ "$pub" != *.pub ]]; then
    echo "derive_ssh_privkey_file: SSH public key path must end in .pub (got: ${pub}). Pass the public-key path; the private key is derived by stripping .pub." >&2
    return 2
  fi
  local priv="${pub%.pub}"
  if [[ ! -r "$priv" ]]; then
    echo "derive_ssh_privkey_file: SSH private key not readable: ${priv} (expected next to ${pub})" >&2
    return 1
  fi
  printf '%s\n' "$priv"
}
