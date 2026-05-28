# Rust CLI migration guide — `make` → `hbird`

Operator-facing companion to [`docs/rust-cli.md`](rust-cli.md) (per-phase
status table) and [`rust/README.md`](../rust/README.md) (workspace
layout). This doc is the lookup index for "I know the `make` target —
what's the equivalent `hbird` invocation?".

> **Status (v0.1.0 partial cutover, [#353]):** `make` recipes for
> update-cluster / verify-* / export-argocd / get-kubeconfig / nodes /
> kubectl now delegate to `hbird` directly (the underlying bash scripts
> were **removed**). Phase 4 (deploy-cluster / destroy-cluster /
> spawn-workers) bash scripts are **retained** in v0.1.0 — full removal
> blocked on [#289] (Rust destructive impl), scheduled for v0.2.0.
>
> **Cross-runtime dependency:** the post-cutover Makefile recipes
> require `hbird` CLI on PATH. See "Install the binary" below.

## TL;DR — side-by-side

| Workflow | `make` (delegates to hbird in v0.1.0) | `hbird` (Rust, canonical) | Status |
|----------|----------------------------------------|----------------------------|--------|
| Deploy cluster | `make deploy-cluster CONFIG=cluster.local.conf` | `hbird deploy-cluster --config cluster.local.conf` (dry-run only; live tracked by [#289]) | **Bash retained** in v0.1.0 (Phase 4 deferred to v0.2.0) |
| Tear down cluster | `make destroy-cluster CONFIG=cluster.local.conf` | `hbird destroy-cluster --config cluster.local.conf` (dry-run only; live tracked by [#289]) | **Bash retained** in v0.1.0 (Phase 4 deferred to v0.2.0) |
| Rolling upgrade | `make update-cluster CONFIG=cluster.local.conf` | `hbird update-cluster --config cluster.local.conf` | **Rust canonical** (bash `scripts/update-cluster.sh` removed in v0.1.0 [#353]) |
| Spawn N workers | `make spawn-workers CONFIG=cluster.local.conf COUNT=2` | `hbird spawn-workers --config cluster.local.conf --count 2` (dry-run only; live tracked by [#289]) | **Bash retained** in v0.1.0 (Phase 4 deferred to v0.2.0) |
| Verify encryption | `make verify-encryption CONFIG=cluster.local.conf` | `hbird verify encryption --config cluster.local.conf` | **Rust canonical** (bash `scripts/verify-encryption.sh` removed in v0.1.0 [#353]) |
| Verify hardening | `make verify-hardening CONFIG=cluster.local.conf` | `hbird verify hardening --config cluster.local.conf` | **Rust canonical** (bash `scripts/verify-hardening.sh` removed in v0.1.0 [#353]) |
| Verify app-deploy | `make verify-app-deploy CONFIG=cluster.local.conf` | `hbird verify app-deploy --config cluster.local.conf` | **Rust canonical** (bash `scripts/verify-app-deploy.sh` removed in v0.1.0 [#353]) |
| Verify all | `make verify-all CONFIG=cluster.local.conf` | `hbird verify all --config cluster.local.conf` | **Rust canonical** (bash removed in v0.1.0 [#353]) |
| Fetch admin kubeconfig | `make get-kubeconfig CONFIG=cluster.local.conf` | `hbird get-kubeconfig --config cluster.local.conf` | **Rust canonical** (bash `scripts/export-argocd.sh` removed in v0.1.0 [#353]) |
| Export ArgoCD kubeconfig | `make export-argocd CONFIG=cluster.local.conf` | `hbird export-argocd --config cluster.local.conf` | **Rust canonical** (bash `scripts/export-argocd.sh` removed in v0.1.0 [#353]) |
| `kubectl get nodes` via SSH tunnel | `make nodes` | `hbird nodes --config cluster.local.conf` | **Rust canonical** (bash `scripts/kubectl-k8s.sh` removed in v0.1.0 [#353]) |
| Arbitrary `kubectl` via SSH tunnel | `make kubectl ARGS='get pods -A'` | `hbird kubectl --config cluster.local.conf -- get pods -A` | **Rust canonical** (bash `scripts/kubectl-k8s.sh` removed in v0.1.0 [#353]) |

## Install the binary

`hbird` ships as a single statically-linked binary. The release pipeline
is tracked by [#290] (cosign-signed musl binary + OCI image via
`cargo-dist`).

### Until [#290] lands: build from source

The Rust workspace lives at [`rust/`](../rust/). The devcontainer is the
supported build environment (host stays free of Rust toolchain). From
the repo root:

```bash
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . bash -c 'cd rust && cargo build --release -p hbird-cli'
# binary lands at rust/target/release/hbird
cp rust/target/release/hbird ~/.local/bin/hbird   # or wherever your $PATH points
```

See [`rust/README.md`](../rust/README.md) for the workspace layout, the
pinned toolchain, and the CI gates that run on every PR.

### After [#290] lands

Download the signed binary from the GitHub release page (link will be
populated in this doc once [#290] merges):

```bash
# Pattern (placeholder — verify the actual asset names against the release):
curl -fsSL https://github.com/aatchison/hummingbird-k8s/releases/latest/download/hbird-x86_64-unknown-linux-musl.tar.gz \
  | tar -xz -C ~/.local/bin hbird
cosign verify-blob \
  --certificate hbird.pem \
  --signature hbird.sig \
  --certificate-identity-regexp '.+github.com/aatchison/hummingbird-k8s/.+' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ~/.local/bin/hbird
hbird --version
```

## Cross-cutting flag-spelling differences

The `hbird` flag set follows the operator-mental-model contract from
epic [#279]: every bash twin's flag has a Rust counterpart with the
same meaning. A few spellings changed for consistency across
subcommands. Where the bash spelling was load-bearing for operator
muscle memory, clap `alias` keeps the old spelling working.

| Bash twin | `hbird` shape | Notes |
|-----------|---------------|-------|
| Positional `CONFIG=path` | `--config path` (also `CONFIG=path` env) | Every subcommand reads `--config` or the `CONFIG` env var, matching the bash twin's `CONFIG=` source pattern. |
| `KVM_HOST=geary` env | `--kvm-host geary` (also `KVM_HOST=geary` env) | Both work. Env-var fallback preserved via clap `env =`. |
| `CP_NAME=hbird-cp1` env | `--cp-name hbird-cp1` (also `CP_NAME=hbird-cp1` env) | Same — both surfaces honored. |
| `CP_IP=192.168.122.10` env | `--cp-ip 192.168.122.10` (also `CP_IP=…` env) | Same — both surfaces honored. |
| `--context-name foo` | `--context foo` (alias `--context-name` preserved) | PR #319 review L9 — flag renamed for consistency across subcommands; old spelling still works. |
| `HBIRD_REMOTE_NO_SUDO=1` env | `--no-sudo` (also `HBIRD_REMOTE_NO_SUDO=1` env) | Bash-twin `--no-sudo` had no flag form; Rust adds one. Boolish parser accepts `1`/`0`/`yes`/`no` on the env-var side. |
| `FORCE=1 make get-kubeconfig …` | `hbird get-kubeconfig --force …` | Same semantics — pre-existing output is snapshotted to `*.bak-<UTC>` before overwrite. |
| `make kubectl ARGS='get pods -A'` | `hbird kubectl -- get pods -A` | Trailing `--` separates `hbird`'s own flags from the pass-through; clap's `trailing_var_arg` + `allow_hyphen_values` make the `-A` flow through to kubectl. |
| `FLAGS='--workers-only --skip-drain' make update-cluster` | `hbird update-cluster --workers-only --skip-drain …` | Each `FLAGS=` token is a real first-class clap flag on the Rust side. |

## Per-target reference

Each section maps one Makefile target to its `hbird` counterpart, with
the bash invocation, the Rust invocation, and any flag-spelling deltas
worth knowing about.

For per-flag exhaustive docs, run `hbird <subcommand> --help` — clap's
generated help is the source of truth.

### `deploy-cluster`

End-to-end deploy from a `cluster.local.conf`: 1 CP + N workers,
hybrid bib + cloud-init.

```bash
# Bash (canonical):
make deploy-cluster CONFIG=cluster.local.conf
KVM_HOST=geary make deploy-cluster CONFIG=cluster.local.conf      # remote KVM host
KVM_HOST=geary HBIRD_REMOTE_NO_SUDO=1 \
  make deploy-cluster CONFIG=cluster.local.conf                   # libvirt-group operator

# Rust:
hbird deploy-cluster --config cluster.local.conf
hbird deploy-cluster --config cluster.local.conf --kvm-host geary
hbird deploy-cluster --config cluster.local.conf --kvm-host geary --no-sudo
hbird deploy-cluster --config cluster.local.conf --dry-run         # plan-only (Rust-side addition; bash twin has no --dry-run)
```

**Status:** Dry-run parity landed in Phase 4 ([PR #337]); live
execution (bib + virt-install + guestfish) deferred to [#335]. For
real deploys today, stay on the `make` target — the `hbird` binary's
live path is still scaffolded.

See [`docs/deploy-cluster.md`](deploy-cluster.md) for the full deploy
reference.

### `destroy-cluster`

Tear down VMs + qcow2 + seed ISOs for the names in `cluster.local.conf`.

```bash
# Bash:
make destroy-cluster CONFIG=cluster.local.conf
KVM_HOST=geary make destroy-cluster CONFIG=cluster.local.conf

# Rust:
hbird destroy-cluster --config cluster.local.conf
hbird destroy-cluster --config cluster.local.conf --kvm-host geary --no-sudo
hbird destroy-cluster --config cluster.local.conf --dry-run        # plan-only (Rust addition)
```

**Status:** Phase 4 ([PR #337]) — both dry-run and live execution
landed. `hbird destroy-cluster` invokes the same `virsh destroy` /
`virsh undefine` / `rm -f` verbs the bash twin does, via the additive
`hbird-virt::Connection::{destroy_domain, undefine_domain, remote_rm_f,
remote_rm_rf, remote_path_exists}` API.

### `update-cluster` / `update-workers` / `update-node`

Rolling bootc upgrade — CP first (brief apiserver outage), then each
worker drained → upgraded → uncordoned. Same `cluster.local.conf`
drives both targets.

```bash
# Bash (full roll):
make update-cluster CONFIG=cluster.local.conf
make update-workers CONFIG=cluster.local.conf                                # skip CP
make update-node    CONFIG=cluster.local.conf NODE=hbird-w1                  # one node
FLAGS='--start-from=hbird-w2 --parallel=2' make update-cluster CONFIG=cluster.local.conf

# Rust:
hbird update-cluster --config cluster.local.conf
hbird update-cluster --config cluster.local.conf --workers-only
hbird update-cluster --config cluster.local.conf --node hbird-w1             # single node
hbird update-cluster --config cluster.local.conf --start-from hbird-w2 --parallel 2
hbird update-cluster --config cluster.local.conf --dry-run                   # plan-only (bash-twin parity, byte-for-byte fixtures)
```

**Flag map:** every `FLAGS=` token from the bash twin is a real
first-class clap flag in `hbird update-cluster`. Direct map:

| Bash `FLAGS=` token | Rust flag |
|---------------------|-----------|
| `--workers-only` | `--workers-only` |
| `--node=NAME` | `--node NAME` |
| `--start-from=NAME` | `--start-from NAME` |
| `--parallel=N` | `--parallel N` |
| `--skip-drain` | `--skip-drain` |
| `--skip-gates` | `--skip-gates` |
| `--continue-on-error` | `--continue-on-error` |
| `--no-delete-emptydir-data` | `--no-delete-emptydir-data` |
| `--node-name-override DOMAIN=NODE` | `--node-name-override DOMAIN=NODE` (repeatable) |
| `--dry-run` | `--dry-run` |

Env-tunable timeouts (`DRAIN_TIMEOUT`, `READY_TIMEOUT`,
`DAEMONSET_TIMEOUT`, `APISERVER_TIMEOUT`, `SSH_TIMEOUT`,
`SSH_DROP_TIMEOUT`, `INTER_NODE_SLEEP`) are honored verbatim on the
Rust side — the parser even reproduces the bash twin's regex
validation diagnostics for hostile values
(`READY_TIMEOUT='a[0$(reboot)]'` → identical error message).

**Status:** Phase 1A ([PR #321]) — dry-run parity pinned by eleven
byte-for-byte fixtures under
`rust/crates/hbird-cli/tests/update_cluster/fixtures/`. Phase 1B
cycles 1–4 ([PRs #325 #344 #346 #347]) — live drain/uncordon, bootID
gate, DaemonSet-ready gate, and `wait_apiserver_back` all landed.

**Deferred:** `timer_stop` / `timer_start` (block #4 of the live
plan) remains stubbed because the geary cluster doesn't run the
`bootc-semver-update.timer` the bash twin pauses/resumes during a
roll. The Rust binary surfaces a stable
`live_mode_not_implemented` diagnostic for these two helpers; the
cluster-rotation paths themselves are complete. Tracked by [#322].

See [`docs/update-cluster.md`](update-cluster.md) for the full
update-cluster reference (drain semantics, bootID gate, DaemonSet
readiness gate, escape hatches).

### `spawn-workers`

Spawn N additional workers against an existing cluster.

```bash
# Bash:
make spawn-workers CONFIG=cluster.local.conf COUNT=2

# Rust:
hbird spawn-workers --config cluster.local.conf --count 2
hbird spawn-workers --config cluster.local.conf --count 2 --kvm-host geary --no-sudo
hbird spawn-workers --config cluster.local.conf --count 2 --dry-run            # plan-only (Rust addition)
```

**Status:** Phase 4 ([PR #337]) — dry-run planner only; live execution
(virt-install + guestfish parity) deferred to [#335]. Use the `make`
target for real spawns today.

### `verify-encryption` / `verify-hardening` / `verify-app-deploy` / `verify-all`

Post-deploy verifiers.

```bash
# Bash:
make verify-encryption CONFIG=cluster.local.conf
make verify-hardening  CONFIG=cluster.local.conf
make verify-app-deploy CONFIG=cluster.local.conf
make verify-all        CONFIG=cluster.local.conf
KVM_HOST=geary make verify-hardening CONFIG=cluster.local.conf

# Rust:
hbird verify encryption  --config cluster.local.conf
hbird verify hardening   --config cluster.local.conf
hbird verify app-deploy  --config cluster.local.conf
hbird verify all         --config cluster.local.conf
hbird verify hardening   --config cluster.local.conf --kvm-host geary
```

**Status:** Phase 2 ([PR #330]) — live-validated against the geary
cluster 2026-05-27, bash twin removed in v0.1.0 cutover ([#353]). The
Rust path SSHes directly to `root@CP_IP` (the removed
`scripts/kubectl-k8s.sh`'s port-forward wrapper had a stdin-swallowing
bug — [#332] — so PSA-rejection checks in the bash twin missed the
`violates PodSecurity` marker on a correctly-hardened cluster; the
Rust twin observes enforcement correctly).

See [`docs/security-hardening.md`](security-hardening.md),
[`docs/etcd-encryption.md`](etcd-encryption.md), and
[`docs/app-deploy-verify.md`](app-deploy-verify.md) for what each
verifier checks.

### `get-kubeconfig`

Daily-use kubeconfig fetcher. Writes `./kubeconfig.yaml` with a
context named after `CP_NAME` (no `hummingbird-` prefix).

```bash
# Bash:
make get-kubeconfig CONFIG=cluster.local.conf
make get-kubeconfig CONFIG=cluster.local.conf KVM_HOST=geary
make get-kubeconfig CONFIG=cluster.local.conf FORCE=1
make get-kubeconfig CONFIG=cluster.local.conf \
    SERVER=https://cluster.example.com:6443 \
    CONTEXT=prod-cp \
    OUTPUT=$HOME/.kube/hummingbird.yaml

# Rust:
hbird get-kubeconfig --config cluster.local.conf
hbird get-kubeconfig --config cluster.local.conf --kvm-host geary             # via --kvm-host
hbird get-kubeconfig --config cluster.local.conf --proxy-jump geary           # equivalent — explicit ProxyJump
hbird get-kubeconfig --config cluster.local.conf --force
hbird get-kubeconfig --config cluster.local.conf \
    --server https://cluster.example.com:6443 \
    --context prod-cp \
    --output $HOME/.kube/hummingbird.yaml
hbird get-kubeconfig --config cluster.local.conf --context-name prod-cp       # alias preserved
```

**Status:** Phase 3 ([PR #334]) — live-validated. The Rust path ships
the corrected ProxyJump resolution shape (post-[#306]: config-then-CLI,
matching documented bash semantics) and non-TTY SSH (post-[#307]: sudo
can't emit OSC escapes that pollute stdout).

See [`docs/argocd.md`](argocd.md#leak-recovery-playbook-if-the-exported-file-leaks)
for the security model (applies identically — the fetched file is
`admin.conf`-grade).

### `export-argocd`

ArgoCD-shaped kubeconfig export. Writes `./argocd-kubeconfig.yaml`
with a context named `hummingbird-$CP_NAME` (prefix avoids collision
with whatever's already registered).

```bash
# Bash:
make export-argocd CONFIG=cluster.local.conf
make export-argocd CONFIG=cluster.local.conf FORCE=1
KVM_HOST=geary make export-argocd CONFIG=cluster.local.conf

# Rust:
hbird export-argocd --config cluster.local.conf
hbird export-argocd --config cluster.local.conf --force
hbird export-argocd --config cluster.local.conf --kvm-host geary
hbird export-argocd --config cluster.local.conf --server https://cluster.example.com:6443
```

**Status:** Phase 3 ([PR #334]) — live-validated. Same #306/#307
fixes as `get-kubeconfig` (shared core via
`rust/crates/hbird-cli/src/cp_kubectl.rs`).

See [`docs/argocd.md`](argocd.md) for the security model, leak
recovery, and cert lifecycle.

### `nodes`

`kubectl get nodes` via the SSH-tunnel wrapper. Daily smoke check.

```bash
# Bash:
make nodes
KVM_HOST=geary make nodes

# Rust:
hbird nodes --config cluster.local.conf
hbird nodes --config cluster.local.conf --kvm-host geary
hbird nodes --cp-name hbird-cp1 --cp-ip 192.168.122.10 --kvm-host geary       # explicit flags, no config file
```

**Status:** Phase 3 ([PR #334]) — byte-for-byte parity with the bash
twin (empty diff in the live-validate fixture).

### `kubectl` (pass-through)

Arbitrary `kubectl` via the SSH-tunnel wrapper.

```bash
# Bash:
make kubectl ARGS='get pods -A'
make kubectl ARGS='-n kube-system logs ds/cilium'
KVM_HOST=geary make kubectl ARGS='get nodes -o wide'

# Rust:
hbird kubectl --config cluster.local.conf -- get pods -A
hbird kubectl --config cluster.local.conf -- -n kube-system logs ds/cilium
hbird kubectl --config cluster.local.conf --kvm-host geary -- get nodes -o wide
```

The `--` separator is required when the pass-through args start with
a dash — clap needs the hint to stop parsing its own flags. For
all-positional args (`hbird kubectl -- get pods`) it's optional but
recommended for muscle memory consistency.

**Status:** Phase 3 ([PR #334]) — byte-for-byte parity.

`hbird kubectl` resolves CP_IP automatically via virsh-domifaddr (same
shape as the deleted `scripts/kubectl-k8s.sh`'s `ssh $KVM_HOST virsh -c
qemu:///system domifaddr $CP_NAME | awk /ipv4/...` pipeline) when CP_IP
isn't pinned in `--config`. Workstation operators only need `KVM_HOST`
set; no need to pin `CP_IP=` for day-to-day kubectl. PR #366 round-2 H1
closed the previously-deferred placeholder.

### Still bash-only (not yet on `hbird`)

A handful of operator targets remain bash-only by design — these run
exclusively on the KVM host (etcd direct access, libvirt-pool ops)
and live in `/usr/libexec/` on the image, driven by Makefile targets.
A Rust port for these is not currently scheduled.

| Target | Bash | Why bash-only |
|--------|------|---------------|
| `backup-etcd` | `sudo make backup-etcd` | etcd `snapshot save` runs directly against the CP's local etcd unix socket. |
| `restore-etcd` | `sudo make restore-etcd SNAP=<file>` | Same — restore is a CP-local etcdctl invocation. |
| `rotate-etcd-key` | `sudo make rotate-etcd-key` | Walks a 4-stage rotation against `/etc/kubernetes/manifests/kube-apiserver.yaml`. |
| `clean-vms` | `make clean-vms` (honors `KVM_HOST`) | libvirt pool cleanup — already C3-wrapped to honor remote KVM host. |
| `switch-to-ghcr` | `sudo make switch-to-ghcr` / `KVM_HOST=geary make switch-to-ghcr` | bootc switch on every VM. C3-wrapped. |
| `image-k8s` / `image-worker` / `push-image-*` | `make image-k8s …` | Local `podman build` + `bootc-image-builder`; the operator pipeline lives in `.github/workflows/`, not in `hbird`. |
| `kube-bench` | `KVM_HOST=geary make kube-bench` | Runs the CIS scanner pod against the live apiserver. |

If you find yourself wanting any of these in `hbird`, open an issue —
the surface is intentionally small for now, and the bash twins are
exercised by integration tests.

## Logging recipes

`hbird` uses the [`tracing`] / [`tracing-subscriber`] ecosystem (chosen
by [#323], wired in [PR #326]). Filter via `RUST_LOG`:

```bash
RUST_LOG=hbird_cli=debug   hbird update-cluster --config cluster.local.conf  # CLI + subcommand spans
RUST_LOG=hbird_ssh=debug   hbird verify hardening --config …                 # SSH spawn / completion / non-zero-exit
RUST_LOG=hbird_virt=debug  hbird nodes --config …                            # virsh wrapper spans
RUST_LOG=hbird_cli=debug,hbird_ssh=debug  hbird update-cluster …             # both
```

Defaults: `info` filter, writer is stderr (stdout is reserved for the
bash-twin-style `[update-cluster] …` lines that the dry-run fixtures
pin byte-for-byte), no ANSI / no timestamps / no targets, span CLOSE
events at debug. Set `RUST_BACKTRACE=1` to opt into a stack trace on
error.

## Migration recipe — bash to Rust on the same operator workflow

A typical operator flow today (bash) and its Rust counterpart, side
by side:

```bash
# --- BASH (canonical) ---------------------------------------------
export KVM_HOST=geary
make deploy-cluster   CONFIG=cluster.local.conf
make verify-all       CONFIG=cluster.local.conf
make nodes
make kubectl          ARGS='get pods -A'
make get-kubeconfig   CONFIG=cluster.local.conf
make update-cluster   CONFIG=cluster.local.conf
make destroy-cluster  CONFIG=cluster.local.conf

# --- RUST (parallel surface) --------------------------------------
export KVM_HOST=geary
hbird deploy-cluster   --config cluster.local.conf                          # dry-run parity; live tracked by #335
hbird verify all       --config cluster.local.conf
hbird nodes            --config cluster.local.conf
hbird kubectl          --config cluster.local.conf -- get pods -A
hbird get-kubeconfig   --config cluster.local.conf
hbird update-cluster   --config cluster.local.conf
hbird destroy-cluster  --config cluster.local.conf                          # full live execution
```

`KVM_HOST` is read from the env by both sides — no per-invocation
spelling change for the SSH-via-KVM-host path.

## Where the canonical implementation lives (post v0.1.0 partial cutover, [#353])

| Concern | Canonical today | Status |
|---------|-----------------|--------|
| Cluster lifecycle — update | `rust/crates/hbird-cli/src/commands/update_cluster.rs` | Bash removed in v0.1.0 [#353] |
| Cluster lifecycle — deploy/destroy/spawn | `scripts/{deploy,destroy,spawn-workers}-cluster.sh` (bash); Rust dry-run only in `rust/crates/hbird-cli/src/commands/{deploy_cluster,destroy_cluster,spawn_workers}.rs` | **Bash retained** in v0.1.0; live Rust tracked by [#289] / Phase 4 deferred to v0.2.0 |
| Verifiers | `rust/crates/hbird-cli/src/commands/verify.rs` | Bash removed in v0.1.0 [#353] |
| Kubeconfig export | `rust/crates/hbird-cli/src/commands/{export_argocd,get_kubeconfig}.rs` (shared core) | Bash `scripts/export-argocd.sh` removed in v0.1.0 [#353] |
| kubectl SSH-tunnel wrapper | `rust/crates/hbird-cli/src/commands/{nodes,kubectl}.rs` + `rust/crates/hbird-cli/src/cp_kubectl.rs` | Bash `scripts/kubectl-k8s.sh` removed in v0.1.0 [#353] |
| SSH transport (bash-side) | `scripts/lib/ssh-wrap.sh` + `ssh_opts_array{,_no_identity}` | Retained for kept Phase 4 bash scripts (deploy/destroy/spawn) |
| SSH transport (Rust-side) | `rust/crates/hbird-ssh/` | Used by every Rust subcommand |
| libvirt verbs | `rust/crates/hbird-virt/` (Rust) + inline `virsh` in retained Phase 4 bash | Mixed; Rust canonical for Phase 1-3, bash retained for Phase 4 |
| Cluster config parse | `rust/crates/hbird-config/` (Rust) + inline `source $CONFIG` in retained Phase 4 bash | Mixed |

[#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
[#289]: https://github.com/aatchison/hummingbird-k8s/issues/289
[#290]: https://github.com/aatchison/hummingbird-k8s/issues/290
[#353]: https://github.com/aatchison/hummingbird-k8s/issues/353
[#306]: https://github.com/aatchison/hummingbird-k8s/issues/306
[#307]: https://github.com/aatchison/hummingbird-k8s/issues/307
[#322]: https://github.com/aatchison/hummingbird-k8s/issues/322
[#323]: https://github.com/aatchison/hummingbird-k8s/issues/323
[#332]: https://github.com/aatchison/hummingbird-k8s/issues/332
[#335]: https://github.com/aatchison/hummingbird-k8s/issues/335
[PR #321]: https://github.com/aatchison/hummingbird-k8s/pull/321
[PR #325]: https://github.com/aatchison/hummingbird-k8s/pull/325
[PR #326]: https://github.com/aatchison/hummingbird-k8s/pull/326
[PR #330]: https://github.com/aatchison/hummingbird-k8s/pull/330
[PR #334]: https://github.com/aatchison/hummingbird-k8s/pull/334
[PR #337]: https://github.com/aatchison/hummingbird-k8s/pull/337
[PR #344]: https://github.com/aatchison/hummingbird-k8s/pull/344
[PR #346]: https://github.com/aatchison/hummingbird-k8s/pull/346
[PR #347]: https://github.com/aatchison/hummingbird-k8s/pull/347
[PRs #325 #344 #346 #347]: https://github.com/aatchison/hummingbird-k8s/issues/322
[`tracing`]: https://docs.rs/tracing
[`tracing-subscriber`]: https://docs.rs/tracing-subscriber
