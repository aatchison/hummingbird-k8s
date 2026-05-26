# `make deploy-cluster` — hybrid bib + cloud-init orchestrator

`scripts/deploy-cluster.sh` is the operator-facing "deploy a real cluster"
entry point. One config file → one CP + N workers, end-to-end:

```bash
cp cluster.example.conf cluster.local.conf
$EDITOR cluster.local.conf
make deploy-cluster CONFIG=cluster.local.conf
```

This is the only supported way to stand up a cluster after #216 retired
the legacy single-VM `make k8s` / `make workers` / `make spawn` targets.

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
- `bootc-image-builder` accessible (the bib container image is pulled from `quay.io/centos-bootc/bootc-image-builder`) — only needed if you set `IMAGE_SOURCE=local`
- `cloud-localds` (`cloud-utils` package) **or** `genisoimage`/`mkisofs`
  on `$PATH` — used to build the NoCloud seed ISO
- The published GHCR images for the tag you'll pin to (default
  `IMAGE_SOURCE=ghcr`) reachable, **or** the ability to run
  `make image-k8s-with-cloud-init` locally (`IMAGE_SOURCE=local`, the
  fast-iteration path for testing an unpublished image)
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

# 2. Deploy. CONFIG= is mandatory. No `sudo` required (issue #233);
#    the script re-execs on $KVM_HOST via SSH when set, or probes for
#    local root and prints a hint if neither path is available.
make deploy-cluster CONFIG=cluster.local.conf

# Equivalent direct invocation (the script handles privilege escalation
# itself — see docs/deploy-cluster.md for the three no-op paths):
sudo bash scripts/deploy-cluster.sh cluster.local.conf
```

> **Local fallback path with custom podman storage.** If you're
> running `make deploy-cluster` directly on the KVM host (not the
> remote-SSH path) and you've overridden `STORAGE_DRIVER`,
> `PODMAN_ROOT`, or `PODMAN_RUNROOT`, prefix `sudo --preserve-env`
> explicitly so those vars survive into the script:
>
> ```bash
> sudo --preserve-env=STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT,HOME \
>     make deploy-cluster CONFIG=cluster.local.conf
> ```
>
> Before #233 the Makefile carried these for you (the recipe ran
> `sudo --preserve-env=…`). Now that `sudo` is out of the recipe, the
> Makefile can't carry them for you anymore — it's the operator's job
> at the outer-`sudo` boundary. The remote-SSH path forwards env vars
> via `scripts/lib/ssh-wrap.sh`'s allowlist and is unaffected. Same
> applies to `destroy-cluster` / `update-cluster` / `update-workers` /
> `update-node`.

When the script finishes, it prints the CP IP and a one-liner for
`kubectl` access. `make nodes` (which uses `scripts/kubectl-k8s.sh` to
SSH-tunnel the apiserver) works against the deployed CP.

## Remote KVM-host operation (`KVM_HOST=`)

Since C3 (#232), `deploy-cluster.sh`, `destroy-cluster.sh`,
`update-cluster.sh`, and `spawn-workers.sh` self-host on the KVM host
via SSH when `KVM_HOST` is set and the local short hostname doesn't
match `${KVM_HOST%%.*}`. The client never needs `sudo` or `libvirt`
installed — only `ssh` + the operator's SSH key. Sudo happens on the
remote.

### One-time remote setup

The shim assumes a sibling checkout of hummingbird-k8s already exists
on the KVM host at `$HBIRD_REMOTE_REPO` (default `~/hummingbird-k8s`).
Operator does the one-time clone:

```bash
ssh $KVM_HOST 'git clone https://github.com/aatchison/hummingbird-k8s ~/hummingbird-k8s'
```

The shim intentionally does **not** auto-clone — the operator
decides which branch/ref the remote tracks. If the directory is
missing the shim exits with a clear remediation hint pointing at the
clone URL.

### Usage

```bash
# From a laptop. KVM_HOST should be an SSH alias (~/.ssh/config) that
# can reach the operator's libvirt host as a user with sudo. The shim
# ssh -t's into KVM_HOST, runs `cd $HBIRD_REMOTE_REPO`, then sudo
# env=...allowlist... bash $remote_script, executing the script from
# disk on the remote (NOT streamed via stdin — that pattern was
# incompatible with the wrapped scripts' source-lookup logic).
export KVM_HOST=geary
make deploy-cluster CONFIG=cluster.local.conf
```

The shim is a no-op when `KVM_HOST` is unset, when the local host short
name matches `${KVM_HOST%%.*}` (operator already on the KVM host), or
when the script body is being re-executed on the remote side
(`HBIRD_REMOTE_REEXEC=1` sentinel).

`KVM_HOST` is expected to be an SSH alias (`~/.ssh/config` entry) or a
hostname whose first label matches the local hostname when you're
already on the KVM host. Bare IP literals and unrelated FQDNs are
accepted but won't trip the "already local" guard correctly — use
`~/.ssh/config` to define a stable alias.

NOPASSWD sudo on the remote is RECOMMENDED for unattended runs, but
not mandatory — with the round-2 checkout-on-remote model, sudo can
prompt normally on the SSH TTY.

### Pre-flight checks

Before the re-exec, the shim verifies (in order):

1. SSH reachability (`ssh -o BatchMode=yes -o ConnectTimeout=5
   $KVM_HOST true`). Fails fast with "cannot reach KVM_HOST=… via
   SSH — check ~/.ssh/config + key auth" if the host is wrong, the
   key isn't loaded, or the network is down.
2. Remote checkout existence (`ssh $KVM_HOST 'test -d
   $HBIRD_REMOTE_REPO/scripts'`). Fails with a one-liner remediation
   hint pointing at the clone URL when the checkout is missing.

Skip both checks (test/CI use only) with
`HBIRD_SSH_WRAP_DRY_RUN_PREFLIGHT=1`.

### Env-var passthrough

Env-var passthrough is an **explicit allowlist** maintained in
`scripts/lib/ssh-wrap.sh` and pinned by `tests/scripts/ssh-wrap.bats`.
Opaque forwarding was rejected as a footgun: an operator's local
`AWS_*` or `PROXY_*` exports must not silently change remote behavior.
Adding a new tunable to the four wrapped scripts requires a
corresponding entry in `HBIRD_SSH_WRAP_ALLOWED_ENV` (and the test
will fail until you add it).

Current allowlist:

| Group | Vars |
| --- | --- |
| Config + flags | `CONFIG`, `FLAGS` |
| Image + auto-update | `IMAGE_SOURCE`, `GHCR_TAG`, `AUTO_UPDATE_CP`, `SWITCH_TO_GHCR`, `BOOTC_UPDATE_SCHEDULE`, `BOOTC_UPDATE_REPO_K8S`, `BOOTC_UPDATE_REPO_WORKER` |
| Update-cluster flags | `DRY_RUN`, `SKIP_DRAIN`, `WORKERS_ONLY`, `NODE`, `START_FROM`, `PARALLEL` |
| Timeouts | `READY_TIMEOUT`, `DRAIN_TIMEOUT`, `APISERVER_TIMEOUT`, `SSH_TIMEOUT`, `INTER_NODE_SLEEP`, `DAEMONSET_TIMEOUT` |
| Topology | `CP_NAME`, `WORKER_NAMES`, `POOL_DIR` |
| Image-build knobs | `VM_USER`, `STORAGE_DRIVER`, `PODMAN_ROOT`, `PODMAN_RUNROOT`, `APISERVER_EXTRA_SANS` |
| Shim itself | `HBIRD_AUTOLOAD_CONFIG_LOCAL`, `HBIRD_REMOTE_REPO` |

Every value is passed through `printf %q` before being interpolated
into the remote command, so values with spaces, quotes, or shell
metas reach the remote unmangled.

`CONFIG=<local-path>` is special-cased: the local config file is
`scp`'d to a remote `mktemp -d` and the remote `CONFIG=` is rewritten
to point at the copy before the script runs.

### `HBIRD_REMOTE_REPO` override

| Var | Default | Effect |
| --- | --- | --- |
| `HBIRD_REMOTE_REPO` | `~/hummingbird-k8s` | Absolute path on `$KVM_HOST` to the remote git checkout the shim execs from. Override when you keep the repo somewhere non-standard (e.g. `/opt/hummingbird-k8s` or `/srv/repos/hummingbird-k8s`). Tilde expansion happens on the remote shell's word-splitting pass — start with `/` or `~/` only. |

## Config surface

The full set is in `cluster.example.conf`; the essentials:

| Knob | Required? | Default | Purpose |
| --- | --- | --- | --- |
| `CP_NAME` | yes | — | libvirt domain name for the CP. |
| `WORKER_NAMES` | no | `(${CP_NAME}-w1 ${CP_NAME}-w2)` | bash array of worker domain names. |
| `SSH_PUBKEY_FILE` | yes | — | Path to the operator's public key; baked AND in cloud-init. |
| `IMAGE_SOURCE` | no | `ghcr` | `ghcr` (pull from registry — the registry-first golden path) or `local` (build from this repo — power-user / fast iteration). |
| `GHCR_TAG` | no | `latest` | Tag used for `bootc switch` and for `ghcr` pulls. |
| `ENABLE_CLOUD_INIT` | yes, must be `1` | — | The deploy script refuses to run without it. |
| `AUTO_UPDATE_CP` | no | `true` | Emit a runcmd to enable `bootc-fetch-apply-updates.timer` on the CP. Overrides #48's opt-out. |
| `SWITCH_TO_GHCR` | no | `true` | Emit a `bootc switch ghcr.io/...:$GHCR_TAG` runcmd on first boot. |
| `CP_MEMORY` / `CP_VCPUS` | no | `8192` / `4` | CP sizing. |
| `WORKER_MEMORY` / `WORKER_VCPUS` | no | `4096` / `2` | Per-worker sizing. |
| `POOL_DIR` | no | `/var/lib/libvirt/images` | Where qcow2s + seed ISOs land. |
| `RUN_VERIFY` | no | `false` | Run `scripts/verify-app-deploy.sh` after Ready. |
| `KVM_HOST` | no | unset | Recorded in the summary for downstream `scripts/kubectl-k8s.sh` use. |
| `BOOTC_UPDATE_SCHEDULE` | no | unset (use image default) | Override the `bootc-semver-update.timer` `OnCalendar=` on every node. Any systemd `OnCalendar=` value. See [Customizing auto-update](#customizing-auto-update). |
| `BOOTC_UPDATE_REPO_K8S` | no | unset (use image-baked default) | OCI ref without tag — overrides the CP's tracked semver-update repo (e.g. point at a fork). |
| `BOOTC_UPDATE_REPO_WORKER` | no | unset (use image-baked default) | OCI ref without tag — overrides workers' tracked semver-update repo. |

When `AUTO_UPDATE_CP=false`, no `systemctl enable` runcmd is emitted —
the YAML stays clean rather than relying on a no-op enable. Same logic
for `SWITCH_TO_GHCR=false`.

## What runs, in order

1. **Validate config.** Hard-fail if `ENABLE_CLOUD_INIT != 1`,
   `SSH_PUBKEY_FILE` missing, `IMAGE_SOURCE` invalid, etc. Defaults
   fill in for the optional knobs — notably `IMAGE_SOURCE` falls
   through to `ghcr` when unset (a one-line log notice records the
   fall-through so the operator sees what was resolved).
2. **Acquire images.** `podman pull` from GHCR (default
   `IMAGE_SOURCE=ghcr` — the registry-first golden path) or
   `make image-k8s-with-cloud-init image-worker-with-cloud-init`
   (`IMAGE_SOURCE=local`, for testing an unpublished image).
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
| `bootc-semver-update.timer` schedule override | **cloud-init** (`write_files` + `runcmd` reload) | Per `BOOTC_UPDATE_SCHEDULE`; only when set. See [Customizing auto-update](#customizing-auto-update). |
| `/etc/hummingbird/bootc-update.env` (REPO override) | **cloud-init** (`write_files`) | Per `BOOTC_UPDATE_REPO_K8S` / `BOOTC_UPDATE_REPO_WORKER`; only when set. |

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

`make clean-vms` (since issue #221) sweeps both the qcow2 disks
(`$POOL_DIR/hummingbird-*.qcow2`) and the cloud-init seed ISOs
(`$POOL_DIR/*-seed.iso`, `$POOL_DIR/*-cloud-init.iso`) in addition to
the `virsh destroy` / `undefine` loop. The sweep uses `rm -f`, so it's
idempotent on a clean host. Override `POOL_DIR=` if your libvirt
storage pool is not `/var/lib/libvirt/images`.

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

## Customizing auto-update

The image ships a `bootc-semver-update.timer` that fires daily with a
~30-min randomized delay and an upstream-baked registry repo. The deploy
script exposes three knobs to override those defaults per cluster
without rebuilding the image — see [docs/auto-updates.md](auto-updates.md)
for the image-side contract.

### `BOOTC_UPDATE_SCHEDULE` — change when the timer fires

Accepts any [systemd `OnCalendar=`](https://www.freedesktop.org/software/systemd/man/systemd.time.html)
value. Examples:

```bash
# Weekly, Monday 3 AM (maintenance window)
BOOTC_UPDATE_SCHEDULE="Mon *-*-* 03:00:00"

# Every 15 minutes — for testing the update loop
BOOTC_UPDATE_SCHEDULE="*:0/15"

# Hourly
BOOTC_UPDATE_SCHEDULE="hourly"
```

Leave commented to keep the image default (daily with random delay).

When set, the deploy script writes
`/etc/systemd/system/bootc-semver-update.timer.d/schedule.conf` via
cloud-init `write_files` on **every** node (CP and workers) — it's a
cluster-wide knob. The drop-in clears the baked `OnCalendar=` and sets
the override:

```ini
[Timer]
OnCalendar=
OnCalendar=Mon *-*-* 03:00:00
```

The deploy script also adds a `runcmd` to `systemctl daemon-reload &&
systemctl restart bootc-semver-update.timer` so the override **takes
effect on first boot**, not just on subsequent reboots.

### `BOOTC_UPDATE_REPO_K8S` / `BOOTC_UPDATE_REPO_WORKER` — track a different repo

By default the timer tracks the upstream `ghcr.io/aatchison/hummingbird-k8s`
and `ghcr.io/aatchison/hummingbird-k8s-worker` repos (baked at image
build time in `/etc/hummingbird/bootc-update.env`). To track a fork's
tags instead — e.g. you're running your own builds out of
`ghcr.io/yourorg/hummingbird-k8s` — set:

```bash
BOOTC_UPDATE_REPO_K8S=ghcr.io/yourorg/hummingbird-k8s
BOOTC_UPDATE_REPO_WORKER=ghcr.io/yourorg/hummingbird-k8s-worker
```

Format is an OCI ref **without** a tag — the semver-update script
discovers and pins to the latest matching `vX.Y.Z` tag on its own.
Note: `bootc-semver-update.timer` is separate from the legacy
`bootc-fetch-apply-updates.timer` toggled by `AUTO_UPDATE_CP` (which
follows the currently-pinned tag rather than discovering new semver
tags). The two timers coexist; the semver one is the future-facing path.

The two repo knobs are independent — set one without the other if your
CP and workers should track different streams (uncommon but supported).

### When overrides take effect

Cloud-init writes the drop-in / env override during **first boot**, then
the same runcmd block reloads systemd so the change applies immediately.
A subsequent `bootc switch` + reboot preserves the drop-in (it lives in
`/etc/`, not in the ostree-managed `/usr/`). Existing clusters won't
pick up a changed `BOOTC_UPDATE_SCHEDULE` from `cluster.local.conf`
because cloud-init only runs at first boot — to retune an existing
cluster, edit the drop-in by hand or re-deploy the affected nodes.

## How deploy-cluster differs from raw image builds

`make deploy-cluster` is the operator-facing wrapper around the
`build → qcow2 → cloud-init seed → virt-install → kubeadm join`
pipeline. The underlying primitives (`scripts/build-k8s.sh`,
`scripts/build-worker.sh`, `lib/build-common.sh`) are still callable
directly when you want to iterate on the image layer alone (e.g.
testing a Containerfile change against an existing cluster). Once
you're ready to stand up VMs, deploy-cluster is the only supported
path:

- It carries per-VM dynamic state (hostname, worker join token,
  post-boot runcmd) via cloud-init `write_files` + NoCloud seed ISO —
  no libguestfs OS-introspection workaround required.
- It enables `AUTO_UPDATE_CP` (cloud-init runcmd) when the operator
  opts in via `cluster.local.conf`; the CP image otherwise ships with
  the timer off (#48).
- It supports operator-chosen names (`CP_NAME=hbird-cp1`,
  `WORKER_NAMES=(hbird-w1 hbird-w2)`) rather than the legacy
  single-VM `hummingbird-k8s-worker-{1..N}` naming.

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
