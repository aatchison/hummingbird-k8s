# Cilium migration (flannel → Cilium)

This document describes why `hummingbird-k8s` swapped its default CNI
from flannel to Cilium, what the change actually does at the file
level, and the operational caveats that come with it.

## Why Cilium

Flannel is a routing/encapsulation-only CNI: it gets pods talking but
it does **not** enforce `NetworkPolicy`. Applying a
`policy/v1.NetworkPolicy` against a flannel-only cluster silently
succeeds and silently does nothing — there is no datapath to drop
traffic. For a security-hardened single-node lab image that already
ships PodSecurity Admission, audit logging, and etcd encryption at
rest, "your network policies are decorative" is a real gap (issue #6).

Cilium replaces flannel as the CNI and brings:

- **NetworkPolicy enforcement** — the standard
  `networking.k8s.io/v1.NetworkPolicy` resource is honored by the
  Cilium agent's eBPF datapath. A deny-all default policy actually
  drops pod-to-pod traffic.
- **eBPF datapath** — packet processing runs as eBPF programs
  attached to the network stack rather than as iptables chains. On a
  modern kernel (Hummingbird base ships one) this is both faster and
  observable via `cilium monitor` / Hubble.
- **CiliumNetworkPolicy** — a richer policy CRD with L7 awareness
  (HTTP method/path, DNS, Kafka, …) for cases where the upstream
  resource isn't expressive enough. Not used by this image by
  default; available the moment Cilium is installed.

## What changed

This rolls in #6 (CNI swap) and #50 (audit log mount scope) together
because both edit the same kubeadm `extraVolumes` region of
`containers/k8s/k8s-init.sh` (referred to as `k8s-init.sh` below).

### `containers/k8s/k8s-init.sh`

- The flannel install step:
  ```bash
  kubectl apply -f https://raw.githubusercontent.com/flannel-io/flannel/.../kube-flannel.yml
  ```
  is replaced with a `cilium install` invocation that calls the
  cilium-cli binary baked into the image:
  ```bash
  KUBECONFIG=/etc/kubernetes/admin.conf cilium install \
    --version 1.16.5 \
    --set kubeProxyReplacement=true \
    --set k8sServiceHost=auto \
    --set k8sServicePort=6443 \
    --set hubble.enabled=true \
    --set hubble.relay.enabled=true \
    --set hubble.ui.enabled=false \
    --wait \
    --wait-duration 5m
  ```
- The apiserver audit log path moves from `/var/log/k8s-audit.log` to
  `/var/log/kubernetes/k8s-audit.log` (#50).
- The corresponding `audit-log` extraVolume's `hostPath` and
  `mountPath` move from `/var/log` to `/var/log/kubernetes`. The
  apiserver static pod can no longer scribble anywhere under
  `/var/log/` on the host — only into its own kubernetes subdir.
  `pathType: DirectoryOrCreate` is retained so the directory is
  auto-created on first boot.

### `containers/k8s/Containerfile`

- The cilium-cli binary is pre-baked into the image at `/usr/bin/cilium`
  via a `curl + tar` extraction from the upstream GitHub release tarball.
  The version is controlled by the `CILIUM_CLI_VERSION` build-arg
  (default `v0.16.16`). Baking the binary in keeps `k8s-init.sh`'s
  first-boot path offline-friendly: it does not need to fetch a CLI
  tarball before installing the CNI.

### `containers/shared/kubernetes/admission-control-config.yaml`

- The `kube-flannel` namespace exemption (it carried the flannel
  daemonset, which runs with elevated privileges incompatible with
  `restricted` PodSecurity) is removed. Cilium's components run in
  `kube-system`, which is already exempt — no new exemption needed.

## Install method

The CNI is installed via the `cilium-cli` binary, which is baked
into the image at build time and invoked from `k8s-init.sh` on first
boot.

How it's installed:

- The `cilium-cli` single static binary is downloaded during
  `containers/k8s/Containerfile` build (`curl` + `tar -xzf`) and placed at
  `/usr/bin/cilium`. The version is pinned via the
  `CILIUM_CLI_VERSION` build-arg (default `v0.16.16`).
- On first boot `k8s-init.sh` calls `cilium install` with
  `--version 1.16.5`, `--set kubeProxyReplacement=true` (#94),
  `--set k8sServiceHost=auto --set k8sServicePort=6443` (required
  companions to kpr=true), `--set hubble.enabled=true --set
  hubble.relay.enabled=true --set hubble.ui.enabled=false` (#78), and
  `--wait --wait-duration 5m`. The `--version` flag pins the Cilium
  agent/operator image version (independent of the CLI version).
- The Cilium CLI internally renders the upstream Helm chart with the
  supplied `--set key=value` values and applies the resulting
  manifests to the cluster. Anything the chart exposes is reachable
  via more `--set` flags — no Helm binary needed in the image.
- Further Cilium features (transparent encryption, BPF host routing
  toggles, BGP, …) can be enabled by adding more `--set` flags to the
  `cilium install` invocation. No new packages required.

Why this approach (instead of `kubectl apply -f <quick-install.yaml>`):
upstream Cilium retired the `install/kubernetes/quick-install.yaml`
manifest from the `cilium/cilium` repo and now expects installs to go
through Helm or `cilium-cli`. The previous URL
(`raw.githubusercontent.com/cilium/cilium/main/install/kubernetes/quick-install.yaml`)
returns HTTP 404. `cilium-cli` is the single-binary path that doesn't
require a Helm install in the image.

## What changes from flannel

- **No `kube-flannel` namespace.** The CNI daemonset now lives in
  `kube-system` as the `cilium` and `cilium-operator` workloads.
  Any external tooling that grepped for `kube-flannel` will need to
  point at `kube-system` (label selector `k8s-app=cilium` for the
  agent).
- **kube-proxy is gone.** Cilium runs with
  `--set kubeProxyReplacement=true` (#94) and the kubeadm
  `addon/kube-proxy` phase is skipped via
  `kubeadm init --skip-phases=addon/kube-proxy`. All L4 service
  routing (ClusterIP, NodePort, ExternalIP, LoadBalancer) runs in
  Cilium's eBPF datapath. There is no `kube-proxy` DaemonSet in
  `kube-system` and no `iptables` / `ipvs` rules managed by
  kube-proxy on the host.
- **Hubble (flow visibility) is on.** The Cilium agent has Hubble
  enabled and the `hubble-relay` Deployment is installed so
  `cilium hubble port-forward` + `hubble observe` work without any
  extra install step (#78). The Hubble UI is intentionally **off** —
  port-forwarding the relay from the CLI is the lean path; baking
  the UI in would add pods (`hubble-ui`, `hubble-ui-backend`) and a
  Service that aren't needed for a single-node lab image. Flip
  `--set hubble.ui.enabled=true` in `k8s-init.sh` to enable it.
- **Transparent encryption still off by default.** WireGuard / IPsec
  are not enabled. Both can be turned on by adding the corresponding
  `--set` flags to the `cilium install` invocation in `k8s-init.sh`
  in a follow-up PR.
- **NetworkPolicy is now real.** A deny-all policy in any namespace
  will actually drop traffic. Existing workloads that assumed an
  open network (which is most lab workloads) should be unaffected
  unless they relied on cross-namespace traffic that happened to
  ignore an existing-but-non-enforced policy.

## Operational caveats

### cilium-cli flags this image passes

`cilium install` is invoked with these explicit overrides:

- **kube-proxy replacement is on** (`--set kubeProxyReplacement=true`,
  #94). Cilium handles all L4 service routing in eBPF.
- **k8sServiceHost=auto, k8sServicePort=6443**. Required companions
  to `kubeProxyReplacement=true`: with no kube-proxy in the cluster
  there is no ClusterIP for `kubernetes.default.svc`, so the Cilium
  agent needs to know where the apiserver lives. `auto` resolves to
  the kubeadm-published control-plane endpoint.
- **Hubble + relay on, UI off** (`--set hubble.enabled=true`,
  `--set hubble.relay.enabled=true`, `--set hubble.ui.enabled=false`,
  #78). See "Hubble flow visibility" below for the access path.
- Transparent encryption (WireGuard/IPsec) is **not** enabled.
- BPF host routing is on by default in current Cilium versions, but
  is **not** explicitly configured here.

If you need any of the remaining-off features, add the appropriate
`--set` flag(s) to the `cilium install` call in `k8s-init.sh`.

### kubeProxyReplacement (closes #94)

Setting `kubeProxyReplacement=true` tells the Cilium agent to take
over all of kube-proxy's responsibilities (ClusterIP, NodePort,
ExternalIP, LoadBalancer, host-port mapping, session affinity) using
eBPF programs attached to the socket and tc/XDP hooks. Two things
have to line up for this to work:

1. **kubeadm must not install the kube-proxy addon.**
   `k8s-init.sh` passes `--skip-phases=addon/kube-proxy` to
   `kubeadm init`, so the kube-proxy DaemonSet is never created.
   This is a CLI flag rather than a `ClusterConfiguration` field in
   `kubeadm.k8s.io/v1beta4` — there is no YAML equivalent.
2. **Cilium must know the apiserver address.** Without kube-proxy
   there is no ClusterIP for `kubernetes.default.svc` that Cilium
   itself could route to. `k8sServiceHost=auto` lets cilium-cli
   resolve the kubeadm-published control-plane endpoint;
   `k8sServicePort=6443` matches kubeadm's default secure port. If
   you override `controlPlaneEndpoint` to a non-6443 port, override
   `k8sServicePort` to match.

Verify kube-proxy is gone and Cilium owns the service plane:

```bash
# No kube-proxy DaemonSet.
KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n kube-system \
  get ds kube-proxy 2>&1 | grep -q NotFound && echo "kube-proxy: absent (as expected)"

# Cilium reports kube-proxy-replacement=Strict.
KUBECONFIG=/etc/kubernetes/admin.conf cilium status --wait | grep -i 'KubeProxyReplacement'
```

### Hubble flow visibility (closes #78)

Hubble is Cilium's observability layer — it taps the agent's eBPF
datapath and exposes per-flow records (5-tuple, verdict, L7 metadata
where Cilium can parse it) via a gRPC API. With
`--set hubble.enabled=true --set hubble.relay.enabled=true`:

- The `cilium` agent emits flow records into its in-memory ring
  buffer.
- The `hubble-relay` Deployment in `kube-system` exposes the
  aggregated flow stream over gRPC.

Operators reach Hubble from the cilium-cli baked into the image:

```bash
# Open a local tunnel to the relay (foreground).
KUBECONFIG=/etc/kubernetes/admin.conf cilium hubble port-forward &

# Stream flows.
KUBECONFIG=/etc/kubernetes/admin.conf hubble observe --follow

# Drops only (e.g. to confirm a NetworkPolicy is firing).
KUBECONFIG=/etc/kubernetes/admin.conf hubble observe --verdict DROPPED --follow
```

The Hubble UI is intentionally **not** installed. The relay +
`hubble observe` CLI cover the lab use case (debugging a policy,
watching what a pod sends) without the extra Deployments/Service
that the UI requires. To enable the UI, flip
`--set hubble.ui.enabled=true` in `k8s-init.sh` and rebuild.

### Existing single-node clusters must rebuild

`k8s-init.service` is one-shot and gated by `/var/lib/k8s-init.done`.
A `bootc upgrade` to an image that contains this change does **not**
re-run `kubeadm init` and does **not** re-apply the CNI. Existing
clusters that were initialized against flannel will keep their
flannel install across a bootc upgrade.

To migrate an existing VM: rebuild from the new image. There is no
in-place flannel→Cilium swap in this round (Cilium does ship a
`cilium-cli upgrade` story, but cross-CNI migration is brittle and
out of scope for a lab image).

### Kernel feature requirements

Cilium needs a BPF filesystem mount and a kernel built with the
relevant BPF features (CONFIG_BPF, CONFIG_BPF_SYSCALL, CONFIG_NET_CLS_BPF,
CONFIG_NET_SCH_INGRESS, and friends). The Hummingbird base image
currently ships these. If a future minimal base drops them, Cilium
won't start — the `cilium` daemonset will go CrashLoopBackOff with
"BPF filesystem not mounted" or similar in the logs.

### Rollback limitations

> **Warning — a CNI swap is one-way without a pre-swap etcd snapshot.**
> `bootc rollback` restores the previous filesystem deployment (the
> prior image's `/usr`, `/etc` baseline, baked-in binaries, etc.) and
> reboots into it. It does **not** roll back cluster state stored in
> etcd. The CRDs, Deployments, IP allocations, and policy objects that
> Cilium writes to etcd survive a `bootc rollback` to a flannel-era
> image. Without an etcd snapshot taken **before** the swap, there is
> no clean path back to a flannel-only cluster — see issue #95.

**Before swapping the CNI on a live cluster:**

1. Snapshot etcd:
   `make backup-etcd LABEL=pre-cni-swap`
   (or `scripts/backup-etcd.sh ~/backups --label pre-cni-swap`).
   The label makes the snapshot's purpose obvious on disk:
   `~/backups/etcd-snapshot-20260522T180000Z-pre-cni-swap.db`.
2. Verify the snapshot integrity:
   `etcdctl --write-out=table snapshot status <snapshot.db>`
   (or via the podman-in-place invocation in
   [`docs/backup-restore.md`](backup-restore.md) if `etcdctl` isn't
   installed locally).
3. Off-host the snapshot — copy it to a different host before
   proceeding, so a `bootc rollback` that wipes the CP's working
   directory doesn't also lose the backup.
4. Apply the CNI swap (rebuild the image with the Cilium change,
   redeploy the CP).
5. If the swap breaks workloads: `bootc rollback` + reboot the CP
   into the pre-swap image, then restore via
   `scripts/restore-etcd.sh <snapshot.db>` to return the cluster to
   its pre-swap state.

See [`docs/backup-restore.md#when-to-snapshot`](backup-restore.md#when-to-snapshot)
for the broader list of high-risk operations (CNI swaps, k8s upgrades,
bulk CRD changes, etc.) that warrant a labeled snapshot first.

See `docs/rollback.md` for the rollback path itself (manual + auto).
After a rollback from a Cilium-enabled image to a flannel-era image
**without** a pre-swap snapshot:

- Cilium CRDs installed at first boot (e.g. `CiliumNetworkPolicy`,
  `CiliumEndpoint`, `CiliumIdentity`, …) remain registered with the
  apiserver.
- Cilium IP allocations and endpoint records persist in etcd.
- The Cilium agent/operator workloads will be gone from the image, but
  their Deployment / DaemonSet specs in etcd will remain until
  manually deleted.

The result is a cluster with a flannel-era binary surface but Cilium
detritus in etcd — both CNIs nominally trying to manage the same
podCIDR. This is not a clean state.

Recommended approach if a rollback across the CNI swap is needed
(the canonical path; the numbered procedure above is the same):

1. Before upgrading, take a **labeled** etcd snapshot of the
   pre-Cilium cluster
   (`make backup-etcd LABEL=pre-cni-swap` or
   `scripts/backup-etcd.sh ~/backups --label pre-cni-swap`)
   and stash it off the VM.
2. If a rollback becomes necessary, `bootc rollback` + reboot, then
   restore the snapshot into the rolled-back image's etcd via
   `scripts/restore-etcd.sh <snapshot.db>` before any workloads come
   up.

For a lab cluster the simpler path is often to rebuild from the old
image rather than attempt a CNI-crossing rollback — but that still
loses any workload state created after the swap. The labeled
snapshot above is the only way to preserve that state across the
rollback.

### Version pinning is explicit

Two versions are pinned independently:

- `CILIUM_CLI_VERSION` (build-arg, default `v0.16.16`) — the version
  of the cilium-cli binary baked into the image.
- `--version 1.16.5` in the `cilium install` call — the version of
  the Cilium agent/operator images that get deployed into the
  cluster. The CLI version and the Cilium version do not have to
  move in lockstep.

Bumping either version is a deliberate edit to `containers/k8s/Containerfile` or
`k8s-init.sh`. There is no `latest` / `main` fetch at first boot, so
two VMs built from the same image are reproducible regardless of when
they are provisioned.

## Verifying the install

After a VM boots fresh from an image with this change:

```bash
# Cilium agent pods on each node should be Ready.
KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n kube-system \
  get pods -l k8s-app=cilium

# Cilium operator should be Running.
KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n kube-system \
  get pods -l io.cilium/app=operator

# Nodes should report Ready (Cilium publishes the node-network-ready
# condition once its agent has set up the datapath on that node).
KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes
```

To verify policy enforcement actually works:

```bash
# Run two pods in the same namespace.
kubectl run a --image=quay.io/curl/curl --command -- sleep 3600
kubectl run b --image=quay.io/curl/curl --command -- sleep 3600

# Confirm a -> b reachable on pod IP before any policy.
kubectl exec a -- curl -m 2 -sS http://$(kubectl get pod b -o jsonpath='{.status.podIP}')

# Apply a deny-all NetworkPolicy.
cat <<EOF | kubectl apply -f -
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: deny-all
spec:
  podSelector: {}
  policyTypes: [Ingress, Egress]
EOF

# a -> b should now fail (timeout). On flannel it would have
# silently still worked.
kubectl exec a -- curl -m 2 -sS http://$(kubectl get pod b -o jsonpath='{.status.podIP}') || \
  echo "EXPECTED: traffic blocked by deny-all policy"
```

If the second `curl` succeeds, NetworkPolicy is not being enforced
and the swap has not delivered its primary value.
