# `make deploy-cluster` — hybrid bib + cloud-init orchestrator

`scripts/deploy-cluster.sh` is the operator-facing "deploy a real cluster"
entry point. One config file → one CP + N workers, end-to-end:

```bash
cp cluster.example.conf cluster.local.conf
$EDITOR cluster.local.conf
sudo make deploy-cluster CONFIG=cluster.local.conf
```

It exists alongside `make k8s && make workers` (the dev-iteration path),
but they cover different shapes — see [Differences](#differences-from-make-k8s--make-workers).

## The hybrid model

State is split along a clean seam:

| Carrier | What it holds | Why |
| --- | --- | --- |
| **bib customizations** (baked at qcow2 build time) | Default user account, SSH pubkey set, hardening configs, all image content (kubeadm, cri-o, kubelet, Cilium prep) | Reproducible — the qcow2 digest determines the runtime state. Same image boots the same way every time. |
| **cloud-init NoCloud seed ISO** (attached at virt-install time, persisted on disk) | Per-VM hostname; worker join command; first-boot runcmd (`bootc switch`, enable auto-update timer) | Per-VM dynamic state that can't be baked into a shared image — and shouldn't have to be injected post-build via libguestfs. |

The seed ISO is built on the KVM host (`cloud-localds` or `genisoimage`)
and attached as a CD-ROM via `virt-install --disk ...,device=cdrom`. It
lives in `POOL_DIR` next to the qcow2 so `virsh destroy` + `virsh start`
keeps the same NoCloud datasource.

## Prerequisites

The KVM host that will run the deploy needs:

- `libvirt-daemon-system` running + `qemu:///system` reachable as root
- `libvirt-clients` (`virsh`) + `virt-install` (`virtinst`/`virt-install` package) on `$PATH`
- `podman` (for image pulls from GHCR or local builds)
- `bootc-image-builder` accessible (the bib container image is pulled from `quay.io/centos-bootc/bootc-image-builder`)
- `cloud-localds` (`cloud-utils` package) **or** `genisoimage`/`mkisofs`
  on `$PATH` — used to build the NoCloud seed ISO
- The published GHCR images for the tag you'll pin to (`IMAGE_SOURCE=ghcr`)
  reachable, **or** the ability to run `make image-k8s-with-cloud-init`
  locally (`IMAGE_SOURCE=local`)
- A clean libvirt: no existing domains with the names you're about to
  use (`virsh -c qemu:///system list --all`). The script refuses to
  overwrite an existing domain.
- Outbound network to `ghcr.io`, `quay.io`, and (during first boot for cilium-cli)
  `github.com`


> **Warning — `AUTO_UPDATE_CP=true` on a single-CP cluster.** With this on, the
> CP's `bootc-fetch-apply-updates.timer` will reboot the CP whenever a new
> image lands at the tracked tag. A single-CP cluster has **no apiserver
> availability** during the ~1–2 min reboot window. For production use either
> deploy 3 CPs (see #11) or set `AUTO_UPDATE_CP=false` and run upgrades
> manually during a maintenance window.

## Quickstart

```bash
# 1. Copy the example and edit it.
cp cluster.example.conf cluster.local.conf
$EDITOR cluster.local.conf

# 2. Deploy. CONFIG= is mandatory.
sudo make deploy-cluster CONFIG=cluster.local.conf

# Equivalent direct invocation:
sudo bash scripts/deploy-cluster.sh cluster.local.conf
```

When the script finishes, it prints the CP IP and a one-liner for
`kubectl` access. `make nodes` (which uses `scripts/kubectl-k8s.sh` to
SSH-tunnel the apiserver) works as it does for `make k8s`.

## Config surface

The full set is in `cluster.example.conf`; the essentials:

| Knob | Required? | Default | Purpose |
| --- | --- | --- | --- |
| `CP_NAME` | yes | — | libvirt domain name for the CP. |
| `WORKER_NAMES` | no | `(${CP_NAME}-w1 ${CP_NAME}-w2)` | bash array of worker domain names. |
| `SSH_PUBKEY_FILE` | yes | — | Path to the operator's public key; baked AND in cloud-init. |
| `IMAGE_SOURCE` | yes | — | `ghcr` (pull from registry) or `local` (build from this repo). |
| `GHCR_TAG` | no | `latest` | Tag used for `bootc switch` and for `ghcr` pulls. |
| `ENABLE_CLOUD_INIT` | yes, must be `1` | — | The deploy script refuses to run without it. |
| `AUTO_UPDATE_CP` | no | `true` | Emit a runcmd to enable `bootc-fetch-apply-updates.timer` on the CP. Overrides #48's opt-out. |
| `SWITCH_TO_GHCR` | no | `true` | Emit a `bootc switch ghcr.io/...:$GHCR_TAG` runcmd on first boot. |
| `CP_MEMORY` / `CP_VCPUS` | no | `8192` / `4` | CP sizing. |
| `WORKER_MEMORY` / `WORKER_VCPUS` | no | `4096` / `2` | Per-worker sizing. |
| `POOL_DIR` | no | `/var/lib/libvirt/images` | Where qcow2s + seed ISOs land. |
| `RUN_VERIFY` | no | `false` | Run `scripts/verify-app-deploy.sh` after Ready. |
| `KVM_HOST` | no | unset | Recorded in the summary for downstream `scripts/kubectl-k8s.sh` use. |

When `AUTO_UPDATE_CP=false`, no `systemctl enable` runcmd is emitted —
the YAML stays clean rather than relying on a no-op enable. Same logic
for `SWITCH_TO_GHCR=false`.

## What runs, in order

1. **Validate config.** Hard-fail if `ENABLE_CLOUD_INIT != 1`,
   `SSH_PUBKEY_FILE` missing, `IMAGE_SOURCE` invalid, etc.
2. **Acquire images.** `podman pull` from GHCR (`IMAGE_SOURCE=ghcr`) or
   `make image-k8s-with-cloud-init image-worker-with-cloud-init`
   (`IMAGE_SOURCE=local`).
3. **Build qcow2 templates** via `lib/build-common.sh`'s
   `render_bib_config` + `build_qcow2` — same path `scripts/build-k8s.sh`
   and `scripts/build-worker.sh` use. Output:
   `${POOL_DIR}/hummingbird-k8s-deploy.qcow2` and
   `${POOL_DIR}/hummingbird-k8s-worker-deploy.qcow2`.
4. **Build CP seed.** Emit `#cloud-config` with hostname, SSH key, and
   the conditional runcmd block. Wrap into a NoCloud ISO.
5. **Clone the CP qcow2** (reflink where the FS supports it),
   **virt-install** with the seed as a CD-ROM.
6. **Wait for CP Ready.** Resolve CP IP via `virsh domifaddr`, SSH in as
   root (pubkey baked from `SUDO_USER`'s key), poll `kubectl get nodes`
   until the first node is `Ready`.
7. **Mint join token** on the CP: `kubeadm token create --ttl 2h
   --print-join-command`. Short TTL on purpose — see
   [`docs/worker-tokens.md`](worker-tokens.md).
8. **Per-worker seed.** Emit `#cloud-config` with hostname, SSH key,
   `write_files` for `/etc/hummingbird/worker-join.env`, and the
   `bootc switch` runcmd. Worker-init.service has
   `After=cloud-init.target`, so cloud-final's write_files completes
   before kubeadm join fires.
9. **virt-install workers in parallel.** Each gets its own seed ISO.
10. **Wait for `N+1` nodes Ready.**
11. **Optional verify.** `RUN_VERIFY=true` runs
    `scripts/verify-app-deploy.sh`. Non-zero is informational — the
    cluster is up regardless.
12. **Summary.** Prints CP IP, kubeconfig path, kubectl command.

## What stays bib-baked vs. what's cloud-init'd

| State | Carrier | Notes |
| --- | --- | --- |
| Default user (`core` by default) | bib | `[[customizations.user]]` block, rendered by `lib/build-common.sh`. |
| SSH pubkeys baked into `core@` and (optionally) `root@` | bib | Operator's `$SSH_PUBKEY_FILE` is on both surfaces — cloud-init's `ssh_authorized_keys` is additive, not replacement. |
| SSH hardening (PermitRootLogin prohibit-password, etc.) | bib | Image-level drop-ins. |
| PSA, audit, kubelet protect-kernel-defaults | bib | Static — same on every node. |
| kubeadm, cri-o, kubelet, Cilium prep | bib | The whole point of bootc. |
| Hostname | **cloud-init** | Per-VM; otherwise every VM boots with the same default. |
| `/etc/hummingbird/worker-join.env` | **cloud-init** (`write_files`) | Short-TTL token minted from the live CP. NOT baked. |
| `bootc switch` to GHCR | **cloud-init** (`runcmd`) | So the auto-update timer has a remote ref. |
| `bootc-fetch-apply-updates.timer` enabled on the CP | **cloud-init** (`runcmd`) | Overrides #48's opt-out per `AUTO_UPDATE_CP`. |

## Verifying the deploy

The standard verifiers all work against a deploy-cluster cluster:

```bash
bash scripts/verify-hardening.sh
bash scripts/verify-app-deploy.sh
bash scripts/verify-encryption.sh
make verify-all
```

Set `RUN_VERIFY=true` in `cluster.local.conf` to have
`verify-app-deploy.sh` run automatically once the cluster reaches Ready.

## Tear-down

```bash
# All hummingbird-* VMs at once:
sudo make clean-vms

# Or a specific VM:
sudo virsh -c qemu:///system destroy hbird-cp1
sudo virsh -c qemu:///system undefine hbird-cp1
sudo rm -f /var/lib/libvirt/images/hbird-cp1.qcow2\
           /var/lib/libvirt/images/hbird-cp1-seed.iso
```

The seed ISOs live next to the qcow2s in `POOL_DIR`. They're not picked
up by `make clean-vms` because they're not VMs — clean them by hand if
you want a fully fresh deploy with no NoCloud datasources lying around.

## Updating a deployed cluster

Once a cluster is up, image bumps don't require tearing it down. The
coordinated alternative to the per-VM auto-update timer is
`make update-cluster CONFIG=…`, which walks the cluster one node at a
time with drain/uncordon and bounded waits. It reads the same config
file as the deploy. See [`docs/update-cluster.md`](update-cluster.md)
for the full flag and config reference; the per-VM timer path is
covered in [`docs/auto-updates.md`](auto-updates.md).

## Auto-update behavior

When `SWITCH_TO_GHCR=true` (default), every VM gets a first-boot
`bootc switch ghcr.io/aatchison/hummingbird-<flavor>:$GHCR_TAG` runcmd.
Combined with `AUTO_UPDATE_CP=true` (CP) and the worker image's default
auto-update timer, this means:

- Tag a new release → CI builds + publishes a new `:latest` (or
  `:vX.Y.Z`) digest.
- Each VM's bootc auto-update timer notices, stages the new
  deployment, reboots.
- The cluster rolls forward without operator action.

If you want to pin a deploy to a specific tag, set `GHCR_TAG=vX.Y.Z` in
`cluster.local.conf`. The `bootc switch` runcmd will pin to that tag —
auto-update is per-tag, so it stays on `vX.Y.Z`'s digest stream.

To disable auto-update entirely after deploy:

```bash
ssh root@<vm-ip> systemctl disable --now bootc-fetch-apply-updates.timer
```

## Differences from `make k8s && make workers`

| Concern | `make k8s && make workers` | `make deploy-cluster CONFIG=…` |
| --- | --- | --- |
| Entry point | Two separate make targets | One config file, one command |
| Hostname per VM | All VMs get the bib-baked hostname | Per-VM via cloud-init |
| Worker join token | Injected into the qcow2 via guestfish (mounts ostree deploy dir directly) | Injected via cloud-init `write_files` — no libguestfs OS-introspection workaround |
| Cloud-init needed? | No (default image works) | Yes (`ENABLE_CLOUD_INIT=1` required) |
| `bootc switch` to GHCR | Post-deploy via `scripts/switch-to-ghcr.sh` over SSH | First-boot via cloud-init runcmd |
| Auto-update timer on CP | Off by default (#48); `make redo-k8s` does not enable | `AUTO_UPDATE_CP=true` (default) enables via cloud-init runcmd |
| Worker count | `COUNT=N` env var, names `hummingbird-k8s-worker-{1..N}` | `WORKER_NAMES=(…)` array, operator-chosen names |
| Best for | Dev iteration, single-flavor rebuilds, ad-hoc adds via `make spawn` | Deploying a named cluster to a host that should stay up |

Neither path is going away. `redo-k8s.sh` + `redo-workers.sh` keep
working unchanged; this is an additional surface, not a replacement.

## Troubleshooting

- **Script exits "ENABLE_CLOUD_INIT must be 1".** Set
  `ENABLE_CLOUD_INIT=1` in `cluster.local.conf`. The deploy path
  depends on cloud-init being in the image — there's no fallback.
- **`need one of cloud-localds / genisoimage / mkisofs` on the KVM host.**
  `sudo dnf install -y cloud-utils` or `sudo dnf install -y genisoimage`.
- **`could not resolve CP IP after ~5 minutes`.** The VM didn't get a
  DHCP lease. Open `virsh -c qemu:///system console "$CP_NAME"` and
  look for early-boot errors (likely a missing kernel feature or a
  base-image regression).
- **`CP never reached Ready`.** `ssh root@<cp-ip> journalctl -u
  k8s-init.service` and `journalctl -u kubelet.service`. The most
  common cause is a kubeadm preflight failure on the base image.
- **`cluster never reached N+1 Ready nodes`.** A worker's join may
  have failed. `ssh root@<worker-ip> journalctl -u worker-init.service`
  for the kubeadm join logs. The token has a 2h TTL, so a stale token
  is unusual unless the deploy stalled in the middle.
- **`worker VM 'X' is already defined`.** The script refuses to
  overwrite. Either pick different `WORKER_NAMES` or
  `sudo make clean-vms` first.
- **Seed ISOs left behind after a failed deploy.** The script's exit
  trap removes seed ISOs it created when the deploy fails before
  completion. A successful deploy keeps them so `virsh start` after a
  `virsh destroy` re-reads the same NoCloud datasource.
