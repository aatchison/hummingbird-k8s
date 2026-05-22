# Integration tests

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

Both drivers use a unique VM name keyed on `GITHUB_RUN_ID`
(`hummingbird-it-boot-<run_id>` / `hummingbird-it-upgrade-<run_id>`), so
parallel runs don't collide. A trap-based cleanup always:

- `virsh destroy` + `virsh undefine --nvram`
- removes the per-test qcow2 from the libvirt pool
- removes the `mktemp -d` working dir (which holds the ephemeral SSH key
  and bib config)

On failure the trap also prints the VM name, last-known IP, and the last 30
lines of `journalctl -u k8s-init` from the VM before tearing down — these
land in the GitHub Actions log for the run.

## Security note — fork PRs

Neither workflow runs on `pull_request`, so fork PRs can't reach the
self-hosted runner. The `if:` guard in each workflow keeps that property
explicit if someone later adds a PR trigger. See the "Security caveat" in
`docs/self-hosted-runner.md`.
