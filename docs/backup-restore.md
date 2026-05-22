# etcd backup and restore

This document covers how `hummingbird-k8s` backs up the cluster's etcd
state, how to restore from a snapshot, and the operational caveats
that follow.

The scripts live at `scripts/backup-etcd.sh` and
`scripts/restore-etcd.sh`; the `make backup-etcd` and `make
restore-etcd` targets are thin wrappers around them.

## What an etcd snapshot covers

An etcd snapshot is a point-in-time dump of the entire Kubernetes
API server's state — everything the apiserver persists is in etcd:

- Nodes, pods, deployments, statefulsets, daemonsets, jobs,
  cronjobs.
- Services, endpoints, ingresses, NetworkPolicies.
- Secrets, ConfigMaps (encrypted at rest if
  [`docs/etcd-encryption.md`](etcd-encryption.md) is enabled).
- RBAC (Roles, RoleBindings, ClusterRoles, ClusterRoleBindings),
  ServiceAccounts.
- CRDs and any custom resources stored in etcd.

What an etcd snapshot does **not** cover:

- PersistentVolume data. PVs are not in scope today (single-node
  topology, no CSI), but if you add one later the snapshot will
  contain only the PV/PVC objects, not the bytes inside the
  volume. Back up the underlying storage separately.
- In-flight workload data (in-memory state of running pods).
- The etcd encryption key. If
  [`docs/etcd-encryption.md`](etcd-encryption.md) is active, the
  on-disk envelope in the snapshot is encrypted with the key at
  `/etc/kubernetes/encryption-config.yaml` on the CP. Restoring to a
  different VM requires the same key — back the key file up
  separately, ideally to a different host or password manager.
- Per-VM kubelet certs, the kubeadm CA, and other PKI under
  `/etc/kubernetes/pki/`. These are needed to rejoin workers if the
  CP is rebuilt; back them up alongside the snapshot if you plan to
  restore onto a fresh VM rather than rebuild the cluster.

## Cadence

Pick the cadence that matches your acceptable data-loss window.

| Environment | Recommended cadence | Offload |
| --- | --- | --- |
| Lab / homelab | Daily | Local disk is fine. |
| Pre-production | Hourly | Off-host copy (S3, MinIO, NAS). |
| Production | Hourly + before any cluster change | Off-host **and** off-site. |

Schedule via `cron`, a systemd timer, or a CI job — the script is
non-interactive. Example crontab on the orchestrator host:

```cron
# Daily etcd backup at 02:15 UTC; keep 14 days.
15 2 * * * KVM_HOST=geary /home/ops/hummingbird-k8s/scripts/backup-etcd.sh /var/backups/etcd \
            && find /var/backups/etcd -name 'etcd-snapshot-*.db' -mtime +14 -delete
```

For anything beyond a lab, also offload to object storage (e.g.
`aws s3 cp $DST s3://bucket/etcd/`) — a snapshot that lives only on
the same host as the cluster does not protect you from host loss.

## When to snapshot

Take a **labeled** snapshot **before** any of these operations — they
alter etcd state in ways `bootc rollback` cannot undo (`bootc
rollback` restores the previous filesystem deployment, not etcd
state — see [`docs/rollback.md`](rollback.md) and issue #95):

- **CNI swap** (e.g. flannel ↔ Cilium) — Cilium CRDs, IP allocations,
  and policy objects persist in etcd across a `bootc rollback`;
  without a pre-swap snapshot the swap is effectively one-way. See
  [`docs/cilium-migration.md#rollback-limitations`](cilium-migration.md#rollback-limitations).
- **Kubernetes major-version upgrade** — `kubeadm upgrade` rewrites
  resources in etcd to the new storage version; rolling the binaries
  back without rolling etcd back leaves the apiserver looking at
  newer-shape data than it can decode. See `docs/k8s-version-upgrade.md`
  once it lands.
- **Bulk CRD changes** — installing, upgrading, or removing CRDs is
  irreversible from etcd's point of view once the schema version
  changes. Resources written under an old schema may not decode
  under the new one and vice versa.
- **Restoring a custom admission policy** (PSA, ValidatingAdmission,
  webhook configs) that could mass-reject existing workloads on
  re-admission.
- **Disabling or enabling encryption-at-rest** — flipping the
  EncryptionConfiguration provider rewrites Secret/ConfigMap envelopes
  on the next write; reverting without a snapshot taken on the
  pre-flip side requires the prior key material. See
  [`docs/etcd-encryption.md`](etcd-encryption.md).

Use the `--label` flag to make the purpose obvious in your backup
directory:

```bash
scripts/backup-etcd.sh ~/backups --label pre-cni-swap
# -> ~/backups/etcd-snapshot-20260522T180000Z-pre-cni-swap.db

# Or via make:
make backup-etcd LABEL=pre-cni-swap
```

Labels are sanitized to `[A-Za-z0-9._-]` — anything outside that set
is replaced with `-`. After taking the snapshot, **off-host it** (copy
to a different machine or object storage) before performing the
operation, so a rollback that wipes the CP's working directory
doesn't also lose the backup.

## Backing up

```bash
# default outdir is ./backups/
make backup-etcd

# or, explicit outdir + via the script:
scripts/backup-etcd.sh ~/etcd-backups

# labeled snapshot for a high-risk operation (see "When to snapshot"):
make backup-etcd LABEL=pre-cni-swap
scripts/backup-etcd.sh ~/etcd-backups --label pre-cni-swap
```

What happens:

1. The script asks `kubectl` for the CP's InternalIP.
2. SSHes to the CP (optionally via `KVM_HOST` as a ProxyJump), finds
   the etcd container ID with `crictl ps --name etcd -q`, and runs
   `etcdctl snapshot save /tmp/snapshot.db` directly inside that
   container (no shell wrapper — the distroless etcd image has no
   `sh`).
3. Logs `etcdctl snapshot status` so you can see hash + revision +
   total keys.
4. `scp`s the file back to `$outdir/etcd-snapshot-<UTC-ts>.db`.
5. Removes the temp copy from the CP.

Verify a snapshot you took:

```bash
etcdctl --write-out=table snapshot status backups/etcd-snapshot-*.db
```

If `etcdctl` isn't installed locally, you can run it through the
same etcd image the CP uses:

```bash
podman run --rm -v "$PWD/backups:/b" registry.k8s.io/etcd:3.5.15-0 \
  etcdctl --write-out=table snapshot status /b/etcd-snapshot-XYZ.db
```

## Restoring

```bash
make restore-etcd SNAP=~/etcd-backups/etcd-snapshot-20260520T021500Z.db
# or:
scripts/restore-etcd.sh ~/etcd-backups/etcd-snapshot-20260520T021500Z.db
```

Restore is **destructive**. Only run it when the cluster is broken —
do not use it to roll back an unwanted `kubectl apply`. Use
`kubectl rollout undo` or git-revert + reapply for that.

The script:

1. Prompts (one Enter) to confirm.
2. SSHes to the CP and moves `/etc/kubernetes/manifests` aside, which
   causes kubelet to tear down the apiserver + etcd + scheduler +
   controller-manager static pods.
3. Stops kubelet.
4. Renames `/var/lib/etcd` to `/var/lib/etcd.before-restore.<ts>` so
   nothing is irreversibly destroyed.
5. Runs `etcdctl snapshot restore` into a fresh `/var/lib/etcd` via
   the same etcd image already pulled on the CP.
6. Restores the manifests directory and starts kubelet so the
   apiserver comes back up against the restored etcd.
7. Sleeps 30s and runs `kubectl get nodes` to confirm.

If the etcd image auto-detection picks the wrong tag (rare; only an
issue if multiple etcd versions have been pulled), the script logs
the image it chose. Override by editing the script's `ETCD_IMG=`
fallback, or run `etcdctl snapshot restore` by hand using the
podman invocation in the comment block at the top of
`scripts/restore-etcd.sh`.

After restore, expect:

- Workloads scheduled **between** the snapshot and the crash will not
  come back — they don't exist in etcd anymore.
- Workers stay joined (their kubelet certs are still valid; the node
  objects are restored from the snapshot).
- If etcd encryption is active, the apiserver must have the same
  `/etc/kubernetes/encryption-config.yaml` key it had when the
  snapshot was taken — otherwise it can't decrypt Secrets and
  ConfigMaps. See [`docs/etcd-encryption.md`](etcd-encryption.md).

## Full disaster recovery (lost the CP host)

If the CP VM (or the whole KVM host) is gone:

1. Rebuild the CP from a fresh image: `sudo make k8s`. This
   reinitializes etcd to an empty state, which is fine — we are
   about to overwrite it.
2. Copy the most recent snapshot to the orchestrator (or wherever
   you'll run the restore script from).
3. Copy the previous CP's `/etc/kubernetes/encryption-config.yaml`
   onto the new CP at the same path (only if encryption was
   enabled). This file is **not** in the snapshot.
4. Run `make restore-etcd SNAP=…`.
5. Wait for `kubectl get nodes` to show all nodes.
6. Workers will reconnect on their own if their kubelet certs are
   still valid AND the new CP has the same CA. If the CA was
   regenerated (because you didn't preserve `/etc/kubernetes/pki/`),
   the workers won't trust the new apiserver — easiest fix is to
   rebuild the workers from the image with `sudo make workers`,
   which generates fresh join tokens and rejoins them. The
   restored etcd will see them as new node objects (the old ones
   will be there too, in `NotReady`; clean up with
   `kubectl delete node <stale>`).

Workload data loss: anything created or modified between the
snapshot and the crash is gone. This is why cadence matters more
than the restore tooling itself.

## VM-level snapshots

The scripts in this repo cover etcd state only. For full VM
snapshots (kernel, container images, configs not stored in etcd),
use libvirt:

```bash
# On the KVM host:
sudo virsh -c qemu:///system snapshot-create-as \
  hummingbird-k8s pre-upgrade-$(date -u +%Y%m%dT%H%M%SZ) \
  --description 'before bootc upgrade' --atomic
sudo virsh -c qemu:///system snapshot-list hummingbird-k8s
sudo virsh -c qemu:///system snapshot-revert hummingbird-k8s <name>
```

This is useful before a `bootc upgrade` or any change that touches
the host OS. It is **not** a substitute for etcd snapshots — VM
snapshots are large, slow, and not portable across hosts. Treat
them as a parachute for an in-place upgrade, and treat etcd
snapshots as the actual backup.

## See also

- [`docs/etcd-encryption.md`](etcd-encryption.md) — what's encrypted
  and the key-handling caveats above.
- [`scripts/backup-etcd.sh`](../scripts/backup-etcd.sh)
- [`scripts/restore-etcd.sh`](../scripts/restore-etcd.sh)
