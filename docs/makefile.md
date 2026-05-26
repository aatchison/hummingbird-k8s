# Makefile cheatsheet

The top-level `Makefile` is a thin wrapper over the driver scripts under
`scripts/`. Both the `make` targets and the underlying scripts still work —
the Makefile just gives operators a stable, discoverable entrypoint. Run
`make help` to print the full cheatsheet, or browse `scripts/` directly.

## Common flows

Fresh cluster deploy on a clean KVM host (1 CP + N workers, the only
supported path since #216):

```bash
cp cluster.example.conf cluster.local.conf
$EDITOR cluster.local.conf                    # set CP_NAME, WORKER_NAMES, IMAGE_SOURCE, ...
sudo make deploy-cluster CONFIG=cluster.local.conf
make verify-all                                # verify-encryption + verify-hardening + verify-app-deploy
```

Stand-alone image builds (no qcow2, no VM — fast iteration on Containerfile
changes, mirrors what `pr-validate.yml` does). These run **rootless** as
the invoking user; no `sudo` is needed because the recipes only call
`podman build` (qcow2 + libvirt steps that DO need root live behind
`make deploy-cluster`):

```bash
make image-k8s            # control plane OCI image (rootless)
make image-worker         # worker template OCI image (rootless)
make image-all            # both
```

Publish locally-built images to GHCR (companion to the tag-driven GHA
workflow under `.github/workflows/build-*.yml`; useful for cutting an
ad-hoc tag from a workstation without going through a tag push):

```bash
gh auth login                                       # if not already
podman login ghcr.io                                # GH_TOKEN with write:packages
make push-image-k8s    IMAGE_TAG=v0.1.x             # tag + push CP image
make push-image-worker IMAGE_TAG=v0.1.x             # tag + push worker image
make push-image-all    IMAGE_TAG=v0.1.x             # both
```

`IMAGE_TAG` defaults to `latest`; override per release. `GHCR_REGISTRY`
defaults to `ghcr.io/aatchison` — override for forks/mirrors.

Ad-hoc `kubectl` from the client (needs `KVM_HOST` set, see `config.example.sh`):

```bash
make nodes
make kubectl ARGS='get pods -A'
```

Tear it all down:

```bash
sudo make clean               # destroys + undefines VMs and removes local images
```

Rolling update of an existing deploy-cluster cluster (CP first, no
drain; each worker drained → `bootc upgrade --apply` → uncordoned):

```bash
# Whole cluster:
sudo make update-cluster  CONFIG=cluster.local.conf

# Workers only (skip the CP — useful if CP already on latest):
sudo make update-workers  CONFIG=cluster.local.conf

# One specific node (CP_NAME or a WORKER_NAMES entry):
sudo make update-node     CONFIG=cluster.local.conf NODE=hbird-w1
```

Full flag reference, lock-file behavior, recovery procedure, and time
estimates per cluster size: [`update-cluster.md`](update-cluster.md).

Unit tests for `lib/build-common.sh` (no sudo, runs in a pinned bats
container — same job CI runs as `unit-tests-lib`):

```bash
make test-lib
```

See [`integration-tests.md`](integration-tests.md#unit-tests) for the
test surface.

## Variables

| Variable        | Default                       | Used by                       |
| ---             | ---                           | ---                           |
| `CONFIG`        | (required)                    | `deploy-cluster`, `destroy-cluster`, `update-cluster`, `update-workers`, `update-node`, `export-argocd`, `get-kubeconfig` |
| `NODE`          | (required for `update-node`)  | `update-node`                 |
| `FLAGS`         | empty                         | `update-cluster`, `update-workers`, `update-node` (pass-through to scripts/update-cluster.sh) |
| `ARGS`          | empty                         | `kubectl`                     |
| `POOL_DIR`      | `/var/lib/libvirt/images`     | `clean-vms`                   |
| `KVM_HOST`      | unset                         | `kubectl` / `nodes`           |
| `IMAGE_TAG`     | `latest`                      | `push-image-k8s`, `push-image-worker`, `push-image-all` |
| `GHCR_REGISTRY` | `ghcr.io/aatchison`           | `push-image-*` (override for forks/mirrors) |

Any other env vars honored by `cluster.local.conf` / `config.local.sh`
(e.g. `VM_USER`, `APISERVER_EXTRA_SANS`, `STORAGE_DRIVER`, `PODMAN_ROOT`,
`PODMAN_RUNROOT`) flow through to the underlying scripts unchanged.

## Version pinning

Pinned versions live in two different places depending on whether the
artifact is baked into a container image or fetched by a runtime
script. This is intentional but easy to trip over, so it's worth
calling out explicitly.

| Variable             | Kind            | Defined in                              | Default    |
| ---                  | ---             | ---                                     | ---        |
| `K8S_VERSION`        | Containerfile `ARG` (build-arg) | `containers/k8s/Containerfile`, `containers/k8s-worker/Containerfile` | `v1.31`    |
| `CILIUM_CLI_VERSION` | Containerfile `ARG` (build-arg) | `containers/k8s/Containerfile`          | `v0.16.16` |
| `KUBE_BENCH_VERSION` | shell env var   | `scripts/run-kube-bench.sh`             | `v0.15.5`  |

Build-args (`K8S_VERSION`, `CILIUM_CLI_VERSION`) are consumed at image
build time and have to be passed via `podman build --build-arg
NAME=VALUE` (or set in the build wrapper) — exporting them in the
shell does nothing on its own. The Cilium agent/operator version
deployed at first boot is pinned separately inside
`containers/k8s/k8s-init.sh` (`cilium install --version ...`) and is
deliberately not exposed as a build-arg; see
[`cilium-migration.md`](cilium-migration.md#version-pinning-is-explicit).

The env-var pattern (`KUBE_BENCH_VERSION`) is used by scripts that run
against a live cluster *after* the image is built — they have no
container layer to bake into, so they read the version from the
caller's environment. Export them in the shell before running the
script:

```bash
KUBE_BENCH_VERSION=v0.15.5 ./scripts/run-kube-bench.sh
```

See [`kube-bench.md`](kube-bench.md) for the full env-var surface of
that script.
