# App-deploy smoke test

`scripts/verify-app-deploy.sh` is a post-deploy verifier that confirms the
cluster can actually run a workload the way an operator would — not just
that the apiserver answers and nodes report Ready.

## What it verifies

- A normal `Deployment` + `Service` can be created in a non-system
  namespace and is admitted under the cluster-default `restricted`
  Pod Security Standard (see [`security-hardening.md`](security-hardening.md) #1).
- The Deployment becomes `Available` within 2 minutes (image pull,
  scheduling, kubelet, runtime all healthy).
- A second PSA-restricted Pod (busybox probe) can resolve the Service
  name and reach the backend Pod over the ClusterIP — i.e. pod-to-pod
  networking and CoreDNS both work. This is CNI-agnostic: the cluster
  currently runs Cilium (see [`cilium-migration.md`](cilium-migration.md)),
  but the same checks passed against the legacy flannel install before
  the migration landed in #50/#56.

## How to run

### From a workstation through the KVM host (recommended)

This is the common topology — your laptop reaches the cluster only via
SSH through the KVM host. Set `KVM_HOST` and the verifier auto-defaults
`KUBECTL` to the in-repo `scripts/kubectl-k8s.sh` wrapper, which tunnels
kubectl through the KVM host's libvirt NAT. No local kubectl context
required (issue #271 F3):

```bash
KVM_HOST=geary make verify-app-deploy
# ...or, via CONFIG (kubectl-k8s.sh-style: pulls CP_NAME / KVM_HOST from
# the cluster.local.conf you generated at deploy time):
make verify-app-deploy CONFIG=cluster.local.conf
```

The Makefile recipe forwards `CONFIG` and the operator's `KVM_HOST` to
the script, which sources `CONFIG` and then sets `KUBECTL` to the
tunneled wrapper when `KVM_HOST` is set. The wrapper in turn uses
`ssh -t` to `$KVM_HOST` for the `sudo virsh domifaddr` IP probe so a
cold sudo cache on the KVM host can still prompt for your password
(issue #249).

### From the CP host (kubectl on `PATH`)

When the script is running on the control-plane node itself (e.g. the
self-hosted runner copies it onto the VM and execs it there — see
`tests/integration-boot.sh`), no `KVM_HOST` is set and the script falls
back to plain `kubectl`:

```bash
./scripts/verify-app-deploy.sh
# or, via the Makefile from a checkout on the CP host:
make verify-app-deploy
```

### Explicit kubectl override

You can still spell out `KUBECTL=` for any wrapper or path:

```bash
KUBECTL=./scripts/kubectl-k8s.sh ./scripts/verify-app-deploy.sh
KUBECTL=kubectl ./scripts/verify-app-deploy.sh   # force plain kubectl
```

An explicit `KUBECTL=` always wins over the `KVM_HOST`-derived default.

## Expected output

```text
[verify-app-deploy] creating namespace smoketest-1716396200
[verify-app-deploy] applying nginx Deployment + Service in smoketest-1716396200
[verify-app-deploy] waiting up to 2m for deployment/nginx to become Available
[verify-app-deploy] probing http://nginx:8080 from an in-cluster busybox pod
[verify-app-deploy] PASS: nginx returned the welcome page over ClusterIP
[verify-app-deploy] verify-app-deploy: PASS
[verify-app-deploy] cleanup: deleting namespace smoketest-1716396200
```

A `trap` ensures the `smoketest-<timestamp>` namespace is deleted on
both success and failure paths.

## What it does not cover

These are intentionally out of scope and tracked as follow-ups:

- External LoadBalancer / NodePort reachability from outside the cluster.
- Ingress (no controller is assumed to be installed).
- PersistentVolumes / CSI.
- NetworkPolicy enforcement (`restricted` PSS does not imply NetPol).

If any of these become a supported feature of `hummingbird-k8s`, file a
follow-up issue and extend this script (or add a sibling verifier).

## Rust counterpart

`hbird verify app-deploy --config cluster.local.conf` is the Rust twin
(Phase 2, [PR #330]) — same PSA-restricted nginx deploy +
pod-to-pod-connectivity probe sequence. Live-validated against the
geary cluster. See
[`docs/rust-cli-migration.md`](rust-cli-migration.md#verify-encryption--verify-hardening--verify-app-deploy--verify-all)
for the per-flag map.

[PR #330]: https://github.com/aatchison/hummingbird-k8s/pull/330
