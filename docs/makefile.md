# Makefile cheatsheet

The top-level `Makefile` is a thin wrapper over the driver scripts under
`scripts/`. Both the `make` targets and the underlying scripts still work —
the Makefile just gives operators a stable, discoverable entrypoint. Run
`make help` to print the full cheatsheet, or browse `scripts/` directly.

## Common flows

Full fresh upstream-k8s deploy on a clean KVM host:

```bash
sudo make k8s                 # build + define + start the control plane
sudo make workers COUNT=2     # build worker image and spawn 2 workers
make verify-all               # verify-encryption + verify-hardening + verify-app-deploy
```

k3s single-node:

```bash
sudo make k3s
```

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

| Variable   | Default                       | Used by                       |
| ---        | ---                           | ---                           |
| `COUNT`    | `2`                           | `workers`, `spawn`            |
| `ARGS`     | empty                         | `kubectl`                     |
| `POOL_DIR` | `/var/lib/libvirt/images`     | `clean-vms`                   |
| `KVM_HOST` | unset                         | `kubectl` / `nodes`           |

Any other env vars honored by `config.local.sh` (e.g. `VM_USER`,
`APISERVER_EXTRA_SANS`) flow through to the underlying scripts unchanged.

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
