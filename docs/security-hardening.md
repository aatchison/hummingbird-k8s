# Security hardening: PodSecurity, audit logging, kubelet defaults

This document describes three control-plane hardening controls that
`hummingbird-k8s` enables at first-boot, what they cost operationally,
and how to verify each one.

## Controls

### 1. PodSecurity Admission — cluster-wide `restricted` (closes #7)

The apiserver is started with
`--admission-control-config-file=/etc/kubernetes/admission-control-config.yaml`,
pointing at an `AdmissionConfiguration` document whose `PodSecurity`
plugin is configured with:

```yaml
defaults:
  enforce: restricted
  enforce-version: latest
  audit: restricted
  warn: restricted
```

This means every namespace in the cluster defaults to the
[`restricted` Pod Security Standard][pss] — no privileged containers,
no host namespaces, drops `ALL` Linux capabilities, runs as non-root,
uses a seccomp profile, etc.

System namespaces that must run privileged daemons (kube-proxy, CNI,
etc.) are exempted explicitly in `admission-control-config.yaml`:

- `kube-system`
- `kube-public`
- `kube-node-lease`

The CNI (Cilium) runs in `kube-system`, which is already exempt — see
[`docs/cilium-migration.md`](cilium-migration.md).

[pss]: https://kubernetes.io/docs/concepts/security/pod-security-standards/

### 2. Apiserver audit logging (closes #13)

The apiserver writes JSON audit records to
`/var/log/kubernetes/k8s-audit.log` on the host, via these flags:

| Flag | Value |
|------|-------|
| `--audit-policy-file` | `/etc/kubernetes/audit-policy.yaml` |
| `--audit-log-path` | `/var/log/kubernetes/k8s-audit.log` |
| `--audit-log-maxsize` | `100` (MB per file) |
| `--audit-log-maxbackup` | `5` (rotated files retained) |

The audit policy (`etc/kubernetes/audit-policy.yaml`) is tuned to keep
the log signal-to-noise ratio reasonable on a single-node lab cluster:

- Read traffic (`get` / `list` / `watch`) on built-in resources is
  dropped entirely.
- `events` are dropped entirely (they are already a queryable resource).
- Secret / ConfigMap and `authentication.k8s.io` / `authorization.k8s.io`
  accesses are recorded at `Metadata` level — enough to see who touched
  a Secret without writing the Secret's contents into the audit log.
- Everything else lands at `Request` level (no response bodies).

`/var/log/kubernetes` is mounted into the apiserver static pod as a
writable HostPath so the apiserver process inside the pod can append
to the log on the host filesystem.

### 3. Kubelet hardening: `protect-kernel-defaults` + `rotate-certificates`

The kubelet is started with two hardening flags via
`InitConfiguration.nodeRegistration.kubeletExtraArgs`:

| Flag | Value | Rationale |
|------|-------|-----------|
| `protect-kernel-defaults` | `true` | kube-bench CIS 4.2.6 — kubelet refuses to *modify* kernel parameters at runtime; it will only start if the host already has the values it expects. |
| `rotate-certificates` | `true` | The kubelet auto-renews its client certificate (used to authenticate to the apiserver as `system:node:<hostname>`) before expiry by issuing a new CSR against the apiserver and consuming the signed cert (#121). |

With `protect-kernel-defaults=true` set,
`containers/k8s/Containerfile` pre-sets the relevant sysctls in
`/etc/sysctl.d/k8s.conf`:

```
vm.overcommit_memory      = 1
vm.panic_on_oom           = 0
kernel.panic              = 10
kernel.panic_on_oops      = 1
kernel.keys.root_maxkeys  = 1000000
kernel.keys.root_maxbytes = 25000000
```

If kubelet refuses to start with a message like
`kernel parameter "vm.overcommit_memory" doesn't match`, the host
sysctl values have drifted from these — `sysctl --system` to reapply.

With `rotate-certificates=true`, the kubelet watches its own
`/var/lib/kubelet/pki/kubelet-client-current.pem`; ~70% through its
validity window it submits a CSR via the
`certificates.k8s.io/v1.CertificateSigningRequest` API and the
controller-manager auto-approves it (the `kubelet-client-auto-approve`
ClusterRoleBinding shipped by kubeadm covers this). Without this flag
the kubelet's client cert silently expires after ~1y on a single-node
lab cluster and the node goes `NotReady` until manually re-bootstrapped.

### 4. Apiserver request timeout (closes #121)

The apiserver `ClusterConfiguration` sets two timeout knobs:

| Knob | Value | Purpose |
|------|-------|---------|
| `apiServer.timeoutForControlPlane` | `5m0s` | how long `kubeadm init` waits for the static-pod control-plane to become healthy before declaring init failed |
| `apiServer.extraArgs[request-timeout]` | `5m` | apiserver flag — upper bound on the duration of any single HTTP request handler |

The default `request-timeout` (1m for most endpoints, longer for
watch streams) lets a wedged storage path tie up a handler goroutine
indefinitely. `5m` is a deliberate, finite ceiling: long enough that
healthy long-poll watches reset cleanly, short enough that a stuck
handler bounces and surfaces as a 5xx instead of silently consuming
goroutines forever.

## Where the configs live

| Path inside image | Source in repo |
|---|---|
| `/etc/kubernetes/admission-control-config.yaml` | `containers/shared/kubernetes/admission-control-config.yaml` |
| `/etc/kubernetes/audit-policy.yaml` | `containers/shared/kubernetes/audit-policy.yaml` |
| `/etc/sysctl.d/k8s.conf` | inline in `containers/k8s/Containerfile` |
| `/etc/kubernetes/kubeadm-init.yaml` | generated at first boot by `containers/k8s/k8s-init.sh` |

All three files baked into the image are read-only references; the
apiserver static pod mounts them via `extraVolumes` in
`ClusterConfiguration.apiServer`.

## Verifying each control

All three controls can be checked in one shot with
[`scripts/verify-hardening.sh`](../scripts/verify-hardening.sh) — the
recommended invocation is through `make verify-hardening`, which wires
`KUBECTL=` through to the SSH-tunnel wrapper for free:

```bash
KVM_HOST=thegeary make verify-hardening CONFIG=cluster.local.conf
```

If `$KVM_HOST` is set, the script tunnels SSH through that host two
ways: (1) `ssh -o ProxyJump=$KVM_HOST root@$CP_IP` for the on-CP audit
log + kubelet checks, and (2) `KUBECTL=scripts/kubectl-k8s.sh` for the
PodSecurity apply + node lookup (`kubectl-k8s.sh` opens a local-port
tunnel through `$KVM_HOST` to the apiserver). This lets you run the
verifier from your dev machine without setting up a route to the
libvirt NAT subnet. (#271 F4)

CP_IP resolution order (when CP_IP is not set in the environment):

1. `resolve_cp_ip "$CP_NAME"` — uses `ssh $KVM_HOST virsh domifaddr` on
   the KVM host (no local libvirt required on the workstation).
2. `$KUBECTL get nodes -l node-role.kubernetes.io/control-plane` —
   falls back to asking the apiserver via the tunneled kubectl wrapper.

Run from the KVM host itself, omit `KVM_HOST` and the script falls back
to direct SSH + the local `virsh` for resolution. Override with
`KUBECTL=kubectl` if you have a native kubectl already pointed at the
right cluster.

The script requires SSH-as-root to the CP host. It exits 0 only if
PodSecurity rejects a privileged pod, the apiserver audit log is
non-empty, the kubelet is running with `--protect-kernel-defaults=true`,
and the kubelet is running with `--rotate-certificates=true`. Run it
after every redeploy.

The individual checks below are the same controls, broken out for manual
debugging.

### PodSecurity restricted

A privileged pod into a non-exempt namespace must be rejected:

```bash
kubectl run pwn --image=busybox --restart=Never \
  --overrides='{"spec":{"hostPID":true,"containers":[{"name":"x","image":"busybox","securityContext":{"privileged":true}}]}}'
```

Expect an admission webhook response like:

```
Error from server (Forbidden): pods "pwn" is forbidden:
violates PodSecurity "restricted:latest": ...
```

If a workload legitimately needs more than `restricted`, label its
namespace:

```bash
kubectl label namespace my-ns \
  pod-security.kubernetes.io/enforce=privileged --overwrite
```

### Debugging blocked workloads

When a pod fails admission, the operator should:

1. Look at recent admission failures across the cluster:

   ```bash
   kubectl get events -A | grep ForbiddenError
   ```

2. Grep the apiserver audit log on the CP for the failed CreatePod:

   ```bash
   ssh root@<cp> "grep ForbiddenError /var/log/kubernetes/k8s-audit.log"
   ```

3. Decide between two paths:
   - **Harden the workload** so it satisfies `restricted` (`runAsNonRoot`,
     `seccompProfile`, `capabilities.drop: [ALL]`, etc.) — preferred for
     anything you control or want to keep auditable.
   - **Relax the namespace** to `baseline` or `privileged` if it is an
     internal, trusted namespace where hardening the workload is not
     feasible:

     ```bash
     kubectl label namespace my-ns \
       pod-security.kubernetes.io/enforce=privileged --overwrite
     ```

### Audit logging

Generate any apiserver write and tail the log on the CP host:

```bash
kubectl create configmap audit-probe --from-literal=k=v
sudo tail -n 5 /var/log/kubernetes/k8s-audit.log
```

Each line is a JSON `audit.k8s.io/v1` `Event` object. You should see
the `create configmap audit-probe` request recorded at `Request`
level with the user, sourceIPs, and request URI.

### Kubelet protect-kernel-defaults

Read back one of the pre-set sysctls and confirm the kubelet hasn't
trampled it:

```bash
sysctl kernel.panic vm.overcommit_memory
```

Expect `kernel.panic = 10` and `vm.overcommit_memory = 1`. Also
confirm the flag is actually set on the running kubelet:

```bash
sudo ps -ef | grep '[k]ubelet' | tr ' ' '\n' | grep protect-kernel
```

## Caveats

### `restricted` will block almost any third-party workload

`restricted` is the strictest PSS profile. Most off-the-shelf Helm
charts will fail admission until you either:

1. Label the target namespace to a looser profile (`baseline` or
   `privileged`), or
2. Patch the chart's pod spec to satisfy `restricted`
   (`runAsNonRoot`, `seccompProfile`, `capabilities.drop: [ALL]`, etc.).

Plan for this — `restricted` is the right default for a lab cluster,
but it's not invisible to operators.

### Audit log can grow

`/var/log/kubernetes/k8s-audit.log` is on the VM root filesystem with
`--audit-log-maxsize=100` and `--audit-log-maxbackup=5` — i.e. ~600MB
upper bound. On a busy cluster you may want to either reduce the
policy's coverage further or ship logs off-node.

### CNI namespace exemption

The CNI (Cilium) runs in `kube-system`, which is already on the
exemption list — no CNI-specific exemption is required. See
[`docs/cilium-migration.md`](cilium-migration.md) for details on the
CNI swap.

### Cluster recovery from a half-init

If `kubeadm init` dies partway through and you re-run `k8s-init.sh`,
the recovery path (`kubeadm reset` + remove generated YAML) deletes
`encryption-config.yaml` and rewrites `kubeadm-init.yaml`, but does
**not** remove the bake-in image files
(`admission-control-config.yaml`, `audit-policy.yaml`) — they live in
the read-only image layer and survive untouched.

## Cluster-wide policies

After `kubeadm init` and the Cilium install, `k8s-init.sh` applies three
additional manifests that establish a baseline in-cluster posture. The
manifests are baked into the image at build time (so first-boot does
not depend on reachability to upstream registries) and applied with
`kubectl apply` on the admin kubeconfig.

| Control | Manifest | Closes |
|---|---|---|
| metrics-server v0.7.2 (kubectl top) | `containers/shared/kubernetes/metrics-server.yaml` | #73 |
| Default-ns `LimitRange` + `ResourceQuota` | `containers/shared/kubernetes/default-ns-quota.yaml` | #87 |
| `default` SA in `default` ns: `automountServiceAccountToken: false` | `containers/shared/kubernetes/restrict-sa-token-mount.yaml` | #84 |

### metrics-server (closes #73)

The upstream `components.yaml` for v0.7.2 is vendored into the repo with
one local patch: `--kubelet-insecure-tls` is appended to the Deployment's
container args. The kubelet's serving cert is signed by the cluster's
own CA, which metrics-server does not trust by default; we accept that
on a single-node lab cluster rather than wire up a cert-rotation flow.

Verify:

```bash
kubectl top nodes
```

A non-empty table (CPU and memory columns populated) means metrics-server
is scraping the kubelet and the API aggregator path is healthy. If you
see `error: Metrics API not available`, wait ~30s and retry —
metrics-server needs one scrape interval before it can answer.

### Default-namespace LimitRange + ResourceQuota (closes #87)

`default-ns-quota.yaml` installs two objects in `default`:

- A `LimitRange` named `defaults` — every Container in the namespace gets
  a default request (`10m` CPU / `64Mi` memory) and default limit
  (`500m` / `512Mi`) if the pod spec doesn't set its own. This keeps a
  forgotten `kubectl run` from getting Best-Effort QoS.
- A `ResourceQuota` named `caps` — the namespace as a whole is capped at
  `4`/`8` CPU (requests/limits), `8Gi`/`16Gi` memory, and 50 pods. A
  runaway controller can't exhaust the node from the default namespace.

Verify:

```bash
kubectl describe ns default | grep -A3 'Resource Limits\|Resource Quotas'
```

You should see both the LimitRange's defaults and the ResourceQuota's
hard caps listed.

### Restrict default SA token mounting (closes #84)

`restrict-sa-token-mount.yaml` patches *only* the `default` ServiceAccount
in the `default` namespace to opt out of automatic token mounting. Pods
that don't declare a `serviceAccountName` and don't set
`automountServiceAccountToken: true` will not get a kube API token
projected into them. Workloads that legitimately need a token must
declare a different SA or opt back in explicitly.

This is deliberately scoped to one SA: SAs in other namespaces (including
all system namespaces) keep their default of `automountServiceAccountToken: true`
so that controllers and operators that rely on the projected token
continue to work.

Verify:

```bash
kubectl get sa default -n default -o jsonpath='{.automountServiceAccountToken}'
```

Should print `false`.
