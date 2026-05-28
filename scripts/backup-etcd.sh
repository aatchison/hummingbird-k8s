#!/usr/bin/env bash
# Take an etcd snapshot of the running control plane via crictl exec.
#
# The k8s control plane runs etcd as a static pod whose image is
# `registry.k8s.io/etcd:*-distroless` — there is no shell inside, so
# `kubectl exec etcd-<node> -- sh -c '...'` does not work. We instead
# SSH to the CP, find the etcd container ID via crictl, and run
# `etcdctl snapshot save` directly inside that container (no shell
# wrapper), then scp the file back.
#
# Usage:
#   scripts/backup-etcd.sh [outdir] [--label <text>]
#   # default outdir: ./backups
#
# Flags:
#   --label <text>  Append `-<text>` to the snapshot filename so the
#                   reason for the snapshot is obvious on disk. The
#                   label is sanitized to [A-Za-z0-9._-]. Example:
#                     scripts/backup-etcd.sh ~/backups --label pre-cni-swap
#                     -> ~/backups/etcd-snapshot-20260522T180000Z-pre-cni-swap.db
#                   See docs/backup-restore.md ("When to snapshot")
#                   for the high-risk operations that warrant a
#                   labeled snapshot first.
#
# Env:
#   CP_IP     — CP node IP. Defaults to the InternalIP of the first
#               control-plane node (via `hbird kubectl get nodes`).
#               # Cross-runtime dependency (v0.1.0 cutover, #353):
#               # `hbird` CLI is required at runtime when CP_IP is
#               # unset; the Rust twin replaced scripts/kubectl-k8s.sh.
#               # `hbird kubectl` auto-resolves CP_IP via
#               # `ssh $KVM_HOST virsh -c qemu:///system domifaddr
#               # $CP_NAME` (PR #366 round-2 H1), so workstation
#               # operators only need KVM_HOST set — same shape as the
#               # deleted scripts/kubectl-k8s.sh.
#   KVM_HOST  — Optional SSH ProxyJump host. If set, the SSH/SCP to
#               the CP routes through this host. Useful when the CP
#               VM's IP is only reachable from inside the KVM host's
#               libvirt NAT (the common topology for this repo).
#
# Output:
#   $outdir/etcd-snapshot-<UTC-timestamp>[-<label>].db
#
# See docs/backup-restore.md for cadence / restore guidance.
#
# Path-anchoring: this script anchors REPO_ROOT for any future sibling
# script references; today every kubectl call routes through `hbird
# kubectl` directly. See issue #271 F6 and #353 cutover.
set -euo pipefail

# PR #366 round-2 M1: hbird-PATH preflight. `hbird kubectl` is the
# canonical kubectl entry point after #353; without it on PATH the
# CP_IP probe below fails with a less-actionable "command not found".
command -v hbird >/dev/null || {
  echo "ERROR: hbird CLI required for ${0##*/} (post-#353 cutover)" >&2
  echo "       Install per docs/rust-cli.md" >&2
  exit 2
}

# Resolve repo root from this script's location so relative siblings
# work from any cwd. Matches the pattern established by
# scripts/run-kube-bench.sh. After the v0.1.0 cutover (#353), this
# script no longer references any sibling scripts (kubectl moved to
# `hbird kubectl`); REPO_ROOT is preserved as preamble boilerplate so
# future sibling references don't have to re-derive it.
# shellcheck disable=SC2034  # preserved for future sibling-script use
REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

OUTDIR=""
LABEL=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --label)
      [[ $# -ge 2 ]] || { echo "--label requires a value" >&2; exit 2; }
      LABEL="$2"
      shift 2
      ;;
    --label=*)
      LABEL="${1#--label=}"
      shift
      ;;
    -h|--help)
      sed -n '2,30p' "$0"
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "Unknown flag: $1" >&2
      exit 2
      ;;
    *)
      if [[ -z "$OUTDIR" ]]; then
        OUTDIR="$1"
        shift
      else
        echo "Unexpected positional arg: $1" >&2
        exit 2
      fi
      ;;
  esac
done

OUTDIR="${OUTDIR:-./backups}"
mkdir -p "$OUTDIR"
TS=$(date -u +%Y%m%dT%H%M%SZ)

# Sanitize label: keep only [A-Za-z0-9._-]; reject empty post-sanitize.
SUFFIX=""
if [[ -n "$LABEL" ]]; then
  CLEAN_LABEL=$(printf '%s' "$LABEL" | tr -c 'A-Za-z0-9._-' '-' | sed 's/^-*//; s/-*$//')
  [[ -n "$CLEAN_LABEL" ]] || { echo "--label resolved to empty after sanitize" >&2; exit 2; }
  SUFFIX="-$CLEAN_LABEL"
fi
DST="$OUTDIR/etcd-snapshot-$TS$SUFFIX.db"

# Cross-runtime dependency (v0.1.0 cutover, #353):
# `hbird` CLI is required at runtime to resolve CP_IP from the cluster.
# scripts/kubectl-k8s.sh was removed in v0.1.0; the Rust twin
# (`hbird kubectl`) is the canonical kubectl entry point. `hbird` is
# preflight-checked at the top of this script, so this step won't fail
# with command-not-found. `hbird kubectl` auto-resolves CP_IP via
# virsh-domifaddr on KVM_HOST (PR #366 round-2 H1) when CP_IP isn't
# pinned in CONFIG.
CP_IP="${CP_IP:-$(hbird kubectl get nodes \
  -l node-role.kubernetes.io/control-plane \
  -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')}"
[[ -n "$CP_IP" ]] || { echo "Could not resolve control-plane IP" >&2; exit 1; }

SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=no)
[[ -n "${KVM_HOST:-}" ]] && SSH_OPTS+=(-o ProxyJump="$KVM_HOST")

echo "Snapshotting etcd on $CP_IP -> $DST"
ssh "${SSH_OPTS[@]}" "root@$CP_IP" "
  set -euo pipefail
  ETCD=\$(crictl ps --name etcd -q | head -1)
  [[ -n \"\$ETCD\" ]] || { echo 'no etcd container found' >&2; exit 1; }
  crictl exec \"\$ETCD\" etcdctl \
    --cacert=/etc/kubernetes/pki/etcd/ca.crt \
    --cert=/etc/kubernetes/pki/etcd/server.crt \
    --key=/etc/kubernetes/pki/etcd/server.key \
    snapshot save /tmp/snapshot.db
  crictl exec \"\$ETCD\" etcdctl --write-out=table snapshot status /tmp/snapshot.db
" >/dev/null

# Pull the file out of the etcd container's /tmp (which is the host's
# /var/lib/kubelet/pods/.../volumes/.../tmp via the static pod mount —
# but `crictl exec ... snapshot save /tmp/snapshot.db` writes to the
# container's view, which for the etcd static pod maps to the host's
# /var/lib/etcd/... area in practice). On distroless etcd images the
# safer path is to copy via `crictl cp` if available, but plain scp of
# /tmp/snapshot.db works because the etcd static pod has /tmp from the
# host bind-mounted in the kubelet-managed pod sandbox.
#
# If `scp` of /tmp/snapshot.db fails on a future etcd image that no
# longer bind-mounts host /tmp, fall back to:
#   ssh root@$CP_IP "crictl cp \$ETCD:/tmp/snapshot.db /tmp/snapshot.db"
# then scp from there.
scp "${SSH_OPTS[@]}" "root@$CP_IP:/tmp/snapshot.db" "$DST"
ssh "${SSH_OPTS[@]}" "root@$CP_IP" "rm -f /tmp/snapshot.db"

echo "Saved: $DST  ($(du -h "$DST" | cut -f1))"
