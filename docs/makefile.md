# Makefile cheatsheet

The top-level `Makefile` is a thin wrapper over the driver scripts under
`scripts/`. Both the `make` targets and the underlying scripts still work —
the Makefile just gives operators a stable, discoverable entrypoint. Run
`make help` to print the full cheatsheet, or browse `scripts/` directly.

## Common flows

Full fresh upstream-k8s deploy on a clean KVM host:

```bash
sudo make k8s                 # build + define + start the control plane
sudo make workers COUNT=2     # build worker image and spawn 2 workers
make verify-all               # verify-encryption + verify-hardening + verify-app-deploy
```

k3s single-node:

```bash
sudo make k3s
```

Ad-hoc `kubectl` from the client (needs `KVM_HOST` set, see `config.example.sh`):

```bash
make nodes
make kubectl ARGS='get pods -A'
```

Tear it all down:

```bash
sudo make clean               # destroys + undefines VMs and removes local images
```

## Variables

| Variable   | Default                       | Used by                       |
| ---        | ---                           | ---                           |
| `COUNT`    | `2`                           | `workers`, `spawn`            |
| `ARGS`     | empty                         | `kubectl`                     |
| `POOL_DIR` | `/var/lib/libvirt/images`     | `clean-vms`                   |
| `KVM_HOST` | unset                         | `kubectl` / `nodes`           |

Any other env vars honored by `config.local.sh` (e.g. `VM_USER`,
`APISERVER_EXTRA_SANS`) flow through to the underlying scripts unchanged.
