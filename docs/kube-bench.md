# kube-bench: CIS Kubernetes Benchmark scan

This document covers how `hummingbird-k8s` runs
[aquasecurity/kube-bench](https://github.com/aquasecurity/kube-bench)
against the live cluster, where the baseline lives, and how to triage
findings.

## What kube-bench is

kube-bench is a Go binary, published as a container image, that walks a
running Kubernetes cluster and grades it against the
[CIS Kubernetes Benchmark](https://www.cisecurity.org/benchmark/kubernetes).
It checks file permissions on `/etc/kubernetes/*`, flags on the static
pod manifests (`kube-apiserver`, `kube-controller-manager`,
`kube-scheduler`, `etcd`), kubelet config, and a handful of policy
items.

We care because the benchmark codifies the well-known apiserver,
kubelet, and etcd hardening knobs — exactly the surface we have other
issues open against (PodSecurity #7, audit logs #13, etcd encryption #8,
Cilium NetworkPolicy #6, …). Running kube-bench gives us a single
authoritative checklist instead of curating one ourselves.

Important: kube-bench grades the **actual running config**, not the
image. Anything you change in `k8s-init.sh` — kubeadm flags, file
modes, encryption config — will show up in the next run. Rerun after
every `k8s-init.sh` change.

## How to run

From a client with SSH access to the KVM host (`thegeary`, `judah`,
etc.):

```sh
KVM_HOST=thegeary bash scripts/run-kube-bench.sh
```

The script uses `kubectl-k8s.sh` under the hood (SSH tunnel + podman
kubectl, no local kubectl install required).

What it does:

1. Confirms the cluster is reachable.
2. Notes the control-plane node (label
   `node-role.kubernetes.io/control-plane`) for the operator's records.
3. Applies aquasec's `job-master.yaml` — same kube-bench image, but
   with nodeAffinity for `node-role.kubernetes.io/control-plane` and a
   matching toleration, so it scans the CP (sections 1.x apiserver /
   2.x etcd / 3.x control-plane config).
4. Waits for that Job to complete, captures its logs.
5. Applies `job-node.yaml`, which lands on a worker and covers 4.x
   kubelet checks.
6. Waits and captures those logs too.
7. Streams the combined report to stdout, banner-separated per target.
8. Prints a `[FAIL]` + `[WARN]` summary to stderr.
9. Deletes both Jobs on exit.

Running just `job.yaml` (the combined manifest) is the obvious
default, but it lets scheduling decide which node gets scanned — and
on a cluster with more workers than CPs the master-target sections go
missing silently. The two-Job approach pins the role.

The full report goes to stdout; the summary goes to stderr. That way
you can refresh the baseline file with:

```sh
KVM_HOST=thegeary bash scripts/run-kube-bench.sh > scripts/kube-bench-baseline.txt
```

…and the human-readable summary still prints to your terminal.

### Knobs

| Env                  | Default              | Meaning                                  |
|----------------------|----------------------|------------------------------------------|
| `KUBE_BENCH_VERSION` | `v0.15.5`            | kube-bench release tag, pinned for reproducibility. |
| `KUBE_BENCH_TIMEOUT` | `5m`                 | `kubectl wait` budget per Job.           |
| `KUBE_BENCH_NS`      | `default`            | Namespace to run the Jobs in.            |
| `KUBE_BENCH_TARGETS` | `master node`        | Space-separated subset of `{master, node}`. Skip a half if you only want one. |
| `KUBECTL`            | `./kubectl-k8s.sh`   | Override if you have a local kubectl.    |
| `KVM_HOST`           | (none)               | Required by `kubectl-k8s.sh`. See `config.example.sh`. |

## How to read the output

kube-bench prefixes every check with one of:

- `[PASS]` — control is implemented.
- `[FAIL]` — control is missing and the benchmark considers it a hard
  miss. Remediation steps are printed below the line.
- `[WARN]` — control either can't be auto-checked or is conditional
  (e.g. "if you use feature X, configure Y"). Read the remediation
  before deciding it's a no-op for us.
- `[INFO]` — context only, no action implied.

Sections (the leading number in each check ID, e.g. `1.2.5`):

- **1.x master**: control-plane components and their static pod manifests.
- **2.x etcd**: etcd-specific flags.
- **3.x control-plane configuration**: policy files referenced by the apiserver.
- **4.x worker**: kubelet config and on-node file modes.
- **5.x policies**: cluster-level workload + RBAC policies (PodSecurity, NetworkPolicy, default service accounts, etc.).

For a single-node lab cluster, the categories that matter most are
**1.x** (do our apiserver flags match the recommendations?) and **5.x**
(do we have PodSecurity admission and a default NetworkPolicy?).
**4.x** matters once we have worker nodes joining.

## Baseline

The checked-in baseline lives at
[`scripts/kube-bench-baseline.txt`](../scripts/kube-bench-baseline.txt).

It is **not** a CI gate. It is a snapshot of the report from a known
cluster state, kept in the repo so that:

- Reviewers can see what kube-bench thinks of our defaults without
  spinning up a cluster.
- Future runs can be diffed against it (`diff baseline.txt
  <(bash scripts/run-kube-bench.sh)`) to spot regressions.

When `k8s-init.sh`, the kubeadm config, or any default policy changes,
rerun the script and refresh the baseline file.

### Current baseline status (2026-05-22)

The committed baseline was captured with an earlier draft of this
script that used kube-bench's combined `job.yaml`, which landed on a
worker. As a result, only the **node-target** sections (4.x kubelet,
5.x policies) are present. The master-target sections (1.x apiserver,
2.x etcd, 3.x control-plane config) need a follow-up run of the
current script — which now applies `job-master.yaml` and
`job-node.yaml` explicitly — from an operator session that has
working ssh+sudo through the KVM host.

The captured node-target slice already shows:

- 1 `[FAIL]`: 4.2.6 `--protect-kernel-defaults=true` is not set on
  the kubelet.
- 27 `[WARN]`: mostly 5.x Manual policy checks (no PodSecurity, no
  NetworkPolicy, default ServiceAccounts mounted, etc.) — these map
  to the issues in the table below.

## Burning down findings

Each high-severity category gets its own backlog issue, not one
per-line. The mapping today:

| kube-bench area                                | Issue |
|-----------------------------------------------|-------|
| PodSecurity admission (default `restricted`)  | [#7](https://github.com/aatchison/hummingbird-k8s/issues/7) |
| Apiserver audit logging                        | [#13](https://github.com/aatchison/hummingbird-k8s/issues/13) |
| etcd encryption at rest                        | [#8](https://github.com/aatchison/hummingbird-k8s/issues/8) (merged) |
| NetworkPolicy enforcement (Cilium)             | [#6](https://github.com/aatchison/hummingbird-k8s/issues/6) |
| Kubeadm join token TTLs                        | [#9](https://github.com/aatchison/hummingbird-k8s/issues/9) (merged) |

When you triage a new run, assign individual `[FAIL]` lines to whichever
of these issues already covers the category. Only open a new issue if
the finding doesn't fit any of the above (e.g. file-mode hardening on
`/etc/kubernetes/*`).

Do **not** auto-file an issue per finding. kube-bench's failure list is
noisy and a lot of the items are "we don't use that feature, so the
check is irrelevant"; let the operator triage.

## Caveats

- kube-bench reads on-host paths via `hostPath` mounts and uses
  `hostPID`. That's why we run it as an in-cluster Job — running the
  binary off-cluster would have nothing to inspect. The Job manifest
  matches upstream's; we pin the release tag (default `v0.15.5`) for
  reproducibility.
- The kube-bench container is `docker.io/aquasec/kube-bench`. We
  don't currently verify its signature (cf. #5). Worth keeping in mind
  when interpreting the baseline.
- Results depend on the cluster's actual config, **not** on the bootc
  image alone. A VM still running an older `k8s-init.sh` will produce
  a different baseline than one rebuilt against current `main`.
