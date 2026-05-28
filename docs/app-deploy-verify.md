# App-deploy smoke test

`hbird verify app-deploy` is a post-deploy verifier that confirms the
cluster can actually run a workload the way an operator would — not
just that the apiserver answers and nodes report Ready. It replaced
the bash twin `scripts/verify-app-deploy.sh` in the v0.1.0 cutover
([#353]).

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
SSH through the KVM host. Set `KVM_HOST` and `hbird verify
app-deploy` SSHes through it as ProxyJump to `root@CP_IP`:

```bash
KVM_HOST=geary make verify-app-deploy CONFIG=cluster.local.conf
# ...or, directly via hbird:
KVM_HOST=geary hbird verify app-deploy --config cluster.local.conf
```

The Makefile recipe forwards `CONFIG` and the operator's `KVM_HOST`
to the Rust verifier, which sources `CONFIG` and uses `KVM_HOST` as
ProxyJump for the SSH chain.

### From the KVM host directly (on-host detection skip)

When `hbird verify app-deploy` runs on the KVM host itself (e.g.
inside `make deploy-cluster`'s `RUN_VERIFY=true` re-exec, or operator
driving the verifier directly on the hypervisor), the Rust path
detects that `hostname -s` matches `KVM_HOST` and **skips with
exit 0** — there is no usable in-place verifier path (plain `kubectl`
is generally not on the KVM host's PATH; setting up a tunnel-to-self
is the bug). The skip is logged so operators see:

```
[verify-app-deploy] already on KVM_HOST (geary); skipping in-place verify to avoid ssh-to-self loop (#353/#362)
[verify-app-deploy]   hint: re-run from a workstation with: hbird verify app-deploy --config <conf> --kvm-host geary
```

This detection mirrors the bash #362 fix (PR #364) ported to the
Rust path; known limitation: when `KVM_HOST` is an IP literal or
a `~/.ssh/config` alias whose `Host` name does not match the short
hostname, detection misses. The cluster is up either way; the
verifier can be re-run from a workstation.

### Explicit overrides

You can still spell out `--cp-ip` or `--cp-name` to bypass config
discovery:

```bash
hbird verify app-deploy --cp-ip 192.168.122.42
hbird verify app-deploy --config cluster.local.conf --cp-name hbird-cp1
```

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

A RAII cleanup guard ensures the `smoketest-<timestamp>` namespace
is deleted on both success and failure paths (mirroring the bash
twin's `trap cleanup EXIT`).

## What it does not cover

These are intentionally out of scope and tracked as follow-ups:

- External LoadBalancer / NodePort reachability from outside the cluster.
- Ingress (no controller is assumed to be installed).
- PersistentVolumes / CSI.
- NetworkPolicy enforcement (`restricted` PSS does not imply NetPol).

If any of these become a supported feature of `hummingbird-k8s`, file a
follow-up issue and extend the Rust verifier (`rust/crates/hbird-cli/src/commands/verify.rs`).

## Implementation

`hbird verify app-deploy --config cluster.local.conf` lives at
`rust/crates/hbird-cli/src/commands/verify.rs::run_verify_app_deploy`
(Phase 2, [PR #330]) — same PSA-restricted nginx deploy +
pod-to-pod-connectivity probe sequence as the (now removed) bash
twin. Live-validated against the geary cluster. See
[`docs/rust-cli-migration.md`](rust-cli-migration.md#tldr--side-by-side)
for the per-flag map.

[PR #330]: https://github.com/aatchison/hummingbird-k8s/pull/330
[#353]: https://github.com/aatchison/hummingbird-k8s/issues/353
