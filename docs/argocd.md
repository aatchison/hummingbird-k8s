# Registering a Hummingbird cluster with ArgoCD

`scripts/export-argocd.sh` (also reachable as `make export-argocd`) produces
a kubeconfig you can hand to `argocd cluster add`. It exists because
ArgoCD's "add cluster" flow is a one-shot bootstrap — it doesn't keep the
credentials you give it; instead it uses them once to create its own
scoped ServiceAccount in the target cluster and then authenticates with
that SA's token. So we don't need an ArgoCD-specific credential. We just
need a valid, usable-once kubeconfig pointed at a reachable apiserver.

`make get-kubeconfig` (issue #195) wraps the same primitive for daily
operator use — both targets call `scripts/export-argocd.sh`; the only
differences are the defaults. `export-argocd` writes
`argocd-kubeconfig.yaml` and prefixes the context with `hummingbird-` so
it can sit alongside a regular kubeconfig without name collisions on
`argocd cluster add`. `get-kubeconfig` writes `kubeconfig.yaml` and uses
`CP_NAME` directly (no prefix), so `kubectl --context=<CP_NAME>` matches
the libvirt domain name the operator already uses. The fetch+rewrite
logic lives in one script — there is no separate `scripts/get-kubeconfig.sh`.

## Prerequisites

Where to run `argocd cluster add`: on the workstation that has already
authenticated to your ArgoCD control plane via `argocd login
<argocd-server>`. The ArgoCD CLI sends the kubeconfig contents to that
server over the authenticated session — the server uses the file once
to mint a scoped ServiceAccount in the target cluster, then discards it.

Concretely:

1. On your workstation, log in to ArgoCD (one-time, until the session
   token expires):

   ```bash
   argocd login argocd.example.com   # follow the prompt for SSO / token
   argocd context                    # confirm the active context
   ```

2. Produce the kubeconfig from this repo:

   ```bash
   make export-argocd CONFIG=cluster.local.conf
   ```

3. Hand it to ArgoCD from the same workstation:

   ```bash
   argocd cluster add hummingbird-<CP_NAME> \
       --kubeconfig ./argocd-kubeconfig.yaml
   ```

The exported file does NOT need to be present on the ArgoCD server — the
ArgoCD CLI uploads it as part of the registration RPC. After
registration, delete the local file (or store it 0600 offline). See "Why
this credential is sensitive" below.

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

Treat it as a full-cluster credential compromise — but be honest about
what recovery actually means.

**kubeadm has no CRL.** The Kubernetes apiserver, as kubeadm configures
it, trusts any unexpired certificate signed by the cluster CA. There is
no revocation list, no OCSP, no "this serial number is dead now" knob.
Running `sudo kubeadm certs renew all` re-issues the local admin client
cert with a new key — but **the leaked cert remains valid until its
notAfter date** (the default kubeadm client-cert lifetime is 1 year).
Anyone holding the leaked file can still authenticate against the CP
until that expiry passes.

The only true revocation for a leaked `admin.conf` is to **rebuild the
cluster** with a fresh CA. Workarounds short of that:

- **Network-block the apiserver from where the attacker can reach it**
  (firewall the CP, take the cluster off the routable network) until you
  can rebuild or until the leaked cert expires.
- **Rotate the cluster CA** — kubeadm has no in-place CA-rotation
  command, so this is a manual + destructive procedure (regenerate
  `/etc/kubernetes/pki/ca.*`, re-sign every leaf cert, restart every
  kubelet with the new CA bundle). In practice this is the same effort
  as a rebuild.
- **Run `kubeadm certs renew all`** anyway — it doesn't revoke the
  leaked cert, but it does give you a fresh local credential to keep
  using, and it pushes the *next* compromise window forward by another
  year:

  ```bash
  # CP node:
  sudo kubeadm certs renew all
  sudo systemctl restart kubelet
  ```

If you suspect the CA private key (in `/etc/kubernetes/pki/ca.key`)
also leaked — the exported `admin.conf` does NOT contain it, only the CA
bundle — rebuild is unconditional. There is no in-place CA rotation in
upstream kubeadm.

For the etcd at-rest encryption key (a separate credential class), see
[etcd-encryption.md](etcd-encryption.md) and
`scripts/rotate-etcd-encryption-key.sh`.

## Cert lifecycle and re-exporting

kubeadm issues `/etc/kubernetes/admin.conf`'s client cert with the
default lifetime of **1 year**. After that, the cert in the exported
file no longer authenticates — even though the kubeconfig is otherwise
structurally valid.

ArgoCD itself is unaffected: it discarded the original credential
immediately after `argocd cluster add` and has been authenticating with
its own ServiceAccount token ever since. The SA token has no fixed
expiry tied to admin.conf.

You only need to re-export if you intend to **re-register the cluster**
with ArgoCD (new ArgoCD instance, restored from backup, etc.):

```bash
# CP node: refresh every kubeadm-managed cert (issues new 1y leaves).
sudo kubeadm certs renew all
sudo systemctl restart kubelet

# Workstation: produce a fresh argocd-kubeconfig.yaml from the new
# admin.conf.
make export-argocd CONFIG=cluster.local.conf FORCE=1
```

If you forget the `FORCE=1`, the script refuses to overwrite the stale
file — by design.

## Sanity check before handing the file to ArgoCD

```bash
KUBECONFIG=./argocd-kubeconfig.yaml kubectl get nodes
```

If that returns the nodes list, the file is valid. If it fails with a
TLS or "connection refused" error, the `--server URL` you passed isn't
reachable from where you're running `kubectl` — pass a `--server` flag
that matches the network path ArgoCD itself will take.

## Audit trail: what `argocd cluster add` does in the cluster

The kubeconfig you hand to `argocd cluster add` is used ONCE, by the
ArgoCD CLI on your workstation, to create three objects in the target
cluster's `argocd-system`-style namespace (the exact name depends on
ArgoCD's own deployment; it's the namespace ArgoCD has configured
itself to register cluster credentials into). The apiserver records
this as authenticated activity by the `kubernetes-admin` user (kubeadm's
built-in client cert subject) in the `system:masters` group.

If you have apiserver audit logging enabled per `docs/security-hardening.md`,
the corresponding entries land in `/var/log/kubernetes/k8s-audit.log` on
the CP. To find them after a registration:

```bash
# Run on the CP node:
sudo grep -E \
    'kubernetes-admin.*(ServiceAccount|ClusterRoleBinding|Secret).*argocd' \
    /var/log/kubernetes/k8s-audit.log
```

You should see three RequestResponse entries with
`"verb":"create"`:

| Verb | Resource | Purpose |
| --- | --- | --- |
| `create` | `serviceaccount` | The SA ArgoCD will authenticate as henceforth. |
| `create` | `clusterrolebinding` | Binds the SA to the cluster-admin (or scoped) role. |
| `create` | `secret` | Holds the SA token + CA bundle ArgoCD reads back. |

The User field will be `kubernetes-admin`, the Group will include
`system:masters`. There is no separate ArgoCD-specific identity at this
stage — the registration is performed AS admin. After this one
bootstrap, all subsequent ArgoCD-driven apiserver traffic comes from
the SA above (its name typically includes `argocd-manager`).

If you see additional `kubernetes-admin` audit entries after the
registration window closed, treat that as a credential-leak signal
(the exported file should have been deleted; see the leak playbook).

## HA / load-balanced control planes

In an HA kubeadm cluster, `/etc/kubernetes/admin.conf` on each CP has a
`server:` URL pointing at the load-balanced apiserver endpoint (the
`controlPlaneEndpoint` you configured at `kubeadm init` time), NOT at
the individual CP node's local IP. The export inherits that. Don't
override it with `--server https://<single-CP-IP>:6443` — that pins
ArgoCD to one CP node and defeats HA. Pass `--server` only when:

- The cluster is single-CP (this repo's default), AND
- The libvirt-assigned CP IP isn't reachable from the ArgoCD pod, AND
- You have a stable address (LB, ingress, DNS name) that IS.

In all other cases, take the default and let the kubeconfig keep the
URL kubeadm put there.

## ProxyJump via the KVM host

When the operator workstation can't directly reach the CP — most often
because the CP is on a libvirt NAT subnet that's not routable outside
the KVM host — `scripts/export-argocd.sh` can tunnel its SSH session
through the KVM host. Two equivalent ways to enable it:

```bash
# Env var (matches scripts/kubectl-k8s.sh and backup-etcd.sh):
KVM_HOST=geary make export-argocd CONFIG=cluster.local.conf

# Explicit flag:
make export-argocd CONFIG=cluster.local.conf PROXY_JUMP=geary
```

Under the hood this adds `-o ProxyJump=$HOST` to the SSH option set
used to pull `admin.conf` from the CP. The flag wins when both are set.
This affects only the *fetch* — the resulting kubeconfig still embeds
the `--server` URL ArgoCD itself will use (which has nothing to do
with how you fetched the file), so you typically also want
`--server https://<reachable-address>:6443` when the CP isn't directly
reachable from ArgoCD either.

Same pattern, three different surfaces — `KVM_HOST` is recognized by
`scripts/kubectl-k8s.sh`, `scripts/backup-etcd.sh`, and
`scripts/export-argocd.sh`. If your workstation needs the ProxyJump for
one, it almost certainly needs it for all three.
