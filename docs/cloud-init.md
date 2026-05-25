# Optional cloud-init support

Hummingbird's canonical image-customization path is the
`[[customizations.user]]` blocks in `bib-config.toml`, rendered by
`lib/build-common.sh` from the `VM_USER` / `SSH_PUBKEY_*` env vars and
applied at qcow2 build time. That covers the homelab use case: one
operator, one set of pubkeys, baked into the image.

Cloud-init is an **opt-in** layer for everything that doesn't fit that
shape — per-VM user-data, automation pipelines that mint VMs from a
shared template, multi-tenant environments where each VM needs a
different set of SSH keys, packages, or first-boot commands.

The default image **does not** ship cloud-init. Enabling it is a
single build-arg.

## Why optional

- The default image is lean (no `cloud-init` / `cloud-utils-growpart`
  package weight, no datasource lookup on every boot).
- bib customization is reproducible and self-contained: the embedded
  pubkeys and user account are part of the image digest, so the same
  qcow2 always boots with the same configuration.
- bootc's read-only `/usr` design plus ostree's `/etc` overlay make
  cloud-init's runtime mutations safe but not free — the changes land
  in the active deployment's `/etc` and persist across reboots, but
  they do **not** propagate to a rolled-back deployment.

## When to enable

Turn cloud-init on when any of these is true:

- The same image is deployed to multiple VMs that need different SSH
  keys or hostnames.
- A pipeline (Terraform, Ansible, custom orchestrator) generates
  per-VM user-data and passes it to `virt-install`.
- You want to inject one-shot `runcmd` scripts (extra package
  installs, file writes) without rebuilding the image.

## How to enable

Build-time:

```bash
# via the Makefile convenience targets
make image-k8s-with-cloud-init
make image-worker-with-cloud-init

# or directly via the env var
ENABLE_CLOUD_INIT=1 sudo bash scripts/build-k8s.sh
```

The `make deploy-cluster` path requires `ENABLE_CLOUD_INIT=1` in
`cluster.local.conf` and will set the build-arg automatically when
`IMAGE_SOURCE=local`.

The build-arg is read by `containers/<flavor>/Containerfile`. When
`ENABLE_CLOUD_INIT=1`, the build:

1. `dnf install -y cloud-init cloud-utils-growpart`
2. Enables `cloud-init-local`, `cloud-init-main`,
   `cloud-init-network`, `cloud-config`, `cloud-final` via a
   `10-cloud-init.preset` (low number so it wins over Hummingbird's
   `99-default-disable.preset`). cloud-init 26+ split the legacy
   `cloud-init.service` into `-main` + `-network`; the preset covers
   both so the unit ordering matches upstream defaults.
3. Drops `/etc/cloud/cloud.cfg.d/99-hummingbird-datasource.cfg` with
   the datasource priority list: `[ NoCloud, ConfigDrive, OpenStack,
   None ]`.

When `ENABLE_CLOUD_INIT=0` (the default), those two files are also
removed during the conditional build step, so the resulting image is
byte-identical to a build that doesn't pass the build-arg at all.

## Injecting user-data via virt-install

The simplest end-to-end example uses `virt-install --cloud-init`:

```bash
cat > /tmp/user-data <<'EOF'
#cloud-config
users:
  - name: alice
    ssh_authorized_keys:
      - ssh-ed25519 AAAA... alice@laptop
    groups: wheel
    sudo: ALL=(ALL) NOPASSWD:ALL
packages:
  - htop
  - tmux
runcmd:
  - [ systemctl, enable, --now, htop.service ]
EOF

cat > /tmp/meta-data <<'EOF'
instance-id: hummingbird-k8s-alice-1
local-hostname: alice-cp.lab
EOF

virt-install \
  --name hummingbird-k8s-alice \
  --memory 8192 --vcpus 4 \
  --disk path=/var/lib/libvirt/images/hummingbird-k8s.qcow2,format=qcow2 \
  --os-variant fedora-eln \
  --import \
  --network network=default \
  --cloud-init user-data=/tmp/user-data,meta-data=/tmp/meta-data \
  --noautoconsole
```

`virt-install --cloud-init` builds an ephemeral NoCloud seed ISO from
the supplied files and attaches it as a CD-ROM; cloud-init in the
guest picks it up via the NoCloud datasource on first boot.

## What cloud-init runs at boot

- `cloud-init-local` (before `network.target`): reads the NoCloud
  seed, sets the hostname.
- `cloud-init-network` + `cloud-init-main` (after network is up):
  consume the rest of the metadata.
- `cloud-config`: applies `users`, `ssh_authorized_keys`, `write_files`,
  `packages`.
- `cloud-final` (last): runs `runcmd`, `bootcmd`, scripts in
  `/etc/cloud/cloud.cfg.d/`.

## Interaction with k8s-init.sh and worker-init.sh

`k8s-init.service` and `worker-init.service` carry `After=cloud-init.target`,
so when cloud-init is installed:

1. cloud-init's `cloud-final` stage completes (SSH keys placed,
   packages installed, runcmd executed).
2. `cloud-init.target` becomes active.
3. `k8s-init.sh` / `worker-init.sh` runs.

When cloud-init is **not** installed, `cloud-init.target` is unknown
and the `After=` is a silent no-op — k8s-init still runs after
`crio.service` and `network-online.target` as before. The default
image is unaffected.

If your user-data does something that affects kubelet / cri-o (e.g.,
writes a `/etc/crio/crio.conf.d/` drop-in), the ordering guarantees
that drop-in is in place before `kubeadm init` reads it.

### Hostname authority

cloud-init's hostname module is **authoritative** for worker nodes. If
your NoCloud seed declares a `local-hostname` in `meta-data` (or a
`hostname:` directive in `#cloud-config` user-data), cloud-init sets
it during the network stage — well before `worker-init.service`
fires. `worker-init.sh` then inspects the current hostname and only
falls back to its machine-id-derived `humbird-worker-<suffix>` name
when the existing hostname matches `localhost` / `localhost.*` (the
unseeded default). This means:

- `make deploy-cluster` (which seeds per-VM cloud-init with the
  `WORKER_NAMES` from your cluster config) ends up with workers
  joined to the cluster under exactly those names — `kubectl get
  nodes` shows `hbird-w1`, `hbird-w2`, etc., not the
  `humbird-worker-<machine-id>` pattern.
- `scripts/spawn-workers.sh` (the no-cloud-init worker primitive that
  `deploy-cluster.sh` falls back to when cloud-init is not in play —
  injects `worker-join.env`, never touches hostname) still produces
  unique node names via the `localhost*` fallback.

To override cloud-init's hostname from your own user-data, set it
through standard cloud-init mechanisms (`local-hostname` in
meta-data, or `hostname:` / `fqdn:` in `#cloud-config`); do not
expect `worker-init.sh` to do it for you.

## Caveats

- **ostree / bootc and /etc.** cloud-init writes to `/etc`, which on
  bootc is part of the deployment's `/etc` overlay. Changes persist
  across reboots of the current deployment but are **not** carried
  forward when `bootc upgrade` stages a new deployment; the staged
  deployment starts from the new image's `/etc` and re-runs
  cloud-init only if its instance-id changed (which on a fresh boot
  of an unchanged VM does not happen). For per-boot scripts, prefer
  `bootcmd` over `runcmd`.
- **No re-customization without instance-id bump.** The standard
  cloud-init "no-op on subsequent boots" rule applies — to force a
  re-run with a new user-data, change the seed's `instance-id`.
- **No public-key fetch from the metadata service.** The `NoCloud`
  datasource only reads what's on the seed; it does not call out to
  a metadata IP. This is intentional for the homelab use case but
  means a libvirt network without a metadata service still works.
- **Mixing with bib-baked users.** Both paths can coexist — the
  bib `[[customizations.user]]` user (default `core`) is present
  regardless, and cloud-init adds the user-data users on top. Pubkey
  files are merged, not overwritten.
- **Image size delta.** Expect roughly +30 MB compressed for the
  cloud-init package set. Not enough to matter for a 600 MB+ k8s
  image.
- **Build dependency.** Hummingbird's curated repo ships
  `cloud-init` itself but not all of its deps (xfsprogs, python3
  modules, dhcpcd, nc). The conditional build block adds a
  one-shot Fedora Rawhide repo just for the cloud-init install,
  then removes the repo file before the layer commits — the
  resulting image does not carry a dangling Rawhide
  configuration. This works uniformly across k8s and k8s-worker
  (both already have a permanent Rawhide repo for
  iptables/socat/etc, but the cloud-init block stays
  self-contained so it's resilient to reordering). The Fedora
  GPG keyring is imported and the repo runs with `gpgcheck=1`,
  so cloud-init's RPM dependencies are signature-verified at
  install time (#70).

## Disabling at runtime on an enabled image

If an operator built with `ENABLE_CLOUD_INIT=1` but a specific VM
should skip cloud-init:

```bash
# Pass via virt-install's --extra-args:
--extra-args 'cloud-init=disabled'

# Or post-boot:
sudo touch /etc/cloud/cloud-init.disabled
sudo systemctl reboot
```
