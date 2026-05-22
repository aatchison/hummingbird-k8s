# hummingbird-test

Build Fedora Hummingbird-based bootc VMs with Kubernetes baked in. Two flavors:

- **`hummingbird-k3s`** — Hummingbird base + k3s single-binary install. Lightweight single-node.
- **`hummingbird-k8s`** — Hummingbird base + upstream `kubelet`/`kubeadm`/`kubectl` + `cri-o`. Real Kubernetes control plane.
- **`hummingbird-k8s-worker`** — Worker variant that auto-joins the `hummingbird-k8s` control plane on first boot.

All three are bootc images (`quay.io/hummingbird-community/bootc-os` base), built with `podman build`, converted to `qcow2` via `bootc-image-builder`, and run as KVM VMs via libvirt (`qemu:///system`).

See [`NOTES.md`](NOTES.md) for design decisions, gotchas, and operational notes.

## Quick start (single-node k8s)

On the KVM host:

```bash
sudo bash redo-k8s.sh                  # build + define `hummingbird-k8s` (control plane)
sudo bash redo-workers.sh 2            # build worker image + spawn 2 workers
```

From a client machine:

```bash
./kubectl-k8s.sh get nodes             # uses SSH tunnel + container kubectl
```

## CI / Releases

GitHub Actions builds an OCI image for each flavor and pushes to GHCR on tagged releases:

| Tag pattern | Published image |
|---|---|
| `k3s/vX.Y.Z` | `ghcr.io/<OWNER>/hummingbird-k3s:vX.Y.Z` |
| `k8s/vX.Y.Z` | `ghcr.io/<OWNER>/hummingbird-k8s:vX.Y.Z` |
| `worker/vX.Y.Z` | `ghcr.io/<OWNER>/hummingbird-k8s-worker:vX.Y.Z` |

Bump a flavor independently: `git tag k3s/v0.2.0 && git push --tags`.
