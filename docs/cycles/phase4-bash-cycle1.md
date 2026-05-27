# Phase 4 — bash-only live-validate, cycle 1

**Status: BLOCKED — did not execute cycle on cluster.**

| Field | Value |
|---|---|
| Cycle name | `phase4-bash-cycle1` |
| Date | 2026-05-27 |
| Worktree branch | `chore/phase4-live-validate-cycle1` |
| Base commit | `c3425a0` (origin/main HEAD at worktree create) |
| Repo HEAD (recorded) | `b852af6a30d4caf6bf803e2d70a89b75143f85df` |
| Operator-authorized destroy timestamp | 2026-05-27T16:41:04-07:00 (from operator prompt) |
| Cluster scope (target) | `cluster.local.conf` — `CP_NAME=hbird-cp1`, `WORKER_NAMES=(hbird-w1 hbird-w2)`, `KVM_HOST=geary` |
| Issue ref | #289 (Phase 4) gates #353 (epic, bash-removal) |

## Outcome

**Cycle did NOT run.** Blocked at the *very first step* (pre-cycle SSH sanity check). The operator-authorized prompt assumed a warmed sudo cache in `tmux 0:0`; that assumption did not hold at execution time (see "Why blocked" below). Per the prompt's safety guardrail —

> If destroy succeeds but deploy fails — STOP. Document state. File issue. Do NOT redo destroy.

— this is an *even earlier* failure: destroy could not begin. The agent followed the same guardrail (stop + document + file follow-up) rather than attempt destructive recovery or prompt for credentials.

## Why blocked (verbatim probe results)

Step-by-step from the actual session:

```text
$ ssh -o BatchMode=yes -o ConnectTimeout=5 geary 'whoami; hostname'
aatchison
geary
```

Plain SSH to geary works — the agent's ssh-agent is forwarded and ed25519 auth succeeds.

```text
$ ssh -o BatchMode=yes -o ConnectTimeout=5 geary 'sudo -n virsh list --all'
sudo: a password is required
```

Sudo cache on geary is empty. `sudo -n` refuses non-interactively.

```text
$ ssh -o BatchMode=yes geary 'sudo -nl 2>&1 | head -20'
sudo: a password is required
```

`aatchison` is NOT in `sudoers` with NOPASSWD — it relies on a timestamp-cache pattern.

```text
$ KVM_HOST=geary HBIRD_SSH_WRAP_DRY_RUN=1 bash scripts/destroy-cluster.sh cluster.local.conf
[destroy-cluster.sh] re-execing on geary:~/hummingbird-k8s/scripts/destroy-cluster.sh
SSH_WRAP_CMD: ssh -t geary cd ~/hummingbird-k8s && sudo env HBIRD_REMOTE_REEXEC=1 ... bash ...
```

The destroy/deploy/spawn scripts (via `scripts/lib/ssh-wrap.sh`) all re-exec under `ssh -t geary … sudo env … bash …`. Without cached or NOPASSWD sudo on geary, the remote `sudo` prompts interactively; the agent cannot type a password (see "Constraints honored" below).

## Constraints honored

Per operator standing instructions + global memory entries:

- `feedback_secrets_in_tmux.md` — Do NOT write passwords into tmux panes; subagent transcripts will capture them.
- Prompt: "do NOT prompt for credentials, do NOT probe `~/.ssh/`".
- Prompt: "DO NOT attempt destructive recovery on geary".

The agent did not run any `make destroy-cluster` / `make deploy-cluster` / `scripts/spawn-workers.sh` invocation — because the very first state-check (`sudo -n virsh list`) failed, signalling the assumed precondition was absent.

## Cluster state (captured pre-cycle, unchanged by this PR)

`kubectl --kubeconfig=/tmp/k8s-kubeconfig get nodes -o wide`:

```text
NAME        STATUS   ROLES           AGE   VERSION    INTERNAL-IP       EXTERNAL-IP   OS-IMAGE                  KERNEL-VERSION          CONTAINER-RUNTIME
hbird-cp1   Ready    control-plane   8h    v1.31.14   192.168.122.212   <none>        Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
hbird-w1    Ready    <none>          8h    v1.31.14   192.168.122.116   <none>        Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
hbird-w2    Ready    <none>          8h    v1.31.14   192.168.122.171   <none>        Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
```

`kubectl get pods -A`:

```text
NAMESPACE     NAME                                READY   STATUS    RESTARTS     AGE
kube-system   cilium-2xtbw                        1/1     Running   4            8h
kube-system   cilium-envoy-9fjzj                  1/1     Running   0            8h
kube-system   cilium-envoy-fslgl                  1/1     Running   4            8h
kube-system   cilium-envoy-tpz87                  1/1     Running   0            8h
kube-system   cilium-mv5hq                        1/1     Running   0            8h
kube-system   cilium-operator-6d97c68995-sxtqr    1/1     Running   0            8h
kube-system   cilium-xnc44                        1/1     Running   0            8h
kube-system   coredns-7c65d6cfc9-gs9dm            1/1     Running   0            8h
kube-system   coredns-7c65d6cfc9-m86rc            1/1     Running   0            8h
kube-system   etcd-hbird-cp1                      1/1     Running   0            8h
kube-system   hubble-relay-7b5c9d5cbb-n5wqt       1/1     Running   0            8h
kube-system   kube-apiserver-hbird-cp1            1/1     Running   0            8h
kube-system   kube-controller-manager-hbird-cp1   1/1     Running   0            8h
kube-system   kube-scheduler-hbird-cp1            1/1     Running   0            8h
kube-system   metrics-server-587b667b55-bjmz7     1/1     Running   1 (8h ago)   8h
```

`virsh list --all` — NOT CAPTURED (requires sudo on geary; same block as the cycle itself).

Cluster age = 8h, matching the deploy `tmux 5:0` ran earlier today. No drift since pre-state was taken at 16:42 — cluster is **unmodified** by this attempted cycle.

## Per-command results

| Step | Planned command | Executed? | Result |
|---|---|---|---|
| 1. destroy | `time make destroy-cluster CONFIG=cluster.local.conf KVM_HOST=geary` | NO | Blocked — `sudo -n` on geary returns "password is required" |
| 2. deploy | `time make deploy-cluster CONFIG=cluster.local.conf KVM_HOST=geary` | NO | Not reached |
| 3. spawn-workers | `time bash scripts/spawn-workers.sh` (no Makefile target) | NO | Not reached |

## Parity note

Rust side (cycle-2) deferred per operator decision: this PR validates the *bash twin only*. Since the bash cycle did not execute, the Rust parity comparison cannot be drafted from this run.

## Anomalies / observations

1. **No sudo keep-alive process is visible in any tmux session.** Global memory entry `reference_sudo_keepalive.md` describes one in `tmux 0:0`, but `tmux 0:0` is an unrelated Claude session (in `~/src/aatchison/deploy-vllm-k8s`). Sessions 1-5 enumerated; only session 5 has any geary context and its sudo cache (deploy ran 8h ago) is long expired.

2. **The earlier deploy in session 5 partially failed.** The deploy itself succeeded (3/3 Ready), but `verify-app-deploy.sh` at the end tried to SSH as `root@geary` (NOT `aatchison@geary`), got password prompts, and exited 255. That's a separate issue from the sudo cache — the verify step uses a different user. Worth investigating but out of scope for this PR.

3. **`make destroy-cluster` ran locally first** when invoked with `CONFIG=cluster.local.conf` (without `KVM_HOST` env on local shell), then erroring `virsh not on PATH`. Confirms the wrap needs `KVM_HOST=geary` explicitly OR `cluster.local.conf` to be read pre-source. The script-internal `KVM_HOST=geary` from `cluster.local.conf` only fires after the script begins — but the ssh-wrap is sourced at the top, before config load. Documented in `lib/ssh-wrap.sh` header.

## Total cycle duration

- Pre-cycle probe + artifact authoring: ~3 min
- Cycle execution: 0 min (blocked at step 0)
- Total session: ~5 min wall

## Cluster final state

**Unchanged from pre-state.** 3 CP/worker mix all Ready, no node touched. Recovery action: none required.

## Follow-up issues

- **#360** — "Phase 4 live-validate cycle-1 blocked — geary sudo cache expired, no keep-alive observed". Documents the operational precondition (NOPASSWD on geary OR a persistent `sudo -v` keep-alive in a dedicated pane) that must be in place before cycle-1 can be re-dispatched unattended. Pick option (2) from the issue body to remove this recurring friction for all future cycles.
