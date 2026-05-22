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

The apiserver writes JSON audit records to `/var/log/k8s-audit.log` on
the host, via these flags:

| Flag | Value |
|------|-------|
| `--audit-policy-file` | `/etc/kubernetes/audit-policy.yaml` |
| `--audit-log-path` | `/var/log/k8s-audit.log` |
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

`/var/log` is mounted into the apiserver static pod as a writable
HostPath so the apiserver process inside the pod can append to the log
on the host filesystem.

### 3. Kubelet `--protect-kernel-defaults=true` (kube-bench CIS 4.2.6)

The kubelet is started with `--protect-kernel-defaults=true` via
`InitConfiguration.nodeRegistration.kubeletExtraArgs`. With this flag
set, the kubelet refuses to *modify* kernel parameters at runtime — it
will only start if the host already has the values it expects.

`Containerfile.k8s` therefore pre-sets the relevant sysctls in
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

## Where the configs live

| Path inside image | Source in repo |
|---|---|
| `/etc/kubernetes/admission-control-config.yaml` | `etc/kubernetes/admission-control-config.yaml` |
| `/etc/kubernetes/audit-policy.yaml` | `etc/kubernetes/audit-policy.yaml` |
| `/etc/sysctl.d/k8s.conf` | inline in `Containerfile.k8s` |
| `/etc/kubernetes/kubeadm-init.yaml` | generated at first boot by `k8s-init.sh` |

All three files baked into the image are read-only references; the
apiserver static pod mounts them via `extraVolumes` in
`ClusterConfiguration.apiServer`.

## Verifying each control

All three controls can be checked in one shot with
[`scripts/verify-hardening.sh`](../scripts/verify-hardening.sh):

```bash
CP_IP=192.168.122.42 ./scripts/verify-hardening.sh
```

The script requires `kubectl` access to the cluster and SSH-as-root to the
CP host. It exits 0 only if PodSecurity rejects a privileged pod, the
apiserver audit log is non-empty, and the kubelet is running with
`--protect-kernel-defaults=true`. Run it after every redeploy.

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

2. Grep the apiserver audit log on the CP for the failed CreatePod (path
   becomes `/var/log/kubernetes/k8s-audit.log` after #50; today's path is
   `/var/log/k8s-audit.log`):

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
sudo tail -n 5 /var/log/k8s-audit.log
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

`/var/log/k8s-audit.log` is on the VM root filesystem with
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
