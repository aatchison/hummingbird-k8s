#!/usr/bin/env bash
# integration-export-argocd.sh — drive scripts/export-argocd.sh assertions.
#
# Used by .github/workflows/integration-export-argocd.yml. Outside the
# YAML so shellcheck can lint it and the test logic can be reviewed as
# bash rather than embedded `run: |` blocks (matches the
# tests/integration-cloud-init.sh convention).
#
# Sub-commands:
#   test-basic-export       T1: make export-argocd → assert 0600,
#                           apiVersion/kind, rewritten server URL,
#                           rewritten context name.
#   test-kubeconfig-usable  T2: KUBECONFIG=<exported> kubectl get nodes
#                           returns CP_NAME Ready.
#   test-refuse-clobber     T3: rerun without FORCE=1 must exit non-zero
#                           and surface "already exists" / "--force".
#   test-force-overwrite    T4: FORCE=1 advances mtime.
#   test-hostile-server     T5: --server with shell metacharacters must
#                           be rejected by the script's own validator
#                           BEFORE any rewrite touches the filesystem.
#                           Bypasses `make` to keep the test isolated
#                           from Make's literal $(SERVER) substitution
#                           into the recipe shell (which is what shipped
#                           the BLOCKER in the original PR).
#   test-get-kubeconfig     T6: optional sibling target from PR #197.
#                           Skipped if `make get-kubeconfig` is absent.
#   verify-nodes-by-name    Confirm CP_NAME and every WORKER_NAMES entry
#                           appear in `kubectl get nodes` as Ready. Used
#                           by the workflow's post-deploy gate so we
#                           assert by-name, not just by count.
#
# All sub-commands source $CONFIG to get CP_NAME / WORKER_NAMES.
# $SSH_PRIVKEY_FILE defaults to ~/.ssh/integration_test_key (the workflow
# generates it per-run).

set -euo pipefail

CONFIG="${CONFIG:?CONFIG=<path-to-cluster.ci.conf> is required}"
[[ -r "$CONFIG" ]] || { echo "config not readable: $CONFIG" >&2; exit 2; }

# shellcheck disable=SC1090
source "$CONFIG"

SSH_PRIVKEY_FILE="${SSH_PRIVKEY_FILE:-${HOME}/.ssh/integration_test_key}"

REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

ssh_root() {
  local host="$1"; shift
  ssh -i "$SSH_PRIVKEY_FILE" \
      -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR \
      -o ConnectTimeout=10 \
      -o BatchMode=yes \
      "root@${host}" "$@"
}

resolve_cp_ip() {
  sudo virsh -c qemu:///system domifaddr "$CP_NAME" 2>/dev/null \
    | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}'
}

# ---- verify-nodes-by-name ---------------------------------------------------

cmd_verify_nodes_by_name() {
  local CP_IP nodes
  CP_IP="$(resolve_cp_ip)"
  [[ -n "$CP_IP" ]] || { echo "::error::cannot resolve CP_IP for $CP_NAME" >&2; exit 1; }

  nodes="$(ssh_root "$CP_IP" \
    'kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes --no-headers')"
  printf '%s\n' "$nodes"

  local fail=0
  for name in "$CP_NAME" "${WORKER_NAMES[@]}"; do
    if ! grep -qE "^${name}[[:space:]]+Ready" <<<"$nodes"; then
      echo "::error::node ${name} is not Ready (or missing from kubectl output)" >&2
      fail=1
    fi
  done
  if (( fail != 0 )); then
    return 1
  fi
  echo "ok: CP_NAME + ${#WORKER_NAMES[@]} workers all Ready by name"
}

# ---- T1: basic export -------------------------------------------------------

cmd_test_basic_export() {
  rm -f /tmp/ci-kubeconfig.yaml
  ( cd "$REPO_ROOT" && \
      make export-argocd CONFIG="$CONFIG" OUTPUT=/tmp/ci-kubeconfig.yaml )

  [[ -f /tmp/ci-kubeconfig.yaml ]] \
    || { echo "::error::export did not produce /tmp/ci-kubeconfig.yaml" >&2; return 1; }

  local mode
  mode="$(stat -c '%a' /tmp/ci-kubeconfig.yaml)"
  if [[ "$mode" != "600" ]]; then
    echo "::error::expected mode 0600 on exported kubeconfig, got ${mode}" >&2
    return 1
  fi

  grep -q '^apiVersion: v1' /tmp/ci-kubeconfig.yaml \
    || { echo "::error::missing 'apiVersion: v1'" >&2; return 1; }
  grep -q '^kind: Config' /tmp/ci-kubeconfig.yaml \
    || { echo "::error::missing 'kind: Config'" >&2; return 1; }

  local CP_IP
  CP_IP="$(resolve_cp_ip)"
  if ! grep -qE "server:[[:space:]]+https://${CP_IP}:6443\$" /tmp/ci-kubeconfig.yaml; then
    echo "::error::server URL not rewritten to https://${CP_IP}:6443" >&2
    grep -E 'server:' /tmp/ci-kubeconfig.yaml || true
    return 1
  fi

  local want_ctx="hummingbird-${CP_NAME}"
  grep -qE "name:[[:space:]]+${want_ctx}\$" /tmp/ci-kubeconfig.yaml \
    || { echo "::error::expected context name '${want_ctx}' not present" >&2; return 1; }
  grep -qE "current-context:[[:space:]]+${want_ctx}\$" /tmp/ci-kubeconfig.yaml \
    || { echo "::error::current-context not set to '${want_ctx}'" >&2; return 1; }

  echo "ok: basic export produced a sane 0600 kubeconfig"
}

# ---- T2: usable kubeconfig --------------------------------------------------

cmd_test_kubeconfig_usable() {
  local out
  out="$(KUBECONFIG=/tmp/ci-kubeconfig.yaml \
         kubectl --insecure-skip-tls-verify=true get nodes --no-headers)"
  if ! grep -qE "^${CP_NAME}[[:space:]]+Ready" <<<"$out"; then
    echo "::error::exported kubeconfig does not list ${CP_NAME} as Ready" >&2
    echo "$out" >&2
    return 1
  fi
  echo "ok: kubectl via exported kubeconfig returned ${CP_NAME} Ready"
}

# ---- T3: refuse-to-clobber --------------------------------------------------

cmd_test_refuse_clobber() {
  set +e
  local out rc
  out="$(cd "$REPO_ROOT" && \
         make export-argocd CONFIG="$CONFIG" OUTPUT=/tmp/ci-kubeconfig.yaml 2>&1)"
  rc=$?
  set -e
  if [[ $rc -eq 0 ]]; then
    echo "::error::expected non-zero exit when output exists and FORCE!=1, got 0" >&2
    echo "$out" >&2
    return 1
  fi
  if ! grep -qE "already exists|--force" <<<"$out"; then
    echo "::error::refuse-to-clobber output did not mention the override flag" >&2
    echo "$out" >&2
    return 1
  fi
  echo "ok: refuse-to-clobber fired (rc=${rc})"
}

# ---- T4: FORCE=1 overwrites -------------------------------------------------

cmd_test_force_overwrite() {
  local before after
  before="$(stat -c '%Y' /tmp/ci-kubeconfig.yaml)"
  sleep 1  # advance coarse FS clocks
  ( cd "$REPO_ROOT" && \
      make export-argocd CONFIG="$CONFIG" OUTPUT=/tmp/ci-kubeconfig.yaml FORCE=1 )
  after="$(stat -c '%Y' /tmp/ci-kubeconfig.yaml)"
  if [[ "$after" -le "$before" ]]; then
    echo "::error::FORCE=1 did not overwrite (mtime ${before} -> ${after})" >&2
    return 1
  fi
  echo "ok: FORCE=1 overwrote the kubeconfig"
}

# ---- T5: hostile --server ---------------------------------------------------

# Bypass `make` entirely. Make's recipe expands `$(SERVER)` LITERALLY into
# the recipe shell, so a payload containing `";rm -rf /;\n` closes the
# shell quoting of the recipe and runs `rm -rf /` BEFORE the script's
# argument parser ever sees the value. Going around make exercises the
# real validation surface (scripts/export-argocd.sh's regex at line 83)
# without compromising the runner.
cmd_test_hostile_server() {
  rm -f /tmp/ci-bad.yaml
  set +e
  local out rc
  out="$(cd "$REPO_ROOT" && \
         CONFIG="$CONFIG" bash scripts/export-argocd.sh \
           --output /tmp/ci-bad.yaml \
           --server 'https://x";rm -rf /;\n' 2>&1)"
  rc=$?
  set -e
  if [[ $rc -eq 0 ]]; then
    echo "::error::hostile --server was accepted (script returned 0)" >&2
    echo "$out" >&2
    return 1
  fi
  if [[ -e /tmp/ci-bad.yaml ]]; then
    echo "::error::output file was created despite rejection" >&2
    ls -l /tmp/ci-bad.yaml >&2 || true
    return 1
  fi
  if ! grep -qE "invalid --server" <<<"$out"; then
    echo "::error::expected 'invalid --server' diagnostic, got:" >&2
    echo "$out" >&2
    return 1
  fi
  echo "ok: hostile --server rejected before any rewrite (rc=${rc})"
}

# ---- T6: get-kubeconfig sibling (PR #197 — gated) ---------------------------

cmd_test_get_kubeconfig() {
  if ! ( cd "$REPO_ROOT" && make help 2>/dev/null ) \
       | grep -qE '^[[:space:]]+get-kubeconfig'; then
    echo "skip: make get-kubeconfig is not yet present (PR #197 unmerged)"
    return 0
  fi

  rm -f /tmp/ci-get.yaml
  ( cd "$REPO_ROOT" && \
      make get-kubeconfig CONFIG="$CONFIG" OUTPUT=/tmp/ci-get.yaml )

  [[ -f /tmp/ci-get.yaml ]] \
    || { echo "::error::get-kubeconfig did not produce /tmp/ci-get.yaml" >&2; return 1; }
  grep -qE "name:[[:space:]]+${CP_NAME}\$" /tmp/ci-get.yaml \
    || { echo "::error::get-kubeconfig missing CP_NAME context '${CP_NAME}'" >&2; return 1; }
  if grep -qE "name:[[:space:]]+hummingbird-${CP_NAME}\$" /tmp/ci-get.yaml; then
    echo "::error::get-kubeconfig leaked hummingbird- prefix into context name" >&2
    return 1
  fi

  local out
  out="$(KUBECONFIG=/tmp/ci-get.yaml \
         kubectl --insecure-skip-tls-verify=true get nodes --no-headers)"
  if ! grep -qE "^${CP_NAME}[[:space:]]+Ready" <<<"$out"; then
    echo "::error::get-kubeconfig output is not usable against the cluster" >&2
    echo "$out" >&2
    return 1
  fi
  echo "ok: get-kubeconfig sibling works and uses bare CP_NAME context"
}

# ---- dispatch ---------------------------------------------------------------

cmd="${1:-}"
shift || true
case "$cmd" in
  verify-nodes-by-name)   cmd_verify_nodes_by_name "$@" ;;
  test-basic-export)      cmd_test_basic_export "$@" ;;
  test-kubeconfig-usable) cmd_test_kubeconfig_usable "$@" ;;
  test-refuse-clobber)    cmd_test_refuse_clobber "$@" ;;
  test-force-overwrite)   cmd_test_force_overwrite "$@" ;;
  test-hostile-server)    cmd_test_hostile_server "$@" ;;
  test-get-kubeconfig)    cmd_test_get_kubeconfig "$@" ;;
  *)
    echo "usage: CONFIG=<path> $0 {verify-nodes-by-name|test-basic-export|test-kubeconfig-usable|test-refuse-clobber|test-force-overwrite|test-hostile-server|test-get-kubeconfig}" >&2
    exit 2
    ;;
esac
