# Rust CLI (work in progress — epic [#279])

The bash client-side tooling under [`../scripts/`](../scripts/) is being
rewritten in Rust over several phases. See [`rust/README.md`](../rust/README.md)
for the workspace layout and the epic for the architectural plan.

## Install (from a tagged release)

Releases are published on `v*` tag push by
[`.github/workflows/release.yml`](../.github/workflows/release.yml) (issue
[#290]). Each release ships:

- A statically-linked `x86_64-unknown-linux-musl` `hbird` binary (no
  glibc / no shared-library dependencies — runs on any Linux x86_64
  host, including Alpine / scratch containers).
- A cosign keyless-OIDC signature (`.sig` + `.pem`) over the binary,
  recorded in the Rekor public transparency log.
- An OCI image at `ghcr.io/aatchison/hbird:VERSION` (immutable tag) +
  `ghcr.io/aatchison/hbird:latest`, also cosign-signed by digest. The
  image is `FROM scratch` (~5MB, single-binary).
- A `SHA256SUMS` file for the binary.

### Verify + install the binary

```sh
VERSION=v0.0.1
curl -sSL -o hbird               "https://github.com/aatchison/hummingbird-k8s/releases/download/${VERSION}/hbird-${VERSION}-x86_64-unknown-linux-musl"
curl -sSL -o hbird.cosign.bundle "https://github.com/aatchison/hummingbird-k8s/releases/download/${VERSION}/hbird-${VERSION}-x86_64-unknown-linux-musl.cosign.bundle"
cosign verify-blob \
  --bundle hbird.cosign.bundle \
  --certificate-identity-regexp "^https://github.com/aatchison/hummingbird-k8s/.github/workflows/release.yml@" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  hbird
chmod +x hbird
sudo install -m 0755 hbird /usr/local/bin/hbird
hbird --version
```

`cosign verify-blob` validates that (a) the signature matches the
binary's hash and (b) the signing cert was minted by Fulcio for this
exact GitHub Actions workflow on this repository. A failure means
either the binary or signature was tampered with, or someone is
serving a forgery from a look-alike URL.

The `--certificate-identity-regexp` value is the *subject claim*
Fulcio stamps onto the short-lived cert during keyless OIDC signing —
it must match the workflow's full URI:
`https://github.com/<owner>/<repo>/.github/workflows/<file>.yml@<git-ref>`.
The trailing `@` is load-bearing: it anchors the regex so a forger
can't craft a cert from a fork whose workflow file path happens to
share the prefix. Adjust `<git-ref>` to `refs/tags/v0.0.1` if you
want to pin verification to a specific tagged release rather than
any signing event from this workflow.

### Verify + pull the OCI image

```sh
VERSION=v0.0.1
podman pull ghcr.io/aatchison/hbird:${VERSION}
cosign verify ghcr.io/aatchison/hbird:${VERSION} \
  --certificate-identity-regexp "^https://github.com/aatchison/hummingbird-k8s/.github/workflows/release.yml@" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com"
podman run --rm ghcr.io/aatchison/hbird:${VERSION} --version
```

The image is `FROM scratch`, so it has no shell — invocations must
go via `podman run … <subcommand>`, not `podman exec`. Operators
typically use the OCI image only for k8s-native CronJob /
`run-once` workflows; on a workstation the binary install is faster
and integrates with the operator's `ssh-agent` / `~/.ssh` automatically.

### Smoke-testing the release pipeline before a tag push

The release workflow accepts a `workflow_dispatch` trigger with a
`dry_run` input (defaults to `true`). When `dry_run=true`, every
step EXCEPT the final publish (GHCR push, cosign sign image, gh
release create) runs — the binary is built, statically-linked,
cosign-signed (the OIDC flow exercised end-to-end, with
`--tlog-upload=false` so no permanent Rekor record is created), and
the OCI image is built locally. Use this before a real `v*` tag push
to confirm the workflow is healthy.

```sh
gh workflow run release.yml -f version=v0.0.0-dryrun -f dry_run=true
```

Output lands in the workflow run's **Summary** tab in the Actions
UI — staged binary path, signature paths, OCI image digest, and the
explicit "what was skipped" list are all there. `gh run watch
<run-id>` streams the logs; `gh run view <run-id>` jumps to the
summary.

[#290]: https://github.com/aatchison/hummingbird-k8s/issues/290

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
| [#286](https://github.com/aatchison/hummingbird-k8s/issues/286) | `update-cluster` Phase 1A — dry-run parity + orchestration scaffold | landed (PR #321) |
| [#322](https://github.com/aatchison/hummingbird-k8s/issues/322) | `update-cluster` Phase 1B — live-execution slice | cycles 1 (`cp_kubectl` + drain/uncordon, PR #325) + 2 (bootID + bootc upgrade + SSH drop/back, PR #344) + 3 (DaemonSet-ready gate, [#328](https://github.com/aatchison/hummingbird-k8s/issues/328)) + 4 (`wait_apiserver_back`, [#329](https://github.com/aatchison/hummingbird-k8s/issues/329)) landed. Remaining `live_mode_not_implemented` sites: `timer_stop` / `timer_start` (block #4) — cluster doesn't run scheduled-update timers per bash twin, deferred outside Phase 1B cycle scope. |
| [#287](https://github.com/aatchison/hummingbird-k8s/issues/287) | `verify-*` Phase 2 — encryption / hardening / app-deploy / all | landed (PR #330) — live-validated 2026-05-27 |
| [#288](https://github.com/aatchison/hummingbird-k8s/issues/288) | Phase 3 — `export-argocd` / `get-kubeconfig` / `nodes` / `kubectl` | landed (PR #334) — live-validated 2026-05-27 |

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
hbird deploy-cluster      <-> make deploy-cluster      (dry-run landed PR #289; live tracked by #335)
hbird destroy-cluster     <-> make destroy-cluster     (dry-run + live landed PR #289)
hbird spawn-workers       <-> make spawn-workers       (dry-run landed PR #289; live tracked by #335)
hbird update-cluster      <-> make update-cluster      (body tracked by #286)
hbird verify encryption   <-> make verify-encryption   (body tracked by #287)
hbird verify hardening    <-> make verify-hardening    (body tracked by #287)
hbird verify app-deploy   <-> make verify-app-deploy   (body tracked by #287)
hbird verify all          <-> make verify-all          (body tracked by #287)
hbird get-kubeconfig      <-> make get-kubeconfig      (landed: PR #288)
hbird export-argocd       <-> make export-argocd      (landed: PR #288)
hbird nodes               <-> make nodes               (landed: PR #288)
hbird kubectl …           <-> make kubectl             (landed: PR #288)
```

Subcommand status:

- `update-cluster` — Phase 1A (dry-run) landed (#286 / PR #321); Phase 1B
  cycle 1 (cp_kubectl + drain/uncordon) landed (#322 / PR #325); cycle 2
  (bootID + bootc upgrade + SSH drop/back) landed (#327 / PR #344);
  cycle 3 (DaemonSet-ready gate) landed (#328 / PR #346); cycle 4
  (`wait_apiserver_back`) landed (#329). Phase 1B is now complete for
  the cluster-rotation paths; `timer_stop`/`timer_start` (block #4)
  remain stubbed but the geary cluster doesn't run scheduled-update
  timers so they sit outside the Phase 1B cycle scope.
- `verify {encryption,hardening,app-deploy,all}` — Phase 2 landed
  (#287 / PR #330). Live-validated against the geary cluster
  2026-05-27.
- `export-argocd` / `get-kubeconfig` / `nodes` / `kubectl` — Phase 3
  landed (#288 / PR #334). Each routes through the shared `cp_kubectl`
  module (sibling of `commands/`) — same SSH+ProxyJump wiring as Phase
  1B cycle 1. The export-argocd / get-kubeconfig paths ship the
  *corrected* shape (post-#306 ProxyJump-after-config and post-#307
  non-TTY SSH) — see fixture annotations.
- `deploy-cluster` / `destroy-cluster` / `spawn-workers` — Phase 4
  landed (#289). Dry-run planners for all three pin byte-for-byte
  fixtures (`tests/update_cluster/fixtures/dry_run_{deploy,destroy,spawn}.txt`).
  `destroy-cluster` ships with full live execution wired through
  `hbird-virt::Connection` (additive `destroy_domain` / `undefine_domain`
  / `remote_rm_f` / `remote_rm_rf` / `remote_path_exists` verbs)
  bridged to `hbird-ssh::Client` via an in-crate `CliSshBridge`.
  `deploy-cluster` + `spawn-workers` live execution is deferred to
  [#335](https://github.com/aatchison/hummingbird-k8s/issues/335) —
  bib + virt-install + guestfish parity is a hundreds-of-LOC follow-up
  that overlaps with #311's bib-rootful work in flight.

The flag set + help text are stable; the Makefile will start
dispatching to `hbird` per-target as each implementation reaches
parity.

Tracing/logging — `tracing` + `tracing-subscriber` (chosen by [#323],
PR #326). Library crates (`hbird-config`, `hbird-ssh`, `hbird-virt`)
depend on `tracing` only and emit spans + events via
`#[tracing::instrument]`. The `hbird` binary owns subscriber init in
`main()`: default filter is `info` (configurable via `RUST_LOG`),
writer is stderr (stdout is reserved for the bash-twin-style
`[update-cluster] …` lines that PR #321's fixtures pin byte-for-byte),
no ANSI, no timestamps, no targets, span CLOSE events at debug.

Verbose-log recipes for operator triage:

```sh
RUST_LOG=hbird_cli=debug hbird update-cluster ...      # CLI + subcommand spans
RUST_LOG=hbird_ssh=debug hbird update-cluster ...      # SSH spawn / completion / non-zero-exit events
RUST_LOG=hbird_virt=debug hbird nodes                  # virsh wrapper spans
RUST_LOG=hbird_cli=debug,hbird_ssh=debug ...           # multiple crates
```

Span `err(Debug)` directive records `error = ?err` automatically when
an instrumented fn returns `Err`, so an operator running at default
`info` still sees error context (just no debug spam on the happy path).

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

### Phase 1B (`update-cluster`) — live execution (in progress)

The live (non-dry-run) execution slice is tracked by [#322] and lands
in cycles. Each cycle wires one or more bash-twin helpers and validates
against the live geary cluster with a bash-vs-Rust diff captured under
`rust/crates/hbird-cli/tests/update_cluster/fixtures/live/`.

| Cycle | Block | Helpers wired | Status |
|-------|-------|---------------|--------|
| 1 | #5 + part of update_worker | `cp_kubectl`, drain, uncordon | landed (PR #325) |
| 2 | #6 + #8 + #10 | `capture_node_bootid`, `wait_node_bootid_changed`, `bootc_upgrade_apply` (+ `bootc_has_apply` + `bootc_booted_digest`), `wait_ssh_drop`, `wait_ssh_back` | landed (#327) |
| 3 | #9 | `wait_node_ready`, `wait_node_daemonsets_ready` (+ `_collect_unready_names` parser) | landed (#328) |
| 4 | #7 | `wait_apiserver_back` (etcd-backup not invoked inline by bash twin — separate `make backup-etcd` target out of scope) | landed (#329) |

### Phase 3 (`export-argocd` / `get-kubeconfig` / `nodes` / `kubectl`)

Landed by [#288]. Four subcommands share the
`crates/hbird-cli/src/cp_kubectl.rs` module (sibling of `commands/`)
which provides:

- `cp_kubectl_raw(target, command)` — kubectl-wrapped SSH (errs on
  non-zero exit). Used by `nodes` / `kubectl`.
- `cp_ssh_capture(target, command)` — raw SSH (errs on non-zero,
  returns raw bytes for binary payloads). Used by `export-argocd` /
  `get-kubeconfig` to `sudo cat /etc/kubernetes/admin.conf`.
- `cp_kubectl_with_stdin_lenient` / `cp_ssh_lenient` — non-zero
  tolerated variants reserved for #287's PSA-rejection + audit-log
  probes. Both `#[allow(dead_code)]` in this PR; #287 picks them up.

Two known bash bugs the Rust impl deliberately fixes:

- **[#306]** — bash sets `PROXY_JUMP="${KVM_HOST:-}"` BEFORE sourcing
  `$CONFIG`, so `KVM_HOST=geary` pinned in CONFIG doesn't activate
  ProxyJump unless also exported in the operator's shell. The Rust
  path resolves AFTER config load (config → CLI/env override → empty),
  matching the documented bash semantics.
- **[#307]** — bash `cp_ssh() { ssh -t ... }` allocates a remote PTY;
  modern sudo can emit OSC session-start escapes that pollute stdout
  and break the downstream `grep -q '^apiVersion:'` sanity check. The
  Rust path uses non-TTY SSH (BatchMode=yes is `hbird-ssh`'s default).
  A unit test (`sanity_check_rejects_osc_prefix`) pins the rejection
  shape so a future regression catches it loudly.

Live-validate fixtures:
`crates/hbird-cli/tests/update_cluster/fixtures/live/cycle_{nodes,kubectl,export_argocd,get_kubeconfig}.txt`.
`nodes` + `kubectl` are byte-for-byte parity (empty diff);
`export-argocd` + `get-kubeconfig` carry intentional log-line
divergence (Rust ships the fixed shape; bash retains buggy behavior
until #306/#307 land). See each fixture for the per-cycle annotation.

[#306]: https://github.com/aatchison/hummingbird-k8s/issues/306
[#307]: https://github.com/aatchison/hummingbird-k8s/issues/307

#### Live-validate methodology (template for cycles 2–4)

The canonical pattern is `rust/crates/hbird-cli/tests/update_cluster/fixtures/live/cycle1_drain_uncordon.txt`:

1. Capture the bash twin's behavior by running the equivalent
   `ssh -J $KVM_HOST root@$CP_IP "kubectl ... <cmd>"` directly (the
   operator-side `cp_kubectl` shape) — record stdout + stderr + exit
   code.
2. Restore the cluster to its baseline state (`bootc rollback` on the
   target node for cycles 2+; cycle 1 drain+uncordon is non-destructive
   so no rollback needed).
3. Run the env-gated Rust live test
   (`HBIRD_LIVE_TEST=1 cargo test -p hbird-cli --test <cycle>_live`)
   that exercises the newly-wired helper through `hbird_ssh::Client`.
4. Diff the two captures. Empty diff (modulo timestamps, node AGE
   values, and bootID-after) = block validated. Commit the side-by-side
   capture as the cycle's fixture file.

### Phase 2 (`verify-*`) — landed in PR #287

The four `hbird verify <sub>` subcommands (`encryption`, `hardening`,
`app-deploy`, `all`) replace the bash twins in
`scripts/verify-encryption.sh`, `scripts/verify-hardening.sh`,
`scripts/verify-app-deploy.sh`. Each Rust verifier function carries the
bash twin's grep-anchor name (e.g.
`check_podsecurity_rejects_privileged`, `check_apiserver_audit_log_nonempty`)
to preserve the operator-mental-model contract from the epic.

A new shared module `hbird-cli/src/cp_kubectl.rs` houses the
`cp_kubectl_raw` / `cp_kubectl_with_stdin_lenient` / `cp_ssh_lenient`
helpers — extracted from `commands/update_cluster.rs` so both
update-cluster (Phase 1B) and verify-\* (Phase 2) share the SSH +
shell-metacharacter-defense wiring. update_cluster.rs's existing
`cp_kubectl` keeps its parallel-batch log-prefix threading and isn't
disturbed.

Live-validate fixtures from the geary cluster:

- `rust/crates/hbird-cli/tests/update_cluster/fixtures/live/cycle_verify_encryption.txt`
- `rust/crates/hbird-cli/tests/update_cluster/fixtures/live/cycle_verify_hardening.txt`
- `rust/crates/hbird-cli/tests/update_cluster/fixtures/live/cycle_verify_app_deploy.txt`
- `rust/crates/hbird-cli/tests/update_cluster/fixtures/live/cycle_verify_all.txt`

Notable parity finding: the bash twin's PSA-rejection check (verify-hardening
check 1/3) is masked by a pre-existing stdin-handoff bug in the
`scripts/kubectl-k8s.sh` port-forward wrapper — `kubectl apply -f -`
receives an empty stdin and errors with `error: no objects passed to
apply`, so the bash twin misses the `violates PodSecurity` marker and
marks check 1 FAIL. The Rust path bypasses the wrapper (SSH directly
to root@CP_IP and pipe stdin), so kubectl sees the manifest and the
apiserver correctly rejects it — Rust observes PSA enforcement that
bash misses. Documented in `cycle_verify_hardening.txt`.

#### Still scaffolded (2 of 13 `live_mode_not_implemented` sites)

Phase 1B cycles 1–4 are now complete. Only the scheduled-update timer
helpers remain stubbed; they sit outside the Phase 1B cycle scope
because the geary cluster doesn't run the systemd `hbird-bootc-update`
timer the bash twin's `timer_stop`/`timer_start` pause and resume.

- `timer_stop` / `timer_start` (block #4)

Cycle 2 (#327) wired the bootID gate (`capture_node_bootid`,
`wait_node_bootid_changed`), the bootc upgrade life-cycle
(`bootc_has_apply`, `bootc_booted_digest`, `bootc_upgrade_apply`), and
the SSH drop/back gates (`wait_ssh_drop`, `wait_ssh_back`).

Real virsh-domifaddr IP resolution through `hbird-virt::Connection`
also remains stubbed — operators set `CP_IP=`/`WORKER_IPS=()` in
`cluster.local.conf` to bypass for now.

True parallel-batch concurrency (the scaffold processes the batch
serially under the `[parallel:NAME]` log prefix) is preserved so the
dry-run log shape still matches the bash twin byte-for-byte; full
concurrency lands when block #13 ships.

[#279]: https://github.com/aatchison/hummingbird-k8s/issues/279
[#286]: https://github.com/aatchison/hummingbird-k8s/issues/286
[#288]: https://github.com/aatchison/hummingbird-k8s/issues/288
[#291]: https://github.com/aatchison/hummingbird-k8s/issues/291
[#322]: https://github.com/aatchison/hummingbird-k8s/issues/322
