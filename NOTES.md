# hummingbird-k8s — findings

Notes from setting up Fedora Hummingbird VMs on `<kvm-host>` (KVM host) with
two flavors of single-node Kubernetes layered into the bootc image.

## What's here

| File | Role |
|---|---|
| `containers/k3s/Containerfile` + `containers/k3s/10-k3s.preset` | `hummingbird-k3s` image inputs (k3s baked in) |
| `containers/k8s/Containerfile` + `k8s-init.sh` / `k8s-init.service` / `10-k8s.preset` | `hummingbird-k8s` image inputs (upstream kubelet/kubeadm/cri-o, control-plane) |
| `containers/k8s-worker/Containerfile` + `worker-init.sh` / `worker-init.service` / `10-k8s-worker.preset` | Worker template image inputs |
| `containers/shared/ssh/99-no-passwords.conf` | sshd drop-in COPYd into every flavor |
| `containers/shared/kubernetes/{admission-control-config,audit-policy}.yaml` | Apiserver hardening configs COPYd into the k8s image |
| `scripts/build-k3s.sh` / `scripts/define-vm.sh` / `scripts/redo-k3s.sh` | `hummingbird-k3s` build + define + redo |
| `scripts/build-k8s.sh` / `scripts/define-vm-k8s.sh` / `scripts/redo-k8s.sh` | `hummingbird-k8s` control plane build + define + redo |
| `scripts/build-worker.sh` / `scripts/spawn-workers.sh` / `scripts/redo-workers.sh` | Worker build + spawner |
| `scripts/kubectl-k8s.sh` | SSH-tunnel-through-KVM-host kubectl wrapper |
| `scripts/migrate-to-system.sh` | One-time helper that moved the original VM from `qemu:///session` to system |
| `Makefile` | Canonical operator entry point — every target wraps a `scripts/` script. `make help` lists them. |
| `worker-join.env` | Cached kubeadm join command. `spawn-workers.sh` mints one per-VM at qcow2 build time. |
| `bib-config.toml` | Generated at build time — initial user, SSH keys, hashed sudo password |
| `references/k8s-bootc-talk.transcript.txt` | KubeCon India 2025 talk transcript (Berkus + Kumar) |

## Host setup (<kvm-host>)

- Fedora 44 Server, libvirt + cockpit-machines already installed.
- Two libvirt connections in use:
  - `qemu:///session` — pools `iso`, `userspace` (rootless; existing `molty` VM lives here)
  - `qemu:///system` — pools `iso`, `iso-1`, `mass2` (`/var/lib/libvirt/images`), `secondary`; network `default` (NAT 192.168.122.0/24)
- All our VMs live under **system libvirt** so they get a real bridge with DHCP, libvirt sees the IP, and Cockpit lists them in the host's main VM panel.

## Build pipeline (what actually works)

1. `podman build` a layered OCI image `FROM quay.io/hummingbird-community/bootc-os@sha256:<digest>` (pinned in each Containerfile and in `lib/build-common.sh`'s `BASE_IMAGE` default), tagged `localhost/hummingbird-<flavor>:latest`.
2. `bootc-image-builder` converts that local image to a qcow2. Critical flag: pass `--local` so bib reads from local podman storage instead of trying to pull. Bind-mount `/var/lib/containers/storage` so the bib container can see the local image.
3. Output goes to `/var/lib/libvirt/images/<name>.qcow2`. `chown root:root` + `chmod 0644` so system qemu can read it.
4. `virsh -c qemu:///system pool-refresh mass2` so libvirt picks up the new volume.
5. `virt-install --connect qemu:///system ... --network network=default --import` defines and starts the VM.

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

## `POOL_DIR` — libvirt storage pool location

- `POOL_DIR` controls where qcow2 disks are written and where `virt-install --disk`
  expects to find them. It is honored by `lib/build-common.sh` (build phase), by
  `define-vm.sh` / `define-vm-k8s.sh` (define phase), and — once PR #28 lands — by
  `spawn-workers.sh`.
- Default: `/var/lib/libvirt/images` (libvirt's stock pool dir).
- Override per host by exporting `POOL_DIR=/your/path` in `config.local.sh`. All
  build + define scripts source that file from the repo root, so a single setting
  keeps build/define/spawn pointing at the same directory.

## How bootc auto-update works

- Image lives in any OCI registry (ghcr.io, quay.io, your own). GitHub repos *themselves* aren't pullable — publish to GHCR.
- VM tracks the image reference set at install time (or via `bootc switch`).
- `bootc upgrade --check` queries the registry; `bootc upgrade` stages the new image (chunked, only changed layers); reboot applies.
- `bootc-fetch-apply-updates.timer` ships with Hummingbird but is **disabled by default**. Enable it (via Containerfile or runtime) for daily auto-update + auto-reboot. Most fleets want to enable the fetch but orchestrate the reboot externally.
- `bootc rollback` swaps to the prior deployment; reboot finalizes.
- As of #4 the timer is **ENABLED by default** in all three flavors (k3s, CP, worker). See `docs/auto-updates.md` for caveats — notably that control planes reboot without draining themselves.

## Two install styles

### k3s (`containers/k3s/Containerfile`)
- Upstream installer drops single `/usr/bin/k3s` binary + systemd unit.
- Cluster up in ~10s after boot. CNI = flannel (k3s default), ingress = traefik, storage = local-path — all auto-deployed.
- Best fit for "single node with containers."

### Upstream kubeadm (`containers/k8s/Containerfile`)
- Adds Fedora Rawhide as a secondary repo (Hummingbird's curated set lacks `iptables-nft`, `socat`, `conntrack-tools`, `ethtool`).
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

## SSH access summary

- Default config: key-only auth (`PasswordAuthentication no`), no sudo (user is not in wheel, no password set), but `root` SSH is allowed *with the same pubkeys* via `PermitRootLogin prohibit-password`. Admin tasks happen as root.
- Pubkeys: whatever `SSH_PUBKEY_FILES` resolves to (default: caller's `~/.ssh/id_ed25519.pub`).
- To opt back into sudo: set `VM_USER_GROUPS=wheel` and `VM_PASSWORD=…` in `config.local.sh`.
- To opt out of root SSH: set `ENABLE_ROOT_SSH=0`. The unprivileged user is then the only way in.

## Useful one-liners

```bash
# Find a VM's IP
sudo virsh -c qemu:///system net-dhcp-leases default | grep <vm-name>

# Force rebuild + redefine (via Makefile)
sudo make -C ~/hummingbird-k8s k3s                 # k3s
sudo make -C ~/hummingbird-k8s k8s                 # upstream k8s control-plane
sudo make -C ~/hummingbird-k8s workers COUNT=2     # wipe + build + spawn N workers
sudo make -C ~/hummingbird-k8s spawn COUNT=3       # spawn N more workers from the existing template

# Direct script invocation still works:
sudo bash ~/hummingbird-k8s/scripts/redo-k3s.sh
sudo bash ~/hummingbird-k8s/scripts/redo-k8s.sh
sudo bash ~/hummingbird-k8s/scripts/redo-workers.sh 2
sudo bash ~/hummingbird-k8s/scripts/spawn-workers.sh 3

# Enable bootc auto-update timer at runtime
sudo systemctl enable --now bootc-fetch-apply-updates.timer
```
