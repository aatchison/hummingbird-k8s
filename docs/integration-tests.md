# Tests

Two layers of automated tests cover the project:

1. **Unit tests** for `lib/build-common.sh` helpers (bats-core, fast, run on
   every PR on the standard `ubuntu-latest` runner — see "Unit tests" below).
2. **Integration tests** that boot real VMs from published images on a
   self-hosted KVM host (see "Integration tests" below).

## Unit tests

`tests/lib/build-common.bats` covers the pure-Bash helpers in
`lib/build-common.sh`:

- `ssh_pubkey_blob` — file-only / GitHub-only / empty / unreadable / dedup.
- `ssh_pubkeys_from_github` — empty input, comma-separated parsing, trailing
  whitespace/comma tolerance. `curl` is stubbed as a shell function so the
  tests never reach github.com.
- `_render_user_block` — TOML field rendering, multi-group arrays, the
  `name=root` branch that suppresses `groups`.
- `render_bib_config` — orchestration only (one vs. two `[[customizations.user]]`
  blocks). The full TOML snapshot is handled by `render-bib-config-snapshot`
  in `pr-validate.yml`, so these tests deliberately don't duplicate it.

`require_root` and `build_qcow2` are intentionally not unit-tested — they
require root + podman + libvirt, which is the integration suite's job.

Run locally:

```bash
make test-lib
```

The same invocation runs in CI as the `unit-tests-lib` job in
`.github/workflows/pr-validate.yml`. The bats container is pinned by digest so
upstream tag movement can't silently change the test runtime.

### `tests/scripts/` — script-level bats tests (issue #189)

A second bats suite under `tests/scripts/` covers the operator-facing
scripts that aren't pure helpers — flag parsing, dry-run sequencing, YAML
rewrite roundtrips:

- `tests/scripts/update-cluster.bats` — `--workers-only` + `--node=`
  mutual-exclusion, bare `--node` rejection, unknown-arg handling, `--help`
  formatting, and `--dry-run` sequencing. The dry-run cases deliberately
  codify the patterns that `integration-update-cluster.yml`'s
  `assert-dry-run-sequence` command keys on — so a refactor that subtly
  changes log lines is caught at pr-validate time, not only on the
  self-hosted runner.
- `tests/scripts/export-argocd.bats` — exercises `rewrite_kubeconfig()`
  (the YAML rewrite primitive shared by `export-argocd` and
  `get-kubeconfig`) against `tests/fixtures/admin.conf`. Asserts that the
  kubeadm-default key lines (cluster/context/user/current-context) are
  rewritten while non-key lines — YAML comments and base64 PEM blobs that
  happen to contain the literal substring `kubernetes` — survive
  byte-identical. Both the Go-`yq` path (skipped when `yq` is absent) and
  the line-anchored `sed` fallback are exercised. Also covers `--server`
  URL validation (newline / quote / command-substitution rejected) and
  `--force` refuse-to-clobber.

`scripts/export-argocd.sh` is structured so the rewrite logic is callable
when the script is sourced (sourced-mode short-circuit near the top
returns 0 before any SSH activity), which is what lets the bats suite
unit-test the YAML rewrite without a real cluster.

Run locally:

```bash
make test-scripts        # scripts/ suite only
make test-all            # lib/ + scripts/ together
```

The same invocations run in CI as the `unit-tests-scripts` and (existing)
`unit-tests-lib` jobs in `.github/workflows/pr-validate.yml`.

## Integration tests

Seven integration workflows exercise the published Hummingbird images on a
real KVM host. All run on the self-hosted runner gated by the `kvm,libvirt`
labels (see `docs/self-hosted-runner.md`).

## Workflows

### `integration-boot.yml` (#32 — boot-time CP, k8s flavor)

Verifies that a freshly-built `hummingbird-k8s` image actually boots into a
single-node control plane and that the cluster's networking + workload
posture is healthy.

- **Triggers:**
  - `workflow_run` on a successful `Build & publish — hummingbird-k8s`
    (which itself runs on `k8s/v*` tag pushes) — auto-verifies every
    release.
  - `workflow_dispatch` — for ad-hoc runs against any tag or `latest`.
- **What it exercises:** bib → qcow2 → virt-install → wait for
  `/var/lib/k8s-init.done` → assert exactly one Ready node → run
  `scripts/verify-hardening.sh` (PodSecurity restricted, apiserver audit log,
  kubelet `--protect-kernel-defaults=true`) → NetworkPolicy enforcement
  (deny-all blocks, removal restores) → `scripts/verify-app-deploy.sh`
  (nginx Deployment + Service + PSA-restricted probe).
- **Driver:** `tests/integration-boot.sh <tag>`.

Manual run:

```bash
gh workflow run integration-boot.yml -f tag=v0.1.33
```

### `integration-workers-join.yml` (worker join flow)

Stands up a CP and N worker VMs from the published worker template, with
the kubeadm join command injected via guestfish raw-partition mount (the
same approach `scripts/spawn-workers.sh` uses — libguestfs OS introspection
breaks on the bootc/ostree layout). Asserts `kubectl get nodes` ends up
showing `worker_count + 1` Ready nodes.

- **Triggers:** `workflow_dispatch` only (heavier; ~30 min wall-time).
- **Inputs:** `cp_tag` (default `v0.1.33`), `worker_tag` (default `v0.1.9`),
  `worker_count` (default `2`).
- **What it exercises:** bib build of CP + worker images → CP boot → mint
  a 2h kubeadm join token → bib build of worker template → for each
  worker: cp --reflink the template, inject `worker-join.env` into the
  active ostree deployment dir via guestfish, virt-install (parallel) →
  wait for N+1 Ready nodes.
- **Driver:** `tests/integration-workers-join.sh <cp_tag> <worker_tag> <count>`.

Manual run:

```bash
gh workflow run integration-workers-join.yml \
  -f cp_tag=v0.1.33 -f worker_tag=v0.1.9 -f worker_count=2
```

### `integration-bootc-upgrade.yml` (#12 — end-to-end bootc upgrade)

Stands up a CP on a starting tag, then exercises a real `bootc switch` +
`bootc upgrade` + reboot to a target tag.

- **Triggers:** `workflow_dispatch` only (slower; manual gate).
- **Inputs:** `from_tag`, `to_tag` (without the `k8s/` prefix; e.g. `v0.1.9`).
- **What it exercises:** boot on `from_tag` → capture pre-upgrade
  `bootc status` + kubelet version → `bootc switch ${to_tag}` → `bootc
  upgrade` → `systemctl reboot` → wait for VM to come back → assert `bootc
  status` shows the new image, `/var/lib/k8s-init.done` still present,
  kubelet active, node Ready.
- **Driver:** `tests/integration-bootc-upgrade.sh <from_tag> <to_tag>`.

Manual run:

```bash
gh workflow run integration-bootc-upgrade.yml \
  -f from_tag=v0.1.9 -f to_tag=v0.1.10
```

### `integration-bootc-rollback.yml` (#100 — end-to-end bootc rollback)

Same shape as the upgrade workflow but adds a rollback leg: after the VM
has come back on `to_tag`, the driver runs `bootc rollback` + reboot and
asserts the VM lands back on `from_tag` with the cluster still healthy.

- **Triggers:** `workflow_dispatch` only.
- **Inputs:** `from_tag`, `to_tag`.
- **What it exercises:** boot on `from_tag` → `bootc switch ${to_tag}` +
  `bootc upgrade` + reboot → assert on `to_tag` → `bootc rollback` +
  reboot → assert back on `from_tag`, `/var/lib/k8s-init.done` still
  present, kubelet active, node Ready.
- **Driver:** `tests/integration-bootc-rollback.sh <from_tag> <to_tag>`.

Manual run:

```bash
gh workflow run integration-bootc-rollback.yml \
  -f from_tag=v0.1.11 -f to_tag=v0.1.12
```

See `docs/rollback.md` for the auto-rollback behaviour built on top of
this path.

### `integration-update-cluster.yml` (#196 — rolling cluster update)

End-to-end exercise of `scripts/update-cluster.sh` (PR #187) against a real
1-CP + 2-worker cluster. The headline assertion is that PR #187's
`wait_node_ready` regex fix (`$2 ~ /^Ready(,|$)/`) actually keeps working:
the script must successfully wait through a worker rejoining as
`Ready,SchedulingDisabled` post-reboot and then uncordon it.

- **Triggers:**
  - `workflow_run` on a successful `Build & publish — hummingbird-k8s`
    restricted to `k8s/v*` tag refs (auto-runs against each release).
  - `workflow_dispatch` — for ad-hoc manual replays.
- **What it exercises:** deploy a CP + 2 workers via `make deploy-cluster`
  → assert all three are Ready by name (not just by count) → drive
  `scripts/update-cluster.sh --dry-run` and validate the expected per-target
  log lines (CP timer-stop, drain, uncordon, etc.) appear with the expected
  multiplicity → install a `/usr/local/bin/bootc` shim on one worker to
  force the real reboot codepath (the no-update short-circuit would
  otherwise bypass `wait_node_ready` entirely) → run
  `make update-node NODE=<worker>` and assert the worker reached
  `Ready,SchedulingDisabled` and was uncordoned → SIGINT a second
  `make update-node` mid-flight (gated on `.spec.unschedulable=true`,
  proving the drain actually completed) and assert
  `cleanup_on_exit`'s recovery banner surfaces the manual uncordon
  command. Failure artifacts (virsh dumpxml, journalctl, kubectl state)
  are uploaded on failure.
- **Driver:** `tests/integration-update-cluster.sh` (sub-commands:
  `assert-dry-run-sequence`, `force-worker-upgrade`,
  `unforce-worker-upgrade`, `assert-update-node-real`, `assert-exit-trap`).
- **Fixture:** `tests/fixtures/cluster.ci.conf` (DO NOT copy this for a
  real cluster — it carries CI-only sizing and skips the verify suite).
  Names are rewritten per-run with `${run_id}-${run_attempt}` suffixes to
  avoid libvirt-domain collisions across concurrent runs.

Manual run:

```bash
gh workflow run integration-update-cluster.yml
```

### `integration-export-argocd.yml` (#196 — export-argocd kubeconfig)

End-to-end exercise of `scripts/export-argocd.sh` (PR #188) against the
same 1-CP + 2-worker fixture. Validates the full operator-facing surface
of `make export-argocd`.

- **Triggers:** identical to `integration-update-cluster.yml` —
  `workflow_run` on a successful k8s/v* build, or `workflow_dispatch`.
- **What it exercises (T1–T6):**
  - **T1 basic export** — `make export-argocd OUTPUT=…` emits a 0600
    kubeconfig with rewritten `server:` URL and `hummingbird-<CP_NAME>`
    context.
  - **T2 usable kubeconfig** — `KUBECONFIG=…  kubectl get nodes` returns
    `CP_NAME Ready` via the rewritten apiserver URL.
  - **T3 refuse-to-clobber** — a second invocation without `FORCE=1`
    exits non-zero and surfaces `already exists` / `--force`.
  - **T4 force overwrite** — `FORCE=1` advances mtime cleanly.
  - **T5 hostile --server** — calls `scripts/export-argocd.sh` directly
    (NOT via `make`, because Make's recipe substitutes `$(SERVER)`
    literally into the shell — a payload containing `";rm -rf /;\n` would
    run on the runner BEFORE the script's regex validator saw it). The
    test asserts the script rejects the payload, never produces the
    output file, and surfaces `invalid --server`.
  - **T6 get-kubeconfig sibling** — gated on PR #197's `make
    get-kubeconfig` target landing. Verifies the sibling target uses
    the bare `CP_NAME` context (no `hummingbird-` prefix) and is
    similarly usable.
- **Driver:** `tests/integration-export-argocd.sh` (sub-commands:
  `verify-nodes-by-name`, `test-basic-export`, `test-kubeconfig-usable`,
  `test-refuse-clobber`, `test-force-overwrite`, `test-hostile-server`,
  `test-get-kubeconfig`).
- **Fixture:** `tests/fixtures/cluster.ci.conf` (shared with the
  update-cluster workflow). The mikefarah/yq Go variant is installed by
  release binary so the canonical yq-driven rewrite path is exercised
  (the script's sed fallback is exercised in unit tests).

Manual run:

```bash
gh workflow run integration-export-argocd.yml
```

## What the runner needs

Beyond the baseline in `docs/self-hosted-runner.md`:

- `podman` (build + pull images).
- `virt-install`, `virsh` (libvirt CLI), `qemu-kvm` working under
  `qemu:///system`.
- `ssh`, `ssh-keygen`, `scp`.
- Passwordless `sudo` for the runner user (both drivers are invoked under
  `sudo` because bib needs loopback mounts and libvirt's system URI needs
  root).
- A writable libvirt storage pool dir; defaults to `/var/lib/libvirt/images`
  (override with `LIBVIRT_POOL_DIR`).
- `cilium-cli` is **not** required on the runner — it's baked into the image
  and only runs inside the VM as part of `k8s-init`.

## Teardown + isolation

All drivers use a unique VM name keyed on `GITHUB_RUN_ID`
(`hummingbird-it-boot-<run_id>`, `hummingbird-it-upgrade-<run_id>`,
`hummingbird-it-rollback-<run_id>`,
`hummingbird-it-workers-<run_id>-{cp,wN}`), so parallel runs don't collide
and the long-lived cluster VMs on the host (`hummingbird-k8s`,
`hummingbird-k8s-worker-{1,2}` — or whatever names the operator picks via
`cluster.local.conf`) are NEVER touched.
A trap-based cleanup always:

- `virsh destroy` + `virsh undefine --nvram`
- removes the per-test qcow2 from the libvirt pool
- removes the `mktemp -d` working dir (which holds the ephemeral SSH key
  and bib config)

On failure the trap also prints the VM name, last-known IP, and the last 30
lines of `journalctl -u k8s-init` from the VM before tearing down — these
land in the GitHub Actions log for the run.

`tests/integration-boot.sh` additionally captures the VM's serial console
PTY into `/var/log/libvirt/qemu/<VM_NAME>-console.log` (#224), and prints
the last 200 lines of it on failure. The same file is uploaded as an
`integration-boot-serial-<run_id>` workflow artifact by
`.github/workflows/integration-boot.yml` so an SSH-never-up failure
(timeout with no journal access) still has a recoverable boot log. The
PTY tee is best-effort and routinely no-ops on the geary-docker runner
because `/dev/pts/` lives in qemu's host mount namespace rather than the
runner container's — when that happens the test log says so and the
SSH-based diagnostics remain the only window into the VM.

The in-script trap is best-effort: when GitHub cancels a runner or the
runner container is OOM-killed, the bash process is terminated before
the EXIT trap can run, leaving the VM + qcow2 on the host (issue #162).
Each integration workflow therefore also has an `if: always()`
**Force-clean integration VMs** step that runs after the test step on
every outcome (success, failure, cancel). It greps `virsh list --all`
for the `hummingbird-it-` prefix only — production cluster VMs are
never matched — and is fully defensive (`|| true` on every command), so
it can never itself fail the job.

### Podman storage isolation (issue #199)

`integration-update-cluster.yml` and `integration-export-argocd.yml`
declare a per-run podman graphroot:

```yaml
env:
  STORAGE_DRIVER: vfs
  PODMAN_ROOT: /var/lib/integration-storage/${{ github.run_id }}-${{ github.run_attempt }}
  PODMAN_RUNROOT: /run/integration-storage/${{ github.run_id }}-${{ github.run_attempt }}
```

These vars are consumed end-to-end by the build path
(`lib/build-common.sh:build_qcow2()` plus the consumer-side
`podman pull` / `podman build` in `scripts/build-*.sh` and
`scripts/deploy-cluster.sh`, all via the shared `podman_storage_opts`
helper). They translate to top-level `podman --root / --runroot /
--storage-driver` flags on every podman invocation in the build path.
Inside `build_qcow2`, the
`-v $PODMAN_ROOT:/var/lib/containers/storage` bind-mount is what
gives the nested podman that BIB spawns its isolated graph view —
podman does not honor `PODMAN_ROOT` / `PODMAN_RUNROOT` as env-var
names natively, so the bind-mount remap is the real mechanism for
the nested podman; `-e STORAGE_DRIVER` is forwarded so the nested
podman picks the same driver.

The path includes `run_attempt` (not just `run_id`) because re-runs
of a workflow share `run_id` but get distinct `run_attempt` values —
without that suffix a re-run would collide with the prior attempt's
graphroot on the runner.

Two concurrent integration runs on the same self-hosted host
therefore cannot corrupt each other's overlay graph: each lives
under its own `/var/lib/integration-storage/<run_id>-<run_attempt>`
graphroot.

Before #199 the three env vars were declared in the workflow but never
consumed by the build path (only `tests/integration-cloud-init.sh` and
siblings read them), so the "isolated storage" claim was misleading —
the `concurrency:` block was what actually prevented collisions. After
#199 the isolation is real, and `concurrency:` stays as belt-and-
suspenders against libvirt VM-name + `/var/lib/libvirt/images` pool
collisions, which are still NOT per-run-isolated.

Disk-usage note: the vfs storage driver does not deduplicate layers
across images (each layer is a full copy under `$PODMAN_ROOT`), so an
isolated CI graphroot for one run is roughly `N × image_size` where N
is the layer count of the bootc base + flavor images. Expect ~5-10 GB
per run on the self-hosted runner. Old graphroots are removed at the
end of each job by the "Clean up isolated podman storage" step; on a
runner crash that step may not execute and `/var/lib/integration-
storage/` can accumulate stale dirs — manual `find /var/lib/
integration-storage -maxdepth 1 -mtime +1 -exec rm -rf {} \;` is the
maintenance command. (A scheduled cleanup workflow is tracked as a
follow-up.)

See `docs/development.md` ("Podman storage isolation") for the
underlying contract.

## Security note — fork PRs

None of these workflows run on `pull_request`, so fork PRs can't reach the
self-hosted runner. The `if:` guard in each workflow keeps that property
explicit if someone later adds a PR trigger. See the "Security caveat" in
`docs/self-hosted-runner.md`.
