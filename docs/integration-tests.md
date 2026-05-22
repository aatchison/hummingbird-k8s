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

## Integration tests

Two integration workflows exercise the published `hummingbird-k8s` image on a
real KVM host. Both run on the self-hosted runner gated by the `kvm,libvirt`
labels (see `docs/self-hosted-runner.md`).

## Workflows

### `integration-boot.yml` (#32 — boot-time CP)

Verifies that a freshly-built image actually boots into a single-node
control plane.

- **Triggers:**
  - `push` to tags matching `k8s/v*` — auto-verifies every release.
  - `workflow_dispatch` — for ad-hoc runs against any tag or `latest`.
- **What it exercises:** bib → qcow2 → virt-install → wait for
  `/var/lib/k8s-init.done` → assert exactly one Ready node → run
  `scripts/verify-hardening.sh` (PodSecurity restricted, apiserver audit log,
  kubelet `--protect-kernel-defaults=true`).
- **Driver:** `tests/integration-boot.sh <tag>`.

Manual run:

```bash
gh workflow run integration-boot.yml -f tag=v0.1.10
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
(`hummingbird-it-boot-<run_id>` / `hummingbird-it-upgrade-<run_id>` /
`hummingbird-it-rollback-<run_id>`), so parallel runs don't collide. A
trap-based cleanup always:

- `virsh destroy` + `virsh undefine --nvram`
- removes the per-test qcow2 from the libvirt pool
- removes the `mktemp -d` working dir (which holds the ephemeral SSH key
  and bib config)

On failure the trap also prints the VM name, last-known IP, and the last 30
lines of `journalctl -u k8s-init` from the VM before tearing down — these
land in the GitHub Actions log for the run.

## Security note — fork PRs

None of these workflows run on `pull_request`, so fork PRs can't reach the
self-hosted runner. The `if:` guard in each workflow keeps that property
explicit if someone later adds a PR trigger. See the "Security caveat" in
`docs/self-hosted-runner.md`.
