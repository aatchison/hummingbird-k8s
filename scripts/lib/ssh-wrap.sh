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
# Execution model (round-2 architectural pivot):
#   The shim assumes a sibling checkout of hummingbird-k8s already
#   exists on the remote at $HBIRD_REMOTE_REPO (default ~/hummingbird-k8s).
#   It `cd`s into that checkout and execs `bash scripts/<name>.sh` FROM
#   DISK, rather than streaming the script body over stdin. Streaming
#   was a dead end: every wrapped script does
#       SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
#       REPO_ROOT="${SCRIPT_DIR}/.."
#       source "${REPO_ROOT}/lib/build-common.sh"
#   and under `bash -s` $0 is "bash" / a temp path, so SCRIPT_DIR
#   resolves wrong and the source fails on first call.
#
#   Operator does the one-time setup:
#       ssh $KVM_HOST 'git clone https://github.com/aatchison/hummingbird-k8s ~/hummingbird-k8s'
#   We deliberately do NOT auto-clone — the operator decides which
#   branch/ref the remote checkout tracks.
#
# Sudo on the remote:
#   With the checkout-on-remote model, sudo can prompt normally on the
#   SSH TTY (no stdin-streaming conflict). NOPASSWD sudo is RECOMMENDED
#   for unattended runs but no longer mandatory.
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
# Quoting:
#   Every env value and positional arg is escaped through `printf %q`
#   before being interpolated into the remote command. That closes the
#   command-injection surface that pre-round-2 had (FLAGS="--foo; rm -rf"
#   would have been word-split on the remote shell).
#
# Test hooks:
#   HBIRD_REMOTE_REEXEC=1     — Sentinel set by the SSH'd remote side
#                               to prevent infinite re-exec. Also used by
#                               tests to bypass the shim entirely. We
#                               `unset` it on the client side before
#                               building the remote command, as a
#                               defense against client-side env
#                               pollution.
#   HBIRD_SSH_WRAP_DRY_RUN=1  — Print the SSH command we WOULD run
#                               (prefixed `SSH_WRAP_CMD: `) and exit 0,
#                               instead of actually exec'ing ssh. Used
#                               by ssh-wrap.bats to pin the env
#                               allowlist + quoting contract.
#   HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1
#                             — Skip the SSH reachability + remote-repo
#                               existence pre-flight checks. Used by
#                               bats so tests don't have to stub a
#                               working ssh path to otherwise-foreign
#                               hosts.
#   HBIRD_SSH_WRAP_DRY_RUN_SCP=1
#                             — In the CONFIG-scp branch, print
#                               `SCP_WOULD_RUN: <src> -> <dest>` and use
#                               a deterministic remote tempdir
#                               (/tmp/hbird-dryrun) instead of actually
#                               calling scp. Lets bats exercise the
#                               CONFIG-rewrite path without a working
#                               remote. Also used by the operator-pubkey
#                               scp branch (#248).
#
# Operator workstation pubkey (#248):
#   When the local CONFIG declares `SSH_PUBKEY_FILE=`, that path is
#   resolved against the OPERATOR's workstation — but the script runs on
#   the KVM host, where the same absolute path may point at a DIFFERENT
#   pubkey (the KVM host's own key, not the operator's). Result: the CP
#   gets the KVM host's key baked, the operator can't SSH directly from
#   their workstation.
#
#   Fix (Model 2, additive — no private keys travel): in addition to scp'ing
#   the CONFIG, also scp the operator's pubkey to the remote tempdir and
#   forward its remote path via `HBIRD_OPERATOR_PUBKEY_FILE`. The deploy
#   script ADDS that path to `SSH_PUBKEY_FILES` (colon-separated), so the
#   CP ends up with BOTH the KVM host's key (used by the script to SSH to
#   the freshly-booted CP) AND the operator's workstation key (used by the
#   operator for direct access). See issue #248.

# Default for the remote checkout path. Operator can override via the
# environment or cluster.local.conf. The remote MUST have a git clone
# of hummingbird-k8s at this path; the shim does not auto-clone.
: "${HBIRD_REMOTE_REPO:=~/hummingbird-k8s}"

# Allowlist of env vars to forward to the remote side. Keep tight.
# Declared at top-level so tests can introspect it without invoking
# hbird_ssh_wrap_maybe_reexec.
HBIRD_SSH_WRAP_ALLOWED_ENV=(
  CONFIG FLAGS
  AUTO_UPDATE_CP SWITCH_TO_GHCR
  BOOTC_UPDATE_SCHEDULE BOOTC_UPDATE_REPO_K8S BOOTC_UPDATE_REPO_WORKER
  IMAGE_SOURCE GHCR_ORG GHCR_TAG BOOTC_SWITCH_TO_GHCR
  DRY_RUN SKIP_DRAIN WORKERS_ONLY NODE START_FROM PARALLEL
  READY_TIMEOUT DRAIN_TIMEOUT APISERVER_TIMEOUT SSH_TIMEOUT
  INTER_NODE_SLEEP DAEMONSET_TIMEOUT
  CP_NAME WORKER_NAMES POOL_DIR POOL_NAME
  VM_USER STORAGE_DRIVER PODMAN_ROOT PODMAN_RUNROOT APISERVER_EXTRA_SANS
  FORCE_REBUILD
  HBIRD_AUTOLOAD_CONFIG_LOCAL HBIRD_REMOTE_REPO
  HBIRD_OPERATOR_PUBKEY_FILE
  HBIRD_REMOTE_NO_SUDO
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
  # Note: this assumes KVM_HOST is an ssh alias / short hostname, not a
  # bare IPv4/IPv6 literal or an unrelated FQDN whose label set doesn't
  # overlap with the local hostname. See docs/deploy-cluster.md for the
  # supported value space.
  [[ "$local_host" != "${KVM_HOST%%.*}" ]] || return 0

  # Defense against client-side env pollution: clear the sentinel
  # locally so a stray HBIRD_REMOTE_REEXEC=1 in the operator's shell
  # can't trick us into building a remote command that re-disables
  # itself. (We already returned 0 above if the sentinel was truly
  # set; this `unset` defends against future code that might consult
  # the variable below.)
  unset HBIRD_REMOTE_REEXEC

  local script_basename
  script_basename="$(basename "$self")"

  # Pre-flight checks (skip with HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1 for
  # tests). Friendly errors here save a confusing failure later.
  if [[ "${HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT:-0}" != 1 ]]; then
    if ! ssh -o BatchMode=yes -o ConnectTimeout=5 "$KVM_HOST" true 2>/dev/null; then
      echo "[${script_basename}] cannot reach KVM_HOST='${KVM_HOST}' via SSH — check ~/.ssh/config + key auth" >&2
      exit 1
    fi
    if ! ssh "$KVM_HOST" "test -d ${HBIRD_REMOTE_REPO}/scripts" 2>/dev/null; then
      echo "[${script_basename}] remote repo not found at ${KVM_HOST}:${HBIRD_REMOTE_REPO}/scripts" >&2
      echo "  fix: ssh ${KVM_HOST} 'git clone https://github.com/aatchison/hummingbird-k8s ${HBIRD_REMOTE_REPO}'" >&2
      echo "  (or override HBIRD_REMOTE_REPO env var)" >&2
      exit 1
    fi
  fi

  local env_args=()
  local v i
  for v in "${HBIRD_SSH_WRAP_ALLOWED_ENV[@]}"; do
    # HBIRD_OPERATOR_PUBKEY_FILE is shim-managed (#248): the shim derives
    # it from the scp'd pubkey path below and appends it AFTER the
    # visible-env log line. Skip it here so a stale value from the
    # operator's shell doesn't end up double-forwarded or in the log.
    # The var stays in the allowlist (and in the bats pin test) so that
    # `sudo env` accepts the forwarded value on the remote.
    [[ "$v" == HBIRD_OPERATOR_PUBKEY_FILE ]] && continue
    if [[ -n "${!v+x}" ]]; then
      env_args+=("${v}=${!v}")
    fi
  done

  # If CONFIG is a local file, scp it to a tempdir on the remote and
  # rewrite the CONFIG entry in env_args so the remote script sources
  # the copied file.
  local remote_tmp="" remote_config_path=""
  if [[ -n "${CONFIG:-}" && -f "$CONFIG" ]]; then
    if [[ "${HBIRD_SSH_WRAP_DRY_RUN_SCP:-0}" = 1 ]]; then
      remote_tmp="/tmp/hbird-dryrun"
      remote_config_path="${remote_tmp}/$(basename "$CONFIG")"
      echo "SCP_WOULD_RUN: ${CONFIG} -> ${KVM_HOST}:${remote_config_path}" >&2
    else
      remote_tmp="$(ssh "$KVM_HOST" 'mktemp -d -t hbird-XXXXXX')"
      # H2: guard against empty remote_tmp from a failed `mktemp -d`.
      # Without this, an scp to "${KVM_HOST}:/$(basename "$CONFIG")"
      # would write to the remote filesystem root.
      if [[ -z "$remote_tmp" ]]; then
        echo "[${script_basename}] remote mktemp -d failed; refusing to scp CONFIG" >&2
        exit 1
      fi
      remote_config_path="${remote_tmp}/$(basename "$CONFIG")"
      # M6: drop -q so scp errors surface; wrap in if/then for a
      # friendly diagnostic on failure.
      if ! scp "$CONFIG" "${KVM_HOST}:${remote_config_path}"; then
        echo "[${script_basename}] scp of CONFIG to ${KVM_HOST}:${remote_config_path} failed" >&2
        exit 1
      fi
    fi
    for i in "${!env_args[@]}"; do
      [[ "${env_args[i]}" == CONFIG=* ]] && env_args[i]="CONFIG=${remote_config_path}"
    done

    # #248: also scp the operator's workstation pubkey to the remote
    # tempdir and forward its remote path via HBIRD_OPERATOR_PUBKEY_FILE.
    # The deploy script ADDS this path to SSH_PUBKEY_FILES so the CP
    # gets BOTH the KVM host's key (used by the script to SSH to the
    # freshly-booted CP) AND the operator's key (used by the operator
    # for direct workstation->CP SSH). No private keys travel.
    #
    # Parse SSH_PUBKEY_FILE out of the local CONFIG in a subshell so we
    # don't pollute the parent env. The operator might also set
    # SSH_PUBKEY_FILE in their shell env (cluster.example.conf doesn't
    # encourage this, but ":${SSH_PUBKEY_FILE:-}" honors both).
    local local_pubkey_file=""
    local_pubkey_file="$(
      bash -c "set +u; source $(printf '%q' "$CONFIG") >/dev/null 2>&1 || true; printf '%s' \"\${SSH_PUBKEY_FILE:-}\""
    )" || true
    if [[ -n "$local_pubkey_file" && -r "$local_pubkey_file" ]]; then
      local remote_pubkey_path
      remote_pubkey_path="${remote_tmp}/$(basename "$local_pubkey_file")"
      if [[ "${HBIRD_SSH_WRAP_DRY_RUN_SCP:-0}" = 1 ]]; then
        echo "SCP_WOULD_RUN: ${local_pubkey_file} -> ${KVM_HOST}:${remote_pubkey_path}" >&2
      else
        if ! scp "$local_pubkey_file" "${KVM_HOST}:${remote_pubkey_path}"; then
          echo "[${script_basename}] scp of operator pubkey to ${KVM_HOST}:${remote_pubkey_path} failed" >&2
          exit 1
        fi
      fi
      # Stash for the post-log append below (so it stays out of the
      # operator-facing visible-env log line — it's a shim-internal var,
      # not something the operator set).
      HBIRD_OPERATOR_PUBKEY_FILE="$remote_pubkey_path"
      export HBIRD_OPERATOR_PUBKEY_FILE
    fi
  fi

  # Rewrite any positional arg matching the local CONFIG path to the
  # scp'd remote temp path. This closes #245: scripts like
  # deploy-cluster.sh take CONFIG as a positional arg via the Makefile
  # (`bash scripts/deploy-cluster.sh "$(CONFIG)"`) and prefer $1 over
  # the CONFIG env var. Without this rewrite, the env-var fix above is
  # silently bypassed and the remote reads a stale ~/hummingbird-k8s
  # checkout's CONFIG file instead of the operator's freshly-scp'd one.
  local quoted_args_arr=()
  local a
  for a in "$@"; do
    if [[ -n "${CONFIG:-}" && -n "${remote_config_path:-}" && "$a" == "$CONFIG" ]]; then
      quoted_args_arr+=("$remote_config_path")
    else
      quoted_args_arr+=("$a")
    fi
  done
  local quoted_args=""
  for a in "${quoted_args_arr[@]}"; do
    quoted_args+="$(printf '%q ' "$a")"
  done

  # Resolve the remote script path against the operator's checkout.
  local remote_script="${HBIRD_REMOTE_REPO}/scripts/${script_basename}"
  echo "[${script_basename}] re-execing on ${KVM_HOST}:${remote_script} (env: ${env_args[*]:-(empty)})" >&2

  # #248: append the operator-pubkey forwarding var AFTER the
  # operator-facing visible-env log line above. It's a shim-internal
  # var (not something the operator set), so it'd be noise in the log,
  # but it MUST be on the remote command line so deploy-cluster.sh sees
  # it. The remote pubkey path itself is benign (no key material in the
  # path; the .pub content lives in the scp'd file at that path).
  if [[ -n "${HBIRD_OPERATOR_PUBKEY_FILE:-}" ]]; then
    env_args+=("HBIRD_OPERATOR_PUBKEY_FILE=${HBIRD_OPERATOR_PUBKEY_FILE}")
  fi

  # Properly quote every env-arg using printf %q. This closes the
  # round-1 command-injection HIGH finding: values containing spaces,
  # quotes, or shell metas now reach the remote bash unmangled.
  local quoted_env=""
  for v in "${env_args[@]}"; do
    # env_args entries look like NAME=value; split on first '=' so we
    # can quote NAME and value independently. NAME is allowlisted so
    # never contains metachars in practice, but quote it anyway.
    local name="${v%%=*}"
    local val="${v#*=}"
    quoted_env+="$(printf '%q=%q ' "$name" "$val")"
  done

  # HBIRD_REMOTE_NO_SUDO=1 opts out of `sudo` on the remote: appropriate
  # when the operator is already in the `libvirt` group on the KVM host
  # and the wrapped script (e.g. update-cluster.sh) doesn't otherwise
  # need root. Default keeps `sudo` for safety — deploy/destroy/spawn
  # still need root for POOL_DIR writes (Phase 3, separate issue). (#269)
  local sudo_prefix="sudo "
  if [[ "${HBIRD_REMOTE_NO_SUDO:-0}" == "1" ]]; then
    sudo_prefix=""
  fi

  # Test hook: print the would-be command and exit. Lets bats assert the
  # exact env-var allowlist + quoting behavior without spawning real ssh.
  if [[ "${HBIRD_SSH_WRAP_DRY_RUN:-0}" = 1 ]]; then
    printf 'SSH_WRAP_CMD: ssh -t %s cd %s && %senv HBIRD_REMOTE_REEXEC=1 %sbash %s %s\n' \
      "$KVM_HOST" "$HBIRD_REMOTE_REPO" "$sudo_prefix" "$quoted_env" "$remote_script" "$quoted_args"
    exit 0
  fi

  # cd into the remote checkout, then (sudo) env=... bash <script> from
  # disk. HBIRD_REMOTE_REEXEC=1 prevents infinite re-exec.
  exec ssh -t "$KVM_HOST" "cd ${HBIRD_REMOTE_REPO} && ${sudo_prefix}env HBIRD_REMOTE_REEXEC=1 ${quoted_env}bash ${remote_script} ${quoted_args}"
}
