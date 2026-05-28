# `make deploy-cluster` — hybrid bib + cloud-init orchestrator

`scripts/deploy-cluster.sh` is the operator-facing "deploy a real cluster"
entry point. One config file → one CP + N workers, end-to-end:

```bash
cp cluster.example.conf cluster.local.conf
$EDITOR cluster.local.conf
make deploy-cluster CONFIG=cluster.local.conf
```

This is the only supported way to stand up a cluster.

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
`kubectl` access. `make nodes` (which delegates to `hbird kubectl` —
the Rust twin that replaced `scripts/kubectl-k8s.sh` in the v0.1.0
cutover [#353]) works against the deployed CP. Cross-runtime
dependency: `hbird` CLI must be on PATH.

## Remote KVM-host operation (`KVM_HOST=`)

Since C3 (#232), `deploy-cluster.sh`, `destroy-cluster.sh`,
`update-cluster.sh`, and `spawn-workers.sh` self-host on the KVM host
via SSH when `KVM_HOST` is set and the local short hostname doesn't
match `${KVM_HOST%%.*}`. The client never needs `sudo` or `libvirt`
installed — only `ssh` + the operator's SSH key. Sudo happens on the
remote by default; `update-cluster`, `deploy-cluster`, and
`destroy-cluster` accept a libvirt-group operator on the remote and
can be invoked without sudo entirely via `HBIRD_REMOTE_NO_SUDO=1` —
see [Running without sudo](#running-without-sudo-libvirt-group-operator-305)
below (#269, extended to deploy/destroy by #305). `spawn-workers.sh`
still requires root on the remote — that's the remaining piece of
`#269`'s original scope.

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
prompt normally on the SSH TTY. As of #305, the cleaner answer for
unattended runs from a workstation is to skip `sudo` entirely via
`HBIRD_REMOTE_NO_SUDO=1` + a libvirt-group operator on the KVM host —
see [Running without sudo](#running-without-sudo-libvirt-group-operator-305).

### Running without sudo (libvirt-group operator, #305)

`deploy-cluster.sh`, `destroy-cluster.sh`, and `update-cluster.sh`
all accept either root OR a member of the `libvirt` group on the KVM
host. libvirt authorizes `qemu:///system` via the unix-socket group
(not via sudo); `bootc-image-builder` runs rootless under podman;
remote ops on the cluster VMs (ssh + kubectl) run as `root@<node-ip>`
via the operator's SSH key and are unaffected by the local EUID.

**One-time setup on the KVM host:**

```bash
# 1. Add the operator to the libvirt group.
ssh $KVM_HOST 'sudo usermod -aG libvirt $USER && newgrp libvirt'
# (or just log out + back in to pick up the new group)

# 2. Make POOL_DIR group-writable so deploy-cluster can drop qcow2 +
#    seed ISO files there without root. setgid (2775) makes new files
#    inherit the libvirt group, so qemu (which is also in libvirt) can
#    read them after libvirt's dynamic_ownership chowns to qemu:qemu at
#    VM start.
#
#    Substitute the actual POOL_DIR value from your cluster.local.conf —
#    the placeholder below is illustrative. Example for the geary host
#    where cluster.local.conf has POOL_DIR=/mnt/mass2/vms:
#      ssh geary 'sudo chgrp libvirt /mnt/mass2/vms && sudo chmod 2775 /mnt/mass2/vms'
ssh $KVM_HOST "sudo chgrp libvirt <POOL_DIR_from_cluster.local.conf> && sudo chmod 2775 <POOL_DIR_from_cluster.local.conf>"
```

**Invocation from a workstation:**

```bash
# The shim re-execs over SSH WITHOUT prefixing sudo on the remote when
# HBIRD_REMOTE_NO_SUDO=1 is set. Combined with the libvirt-group
# operator on the remote (above), this is a fully unattended path —
# no TTY, no password prompt.
KVM_HOST=geary HBIRD_REMOTE_NO_SUDO=1 \
  make deploy-cluster CONFIG=cluster.local.conf
KVM_HOST=geary HBIRD_REMOTE_NO_SUDO=1 \
  make destroy-cluster CONFIG=cluster.local.conf
```

**Invocation on the KVM host directly:**

```bash
# As a libvirt-group user (no sudo needed):
make deploy-cluster   CONFIG=cluster.local.conf
make destroy-cluster  CONFIG=cluster.local.conf
make update-cluster   CONFIG=cluster.local.conf

# Or as root (the pre-#305 invocation pattern still works):
sudo make deploy-cluster CONFIG=cluster.local.conf
```

Defaults are conservative: `HBIRD_REMOTE_NO_SUDO` is **off** by
default. `spawn-workers.sh` still requires root on the remote (that's
the remaining bit of #269's original scope, not yet folded into the
no-sudo path).

**Pre-#310 hosts:** if a host ever ran `sudo bash scripts/deploy-cluster.sh`
before PR for #310 landed, the BIB config files
(`bib-config-deploy-{cp,worker}.toml`) were left behind in the
operator's checkout owned `root:root` mode `0644`. Subsequent no-sudo
deploys would fail at the rewrite with `Permission denied`. Post-#310
these files are written to `mktemp` paths (no REPO_ROOT side-effect), so
new operators never hit this — but existing hosts may need a one-time
cleanup of the leftover files:

```bash
ssh $KVM_HOST 'sudo rm -f ~/hummingbird-k8s/bib-config-deploy-{cp,worker}.toml'
```

See #310 for the rationale (sibling bug to #305's POOL_DIR migration).

### Reusing pre-built qcow2 templates (#311)

`bootc-image-builder` (bib) — invoked by `build_qcow2` in
`lib/build-common.sh` — hard-requires **rootful** podman and refuses
to run under the rootless podman that a libvirt-group operator gets by
default (it errors with `cannot validate the setup: this command must
be run in rootful (not rootless) podman`; the constraint is upstream
and intentional, not a missing flag). That means a no-sudo
`make deploy-cluster` on a fresh host cannot build the qcow2
templates from scratch.

To make `HBIRD_REMOTE_NO_SUDO=1 deploy-cluster` usable in the common
re-stand-up workflow (destroy → redeploy without a new image),
`build_qcow2` short-circuits when the target qcow2 already exists in
`$POOL_DIR`:

```text
[build_qcow2] skipping rebuild: /path/to/hummingbird-k8s.qcow2 already exists (set FORCE_REBUILD=1 to force)
```

Behavior matrix:

| qcow2 in `$POOL_DIR` | `FORCE_REBUILD` | Result |
| --- | --- | --- |
| missing | unset / `0` | run bib (rebuild) |
| missing | `1` | run bib (rebuild) |
| present (non-empty) | unset / `0` | **skip + log** (new, default) |
| present (non-empty) | `1` | run bib (operator override) |

Zero-byte qcow2 files (e.g. from a crashed previous build) do NOT
trip the skip — the guard uses `[[ -s "$qcow" ]]`, not `[[ -e ]]`.

**Operator workflow for the no-sudo path:**

1. One sudo'd deploy (or one root-on-the-KVM-host deploy) to seed the
   qcow2 templates into `$POOL_DIR`. This is the only time bib runs.
2. Subsequent destroy → redeploy cycles can use
   `HBIRD_REMOTE_NO_SUDO=1` — `build_qcow2` will skip bib and reuse
   the templates.
3. When pulling a new GHCR tag, opt out of the skip with
   `FORCE_REBUILD=1`, which will require sudo for that one
   invocation (bib still needs rootful podman):

   ```bash
   FORCE_REBUILD=1 sudo make deploy-cluster CONFIG=cluster.local.conf
   ```

**Staleness caveat:** the skip does not validate that the on-disk
qcow2 matches the local container image's digest. After pulling a
new `ghcr.io/aatchison/hummingbird-k8s:<tag>`, the templates on disk
are stale until you set `FORCE_REBUILD=1` and rebuild. A future
candidate could compare the qcow2's recorded build digest against
the current image — see #311 for the trade-off.

Diagnostic when the operator is neither root nor in the `libvirt`
group on the KVM host:

```text
[deploy-cluster] ERROR: must be root or a member of the libvirt group on this host. Add yourself with:
  sudo usermod -aG libvirt $USER && newgrp libvirt
then rerun. POOL_DIR must also be group-writable. One-time setup (substitute your POOL_DIR from cluster.local.conf — defaults to /var/lib/libvirt/images):
  sudo chgrp libvirt /var/lib/libvirt/images
  sudo chmod 2775 /var/lib/libvirt/images
See docs/deploy-cluster.md#running-without-sudo-libvirt-group-operator-305.
```

The same diagnostic shape is emitted by `destroy-cluster.sh` (with
its own script name) when run by a non-root non-libvirt-group user.

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
| Image-build knobs | `VM_USER`, `STORAGE_DRIVER`, `PODMAN_ROOT`, `PODMAN_RUNROOT`, `APISERVER_EXTRA_SANS`, `FORCE_REBUILD` (when `=1`, opts out of `build_qcow2`'s skip-if-exists shortcut and forces a fresh bib invocation; defaults off — pre-existing qcow2 templates are reused. See [Reusing pre-built qcow2 templates](#reusing-pre-built-qcow2-templates-311) below. #311 candidate d.) |
| Shim itself | `HBIRD_AUTOLOAD_CONFIG_LOCAL`, `HBIRD_REMOTE_REPO`, `HBIRD_OPERATOR_PUBKEY_FILE` (shim-managed, see #248 below), `HBIRD_REMOTE_NO_SUDO` (drops `sudo` from the remote exec when `=1`; see [Running without sudo](#running-without-sudo-libvirt-group-operator-305) for the libvirt-group story — #269 for update-cluster, #305 for deploy/destroy) |

Every value is passed through `printf %q` before being interpolated
into the remote command, so values with spaces, quotes, or shell
metas reach the remote unmangled.

`CONFIG=<local-path>` is special-cased: the local config file is
`scp`'d to a remote `mktemp -d` and the remote `CONFIG=` is rewritten
to point at the copy before the script runs.

### Operator workstation pubkey baked alongside KVM-host key (#248)

`SSH_PUBKEY_FILE=` inside the operator's local `CONFIG` is a **path**
— and the deploy script (which runs on the KVM host after the shim
re-execs) would otherwise resolve that path against the KVM host's
filesystem, baking the KVM host's `~/.ssh/id_ed25519.pub` into the
cluster instead of the operator's workstation key. The operator then
couldn't SSH directly from their workstation to the CP.

To fix this without transporting private keys: when the shim scp's the
`CONFIG`, it ALSO scp's the file referenced by `SSH_PUBKEY_FILE` in
that config to the same remote tempdir, then forwards the remote path
via the shim-internal `HBIRD_OPERATOR_PUBKEY_FILE` env var.
`deploy-cluster.sh` appends that path to `SSH_PUBKEY_FILES`
(colon-separated) so `build_qcow2` bakes BOTH keys into the cluster:

| Key | Used by | Path inside the cluster |
| --- | --- | --- |
| KVM-host's pubkey (`SSH_PUBKEY_FILE`) | `deploy-cluster.sh` itself, to SSH into the freshly-booted CP via `SSH_PRIVKEY_FILE = ${SSH_PUBKEY_FILE%.pub}` | `/root/.ssh/authorized_keys` |
| Operator's workstation pubkey (`HBIRD_OPERATOR_PUBKEY_FILE`) | Operator, for direct `ssh root@<cp-ip>` from the workstation | `/root/.ssh/authorized_keys` (appended) |

`HBIRD_OPERATOR_PUBKEY_FILE` is a shim-internal var: it's on the SSH
command line and in the allowlist (so `sudo env` accepts it on the
remote) but it's hidden from the operator-facing visible-env log line
since the operator didn't set it themselves. When the two paths
resolve to the same file (e.g. the workstation IS the KVM host and the
shim is a no-op anyway, or by deliberate config), the append is
skipped — no duplicate `authorized_keys` entry.

No private-key material crosses the wire; only the `.pub` file
content. See issue #248 for the full design discussion.

### `HBIRD_REMOTE_REPO` override

| Var | Default | Effect |
| --- | --- | --- |
| `HBIRD_REMOTE_REPO` | `~/hummingbird-k8s` | Absolute path on `$KVM_HOST` to the remote git checkout the shim execs from. Override when you keep the repo somewhere non-standard (e.g. `/opt/hummingbird-k8s` or `/srv/repos/hummingbird-k8s`). Tilde expansion happens on the remote shell's word-splitting pass — start with `/` or `~/` only. |

### Remote-checkout freshness (#365)

The shim execs `bash scripts/<name>.sh` from the **remote checkout's
filesystem**, not from the local repo's. If the remote checkout is
behind a fix you just merged on your workstation, the re-exec silently
runs the pre-merge code. Surfaced in cycle 2 when `geary`'s checkout
was missing the #364 merge and silently ran the pre-fix
`verify-app-deploy.sh`.

The shim now does a `git rev-parse HEAD` on `$KVM_HOST:$HBIRD_REMOTE_REPO`
before re-exec and WARNs to stderr when the remote is BEHIND, DIVERGED,
or unreadable. Equal SHAs print nothing (silent good path). The check
runs only on the operator's workstation — when CI workflows run on the
KVM host directly (`runs-on: [self-hosted, kvm, libvirt]`), the shim
itself doesn't fire, so the check is a no-op in CI today.

| Var | Default | Effect |
| --- | --- | --- |
| `HBIRD_REMOTE_FRESHNESS_CHECK` | `1` (on) | Boolish opt-out (`0`/`false`/`no`/`off`). Disables the freshness check entirely. Useful for release-train rollback drills or any case where running a known-older remote checkout is intentional. Client-side only — never forwarded to the remote. |
| `HBIRD_REMOTE_STRICT`          | `0` (warn-only) | Boolish opt-in (`1`/`true`/`yes`/`on`). Upgrades stale-remote WARN into a hard fail (exit 1, no re-exec). Network-unreachable still only WARNs — that's transient, the real ssh below will surface the actual error. Suggested for any future CI workflow that drives the shim from a non-KVM-host runner. |
| `HBIRD_REMOTE_LAG_THRESHOLD`   | `5`             | Maximum number of commits the remote may be BEHIND before WARN fires. Default `5` swallows release-train rebase noise. Set to `0` to WARN on any lag. Non-numeric values fall back to the default. |

The check distinguishes four failure modes so the operator gets
actionable text:

| Case | Trigger | WARN text | STRICT behavior |
| --- | --- | --- | --- |
| (a) Network/auth | `ssh` exit code non-zero | "cannot reach $KVM_HOST … ssh exit N — proceeding" | Still WARNs only; does NOT hard-fail (transient) |
| (b) Bad checkout | `ssh` OK, `git rev-parse HEAD` returns empty | "$KVM_HOST:$HBIRD_REMOTE_REPO exists but is not a git checkout … looks like a tarball extract or a corrupted .git" + re-clone recovery hint | Hard-fail under STRICT |
| (c) Behind | `merge-base --is-ancestor remote local` succeeds | "BEHIND local by N commit(s)" + `fetch && reset --hard origin/main` recovery hint (when local is on `main`; manual reconcile hint when on a topic branch) | Hard-fail under STRICT |
| (d) Diverged | Not equal, not ancestor | "diverges from local — re-exec semantics may surprise you" | Hard-fail under STRICT |

When the operator's local branch is NOT `main`, the WARN text appends a
"local on topic branch '$BRANCH' — diverge is expected" hint so a
feature-branch developer doesn't get misled into force-pushing main on
the KVM host.

Per-session caching: results are memoized in `$XDG_RUNTIME_DIR/hbird-freshness/`
(falls back to `$TMPDIR` then `/tmp`) for 60 seconds, keyed by
`$KVM_HOST + $HBIRD_REMOTE_REPO + local_sha + strict_flag`. A Make
pipeline that runs deploy + update + spawn-workers back-to-back pays
the SSH round-trip cost once. Set
`HBIRD_SSH_WRAP_FRESHNESS_CACHE=0` to disable.

## Config surface

The full set is in `cluster.example.conf`; the essentials:

| Knob | Required? | Default | Purpose |
| --- | --- | --- | --- |
| `CP_NAME` | yes | — | libvirt domain name for the CP. |
| `WORKER_NAMES` | no | `(${CP_NAME}-w1 ${CP_NAME}-w2)` | bash array of worker domain names — also seeded as each worker's cloud-init hostname, so the names are visible verbatim in `virsh list`, `/etc/hostname`, AND `kubectl get nodes`. Set `WORKER_NAMES=()` for a CP-only deploy. (#254.) |
| `SSH_PUBKEY_FILE` | yes | — | Path to the operator's public key; baked AND in cloud-init. |
| `IMAGE_SOURCE` | no | `ghcr` | `ghcr` (pull from registry — the registry-first golden path) or `local` (build from this repo — power-user / fast iteration). |
| `GHCR_TAG` | no | `latest` | Tag used for `bootc switch` and for `ghcr` pulls. |
| `ENABLE_CLOUD_INIT` | yes, must be `1` | — | The deploy script refuses to run without it. |
| `AUTO_UPDATE_CP` | no | `true` | Emit a runcmd to enable `bootc-fetch-apply-updates.timer` on the CP. Overrides #48's opt-out. |
| `SWITCH_TO_GHCR` | no | `true` | Emit a `bootc switch ghcr.io/...:$GHCR_TAG` runcmd on first boot. |
| `CP_MEMORY` / `CP_VCPUS` | no | `8192` / `4` | CP sizing. |
| `WORKER_MEMORY` / `WORKER_VCPUS` | no | `4096` / `2` | Per-worker sizing. |
| `POOL_DIR` | no | `/var/lib/libvirt/images` | Where qcow2s + seed ISOs land. |
| `RUN_VERIFY` | no | `false` | Run `hbird verify app-deploy` (post-#353, was `scripts/verify-app-deploy.sh`) after Ready. |
| `KVM_HOST` | no | unset | Recorded in the summary for downstream `hbird kubectl` use (post-#353, was `scripts/kubectl-k8s.sh`). |
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
8. **Per-worker seed.** Emit `#cloud-config` with `hostname:
   ${WORKER_NAMES[i]}`, SSH key, `write_files` for
   `/etc/hummingbird/worker-join.env`, and the `bootc switch` runcmd.
   `worker-init.sh` reads `hostnamectl --static` directly (which
   reflects what cloud-init wrote to `/etc/hostname` during the init
   stage, before `multi-user.target` activates) so kubeadm join uses
   the operator-declared name even when the running kernel hostname is
   stale — see #254. It does NOT call `cloud-init status --wait`:
   that wait deadlocks against `multi-user.target` — see #265.
9. **virt-install workers in parallel.** Each gets its own seed ISO.
10. **Wait for `N+1` nodes Ready.**
11. **Optional verify.** `RUN_VERIFY=true` runs
    `hbird verify app-deploy` (post-#353, was `scripts/verify-app-deploy.sh`). Non-zero is informational — the
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

The standard verifiers all work against a deploy-cluster cluster.
From a workstation, set `KVM_HOST` so the Rust verifiers tunnel
through the KVM host (`hbird verify <sub>` resolves CP_IP via
`hbird-config` / `--cp-ip` and uses ProxyJump=$KVM_HOST for SSH).
Cross-runtime dependency: `hbird` CLI must be on PATH after the
v0.1.0 cutover ([#353]).

```bash
KVM_HOST=geary hbird verify hardening --config cluster.local.conf
KVM_HOST=geary hbird verify app-deploy --config cluster.local.conf
KVM_HOST=geary hbird verify encryption --config cluster.local.conf
KVM_HOST=geary make verify-all CONFIG=cluster.local.conf
```

When the verifier runs on the KVM host itself (e.g. inside `make
deploy-cluster`'s RUN_VERIFY re-exec, or operator on the hypervisor),
the Rust path drops ProxyJump (for hardening/encryption) or skips
with exit 0 (for app-deploy) — see [`docs/app-deploy-verify.md`](app-deploy-verify.md)
"From the KVM host directly" for details.

On the KVM host itself, drop `KVM_HOST=` and the scripts use local
libvirt directly.

Set `RUN_VERIFY=true` in `cluster.local.conf` to have
`verify-app-deploy.sh` run automatically once the cluster reaches Ready.

## Tear-down

```bash
# All hummingbird-* VMs at once. Honors KVM_HOST — from a workstation
# with no local libvirt, set KVM_HOST and the C3 SSH-wrap shim
# (scripts/lib/ssh-wrap.sh) re-execs scripts/clean-vms.sh on the KVM
# host. On the KVM host directly the script self-elevates via sudo.
make clean-vms
KVM_HOST=geary make clean-vms          # from a workstation

# Or a specific VM:
sudo virsh -c qemu:///system destroy hbird-cp1
sudo virsh -c qemu:///system undefine hbird-cp1
sudo rm -f /var/lib/libvirt/images/hbird-cp1.qcow2\
           /var/lib/libvirt/images/hbird-cp1-seed.iso
```

`make clean-vms` also sweeps stale `hummingbird-*.qcow2`, `*-seed.iso`,
and `*-cloud-init.iso` files under `POOL_DIR` (default
`/var/lib/libvirt/images`; override with `POOL_DIR=`). Pre-#216
layouts (k3s, legacy single-VM names) and any seed ISO left behind by
a prior deploy are removed in the same pass, then
`virsh pool-refresh default` is run so libvirt drops the rm'd files
from the volumes catalog. Idempotent on a clean host. See issue #221.

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
  `make clean-vms` first (self-elevates / honors `KVM_HOST`).
- **Seed ISOs left behind after a failed deploy.** The script's exit
  trap removes seed ISOs it created when the deploy fails before
  completion. A successful deploy keeps them so `virsh start` after a
  `virsh destroy` re-reads the same NoCloud datasource.

## Rust counterpart

`hbird deploy-cluster --config cluster.local.conf` mirrors the same
flag surface (`--kvm-host`, `--no-sudo`, plus a `--dry-run` plan-only
mode the bash twin lacks). Dry-run parity landed in Phase 4
([PR #337]); live execution (bib + virt-install + guestfish) is
deferred to [#335], so today the bash `make deploy-cluster` target
remains canonical for real deploys. For the full `make → hbird`
lookup table see [`docs/rust-cli-migration.md`](rust-cli-migration.md).

[PR #337]: https://github.com/aatchison/hummingbird-k8s/pull/337
[#335]: https://github.com/aatchison/hummingbird-k8s/issues/335
[#353]: https://github.com/aatchison/hummingbird-k8s/issues/353
