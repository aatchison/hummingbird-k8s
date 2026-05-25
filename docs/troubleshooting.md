# Troubleshooting

Operator-facing runbook of real failure modes encountered while building and
running Hummingbird-k8s, with the symptom we actually saw and the fix that
stuck. Each entry points at the PR that landed the fix (or hardened the area)
so you can read the deeper context if needed. For broader design rationale,
see [`../NOTES.md`](../NOTES.md).

## Image build

### systemd preset stripping at bib time

The Hummingbird base ships `99-default-disable.preset`, which runs during the
image-build stage and removes manual `multi-user.target.wants/` symlinks for
units the preset doesn't explicitly enable. A Containerfile that does
`RUN systemctl enable foo.service` (or worse, `ln -sf …`) silently loses the
enablement before the qcow2 is produced.

- Symptom: on the booted VM, `systemctl is-enabled kubelet.service` (or
  `k8s-init.service`, or the bootc-update timer) reports `disabled`, and the
  unit never starts at boot despite the Containerfile appearing to enable it.
- Fix: ship a lower-numbered preset file (e.g.
  `/usr/lib/systemd/system-preset/10-k8s.preset`) containing
  `enable kubelet.service` / `enable k8s-init.service`, and `RUN systemctl
  preset <unit>` at build time. Lower number wins over `99-default-disable`.
- Reference: [PR #47](https://github.com/aatchison/hummingbird-k8s/pull/47).

### libguestfs OS-introspection on bootc/ostree

Both `virt-customize -a foo.qcow2 …` and `guestfish -i -a foo.qcow2 …` rely
on libguestfs's OS-detection to mount the right filesystems. That detection
does not understand bootc/ostree layouts and bails out.

- Symptom:
  `virt-customize: error: no operating systems were found in the guest image`
  (same message from `guestfish -i`). The qcow2 is fine — the tool just
  refuses to mount it.
- Fix: skip introspection. Use `guestfish` in non-`-i` mode, list partitions
  with `run` + `list-filesystems`, mount the root partition raw, then write
  to the active ostree deployment's etc dir at
  `/ostree/deploy/<stateroot>/deploy/<commit>.0/etc/`. `spawn-workers.sh`
  does this to inject per-worker join tokens.
- Reference: [PR #40](https://github.com/aatchison/hummingbird-k8s/pull/40).

### bootc qcow2 build under a containerized runner

When `bootc-image-builder` runs inside a container (e.g. a self-hosted
GitHub Actions runner whose podman uses the `overlay` storage driver),
nested overlay mounts fail.

- Symptom:
  `Error: kernel does not support overlay fs: overlay is not supported over
  overlayfs` from the bib invocation; the qcow2 conversion step exits
  non-zero before producing any output.
- Fix: export `STORAGE_DRIVER=vfs` before invoking `make deploy-cluster`
  (or the underlying `scripts/build-*.sh`). `vfs` is slower but always works.
- Caveat: setting `STORAGE_DRIVER=vfs` alone is not enough if a previous
  podman run already initialized the libpod database with `overlay`.
  Symptom is the misleading error:
  `User-selected graph driver "vfs" overwritten by graph driver "overlay"
  from database — delete libpod local files to resolve`. The integration
  tests work around this by routing podman through a fresh, per-run
  `--root`/`--runroot` and passing `--storage-driver vfs` on every
  invocation. For ad-hoc builds either wipe the local libpod database
  (`sudo podman system reset --force` — destructive) or use a dedicated
  `--root`.
- Reference: [PR #127](https://github.com/aatchison/hummingbird-k8s/pull/127),
  [issue #139](https://github.com/aatchison/hummingbird-k8s/issues/139).

### sha256sum filename mismatch

Upstream `.sha256sum` files embed the original filename next to the digest.
`sha256sum -c` is filename-sensitive: if your local copy is named
differently, it can't find the file even though the bytes match.

- Symptom: `sha256sum: <local-name>: No such file or directory` followed by
  `WARNING: 1 listed file could not be read`, or
  `<expected-name>: FAILED open or read`.
- Fix: download with the upstream basename
  (`curl -o "$(basename "$URL")" "$URL"`), or skip the checksum file and
  compare the digest directly:
  `[ "$(sha256sum local.tar.gz | cut -d' ' -f1)" = "$EXPECTED" ]`.
- Reference: [PR #56](https://github.com/aatchison/hummingbird-k8s/pull/56)
  follow-up.

## Cluster bring-up

### cilium-cli `$HOME` missing under systemd

`cilium-cli` resolves a cache dir from `$XDG_CACHE_HOME` or `$HOME`. Under
`systemd`, neither is set for a unit running as `root`, so the `cilium
install` step inside `k8s-init.service` aborts immediately after `kubeadm
init` succeeds — leaving a half-initialised cluster with no CNI.

- Symptom (`journalctl -u k8s-init.service`):
  `Error: neither $XDG_CACHE_HOME nor $HOME are defined`, followed by
  `k8s-init.service: main process exited, code=exited, status=1/FAILURE`.
  `kubectl get nodes` shows the CP as `NotReady` indefinitely.
- Fix: export both near the top of `k8s-init.sh` before invoking
  `cilium install`:

  ```bash
  export HOME=/root
  export XDG_CACHE_HOME=/var/cache
  ```

  Already in k8s/v0.1.10+ — upgrade the CP image if you're seeing this.
- Reference: [PR #61](https://github.com/aatchison/hummingbird-k8s/pull/61).

### kubeadm v1beta4 `--cri-socket` + `--config` conflict

`kubeadm` v1.31+ rejects passing the same setting in both the CLI and the
v1beta4 config YAML. Earlier versions tolerated the duplication.

- Symptom:
  `cannot specify both '--cri-socket' and 'criSocket' in the config file`
  from `kubeadm init`; `k8s-init.service` exits 1 before any control-plane
  pods come up.
- Fix: drop `--cri-socket` from the CLI invocation; keep
  `nodeRegistration.criSocket: unix:///var/run/crio/crio.sock` in the
  v1beta4 InitConfiguration. The config is the source of truth.
- Reference: [PR #33](https://github.com/aatchison/hummingbird-k8s/pull/33).

### Cilium pods CrashLoopBackOff after CNI swap

After replacing flannel with Cilium (or on a minimal base image), the
Cilium daemonset can crashloop if the kernel lacks the BPF features Cilium
needs.

- Symptom: `kubectl -n kube-system get pods -l k8s-app=cilium` shows
  `CrashLoopBackOff`; pod logs mention BPF program load failures, missing
  `CONFIG_BPF_SYSCALL`, or `cgroup v2` requirements. Nodes stay `NotReady`
  with `network plugin is not ready` events.
- Fix: confirm the kernel ships the needed features
  (`zgrep -E 'CONFIG_(BPF|CGROUP_BPF|NET_CLS_BPF)' /proc/config.gz`).
  Hummingbird's base kernel currently has them all; if a future base drops
  one, pin the previous base digest. For a one-off diagnosis:
  `kubectl -n kube-system describe pod -l k8s-app=cilium`.
- Reference: [PR #56](https://github.com/aatchison/hummingbird-k8s/pull/56).

### Worker NotReady for ~60s after join

A freshly joined worker reports `NotReady` until the Cilium daemonset
schedules its agent pod there and the CNI plugin reports healthy.

- Symptom: `kubectl get nodes` shows the new worker as `NotReady` for up to
  ~60s; `kubectl -n kube-system get pods -o wide` shows the Cilium agent
  pod still `ContainerCreating` on that node.
- Fix: wait. Re-check `kubectl get nodes` after ~60s. If the worker is
  still `NotReady` after 2-3 minutes, fall through to the Cilium
  CrashLoopBackOff entry above.

## Client / verification

### kubectl tunnel times out

`scripts/kubectl-k8s.sh` runs a backgrounded `ssh -fNL 6443:127.0.0.1:6443`
to the KVM host. Re-running the script while a stale tunnel from a previous
deploy is still alive (against a now-rebuilt VM) leaves you talking to the
old endpoint.

- Symptom: `Ncat: TIMEOUT.` from the kubectl container, or `kubectl` itself
  reports `Unable to connect to the server: dial tcp 127.0.0.1:6443:
  connect: connection refused` after a recent `make deploy-cluster`.
- Fix: kill the stale tunnel and start fresh:

  ```bash
  pkill -f "ssh.*6443.*$KVM_HOST"
  rm -f /tmp/k8s-kubeconfig
  make kubectl ARGS='get nodes'
  ```

### Verifier scripts can't reach VMs from a non-KVM host

`scripts/verify-*.sh` originally did `ssh root@192.168.122.x`, which only
works from inside the libvirt NAT subnet (i.e. on the KVM host itself).
Running them from a laptop, or from a different host on the LAN, fails to
route.

- Symptom:
  `ssh: connect to host 192.168.122.10 port 22: No route to host` (or
  `Network is unreachable`) from `make verify-hardening` /
  `make verify-encryption` when invoked off-host.
- Fix: set `KVM_HOST=<ssh-alias-of-kvm-host>` in the environment. The
  verifier scripts use `-J "$KVM_HOST"` to ProxyJump through the host.
- Reference: [PR #63](https://github.com/aatchison/hummingbird-k8s/pull/63).

## Internal / agent-host

### Subagents can't poll across backgrounded waits

A subagent that backgrounds a long-running bash loop (`while true; do …;
sleep N; done &`) gets terminated when the parent's turn ends; the loop
never reports back. The same applies to `tmux send-keys` plus a later
`tmux capture-pane` from a fresh subagent — state isn't preserved.

- Symptom: the subagent appears to "wait" but its scheduled re-check never
  fires; the parent sees no result.
- Fix: keep polls in the foreground (`until <cond>; do sleep N; done`
  without `&`) inside one subagent turn, or split work across multiple
  subagent dispatches so each one fully resolves before exiting. This is an
  agent-host convention, not a repo bug.
