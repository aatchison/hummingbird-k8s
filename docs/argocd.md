# Registering a Hummingbird cluster with ArgoCD

`scripts/export-argocd.sh` (also reachable as `make export-argocd`) produces
a kubeconfig you can hand to `argocd cluster add`. It exists because
ArgoCD's "add cluster" flow is a one-shot bootstrap — it doesn't keep the
credentials you give it; instead it uses them once to create its own
scoped ServiceAccount in the target cluster and then authenticates with
that SA's token. So we don't need an ArgoCD-specific credential. We just
need a valid, usable-once kubeconfig pointed at a reachable apiserver.

## What the export contains

The output file is a verbatim copy of `/etc/kubernetes/admin.conf` from
the control plane, with two surgical rewrites:

1. **`server:` URL** — kubeadm bakes the CP node's local IP into
   `admin.conf`. That IP often isn't routable from where ArgoCD runs
   (different subnet, behind a load balancer, accessed via a DNS name).
   The script defaults to `https://<CP_IP>:6443` derived from
   `cluster.local.conf` / libvirt, and `--server URL` lets you override
   that to the address ArgoCD will actually use.

2. **Cluster / context / user `name:`** — kubeadm emits the defaults
   `cluster=kubernetes`, `user=kubernetes-admin`,
   `context=kubernetes-admin@kubernetes`. If two Hummingbird clusters
   both used these defaults, dropping one kubeconfig on top of another
   (or merging them) would silently overwrite the prior entries. The
   script renames all three to `hummingbird-<CP_NAME>` (override with
   `--context-name`).

TLS material (`certificate-authority-data`,
`client-certificate-data`, `client-key-data`) is left untouched.

## Why this credential is sensitive

`admin.conf` is the kubeadm break-glass credential. It carries:

- The cluster CA bundle (lets you trust the apiserver's serving cert).
- A client cert + key signed by that CA with `O=system:masters`. The
  `system:masters` group is hardcoded in the apiserver authorizer as
  "always allow" — full cluster-admin, bypassing RBAC.

This is why the script:

- Writes the file with `mode 0600` and uses a `umask 077` temp file as
  the staging path (never world-readable, even transiently).
- Refuses to print the contents to stdout.
- Refuses to overwrite an existing output file unless `--force`.

After `argocd cluster add` succeeds, ArgoCD authenticates with the SA
token it minted in the target cluster, not with `admin.conf`. **Delete
the exported file, or chmod-0600-store it somewhere offline**, once the
cluster is registered. There's no reason to keep it on a workstation.

## If the exported file leaks

Treat it as a full-cluster credential compromise. The kubeadm-standard
recovery is to rotate the cluster CA + client certs on the CP. From the
CP node:

```bash
# Rotate every kubeadm-managed cert (apiserver, controller-manager,
# scheduler, admin.conf, etc.). This re-signs the admin client cert.
sudo kubeadm certs renew all

# Restart the static-pod control plane to pick up the new certs.
sudo systemctl restart kubelet
```

`kubeadm certs renew` re-signs the leaf certs but keeps the existing
cluster CA. If you suspect the CA private key (in `/etc/kubernetes/pki/`)
also leaked — the exported `admin.conf` does NOT contain the CA private
key, only the CA bundle — you have to rebuild the cluster from scratch;
there is no in-place CA rotation in upstream kubeadm.

For the etcd at-rest encryption key (a separate credential class), see
[etcd-encryption.md](etcd-encryption.md) and
`scripts/rotate-etcd-encryption-key.sh`.

## Sanity check before handing the file to ArgoCD

```bash
KUBECONFIG=./argocd-kubeconfig.yaml kubectl get nodes
```

If that returns the nodes list, the file is valid. If it fails with a
TLS or "connection refused" error, the `--server URL` you passed isn't
reachable from where you're running `kubectl` — pass a `--server` flag
that matches the network path ArgoCD itself will take.
