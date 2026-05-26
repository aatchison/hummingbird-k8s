#!/usr/bin/env bash
# scripts/lib/ssh-wrap.sh — Remote KVM-host re-exec shim shared by
# deploy-cluster.sh, destroy-cluster.sh, update-cluster.sh,
# spawn-workers.sh. Sourced; not meant to be executed directly.
#
# Why this exists (C3, #232):
#   The libvirt-touching scripts must run on the KVM host (qemu:///system,
#   bootc-image-builder, etc). When KVM_HOST is set and we're NOT already
#   on that host, the four scripts re-exec themselves on the KVM host
#   via SSH. The client only needs `ssh` + the operator's SSH key —
#   no `sudo` typed locally, no libvirt installed locally.
#
# Contract:
#   - Sourced after `set -euo pipefail` and any source-only test guards.
#   - Caller invokes:  hbird_ssh_wrap_maybe_reexec "$0" "$@"
#   - When the shim fires it `exec`s ssh; control never returns.
#   - When the shim is a no-op (KVM_HOST unset, we're on the KVM host,
#     or HBIRD_REMOTE_REEXEC=1 sentinel is set) the function returns 0
#     and the caller proceeds normally.
#
# Env-var passthrough: EXPLICIT ALLOWLIST. Anything else stays
# client-side. Add new vars to HBIRD_SSH_WRAP_ALLOWED_ENV below if a
# script grows new tunables. The allowlist is pinned by
# tests/scripts/ssh-wrap.bats (intentionally — opaque forwarding is a
# footgun: operator's local exports would silently change remote
# behavior).
#
# Test hooks:
#   HBIRD_REMOTE_REEXEC=1     — Sentinel set by the SSH'd remote side
#                               to prevent infinite re-exec. Also used by
#                               tests to bypass the shim entirely.
#   HBIRD_SSH_WRAP_DRY_RUN=1  — Print the SSH command we WOULD run
#                               (prefixed `SSH_WRAP_CMD: `) and exit 0,
#                               instead of actually exec'ing ssh. Used
#                               by ssh-wrap.bats to pin the env
#                               allowlist contract.

# Allowlist of env vars to forward to the remote side. Keep tight.
# Declared at top-level so tests can introspect it without invoking
# hbird_ssh_wrap_maybe_reexec.
HBIRD_SSH_WRAP_ALLOWED_ENV=(
  CONFIG FLAGS
  AUTO_UPDATE_CP SWITCH_TO_GHCR
  BOOTC_UPDATE_SCHEDULE BOOTC_UPDATE_REPO_K8S BOOTC_UPDATE_REPO_WORKER
  IMAGE_SOURCE GHCR_TAG
  DRY_RUN SKIP_DRAIN WORKERS_ONLY NODE START_FROM PARALLEL
  READY_TIMEOUT DRAIN_TIMEOUT APISERVER_TIMEOUT SSH_TIMEOUT
  INTER_NODE_SLEEP DAEMONSET_TIMEOUT
  CP_NAME WORKER_NAMES POOL_DIR
)

# hbird_ssh_wrap_maybe_reexec "$0" "$@"
# If we should re-exec on the KVM host, `exec`s ssh (does not return).
# Otherwise returns 0 and the caller continues locally.
hbird_ssh_wrap_maybe_reexec() {
  local self="$1"; shift

  # Guard: only re-exec when KVM_HOST is set, we're not already there,
  # and we're not running as the re-exec'd child on the remote side.
  [[ -n "${KVM_HOST:-}" ]] || return 0
  [[ -z "${HBIRD_REMOTE_REEXEC:-}" ]] || return 0
  local local_host
  local_host="$(hostname -s 2>/dev/null || hostname)"
  # Compare against KVM_HOST stripped to short form (geary == geary.lan).
  [[ "$local_host" != "${KVM_HOST%%.*}" ]] || return 0

  local env_args=()
  local v i
  for v in "${HBIRD_SSH_WRAP_ALLOWED_ENV[@]}"; do
    if [[ -n "${!v+x}" ]]; then
      env_args+=("${v}=${!v}")
    fi
  done

  # If CONFIG is a local file, scp it to a tempdir on the remote and
  # rewrite the CONFIG entry in env_args so the remote script sources
  # the copied file.
  if [[ -n "${CONFIG:-}" && -f "$CONFIG" ]]; then
    local remote_tmp remote_config_path
    remote_tmp="$(ssh "$KVM_HOST" 'mktemp -d -t hbird-XXXXXX')"
    remote_config_path="${remote_tmp}/$(basename "$CONFIG")"
    scp -q "$CONFIG" "${KVM_HOST}:${remote_config_path}"
    for i in "${!env_args[@]}"; do
      [[ "${env_args[i]}" == CONFIG=* ]] && env_args[i]="CONFIG=${remote_config_path}"
    done
  fi

  local script_basename
  script_basename="$(basename "$self")"
  echo "[${script_basename}] re-execing on ${KVM_HOST} via SSH (env: ${env_args[*]:-(empty)})" >&2

  # Test hook: print the would-be command and exit. Lets bats assert the
  # exact env-var allowlist without spawning real ssh.
  if [[ "${HBIRD_SSH_WRAP_DRY_RUN:-0}" = 1 ]]; then
    printf 'SSH_WRAP_CMD: ssh -t %s sudo env HBIRD_REMOTE_REEXEC=1' "$KVM_HOST"
    local a
    for a in "${env_args[@]}"; do printf ' %s' "$a"; done
    printf ' bash -s --'
    for a in "$@"; do printf ' %s' "$a"; done
    printf '\n'
    exit 0
  fi

  # Stream the script body via stdin so we don't need it pre-existing
  # on the remote. HBIRD_REMOTE_REEXEC=1 prevents infinite re-exec.
  exec ssh -t "$KVM_HOST" "sudo env HBIRD_REMOTE_REEXEC=1 ${env_args[*]} bash -s -- $*" <"$self"
}
