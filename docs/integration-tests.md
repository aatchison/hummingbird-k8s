# Integration tests

Five integration workflows exercise the published Hummingbird images on a
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

### `integration-boot-k3s.yml` (boot-time CP, k3s flavor)

Same shape as the k8s boot test but for the k3s flavor.

- **Triggers:**
  - `workflow_run` on a successful `Build & publish — hummingbird-k3s`
    (which itself runs on `k3s/v*` tag pushes).
  - `workflow_dispatch`.
- **What it exercises:** bib → qcow2 → virt-install → wait for
  `k3s.service active` → assert exactly one Ready node (via `k3s kubectl`) →
  smoke-deploy a tiny PSA-compliant nginx pod + Service and curl it from an
  in-cluster busybox pod. `verify-hardening.sh` is intentionally NOT run —
  the hardening suite is specific to the kubeadm-based k8s stack.
- **Driver:** `tests/integration-boot-k3s.sh <tag>`.

Manual run:

```bash
gh workflow run integration-boot-k3s.yml -f tag=k3s/v0.1.12
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
(`hummingbird-it-boot-<run_id>`, `hummingbird-it-boot-k3s-<run_id>`,
`hummingbird-it-upgrade-<run_id>`, `hummingbird-it-rollback-<run_id>`,
`hummingbird-it-workers-<run_id>-{cp,wN}`), so parallel runs don't collide
and the long-lived cluster VMs on the host (`hummingbird-k3s`,
`hummingbird-k8s`, `hummingbird-k8s-worker-{1,2}`) are NEVER touched.
A trap-based cleanup always:

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
