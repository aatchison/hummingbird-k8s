# K8s major-version upgrade strategy

How to move a Hummingbird cluster from one Kubernetes minor (e.g. `v1.31`) to
the next (e.g. `v1.32`). Patch-version bumps (`v1.31.4` → `v1.31.5`) happen
automatically at image-build time and need no operator action.

## Why this is its own doc

A `bootc switch` (or auto-update) swaps the OS image on a deployed VM, but it
does **not** change the cluster's Kubernetes API-server version in place:

- etcd state is preserved across the reboot, so the existing apiserver static
  pod manifest (`/etc/kubernetes/manifests/kube-apiserver.yaml`) stays bound to
  the K8s version that was baked into the previous image.
- The new image's `kubelet`/`kubeadm`/`kubectl` binaries are on disk, but the
  static-pod manifests written by the original `kubeadm init` still pin the
  old apiserver/controller-manager/scheduler image tags.

A true K8s minor-version upgrade therefore needs one of:

1. **Rebuild the VM from a new image** with `K8S_VERSION` bumped (clean-slate
   path — what we support today).
2. **In-cluster `kubeadm upgrade`** on the existing VM (in-place — documented
   here for awareness; not currently automated).

## Pre-flight checklist

Before any upgrade, regardless of path:

- **Snapshot etcd.** `make backup-etcd` writes a snapshot under
  `/var/lib/hummingbird/etcd-backups/` on the CP. See
  [`docs/backup-restore.md`](backup-restore.md) for the restore procedure.
- **Audit workload manifests.** Confirm every workload sets
  `imagePullPolicy` and resource `requests`/`limits` explicitly — defaults
  can shift across K8s minors and silently change scheduling behavior.
- **Check the Cilium compatibility matrix.** Hummingbird currently ships
  Cilium 1.16.x, which supports K8s 1.27–1.32. A `v1.31` → `v1.32` bump is
  safe; a `v1.32` → `v1.33` bump requires Cilium 1.17+ — bump Cilium first
  in a separate PR. See [`docs/cilium-migration.md`](cilium-migration.md).
- **Read the upstream changelog.** Removed/graduated APIs (`storage.k8s.io`,
  `flowcontrol.apiserver.k8s.io`, `policy/v1beta1`-style deprecations) bite
  hardest on minor bumps.

## Image-rebuild path (supported)

This is the path Hummingbird is built around: VMs are cattle, not pets.

1. Edit [`containers/k8s/Containerfile`](../containers/k8s/Containerfile):
   bump `ARG K8S_VERSION=v1.32` (or the target minor).
2. Mirror the same change in
   [`containers/k8s-worker/Containerfile`](../containers/k8s-worker/Containerfile)
   so workers and CP stay version-aligned.
3. Open a PR. CI (`pr-validate.yml`) builds the new images. Run the standard
   10-agent review.
4. Tag `k8s/vX.Y.0` (treat a K8s minor bump as a Hummingbird minor bump).
   Worker tag follows: `worker/vX.Y.0`.
5. Wait for the GHCR publish workflows to finish and for cosign to sign the
   images.
6. On the KVM host: destroy the CP VM and redeploy from the new image. The
   worker VMs reconcile automatically once they re-join via the join-token
   flow (see [`docs/worker-tokens.md`](worker-tokens.md)).

This skips the `kubeadm upgrade` dance entirely. Clean-slate VMs are
simpler than in-place upgrades for a homelab, and the etcd snapshot taken in
pre-flight is the safety net if a workload needs to be re-hydrated.

## In-place upgrade path (advanced; not currently automated)

Documented here so the choice is informed — these steps are operator-driven
and not wired into the Makefile or any script.

```bash
# On the CP:
sudo kubeadm upgrade plan
sudo kubeadm upgrade apply v1.32.X

# On each worker:
sudo kubeadm upgrade node
sudo systemctl restart kubelet
```

Caveats:

- The apiserver static pod may fail to start mid-upgrade if etcd's storage
  format changes across the bump. The pre-flight snapshot is the recovery
  path.
- Cilium must be on a version that supports both the old and new K8s minor
  during the rolling upgrade. If the major bump crosses Cilium's support
  window, upgrade Cilium first as a separate step.
- The kubelet/kubeadm RPMs on the VM are pinned to the image's
  `K8S_VERSION` repo channel — an in-place upgrade past that channel
  requires reconfiguring the `kubernetes.repo` baseurl on the live VM.

For the homelab use case, the rebuild path is strongly preferred.

## Skipping versions

- Kubernetes officially supports skipping at most one minor at a time
  (`v1.31` → `v1.32` is fine; `v1.31` → `v1.33` is not). Multi-minor jumps
  require intermediate hops.
- Hummingbird pinning: `K8S_VERSION=v1.31` resolves to the latest `1.31.x`
  patch at build time, so patch bumps roll forward automatically on every
  image rebuild.

## Verifying the upgrade

After the new CP VM is up (rebuild path) or after `kubeadm upgrade` completes
(in-place path):

- `kubectl version` — server reports the new minor.
- `kubectl get nodes -o wide` — `KUBELET` column shows the new version on
  every node.
- `scripts/verify-app-deploy.sh` — end-to-end smoke test of a PSA-restricted
  nginx deploy + pod-to-pod networking still passes (see
  [`docs/app-deploy-verify.md`](app-deploy-verify.md)).
- `scripts/verify-hardening.sh` — PSA, apiserver audit policy, and kubelet
  `--protect-kernel-defaults` flags are still applied (see
  [`docs/security-hardening.md`](security-hardening.md)).

## Cross-links

- [`docs/backup-restore.md`](backup-restore.md) — pre-upgrade etcd snapshot.
- [`docs/cilium-migration.md`](cilium-migration.md) — Cilium compatibility
  caveat across K8s minors.
- [`docs/integration-tests.md`](integration-tests.md) — the bootc-upgrade
  workflow that exercises the rebuild path.
