#!/usr/bin/env bash
# Shared SSH/virsh/log helpers live in lib/build-common.sh; see docs/development.md.
# scripts/export-argocd.sh — produce an ArgoCD-registerable kubeconfig.
#
# ArgoCD's `cluster add` command consumes a normal kubeconfig and uses it
# ONCE at registration time to bootstrap a ServiceAccount + scoped token
# in the target cluster. After that it discards the original credential
# and authenticates via the SA token it minted. So all we need to hand
# ArgoCD is a valid kubeconfig that points at a reachable apiserver and
# carries enough authority to create that SA.
#
# The simplest such file is /etc/kubernetes/admin.conf on the CP — but
# that's a 0600 root-owned file carrying the cluster signing CA and a
# full-cluster client cert. This script:
#
#   1. SSHes to the CP using the same identity flow as deploy-cluster.sh
#      (private key paired with SSH_PUBKEY_FILE from CONFIG).
#   2. `sudo cat`s admin.conf and streams it back over SSH into a
#      umask-077 temp file.
#   3. Rewrites the `server:` URL so ArgoCD can reach the apiserver from
#      wherever it's running (kubeadm bakes in the CP node's local IP,
#      which often isn't routable from the ArgoCD pod).
#   4. Rewrites the kubeadm default cluster/context/user names from
#      `kubernetes` to the operator-supplied --context-name, so the file
#      doesn't collide with the operator's other kubeconfigs and so
#      ArgoCD registers the cluster under a meaningful name.
#   5. Writes to --output with mode 0600. Never echoes admin.conf
#      contents to stdout.
#
# Security: the output IS admin.conf — a full-cluster credential. After
# `argocd cluster add` succeeds, ArgoCD only needs the SA token it
# created; delete or store the file 0600 once registration is done. If
# the file leaks, follow the kubeadm cert rotation procedure (see
# docs/argocd.md).
#
# Usage:
#   CONFIG=cluster.local.conf bash scripts/export-argocd.sh [flags]
#
# Flags:
#   --output FILE         Output path (default: ./argocd-kubeconfig.yaml)
#   --server URL          Override apiserver URL (default: https://<CP_IP>:6443)
#   --context-name NAME   Cluster/context/user name (default: hummingbird-<CP_NAME>)
#   --force               Overwrite an existing output file

set -euo pipefail

# Resolve via BASH_SOURCE so we work both when executed and when sourced
# (tests source this script to call rewrite_kubeconfig directly).
SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
# shellcheck source=../lib/build-common.sh
source "${REPO_ROOT}/lib/build-common.sh"
setup_logging "[export-argocd]"

# ---- detect yq flavor (callable from tests via sourced-mode) ----------------
# There are two unrelated tools that ship as `yq`:
#   - mikefarah/yq    (Go)     — jq-like expressions, supports `-i` and
#                                 ".clusters[0].cluster.server = ..." syntax.
#   - kislyuk/yq      (Python) — wraps jq, different syntax + different flags.
# Only the Go flavor speaks the expressions below. Probe explicitly and
# fall through to the (line-anchored) sed path otherwise.
#
# Exits 0 with stdout set to "1" if Go yq is present; "0" otherwise.
detect_yq_flavor() {
  if command -v yq >/dev/null 2>&1; then
    if yq --version 2>&1 | grep -qiE 'mikefarah|go-yaml'; then
      echo 1
      return 0
    fi
  fi
  echo 0
}

# ---- rewrite_kubeconfig <admin_conf_path> <server_url> <context_name> ------
# In-place rewrite of a kubeadm-shaped admin.conf:
#   1. Rewrites the `server:` URL to <server_url>.
#   2. Rewrites the kubeadm-default cluster=kubernetes / user=kubernetes-admin
#      / context=kubernetes-admin@kubernetes names to <context_name>.
#
# Prefers Go yq when present (structure-aware); falls back to a constrained
# sed that only touches the canonical kubeadm-emitted key lines. The sed
# path is deliberately line-anchored so it does NOT rewrite the substring
# "kubernetes" inside comments or base64 cert / key blobs.
#
# Exposed as a function so tests/scripts/export-argocd.bats can source this
# script and call rewrite_kubeconfig directly without triggering the SSH
# fetch + write flow at the bottom of the file.
rewrite_kubeconfig() {
  local kc="$1" server_url="$2" context_name="$3"
  local use_yq
  use_yq="$(detect_yq_flavor)"

  if [[ "$use_yq" -eq 1 ]]; then
    log "rewriting server URL via yq -> ${server_url}"
    yq -i ".clusters[0].cluster.server = \"${server_url}\"" "$kc"
  else
    log "rewriting server URL via sed -> ${server_url}"
    local esc_url
    esc_url="$(printf '%s\n' "$server_url" | sed -e 's/[\/&]/\\&/g')"
    sed -i -E "s|^([[:space:]]+server:[[:space:]]+).*$|\1${esc_url}|" "$kc"
  fi

  if [[ "$use_yq" -eq 1 ]]; then
    log "rewriting cluster/context/user names via yq -> ${context_name}"
    yq -i "
      .clusters[0].name = \"${context_name}\" |
      .users[0].name = \"${context_name}\" |
      .contexts[0].name = \"${context_name}\" |
      .contexts[0].context.cluster = \"${context_name}\" |
      .contexts[0].context.user = \"${context_name}\" |
      .current-context = \"${context_name}\"
    " "$kc"
  else
    log "rewriting cluster/context/user names via sed -> ${context_name}"
    # Anchor each substitution to the specific kubeconfig key line. Global
    # substitution of "kubernetes" → context_name would also rewrite the
    # string inside YAML comments (e.g. "# kubernetes-the-platform note")
    # and — far worse — could collide with base64-encoded cert / key data
    # where "kubernetes" happens to appear as a literal substring. We
    # target the canonical kubeadm-emitted lines and nothing else; this
    # is structurally equivalent to the yq edits above.
    # Use '#' as the sed delimiter — our regexes contain literal '|' for
    # alternation, which would otherwise collide with the default '|' s||
    # delimiter. Escape '#' in context_name just in case (though our
    # context-name validator already forbids it).
    local esc_ctx
    esc_ctx="$(printf '%s\n' "$context_name" | sed -e 's/[\/&#]/\\&/g')"
    # Six anchored substitutions, one per kubeadm-emitted key line. Order
    # the composite "kubernetes-admin@kubernetes" pattern BEFORE the
    # "kubernetes-admin" and bare "kubernetes" patterns so we don't
    # partial-match the composite first.
    sed -i -E \
      -e "s#^(([[:space:]]+|- )name:[[:space:]]+)kubernetes-admin@kubernetes\$#\1${esc_ctx}#" \
      -e "s#^(current-context:[[:space:]]+)kubernetes-admin@kubernetes\$#\1${esc_ctx}#" \
      -e "s#^(([[:space:]]+|- )name:[[:space:]]+)kubernetes-admin\$#\1${esc_ctx}#" \
      -e "s#^([[:space:]]+user:[[:space:]]+)kubernetes-admin\$#\1${esc_ctx}#" \
      -e "s#^(([[:space:]]+|- )name:[[:space:]]+)kubernetes\$#\1${esc_ctx}#" \
      -e "s#^([[:space:]]+cluster:[[:space:]]+)kubernetes\$#\1${esc_ctx}#" \
      "$kc"
  fi
}

# Sourced for testing? Short-circuit before the main flow so bats can call
# the helper functions above without triggering ssh / config-source / write.
if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
  return 0
fi

# ---- CONFIG + flag parsing --------------------------------------------------

: "${CONFIG:?CONFIG=<path-to-cluster.local.conf> is required}"
[[ -r "$CONFIG" ]] || fail "config not readable: $CONFIG"

OUTPUT=""
SERVER_OVERRIDE=""
CONTEXT_OVERRIDE=""
FORCE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)        OUTPUT="${2:-}";          shift 2 ;;
    --output=*)      OUTPUT="${1#*=}";          shift ;;
    --server)        SERVER_OVERRIDE="${2:-}"; shift 2 ;;
    --server=*)      SERVER_OVERRIDE="${1#*=}"; shift ;;
    --context-name)  CONTEXT_OVERRIDE="${2:-}"; shift 2 ;;
    --context-name=*) CONTEXT_OVERRIDE="${1#*=}"; shift ;;
    --force)         FORCE=1; shift ;;
    -h|--help)
      sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) fail "unknown flag: $1" ;;
  esac
done

# Validate --server BEFORE any interpolation. The override (if any) gets
# spliced into a yq expression and/or a sed s|| substitution downstream;
# both are sensitive to embedded quotes / shell metacharacters. Mirror the
# conservative regex we use for --context-name below — strict enough to
# rule out injection, permissive enough for ipv4 / hostnames / ports /
# paths that operators actually use.
if [[ -n "$SERVER_OVERRIDE" ]]; then
  if ! [[ "$SERVER_OVERRIDE" =~ ^https?://[A-Za-z0-9._:-]+(/[A-Za-z0-9._/-]*)?$ ]]; then
    fail "invalid --server URL: '$SERVER_OVERRIDE' (expected https://host[:port][/path])"
  fi
fi

# ---- source CONFIG ----------------------------------------------------------

# shellcheck disable=SC1090
source "$CONFIG"

: "${CP_NAME:?CP_NAME is required in $CONFIG}"
: "${SSH_PUBKEY_FILE:?SSH_PUBKEY_FILE is required in $CONFIG}"

# Expand any ~ / $HOME in SSH_PUBKEY_FILE the same way deploy-cluster.sh does
# (it's sourced as bash, so the operator may use $HOME). Already-expanded
# paths are no-ops here.
SSH_PUBKEY_FILE="${SSH_PUBKEY_FILE/#\~/$HOME}"

[[ -r "$SSH_PUBKEY_FILE" ]] || fail "SSH_PUBKEY_FILE not readable: $SSH_PUBKEY_FILE"
SSH_PRIVKEY_FILE="$(derive_ssh_privkey_file "$SSH_PUBKEY_FILE")" \
  || fail "SSH private key not readable next to $SSH_PUBKEY_FILE"

# ---- resolve CP_IP ----------------------------------------------------------
# CP_IP can come from the config directly (operator pinned), or be resolved
# via libvirt against CP_NAME on the local KVM host (resolve_vm_ip helper).

if [[ -z "${CP_IP:-}" ]] && command -v virsh >/dev/null 2>&1; then
  CP_IP="$(resolve_vm_ip "$CP_NAME" 2>/dev/null || true)"
fi
[[ -n "${CP_IP:-}" ]] || fail "could not resolve CP_IP — set CP_IP=<ip> in $CONFIG or run from the KVM host with libvirt access to '$CP_NAME'"

# ---- derive defaults --------------------------------------------------------

OUTPUT="${OUTPUT:-${PWD}/argocd-kubeconfig.yaml}"
SERVER_URL="${SERVER_OVERRIDE:-https://${CP_IP}:6443}"
CONTEXT_NAME="${CONTEXT_OVERRIDE:-hummingbird-${CP_NAME}}"

# Validate context name — kubeconfig names can be reasonably permissive but
# ArgoCD treats them as URL-safe IDs. Keep it conservative.
[[ "$CONTEXT_NAME" =~ ^[A-Za-z0-9._-]+$ ]] \
  || fail "--context-name must match [A-Za-z0-9._-]+ (got: '$CONTEXT_NAME')"

if [[ -e "$OUTPUT" && "$FORCE" -ne 1 ]]; then
  fail "output file already exists: $OUTPUT (re-run with --force to overwrite)"
fi

# ---- SSH helper -------------------------------------------------------------

ssh_opts_array CP_SSH_OPTS
cp_ssh() { ssh "${CP_SSH_OPTS[@]}" "root@${CP_IP}" "$@"; }

# ---- pull admin.conf --------------------------------------------------------
# umask 077 in the subshell so the temp file is mode 0600 from creation —
# never world-readable, even transiently. We `sudo cat` because admin.conf
# is 0600 root, and the SSH user may or may not be root (the deploy flow
# logs in as root, so `sudo` is a passthrough; we keep `sudo` for cases
# where the operator wires up a non-root SSH user later).

TMP_KUBECONFIG="$(umask 077 && mktemp -t argocd-kubeconfig-XXXXXX.yaml)"
cleanup() { [[ -n "${TMP_KUBECONFIG:-}" && -e "$TMP_KUBECONFIG" ]] && rm -f "$TMP_KUBECONFIG"; }
trap cleanup EXIT

log "fetching /etc/kubernetes/admin.conf from root@${CP_IP}"
# Let ssh's own stderr flow through to the operator — host-key mismatches,
# Permission denied (publickey), sudo-no-tty, connection-refused all need
# to be visible. The post-check fail() just appends a reminder.
if ! cp_ssh "sudo cat /etc/kubernetes/admin.conf" > "$TMP_KUBECONFIG"; then
  fail "ssh root@${CP_IP} 'sudo cat /etc/kubernetes/admin.conf' failed — see ssh diagnostic above (is the CP up and reachable on $SSH_PRIVKEY_FILE?)"
fi
[[ -s "$TMP_KUBECONFIG" ]] || fail "fetched admin.conf is empty"

# Quick sanity check: looks like a kubeconfig?
if ! grep -q '^apiVersion:' "$TMP_KUBECONFIG" \
  || ! grep -q '^kind: Config' "$TMP_KUBECONFIG"; then
  fail "fetched file does not look like a kubeconfig (no 'kind: Config'). Refusing to continue."
fi

# ---- rewrite server URL + cluster/context/user names -----------------------
# Delegated to rewrite_kubeconfig() (defined at top of file) so the same
# routine is callable from tests/scripts/export-argocd.bats via sourced-mode.

rewrite_kubeconfig "$TMP_KUBECONFIG" "$SERVER_URL" "$CONTEXT_NAME"

# ---- move into place with 0600 ----------------------------------------------
# `install -m 0600` is atomic-mode-set: the destination is created at the
# requested mode in one step, no chmod-after-mv race window.

install -m 0600 "$TMP_KUBECONFIG" "$OUTPUT"
rm -f "$TMP_KUBECONFIG"
# Disarm the EXIT trap — TMP_KUBECONFIG no longer exists.
trap - EXIT

log "kubeconfig written to ${OUTPUT}"
log "register with:  argocd cluster add ${CONTEXT_NAME} --kubeconfig ${OUTPUT}"
log "sanity check:   KUBECONFIG=${OUTPUT} kubectl get nodes"
