# Rust CLI (work in progress — epic [#279])

The bash client-side tooling under [`../scripts/`](../scripts/) is being
rewritten in Rust over several phases. See [`rust/README.md`](../rust/README.md)
for the workspace layout and the epic for the architectural plan.

## Operator-facing status (today)

**The `hbird` binary now builds and parses every operator-facing
subcommand**, but every subcommand body returns
`Err("not yet implemented — tracked by #XXX")`. All `make
deploy-cluster`, `make update-cluster`, `make destroy-cluster`,
`make verify-*`, etc. continue to invoke the existing bash scripts —
those remain canonical until the per-command implementation PRs
([#286]–[#289]) land. The Rust workspace at `../rust/` is a foundation;
subcommands land per the phasing table in
[`rust/README.md`](../rust/README.md).

## Foundation status

| Sub-issue | What | Status |
|-----------|------|--------|
| [#280](https://github.com/aatchison/hummingbird-k8s/issues/280) | Devcontainer + cargo workspace skeleton | landed (PR #313) |
| [#281](https://github.com/aatchison/hummingbird-k8s/issues/281) | CI workflow (fmt / clippy / test / deny / pre-commit / devcontainer smoke / lint inheritance) | landed (PR #314) |
| [#282](https://github.com/aatchison/hummingbird-k8s/issues/282) | `ClusterConfig` parser (first real crate) | landed (PR #315) |
| [#283](https://github.com/aatchison/hummingbird-k8s/issues/283) | clap command tree (`hbird` binary) | landed (PR #319) |
| [#284](https://github.com/aatchison/hummingbird-k8s/issues/284) | virt + `qemu+ssh` URI transport | landed (PR #318) |
| [#285](https://github.com/aatchison/hummingbird-k8s/issues/285) | openssh transport | landed (PR #317) |
| [#286](https://github.com/aatchison/hummingbird-k8s/issues/286) | `update-cluster` Phase 1 — dry-run parity + orchestration scaffold | this PR (live-execution slice deferred) |

## Foundation crates landed so far

| Crate | Purpose | Tracked by |
|-------|---------|-----------|
| `hbird-common` | Workspace bootstrap placeholder (project constants). | [#280] |
| `hbird-config` | Typed parser for `cluster.local.conf`. | [#282] |
| `hbird-ssh` | SSH transport — Rust twin of `scripts/lib/ssh-wrap.sh` + `ssh_opts_array{,_no_identity}`. | [#285] |
| `hbird-virt` | libvirt CLI wrapper + typed `qemu+ssh://` URI parser. Talks to a remote KVM host via a `SshClient` trait object (real impl lands in [#285] / [#286]). | [#284] |
| `hbird-cli` | `hbird` binary — clap-derive command tree mirroring the operator-facing `Makefile` targets. Subcommand bodies arrive in [#286]–[#289]. | [#283] |

[#280]: https://github.com/aatchison/hummingbird-k8s/issues/280
[#282]: https://github.com/aatchison/hummingbird-k8s/issues/282
[#283]: https://github.com/aatchison/hummingbird-k8s/issues/283
[#284]: https://github.com/aatchison/hummingbird-k8s/issues/284
[#285]: https://github.com/aatchison/hummingbird-k8s/issues/285

## `hbird` command tree (scaffolded by [#283])

```text
hbird deploy-cluster      <-> make deploy-cluster      (body tracked by #289)
hbird destroy-cluster     <-> make destroy-cluster     (body tracked by #289)
hbird update-cluster      <-> make update-cluster      (body tracked by #286)
hbird verify encryption   <-> make verify-encryption   (body tracked by #287)
hbird verify hardening    <-> make verify-hardening    (body tracked by #287)
hbird verify app-deploy   <-> make verify-app-deploy   (body tracked by #287)
hbird verify all          <-> make verify-all          (body tracked by #287)
hbird get-kubeconfig      <-> make get-kubeconfig      (body tracked by #288)
hbird export-argocd       <-> make export-argocd      (body tracked by #288)
hbird nodes               <-> make nodes               (body tracked by #288)
hbird kubectl …           <-> make kubectl             (body tracked by #288)
```

Every subcommand currently returns
`Err("not yet implemented — tracked by #XXX")` with the appropriate
sub-issue link. The flag set + help text are stable; the Makefile will
start dispatching to `hbird` per-target as each implementation lands.

Tracing/logging crate choice — deferred. The other foundation crates
left `// TODO(#283)` markers pointing here; this PR keeps the deferral
explicit (the binary currently has nothing to log beyond the dispatch
error) and pushes the decision to the first real subcommand body
([#286] `update-cluster`), which is where a logging surface will
actually be exercised.

## When the Rust binary will appear in the `Makefile`

Per the operator-mental-model contract in the epic, the `Makefile` will
dispatch to either the Rust binary or the bash equivalent on a per-target
basis. The first dispatch lands when [#286] (`update-cluster`) reaches
behavioral parity — error formatting, log shape, and exit codes
indistinguishable from the bash twin against the same cluster.

Until then this doc is a stub; the migration guide lands at
`docs/rust-migration.md` per [#291] once at least one subcommand has
switched dispatch.

## How to verify behavioral parity (Phase 1 onward)

The contract: run both the Rust binary and the bash script against the
same cluster + diff `kubectl`/`virsh` state at each step.

### Phase 1 (`update-cluster`) — dry-run parity (this PR, #286)

The first slice that landed is the **dry-run** path. `hbird
update-cluster --config cluster.local.conf --dry-run` emits a log line
sequence byte-for-byte identical to `make update-cluster
CONFIG=cluster.local.conf FLAGS=--dry-run`. This is pinned in CI by
`rust/crates/hbird-cli/tests/update_cluster_dry_run.rs`, which compares
the Rust output to **eleven** fixtures under
`rust/crates/hbird-cli/tests/update_cluster/fixtures/`. The fixtures
cover every flag combination (`--workers-only`, `--node` for both CP and
worker, `--start-from`, `--skip-drain`, `--skip-gates`,
`--no-delete-emptydir-data`, `--parallel=2`, `--node-name-override`,
`--continue-on-error`) and were captured by running the bash twin
against a synthetic config that names the live cluster's VMs.

### Phase 1 (`update-cluster`) — live execution slice (follow-up)

The live (non-dry-run) execution slice is **deferred** to a follow-up
issue. The orchestration scaffold is in place: lock acquisition,
plan-from-args validation, k8s-node-name resolution, in-flight tracking,
worker batches, and the bash-twin block-by-block helper layout
(`timer_stop`, `bootc_upgrade_apply`, `wait_node_ready`, etc.) all
exist as Rust functions. What's deferred:

- Real SSH round-trips through `hbird-ssh::Client` (the helpers
  currently surface a stable "live-mode not implemented" diagnostic
  that names the bash equivalent for operator orientation).
- Real virsh-domifaddr IP resolution through `hbird-virt::Connection`
  (operators must set `CP_IP=`/`WORKER_IPS=()` in `cluster.local.conf`
  to bypass that path in the live slice's interim).
- True parallel-batch concurrency (the scaffold processes the batch
  serially under the `[parallel:NAME]` log prefix, so the dry-run log
  shape still matches the bash twin byte-for-byte).
- `bootc rollback` + post-reboot live-validate harness (per-block
  fixtures capturing real `kubectl get nodes -o yaml` deltas).

This deliberate scope-bound exists because Phase 1's live-validate
contract requires `make destroy-cluster && make deploy-cluster` between
each behavioral block — that destroys the only live cluster repeatedly
and is best done by the operator-with-tmux rather than an autonomous
agent. The deferred slice is tracked as a follow-up to #286.

[#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
[#286]: https://github.com/aatchison/hummingbird-k8s/issues/286
[#291]: https://github.com/aatchison/hummingbird-k8s/issues/291
