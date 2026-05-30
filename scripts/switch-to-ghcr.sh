#!/usr/bin/env bash
# switch-to-ghcr.sh — Switch deployed Hummingbird VMs from their locally-built
# image reference (`localhost/hummingbird-<flavor>:latest`) to the GHCR-published
# equivalent (`ghcr.io/aatchison/hummingbird-<flavor>:latest`), so that the
# bootc-fetch-apply-updates timer (and any manual `bootc upgrade`) actually has
# a remote to pull from. See issue #138.
#
# Two modes:
#
#   * "all"     — default. Iterate every running `hummingbird-*` VM under
#                 qemu:///system and switch each one. Skip VMs that already
#                 track ghcr.
#
#   * "single VM" — for use by `scripts/deploy-cluster.sh` and
#                 `scripts/spawn-workers.sh` immediately after a fresh
#                 deploy. Invoked as:
#                     scripts/switch-to-ghcr.sh <vm-name> <ghcr-ref>
#                 No discovery / iteration; switches just that one VM.
#                 Errors are non-fatal (the caller's deploy succeeded; the
#                 switch is a follow-on).
#
# Honors BOOTC_SWITCH_TO_GHCR=0 as an escape hatch (operator wants to keep
# tracking localhost on purpose, e.g. for an offline lab).
#
# Single-VM mode also honors FORCE_REBUILD=1 (#375): a fresh local rebuild
# means the operator is boot-testing local Containerfile changes, so flipping
# that just-deployed VM to the GHCR-published image would mask exactly what
# they are testing. In single-VM mode FORCE_REBUILD=1 skips the switch unless
# FORCE_SWITCH=1 is also set, and warns loudly. (all-VMs mode is a deliberate
# operator action — `make switch-to-ghcr` — and is NOT gated on FORCE_REBUILD.)

set -euo pipefail

# ---- Locate self / repo root ------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"

log() { printf '[switch-to-ghcr] %s\n' "$*" >&2; }

# Source-only mode for bats: when HBIRD_SWITCH_TO_GHCR_SOURCE_ONLY=1, return
# from `source` here so the test can introspect the script (e.g. assert the
# C3 SSH-wrap shim is wired in) without the script's libvirt orchestration
# kicking in. Mirrors the HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY guard in
# scripts/deploy-cluster.sh. (#271 F1.)
if [[ "${HBIRD_SWITCH_TO_GHCR_SOURCE_ONLY:-0}" = 1 ]]; then
  return 0
fi

# Escape hatch: operator can disable the auto-switch entirely.
if [[ "${BOOTC_SWITCH_TO_GHCR:-1}" = "0" ]]; then
  log "BOOTC_SWITCH_TO_GHCR=0; skipping."
  exit 0
fi

# ---- Remote KVM-host re-exec shim (C3, #232; #271 F1) ----------------------
# When KVM_HOST is set and we're NOT on the KVM host, re-exec this script
# on the remote host via SSH. The client never needs sudo or libvirt —
# only ssh + the operator's existing SSH key. Sudo happens on the remote.
#
# This script's all-VMs mode (no positional args) iterates every running
# hummingbird-* libvirt domain via `virsh -c qemu:///system`, so it must
# run on the KVM host. The single-VM mode (called by deploy-cluster.sh /
# spawn-workers.sh) ALSO needs local libvirt — those callers themselves
# C3-wrap, so when this script is invoked from inside one of them on the
# KVM host the shim is a no-op (HBIRD_REMOTE_REEXEC=1 sentinel set).
#
# Env-var passthrough: EXPLICIT ALLOWLIST in lib/ssh-wrap.sh. The vars
# this script honors (BOOTC_SWITCH_TO_GHCR, GHCR_ORG, GHCR_TAG) all need
# to be added to HBIRD_SSH_WRAP_ALLOWED_ENV there if operators expect
# workstation-set values to reach the remote side. (GHCR_TAG already is.)
#
# shellcheck source=lib/ssh-wrap.sh
source "${SCRIPT_DIR}/lib/ssh-wrap.sh"
hbird_ssh_wrap_maybe_reexec "$0" "$@"
# ---- End remote re-exec shim -----------------------------------------------

# When invoked via sudo (so virsh has qemu:///system access), the VM-side
# authorized_keys was baked with the calling user's pubkey, not root's. Drop
# back to that user for the ssh calls so the existing key authenticates. The
# same pattern is used by spawn-workers.sh's mint_join_command. Falls through
# to a no-op prefix when not running as root.
SSH_PRIV_PREFIX=()
if [[ $EUID -eq 0 && -n "${SUDO_USER:-}" ]]; then
  SSH_PRIV_PREFIX=(sudo -u "$SUDO_USER")
fi
_ssh() { "${SSH_PRIV_PREFIX[@]}" ssh "$@"; }

# Map a VM name to its expected GHCR image ref. The naming convention is
# `hummingbird-<flavor>` (workers are `hummingbird-k8s-worker-<N>`), so we
# strip the trailing `-<N>` for workers and prepend the GHCR org.
GHCR_ORG="${GHCR_ORG:-ghcr.io/aatchison}"
GHCR_TAG="${GHCR_TAG:-latest}"

flavor_for_vm() {
  local name="$1"
  case "$name" in
    hummingbird-k8s)            echo "hummingbird-k8s" ;;
    hummingbird-k8s-worker-*)   echo "hummingbird-k8s-worker" ;;
    *)                          return 1 ;;
  esac
}

# Resolve the IPv4 lease for a domain via virsh. Empty on failure.
ip_for_vm() {
  local name="$1"
  virsh -c qemu:///system domifaddr "$name" 2>/dev/null \
    | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}'
}

# Wait up to ${1} seconds for a DHCP lease on ${2}. Echoes the IP on success.
wait_for_ip() {
  local tries="$1" name="$2" ip=""
  for _ in $(seq 1 "$tries"); do
    ip="$(ip_for_vm "$name" || true)"
    if [[ -n "$ip" ]]; then
      printf '%s\n' "$ip"
      return 0
    fi
    sleep 2
  done
  return 1
}

# Wait up to ${1} seconds for sshd to accept a connection at root@${2}.
wait_for_ssh() {
  local tries="$1" ip="$2"
  for _ in $(seq 1 "$tries"); do
    if _ssh -o BatchMode=yes -o StrictHostKeyChecking=no \
           -o ConnectTimeout=5 "root@${ip}" true >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  return 1
}

# Return the currently-booted image ref reported by bootc on the remote, or
# empty if we can't reach it / parse it.
current_image_ref() {
  local ip="$1"
  _ssh -o BatchMode=yes -o StrictHostKeyChecking=no \
      -o ConnectTimeout=5 "root@${ip}" \
      'bootc status --json 2>/dev/null' \
    | python3 -c '
import json, sys
try:
    s = json.load(sys.stdin)
    print(s["status"]["booted"]["image"]["image"]["image"])
except Exception:
    pass
' 2>/dev/null || true
}

# Switch a single VM (by name) to the given GHCR ref. Best-effort: log + return
# non-zero on failure so the caller can decide whether to continue.
switch_one() {
  local name="$1" ref="$2"
  local ip
  log "${name}: resolving IP..."
  ip="$(wait_for_ip 30 "$name" || true)"
  if [[ -z "$ip" ]]; then
    log "${name}: no DHCP lease after 60s; SKIP."
    return 1
  fi
  log "${name}: ip=${ip}; waiting for sshd..."
  if ! wait_for_ssh 30 "$ip"; then
    log "${name}: sshd never came up after 60s; SKIP."
    return 1
  fi

  local cur
  cur="$(current_image_ref "$ip" || true)"
  if [[ -n "$cur" && "$cur" == "$ref" ]]; then
    log "${name}: already tracking ${ref}; nothing to do."
    return 0
  fi
  log "${name}: switching from '${cur:-<unknown>}' to '${ref}'..."

  if ! _ssh -o BatchMode=yes -o StrictHostKeyChecking=no \
           -o ConnectTimeout=10 "root@${ip}" \
           "bootc switch '${ref}'" >&2; then
    log "${name}: bootc switch failed (image may not exist on GHCR yet)."
    return 1
  fi

  # Re-read so the operator sees the new staged ref.
  local post
  post="$(current_image_ref "$ip" || true)"
  log "${name}: now tracking '${post:-<unknown>}' (was '${cur:-<unknown>}')."
}

# --- single-VM mode ---------------------------------------------------------
# Invoked by scripts/deploy-cluster.sh / scripts/spawn-workers.sh immediately
# after a fresh deploy. Two positional args: the VM name and the exact GHCR
# ref to switch to.
if [[ $# -ge 1 ]]; then
  vm_name="$1"
  ref="${2:-}"
  # FORCE_REBUILD opt-out (#375). This is the post-deploy/post-spawn caller
  # path (deploy-cluster.sh / spawn-workers.sh). When the operator rebuilt
  # the image locally (FORCE_REBUILD=1), switching the freshly-installed VM
  # to the GHCR-published image would track a possibly-stale remote and mask
  # the local change being boot-tested. Skip unless FORCE_SWITCH=1 explicitly
  # opts back in. Mirrors the HBIRD_REMOTE_STRICT explicit-mode pattern; the
  # primary gate is in spawn-workers.sh, this is the canonical backstop so any
  # single-VM caller is protected. Non-fatal: the deploy already succeeded.
  if [[ "${FORCE_REBUILD:-}" = "1" && "${FORCE_SWITCH:-}" != "1" ]]; then
    log "WARN: FORCE_REBUILD=1 — skipping switch of '${vm_name}' to GHCR so it keeps tracking its freshly-built install-time image (#375)."
    log "WARN: set FORCE_SWITCH=1 to switch anyway, or unset FORCE_REBUILD for normal GHCR-tracking behavior."
    exit 0
  fi
  if [[ -z "$ref" ]]; then
    if flavor="$(flavor_for_vm "$vm_name")"; then
      ref="${GHCR_ORG}/${flavor}:${GHCR_TAG}"
    else
      log "ERROR: cannot infer GHCR ref for VM '${vm_name}' (unknown flavor)."
      exit 1
    fi
  fi
  switch_one "$vm_name" "$ref" || {
    log "WARN: switch failed for ${vm_name}; VM still tracks its install-time image."
    # Non-fatal: deploy already succeeded; switch is best-effort.
    exit 0
  }
  exit 0
fi

# --- all-VMs mode -----------------------------------------------------------
# No args: iterate every running hummingbird-* VM and switch each.
rc=0
mapfile -t VMS < <(virsh -c qemu:///system list --name 2>/dev/null \
                    | grep '^hummingbird-' || true)
if [[ ${#VMS[@]} -eq 0 ]]; then
  log "no running hummingbird-* VMs found."
  exit 0
fi

for vm in "${VMS[@]}"; do
  if ! flavor="$(flavor_for_vm "$vm")"; then
    log "${vm}: unknown flavor; SKIP."
    continue
  fi
  ref="${GHCR_ORG}/${flavor}:${GHCR_TAG}"
  if ! switch_one "$vm" "$ref"; then
    rc=1
    # Per #138 design: capture and STOP for this VM but continue with others.
    continue
  fi
done

exit "$rc"
