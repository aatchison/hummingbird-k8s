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
  networking and CoreDNS both work. This is CNI-agnostic and is
  expected to pass against both flannel (current) and Cilium (after
  the migration in #50/#56).

## How to run

On the CP host (kubectl on `PATH`, kubeconfig at the default location):

```bash
./scripts/verify-app-deploy.sh
```

From a client laptop via the tunnel:

```bash
KUBECTL=./kubectl-k8s.sh ./scripts/verify-app-deploy.sh
```

The script honors `KUBECTL` (default: `kubectl`) so it can be routed
through `kubectl-k8s.sh` or any other wrapper.

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
