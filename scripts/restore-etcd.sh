#!/usr/bin/env bash
# Restore etcd from a snapshot taken by scripts/backup-etcd.sh.
#
# WARNING: This is destructive. It stops the apiserver (by moving
# /etc/kubernetes/manifests aside), replaces /var/lib/etcd with a
# freshly-restored data dir, and restarts kubelet. Existing etcd state
# on the CP is moved to /var/lib/etcd.before-restore.<ts> rather than
# deleted, so a botched restore can be reverted by hand.
#
# Usage:
#   scripts/restore-etcd.sh <snapshot.db>
#
# Env:
#   CP_IP     — CP node IP. Defaults to the InternalIP of the first
#               control-plane node (via `hbird kubectl get nodes`).
#               # Cross-runtime dependency (v0.1.0 cutover, #353):
#               # `hbird` CLI replaced scripts/kubectl-k8s.sh.
#               # `hbird kubectl` auto-resolves CP_IP via
#               # `ssh $KVM_HOST virsh ... domifaddr $CP_NAME` (PR #366
#               # round-2 H1); workstation operators only need KVM_HOST.
#   KVM_HOST  — Optional SSH ProxyJump host (see backup-etcd.sh).
#
# Caveats:
#   - The etcd image used for `etcdctl snapshot restore` is auto-detected
#     from `crictl images`. If detection fails or the wrong image is
#     picked, override by editing the script — or, manually:
#       podman run --rm --network host -v /var/lib:/var/lib -v /tmp:/tmp \
#         registry.k8s.io/etcd:<your-version> \
#         etcdctl snapshot restore /tmp/restore-snapshot.db \
#         --data-dir=/var/lib/etcd
#   - If the cluster has etcd encryption-at-rest enabled (see
#     docs/etcd-encryption.md), the restored snapshot is still encrypted
#     and needs the SAME encryption key. The key lives at
#     /etc/kubernetes/encryption-config.yaml on the CP and is NOT part of
#     the snapshot — back it up separately.
#
# Path-anchoring: this script resolves the sibling kubectl-k8s.sh via
# $REPO_ROOT derived from its own location, so it works no matter what
# cwd the operator runs it from. See issue #271 F6.
set -euo pipefail

# PR #366 round-2 M1: hbird-PATH preflight. `hbird kubectl` is the
# canonical kubectl entry point after #353; without it on PATH the
# CP_IP probe + final verify below fail with a less-actionable
# "command not found".
command -v hbird >/dev/null || {
  echo "ERROR: hbird CLI required for ${0##*/} (post-#353 cutover)" >&2
  echo "       Install per docs/rust-cli.md" >&2
  exit 2
}

# Resolve repo root from this script's location so relative siblings
# work from any cwd. Matches the pattern established by
# scripts/run-kube-bench.sh.
# (Cross-runtime dependency, v0.1.0 cutover, #353: every kubectl call
# now routes through `hbird kubectl` directly; REPO_ROOT is preserved
# as preamble boilerplate for any future sibling-script references.)
# shellcheck disable=SC2034  # preserved for future sibling-script use
REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

SNAP="${1:?Usage: $0 <snapshot.db>}"
[[ -f "$SNAP" ]] || { echo "Snapshot not found: $SNAP" >&2; exit 1; }

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

# Walk the operator through it — do not silently destroy state.
cat <<EOF
About to restore etcd on $CP_IP from $SNAP.

This will:
  1. Move /etc/kubernetes/manifests aside so the apiserver+etcd static
     pods stop.
  2. Stop kubelet.
  3. Rename /var/lib/etcd to /var/lib/etcd.before-restore.<ts>.
  4. Run 'etcdctl snapshot restore' into a fresh /var/lib/etcd.
  5. Restore the manifests directory and start kubelet so the apiserver
     comes back up against the restored etcd.

Press Enter to continue, Ctrl-C to abort.
EOF
read -r _

scp "${SSH_OPTS[@]}" "$SNAP" "root@$CP_IP:/tmp/restore-snapshot.db"

TS=$(date -u +%Y%m%dT%H%M%SZ)
# shellcheck disable=SC2087  # intentional: heredoc body evaluated remotely
ssh "${SSH_OPTS[@]}" "root@$CP_IP" "TS=$TS bash -s" <<'REMOTE'
set -euo pipefail
mv /etc/kubernetes/manifests /etc/kubernetes/manifests.disabled
sleep 10  # give kubelet time to notice and tear the static pods down
systemctl stop kubelet || true
mv /var/lib/etcd "/var/lib/etcd.before-restore.${TS}"

# Best-effort: locate the etcd image already pulled on this CP so we
# don't have to network out for the restore.
ETCD_IMG=$(crictl images 2>/dev/null \
  | awk '/registry\.k8s\.io\/etcd/{print $1":"$2; exit}')
[[ -n "$ETCD_IMG" ]] || ETCD_IMG="registry.k8s.io/etcd:3.5.15-0"

echo "Using etcd image: $ETCD_IMG"
podman run --rm --network host \
  -v /var/lib:/var/lib -v /tmp:/tmp \
  "$ETCD_IMG" \
  etcdctl snapshot restore /tmp/restore-snapshot.db \
    --data-dir=/var/lib/etcd

mv /etc/kubernetes/manifests.disabled /etc/kubernetes/manifests
systemctl start kubelet
rm -f /tmp/restore-snapshot.db
echo 'Restore complete. Apiserver will come back up in ~30s.'
REMOTE

echo "Verifying cluster (after a 30s warm-up)..."
sleep 30
# Cross-runtime dependency (v0.1.0 cutover, #353): hbird CLI required.
hbird kubectl get nodes
