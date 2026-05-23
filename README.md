[![PR validate](https://github.com/aatchison/hummingbird-k8s/actions/workflows/pr-validate.yml/badge.svg?branch=main)](https://github.com/aatchison/hummingbird-k8s/actions/workflows/pr-validate.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/aatchison/hummingbird-k8s?include_prereleases&sort=semver)](https://github.com/aatchison/hummingbird-k8s/releases)

# hummingbird-k8s

Fedora Hummingbird bootc images with Kubernetes baked in, in three flavors,
run as KVM VMs via libvirt (`qemu:///system`) on a single host. Each flavor is
published as a signed OCI image to GHCR:

- `ghcr.io/aatchison/hummingbird-k3s:vX.Y.Z` — k3s single-binary, single-node.
- `ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z` — upstream `kubeadm`/`kubelet`/`cri-o` control plane.
- `ghcr.io/aatchison/hummingbird-k8s-worker:vX.Y.Z` — worker that auto-joins the CP on first boot.

All three derive from `quay.io/hummingbird-community/bootc-os`, are built with
`podman build`, converted to `qcow2` via `bootc-image-builder`, and defined
under libvirt. Design notes and gotchas live in [`NOTES.md`](NOTES.md); this
README is the operator-facing entry point.

## Topology

A single KVM host runs one of two layouts.

Upstream-k8s, control plane + N workers:

```text
        client laptop
              |
              | ssh -L 6443:127.0.0.1:6443  (via kubectl-k8s.sh)
              v
   +-------------------------------------------------+
   |  KVM host  (qemu:///system, default NAT net)    |
   |                                                 |
   |   +-----------------+     +------------------+  |
   |   | hummingbird-k8s |<----| hummingbird-k8s- |  |
   |   |  (control plane)|     |   worker  x N    |  |
   |   |  192.168.122.X  |     |  192.168.122.Y..Z|  |
   |   +-----------------+     +------------------+  |
   |        virbr0  192.168.122.1/24 (libvirt NAT)   |
   +-------------------------------------------------+
```

k3s single-node:

```text
        client laptop --(ssh)--> KVM host --> hummingbird-k3s VM
```

## Prerequisites

Before running `make k3s` / `make k8s`, the KVM host needs:

- Fedora 41+ or RHEL 9.x with libvirt 10+ and `qemu:///system` reachable
  (`virsh -c qemu:///system list` should succeed without errors).
- Tooling on `$PATH`: `podman`, `virt-install`, `qemu-img`, `gh`, `git`.
  `bootc-image-builder` can be installed locally or pulled on demand
  (`quay.io/centos-bootc/bootc-image-builder:latest`).
- User in the `libvirt` group, or run the operator targets with `sudo`:

  ```bash
  sudo usermod -aG libvirt "$USER"
  newgrp libvirt
  ```

- Minimum host capacity for a 1 CP + 2 worker cluster: **16 GB RAM, 4 vCPU,
  60 GB free disk** in `POOL_DIR` (default `/var/lib/libvirt/images/`).
- Host architecture: **linux/amd64 or linux/arm64**. Published images are
  multi-arch manifest indexes; `bootc switch` resolves the right child
  automatically. See [`docs/multi-arch.md`](docs/multi-arch.md).
- An SSH keypair on the host (`~/.ssh/id_ed25519` by default) registered with
  GitHub — the build/publish flow and the SSH commit-signing config both
  consume it.

## Resource requirements

VM sizing defaults are tuned for a small homelab cluster. Override per host
via `config.local.sh`.

| Flavor | RAM | vCPU | qcow2 disk |
| --- | --- | --- | --- |
| `hummingbird-k8s` (control plane) | 8 GB | 4 | 30 GB |
| `hummingbird-k8s-worker` (each) | 4 GB | 2 | 20 GB |
| `hummingbird-k3s` (single-node) | 4 GB | 2 | 20 GB |

Sizing guidance:

- CP RAM is dominated by etcd + apiserver; do not drop below 4 GB.
- Each worker reserves ~500 MB for kubelet/cri-o + Cilium; 4 GB leaves
  ~3 GB for workloads.
- qcow2 files live in `POOL_DIR`. Point `POOL_DIR=/mnt/ssd/libvirt` at an SSD
  for noticeably better etcd write latency; HDDs work but `etcdctl defrag`
  cadence matters more.
- CP/worker memory and vCPU are tunable via `CP_MEMORY`, `CP_VCPUS`,
  `WORKER_MEMORY`, `WORKER_VCPUS` (see #91 once configurable sizing lands).

## Quick start (operator)

On a freshly-set-up KVM host (libvirtd running, `qemu:///system` reachable,
`podman` + `bootc-image-builder` available):

```bash
make help                      # cheatsheet of all targets
sudo make k3s                  # build + define hummingbird-k3s VM
sudo make k8s                  # build + define hummingbird-k8s control plane
sudo make workers COUNT=2      # spawn 2 workers (depends on k8s being up)
make verify-all                # encryption + hardening + app-deploy smoke test
```

The Makefile is the operator entry point — every recipe delegates to a script
under [`scripts/`](scripts/). Run `make help` for the full target list. Direct
script invocations still work (`sudo bash scripts/redo-k8s.sh`); see
[`docs/makefile.md`](docs/makefile.md) for honored env vars.

From a client machine with `KVM_HOST` pointed at the KVM host SSH alias:

```bash
make nodes
# or, for arbitrary kubectl:
make kubectl ARGS='get pods -A'
```

`scripts/kubectl-k8s.sh` opens an SSH tunnel to the CP's apiserver on
`127.0.0.1:6443` and runs `kubectl` in a container against the fetched
admin kubeconfig.

## Useful commands

### On the CP node (SSH'd as root)

```bash
# kubectl with the in-cluster admin kubeconfig
export KUBECONFIG=/etc/kubernetes/admin.conf
kubectl get nodes -o wide
kubectl get pods -A
kubectl top nodes                  # needs metrics-server (auto-installed)

# Cilium status + Hubble flows (Hubble enabled by default since #168)
cilium status --wait
cilium hubble port-forward &       # tunnel to relay
hubble observe --follow            # stream all flows
hubble observe --verdict DROPPED   # only policy-dropped traffic

# bootc lifecycle
bootc status --json | jq '.status.booted.image.image.image'
bootc upgrade                      # pull + stage the next image
bootc rollback && systemctl reboot # roll back if a bad upgrade landed
systemctl status k8s-init.service
journalctl -fu k8s-init.service
```

### From a client (laptop / dev box)

```bash
# Tunnel + run kubectl through the bitnami container or native kubectl
KVM_HOST=geary make nodes
KVM_HOST=geary make kubectl ARGS='get pods -A'
KVM_HOST=geary make kubectl ARGS='-n kube-system logs ds/cilium'

# Verify suite (PSA + audit + kubelet protect-kernel + rotate-certs)
KVM_HOST=geary make verify-hardening
KVM_HOST=geary make verify-encryption
KVM_HOST=geary make verify-app-deploy   # PSA-restricted nginx smoke test
KVM_HOST=geary make verify-all          # all three in sequence

# kube-bench (CIS benchmark scan)
KVM_HOST=geary make kube-bench
```

### Cluster lifecycle

```bash
# Single-host quickstart (dev iteration)
sudo make k8s                    # CP
sudo make workers COUNT=2        # workers
sudo make clean-vms              # tear down all hummingbird-* VMs

# Production-style deploy (hybrid bib + cloud-init)
cp cluster.example.conf cluster.local.conf   # edit it
sudo make deploy-cluster  CONFIG=cluster.local.conf
sudo make destroy-cluster CONFIG=cluster.local.conf

# Point already-deployed VMs at GHCR so the auto-update timer pulls
sudo make switch-to-ghcr
```

### Rolling cluster updates

`make update-cluster` walks the cluster one node at a time, pulls the latest
bootc image with `bootc upgrade --apply`, and reboots into it. Workers are
`kubectl drain`-ed before reboot and `kubectl uncordon`-ed after they rejoin
Ready; the CP is rebooted in place (single-CP topology, no peers to drain
to). The same `cluster.local.conf` that drives `make deploy-cluster` drives
the update.

```bash
# Full rolling update: CP first (no drain — brief apiserver outage), then
# each worker drained -> upgraded -> uncordoned in WORKER_NAMES order.
sudo make update-cluster  CONFIG=cluster.local.conf

# Skip the CP; only roll the workers (useful if CP is already on latest).
sudo make update-workers  CONFIG=cluster.local.conf

# Touch exactly one node — useful for spot-fixing a stuck worker or for
# canarying a new image against a single node before rolling the rest.
sudo make update-node     CONFIG=cluster.local.conf NODE=hbird-w1
```

Heads-up: single-CP means the apiserver is unavailable for ~60-120s while
the CP reboots. `scripts/update-cluster.sh` prints a warning before that
window. Add `--dry-run` (calling the script directly) to preview actions
without ssh/kubectl, or `--skip-drain` as an emergency-rollback escape
hatch for a stuck drain.

Exercised end-to-end by `integration-update-cluster.yml` (see
[docs/integration-tests.md](docs/integration-tests.md)).

Full reference: [docs/update-cluster.md](docs/update-cluster.md).

### etcd backup + key rotation

```bash
sudo make backup-etcd                              # ./backups/etcd-snapshot-<ts>.db
sudo make backup-etcd LABEL=pre-cni-swap           # labeled snapshot
sudo make restore-etcd SNAP=./backups/<file>.db    # destructive; prompts
sudo make rotate-etcd-key                          # walks 4-stage key rotation
```

### Fetch a working kubeconfig

`make get-kubeconfig` is the daily-use sibling of `make export-argocd` — same
SSH-and-rewrite primitive (`scripts/export-argocd.sh`), but with
operator-facing defaults: writes to `./kubeconfig.yaml` and names the
cluster/context/user after `CP_NAME` (no `hummingbird-` prefix) so
`kubectl --context=<CP_NAME>` matches the libvirt domain name you already
think in.

> **Security model**: the file produced is `admin.conf` with the server URL
> rewritten — full-cluster credentials. The client cert inside has a 1-year
> default lifetime (kubeadm's default). Treat it like a private key: written
> with mode 0600, never commit to git (this repo's `.gitignore` excludes
> `kubeconfig.yaml` + `argocd-kubeconfig.yaml`), delete or move to a secure
> store after use. To re-fetch after the cert rotates (or expires), pass
> `FORCE=1`. See [docs/argocd.md](docs/argocd.md) for the leak-recovery
> playbook (which applies identically to this target).

```bash
make get-kubeconfig CONFIG=cluster.local.conf
KUBECONFIG=./kubeconfig.yaml kubectl get nodes

# Override the apiserver URL / context name / output / overwrite-if-present:
make get-kubeconfig CONFIG=cluster.local.conf \
    SERVER=https://cluster.example.com:6443 \
    CONTEXT=prod-cp \
    OUTPUT=$HOME/.kube/hummingbird.yaml \
    FORCE=1
```

For ArgoCD registration use `make export-argocd` instead — same fetch+rewrite
logic, but it names the context `hummingbird-<CP_NAME>` to avoid colliding
with whatever the operator already has in `~/.kube/config`.

### Register the cluster with ArgoCD

`make export-argocd` SSHes to the CP, pulls `/etc/kubernetes/admin.conf`, and
rewrites it into a kubeconfig you can hand to `argocd cluster add`. ArgoCD
only needs the file once — it bootstraps its own scoped ServiceAccount in
the target cluster and stores only that SA's token going forward.

```bash
# Produce ./argocd-kubeconfig.yaml (mode 0600).
make export-argocd CONFIG=cluster.local.conf

# Register the cluster (ArgoCD CLI logged in to your ArgoCD server).
argocd cluster add hummingbird-hbird-cp1 --kubeconfig argocd-kubeconfig.yaml

# Override the apiserver URL if the CP's libvirt IP isn't reachable from
# the ArgoCD pod (LB, ingress, DNS name, etc.):
make export-argocd CONFIG=cluster.local.conf SERVER=https://cluster.example.com:6443

# Re-export (e.g. after `kubeadm certs renew all`) — refuses by default
# to overwrite an existing file:
make export-argocd CONFIG=cluster.local.conf FORCE=1
```

The exported file IS `admin.conf` — a full-cluster credential, with a
1y client-cert lifetime baked in by kubeadm. Delete or chmod-0600-store
it after ArgoCD has registered the cluster, since ArgoCD authenticates
with its own SA token from that point on. See
[docs/argocd.md](docs/argocd.md) for the security model, the (honest)
leak-recovery playbook, and the cert-lifecycle / re-export procedure.

Exercised end-to-end by `integration-export-argocd.yml` (see
[docs/integration-tests.md](docs/integration-tests.md)).

## Configuration

Per-host overrides live in `config.local.sh` (gitignored). Copy
[`config.example.sh`](config.example.sh) to start. Variables are sourced by
the `scripts/build-*.sh` and `scripts/define-vm*.sh` scripts.

| Variable | Default | Controls | Override when |
| --- | --- | --- | --- |
| `VM_USER` | `core` | Initial user account. | Renaming the login user. |
| `VM_USER_GROUPS` | empty | Supplementary groups. | Set `wheel` to allow sudo. |
| `VM_PASSWORD` | empty | Password for `VM_USER`. | Console login; keep empty for key-only. |
| `ENABLE_ROOT_SSH` | `1` | Bake pubkeys into `root@`. | Set `0` to disable root SSH. |
| `SSH_PUBKEY_FILES` | `~/.ssh/id_ed25519.pub` | Pubkey files (colon-sep). | Multiple or non-default keys. |
| `POOL_DIR` | `/var/lib/libvirt/images` | libvirt pool for qcow2. | Larger volume. |
| `BASE_IMAGE` | `quay.io/hummingbird-community/bootc-os@sha256:3bed2fc1…` (digest-pinned) | Upstream bootc base. | Bumping to a newer digest. |
| `BIB` | `quay.io/centos-bootc/bootc-image-builder:latest` | OCI-to-qcow2 builder. | Pinning bib. |
| `KVM_HOST` | unset | SSH alias of KVM host (client-side). | Always on client running `kubectl-k8s.sh`. |
| `APISERVER_EXTRA_SANS` | `127.0.0.1,localhost` | Extra SANs in apiserver cert. | Adding client host/IP for tunnel. |

## Consuming the published images

Existing bootc-based VMs can switch to a published Hummingbird image without
rebuilding locally:

```bash
sudo bootc switch ghcr.io/aatchison/hummingbird-k3s:v0.1.0
sudo systemctl reboot
```

Substitute `hummingbird-k8s` or `hummingbird-k8s-worker` as needed. Images are
signed with cosign (keyless OIDC) at publish time. Verify before switching;
see [`docs/image-verification.md`](docs/image-verification.md) for the
`cosign verify` command and the expected issuer/identity.

## Release process

GitHub Actions builds and publishes one flavor per tag:

| Tag pattern | Workflow | Published image |
| --- | --- | --- |
| `k3s/vX.Y.Z` | `build-k3s.yml` | `ghcr.io/aatchison/hummingbird-k3s:vX.Y.Z` |
| `k8s/vX.Y.Z` | `build-k8s.yml` | `ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z` |
| `worker/vX.Y.Z` | `build-worker.yml` | `ghcr.io/aatchison/hummingbird-k8s-worker:vX.Y.Z` |

Flavors version independently:

```bash
git tag k3s/v0.2.0
git push --tags
```

The publish job refuses to run if the tagged commit isn't reachable from
`main`, so accidental tags on a topic branch will not produce a release.

## Operations

Day-2 documentation lives under [`docs/`](docs):

- [`docs/makefile.md`](docs/makefile.md) — `make` cheatsheet over the underlying driver scripts.
- [`docs/image-verification.md`](docs/image-verification.md) — verify GHCR images with cosign.
- [`docs/etcd-encryption.md`](docs/etcd-encryption.md) — enable encryption-at-rest for etcd.
- [`docs/worker-tokens.md`](docs/worker-tokens.md) — short-TTL, per-VM kubeadm join tokens.
- [`docs/self-hosted-runner.md`](docs/self-hosted-runner.md) — register a KVM-capable GitHub Actions runner.
- [`docs/auto-updates.md`](docs/auto-updates.md) — bootc auto-update timer behavior.
- [`docs/rollback.md`](docs/rollback.md) — manual + auto-rollback (`bootc rollback`, health-check timer).
- [`docs/security-hardening.md`](docs/security-hardening.md) — PodSecurity restricted + apiserver audit + kubelet protect-kernel-defaults; run `make verify-hardening` (or `scripts/verify-hardening.sh`) after each redeploy.
- [`docs/app-deploy-verify.md`](docs/app-deploy-verify.md) — end-to-end smoke test of a PSA-restricted nginx deploy + pod-to-pod networking.
- [`docs/cilium-migration.md`](docs/cilium-migration.md) — Cilium CNI (NetworkPolicy enforcement, eBPF datapath).
- [`docs/troubleshooting.md`](docs/troubleshooting.md) — known failure modes and fixes from operating the cluster.
- [`docs/vm-sizing.md`](docs/vm-sizing.md) — VM resource defaults, tunables, sizing guidance.
- [`docs/backup-restore.md`](docs/backup-restore.md) — etcd snapshot + restore.
- [`docs/k8s-version-upgrade.md`](docs/k8s-version-upgrade.md) — K8s major-version upgrade strategy.
- [`docs/cloud-init.md`](docs/cloud-init.md) — opt-in cloud-init support (`ENABLE_CLOUD_INIT=1`) for per-VM user-data injection via libvirt seed ISO.
- [`docs/multi-arch.md`](docs/multi-arch.md) — multi-arch (linux/amd64 + linux/arm64) manifest index, cosign verification, and CI boot-test coverage.
- [`docs/orchestrator.md`](docs/orchestrator.md) — weekly verify orchestrator (encryption + hardening + app-deploy against the live cluster).
- [`docs/deploy-cluster.md`](docs/deploy-cluster.md) — `make deploy-cluster` hybrid bib + cloud-init orchestrator for deploying 1 CP + N workers from a single config file.

Workflows that need real KVM (orchestrator integration, bootc upgrade e2e)
run on a self-hosted runner on the operator's KVM host.

## Repo layout

```
containers/<flavor>/   per-flavor Containerfile + first-boot scripts
                       (k3s, k8s, k8s-worker). Shared in-image assets
                       under containers/shared/.
scripts/               every driver script (build, define, redo, spawn,
                       kubectl wrapper, verifiers, kube-bench).
lib/                   build-common.sh — sourced by scripts/build-*.sh.
docs/                  day-2 docs (image verification, hardening, etc.).
references/            external materials referenced by docs.
Makefile               canonical operator entry point; all targets call
                       scripts/. `make help` lists them.
```

## Troubleshooting

Common failures and the first thing to try. For deeper context, see
[`NOTES.md`](NOTES.md) and the per-topic docs under [`docs/`](docs).

- **`kubectl` context "connection refused" from the client.** The SSH tunnel
  or fetched kubeconfig is stale. Re-run `make kubectl ARGS='get nodes'`
  (which refreshes `/tmp/k8s-kubeconfig` and the tunnel via
  [`scripts/kubectl-k8s.sh`](scripts/kubectl-k8s.sh)).
- **Cilium pods `CrashLoopBackOff` on the CP.** Usually a kernel BPF
  capability gap — rare on the Hummingbird base, common on minimal Fedora
  hosts. Check `kubectl -n kube-system logs ds/cilium` and confirm
  `CONFIG_BPF_SYSCALL=y` on the VM kernel.
- **`k8s-init.service` failed in `cilium install`.** Fixed in v0.1.10+
  (`$HOME` was unset under the systemd unit). Upgrade the CP image.
- **Worker stays `NotReady` after `make workers`.** The Cilium daemonset
  hasn't scheduled there yet; wait ~60s after join, then re-check
  `kubectl get nodes`.
- **`bootc-image-builder` qcow2 build fails inside a container.** Usually
  the podman storage driver — overlay-on-overlay can't run bib. Export
  `STORAGE_DRIVER=vfs` before `make k8s` (see #124).
- **`ssh-keygen` host-key conflict on a freshly redefined VM.** The libvirt
  NAT lease was reused; clear the stale entry:

  ```bash
  ssh-keygen -R 192.168.122.10
  ```

- **Multi-host clusters.** Not supported today — `qemu:///system` and the
  libvirt NAT topology assume one host. HA CP across hosts is tracked in
  #11.

## License

Licensed under the Apache License, Version 2.0. See [`LICENSE`](LICENSE) and
[`NOTICE`](NOTICE).
