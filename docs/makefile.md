# Makefile cheatsheet

The top-level `Makefile` is a thin wrapper over the driver scripts under
`scripts/`. Both the `make` targets and the underlying scripts still work —
the Makefile just gives operators a stable, discoverable entrypoint. Run
`make help` to print the full cheatsheet, or browse `scripts/` directly.

## Common flows

Fresh cluster deploy on a clean KVM host (1 CP + N workers, the only
supported path):

```bash
cp cluster.example.conf cluster.local.conf
$EDITOR cluster.local.conf                    # set CP_NAME, WORKER_NAMES, IMAGE_SOURCE, ...
make deploy-cluster CONFIG=cluster.local.conf
make verify-all                                # verify-encryption + verify-hardening + verify-app-deploy
```

`make deploy-cluster` no longer requires `sudo` (issue #233). The
underlying script re-execs over SSH to `$KVM_HOST` when set, or probes
for local root and prints a one-line hint if neither path is available.
The same applies to `destroy-cluster`, `update-cluster`,
`update-workers`, and `update-node`. See
[`deploy-cluster.md`](deploy-cluster.md) for the three no-op paths.

> **Local fallback path:** if you're running on the KVM host with
> custom podman storage (`STORAGE_DRIVER`, `PODMAN_ROOT`,
> `PODMAN_RUNROOT`), pass them through the outer `sudo` explicitly:
> `sudo --preserve-env=STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT,HOME make deploy-cluster CONFIG=…`.
> Before #233 the Makefile carried these for you via `sudo
> --preserve-env=…`; with `sudo` removed from the recipe it's now the
> operator's job to keep them across the privilege boundary. Same
> applies to `destroy-cluster` / `update-cluster` /
> `update-workers` / `update-node` when running locally on the KVM
> host with a non-default podman storage location.

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
gh auth login                                                          # if not already
gh auth token | podman login ghcr.io -u <github-user> --password-stdin # GH_TOKEN with write:packages
make push-image-k8s    IMAGE_TAG=v0.1.x                                # tag + push CP image
make push-image-worker IMAGE_TAG=v0.1.x                                # tag + push worker image
make push-image-all    IMAGE_TAG=v0.1.x                                # both
```

The `--password-stdin` form keeps your PAT out of shell history /
`ps aux` snapshots. The older interactive `podman login ghcr.io`
prompt still works if you'd rather paste, but `--password-stdin` is
the documented default. Each `make push-image-*` runs a
`podman login --get-login` preflight against the registry host —
if you skipped the login step, you get a single-line "ERROR: not
logged in to ghcr.io" with the exact command to fix it, rather than
a raw `podman push` "unauthorized" from the registry.

`IMAGE_TAG` defaults to `latest`; override per release. `GHCR_REGISTRY`
defaults to `ghcr.io/aatchison` — override for forks/mirrors.

Operator-supplied values for `STORAGE_DRIVER`, `PODMAN_ROOT`,
`PODMAN_RUNROOT`, `IMAGE_TAG`, and `GHCR_REGISTRY` are validated
against a conservative character allowlist at Makefile parse time
(see [Variables → validated character set](#variables)). A value
containing characters outside the allowlist aborts `make` before any
recipe runs — so a stray space or shell metachar fails fast instead
of being spliced into a `podman` argv.

### Rebuild-on-push behavior

`push-image-{k8s,worker,all}` depends on the matching `image-*`
target, so each push **always rebuilds** the local OCI image first.
This is intentional — it keeps every `IMAGE_TAG=…` cut reproducible
from a single command and matches the GHA workflow's
build-then-push contract. The cost is a podman cache hit (seconds)
on a no-op rebuild; the benefit is that `make push-image-k8s
IMAGE_TAG=v0.1.5` can never push a stale layer from a previous
local build. If you specifically want to retag-without-rebuild
(e.g. cutting a new tag from the exact bits you just pushed),
either invoke `podman tag … && podman push …` directly, or use
`make -t push-image-k8s` to treat prereqs as up-to-date.

### Sharing PODMAN_ROOT / PODMAN_RUNROOT between build and push

If you set `STORAGE_DRIVER` / `PODMAN_ROOT` / `PODMAN_RUNROOT` for
storage isolation (issue #199), set them for the **whole `make`
invocation**, not just the `push-image-*` half. `podman tag` and
`podman push` only see the image the build placed in the alternate
graphroot if they are invoked with the same `--root` / `--runroot`.
Two common shapes:

```bash
# One `make` call: build + tag + push share the env.
PODMAN_ROOT=/tmp/r PODMAN_RUNROOT=/tmp/rr \
  make push-image-k8s IMAGE_TAG=v0.1.x

# Two `make` calls: export the env so BOTH inherit it.
export STORAGE_DRIVER=overlay
export PODMAN_ROOT=/tmp/r
export PODMAN_RUNROOT=/tmp/rr
make image-k8s
make push-image-k8s IMAGE_TAG=v0.1.x
```

Splitting the env between a `make image-*` and a follow-up
`make push-image-*` shell silently lets the second `make` invocation
use the default graphroot — the tag step then fails image-not-found
because the image lives in `$PODMAN_ROOT` instead.

Ad-hoc `kubectl` from the client (needs `KVM_HOST` set, see `config.example.sh`):

```bash
make nodes
make kubectl ARGS='get pods -A'
```

Tear it all down:

```bash
make clean                    # destroys + undefines VMs and removes local images
KVM_HOST=geary make clean-vms # workstation -> KVM host via C3 SSH-wrap (#271 F5)
```

`make clean-vms` delegates to `scripts/clean-vms.sh`, which:

1. Sources `scripts/lib/ssh-wrap.sh` and re-execs on `$KVM_HOST` when
   set (workstation case — no local libvirt required).
2. Destroys + undefines every `hummingbird-*` libvirt domain on the
   target host.
3. Sweeps stale `hummingbird-*.qcow2`, `*-seed.iso`, and
   `*-cloud-init.iso` files under `POOL_DIR` (closes #221).
4. Runs `virsh pool-refresh "$POOL_NAME"` (default pool name
   `default`) so libvirt drops the rm'd files from the volumes
   catalog.

The script self-elevates via `sudo` when run directly on the KVM host
(operator types `make clean-vms`, NOT `sudo make clean-vms`). Override
`POOL_DIR` or `POOL_NAME` from the environment for non-standard
libvirt setups.

Rolling update of an existing deploy-cluster cluster (CP first, no
drain; each worker drained → `bootc upgrade --apply` → uncordoned):

```bash
# Whole cluster:
make update-cluster       CONFIG=cluster.local.conf

# Workers only (skip the CP — useful if CP already on latest):
make update-workers       CONFIG=cluster.local.conf

# One specific node (CP_NAME or a WORKER_NAMES entry):
make update-node          CONFIG=cluster.local.conf NODE=hbird-w1
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

| Variable            | Default                       | Used by                       |
| ---                 | ---                           | ---                           |
| `CONFIG`            | (required)                    | `deploy-cluster`, `destroy-cluster`, `update-cluster`, `update-workers`, `update-node`, `export-argocd`, `get-kubeconfig` |
| `NODE`              | (required for `update-node`)  | `update-node`                 |
| `FLAGS`             | empty                         | `update-cluster`, `update-workers`, `update-node` (pass-through to scripts/update-cluster.sh) |
| `ARGS`              | empty                         | `kubectl`                     |
| `POOL_DIR`          | `/var/lib/libvirt/images`     | `clean-vms` (sweep target dir) |
| `POOL_NAME`         | `default`                     | `clean-vms` (libvirt pool name for `pool-refresh`) |
| `KVM_HOST`          | unset                         | `kubectl` / `nodes` / `clean-vms` (#271 F5) — and since C3 (#232) also `deploy-cluster`, `destroy-cluster`, `update-cluster`, `spawn-workers` (re-exec on the KVM host via SSH; client needs only `ssh`, no local `sudo`). Since #271 F1 also `switch-to-ghcr`. See [`docs/deploy-cluster.md`](deploy-cluster.md#remote-kvm-host-operation-kvm_host). |
| `HBIRD_REMOTE_REPO` | `~/hummingbird-k8s`           | Path on `$KVM_HOST` to the remote git checkout of hummingbird-k8s. Used by the C3 SSH-wrap shim — when the shim fires it `cd`s here and execs `bash scripts/<name>.sh` from disk. Override when the checkout lives somewhere other than `$HOME/hummingbird-k8s`. See [`docs/deploy-cluster.md`](deploy-cluster.md#hbird_remote_repo-override). |
| `HBIRD_REMOTE_NO_SUDO` | unset                       | When `=1`, the C3 SSH-wrap shim emits the remote command WITHOUT the `sudo` prefix. Use with `update-cluster` when the operator is in the `libvirt` group on `$KVM_HOST` (the script doesn't otherwise need root). Default keeps `sudo` for `deploy-cluster` / `destroy-cluster` / `spawn-workers`, which still write to root-owned `POOL_DIR`. See [`docs/update-cluster.md`](update-cluster.md#running-without-sudo-libvirt-group-operator-269). (#269) |
| `IMAGE_TAG`         | `latest`                      | `push-image-k8s`, `push-image-worker`, `push-image-all` |
| `GHCR_REGISTRY`     | `ghcr.io/aatchison`           | `push-image-*` (override for forks/mirrors) |

Any other env vars honored by `cluster.local.conf` / `config.local.sh`
(e.g. `VM_USER`, `APISERVER_EXTRA_SANS`, `STORAGE_DRIVER`, `PODMAN_ROOT`,
`PODMAN_RUNROOT`) flow through to the underlying scripts unchanged.
These knobs are also on the SSH-wrap forwarding allowlist (since #232
round-2) so they reach the remote when `KVM_HOST` triggers the
re-exec.

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
