# etcd encryption at rest

This document describes how `hummingbird-k8s` enables encryption at rest
for Kubernetes Secrets and ConfigMaps stored in etcd, and the operational
caveats that follow.

## What this does

`k8s-init.sh` runs once per VM (gated by `/var/lib/k8s-init.done`). On
that first run it:

1. Generates a fresh 32-byte random key, base64-encoded.
2. Writes `/etc/kubernetes/encryption-config.yaml` (mode 0600,
   root-owned) declaring an `EncryptionConfiguration` v1 with an
   `aesgcm` provider followed by `identity` as a read-fallback for
   any unencrypted rows that existed before this provider was active.
3. Writes `/etc/kubernetes/kubeadm-init.yaml`
   (`kubeadm.k8s.io/v1beta4`) wiring the apiserver to that file via
   `--encryption-provider-config` and mounting it into the static
   pod.
4. Runs `kubeadm init` with that config.

After init, anything the apiserver writes to etcd for `secrets` and
`configmaps` is AES-GCM encrypted. Verify by running
`/usr/libexec/verify-encryption.sh` on the CP node — it creates a probe
Secret, reads it raw from etcd, and asserts the blob starts with
`k8s:enc:aesgcm:`.

## Verifying encryption

The verifier is baked into the k8s control-plane image at
`/usr/libexec/verify-encryption.sh`, so post-deploy verification is just:

```
ssh root@<cp> /usr/libexec/verify-encryption.sh
```

No `scp` from the host repo is needed — the script ships with every
redeploy. The source of truth still lives at `scripts/verify-encryption.sh`
in this repo; `containers/k8s/Containerfile` copies it into the image at build time.

The script reads the probe Secret back out of etcd to confirm the
on-disk envelope. It tries, in order:

1. **Local `etcdctl`** on the CP host (rare on the bootc image; only
   present if you installed `etcd` package manually).
2. **Host-side `crictl exec`** into the running etcd container. This
   is the supported path on Kubernetes 1.31, whose etcd static pod
   image is `registry.k8s.io/etcd:*-distroless` and contains no
   shell — `kubectl exec etcd-<node> -- sh -c '...'` will always
   fail with `exec: "sh": not found in $PATH`. The script runs
   `crictl exec <etcd-container-id> etcdctl ...` directly with no
   intermediate shell.

Both paths must run as root on the CP VM (crictl needs the CRI socket
and etcdctl needs the etcd PKI). Expected output:

```
[verify-encryption] OK: secret in etcd is encrypted (prefix=k8s:enc:aesgcm:)
```

The full prefix on the encrypted row is
`k8s:enc:aesgcm:v1:bootstrap:<binary>` — `bootstrap` is the keyname we
set in `k8s-init.sh`.

## Algorithm choice

We use `aesgcm` (authenticated encryption) per the current Kubernetes
documentation recommendation. The previous draft used `aescbc`, which
is not authenticated; switching costs nothing here because the cluster
is initialized fresh.

## Key lifecycle

- The encryption key is generated **inside the VM** at first boot. It
  is never baked into the bootc image.
- The key file is 0600 root:root on the VM root filesystem.
- There is no key rotation in this round. Rotating requires editing
  the EncryptionConfiguration to add a new key as the first provider
  entry, restarting the apiserver, then rewriting every Secret /
  ConfigMap (`kubectl get secrets -A -o json | kubectl replace -f -`)
  before removing the old key. A follow-up issue will cover that.
- There is no KMS provider in this round. Single-node lab scope.

## Upgrade path for existing VMs

Important caveat: VMs that were initialized before this change keep
their unencrypted etcd. `k8s-init.service` is one-shot and gated by
`/var/lib/k8s-init.done`, so a `bootc upgrade` to a newer image does
not re-run `kubeadm init` and does not retroactively encrypt existing
rows.

To migrate an existing VM:

1. Drain workloads or accept downtime.
2. Snapshot etcd (or back up the whole VM disk).
3. Rebuild the VM from the new image (recommended, simplest).
4. Or, manually: write `/etc/kubernetes/encryption-config.yaml`,
   patch the apiserver static pod manifest to load it, restart the
   apiserver, then rewrite every Secret / ConfigMap so they get
   re-encrypted on the way in.

New VMs built from images that include this change are encrypted from
first boot — no migration needed.

## Resources covered

Only `secrets` and `configmaps` are encrypted. Expanding to other
resources (e.g. `*`) is deferred — it requires re-encrypting all
existing rows and is out of scope for this round.

## Idempotency

`k8s-init.sh` includes a recovery step: if `/etc/kubernetes/admin.conf`
is missing but `/etc/kubernetes/kubeadm-init.yaml` exists, a previous
`kubeadm init` died partway. The script runs `kubeadm reset --force`
and removes the half-written config files so the retry starts clean.

## Gotchas

### `--config` and `--cri-socket` are mutually exclusive (kubeadm v1.31+)

When `kubeadm init` is invoked with `--config=`, kubeadm v1.31 rejects
any CLI flag whose value is also expressible in the configuration
file. Passing `--cri-socket=unix:///var/run/crio/crio.sock` alongside
`--config=` now hard-errors with:

```
can not mix '--config' with arguments [cri-socket]
```

The canonical place to set the CRI socket under `--config` is the
`InitConfiguration.nodeRegistration.criSocket` field of the generated
kubeadm YAML — which `k8s-init.sh` already writes (see the
`nodeRegistration:` block in the heredoc). Do not also pass
`--cri-socket` on the CLI.

Note that `kubeadm reset` is unaffected and still accepts (and in
fact needs) `--cri-socket` when multiple runtimes could be present,
because `reset` does not consume a config file.
