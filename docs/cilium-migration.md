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
`k8s-init.sh`.

### `k8s-init.sh`

- The flannel install step:
  ```bash
  kubectl apply -f https://raw.githubusercontent.com/flannel-io/flannel/.../kube-flannel.yml
  ```
  is replaced with the Cilium quick-install:
  ```bash
  kubectl apply -f https://raw.githubusercontent.com/cilium/cilium/main/install/kubernetes/quick-install.yaml
  ```
- The apiserver audit log path moves from `/var/log/k8s-audit.log` to
  `/var/log/kubernetes/k8s-audit.log` (#50).
- The corresponding `audit-log` extraVolume's `hostPath` and
  `mountPath` move from `/var/log` to `/var/log/kubernetes`. The
  apiserver static pod can no longer scribble anywhere under
  `/var/log/` on the host — only into its own kubernetes subdir.
  `pathType: DirectoryOrCreate` is retained so the directory is
  auto-created on first boot.

### `etc/kubernetes/admission-control-config.yaml`

- The `kube-flannel` namespace exemption (it carried the flannel
  daemonset, which runs with elevated privileges incompatible with
  `restricted` PodSecurity) is removed. Cilium's components run in
  `kube-system`, which is already exempt — no new exemption needed.

## Install method

The change keeps the same install pattern as before: a single
`kubectl apply -f <upstream URL>` from `k8s-init.sh` during first
boot. No new packages are added to the image (no `helm`, no
`cilium-cli`), so `Containerfile.k8s` is untouched.

Trade-off: the upstream URL points at `main`, not a pinned release
tag. The flannel install had the same property (`master` branch URL),
so this is a like-for-like behavior. See "Pin to a release tag?"
below.

## What changes from flannel

- **No `kube-flannel` namespace.** The CNI daemonset now lives in
  `kube-system` as the `cilium` and `cilium-operator` workloads.
  Any external tooling that grepped for `kube-flannel` will need to
  point at `kube-system` (label selector `k8s-app=cilium` for the
  agent).
- **kube-proxy still present.** The quick-install variant keeps
  kube-proxy running. Cilium can replace kube-proxy entirely
  (`kubeProxyReplacement=true`), but that's a Helm-values change and
  the quick-install manifest doesn't toggle it. Operators who want
  kube-proxy-free should switch to a Helm-based install.
- **No Hubble by default.** The quick-install variant does not enable
  Hubble (observability) or transparent encryption (WireGuard /
  IPsec). Both require Helm.
- **NetworkPolicy is now real.** A deny-all policy in any namespace
  will actually drop traffic. Existing workloads that assumed an
  open network (which is most lab workloads) should be unaffected
  unless they relied on cross-namespace traffic that happened to
  ignore an existing-but-non-enforced policy.

## Operational caveats

### Quick-install defaults only

The upstream `quick-install.yaml` is opinionated and ships with
**defaults only**:

- kube-proxy is **not** replaced.
- Hubble is **not** enabled.
- Transparent encryption (WireGuard/IPsec) is **not** enabled.
- BPF host routing is on by default in current Cilium versions, but
  is **not** explicitly configured here.

If you need any of those, switch to a Helm install later. That is a
follow-up issue and out of scope for #6.

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

### Quick-install URL is upstream-controlled

The manifest is fetched from `cilium/cilium`'s `main` branch at first
boot. Upstream is free to break it. This matches the previous
flannel behavior (also fetched from `master`). Pinning is a separate
issue — see below.

## Pin to a release tag?

The current behavior matches what was there before flannel: fetch
from `main`. That has two known weaknesses:

1. Reproducibility — two VMs built from the same image but
   provisioned days apart can pick up different Cilium versions if
   upstream pushes during that window.
2. Availability — if upstream restructures the repo (this has
   happened with Cilium specifically; the `quick-install.yaml` path
   has moved before), first-boot init breaks.

A follow-up issue should pin to a specific tag, e.g.
`https://raw.githubusercontent.com/cilium/cilium/v1.19.4/install/kubernetes/quick-install.yaml`,
and bump it deliberately. That bump cadence is outside the scope of
the initial swap.

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
