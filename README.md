[![PR validate](https://github.com/aatchison/hummingbird-k8s/actions/workflows/pr-validate.yml/badge.svg?branch=main)](https://github.com/aatchison/hummingbird-k8s/actions/workflows/pr-validate.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/aatchison/hummingbird-k8s?include_prereleases&sort=semver)](https://github.com/aatchison/hummingbird-k8s/releases)

# hummingbird-k8s

Fedora Hummingbird bootc images with upstream `kubeadm` Kubernetes baked in,
run as KVM VMs via libvirt (`qemu:///system`) on a single host. Two flavors
are published as signed OCI images to GHCR:

- `ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z` — upstream `kubeadm`/`kubelet`/`cri-o` control plane.
- `ghcr.io/aatchison/hummingbird-k8s-worker:vX.Y.Z` — worker that auto-joins the CP on first boot.

Both derive from `quay.io/hummingbird-community/bootc-os`, are built with
`podman build`, converted to `qcow2` via `bootc-image-builder`, and defined
under libvirt. Design notes and gotchas live in [`NOTES.md`](NOTES.md); this
README is the operator-facing entry point.

## Topology

A single KVM host runs one control plane + N workers:

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

## Prerequisites

Before running `make deploy-cluster`, the KVM host needs:

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
via `cluster.local.conf`.

| Flavor | RAM | vCPU | qcow2 disk |
| --- | --- | --- | --- |
| `hummingbird-k8s` (control plane) | 8 GB | 4 | 30 GB |
| `hummingbird-k8s-worker` (each) | 4 GB | 2 | 20 GB |

Sizing guidance:

- CP RAM is dominated by etcd + apiserver; do not drop below 4 GB.
- Each worker reserves ~500 MB for kubelet/cri-o + Cilium; 4 GB leaves
  ~3 GB for workloads.
- qcow2 files live in `POOL_DIR`. Point `POOL_DIR=/mnt/ssd/libvirt` at an SSD
  for noticeably better etcd write latency; HDDs work but `etcdctl defrag`
  cadence matters more.
- CP/worker memory and vCPU are tunable via `CP_MEMORY`, `CP_VCPUS`,
  `WORKER_MEMORY`, `WORKER_VCPUS` in `cluster.local.conf`.

## Quick start (operator)

**Registry-first is the golden path.** On a freshly-set-up KVM host
(libvirtd running, `qemu:///system` reachable, `podman` available), pull
the published images straight from GHCR — no local build needed:

```bash
make help                                                  # cheatsheet of all targets
cp cluster.example.conf cluster.local.conf                 # edit per host
make deploy-cluster       CONFIG=cluster.local.conf        # 1 CP + N workers, end-to-end
make verify-all                                            # encryption + hardening + app-deploy smoke test
```

The shipped `cluster.example.conf` is registry-first by default
(`IMAGE_SOURCE=ghcr`). `deploy-cluster.sh` also defaults to `ghcr` when
`IMAGE_SOURCE` is unset, so a minimal config that only defines `CP_NAME`,
`SSH_PUBKEY_FILE`, and `ENABLE_CLOUD_INIT=1` will pull from GHCR
out-of-the-box. Building locally instead of pulling? See
[Fast iteration](#fast-iteration-build-locally-instead-of-pulling-from-ghcr).

The five cluster-lifecycle targets — `deploy-cluster`,
`destroy-cluster`, `update-cluster`, `update-workers`, `update-node` —
no longer need `sudo` on the client (issue #233). The scripts handle
privilege escalation themselves: re-exec over SSH to `$KVM_HOST` when
set, or probe for local root on the KVM host. On-host operators can
still prefix `sudo make …` and it'll work; the recipes are plain
`bash scripts/…sh` invocations now. See
[`docs/makefile.md`](docs/makefile.md) for the full mechanism.

`make deploy-cluster` is the only supported way to stand up a cluster. It
drives the full `image -> qcow2 -> virt-install -> kubeadm join` flow from a
single config file (`cluster.local.conf`), with cloud-init carrying per-VM
dynamic state (hostname, worker join token, post-boot runcmd).

### Fast iteration: build locally instead of pulling from GHCR

Set `IMAGE_SOURCE=local` in `cluster.local.conf` when you're intentionally
testing an unpublished image (a Containerfile change, a local cri-o patch,
etc.). The deploy script will then drive the local
`image-k8s-with-cloud-init` + `image-worker-with-cloud-init` Makefile
targets before standing up the qcow2s. This needs `podman` AND
`bootc-image-builder` on the KVM host. The registry-first path needs no
`bootc-image-builder` (BIB); `podman` is still used (to pull the qcow2
layer from GHCR).

The Makefile is the operator entry point — every recipe delegates to a script
under [`scripts/`](scripts/). Run `make help` for the full target list. See
[`docs/makefile.md`](docs/makefile.md) for honored env vars and
[`docs/deploy-cluster.md`](docs/deploy-cluster.md) for the deploy flow's
full reference.

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
# verify-hardening tunnels SSH + kubectl through $KVM_HOST (#271 F4).
KVM_HOST=geary make verify-hardening CONFIG=cluster.local.conf
KVM_HOST=geary make verify-encryption
KVM_HOST=geary make verify-app-deploy   # PSA-restricted nginx smoke test
KVM_HOST=geary make verify-all          # all three in sequence

# kube-bench (CIS benchmark scan)
KVM_HOST=geary make kube-bench
```

#### Remote KVM-host operation for deploy/destroy/update/spawn (#232)

Since C3, the four libvirt-touching scripts also self-host on
`$KVM_HOST` via SSH — so the client never needs `sudo` or `libvirt`
locally, only `ssh` + the operator's existing SSH key. One-time
setup on the KVM host:

```bash
ssh $KVM_HOST 'git clone https://github.com/aatchison/hummingbird-k8s ~/hummingbird-k8s'
```

Then, from a client:

```bash
export KVM_HOST=geary
make deploy-cluster   CONFIG=cluster.local.conf  # ssh -t $KVM_HOST cd ~/hummingbird-k8s && sudo bash scripts/deploy-cluster.sh ...  (drop `sudo` via HBIRD_REMOTE_NO_SUDO=1 + libvirt-group operator; see #305)
make destroy-cluster  CONFIG=cluster.local.conf
make update-cluster   CONFIG=cluster.local.conf
make spawn-workers    COUNT=2
```

The shim execs scripts FROM DISK on the remote at `$HBIRD_REMOTE_REPO`
(default `~/hummingbird-k8s`); override the env var if you keep the
checkout elsewhere. The shim is a no-op when `KVM_HOST` is unset or
when `$(hostname -s)` already matches `${KVM_HOST%%.*}` (you're on the
KVM host already). Local `CONFIG=` files are `scp`'d to the remote
before re-exec, and every env value + arg is `printf %q` quoted
on the way out. Env-var forwarding is an explicit allowlist pinned
by `tests/scripts/ssh-wrap.bats` — see
[`docs/deploy-cluster.md`](docs/deploy-cluster.md#remote-kvm-host-operation-kvm_host)
for the full allowlist, `HBIRD_REMOTE_REPO` override, and pre-flight
diagnostics.

Your workstation's `SSH_PUBKEY_FILE` (the one referenced by your local
`CONFIG`) is also `scp`'d to the remote tempdir and baked into the CP
alongside the KVM-host's own key (#248). Result: after deploy, you can
SSH directly to the CP and workers from your workstation with your
normal SSH key — no need to copy keys around or hop through the KVM
host. No private key material travels; only the `.pub` file is
forwarded.

### Cluster lifecycle

```bash
# Deploy (hybrid bib + cloud-init, the only supported path)
cp cluster.example.conf cluster.local.conf            # edit it
make deploy-cluster       CONFIG=cluster.local.conf
make destroy-cluster      CONFIG=cluster.local.conf

# Tear down stragglers (any hummingbird-* libvirt domain on this host,
# plus stale hummingbird-*.qcow2 / *-seed.iso under POOL_DIR — see #221).
# Honors KVM_HOST: from a workstation with no local libvirt,
#   KVM_HOST=geary make clean-vms
# re-execs on geary via the C3 SSH-wrap shim (#271 F5). On the KVM host
# directly, `make clean-vms` self-elevates via sudo — no `sudo make` needed.
make clean-vms

# Point already-deployed VMs at GHCR so the semver-aware auto-update
# timer (docs/auto-updates.md) has a remote ref to pull from. Without
# this, VMs that booted from a local qcow2 still track localhost:latest
# and the timer can't resolve a remote tag list.
#
# Two paths: on the KVM host (legacy, sudo); or from a workstation with
# KVM_HOST set, in which case the script C3-wraps to the KVM host
# (no local sudo / libvirt needed). See docs/auto-updates.md.
sudo make switch-to-ghcr                  # on the KVM host
KVM_HOST=geary make switch-to-ghcr        # from a workstation (#271 F1)
```

Note: legacy + non-lifecycle targets above (`clean-vms`,
`backup-etcd`, `restore-etcd`, `rotate-etcd-key`) still need `sudo` —
only the five cluster-lifecycle targets (`deploy-cluster`,
`destroy-cluster`, `update-cluster`, `update-workers`, `update-node`)
dropped client-side `sudo` in PR #237. As of #271 F1+F5, `switch-to-ghcr` and `clean-vms` join them when `KVM_HOST` is set (sudo happens on the remote, not
locally).

Going one step further: `update-cluster`, `deploy-cluster`, and
`destroy-cluster` now accept a `libvirt`-group member on the KVM host
(no root needed), and the C3 shim drops `sudo` from the remote exec
when `HBIRD_REMOTE_NO_SUDO=1` is set (#269 for update, #305 for
deploy/destroy). One-time operator setup on the KVM host:

```bash
ssh $KVM_HOST 'sudo usermod -aG libvirt $USER && newgrp libvirt'
# AND, for deploy/destroy (which write qcow2s + seed ISOs to POOL_DIR):
ssh $KVM_HOST "sudo chgrp libvirt \$POOL_DIR && sudo chmod 2775 \$POOL_DIR"
```

Then from a workstation:

```bash
KVM_HOST=geary HBIRD_REMOTE_NO_SUDO=1 \
  make deploy-cluster CONFIG=cluster.local.conf
KVM_HOST=geary HBIRD_REMOTE_NO_SUDO=1 \
  make destroy-cluster CONFIG=cluster.local.conf
KVM_HOST=geary HBIRD_REMOTE_NO_SUDO=1 \
  make update-cluster CONFIG=cluster.local.conf
```

`spawn-workers` is the last lifecycle target that still requires root
on the KVM host — the remaining piece of #269's original scope. Full
reference:
[`docs/deploy-cluster.md#running-without-sudo`](docs/deploy-cluster.md#running-without-sudo-libvirt-group-operator-305).

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
make update-cluster       CONFIG=cluster.local.conf

# Skip the CP; only roll the workers (useful if CP is already on latest).
make update-workers       CONFIG=cluster.local.conf

# Touch exactly one node — useful for spot-fixing a stuck worker or for
# canarying a new image against a single node before rolling the rest.
make update-node          CONFIG=cluster.local.conf NODE=hbird-w1
```

Heads-up: single-CP means the apiserver is unavailable for ~60-120s while
the CP reboots. `scripts/update-cluster.sh` prints a warning before that
window. Add `--dry-run` (calling the script directly) to preview actions
without ssh/kubectl, or `--skip-drain` as an emergency-rollback escape
hatch for a stuck drain.

Readiness gates: between SSH-back and uncordon, the script also requires
the node's `.status.nodeInfo.bootID` to change (proves a real reboot
happened — defeats stale apiserver-cache `Ready=True` hits) **and** every
NEW kube-system DaemonSet pod on the node (Cilium / kube-proxy / coredns)
to report Ready before proceeding. Pre-existing CrashLoops are
snapshotted at gate entry and excluded so an unrelated chronic failure
doesn't halt the whole roll. Both gates are bounded by `READY_TIMEOUT` /
`DAEMONSET_TIMEOUT` (default-equal), with `--skip-gates` as an operator
escape hatch. Full reference:
[Reboot detection (bootID)](docs/update-cluster.md#reboot-detection-bootid)
and [Daemonset readiness gate](docs/update-cluster.md#daemonset-readiness-gate).

Operator-ergonomics flags — pass via `FLAGS=` to the Makefile targets, or
directly to the script: `--start-from=NAME` (resume after an interrupted
roll), `--parallel=N` (process workers in batches of N concurrently),
`--continue-on-error` (record per-node failures, summarize at the end),
`--no-delete-emptydir-data` (preserve emptyDir caches during drain),
`--skip-gates` (escape hatch for misfiring bootID/DS gates),
`--node-name-override DOMAIN=NODE` (pin the k8s node name for a libvirt
domain when the auto-resolution can't find it; see
[#260](docs/update-cluster.md#k8s-node-name-resolution-260)).
Per-step timeouts are tunable via `DRAIN_TIMEOUT`, `READY_TIMEOUT`,
`DAEMONSET_TIMEOUT`, `APISERVER_TIMEOUT`, `SSH_TIMEOUT`, and
`INTER_NODE_SLEEP` env vars.

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

# Tunnel the CP-fetch SSH session through the KVM host when your
# workstation can't reach the libvirt NAT directly. Either form works
# — KVM_HOST is the env-var path most operators already have set;
# PROXY_JUMP is the per-invocation override. KVM_HOST=… also routes
# CP_IP resolution through the KVM host's libvirt — workstation
# operators without local libvirt no longer need to pin `CP_IP=` in
# cluster.local.conf (issue #270).
make get-kubeconfig CONFIG=cluster.local.conf KVM_HOST=geary
make get-kubeconfig CONFIG=cluster.local.conf PROXY_JUMP=geary

# Override the apiserver URL / context name / output / overwrite-if-present:
make get-kubeconfig CONFIG=cluster.local.conf \
    SERVER=https://cluster.example.com:6443 \
    CONTEXT=prod-cp \
    OUTPUT=$HOME/.kube/hummingbird.yaml \
    FORCE=1
```

`FORCE=1` snapshots the existing file to `kubeconfig.yaml.bak-<UTC>`
(mode 0600) before writing. The backups are admin.conf-grade
credentials; `.gitignore` excludes `*.bak-*`, but on a workstation they
accumulate over time — periodically prune (`rm -i kubeconfig.yaml.bak-*`)
once you no longer need a rollback target.

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

# Tunnel the CP-fetch SSH session through the KVM host when your
# workstation can't reach the libvirt NAT directly:
KVM_HOST=geary make export-argocd CONFIG=cluster.local.conf

# Re-export (e.g. after `kubeadm certs renew all`) — refuses by default
# to overwrite an existing file. `--force` snapshots the prior file as
# `argocd-kubeconfig.yaml.bak-<UTC>` before writing the new one:
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

Cluster topology + behavior knobs live in `cluster.local.conf` (gitignored).
Copy [`cluster.example.conf`](cluster.example.conf) to start. `deploy-cluster`
and `update-cluster` source the same file.

Image-build inputs (default user, SSH keys, sudo password) are tuned via the
optional per-host `config.local.sh`; [`config.example.sh`](config.example.sh)
is the template. The build scripts (`scripts/build-*.sh`) source it when
`HBIRD_AUTOLOAD_CONFIG_LOCAL=1` is set.

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
sudo bootc switch ghcr.io/aatchison/hummingbird-k8s:v0.1.0
sudo systemctl reboot
```

Substitute `hummingbird-k8s-worker` for the worker flavor. Images are
signed with cosign (keyless OIDC) at publish time. Verify before switching;
see [`docs/image-verification.md`](docs/image-verification.md) for the
`cosign verify` command and the expected issuer/identity.

## Release process

GitHub Actions builds and publishes one flavor per tag:

| Tag pattern | Workflow | Published image |
| --- | --- | --- |
| `k8s/vX.Y.Z` | `build-k8s.yml` | `ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z` |
| `worker/vX.Y.Z` | `build-worker.yml` | `ghcr.io/aatchison/hummingbird-k8s-worker:vX.Y.Z` |

Flavors version independently:

```bash
git tag k8s/v0.2.0
git push --tags
```

The publish job refuses to run if the tagged commit isn't reachable from
`main`, so accidental tags on a topic branch will not produce a release.

### Publishing images locally (workstation path)

The Makefile also exposes `make push-image-*` for operators who want to
cut an ad-hoc tag from a workstation without going through a tag push
(handy for pre-release validation, or when iterating against a hosted
KVM host that pulls from GHCR rather than building locally). The build
itself runs **rootless** as the invoking user.

```bash
gh auth login                                                          # one-time, if not already
gh auth token | podman login ghcr.io -u <github-user> --password-stdin # GH_TOKEN with write:packages

make image-k8s                                                         # smoke-build (no push)
make push-image-k8s    IMAGE_TAG=v0.1.x                                # tag + push CP image
make push-image-worker IMAGE_TAG=v0.1.x                                # tag + push worker image
make push-image-all    IMAGE_TAG=v0.1.x                                # both
```

The `--password-stdin` form keeps the GH_TOKEN out of shell history /
`ps aux` snapshots — recommended over the interactive `podman login
ghcr.io` password prompt. Each `make push-image-*` runs a
`podman login --get-login` preflight against the registry host so a
missed login step fails with a clear "ERROR: not logged in to ghcr.io"
diagnostic naming the exact command to run, rather than a downstream
`podman push` "unauthorized" surprise.

`IMAGE_TAG` defaults to `latest`; override per release. `GHCR_REGISTRY`
defaults to `ghcr.io/aatchison` — override for forks/mirrors. The tagged
release workflow above is still the canonical signed-+-SBOM path; the
`push-image-*` targets are the unsigned local equivalent for fast
iteration. See [`docs/makefile.md`](docs/makefile.md) for the full
variable surface (including the storage-isolation knobs and how to
keep `PODMAN_ROOT` consistent across the build+push pair).

## Operations

Day-2 documentation lives under [`docs/`](docs):

- [`docs/deploy-cluster.md`](docs/deploy-cluster.md) — `make deploy-cluster` hybrid bib + cloud-init orchestrator for deploying 1 CP + N workers from a single config file.
- [`docs/makefile.md`](docs/makefile.md) — `make` cheatsheet over the underlying driver scripts.
- [`docs/image-verification.md`](docs/image-verification.md) — verify GHCR images with cosign.
- [`docs/etcd-encryption.md`](docs/etcd-encryption.md) — enable encryption-at-rest for etcd.
- [`docs/worker-tokens.md`](docs/worker-tokens.md) — short-TTL, per-VM kubeadm join tokens.
- [`docs/self-hosted-runner.md`](docs/self-hosted-runner.md) — register a KVM-capable GitHub Actions runner.
- [`docs/auto-updates.md`](docs/auto-updates.md) — semver-aware bootc auto-update timer (advances only on new immutable `vMAJOR.MINOR.PATCH` tags, replacing the upstream `:latest`-tracking unit).
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

Workflows that need real KVM (orchestrator integration, bootc upgrade e2e)
run on a self-hosted runner on the operator's KVM host.

## Repo layout

```
containers/<flavor>/   per-flavor Containerfile + first-boot scripts
                       (k8s control plane, k8s-worker). Shared in-image
                       assets under containers/shared/.
scripts/               every driver script (build, deploy-cluster,
                       update-cluster, destroy-cluster, kubectl wrapper,
                       verifiers, kube-bench).
lib/                   build-common.sh — sourced by scripts/build-*.sh;
                       shared SSH/log helpers documented in docs/development.md.
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
- **Worker stays `NotReady` after `make deploy-cluster`.** The Cilium
  daemonset hasn't scheduled there yet; wait ~60s after join, then re-check
  `kubectl get nodes`.
- **`bootc-image-builder` qcow2 build fails inside a container.** Usually
  the podman storage driver — overlay-on-overlay can't run bib. Export
  `STORAGE_DRIVER=vfs` before `make deploy-cluster` (see #124).
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
