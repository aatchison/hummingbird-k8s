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
#   HBIRD_SSH_WRAP_DRY_RUN_FRESHNESS=<state>
#                             — Stub the remote git rev-parse used by the
#                               freshness check (#365). Recognized values:
#                                 equal        — remote SHA == local SHA
#                                 behind       — remote SHA is an ancestor of local
#                                 diverged     — remote SHA is neither ancestor nor equal
#                                 missing      — ssh OK but rev-parse returns
#                                                empty (corrupt .git / tarball)
#                                 unreachable  — ssh failed (network/auth);
#                                                helper must NOT treat as stale
#                                                even under STRICT (round-2 H1)
#                               When unset, the real `ssh ... git rev-parse`
#                               runs (skipped entirely if the SSH-preflight
#                               dry-run hook is also set and no freshness
#                               state was requested). These hooks are bats-
#                               only — the helper emits a WARN if either is
#                               set outside a BATS_TEST_FILENAME context.
#   HBIRD_SSH_WRAP_DRY_RUN_BEHIND_COUNT=<int>
#                             — Synthesize the commits-behind count used by
#                               the lag-threshold check (M9). Defaults to 1
#                               when unset. Only consulted with FRESHNESS=behind.
#   HBIRD_SSH_WRAP_DRY_RUN_LOCAL_SHA=<sha>
#                             — Inject the local-side SHA the freshness
#                               check would otherwise read from `git
#                               rev-parse`. Lets bats run in environments
#                               without git (the pinned bats container has
#                               no git). Production callers leave this
#                               unset.
#
# Remote-checkout freshness (#365):
#   Before re-exec, the shim compares the local repo's HEAD SHA to the
#   remote checkout's HEAD SHA. Stale remotes (behind, diverged, or
#   missing/unreadable) WARN to stderr — they do NOT auto-pull. Equal
#   SHAs print nothing. Surfaced after the cycle-2 dispatch where geary's
#   checkout was missing a merged fix (#364) and silently re-execed the
#   buggy pre-merge script.
#
#   Failure mode distinction (round-2 H1): network-unreachable, missing-
#   git-repo, and genuine behind/diverged are three different operator
#   stories. The helper now distinguishes:
#     (a) ssh failed (network/auth)        — WARN, mention "cannot reach"
#                                            + ssh exit code; do NOT
#                                            treat as stale.
#     (b) ssh OK but rev-parse empty       — WARN, mention "not a git
#                                            repo" (preflight already
#                                            confirmed scripts/ exists,
#                                            so this is a corrupted .git
#                                            or extracted tarball).
#     (c) SHAs differ                      — WARN BEHIND or DIVERGED per
#                                            ancestry decision.
#
#   Topic-branch handling (round-2 H2): when the operator's local branch
#   is NOT main, the freshness comparison is inherently noisy (a topic
#   branch has commits main doesn't, and main has commits the topic
#   doesn't — DIVERGED is the expected state). The helper still runs the
#   check (operator may want to know if their remote has REGRESSED) but
#   prepends a softer "this is expected if on a topic branch" hint to the
#   WARN text so a developer working on a feature branch isn't misled into
#   force-pushing main on the KVM host.
#
#   Lag threshold (M9): even on the BEHIND ancestor case, a remote a few
#   commits behind is usually fine (release-train rebases, etc). The
#   WARN is skipped when behind by <= HBIRD_REMOTE_LAG_THRESHOLD commits
#   (default 5). Set the threshold to 0 to warn on any lag.
#
#   Per-session cache (M2): the rev-parse + ancestry probe costs 1-3 SSH
#   round-trips per shim invocation. A Make pipeline that runs deploy
#   + update + spawn-workers back-to-back pays this 3x. The helper caches
#   the freshness decision in $TMPDIR/hbird-freshness/$KVM_HOST-$sha for
#   60s. Cache file content: 'ok' (no warn) or 'warn:<text>' (replay
#   warn). Local SHA changing (operator pulled mid-session) invalidates
#   automatically because the cache key embeds the local SHA.
#
#   Env toggles (CLIENT-SIDE ONLY — NOT forwarded to the remote):
#     HBIRD_REMOTE_FRESHNESS_CHECK=0  — disable the check entirely
#                                       (default ON). Accepts boolish
#                                       0/1/true/false/yes/no/on/off
#                                       per parity with rust-cli boolish
#                                       parser (rust-cli-migration.md:90).
#     HBIRD_REMOTE_STRICT=1           — exit 1 on stale (default = warn-
#                                       only). Same boolish parser.
#     HBIRD_REMOTE_LAG_THRESHOLD=N    — skip WARN if BEHIND by <= N
#                                       commits (default 5; set to 0 to
#                                       warn on any lag).
#
#   CI vs operator defaults: CI workflows currently run ON the KVM host
#   (runs-on: [self-hosted, kvm, libvirt]), so the shim never fires in
#   CI — the freshness check is operator-workstation only. If a future
#   CI workflow drives the shim from a non-KVM-host runner, set
#   HBIRD_REMOTE_STRICT=1 in that workflow's `env:` block (CI = STRICT
#   by default; operators = WARN by default).
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
  FORCE_REBUILD FORCE_SWITCH
  HBIRD_AUTOLOAD_CONFIG_LOCAL HBIRD_REMOTE_REPO
  HBIRD_OPERATOR_PUBKEY_FILE
  HBIRD_REMOTE_NO_SUDO
)

# _hbird_ssh_wrap_boolish <varname> <default-0-or-1>
# Internal: parse a boolish env-var value with parity to the rust-cli
# boolish parser (rust-cli-migration.md:90). Accepts 0/1, true/false,
# yes/no, on/off (case-insensitive). Empty/unset -> default. Unknown
# value -> default (silently — do not break the shim on operator typo).
# Prints '0' or '1' to stdout.
_hbird_ssh_wrap_boolish() {
  local var="$1" def="$2"
  local raw="${!var:-}"
  if [[ -z "$raw" ]]; then
    printf '%s' "$def"; return 0
  fi
  case "$(printf '%s' "$raw" | tr '[:upper:]' '[:lower:]')" in
    1|true|yes|on)   printf '1' ;;
    0|false|no|off)  printf '0' ;;
    *)               printf '%s' "$def" ;;
  esac
}

# _hbird_ssh_wrap_cache_dir
# Internal: derive a per-user cache dir, preferring XDG_RUNTIME_DIR (tmpfs,
# user-private) then TMPDIR then /tmp. mkdir -p is idempotent.
# Returns failure (so caching is disabled) when
# HBIRD_SSH_WRAP_FRESHNESS_CACHE=0 — bats uses this to pin freshness
# probe behavior deterministically per-test.
_hbird_ssh_wrap_cache_dir() {
  [[ "$(_hbird_ssh_wrap_boolish HBIRD_SSH_WRAP_FRESHNESS_CACHE 1)" == 1 ]] || return 1
  local base="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}"
  local dir="${base}/hbird-freshness"
  mkdir -p "$dir" 2>/dev/null || return 1
  printf '%s' "$dir"
}

# hbird_ssh_wrap_check_remote_freshness <local_repo_dir> <script_basename>
# Compare the local repo's HEAD SHA to the remote checkout's HEAD SHA and
# emit a WARN to stderr when the remote is stale (behind, diverged, or
# missing/unreachable). Returns 0 (warn-only) by default; returns 1 if
# HBIRD_REMOTE_STRICT=1 is set AND the remote is genuinely stale (not
# just unreachable — see distinction below).
#
# Equal SHAs print nothing (silent good path).
#
# Failure-mode distinction (round-2 H1):
#   (a) ssh-unreachable      — WARN only ("cannot reach … ssh exit N"),
#                              do NOT treat as stale, do NOT hard-fail
#                              under STRICT (network blip is not the
#                              operator's "I forgot to git pull"
#                              problem; the subsequent re-exec ssh will
#                              fail with a clearer error).
#   (b) rev-parse returns "" — WARN "not a git repo" (preflight already
#                              confirmed scripts/ dir exists, so this is
#                              a corrupted .git or an extracted tarball).
#                              Hard-fail under STRICT (re-exec will
#                              succeed but run mystery code).
#   (c) SHAs differ          — Real ancestry comparison (BEHIND vs
#                              DIVERGED). Hard-fail under STRICT.
#
# Topic-branch handling (round-2 H2):
#   If the operator's local branch is NOT main, prepend a softer "this is
#   expected if on a topic branch" hint to the WARN so a feature-branch
#   developer isn't misled. Still run the check (operator may want to
#   know if the remote has REGRESSED below their topic branch's merge
#   base).
#
# Honors HBIRD_REMOTE_FRESHNESS_CHECK boolish (default ON).
# Honors HBIRD_REMOTE_STRICT boolish (default OFF).
# Honors HBIRD_REMOTE_LAG_THRESHOLD (default 5) — skip BEHIND WARN if
# behind by <= N commits.
# Honors HBIRD_SSH_WRAP_DRY_RUN_FRESHNESS=<state> to stub the remote
# rev-parse for bats coverage; see header comment for recognized states.
#
# This function does NOT exec; it is a pure check called from the shim
# just before the ssh re-exec.
hbird_ssh_wrap_check_remote_freshness() {
  local local_repo_dir="$1"
  local script_basename="$2"

  # Opt-out: operator disabled the check (boolish — 0/false/no/off).
  [[ "$(_hbird_ssh_wrap_boolish HBIRD_REMOTE_FRESHNESS_CHECK 1)" == 1 ]] || return 0

  local strict
  strict="$(_hbird_ssh_wrap_boolish HBIRD_REMOTE_STRICT 0)"

  # M4: warn loudly if test-only dry-run hooks are set outside a bats
  # context. Operator-shell pollution of these vars would cause silent
  # divergence from production behavior. BATS_TEST_FILENAME is set by
  # the bats harness for every test (and by `run` inside a test).
  if [[ -z "${BATS_TEST_FILENAME:-}" ]]; then
    if [[ -n "${HBIRD_SSH_WRAP_DRY_RUN_FRESHNESS:-}" || -n "${HBIRD_SSH_WRAP_DRY_RUN_LOCAL_SHA:-}" ]]; then
      echo "[${script_basename}] WARN: HBIRD_SSH_WRAP_DRY_RUN_FRESHNESS / DRY_RUN_LOCAL_SHA are test-only hooks; unset them in operator shells" >&2
    fi
  fi

  # Resolve local SHA from the script's own repo (works regardless of cwd
  # or worktree layout). If we cannot get it (script lives outside a git
  # checkout — e.g. installed to /usr/local), silently skip: there is
  # nothing to compare against. The DRY_RUN_LOCAL_SHA hook lets bats
  # inject a value without depending on git being installed.
  local local_sha=""
  if [[ -n "${HBIRD_SSH_WRAP_DRY_RUN_LOCAL_SHA:-}" ]]; then
    local_sha="${HBIRD_SSH_WRAP_DRY_RUN_LOCAL_SHA}"
  else
    local_sha="$(git -C "$local_repo_dir" rev-parse HEAD 2>/dev/null || true)"
  fi
  [[ -n "$local_sha" ]] || return 0

  # H2: detect topic-branch case. local_branch == 'main' is the common
  # operator path; anything else gets a softer WARN prefix. Dry-run
  # mode skips the git call (bats container has no git).
  local local_branch="main"
  if [[ -z "${HBIRD_SSH_WRAP_DRY_RUN_LOCAL_SHA:-}" ]]; then
    local_branch="$(git -C "$local_repo_dir" rev-parse --abbrev-ref HEAD 2>/dev/null || echo main)"
  fi
  local topic_hint=""
  if [[ "$local_branch" != "main" && "$local_branch" != "HEAD" ]]; then
    topic_hint=" (local on topic branch '${local_branch}' — diverge is expected)"
  fi

  # M2: per-session cache. Skip re-probe if cache hit and fresh.
  local cache_dir cache_file cache_age cache_ttl=60
  cache_dir="$(_hbird_ssh_wrap_cache_dir 2>/dev/null || true)"
  if [[ -n "$cache_dir" ]]; then
    # Cache key embeds local_sha (operator pulling invalidates) +
    # KVM_HOST + REMOTE_REPO (different remotes don't collide) +
    # STRICT (strict-mode failure must not be cached as ok).
    local cache_key="${KVM_HOST//\//_}__${HBIRD_REMOTE_REPO//\//_}__${local_sha}__strict${strict}"
    cache_file="${cache_dir}/${cache_key}"
    if [[ -f "$cache_file" ]]; then
      cache_age=$(( $(date +%s) - $(stat -c %Y "$cache_file" 2>/dev/null || echo 0) ))
      if (( cache_age < cache_ttl )); then
        local cached
        cached="$(cat "$cache_file" 2>/dev/null || true)"
        if [[ "$cached" == "ok" ]]; then
          return 0
        elif [[ "$cached" == warn:* ]]; then
          # Replay the cached WARN text verbatim. If strict, return 1
          # (cache key includes strict so a strict-mode failure was
          # already cached as warn:... — see strict handling below).
          printf '%s\n' "${cached#warn:}" >&2
          if [[ "$strict" == 1 ]]; then
            return 1
          fi
          return 0
        elif [[ "$cached" == strictfail:* ]]; then
          printf '%s\n' "${cached#strictfail:}" >&2
          return 1
        fi
      fi
    fi
  fi

  # Resolve the remote SHA. The dry-run-freshness hook lets bats simulate
  # each case without a working SSH path.
  local remote_sha=""
  local ssh_rc=0
  local remote_state=""  # empty | ok | unreachable | not_a_repo
  local dryrun_state="${HBIRD_SSH_WRAP_DRY_RUN_FRESHNESS:-}"
  if [[ -n "$dryrun_state" ]]; then
    case "$dryrun_state" in
      equal)        remote_sha="$local_sha"; remote_state=ok ;;
      missing)      remote_sha=""; remote_state=not_a_repo ;;
      unreachable)  remote_sha=""; remote_state=unreachable; ssh_rc=255 ;;
      behind|diverged)
        # Synthesize a distinct fake SHA — the ancestry decision below
        # is also driven off the same hook, so the literal value is
        # only used for the warning text.
        remote_sha="0000000000000000000000000000000000000000"
        remote_state=ok
        ;;
      *)
        # Unrecognized state — treat as no-op rather than risk a false
        # warning that masks a real bug. The bats suite enumerates the
        # valid states; production callers should never set this var.
        return 0
        ;;
    esac
  else
    # Real path: query the remote. BatchMode=yes prevents a password
    # prompt if key auth fails. ConnectTimeout=5 matches preflight.
    # H1: capture ssh exit code separately so we can distinguish network
    # failure from "ssh OK but git rev-parse empty" (corrupted/tarball
    # remote checkout).
    local remote_sha_raw
    remote_sha_raw="$(ssh -o BatchMode=yes -o ConnectTimeout=5 "$KVM_HOST" \
      "git -C ${HBIRD_REMOTE_REPO} rev-parse HEAD 2>/dev/null" 2>/dev/null)"
    ssh_rc=$?
    if (( ssh_rc != 0 )); then
      remote_state=unreachable
    else
      remote_sha="$(printf '%s' "$remote_sha_raw" | tr -d '[:space:]')"
      if [[ -z "$remote_sha" ]]; then
        remote_state=not_a_repo
      else
        remote_state=ok
      fi
    fi
  fi

  # Helper: cache the outcome. Captures the printed message so future
  # invocations replay the same WARN without re-probing.
  local _cache_msg=""
  _cache_outcome() {
    [[ -n "$cache_dir" && -n "$cache_file" ]] || return 0
    printf '%s' "$1" >"$cache_file" 2>/dev/null || true
  }

  # (a) Network/auth failure: WARN only — never hard-fail under STRICT.
  if [[ "$remote_state" == unreachable ]]; then
    local msg="[${script_basename}] WARN: cannot reach ${KVM_HOST} to check remote checkout freshness (ssh exit ${ssh_rc}) — proceeding; the re-exec ssh will surface the real error"
    echo "$msg" >&2
    _cache_outcome "warn:${msg}"
    return 0
  fi

  # (b) ssh OK but rev-parse returned empty — directory exists (preflight
  # already validated $HBIRD_REMOTE_REPO/scripts) but .git is missing or
  # corrupted (e.g. extracted from a tarball). Recovery is operator-
  # specific (re-clone). Reword vs round-1: distinguish from the
  # network-failure case.
  if [[ "$remote_state" == not_a_repo ]]; then
    local msg1="[${script_basename}] WARN: ${KVM_HOST}:${HBIRD_REMOTE_REPO} exists but is not a git checkout (preflight passed, but 'git rev-parse HEAD' returned empty — looks like a tarball extract or a corrupted .git)"
    local msg2="[${script_basename}] WARN: re-exec will likely run mystery code; recover with 'ssh ${KVM_HOST} rm -rf ${HBIRD_REMOTE_REPO} && ssh ${KVM_HOST} git clone https://github.com/aatchison/hummingbird-k8s ${HBIRD_REMOTE_REPO}' OR rerun with HBIRD_REMOTE_FRESHNESS_CHECK=0 to override"
    echo "$msg1" >&2
    echo "$msg2" >&2
    if [[ "$strict" == 1 ]]; then
      local msg3="[${script_basename}] HBIRD_REMOTE_STRICT=1 — refusing to re-exec on a missing remote checkout"
      echo "$msg3" >&2
      _cache_outcome "strictfail:${msg1}"$'\n'"${msg2}"$'\n'"${msg3}"
      return 1
    fi
    _cache_outcome "warn:${msg1}"$'\n'"${msg2}"
    return 0
  fi

  # (c) ssh OK + non-empty remote_sha — real ancestry comparison.

  # Equal — silent good path.
  if [[ "$local_sha" == "$remote_sha" ]]; then
    _cache_outcome "ok"
    return 0
  fi

  # Different. Decide behind vs diverged. In dry-run-freshness mode we
  # already know which case to take from the hook; otherwise we ask the
  # remote (since the remote is the only side that can see whether
  # $remote_sha is an ancestor of $local_sha in ITS object DB — the
  # operator's workstation may not even have fetched $remote_sha).
  local is_ancestor=0
  local commits_behind=0
  if [[ -n "$dryrun_state" ]]; then
    if [[ "$dryrun_state" == behind ]]; then
      is_ancestor=1
      # Lets the lag-threshold path be tested without git: synthesize
      # a commits-behind count from a dedicated hook.
      commits_behind="${HBIRD_SSH_WRAP_DRY_RUN_BEHIND_COUNT:-1}"
    fi
  else
    # Try the local side first — if our git knows both SHAs (operator
    # just pulled before dispatch, common case), we do not need a
    # second SSH round-trip. Fall back to asking the remote.
    if git -C "$local_repo_dir" merge-base --is-ancestor "$remote_sha" "$local_sha" 2>/dev/null; then
      is_ancestor=1
      commits_behind="$(git -C "$local_repo_dir" rev-list --count "${remote_sha}..${local_sha}" 2>/dev/null || echo 1)"
    elif ssh -o BatchMode=yes -o ConnectTimeout=5 "$KVM_HOST" \
           "git -C ${HBIRD_REMOTE_REPO} merge-base --is-ancestor ${remote_sha} ${local_sha}" \
           >/dev/null 2>&1; then
      is_ancestor=1
      commits_behind="$(ssh -o BatchMode=yes -o ConnectTimeout=5 "$KVM_HOST" \
        "git -C ${HBIRD_REMOTE_REPO} rev-list --count ${remote_sha}..${local_sha} 2>/dev/null" 2>/dev/null \
        | tr -d '[:space:]')"
      [[ -n "$commits_behind" ]] || commits_behind=1
    fi
  fi

  local lag_threshold="${HBIRD_REMOTE_LAG_THRESHOLD:-5}"
  # Non-numeric -> default 5 (defensive).
  [[ "$lag_threshold" =~ ^[0-9]+$ ]] || lag_threshold=5

  if [[ "$is_ancestor" == 1 ]]; then
    # M9: silence the WARN when behind by <= threshold commits AND not
    # on a topic branch (topic-branch WARN is informational and small
    # lag is normal — but keep the WARN if the topic-branch hint is
    # firing because that's a different signal).
    if (( commits_behind <= lag_threshold )) && [[ -z "$topic_hint" ]]; then
      _cache_outcome "ok"
      return 0
    fi
    local msg1="[${script_basename}] WARN: ${KVM_HOST} checkout (${remote_sha}) is BEHIND local (${local_sha}) by ${commits_behind} commit(s)${topic_hint} — fixes from local will not apply on re-exec"
    # M1: recovery hint depends on local branch. On main, the operator
    # almost certainly wants `fetch && reset --hard origin/main`
    # (the remote checkout is meant to mirror main; `pull` could merge
    # if origin diverged). On a topic branch, just `fetch` + manual
    # decision (don't suggest a destructive reset).
    local msg2
    if [[ "$local_branch" == "main" ]]; then
      msg2="[${script_basename}] WARN: to sync, run 'ssh ${KVM_HOST} git -C ${HBIRD_REMOTE_REPO} fetch && ssh ${KVM_HOST} git -C ${HBIRD_REMOTE_REPO} reset --hard origin/main' (will NOT auto-pull; fetch+reset is the safe form for a deployment-tracking checkout — see docs/deploy-cluster.md)"
    else
      msg2="[${script_basename}] WARN: local on '${local_branch}'; run 'ssh ${KVM_HOST} git -C ${HBIRD_REMOTE_REPO} fetch' and reconcile manually (will NOT auto-pull; reset --hard would be destructive on a topic-branch checkout)"
    fi
    echo "$msg1" >&2
    echo "$msg2" >&2
    if [[ "$strict" == 1 ]]; then
      local msg3="[${script_basename}] HBIRD_REMOTE_STRICT=1 — refusing to re-exec on a stale remote checkout"
      echo "$msg3" >&2
      _cache_outcome "strictfail:${msg1}"$'\n'"${msg2}"$'\n'"${msg3}"
      return 1
    fi
    _cache_outcome "warn:${msg1}"$'\n'"${msg2}"
    return 0
  fi

  # Diverged.
  local msg1="[${script_basename}] WARN: ${KVM_HOST} checkout (${remote_sha}) diverges from local (${local_sha})${topic_hint} — re-exec semantics may surprise you"
  echo "$msg1" >&2
  if [[ "$strict" == 1 ]]; then
    local msg2="[${script_basename}] HBIRD_REMOTE_STRICT=1 — refusing to re-exec on a stale remote checkout"
    echo "$msg2" >&2
    _cache_outcome "strictfail:${msg1}"$'\n'"${msg2}"
    return 1
  fi
  _cache_outcome "warn:${msg1}"
  return 0
}

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

  # Freshness check (#365): warn (or hard-fail under STRICT) when the
  # remote checkout's HEAD SHA differs from the local repo's HEAD SHA.
  # Surfaced after a cycle-2 dispatch silently re-execed a buggy
  # pre-merge script because the KVM host's checkout had not been
  # pulled. The check is skipped when the SSH-preflight dry-run hook is
  # set AND no dry-run-freshness state was explicitly requested — that
  # keeps existing bats cases (which only stub SSH reachability)
  # passing unchanged. Bats cases that exercise the freshness branch
  # set HBIRD_SSH_WRAP_DRY_RUN_FRESHNESS=<state> to drive the helper.
  if [[ "${HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT:-0}" != 1 || -n "${HBIRD_SSH_WRAP_DRY_RUN_FRESHNESS:-}" ]]; then
    local local_repo_dir
    local_repo_dir="$(cd "$(dirname "$self")/.." && pwd)"
    if ! hbird_ssh_wrap_check_remote_freshness "$local_repo_dir" "$script_basename"; then
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
