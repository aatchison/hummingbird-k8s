# Phase 4 — bash-only live-validate, cycle 1

**Status: COMPLETE — Attempt 2 succeeded via operator-in-the-loop tmux 5:0 pattern.**
Attempt 1 (below) blocked on geary sudo cache; Attempt 2 ran the full
DESTROY → DEPLOY cycle with the operator typing sudo interactively in
`tmux 5:0`. Cluster came up 3/3 Ready (1 CP + 2 workers). Only post-cycle
failure was the `verify-app-deploy.sh` tail attempting `ssh root@geary`
(tracked as #362).

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

## Attempt 1 (blocked — geary sudo cache expired)

### Outcome

**Cycle did NOT run.** Blocked at the *very first step* (pre-cycle SSH sanity check). The operator-authorized prompt assumed a warmed sudo cache in `tmux 0:0`; that assumption did not hold at execution time (see "Why blocked" below). Per the prompt's safety guardrail —

> If destroy succeeds but deploy fails — STOP. Document state. File issue. Do NOT redo destroy.

— this is an *even earlier* failure: destroy could not begin. The agent followed the same guardrail (stop + document + file follow-up) rather than attempt destructive recovery or prompt for credentials.

### Why blocked (verbatim probe results)

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

### Constraints honored

Per operator standing instructions + global memory entries:

- `feedback_secrets_in_tmux.md` — Do NOT write passwords into tmux panes; subagent transcripts will capture them.
- Prompt: "do NOT prompt for credentials, do NOT probe `~/.ssh/`".
- Prompt: "DO NOT attempt destructive recovery on geary".

The agent did not run any `make destroy-cluster` / `make deploy-cluster` / `scripts/spawn-workers.sh` invocation — because the very first state-check (`sudo -n virsh list`) failed, signalling the assumed precondition was absent.

### Cluster state (captured pre-Attempt-1; unchanged at end of Attempt 1)

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

### Per-command results

| Step | Planned command | Executed? | Result |
|---|---|---|---|
| 1. destroy | `time make destroy-cluster CONFIG=cluster.local.conf KVM_HOST=geary` | NO | Blocked — `sudo -n` on geary returns "password is required" |
| 2. deploy | `time make deploy-cluster CONFIG=cluster.local.conf KVM_HOST=geary` | NO | Not reached |
| 3. spawn-workers | `time bash scripts/spawn-workers.sh` (no Makefile target) | NO | Not reached |

### Parity note

Rust side (cycle-2) deferred per operator decision: this PR validates the *bash twin only*. Since Attempt 1 did not execute the cycle, no parity data was captured from that attempt.

### Anomalies / observations

1. **No sudo keep-alive process is visible in any tmux session.** Global memory entry `reference_sudo_keepalive.md` describes one in `tmux 0:0`, but `tmux 0:0` is an unrelated Claude session (in `~/src/aatchison/deploy-vllm-k8s`). Sessions 1-5 enumerated; only session 5 has any geary context and its sudo cache (deploy ran 8h ago) is long expired.

2. **The earlier deploy in session 5 partially failed.** The deploy itself succeeded (3/3 Ready), but `verify-app-deploy.sh` at the end tried to SSH as `root@geary` (NOT `aatchison@geary`), got password prompts, and exited 255. That's a separate issue from the sudo cache — the verify step uses a different user. Worth investigating but out of scope for this PR.

3. **`make destroy-cluster` ran locally first** when invoked with `CONFIG=cluster.local.conf` (without `KVM_HOST` env on local shell), then erroring `virsh not on PATH`. Confirms the wrap needs `KVM_HOST=geary` explicitly OR `cluster.local.conf` to be read pre-source. The script-internal `KVM_HOST=geary` from `cluster.local.conf` only fires after the script begins — but the ssh-wrap is sourced at the top, before config load. Documented in `lib/ssh-wrap.sh` header.

### Total cycle duration (Attempt 1)

- Pre-cycle probe + artifact authoring: ~3 min
- Cycle execution: 0 min (blocked at step 0)
- Total session: ~5 min wall

### Cluster final state (end of Attempt 1)

**Unchanged from pre-state.** 3 CP/worker mix all Ready, no node touched. Recovery action: none required.

### Follow-up issues filed during Attempt 1

- **#360** — "Phase 4 live-validate cycle-1 blocked — geary sudo cache expired, no keep-alive observed". Documents the operational precondition (NOPASSWD on geary OR a persistent `sudo -v` keep-alive in a dedicated pane) that must be in place before cycle-1 can be re-dispatched unattended. Pick option (2) from the issue body to remove this recurring friction for all future cycles.

---

## Attempt 2 (operator-types-password pattern in tmux 5:0)

### Outcome

**Cycle COMPLETED.** Full DESTROY → DEPLOY cycle ran with the operator
interactively supplying sudo in `tmux 5:0`. Cluster came up 3/3 Ready
(1 CP + 2 workers, all 15 pods Running). Only failure was the trailing
`scripts/verify-app-deploy.sh` run inside `deploy-cluster.sh`, which
attempts `ssh root@geary` — that is tracked as #362, **deploy itself
succeeded**.

### Pre-cycle state (carried over from Attempt 1)

Identical to Attempt 1's captured state — cluster was unchanged between
the two attempts because Attempt 1 never touched it. CP `hbird-cp1` was
on IP `192.168.122.212`, age 8h, deployed from the earlier session 5
deploy run.

### Per-step exit codes + durations (Attempt 2)

| Step | Command | Wall | Exit | Notes |
|---|---|---|---|---|
| 1. DESTROY | `time make destroy-cluster CONFIG=cluster.local.conf KVM_HOST=geary` | **0m16.8s** | **0** | Clean; destroyed hbird-cp1, hbird-w1, hbird-w2 + qcow2/seed-ISOs |
| 2. DEPLOY | `time make deploy-cluster CONFIG=cluster.local.conf KVM_HOST=geary` | **3m58.9s** | **2** (255 from inner ssh) | Cluster Ready 3/3 in 5 attempts. Failure is the **#362** `verify-app-deploy` tail — deploy proper succeeded. Note: deploy implicitly triggered bib worker template rebuild (see **#311**) — the published GHCR worker image was not used because the pool had been torn down |
| 3. SPAWN-WORKERS | (`make spawn-workers …`) | — | **n/a** | The runbook listed a `spawn-workers` make target that **does not exist** (closest is `scripts/spawn-workers.sh` direct). Cluster already at full `WORKER_NAMES=(hbird-w1 hbird-w2)` from step 2, so running `scripts/spawn-workers.sh` would have added duplicates / collisions on the steady-state cluster. Step skipped per the safety guardrail "do not damage a healthy cluster". |

### Final cluster state (end of Attempt 2)

`kubectl get nodes -o wide` after the cycle:

```text
NAME        STATUS   ROLES           AGE     VERSION    INTERNAL-IP       EXTERNAL-IP   OS-IMAGE                  KERNEL-VERSION          CONTAINER-RUNTIME
hbird-cp1   Ready    control-plane   5m49s   v1.31.14   192.168.122.239   <none>        Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
hbird-w1    Ready    <none>          5m5s    v1.31.14   192.168.122.208   <none>        Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
hbird-w2    Ready    <none>          5m4s    v1.31.14   192.168.122.11    <none>        Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
```

`kubectl get pods -A` summary: **15 / 15 pods Running**, no restarts on
the freshly-deployed cluster. Notable pods:

- `cilium-{bdzhh,9jz2w,qk4ln}` — one per node, all Ready
- `cilium-operator-57d4c6ccc4-sfmg2` on hbird-cp1
- `hubble-relay-7b5c9d5cbb-nlwr5` on `10.85.0.4` — **podman-CNI range**, see `project_hubble_relay_cni_race.md` / #259
- `metrics-server-587b667b55-9jcx7` on `10.85.0.5` — **podman-CNI range**, same race
- `coredns-7c65d6cfc9-{t4t26,z5tvv}` on `10.0.0.0/24` (correct cilium range)
- `etcd-hbird-cp1`, `kube-{apiserver,controller-manager,scheduler}-hbird-cp1` all Ready

The Hubble/metrics-server-on-podman-CNI race (#259) reproduced again,
exactly as the operator memory predicted. No bounce was performed in
this cycle because Phase 4 validates the *deploy-cluster recipe as-is* —
rebouncing those pods would mask the bug.

### Total cycle duration (Attempt 2)

- 16:53:05 — DESTROY started
- 16:53:22 — DESTROY done (17s)
- 16:53:53 — DEPLOY started (~30s operator gap typing sudo)
- 16:57:51 — DEPLOY finished (3m58s; cluster Ready at ~3m20s, then the verify tail failed)
- 17:01:57 — final kubectl probe complete
- **Cycle wall: ~9 min** end-to-end (destroy → verified final state)

### Anomalies surfaced

1. **#362** (`verify-app-deploy.sh` SSHes as `root@geary`) reproduced
   — confirmed not a one-off. Caused `make deploy-cluster` to exit
   non-zero despite the actual deploy succeeding. The script also retried
   the trap-cleanup `ssh root@geary` path, requiring 3 wrong-password
   attempts in tmux 5:0 to abort.

2. **Runbook drift**: the dispatched runbook referenced a `make
   spawn-workers` target that does not exist. The closest equivalent is
   `scripts/spawn-workers.sh` directly. Future Phase-4 runbooks should
   either drop step 3 (deploy-cluster handles workers) or specify the raw
   script invocation and a scenario where it's safe to run (e.g.
   *additional* workers on top of an existing cluster, not on a cluster
   already at target worker count).

3. **#259 race reproduced** — Hubble-relay + metrics-server landed on
   `10.85.0.0/24` (podman CNI) instead of the Cilium pod CIDR. Matches the
   operator memory prediction. No mitigation taken in this cycle (would
   mask the bug).

4. **Stale SSH tunnel from prior deploy needed manual replacement.**
   The old `ssh -fNL 6443:192.168.122.212:6443 geary` tunnel persisted
   after `destroy-cluster` — the tunnel must be killed + re-established
   pointing at the new CP IP (`192.168.122.239`) before kubectl works
   from the workstation. `make get-kubeconfig` re-fetched the new CA
   correctly. Worth a tiny `scripts/destroy-cluster.sh` hint that says
   "remember to drop stale `ssh -fNL` tunnels" — filing as low-pri.

### Parity note

Rust side (cycle-2) deferred per operator decision; this PR validates the
*bash twin only*. Now that Attempt 2 produced a successful baseline, a
Rust-side rerun can be diff'd against this artifact for parity.

---

## Cycle methodology — operator-in-the-loop pattern

Triggered by **#360** (geary sudo cache expired, no keep-alive observed).
The pattern used in Attempt 2:

- A long-lived tmux session, pane `5:0`, is the *only* place destructive
  Phase-4 commands run.
- The dispatching subagent uses `tmux send-keys -t 5:0 '<cmd>' Enter`
  to drive each step.
- When the remote `ssh -t geary sudo …` re-exec prompts for the
  operator's local sudo password, the **operator types it directly into
  pane 5:0** — the subagent never sees or captures the password
  (`feedback_secrets_in_tmux.md`).
- After each `Enter` the subagent enters a **foreground while-loop**
  doing `tmux capture-pane -t 5:0 -pS -100 | tail -60`, grepping for
  per-step EXIT markers (`DESTROY_EXIT_0_END`, `DEPLOY_EXIT_*_END`,
  `SPAWN_EXIT_*_END`) every 30s, with a per-step ceiling (15-20 min).
- No `Monitor` tool. No backgrounded poll. The agent stays foreground
  until success or hard-fail.

This pattern is the recommended workaround until #360 picks an option
(NOPASSWD on geary or a dedicated keep-alive pane).

---

## Continuation note — Monitor-tool-bailout caveat

Attempt 2 was split across **two subagents**. The first subagent
successfully started the cycle (DESTROY, DEPLOY initiated) but exited
prematurely via the `Monitor` coordinator-scope tool, which subagents
cannot call — it returns InputValidationError, and the subagent treated
the error as "wait for notification" instead of looping. The second
subagent (this one) re-attached to pane 5:0 in mid-deploy, completed the
wait via the foreground while-loop pattern, and authored the artifact.

Captured by the operator as a playbook entry: **Phase-4 subagents must
not reach for the `Monitor` tool** — coordinator-scope tools are not
available in subagent context, and the failure mode (silent exit) is
indistinguishable from "task done" to the dispatching coordinator.
