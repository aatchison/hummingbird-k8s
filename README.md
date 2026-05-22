[![PR validate](https://github.com/aatchison/hummingbird-k8s/actions/workflows/pr-validate.yml/badge.svg?branch=main)](https://github.com/aatchison/hummingbird-k8s/actions/workflows/pr-validate.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/aatchison/hummingbird-k8s?include_prereleases&sort=semver)](https://github.com/aatchison/hummingbird-k8s/releases)

# hummingbird-k8s

Fedora Hummingbird bootc images with Kubernetes baked in, in three flavors,
run as KVM VMs via libvirt (`qemu:///system`) on a single host. Each flavor is
published as a signed OCI image to GHCR:

- `ghcr.io/aatchison/hummingbird-k3s:vX.Y.Z` — k3s single-binary, single-node.
- `ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z` — upstream `kubeadm`/`kubelet`/`cri-o` control plane.
- `ghcr.io/aatchison/hummingbird-k8s-worker:vX.Y.Z` — worker that auto-joins the CP on first boot.

All three derive from `quay.io/hummingbird-community/bootc-os`, are built with
`podman build`, converted to `qcow2` via `bootc-image-builder`, and defined
under libvirt. Design notes and gotchas live in [`NOTES.md`](NOTES.md); this
README is the operator-facing entry point.

## Topology

A single KVM host runs one of two layouts.

Upstream-k8s, control plane + N workers:

```text
        client laptop
              |
              | ssh -L 6443:127.0.0.1:6443  (via kubectl-k8s.sh)
              v
   +-------------------------------------------------+
   |  KVM host  (qemu:///system, default NAT net)    |
   |                                                 |
   |   +-----------------+     +------------------+  |
   |   | hummingbird-k8s |<----| hummingbird-k8s- |  |
   |   |  (control plane)|     |   worker  x N    |  |
   |   |  192.168.122.X  |     |  192.168.122.Y..Z|  |
   |   +-----------------+     +------------------+  |
   |        virbr0  192.168.122.1/24 (libvirt NAT)   |
   +-------------------------------------------------+
```

k3s single-node:

```text
        client laptop --(ssh)--> KVM host --> hummingbird-k3s VM
```

## Quick start (operator)

On a freshly-set-up KVM host (libvirtd running, `qemu:///system` reachable,
`podman` + `bootc-image-builder` available):

```bash
make help                      # cheatsheet of all targets
sudo make k3s                  # build + define hummingbird-k3s VM
sudo make k8s                  # build + define hummingbird-k8s control plane
sudo make workers COUNT=2      # spawn 2 workers (depends on k8s being up)
make verify-all                # encryption + hardening + app-deploy smoke test
```

The Makefile is the operator entry point — every recipe delegates to a script
under [`scripts/`](scripts/). Run `make help` for the full target list. Direct
script invocations still work (`sudo bash scripts/redo-k8s.sh`); see
[`docs/makefile.md`](docs/makefile.md) for honored env vars.

From a client machine with `KVM_HOST` pointed at the KVM host SSH alias:

```bash
make nodes
# or, for arbitrary kubectl:
make kubectl ARGS='get pods -A'
```

`scripts/kubectl-k8s.sh` opens an SSH tunnel to the CP's apiserver on
`127.0.0.1:6443` and runs `kubectl` in a container against the fetched
admin kubeconfig.

## Configuration

Per-host overrides live in `config.local.sh` (gitignored). Copy
[`config.example.sh`](config.example.sh) to start. Variables are sourced by
the `scripts/build-*.sh` and `scripts/define-vm*.sh` scripts.

| Variable | Default | Controls | Override when |
| --- | --- | --- | --- |
| `VM_USER` | `core` | Initial user account. | Renaming the login user. |
| `VM_USER_GROUPS` | empty | Supplementary groups. | Set `wheel` to allow sudo. |
| `VM_PASSWORD` | empty | Password for `VM_USER`. | Console login; keep empty for key-only. |
| `ENABLE_ROOT_SSH` | `1` | Bake pubkeys into `root@`. | Set `0` to disable root SSH. |
| `SSH_PUBKEY_FILES` | `~/.ssh/id_ed25519.pub` | Pubkey files (colon-sep). | Multiple or non-default keys. |
| `POOL_DIR` | `/var/lib/libvirt/images` | libvirt pool for qcow2. | Larger volume. |
| `BASE_IMAGE` | `quay.io/hummingbird-community/bootc-os@sha256:3bed2fc1…` (digest-pinned) | Upstream bootc base. | Bumping to a newer digest. |
| `BIB` | `quay.io/centos-bootc/bootc-image-builder:latest` | OCI-to-qcow2 builder. | Pinning bib. |
| `KVM_HOST` | unset | SSH alias of KVM host (client-side). | Always on client running `kubectl-k8s.sh`. |
| `APISERVER_EXTRA_SANS` | `127.0.0.1,localhost` | Extra SANs in apiserver cert. | Adding client host/IP for tunnel. |

## Consuming the published images

Existing bootc-based VMs can switch to a published Hummingbird image without
rebuilding locally:

```bash
sudo bootc switch ghcr.io/aatchison/hummingbird-k3s:v0.1.0
sudo systemctl reboot
```

Substitute `hummingbird-k8s` or `hummingbird-k8s-worker` as needed. Images are
signed with cosign (keyless OIDC) at publish time. Verify before switching;
see [`docs/image-verification.md`](docs/image-verification.md) for the
`cosign verify` command and the expected issuer/identity.

## Release process

GitHub Actions builds and publishes one flavor per tag:

| Tag pattern | Workflow | Published image |
| --- | --- | --- |
| `k3s/vX.Y.Z` | `build-k3s.yml` | `ghcr.io/aatchison/hummingbird-k3s:vX.Y.Z` |
| `k8s/vX.Y.Z` | `build-k8s.yml` | `ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z` |
| `worker/vX.Y.Z` | `build-worker.yml` | `ghcr.io/aatchison/hummingbird-k8s-worker:vX.Y.Z` |

Flavors version independently:

```bash
git tag k3s/v0.2.0
git push --tags
```

The publish job refuses to run if the tagged commit isn't reachable from
`main`, so accidental tags on a topic branch will not produce a release.

## Operations

Day-2 documentation lives under [`docs/`](docs):

- [`docs/makefile.md`](docs/makefile.md) — `make` cheatsheet over the underlying driver scripts.
- [`docs/image-verification.md`](docs/image-verification.md) — verify GHCR images with cosign.
- [`docs/etcd-encryption.md`](docs/etcd-encryption.md) — enable encryption-at-rest for etcd.
- [`docs/worker-tokens.md`](docs/worker-tokens.md) — short-TTL, per-VM kubeadm join tokens.
- [`docs/self-hosted-runner.md`](docs/self-hosted-runner.md) — register a KVM-capable GitHub Actions runner.
- [`docs/auto-updates.md`](docs/auto-updates.md) — bootc auto-update timer behavior.
- [`docs/security-hardening.md`](docs/security-hardening.md) — PodSecurity restricted + apiserver audit + kubelet protect-kernel-defaults; run `make verify-hardening` (or `scripts/verify-hardening.sh`) after each redeploy.
- [`docs/app-deploy-verify.md`](docs/app-deploy-verify.md) — end-to-end smoke test of a PSA-restricted nginx deploy + pod-to-pod networking.
- [`docs/cilium-migration.md`](docs/cilium-migration.md) — Cilium CNI (NetworkPolicy enforcement, eBPF datapath).

Workflows that need real KVM (orchestrator integration, bootc upgrade e2e)
run on a self-hosted runner on the operator's KVM host.

## Repo layout

```
containers/<flavor>/   per-flavor Containerfile + first-boot scripts
                       (k3s, k8s, k8s-worker). Shared in-image assets
                       under containers/shared/.
scripts/               every driver script (build, define, redo, spawn,
                       kubectl wrapper, verifiers, kube-bench).
lib/                   build-common.sh — sourced by scripts/build-*.sh.
docs/                  day-2 docs (image verification, hardening, etc.).
references/            external materials referenced by docs.
Makefile               canonical operator entry point; all targets call
                       scripts/. `make help` lists them.
```

## License

Licensed under the Apache License, Version 2.0. See [`LICENSE`](LICENSE) and
[`NOTICE`](NOTICE).
