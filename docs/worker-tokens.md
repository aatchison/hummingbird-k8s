# Per-VM short-TTL kubeadm join tokens

## What changed

The `hummingbird-k8s-worker` template image no longer contains a kubeadm join
token. `scripts/build-worker.sh` produces a pure template qcow2 — the same bits
ship to every host. `scripts/spawn-workers.sh` mints a fresh, short-TTL token
per VM and injects it into that VM's cloned qcow2 at
`/etc/hummingbird/worker-join.env` just before `virt-install`. On first boot,
the in-image `worker-init.sh` reads that file and runs the join exactly as before.

## Why

Previously, `scripts/build-worker.sh` ran `kubeadm token create --ttl 0
--print-join-command` against the control plane and baked the resulting
never-expiring join command into the worker image. That meant:

- The published image embedded a long-lived secret valid until the CP CA
  rotates or the cluster is rebuilt.
- Every worker spawned from one build shared the same token (one leak =
  unbounded blast radius).
- Anyone who pulled the OCI image (or the qcow2) could join the cluster.

The new flow:

- Image carries no token. Compromising the registry/image yields nothing
  useful against the cluster.
- Each VM gets its own short-TTL token (`--ttl 2h` by default, override via
  the `TOKEN_TTL` env var). After two hours, an unused token is dead.
- Token never touches the image registry or any shared artifact — it lives
  only inside one VM's per-instance qcow2.

## Operational requirements

`scripts/spawn-workers.sh` SSHes to the control-plane VM as `root` to mint each
fresh join token. The CP image ships with root SSH enabled
(`ENABLE_ROOT_SSH=1`, the default in `config.example.sh`); disabling it
breaks per-VM token minting and `scripts/spawn-workers.sh` will fail with an
empty/garbled response from the CP.

We use root rather than `core` + sudo because the bootc CP image is
sudoless by design (the wheel user cannot escalate without a password),
and `kubeadm token create` needs to read `/etc/kubernetes/pki/`.

## Adding a worker

The control plane VM must be running. Adding workers happens as part of
`make deploy-cluster` (the script grows `WORKER_NAMES`), or you can call
the worker primitive directly from the KVM host for ad-hoc additions:

```sh
sudo bash scripts/spawn-workers.sh 1
```

That clones the template, asks the running CP for a fresh ~2h token, injects
it into the new VM's disk, and starts the VM. Repeat with a higher count to
add several at once. The script skips names already defined in libvirt, so
re-running it is idempotent.

Override the token TTL or the CP VM name if needed:

```sh
sudo TOKEN_TTL=30m CP_NAME=hummingbird-k8s bash scripts/spawn-workers.sh 1
```

## How injection works

`scripts/spawn-workers.sh` prefers `guestfish` and falls back to `virt-customize`
(both ship in `libguestfs-tools-c` / `libguestfs-tools`). If neither is
installed, it attempts `dnf install -y libguestfs-tools-c` once. Both tools
mount the qcow2 offline, write `/etc/hummingbird/worker-join.env` (mode
0600, owner root), and unmount cleanly. No cloud-init, no second CD-ROM,
no extra metadata server.

`guestfish` is preferred because it can mount the raw root partition
directly. `virt-customize` (and `guestfish -i`) rely on libguestfs OS
introspection to identify the guest OS layout, which fails on the
bootc/ostree-based Hummingbird worker image with `no operating systems
were found in the guest image`. `scripts/spawn-workers.sh` therefore invokes
`guestfish` without `-i`: it explicitly mounts `/dev/sda4` (the root
partition), discovers the active ostree deployment dir
(`/ostree/deploy/<stateroot>/deploy/<commit>.0/`) by listing the on-disk
ostree tree, and writes `worker-join.env` into that deployment's `/etc`.
At boot the kernel mounts the deployment dir as `/`, so the file appears
at the live `/etc/hummingbird/worker-join.env`. For non-ostree images
the script falls back to writing to the partition's `/etc` directly.

## What if someone boots the bare template?

`worker-init.service` carries `ConditionPathExists=/etc/hummingbird/worker-join.env`,
so on a template qcow2 (no token injected) the join unit silently skips
instead of failing the boot. `worker-init.sh` itself also prints an
actionable error pointing the operator at `scripts/spawn-workers.sh`.
