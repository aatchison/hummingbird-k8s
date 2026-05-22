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
#               control-plane node (via scripts/kubectl-k8s.sh).
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
set -euo pipefail

SNAP="${1:?Usage: $0 <snapshot.db>}"
[[ -f "$SNAP" ]] || { echo "Snapshot not found: $SNAP" >&2; exit 1; }

CP_IP="${CP_IP:-$(./scripts/kubectl-k8s.sh get nodes \
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
./scripts/kubectl-k8s.sh get nodes
