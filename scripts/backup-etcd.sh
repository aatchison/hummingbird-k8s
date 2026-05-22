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
#   scripts/backup-etcd.sh [outdir]    # default outdir: ./backups
#
# Env:
#   CP_IP     — CP node IP. Defaults to the InternalIP of the first
#               control-plane node (via scripts/kubectl-k8s.sh).
#   KVM_HOST  — Optional SSH ProxyJump host. If set, the SSH/SCP to
#               the CP routes through this host. Useful when the CP
#               VM's IP is only reachable from inside the KVM host's
#               libvirt NAT (the common topology for this repo).
#
# Output:
#   $outdir/etcd-snapshot-<UTC-timestamp>.db
#
# See docs/backup-restore.md for cadence / restore guidance.
set -euo pipefail

OUTDIR="${1:-./backups}"
mkdir -p "$OUTDIR"
TS=$(date -u +%Y%m%dT%H%M%SZ)
DST="$OUTDIR/etcd-snapshot-$TS.db"

CP_IP="${CP_IP:-$(./scripts/kubectl-k8s.sh get nodes \
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
