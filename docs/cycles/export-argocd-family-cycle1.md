# export-argocd-family — live-validate cycle 1

Gates removal of the corresponding bash twins per epic #353. Refs #288.

- **Cycle ran**: 2026-05-27T16:18:49-07:00
- **HEAD SHA**: `c3425a030e4bd07ba8154e330151e000abcee633`
- **Cluster**: live geary cluster, `hbird-cp1` (`192.168.122.212`) + `hbird-w1` + `hbird-w2`, all `Ready` (8h uptime)
- **Topology config**: `cluster.local.conf` (CP_NAME=hbird-cp1, KVM_HOST=geary)

This artifact mirrors the proving-ground shape established by PR #325 (#322 cycle 1):
ONE end-to-end pair of executions per command in the class, captured side-by-side,
with divergences explained and gated by an explicit verdict.

## Class members

The `export-argocd` family covers four commands — three thin SSH-wrapped kubectl
helpers (`get-kubeconfig`, `nodes`, `kubectl`) plus the ArgoCD-export variant of
the same SSH+fetch+rewrite shape (`export-argocd`). All four are READ-ONLY against
the cluster and gate `scripts/{export-argocd,kubectl-k8s}.sh` removal.

| Command         | Bash twin                                            | Rust subcommand              |
|-----------------|------------------------------------------------------|------------------------------|
| `export-argocd` | `scripts/export-argocd.sh` (via `make export-argocd`)| `hbird export-argocd`        |
| `get-kubeconfig`| `scripts/export-argocd.sh` (via `make get-kubeconfig`)| `hbird get-kubeconfig`      |
| `nodes`         | `scripts/kubectl-k8s.sh get nodes` (via `make nodes`)| `hbird nodes`                |
| `kubectl`       | `scripts/kubectl-k8s.sh ARGS` (via `make kubectl`)   | `hbird kubectl ARGS...`      |

## Cluster state snapshot (pre-cycle)

```
$ ssh -J geary root@192.168.122.212 \
    "kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes"
NAME        STATUS   ROLES           AGE   VERSION
hbird-cp1   Ready    control-plane   8h    v1.31.14
hbird-w1    Ready    <none>          8h    v1.31.14
hbird-w2    Ready    <none>          8h    v1.31.14
```

3/3 Ready going in, 3/3 Ready coming out. All four commands are read-only — no
cordons, no writes, no namespace creation. Cluster state did not change.

## Per-command results

### 1. `export-argocd`

| Side  | Invocation                                                                                                                     | Exit |
|-------|--------------------------------------------------------------------------------------------------------------------------------|------|
| bash  | `CONFIG=cluster.local.conf KVM_HOST=geary CP_IP=192.168.122.212 bash scripts/export-argocd.sh --output … --force --proxy-jump=geary` | 0    |
| rust  | `hbird export-argocd --config cluster.local.conf --output … --force --cp-ip 192.168.122.212`                                   | 0    |

**Output diff (bash YAML vs Rust YAML)**: **EMPTY**.

```
$ diff /tmp/cycle1-export-argocd/bash-argocd.yaml \
       /tmp/cycle1-export-argocd/rust-argocd.yaml
(no output — files are byte-for-byte identical)

$ stat -c '%s %n' /tmp/cycle1-export-argocd/*-argocd.yaml
5679 /tmp/cycle1-export-argocd/bash-argocd.yaml
5679 /tmp/cycle1-export-argocd/rust-argocd.yaml
```

Both files contain:
- `apiVersion: v1`, `kind: Config`
- `server: https://192.168.122.212:6443`
- cluster / context / user names rewritten to `hummingbird-hbird-cp1`
- identical base64 CA + client cert + client key blobs
- mode `0600`

**Log diff (operational, not content)**:
- bash emits "rewriting server URL via sed -> ..." and "rewriting cluster/context/user names via sed -> ..." — bash announces which rewrite path (yq vs sed) it took. Rust uses a pure-Rust line-anchored rewrite (no external yq/sed), so there's nothing to announce. Output is identical.
- rust emits `INFO hbird{subcommand="export-argocd"}: close` — tracing-subscriber CLOSE event (#326). Stderr-only at default RUST_LOG=info.

**Verdict**: **PARITY** — output kubeconfigs byte-for-byte identical.

### 2. `get-kubeconfig`

| Side  | Invocation                                                                                                                     | Exit |
|-------|--------------------------------------------------------------------------------------------------------------------------------|------|
| bash  | `CONFIG=cluster.local.conf KVM_HOST=geary CP_IP=192.168.122.212 make get-kubeconfig OUTPUT=… FORCE=1 PROXY_JUMP=geary`         | 0    |
| rust  | `hbird get-kubeconfig --config cluster.local.conf --output … --force --cp-ip 192.168.122.212`                                  | 0    |

**Output diff (bash YAML vs Rust YAML)**: **EMPTY**.

```
$ diff /tmp/cycle1-export-argocd/bash-kubeconfig.yaml \
       /tmp/cycle1-export-argocd/rust-kubeconfig.yaml
(no output)
```

Both files contain the same shape as `export-argocd` but with context name
defaulting to `CP_NAME` (`hbird-cp1`) — no `hummingbird-` prefix. Mode `0600`,
identical bytes.

**Verdict**: **PARITY**.

### 3. `nodes`

| Side  | Invocation                                              | Exit |
|-------|---------------------------------------------------------|------|
| bash  | `CONFIG=cluster.local.conf KVM_HOST=geary make nodes`   | 0    |
| rust  | `hbird nodes --config cluster.local.conf --cp-ip 192.168.122.212` | 0 |

**Output**:

```
NAME        STATUS   ROLES           AGE   VERSION
hbird-cp1   Ready    control-plane   8h    v1.31.14
hbird-w1    Ready    <none>          8h    v1.31.14
hbird-w2    Ready    <none>          8h    v1.31.14
```

Identical kubectl table on both sides.

**Log diff (operational)**:
- bash emits `Starting tunnel: localhost:6443 -> 192.168.122.212:6443 via geary` — this is `scripts/kubectl-k8s.sh` announcing the SSH `-fNL` tunnel it forks. Rust takes a different path: it SSH-execs `kubectl` directly on the CP (matching the `cp_kubectl` shim wired in #325), so there's no local-port tunnel to announce.
- rust emits the same tracing CLOSE line as above.

**Verdict**: **PARITY** — same node list, same `Ready` statuses, same roles. The
tunnel-vs-direct-exec difference is implementation, not user-visible result.

### 4. `kubectl` (the wrapper)

| Side  | Invocation                                                                            | Exit |
|-------|---------------------------------------------------------------------------------------|------|
| bash  | `CONFIG=cluster.local.conf KVM_HOST=geary make kubectl ARGS="get pods -n kube-system"`| 0    |
| rust  | `hbird kubectl --config cluster.local.conf --cp-ip 192.168.122.212 get pods -n kube-system` | 0 |

**Output**: identical 15-pod listing for `kube-system`, modulo the rust tracing
CLOSE line at the end:

```
NAME                                READY   STATUS    RESTARTS     AGE
cilium-2xtbw                        1/1     Running   4            8h
cilium-envoy-9fjzj                  1/1     Running   0            8h
…
metrics-server-587b667b55-bjmz7     1/1     Running   1 (8h ago)   8h
```

Same pod names, same READY/STATUS/RESTARTS/AGE columns. The variadic pass-through
shape (`hbird kubectl get pods -n kube-system`) works as documented; clap's
`trailing_var_arg` correctly forwards `-n kube-system` without trying to claim it
as its own flag.

**Verdict**: **PARITY**.

## Summary

| Command         | Bash exit | Rust exit | Output match | Verdict |
|-----------------|-----------|-----------|--------------|---------|
| `export-argocd` | 0         | 0         | YES (byte-for-byte YAML) | PARITY |
| `get-kubeconfig`| 0         | 0         | YES (byte-for-byte YAML) | PARITY |
| `nodes`         | 0         | 0         | YES (same node table)    | PARITY |
| `kubectl`       | 0         | 0         | YES (same pod table)     | PARITY |

**All four commands at parity.** The known divergences observed are:
- bash announces SSH tunnel setup on `nodes` / `kubectl` (`Starting tunnel: …`).
  Rust SSH-execs kubectl directly on the CP, so there's nothing to announce.
- bash announces which rewrite path (`yq` vs `sed`) it took for `export-argocd` /
  `get-kubeconfig`. Rust uses a pure-Rust rewrite — no path selection to log.
- rust emits a one-line tracing CLOSE event per command (stderr, silent at
  RUST_LOG=info).

None of these change content or exit semantics. All are documented operational
shape differences, not bugs.

## Bugs #306 / #307 (pre-existing bash issues) — NOT exhibited on this workstation

Issue #288 calls out two bash bugs the Rust path was designed to avoid:

- **#306** — bash defaults `PROXY_JUMP="${KVM_HOST:-}"` BEFORE sourcing CONFIG, so
  `KVM_HOST` in CONFIG (but not shell-exported) resolves to empty. **Did not
  trigger here**: the workstation's libvirt-NAT subnet `192.168.122.0/24` is
  directly routable from judah (no ProxyJump needed for the SSH-to-CP hop), so
  the empty `PROXY_JUMP` default works by accident. On a typical workstation off
  the libvirt subnet, the bash twin would hang / fail.
- **#307** — `ssh -t` + remote `sudo` can emit OSC session-marker escapes into
  stdout, corrupting the captured kubeconfig. **Did not trigger here**: the CP's
  sudo configuration didn't fire the OSC sequence on this version, so the bash
  output is clean. On a sudo upgrade that re-enables the OSC, the bash twin's
  `grep -q '^apiVersion:'` sanity check would fail.

The Rust path is immune to both regardless of workstation — `KVM_HOST` resolution
happens after CONFIG load (Plan::from_args), and SSH defaults to `BatchMode=yes`
(no TTY → no OSC). The fixture `rust/crates/hbird-cli/tests/update_cluster/fixtures/live/cycle_export_argocd.txt`
already captures the per-bug analysis with workstation-specific notes.

## Follow-up issues filed during this cycle

None. No new bugs surfaced. The two pre-existing bash bugs (#306, #307) remain
tracked under their existing issues.

## Cycle 1 verdict: PASS

The four `export-argocd-family` commands are at functional parity with their bash
twins on the live geary cluster. This gate from epic #353 is satisfied:

> - [ ] **export-argocd / get-kubeconfig / nodes / kubectl** — Live-validate cycle 1 (gates #288)

Once epic #353 collects the remaining gates (`verify-*` and
`deploy/destroy/spawn-workers`), the bash-removal PR can land
`scripts/export-argocd.sh` + `scripts/kubectl-k8s.sh` for deletion.
