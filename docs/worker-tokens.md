# Per-VM short-TTL kubeadm join tokens

## What changed

The `hummingbird-k8s-worker` template image no longer contains a kubeadm join
token. `build-worker.sh` produces a pure template qcow2 — the same bits ship
to every host. `spawn-workers.sh` mints a fresh, short-TTL token per VM and
injects it into that VM's cloned qcow2 at `/etc/hummingbird/worker-join.env`
just before `virt-install`. On first boot, `worker-init.sh` reads that file
and runs the join exactly as before.

## Why

Previously, `build-worker.sh` ran `kubeadm token create --ttl 0
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

`spawn-workers.sh` SSHes to the control-plane VM as `root` to mint each
fresh join token. The CP image ships with root SSH enabled
(`ENABLE_ROOT_SSH=1`, the default in `config.example.sh`); disabling it
breaks per-VM token minting and `spawn-workers.sh` will fail with an
empty/garbled response from the CP.

We use root rather than `core` + sudo because the bootc CP image is
sudoless by design (the wheel user cannot escalate without a password),
and `kubeadm token create` needs to read `/etc/kubernetes/pki/`.

## Adding a worker

The control plane VM must be running. From the KVM host:

```sh
sudo bash spawn-workers.sh 1
```

That clones the template, asks the running CP for a fresh ~2h token, injects
it into the new VM's disk, and starts the VM. Repeat with a higher count to
add several at once. To add a single worker alongside existing workers, set
the count appropriately (the script skips names already defined in libvirt).

Override the token TTL or the CP VM name if needed:

```sh
sudo TOKEN_TTL=30m CP_VM_NAME=hummingbird-k8s bash spawn-workers.sh 1
```

## How injection works

`spawn-workers.sh` prefers `guestfish` and falls back to `virt-customize`
(both ship in `libguestfs-tools-c` / `libguestfs-tools`). If neither is
installed, it attempts `dnf install -y libguestfs-tools-c` once. Both tools
mount the qcow2 offline, write `/etc/hummingbird/worker-join.env` (mode
0600, owner root), and unmount cleanly. No cloud-init, no second CD-ROM,
no extra metadata server.

`guestfish` is preferred because it mounts the raw root partition directly
and does not require libguestfs OS introspection. `virt-customize` relies
on introspection to identify the guest OS layout, which fails on the
bootc/ostree-based Hummingbird worker image with `no operating systems
were found in the guest image`. `guestfish` sidesteps that entirely by
just writing to the filesystem.

## What if someone boots the bare template?

`worker-init.service` carries `ConditionPathExists=/etc/hummingbird/worker-join.env`,
so on a template qcow2 (no token injected) the join unit silently skips
instead of failing the boot. `worker-init.sh` itself also prints an
actionable error pointing the operator at `spawn-workers.sh`.
