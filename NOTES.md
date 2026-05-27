# hummingbird-k8s — findings

Notes from setting up Fedora Hummingbird VMs on `<kvm-host>` (KVM host) with
upstream `kubeadm` Kubernetes layered into the bootc image.

## What's here

| File | Role |
|---|---|
| `containers/k8s/Containerfile` + `k8s-init.sh` / `k8s-init.service` / `10-k8s.preset` | `hummingbird-k8s` image inputs (upstream kubelet/kubeadm/cri-o, control-plane) |
| `containers/k8s-worker/Containerfile` + `worker-init.sh` / `worker-init.service` / `10-k8s-worker.preset` | Worker template image inputs |
| `containers/shared/ssh/99-no-passwords.conf` | sshd drop-in COPYd into every flavor |
| `containers/shared/kubernetes/{admission-control-config,audit-policy}.yaml` | Apiserver hardening configs COPYd into the k8s image |
| `scripts/build-k8s.sh` | `hummingbird-k8s` control plane image build |
| `scripts/build-worker.sh` | `hummingbird-k8s-worker` template image build |
| `scripts/deploy-cluster.sh` / `scripts/destroy-cluster.sh` / `scripts/update-cluster.sh` | Cluster lifecycle (1 CP + N workers) driven from `cluster.local.conf` |
| `scripts/spawn-workers.sh` | Clone worker template qcow2, mint per-VM kubeadm join tokens, virt-install N workers |
| `scripts/kubectl-k8s.sh` | SSH-tunnel-through-KVM-host kubectl wrapper |
| `Makefile` | Canonical operator entry point — every target wraps a `scripts/` script. `make help` lists them. |
| `worker-join.env` | Cached kubeadm join command. `spawn-workers.sh` mints one per-VM at qcow2 build time. |
| `bib-config.toml` | Generated at build time — initial user, SSH keys, hashed sudo password |
| `cluster.example.conf` | Operator-edited config template: copy to `cluster.local.conf` and edit. Drives `make deploy-cluster` / `make update-cluster`. |
| `config.example.sh` | Optional per-host build-input overrides (VM_USER, SSH_PUBKEY_FILES, POOL_DIR, etc.). |
| `references/k8s-bootc-talk.transcript.txt` | KubeCon India 2025 talk transcript (Berkus + Kumar) |
| `rust/` + `.devcontainer/` | Rust client-side rewrite — all phases landed (epic #279). `hbird` binary mirrors every operator-facing Makefile target; see [`docs/rust-cli-migration.md`](docs/rust-cli-migration.md). Bash scripts stay canonical until per-target Makefile dispatch flips. |

## Host setup (<kvm-host>)

- Fedora 44 Server, libvirt + cockpit-machines already installed.
- All VMs live under **system libvirt** (`qemu:///system`) so they get a
  real bridge with DHCP, libvirt sees the IP, and Cockpit lists them in
  the host's main VM panel.
- Pools: `iso`, `iso-1`, `mass2` (`/var/lib/libvirt/images`), `secondary`;
  network `default` (NAT 192.168.122.0/24).

## Build pipeline (what actually works)

1. `podman build` a layered OCI image `FROM quay.io/hummingbird-community/bootc-os@sha256:<digest>` (pinned in each Containerfile and in `lib/build-common.sh`'s `BASE_IMAGE` default), tagged `localhost/hummingbird-<flavor>:latest`.
2. `bootc-image-builder` converts that local image to a qcow2. Critical flag: pass `--local` so bib reads from local podman storage instead of trying to pull. Bind-mount `/var/lib/containers/storage` so the bib container can see the local image.
3. Output goes to `/var/lib/libvirt/images/<name>.qcow2`. `chown root:root` + `chmod 0644` so system qemu can read it.
4. `virsh -c qemu:///system pool-refresh mass2` so libvirt picks up the new volume.
5. `virt-install --connect qemu:///system ... --network network=default --import` defines and starts the VM.

`scripts/deploy-cluster.sh` orchestrates the whole pipeline end-to-end for
1 CP + N workers, sourcing `cluster.local.conf` for the operator-supplied
names, IPs (optional), SSH key, image source (`ghcr` vs `local`), and
behavioral toggles (`AUTO_UPDATE_CP`, `SWITCH_TO_GHCR`).

## Accessing the cluster from another machine (<kvm-client> → <kvm-host> VM)

libvirt's default network (typically `192.168.122.0/24`) is its NAT, inside <kvm-host>, not routable from outside. We tunnel + use kubectl in a container so <kvm-client> doesn't need kubectl installed.

1. `kubeadm init --apiserver-cert-extra-sans=<kvm-host>,127.0.0.1,localhost` — adds SANs so the apiserver cert is valid for the tunnel endpoint (baked into `k8s-init.sh`).
2. Pull `/etc/kubernetes/admin.conf` from the VM to a local file.
3. Rewrite `server:` to `https://localhost:6443`.
4. `ssh -fNL 6443:<vm-ip>:6443 <kvm-host>`.
5. `podman run --rm --net=host -v /tmp/k8s-kubeconfig:/kc:ro,Z -e KUBECONFIG=/kc docker.io/bitnami/kubectl:latest get nodes`.

`./scripts/kubectl-k8s.sh <args>` (or `make kubectl ARGS='<args>'`) wraps all of this — auto-discovers the VM IP, sets up the tunnel if missing, runs kubectl in a container with the right kubeconfig.

## Gotchas hit along the way

- **`systemctl enable` inside `podman build` is a no-op.** systemd isn't running in the build context, so the `multi-user.target.wants/` symlinks aren't created. Workaround: explicit `ln -sf`.
- **But Hummingbird also ships `99-default-disable.preset`** which strips unenabled symlinks during the image-build stage. For the kubeadm VM, the manual symlinks for `kubelet.service` + `k8s-init.service` were getting wiped out. Fix: drop a lower-numbered preset file (`/usr/lib/systemd/system-preset/10-k8s.preset`) listing `enable crio.service / kubelet.service / k8s-init.service`. Lower number wins.
- **CRI-O's unit is `crio.service`, NOT `cri-o.service`.** The Fedora RPM doesn't use the hyphen. Both the preset and `k8s-init.service`'s `After=` need to match.
- **bootc's read-only `/usr` breaks kube-controller-manager** out of the box. Its pod mounts a HostPath `flexvolume-dir` at `/usr/libexec/kubernetes/kubelet-plugins/volume/exec` with `DirectoryOrCreate`, which tries to `mkdir /usr/libexec/kubernetes` at runtime and fails read-only. Pre-create the dir at image build (`RUN mkdir -p ...`).
- **bib's `<portForward>` doesn't work with the default slirp `<interface type='user'>`.** It requires `backend='passt'`. Don't bother with port-forwarding for `qemu:///session` user-mode VMs — switch to `qemu:///system` and use the default NAT network instead.
- **`/usr/local` on Hummingbird is wired to `/var`.** Writes there at image-build time don't end up in the OCI layer. Always install to `/usr/bin` or `/usr/libexec`.
- **`bib-config.toml` `key =` accepts multi-line strings** for multiple authorized_keys entries (verified working). Hashed password works in `password =`.
- **First SSH from <kvm-host> to a freshly-recreated VM** trips on stale host keys (libvirt re-issues IPs from its NAT pool) from prior VMs. `ssh-keygen -R <ip>` clears it.
- **bib produces all formats** (qcow2, vmdk, vpc, ovf, archive, gce) even when you only ask for one — just `mv` the qcow2 you care about.
- **`scripts/kubectl-k8s.sh` swallows stdin during its port-forward bootstrap.** Anything piped on stdin (including `<<EOF … EOF` heredocs) is consumed by the tunnel-setup phase before kubectl ever runs, so kubectl reports `no objects passed to apply`. `verify-hardening.sh`'s PSA-rejection check tripped on this and silently FAILed against a correctly-hardened cluster. Workaround for verify-* scripts: bypass the wrapper and ssh direct to root@CP, e.g. `ssh root@$CP_IP "kubectl --kubeconfig=/etc/kubernetes/admin.conf apply -f -" <<EOF … EOF` — the heredoc then flows through SSH untouched. Tracked in #332 (bash twin fix) and #330 (Rust twin's `cp_kubectl_with_stdin_lenient` which already had it right).
- **Rust rewrite shipped (epic #279).** Every operator-facing Makefile target has an `hbird` counterpart in `rust/crates/hbird-cli/`. Foundation crates + Phase 1A/1B/2/3/4 + tracing all landed (PRs #321 #325 #326 #330 #334 #337 #344 #346 #347). The bash scripts under `scripts/` stay canonical until the per-target Makefile dispatch flips (a separate operator decision). The `hbird` binary's deferred work: `hbird deploy-cluster` + `hbird spawn-workers` live execution (tracked by #335) and `update-cluster`'s `timer_stop` / `timer_start` helpers (block #4, deferred because the geary cluster doesn't run scheduled-update timers). See [`docs/rust-cli-migration.md`](docs/rust-cli-migration.md) for the operator-facing `make → hbird` lookup table and [`docs/rust-cli.md`](docs/rust-cli.md) for the per-phase tracking table.

## `POOL_DIR` — libvirt storage pool location

- `POOL_DIR` controls where qcow2 disks are written and where `virt-install --disk`
  expects to find them. It is honored by `lib/build-common.sh` (build phase) and by
  `scripts/deploy-cluster.sh` / `scripts/spawn-workers.sh` (define phase).
- Default: `/var/lib/libvirt/images` (libvirt's stock pool dir).
- Override per host by setting `POOL_DIR=/your/path` in `cluster.local.conf` (or
  `config.local.sh` for build-only flows). All build + define paths source the same
  setting, so a single override keeps everything pointing at the same directory.

## How bootc auto-update works

- Image lives in any OCI registry (ghcr.io, quay.io, your own). GitHub repos *themselves* aren't pullable — publish to GHCR.
- VM tracks the image reference set at install time (or via `bootc switch`).
- `bootc upgrade --check` queries the registry; `bootc upgrade` stages the new image (chunked, only changed layers); reboot applies.
- `bootc-fetch-apply-updates.timer` ships with Hummingbird but is **disabled by default** in the new preset (#181).
- `bootc rollback` swaps to the prior deployment; reboot finalizes.
- As of #181 the canonical timer is **`bootc-semver-update.timer`** (enabled by default in the `k8s` and `k8s-worker` flavors). It resolves the highest `vMAJOR.MINOR.PATCH` tag at the flavor's GHCR repo via `skopeo list-tags` and `bootc switch`es to it daily — so a bad push to `:latest` no longer rolls the fleet. The stock `bootc-fetch-apply-updates.timer` is still on the image (disabled in the preset) for operators who explicitly want `:latest`-tracking. See `docs/auto-updates.md` for the full mechanism + per-host schedule/REPO overrides.

## How bootc-os base-image bumps land (build-time, not runtime)

Distinct from the runtime auto-update above: the `FROM` base in our Containerfiles is **digest-pinned** to a specific `quay.io/hummingbird-community/bootc-os@sha256:…`. Upstream rebuilds `:latest` every 1–4 days (see #298), so a digest pin is what keeps our builds reproducible.

Three files carry the same digest in lockstep:
- `containers/k8s/Containerfile`
- `containers/k8s-worker/Containerfile`
- `lib/build-common.sh` (the `BASE_IMAGE` default if env-var is unset)

Renovate watches all three via a `customManagers` regex rule in `.github/renovate.json` and opens a **single grouped PR** (`bootc-os-base` group) when the upstream `:latest` digest moves. The grouped PR is **never auto-merged** — `pr-validate.yml` only runs `podman build`, which can't catch a base-image drift that breaks first boot (systemd unit rename, kernel module drop, RPM removal). Operator workflow on each bump PR:

1. Read the upstream changelog via `org.opencontainers.image.revision` label diff at `gitlab.com/redhat/hummingbird/containers/-/compare/<old-revision>...<new-revision>` (find each side's revision with `skopeo inspect docker://quay.io/hummingbird-community/bootc-os@<digest> | jq -r '.Labels[\"org.opencontainers.image.revision\"]'`).
2. Boot-test by hand on a KVM host before merge (until the #32-class self-hosted boot test lands).
3. Merge; the next image release will roll the new base layer to the fleet via `bootc-semver-update.timer`.

Tracked under #298 (umbrella) / #304 (Renovate-config implementation).

## Install style: upstream kubeadm (`containers/k8s/Containerfile`)

- Adds Fedora Rawhide as a secondary repo (Hummingbird's curated set lacks `iptables-nft`, `socat`, `conntrack-tools`, `ethtool`). The Fedora GPG keyring bundle is imported at build time and the repo is configured with `gpgcheck=1`, so Rawhide RPMs are signature-verified during install (#70).
- Adds `pkgs.k8s.io` RPM repos for `core` (kubelet/kubeadm/kubectl) and `addons:cri-o`.
- Pre-creates `/usr/libexec/kubernetes/kubelet-plugins/volume/exec` so kube-controller-manager doesn't fail on read-only `/usr`.
- Drops `/etc/modules-load.d/k8s.conf` + sysctls.
- `k8s-init.service` runs once at first boot: `kubeadm init` (with `--apiserver-cert-extra-sans` for tunneled access), installs Cilium via `cilium-cli` (see [`docs/cilium-migration.md`](docs/cilium-migration.md)), untaints the control-plane node, makes admin.conf world-readable, then touches `/var/lib/k8s-init.done` so it doesn't re-run.
- Final cluster: 1 node, ~9 pods (etcd, apiserver, controller-manager, scheduler, kube-proxy, 2x coredns, cilium agent, cilium-operator).
- Roughly matches Red Hat's "Build Your K8s Ready Distro With BootC" talk pattern — Praveen Kumar describes installing the Kubernetes RPMs straight into a fedora-bootc image.

## Reference: Praveen Kumar's public bootc demos

The exact KubeCon India 2025 demo repo isn't public. Two adjacent demos from the same author show the same pattern:

- [`praveenkumar/devconfin26`](https://github.com/praveenkumar/devconfin26) — Gitea dev-platform appliance. Has `.github/workflows/build.yml`, `Containerfile`, `quadlet/`, `systemd/`, `scripts/`. Best template for a GH Actions build pipeline.
- [`praveenkumar/fossasia26`](https://github.com/praveenkumar/fossasia26) — Apache v1 vs v2 (with intentional break in v2) — minimal upgrade/rollback demo.

Both base on `quay.io/fedora/fedora-bootc:43` rather than Hummingbird, since Hummingbird was announced after these were authored.

- Upstream project entry points (bootc, BIB, bootc-os base, cri-o, Cilium, `pkgs.k8s.io`) consolidated in [`docs/upstream-references.md`](docs/upstream-references.md) — file there when you find yourself re-grepping an upstream URL.

## SSH access summary

- Default config: key-only auth (`PasswordAuthentication no`), no sudo (user is not in wheel, no password set), but `root` SSH is allowed *with the same pubkeys* via `PermitRootLogin prohibit-password`. Admin tasks happen as root.
- Pubkeys: whatever `SSH_PUBKEY_FILES` resolves to (default: caller's `~/.ssh/id_ed25519.pub`).
- To opt back into sudo: set `VM_USER_GROUPS=wheel` and `VM_PASSWORD=…` in `config.local.sh`.
- To opt out of root SSH: set `ENABLE_ROOT_SSH=0`. The unprivileged user is then the only way in.

## Useful one-liners

```bash
# Find a VM's IP
sudo virsh -c qemu:///system net-dhcp-leases default | grep <vm-name>

# Deploy / re-deploy a cluster (the only supported path). No `sudo` —
# the script handles privilege escalation (issue #233): re-execs on
# $KVM_HOST via SSH when set, otherwise probes EUID locally and bails
# with a hint if neither path works.
make -C ~/hummingbird-k8s deploy-cluster CONFIG=cluster.local.conf

# Tear it down
make -C ~/hummingbird-k8s destroy-cluster CONFIG=cluster.local.conf

# Direct script invocation still works (deploy-cluster.sh is the entry point):
sudo bash ~/hummingbird-k8s/scripts/deploy-cluster.sh ~/hummingbird-k8s/cluster.local.conf

# Enable bootc auto-update timer at runtime. The image preset already
# enables bootc-semver-update.timer; this line is the runtime equivalent
# for a host built from a pre-#181 image, or to make the OnBootSec=15min
# window fire this boot rather than after the next reboot.
sudo systemctl enable --now bootc-semver-update.timer
```

