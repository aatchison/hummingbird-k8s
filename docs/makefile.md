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
(see [Variables → validated character set](#validated-character-set)).
A value containing characters outside the allowlist aborts `make`
before any recipe runs — so a stray space or shell metachar fails
fast instead of being spliced into a `podman` argv.

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

`make clean-vms` delegates to `scripts/clean-vms.sh` (since #271 F5 /
#292 — previously the recipe inlined `virsh ... | grep '^hummingbird-'`
with no SSH-wrap, so workstation operators saw `virsh: command not
found`). The script:

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
libvirt setups (both are on the C3 SSH-wrap forwarding allowlist so
they reach the remote when `KVM_HOST` triggers the re-exec).

`make switch-to-ghcr` (since #271 F1 / #277) follows the same pattern:
the script sources the C3 SSH-wrap and re-execs on `$KVM_HOST` when
set, so workstation operators no longer hit `virsh: command not found`
when pivoting an existing cluster to track GHCR. `GHCR_ORG`,
`GHCR_TAG`, and `BOOTC_SWITCH_TO_GHCR` flow through the SSH-wrap
allowlist to the remote.

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

This table is the authoritative reference for every variable the
Makefile honors directly (recipe-supplied via `make VAR=val` or
operator env). `tests/makefile.bats` includes a drift fence that
fails CI if a `?=` default in `Makefile` lacks a row here, so the
table cannot silently lag the recipe surface.

### Required / per-target arguments

These are positional from the operator's perspective — pass them on
the `make` command line for the targets that use them.

| Variable            | Default                       | Used by                       |
| ---                 | ---                           | ---                           |
| `CONFIG`            | (required)                    | `deploy-cluster`, `destroy-cluster`, `update-cluster`, `update-workers`, `update-node`, `export-argocd`, `get-kubeconfig`, `nodes`, `kubectl` (CONFIG is optional for `nodes`/`kubectl`; when set, the script reads CP_NAME/KVM_HOST from it) |
| `NODE`              | (required for `update-node`)  | `update-node`                 |
| `FLAGS`             | empty                         | `update-cluster`, `update-workers`, `update-node` (pass-through to `scripts/update-cluster.sh`: `--dry-run`, `--parallel=N`, `--start-from=NAME`, `--continue-on-error`, `--no-delete-emptydir-data`, `--skip-drain`) |
| `ARGS`              | empty                         | `kubectl` (pass-through, e.g. `ARGS='get pods -A'`) |
| `LABEL`             | empty                         | `backup-etcd` (optional `--label <text>` suffix on the snapshot filename) |
| `SNAP`              | (required for `restore-etcd`) | `restore-etcd` (path to the `.db` snapshot to restore) |
| `OUTPUT`            | `kubeconfig.yaml` (`get-kubeconfig`); `argocd-kubeconfig.yaml` (`export-argocd`) | `export-argocd`, `get-kubeconfig` (output kubeconfig path) |
| `SERVER`            | unset (derive from CP)        | `export-argocd`, `get-kubeconfig` (override the apiserver URL written into kubeconfig — e.g. for a load balancer / DNS name) |
| `CONTEXT`           | `hummingbird-$CP_NAME` (`export-argocd`); `$CP_NAME` (`get-kubeconfig`) | `export-argocd`, `get-kubeconfig` (override the kubeconfig context name) |
| `FORCE`             | unset                         | `export-argocd`, `get-kubeconfig` (when `=1`, overwrite an existing `$OUTPUT` instead of erroring) |
| `PROXY_JUMP`        | `$KVM_HOST` when set (`export-argocd`/`get-kubeconfig`); else empty | `export-argocd`, `get-kubeconfig` (SSH `ProxyJump` host inserted into the kubeconfig so kubectl reaches an internal CP through the KVM host) |
| `CILIUM`            | unset (defaults to in-repo pin) | `check-cilium-k8s-compat` (override the Cilium version, e.g. `CILIUM=1.17.0`, for what-if checks against the embedded matrix; passes to the script as `--cilium=X.Y.Z`). |
| `K8S`               | unset (defaults to in-repo pin) | `check-cilium-k8s-compat` (override the K8s minor, e.g. `K8S=v1.32` or `K8S=1.32`, for what-if checks; passes to the script as `--k8s=vX.Y`). |
| `STRICT`            | unset                         | `check-cilium-k8s-compat` (when `=1`, escalates a Cilium/K8s mismatch from a stderr WARN+exit-0 to exit-1 — wire this form into pre-merge gates that should hard-fail on incompatibility). |

### Honored env vars (recipe-supplied or exported)

These have Makefile defaults and are validated at parse time (see
[validated character set](#validated-character-set)).

| Variable            | Default                       | Used by                       |
| ---                 | ---                           | ---                           |
| `POOL_DIR`          | `/var/lib/libvirt/images`     | `clean-vms` (sweep target dir). Not a `?=` Makefile var — consumed inside `scripts/clean-vms.sh`; on the C3 SSH-wrap forwarding allowlist so it reaches the remote when `KVM_HOST` is set. |
| `POOL_NAME`         | `default`                     | `clean-vms` (libvirt pool name passed to `virsh pool-refresh`). Same shape as `POOL_DIR` — script-level default, on the forwarding allowlist (#292). |
| `KVM_HOST`          | unset                         | `kubectl`, `nodes`, `clean-vms` (#271 F5 / #292), `switch-to-ghcr` (#271 F1 / #277), `verify-encryption` / `verify-hardening` / `verify-app-deploy` / `verify-all` (#333 — Makefile forwards `CONFIG=` + `KVM_HOST=` to every verify-* script so `make verify-XXX CONFIG=cluster.local.conf KVM_HOST=geary` reaches the script env), `verify-app-deploy` (#271 F3 / #296 — auto-defaults `KUBECTL` to the in-repo tunneled wrapper so workstation operators don't need a local kubectl context), `verify-hardening` (#271 F4 / #297 — `KUBECTL` wrapper + `resolve_cp_ip` for the CP), `export-argocd` and `get-kubeconfig` (#270 — `resolve_cp_ip` resolves the CP IP over SSH to `$KVM_HOST` when local libvirt is absent), and since C3 (#232) also `deploy-cluster`, `destroy-cluster`, `update-cluster`, `update-workers`, `update-node`, `spawn-workers` (re-exec on the KVM host via SSH; client needs only `ssh`, no local `sudo` or libvirt). See [`docs/deploy-cluster.md`](deploy-cluster.md#remote-kvm-host-operation-kvm_host). |
| `HBIRD_REMOTE_REPO` | `~/hummingbird-k8s`           | Path on `$KVM_HOST` to the remote git checkout of hummingbird-k8s. Used by the C3 SSH-wrap shim — when the shim fires it `cd`s here and execs `bash scripts/<name>.sh` from disk. Override when the checkout lives somewhere other than `$HOME/hummingbird-k8s`. See [`docs/deploy-cluster.md`](deploy-cluster.md#hbird_remote_repo-override). |
| `HBIRD_REMOTE_NO_SUDO` | unset                      | When `=1`, the C3 SSH-wrap shim emits the remote command WITHOUT the `sudo` prefix. Use with `update-cluster` / `update-workers` / `update-node` / `deploy-cluster` / `destroy-cluster` when the operator is in the `libvirt` group on `$KVM_HOST` AND `POOL_DIR` is group-writable on that host (`sudo chgrp libvirt $POOL_DIR && sudo chmod 2775 $POOL_DIR`, one-time). Default keeps `sudo` for `spawn-workers` / `clean-vms`, which still write to root-owned paths. See [`docs/deploy-cluster.md`](deploy-cluster.md#running-without-sudo-libvirt-group-operator-305). (#269 / #272 for update; #305 for deploy/destroy) |
| `IMAGE_TAG`         | `latest`                      | `push-image-k8s`, `push-image-worker`, `push-image-all` (final `:tag` written to GHCR; override per release, e.g. `IMAGE_TAG=v0.1.5`). Validated character set: `A-Z a-z 0-9 . _ : / + -`. |
| `GHCR_REGISTRY`     | `ghcr.io/aatchison`           | `push-image-*` (override for forks/mirrors — e.g. `GHCR_REGISTRY=quay.io/me`). The Makefile also extracts the bare host (e.g. `ghcr.io`) for the `podman login --get-login` preflight. Validated character set: `A-Z a-z 0-9 . / : _ -`. |
| `STORAGE_DRIVER`    | unset                         | `image-*`, `push-image-*` (threaded into `podman --storage-driver`). Rootless-podman storage isolation knob (issue #199 / #230). Validated character set: `A-Z a-z 0-9 . _ / -`. |
| `PODMAN_ROOT`       | unset                         | `image-*`, `push-image-*` (threaded into `podman --root`). Same validation as `STORAGE_DRIVER`. Must be set for the WHOLE `make` invocation, not split across build and push — see [Sharing PODMAN_ROOT / PODMAN_RUNROOT between build and push](#sharing-podman_root--podman_runroot-between-build-and-push). |
| `PODMAN_RUNROOT`    | unset                         | `image-*`, `push-image-*` (threaded into `podman --runroot`). Same shape as `PODMAN_ROOT`. |
| `GHCR_ORG`          | unset (defaults inside script) | `switch-to-ghcr` (GHCR namespace for the bootc image refs the running VMs are pivoted to; consumed by `scripts/switch-to-ghcr.sh`, on the SSH-wrap allowlist since #277). |
| `GHCR_TAG`          | unset (defaults inside script) | `switch-to-ghcr` (tag on the GHCR refs the running VMs pivot to). |
| `BOOTC_SWITCH_TO_GHCR` | unset                      | `switch-to-ghcr` (advisory env switch consumed by `scripts/switch-to-ghcr.sh`; on the SSH-wrap allowlist since #277). |

#### Validated character set

Operator-supplied values for `STORAGE_DRIVER`, `PODMAN_ROOT`,
`PODMAN_RUNROOT`, `IMAGE_TAG`, and `GHCR_REGISTRY` are checked against
a conservative allowlist at Makefile parse time (see #234). A value
containing characters outside the allowlist aborts `make` with a
single-line `$(error)` BEFORE any recipe runs — a stray space or
shell metachar fails fast instead of being spliced into a `podman`
argv. The exact allowlist per variable is in the row above; the
fence applies to every recipe that takes these knobs (build, tag,
push).

### Other env vars (flow-through)

Any other env vars honored by `cluster.local.conf` / `config.local.sh`
(e.g. `VM_USER`, `APISERVER_EXTRA_SANS`, `CP_NAME`, `WORKER_NAMES`,
`READY_TIMEOUT`, `DRAIN_TIMEOUT`, `APISERVER_TIMEOUT`, `SSH_TIMEOUT`,
`AUTO_UPDATE_CP`, `SWITCH_TO_GHCR`, `BOOTC_UPDATE_SCHEDULE`,
`HBIRD_OPERATOR_PUBKEY_FILE`) flow through to the underlying scripts
unchanged. These knobs are also on the SSH-wrap forwarding allowlist
(see `HBIRD_SSH_WRAP_ALLOWED_ENV` in `scripts/lib/ssh-wrap.sh`) so
they reach the remote when `KVM_HOST` triggers the re-exec.

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

### Cilium ↔ K8s compatibility pre-check

Cilium's per-minor support window for Kubernetes is independent of the
two pins above (`K8S_VERSION` in the Containerfile and the
`cilium install --version` line in `k8s-init.sh`). To guard against
bumping `K8S_VERSION` past the pinned Cilium minor's tested window
(issue #303), run:

```bash
make check-cilium-k8s-compat                  # warn-only on currently-committed pins
make check-cilium-k8s-compat K8S=v1.32        # "what if I bump K8s to v1.32?"
make check-cilium-k8s-compat CILIUM=1.17.0    # "what if I bump Cilium to 1.17.0?"
make check-cilium-k8s-compat STRICT=1         # exit 1 on mismatch (pre-merge gate form)
```

The embedded matrix mirrors the per-minor pages at
<https://docs.cilium.io/en/v1.16/network/kubernetes/compatibility/>
(swap the `v1.16` segment to consult another release). Refresh it in
`scripts/check-cilium-k8s-compat.sh` when bumping the Cilium pin past
the highest known minor, **or when upstream adds K8s minors to an
existing Cilium row** (check the per-minor page even between Cilium
bumps — Cilium occasionally back-fills K8s support in patch releases);
`tests/scripts/check-cilium-k8s-compat.bats` pins specific cells so an
accidental row flip is loud.

See also the [`k8s-version-upgrade.md` pre-flight
checklist](k8s-version-upgrade.md#pre-flight-checklist) — this target
codifies the "check the Cilium compatibility matrix" bullet there, so
the operator running the upgrade gets the same answer the docs prescribe
without having to read the upstream matrix by hand.
