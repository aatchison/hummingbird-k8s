# NFS storage: dynamic PersistentVolumeClaims via csi-driver-nfs

Hummingbird clusters ship [csi-driver-nfs](https://github.com/kubernetes-csi/csi-driver-nfs)
(upstream **v4.13.4**, vendored verbatim at
`containers/shared/kubernetes/csi-driver-nfs.yaml`) so a PVC can be satisfied
from an NFS export with no manual PV plumbing:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: my-data
spec:
  accessModes: [ReadWriteMany]
  storageClassName: nfs-csi
  resources:
    requests:
      storage: 5Gi
```

`kubectl apply` that and it binds — the driver creates a subdirectory on the
export and provisions a PV pointing at it.

## Why a CSI driver and not an NFS client on the node image

The node plugin image (`registry.k8s.io/sig-storage/nfsplugin`) is Debian-based
and **carries its own mount helper** — `nfs-common`, a setuid `/sbin/mount.nfs`,
`mount.nfs4`, and `rpc.statd`. It calls `mount(8)` inside its own mount
namespace, and the result becomes visible to kubelet through
`mountPropagation: Bidirectional` on `/var/lib/kubelet/pods`.

So the hummingbird node images deliberately install **no `nfs-utils`, no
`rpcbind`, and no new setuid binary**. That matters on these nodes because:

- `/usr` is read-only and the images are rebuilt from a pinned base — every host
  package is a permanent addition to every node.
- `nfs-utils` pulls ~16 packages (rpcbind, gssproxy, quota, a compat libxml2)
  and *upgrades* two curated `hum1` base RPMs, i.e. an NFS change floating
  packages inside the FIPS-pinned set.
- `nfs-utils` also enables `nfs-client.target` via the base's own
  `90-default.preset`, which outranks `99-default-disable`'s `disable *` — a
  unit you did not ask for.
- Its `/var/lib/nfs/statd` state tree would never reach **already-deployed**
  nodes at all, because bootc unpacks image `/var` only at initial install.

The in-tree `nfs:` volume type and `nfs-subdir-external-provisioner` *would*
need a host helper, since they mount in the host mount namespace. This driver is
chosen partly to avoid that.

## Setup

The driver is applied automatically by `k8s-init.sh` on **new** clusters. The
StorageClass is not — `server`/`share` are site-specific, and a StorageClass
pointing at the wrong export binds PVCs to volumes that silently fail to mount.

```bash
# Fill in your NAS and apply:
sed -e 's/NFS_SERVER_HERE/10.0.0.20/' -e 's#NFS_SHARE_HERE#/export/k8s#' \
  /etc/kubernetes/nfs-storageclass.example.yaml | kubectl apply -f -
```

Add the `storageclass.kubernetes.io/is-default-class: "true"` annotation (
commented in the example) if PVCs without an explicit `storageClassName` should
land here. Only one default per cluster.

### Existing clusters

`k8s-init.service` is once-only (guarded by `/var/lib/k8s-init.done`), so a
cluster that was already initialized will **not** pick the driver up from an
image update. Apply it by hand once, from the CP:

```bash
ssh root@<cp-ip> kubectl --kubeconfig=/etc/kubernetes/admin.conf \
  apply -f /etc/kubernetes/csi-driver-nfs.yaml
```

The file is present on any node whose image includes this change, so the
manifest and the running driver stay version-matched to the image.

## Verifying

```bash
# 1. Driver is up: 1 controller + 1 node pod per node.
kubectl -n kube-system get pods -l app=csi-nfs-controller
kubectl -n kube-system get pods -l app=csi-nfs-node -o wide
kubectl get csidrivers nfs.csi.k8s.io

# 2. End-to-end: a PVC binds and a pod writes to it.
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: nfs-smoke}
spec:
  accessModes: [ReadWriteMany]
  storageClassName: nfs-csi
  resources: {requests: {storage: 1Gi}}
---
apiVersion: v1
kind: Pod
metadata: {name: nfs-smoke}
spec:
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 1000
    seccompProfile: {type: RuntimeDefault}
  containers:
    - name: w
      image: registry.access.redhat.com/ubi9/ubi-minimal
      command: ["sh", "-c", "echo hello > /data/smoke && cat /data/smoke"]
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: {drop: ["ALL"]}
      volumeMounts: [{name: d, mountPath: /data}]
  volumes:
    - name: d
      persistentVolumeClaim: {claimName: nfs-smoke}
EOF

kubectl wait --for=condition=Ready pod/nfs-smoke --timeout=120s
kubectl logs nfs-smoke        # expect: hello
kubectl delete pod/nfs-smoke pvc/nfs-smoke
```

The pod above is deliberately `runAsNonRoot` + `drop: ALL` — it must pass the
cluster-wide `enforce: restricted` PSA policy
(`containers/shared/kubernetes/admission-control-config.yaml`). Only the driver
itself lives in the PSA-exempt `kube-system` namespace.

## Things that will bite you

- **Export permissions, not mount failures, are the usual first problem.**
  Restricted PSA forces non-root pods, and `fsGroup` does not reapply ownership
  on NFS the way it does on block volumes. If the pod mounts but gets `EACCES`
  on write, fix it on the server: export with the right owner, or `all_squash`
  + `anonuid`/`anongid` matching the pod's `runAsUser`/`fsGroup`.
- **NFSv4 id mapping.** These nodes have no regular users and no configured
  idmap `Domain`. A server that returns owners as `name@domain` makes every file
  read as `nobody:nogroup` (65534). Prefer `sec=sys` with numeric IDs.
- **`sec=sys` is trust-by-assertion.** The server believes the UID the node
  claims, cluster-scoped PVs can be claimed from any namespace, and NFS traffic
  is cleartext on the wire. Treat an NFS export as a shared, unauthenticated
  filesystem unless you deploy Kerberos.
- **Hung mounts vs the nightly reboot roll.** These nodes are rebooted
  unattended by the rolling-apply timer (see [auto-reboot.md](auto-reboot.md)).
  `hard` mounts — the correct default for data integrity — block I/O while the
  server is unreachable, which can stall a pod's termination and therefore a
  drain. If your NFS server is less available than your cluster, keep
  `timeo`/`retrans` modest (as in the example) so a drain fails fast and loudly
  rather than hanging, and expect the roll to abort on that node rather than
  proceed — which is the intended safe behaviour.
- **`ReadWriteMany` is real here**, unlike most block storage — that is usually
  the reason to pick NFS in the first place.

## Re-vendoring the driver

```bash
V=v4.13.5   # new upstream tag
for f in rbac-csi-nfs csi-nfs-driverinfo csi-nfs-controller csi-nfs-node; do
  curl -fsSL "https://raw.githubusercontent.com/kubernetes-csi/csi-driver-nfs/$V/deploy/$f.yaml"
done
```

Reassemble in that order under the existing header comment, keeping every
internal `---` separator intact, and re-validate:

```bash
docker run --rm -v "$PWD:/repo" -w /repo ghcr.io/yannh/kubeconform:latest \
  -summary -strict -kubernetes-version 1.31.0 \
  containers/shared/kubernetes/csi-driver-nfs.yaml
```

Do not hand-edit the vendored bodies — keeping them verbatim is what makes the
diff against upstream reviewable.
