# `make update-cluster` — rolling bootc upgrade across a deployed cluster

`scripts/update-cluster.sh` is the operator-facing "roll the cluster
forward to a new image" entry point. It reads the same
`cluster.local.conf` that `make deploy-cluster` consumes and walks the
cluster one node at a time:

```bash
# From a client laptop (since C3 / #232) — re-execs on the KVM host
# via SSH; client never needs sudo or libvirt locally. See
# "Remote KVM-host operation" below for the full picture.
KVM_HOST=geary make update-cluster CONFIG=cluster.local.conf

# On the KVM host (post-#233: the Makefile recipe no longer prefixes
# sudo, so on-host operators either run as root or prepend it themselves):
sudo make update-cluster CONFIG=cluster.local.conf
```

It complements — does not replace — the per-VM
`bootc-semver-update.timer` described in
[`docs/auto-updates.md`](auto-updates.md). That timer (the canonical
post-#181 in-image updater) resolves the highest semver tag at GHCR and
stages the new image without rebooting; this script is the coordinated
path for single-CP clusters where the operator wants drain/uncordon,
ordering, a real reboot, and a clean abort surface.

On pre-#181 hosts the in-image timer is the legacy
`bootc-fetch-apply-updates.timer` (the stock unit that follows whatever
mutable ref is pinned, typically `:latest`); this script handles both
units transparently — see [Interaction with the in-image auto-update
timer](#interaction-with-the-in-image-auto-update-timer) for the
both-stop / semver-start fallback logic.

## Overview

For each node in order — CP first, then each worker in `WORKER_NAMES`
order — the script:

1. Stops both `bootc-semver-update.timer` (post-#181 canonical) **and**
   `bootc-fetch-apply-updates.timer` (legacy) on the node — best-effort
   per unit so whichever timer is present is paused. This avoids racing
   the in-image auto-update timer described in
   [`docs/auto-updates.md`](auto-updates.md).
2. For workers only: `kubectl drain NODE --ignore-daemonsets
   --delete-emptydir-data --timeout=5m`. The CP is **not** drained —
   it's a single-CP topology, there's nowhere to drain to.
3. Snapshots the booted image digest via `bootc status --json` so the
   "no actual update" case can be detected (see
   [bootc no-update detection](#bootc-no-update-detection) below).
4. Captures the node's `.status.nodeInfo.bootID` (see
   [Reboot detection (bootID)](#reboot-detection-bootid) below).
5. `ssh root@NODE_IP bootc upgrade --apply` — `--apply` auto-reboots.
6. Waits for SSH to come back; then waits for `bootID` to differ from
   the pre-reboot value (proves a real reboot happened — defeats the
   stale-apiserver-cache hit on Ready); then for `kubectl get node NAME`
   to report `Ready` (the regex covers `Ready,SchedulingDisabled` too —
   workers come back cordoned-but-Ready until step 8 lands); then for
   every kube-system DaemonSet pod on the node to be Ready (see
   [Daemonset readiness gate](#daemonset-readiness-gate)).
7. For workers: `kubectl uncordon NODE`.
8. Restarts the in-image auto-update timer on the node. The script
   probes for `bootc-semver-update.timer` first (post-#181 canonical)
   and falls back to `bootc-fetch-apply-updates.timer` on pre-#181
   hosts — see [Interaction with the in-image auto-update
   timer](#interaction-with-the-in-image-auto-update-timer).
9. Sleeps 5s before moving on (small breather; not load-bearing).

The CP is rolled before workers so that any apiserver outage happens
while workloads are still on their original nodes — workers don't lose
their connection to the apiserver during their own drains.

## When to use update-cluster vs destroy + redeploy

| Situation | Path |
| --- | --- |
| New image tag landed at GHCR, same `cluster.local.conf` | `make update-cluster` |
| Bumping `K8S_VERSION` via a new built image | `make update-cluster` |
| Changing `WORKER_NAMES`, adding/removing workers | `make destroy-cluster` + `make deploy-cluster` |
| Changing `CP_NAME`, `POOL_DIR`, `SSH_PUBKEY_FILE` | `make destroy-cluster` + `make deploy-cluster` |
| Toggling `AUTO_UPDATE_CP` or `SWITCH_TO_GHCR` | `make destroy-cluster` + `make deploy-cluster` (these only fire at first boot) |
| Spot-fixing a single stuck worker | `make update-node NODE=<name>` |

Rule of thumb: `update-cluster` rolls the **image** the cluster runs.
Anything that's cloud-init seed material — set once at first boot — is
a redeploy. See [`docs/deploy-cluster.md`](deploy-cluster.md) for the
deploy surface.

## Config knobs consumed

`update-cluster.sh` sources the same `cluster.local.conf` as
`deploy-cluster.sh`. From that file it consumes:

| Knob | Required? | Purpose |
| --- | --- | --- |
| `CP_NAME` | yes | libvirt domain name for the CP — used to resolve the CP IP and to identify the CP node in `kubectl`. |
| `WORKER_NAMES` | no (defaults to empty) | bash array of worker domain names. The script walks them in array order. |
| `SSH_PUBKEY_FILE` | yes | The private key paired with this pubkey (`${SSH_PUBKEY_FILE%.pub}`) is used as the SSH identity. Mirrors the pattern from `deploy-cluster.sh` post-PR #185. |
| `CP_IP` | no (env override) | If set, skips the `virsh domifaddr` lookup for the CP. Useful for clusters with static addressing. |
| `WORKER_IPS` | no (env override) | Parallel array to `WORKER_NAMES`; if set must be the same length. Skips `virsh domifaddr` per worker. |

Environment variables the script honors:

| Var | Default | Effect |
| --- | --- | --- |
| `CONFIG` | `./cluster.local.conf` | Path to the cluster config. |
| `DRY_RUN=1` | unset | Same as `--dry-run` — print actions, don't ssh/kubectl. Useful for unprivileged previews; bypasses the root-required check. |
| `DRAIN_TIMEOUT` | `5m` | `kubectl drain --timeout=` value (Go duration string). Tune up for workloads with slow graceful-shutdown. |
| `READY_TIMEOUT` | `300` | Seconds to wait for `kubectl get node` to report `Ready` post-reboot. Also bounds the [bootID-changed gate](#reboot-detection-bootid). Must be > 0. |
| `DAEMONSET_TIMEOUT` | `${READY_TIMEOUT}` | Seconds to wait for the [DaemonSet readiness gate](#daemonset-readiness-gate). Defaults to `READY_TIMEOUT` for compatibility; set independently when the DS gate needs more (or less) headroom than node-Ready. Must be > 0. |
| `APISERVER_TIMEOUT` | `300` | Seconds to wait for the CP apiserver to answer `/readyz` after the CP reboots. |
| `SSH_TIMEOUT` | `300` | Seconds to wait for `ssh root@<ip>` to come back post-reboot. |
| `INTER_NODE_SLEEP` | `5` | Seconds to pause after uncordoning a node before processing the next one (small settle window). |

All knobs honor the standard `VAR=value make …` pattern; the Makefile
targets pass them through via `sudo -E`. See
[Performance tuning](#performance-tuning) for guidance.

## Remote KVM-host operation (`KVM_HOST=`)

Since C3 (#232), `update-cluster.sh` (alongside `deploy-cluster.sh`,
`destroy-cluster.sh`, and `spawn-workers.sh`) self-hosts on the KVM
host via SSH when `KVM_HOST` is set and the local short hostname
doesn't match `${KVM_HOST%%.*}`. The client never needs `sudo` or
`libvirt` installed — only `ssh` + the operator's SSH key. Sudo
happens on the remote.

The shim assumes a sibling checkout of hummingbird-k8s already exists
on the KVM host at `$HBIRD_REMOTE_REPO` (default
`~/hummingbird-k8s`). One-time setup:

```bash
ssh $KVM_HOST 'git clone https://github.com/aatchison/hummingbird-k8s ~/hummingbird-k8s'
```

Then, from a client:

```bash
export KVM_HOST=geary
make update-cluster CONFIG=cluster.local.conf
make update-cluster CONFIG=cluster.local.conf FLAGS=--workers-only
make update-cluster CONFIG=cluster.local.conf FLAGS=--node=hbird-w1
```

The shim is a no-op when `KVM_HOST` is unset, when the local short
hostname matches `${KVM_HOST%%.*}` (operator already on the KVM host),
or when the script body is being re-executed on the remote side
(`HBIRD_REMOTE_REEXEC=1` sentinel).

`KVM_HOST` is expected to be an SSH alias (`~/.ssh/config`) or a
short/long hostname whose first label matches the local hostname when
operating directly on the KVM host. Bare IP literals and unrelated
FQDNs are accepted but won't short-circuit the "already local" guard
correctly — use an `~/.ssh/config` alias instead.

Env-var passthrough is an **explicit allowlist** maintained in
`scripts/lib/ssh-wrap.sh` and pinned by `tests/scripts/ssh-wrap.bats`.
Local `CONFIG=` files are `scp`'d to the remote before re-exec. See
[`docs/deploy-cluster.md`](deploy-cluster.md#remote-kvm-host-operation-kvm_host)
for the full allowlist, the `HBIRD_REMOTE_REPO` override, and the
remediation hints emitted on pre-flight failure (unreachable KVM host
or missing remote checkout).

### Env-var validation (security)

Because `make update-cluster` invokes the script via `sudo -E`,
operator-shell env vars survive the privilege boundary and reach the
script as root. To prevent shell-injection and bash-arithmetic
side-effects from hostile values, every env knob above is validated at
startup with strict regexes before any privileged call:

| Var | Accepted pattern |
| --- | --- |
| `DRAIN_TIMEOUT` | `^[0-9]+(s\|m\|h)?$` (bare integer or a kubectl-style duration) |
| `READY_TIMEOUT` | `^[1-9][0-9]*$` (strictly positive — 0 would short-circuit the gate) |
| `DAEMONSET_TIMEOUT` | `^[1-9][0-9]*$` (strictly positive) |
| `APISERVER_TIMEOUT` | `^[0-9]+$` |
| `SSH_TIMEOUT` | `^[0-9]+$` |
| `INTER_NODE_SLEEP` | `^[0-9]+$` (0 is allowed; it skips the inter-batch sleep) |

Anything else exits the script with `rc=1` and a clear diagnostic
before reaching `ssh` / `kubectl`.

## Full flag reference

### `--workers-only`

Skip the CP entirely; only roll workers.

```bash
make update-workers CONFIG=cluster.local.conf
# or directly:
sudo -E bash scripts/update-cluster.sh --workers-only
```

Use when the CP is already on the target image (e.g. you upgraded it
manually) or when you explicitly want to avoid the apiserver downtime
window during this run.

Mutually exclusive with `--node=`.

### `--node=NAME`

Update exactly one node — either the CP or a single worker.

```bash
# Update one specific worker:
make update-node CONFIG=cluster.local.conf NODE=hbird-w1

# Or directly:
sudo -E bash scripts/update-cluster.sh --node=hbird-w1

# Update only the CP:
sudo -E bash scripts/update-cluster.sh --node=hbird-cp1
```

Useful for:

- Canarying a new image against one node before rolling the rest.
- Re-running just the node that failed mid-rollout.
- Spot-fixing a stuck worker after manual intervention.

Mutually exclusive with `--workers-only`. The value must match either
`CP_NAME` or one of `WORKER_NAMES`; mismatches fail loudly.

### `--skip-drain`

Emergency rollback escape hatch. For workers, skips
`kubectl drain` (the CP already skips drain by design). Also skips the
in-image timer stop/restart dance (covers both
`bootc-semver-update.timer` and `bootc-fetch-apply-updates.timer`) —
appropriate when you're driving an upgrade manually and don't want the
orchestrator touching the in-image timer.

```bash
sudo -E bash scripts/update-cluster.sh --skip-drain
```

Use sparingly — `kubectl drain` is the only thing keeping workloads
from being killed mid-flight. Reserve this for stuck drains where you
have already accepted the workload disruption.

### `--skip-gates`

Operator escape hatch — skip the bootID-changed and DaemonSet-readiness
gates added in PR #208. `wait_node_ready` alone is the post-reboot
signal in this mode, which is what `update-cluster.sh` did pre-#208.

```bash
sudo -E bash scripts/update-cluster.sh --skip-gates
```

Use **only** if you've verified the cluster is healthy via other means
(`kubectl get pods -n kube-system`, manual `bootc status`, etc.). The
gates exist to prevent dataplane outages during the roll — turning them
off re-introduces the stale-apiserver-cache and pending-pods-window
risks described in
[Reboot detection (bootID)](#reboot-detection-bootid) and the
[DaemonSet readiness gate](#daemonset-readiness-gate).

Common reason to reach for this: the gates are misfiring on a
known-healthy cluster (e.g. a non-DaemonSet kube-system pod is
chronically in CrashLoop and pre-#208 baseline-exclusion didn't pick
it up). The right fix in that case is to file an issue with the gate
log line, not to leave `--skip-gates` on permanently.

### `--dry-run`

Print the intended actions without ssh-ing or running kubectl. Doesn't
require root, doesn't acquire the lock, doesn't touch any VM. The
script substitutes `<resolved-at-runtime>` for IPs it would otherwise
look up via `virsh domifaddr`.

```bash
DRY_RUN=1 bash scripts/update-cluster.sh
# or
bash scripts/update-cluster.sh --dry-run
```

Useful for code review, CI smoke tests, and previewing a `--node=` /
`--workers-only` selection.

### `--start-from=NAME`

Resume an interrupted roll. The script walks `WORKER_NAMES` in order
and skips every entry before `NAME`; once `NAME` is encountered it is
processed and the loop continues from there.

```bash
# Original run aborted after hbird-w1; pick up at hbird-w2:
sudo -E bash scripts/update-cluster.sh --start-from=hbird-w2
```

Semantics:

- The CP is **also skipped** when `--start-from=` is set — the resume
  point is implicitly past the CP (which is always rolled first).
- Combines with `--workers-only` (both skip the CP); the two have
  overlapping effects, no conflict.
- **Mutually exclusive with `--node=`** (single-node mode). `--node=` is
  "exactly this one node"; `--start-from=` is "this node onward".
- The value must match one of `WORKER_NAMES`; mismatches fail loudly.

See [Resume after interruption](#resume-after-interruption) for the
recommended workflow.

### `--continue-on-error`

Record per-node failures and proceed instead of aborting. A summary is
printed at the end listing succeeded vs failed nodes. The default
remains **fail-fast** — a single drain failure or stuck `bootc upgrade`
will abort the rest of the cluster's rollout.

```bash
sudo -E bash scripts/update-cluster.sh --continue-on-error
```

Exit codes:

| rc | Meaning |
| --- | --- |
| 0 | Every targeted node succeeded. |
| 1 | Fail-fast abort (the default mode's behavior). |
| 3 | One or more workers failed, but `--continue-on-error` was set. |

Note: failed nodes are left **cordoned** — `--continue-on-error` does
not auto-uncordon. The end-of-run summary lists which nodes need
operator attention.

**Scope**: `--continue-on-error` applies to **worker** failures only.
Control-plane failures (drain, bootc upgrade, wait-for-Ready on the CP)
remain fail-fast — a broken CP is not a state the script can safely
roll past.

### `--no-delete-emptydir-data`

By default the script passes `--delete-emptydir-data` to
`kubectl drain`. That's the right call for ephemeral cache pods
(scratch volumes, build artifacts) but is destructive for workloads
that use `emptyDir` as persistent-ish state — Prometheus' WAL, for
instance, is on `emptyDir` in many Helm charts and will lose pending
samples on every roll.

```bash
sudo -E bash scripts/update-cluster.sh --no-delete-emptydir-data
```

Trade-off: drain will now **block** on any pod that has `emptyDir`
volumes (no `--delete-emptydir-data` and no eviction grace from
kubectl). For affected workloads, manually evict / scale down /
checkpoint before running the upgrade. Combine with
`--continue-on-error` to roll past unevictable workers without
aborting.

### `--parallel=N`

Process workers in batches of N concurrently — each batch fans out to
`update_worker` in background subshells, the script `wait`s for the
batch, replays each subshell's log in deterministic order, then moves
to the next batch.

```bash
sudo -E bash scripts/update-cluster.sh --parallel=2
```

Defaults to `N=1` (serial; the historical behavior). The **CP is still
updated serially** before any worker batch — the CP reboot is too
disruptive to overlap with anything else.

Safety considerations before raising N:

- All workloads must have PodDisruptionBudgets that allow at least N
  concurrent pod evictions. A PDB with `maxUnavailable: 1` will
  deadlock at `N=2`.
- The cluster must have enough headroom to run all replicas of every
  workload on `(workers - N)` nodes during a batch.
- Storage backends (local-path, OpenEBS LocalPV) on N nodes go away
  simultaneously — make sure replicated storage / off-node persistence
  is in place for stateful workloads.
- Network policies that filter on node IP will see N nodes restarting
  at once.

When in doubt, leave at `N=1` and accept the longer wall-clock.

#### Trade-offs and known limitations

- **Output is captured per-worker and replayed after the batch
  completes** so the log sequence is deterministic (rather than two
  workers interleaving log lines at the byte level). The practical
  consequence: during a long batch — say a 5-minute `kubectl drain` on
  both workers — the operator will see **no output at all** until the
  batch finishes. This is intentional but jarring; if you want live
  progress, run with `--parallel=1`.
- **Fail-fast within a batch is best-effort.** When one worker in a
  parallel batch fails (without `--continue-on-error`), siblings
  finish their current step before the script aborts. The script does
  not actively kill in-flight `bootc upgrade --apply` calls on
  siblings — and even if it did, see the next bullet.
- **Ctrl-C does NOT abort an in-flight `bootc upgrade --apply` on a
  worker.** Once the remote `bootc` has started staging the new
  deployment, the reboot will happen even if you Ctrl-C the script
  locally. After interrupting, check `bootc status` on the affected
  node before retrying.

## Apiserver downtime window

The CP is rebooted in place. While it's down — typically **~60-120
seconds** — the apiserver is unavailable cluster-wide. This is a
single-CP architecture consequence; see the warning in
[`docs/deploy-cluster.md`](deploy-cluster.md) about `AUTO_UPDATE_CP=true`
for the same reason.

What this means in practice:

- `kubectl` against the cluster will block until the apiserver returns.
- Pods already running on workers keep running — kubelet keeps them
  alive without the apiserver. They just can't be rescheduled.
- Anything that depends on the apiserver (admission webhooks,
  controllers, ArgoCD reconciliation) pauses until the CP is back.

The script prints a clear warning before the CP reboot. If you can't
tolerate the outage in this maintenance window, run with
`--workers-only` and update the CP later (or move to a 3-CP cluster —
see #11).

## Lock file

A single `flock`-held lock at `/run/hbird-update-cluster.lock` prevents
two `make update-cluster` runs from racing each other into a
mass-cordon. `/run` is tmpfs on systemd hosts, so the lock dies with
the kernel — no stale-lock cleanup is needed across reboots.

If a run dies hard (kill -9, host crash mid-run, etc.) and the lock
shows held for a process that no longer exists, `flock -n` releases it
when the holding fd closes; clearing the file by hand is safe:

```bash
sudo rm -f /run/hbird-update-cluster.lock
```

Dry-run mode skips the lock entirely so it can run without `/run`
write access (CI, unprivileged previews).

## bootc no-update detection

`bootc upgrade --apply` exits cleanly (rc=0, no reboot) when there's
no actual update available — the same return code as "applied and
rebooted via --apply" once you account for the fact that the reboot
side of `--apply` tears down ssh (rc=255 from the client's
perspective). An earlier version of this script misread rc=0 as
"updated", waited for a reboot that never came, and timed out.

Today the script snapshots
`.status.booted.image.imageDigest` (with a fallback to
`.status.booted.image.digest` for older bootc JSON schemas) via
`bootc status --json | jq` before and after the upgrade:

| pre digest | post digest | ssh stayed up? | Interpretation |
| --- | --- | --- | --- |
| sha256:A | sha256:A | yes | No update available — short-circuit waits, move on. |
| sha256:A | sha256:B | yes | Odd — digests differ but no reboot. Treat as success, let `wait_node_ready` verify. |
| (any) | (any) | no — ssh torn down by reboot | Expected `--apply` path. Poll `wait_ssh_back`, then `wait_node_ready`. |

Added in commit `9dabcab` from PR #187 round-1 fixes.

## Reboot detection (bootID)

`wait_node_ready` alone is not sufficient to prove a node has actually
rebooted on the new image — there's a brief window where the apiserver
still serves a cached `Ready=True` from the **pre-reboot** kubelet
lease, before the post-reboot kubelet has re-registered. A naive
`wait_node_ready` returns immediately from that cached state and the
script moves on to drain N+1 while N is in the middle of rebooting,
which can cascade into a real outage.

To defeat this, the script captures the node's
`.status.nodeInfo.bootID` **before** issuing `bootc upgrade --apply`,
and after `wait_ssh_back` returns it polls until the bootID differs
from the captured value:

```text
[update-cluster]   pre-reboot bootID: 7a0e3b8c…
[update-cluster] waiting for node hbird-w1 bootID to change from pre-reboot value (timeout 300s)
[update-cluster] node hbird-w1 bootID changed (pre=7a0e3b8c… post=4f1d22a3…) after ~62s
```

The kernel regenerates `bootID` on every boot, so a changed value
*proves* the node has actually rebooted (no apiserver-cache window can
fake it). Only then does `wait_node_ready` run.

Bounded by the existing `READY_TIMEOUT` env knob (no new env var). On
timeout the gate routes through `worker_fail` for workers (so
`--continue-on-error` still applies) or `fail` for the CP (CP failures
remain fatal). The cordoned-node recovery path from
[Recovery from interruption](#recovery-from-interruption) covers the
case where a worker times out at this gate — the node stays cordoned
until the operator restores it.

Implementation: `capture_node_bootid` + `wait_node_bootid_changed` in
`scripts/update-cluster.sh`. The pre-bootid is captured **before**
`bootc_upgrade_apply` is called; `capture_node_bootid` retries up to
3 times with a 2s sleep between attempts so a single transient
apiserver flake doesn't silently regress the gate. If the capture is
still empty after retries, both `capture_node_bootid` and
`wait_node_bootid_changed` log a `WARN:`-prefixed line (greppable in
postmortems) and the gate skips — there's no useful baseline to
compare against, and blocking the whole roll on a missing field is
worse than letting `wait_node_ready` carry the load alone for that
node.

The polling loop emits a progress heartbeat every ~30s during a slow
reboot, and on timeout logs the last observed `pre=…/cur=…` digests so
an operator can tell at a glance whether the apiserver returned
anything at all.

## Daemonset readiness gate

`wait_node_ready` proves kubelet is alive and the CNI **binary** is
present on disk — it does **not** prove the per-node DaemonSet pods
that implement the data plane (Cilium agent, kube-proxy, coredns) are
running and forwarding traffic. Before this gate, the script could
proceed to drain N+1 while Cilium on N was still in `CrashLoopBackOff`,
producing brief networking outages as the next eviction's pods landed
on N+1 with no functional CNI on N to talk to.

After `wait_node_ready` returns, the script now polls all
kube-system pods scheduled on the node and blocks until every
container reports `ready: true`:

```text
[update-cluster] waiting for kube-system DaemonSet pods on hbird-w1 to be Ready (timeout 300s)
[update-cluster] node hbird-w1 kube-system DaemonSet pods all Ready after ~18s
```

The implementation uses a server-side `kubectl get pods
--field-selector=spec.nodeName=NODE -o jsonpath=…` (no client-side jq
dependency on the operator host; matches the
[`bootc_booted_digest`](#bootc-no-update-detection) pattern of running
jq remotely if at all). The filter narrows to pods on this node and
emits per-container readiness; any `false` keeps the wait running.

Bounded by the `DAEMONSET_TIMEOUT` env knob (defaults to `READY_TIMEOUT`
for backward compatibility — see the env-knob table above). On timeout
the gate routes through `worker_fail` for workers (so
`--continue-on-error` still applies) or `fail` for the CP. The same
cordoned-node recovery path from
[Recovery from interruption](#recovery-from-interruption) applies if a
worker hits this timeout.

Round-1 review hardenings (PR #208):

- **Baseline-unready exclusion.** Pods that are already unready at gate
  entry (chronic CrashLoops in kube-system unrelated to this upgrade)
  are snapshotted and excluded — only **new** unready pods (post-
  baseline) gate progress. Without this exclusion a long-standing
  unrelated failure would block every roll forever.
- **Phase-1 wait for first pod.** After a reboot the DaemonSet controller
  takes a few seconds to bind pods to the freshly-rejoined node. The
  gate now first waits up to 60s for at least one kube-system pod to
  appear on the node, then enters the readiness loop. A genuinely-empty
  node (fresh cluster, no DaemonSets deployed) logs a WARN and proceeds
  rather than hanging.
- **Progress heartbeat** every ~30s during the readiness wait so
  operators tailing the log can see the gate is still alive on a slow
  Cilium rollout.

False-positive note: pods in kube-system that are not DaemonSet-managed
(a transient `Job`, an admission-webhook `Deployment`) will also keep
the wait running if any of their containers is not Ready. In practice
the kube-system namespace is overwhelmingly DaemonSets + CP-only
static pods, and the gate's wall-clock cost is dominated by the
worst-case container rather than the count of pods evaluated — so we
accept the over-conservative match in exchange for not introducing a
fragile owner-reference jsonpath.

For Cilium-specific failure modes that surface at this gate (chronic
agent crashloop, kube-proxy/iptables conflict, missing CRDs), see
[`docs/cilium-migration.md`](cilium-migration.md) and the CNI section
of [`docs/troubleshooting.md`](troubleshooting.md). If
`--skip-gates` is necessary as a workaround, document the cluster
state in the issue you file so the gate logic can be tightened.

## `--apply` fallback for older bootc

`bootc upgrade --apply` is bootc 1.1+. The script probes for support
per-node:

```bash
ssh root@<ip> "bootc upgrade --help | grep -q -- '--apply'"
```

If the flag isn't there, the script falls back to a two-step:

```bash
ssh root@<ip> "bootc upgrade"
ssh root@<ip> "systemctl reboot"
```

The probe is per-IP because heterogeneous clusters (mid-upgrade of
bootc itself across nodes) are possible.

## Interaction with the in-image auto-update timer

Post-#181, both Hummingbird flavors (`k8s` and `k8s-worker`) ship
`bootc-semver-update.timer` **enabled** by default — that's the
canonical in-image updater (semver-aware, no reboot, see
[`docs/auto-updates.md`](auto-updates.md)). The legacy
`bootc-fetch-apply-updates.timer` is **disabled** in the new preset
but still present on the image, and pre-#181 hosts only have the
legacy timer.

On any node with an auto-update timer enabled, leaving it running
during a manual rolling upgrade risks:

- Two concurrent staged deployments racing each other.
- The timer kicking off (post-#181: a `bootc switch`; pre-#181: a
  reboot) while `kubectl drain` is still evicting pods.

To avoid both, `update-cluster.sh` stops **both timer units** on each
node before the per-node upgrade — best-effort per unit so the call
succeeds whether the host is post- or pre-#181. After the per-node
upgrade lands, the script probes the remote for which timer unit
exists and restarts the right one:

```bash
if systemctl cat bootc-semver-update.timer >/dev/null 2>&1; then
  systemctl start bootc-semver-update.timer    # post-#181 canonical
elif systemctl cat bootc-fetch-apply-updates.timer >/dev/null 2>&1; then
  systemctl start bootc-fetch-apply-updates.timer  # pre-#181 fallback
fi
```

This means a pre-#181 host being rolled forward by `update-cluster.sh`
keeps its legacy timer running until the in-place upgrade actually
swaps the image to one with the new preset — at which point subsequent
`update-cluster.sh` runs pick up `bootc-semver-update.timer` instead.
Without the probe, blindly starting `bootc-semver-update.timer` on a
pre-#181 host returned rc=5 (no such unit), swallowed by `|| true`,
leaving the node with no auto-update timer running after a `bootc
upgrade` rc=2 ("no update available") path.

`--skip-drain` turns the dance off — appropriate when you're already
running the upgrade manually and don't want the orchestrator restarting
a timer you intentionally have stopped.

The EXIT trap also best-effort restarts the auto-update timer (with
the same post-#181 / pre-#181 probe) on the in-flight node, so an
aborted run doesn't leave the cluster with auto-updates permanently
off.

## Recovery from interruption

The script tracks two pieces of in-flight state — `IN_FLIGHT_NODE`
(the node currently being updated) and `IN_FLIGHT_DRAINED` (whether
that node has been drained but not yet uncordoned). On any non-zero
exit, the EXIT trap prints a recovery hint:

```text
[update-cluster] WARNING: node hbird-w1 is cordoned and was not uncordoned.
[update-cluster] Restore it manually once you have verified its state:
[update-cluster]   ssh root@<cp-ip> "kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon hbird-w1"
```

The script deliberately does **not** auto-uncordon. Operator Ctrl-C
usually means something else is wrong, and handing workload back to a
possibly-half-upgraded node is worse than leaving it cordoned.

Runbook for recovering after an aborted run:

```bash
# 1. Find the cordoned node:
ssh root@<cp-ip> 'kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes'

# 2. Check it really did come up healthy on the new image:
ssh root@<worker-ip> 'sudo bootc status'
ssh root@<worker-ip> 'systemctl is-active kubelet'

# 3. Uncordon it once you trust the state:
ssh root@<cp-ip> 'kubectl --kubeconfig=/etc/kubernetes/admin.conf uncordon <node>'

# 4. Re-run the rolling update from where you left off. --node=NAME
#    lets you resume on a single node without re-rolling the rest.
make update-node CONFIG=cluster.local.conf NODE=<node>
```

If the script aborted before drain, no recovery is needed beyond
re-running the upgrade.

If the abort came from the
[bootID-changed gate](#reboot-detection-bootid) or the
[DaemonSet readiness gate](#daemonset-readiness-gate) — both bounded
by `READY_TIMEOUT` and routed through `worker_fail` / `fail` — the
node is left cordoned and the runbook above applies as-is. Common
root causes: the bootc reboot stalled at first boot (check
`virsh console <name>` or `journalctl -u systemd-journald`); Cilium
agent on the node is in `CrashLoopBackOff` (check `kubectl logs
-n kube-system <cilium-pod>` from the CP); a network policy is
blocking the new node from reaching the CP.

## Resume after interruption

If a roll dies partway — operator Ctrl-C, host crash, a drain failure
the operator hand-fixed — restart it without re-rolling already-updated
nodes via `--start-from=NAME`:

```bash
# Original run made it through hbird-cp1 + hbird-w1, then failed on
# hbird-w2 mid-drain. Operator hand-fixes hbird-w2's stuck pod, then:
sudo -E bash scripts/update-cluster.sh \
  --start-from=hbird-w2 \
  CONFIG=cluster.local.conf
```

What `--start-from` does:

1. Walks `WORKER_NAMES` in order, skipping every entry that comes
   *before* the named worker.
2. Skips the CP (the CP is always first in the original ordering, so
   a resume point past the first worker is implicitly past the CP).
3. Processes the named worker and every worker after it, in
   `WORKER_NAMES` order.

Compose with `--continue-on-error` if you want a "best-effort resume"
that doesn't re-abort on a different worker:

```bash
sudo -E bash scripts/update-cluster.sh \
  --start-from=hbird-w2 \
  --continue-on-error
```

If you only need to fix **one** node (and don't want to walk subsequent
workers), use `--node=NAME` instead — `--node=` and `--start-from=` are
mutually exclusive on purpose.

## Performance tuning

The default config trades wall-clock for safety: one worker at a time,
generous per-step timeouts. For larger clusters or known-fast workloads
it's worth tuning:

### Parallelism (`--parallel=N`)

```bash
# 5-worker cluster, PDBs allow 2 concurrent evictions:
sudo -E bash scripts/update-cluster.sh --parallel=2
```

The roughly-linear `5 × 5min ≈ 25min` worker time drops to
`ceil(5/2) × 5min ≈ 15min`. The CP still adds its serial `~5min` on
top regardless.

Review the safety considerations in the
[`--parallel=N` flag reference](#-paralleln) before raising N — a
mis-tuned `--parallel=` can stall the whole roll on a PDB deadlock.

### Timeouts (env vars)

The per-step waits default to 5 minutes each, which is generous. On a
small homelab cluster with fast SSDs, the actual times are typically:

| Step | Typical | Default timeout |
| --- | --- | --- |
| drain | 5-30s | `DRAIN_TIMEOUT=5m` |
| SSH back post-reboot | 30-60s | `SSH_TIMEOUT=300` |
| apiserver back (CP only) | 60-120s | `APISERVER_TIMEOUT=300` |
| bootID changed | 30-90s | `READY_TIMEOUT=300` |
| node Ready | 30-90s | `READY_TIMEOUT=300` |
| DaemonSet pods Ready | 10-60s | `DAEMONSET_TIMEOUT=${READY_TIMEOUT}` |

The script's startup banner pre-announces the per-node worst-case time
budget (`drain ${DRAIN_TIMEOUT} + ssh-back ${SSH_TIMEOUT} + bootID
${READY_TIMEOUT} + ready ${READY_TIMEOUT} + daemonsets
${DAEMONSET_TIMEOUT}`) so an operator can compute "when will this
finish?" off the cluster size before hitting Enter.

Tightening these makes a stuck node fail faster instead of waiting the
full 5 minutes:

```bash
DRAIN_TIMEOUT=2m \
SSH_TIMEOUT=120 \
READY_TIMEOUT=180 \
sudo -E make update-cluster CONFIG=cluster.local.conf
```

Conversely, loosen them on slow / busy clusters with heavy graceful
shutdowns or large image pulls on first boot:

```bash
DRAIN_TIMEOUT=15m \
READY_TIMEOUT=600 \
sudo -E make update-cluster CONFIG=cluster.local.conf
```

`INTER_NODE_SLEEP` (default 5s) is the post-uncordon settle window
before moving to the next node. Drop to `0` if you want maximum
throughput; raise to `30` if you want each node's pods to fully
re-spread before the next eviction starts.

### `make` `FLAGS=` passthrough

The Makefile targets honor a `FLAGS=` variable that's appended to the
underlying `scripts/update-cluster.sh` invocation. This means you can
stay in the `make` UX even for the operator-ergonomics flags:

```bash
# Dry-run preview with the resume + parallel + emptydir-preserve combo:
make update-cluster CONFIG=cluster.local.conf \
  FLAGS='--dry-run --start-from=hbird-w2 --parallel=2 --no-delete-emptydir-data'

# Resume an aborted roll, best-effort across remaining workers:
make update-cluster CONFIG=cluster.local.conf \
  FLAGS='--start-from=hbird-w2 --continue-on-error'
```

Combine with the env-tunable timeouts as needed; both `FLAGS=` and the
env vars are propagated to the script.

## Rolling time estimates

Per-node cost is roughly:

- Drain: 5-90s depending on workload count and PodDisruptionBudgets.
- `bootc upgrade --apply` + reboot: 60-120s.
- Wait for SSH back + Ready: 30-90s.
- Uncordon + 5s pause: ~10s.

Round number: **~5 minutes per node** for a typical cluster, dominated
by reboot + kubelet rejoin. Whole-cluster estimates:

| Topology | Approx wall-clock |
| --- | --- |
| 1 CP + 0 workers | ~5 min |
| 1 CP + 2 workers | ~15 min |
| 1 CP + 3 workers | ~20-25 min |
| 1 CP + 5 workers | ~30 min |

Apiserver outage stays at ~60-120s regardless of cluster size — only
the CP causes it, and it happens exactly once per run.

## Troubleshooting

- **`another update-cluster run is in progress (lock … held)`.** Either
  another operator is rolling the cluster, or a previous run died hard
  and the file is stale. Verify nothing is in flight (`ps`, `tmux ls`),
  then `sudo rm -f /run/hbird-update-cluster.lock`.

- **`could not resolve CP IP for domain … via virsh domifaddr`.** The
  VM doesn't have an active lease. Confirm it's running
  (`virsh list --all`) and that the libvirt default network is up.
  Override with `CP_IP=… make update-cluster`.

- **`could not resolve IP for worker …`.** Same as above for a
  worker. Override with `WORKER_IPS=(ip1 ip2 …)` in
  `cluster.local.conf`, parallel-indexed to `WORKER_NAMES`.

- **`drain failed for … ; refusing to continue`.** A workload has a
  PodDisruptionBudget that can't be satisfied, or a Job pod is
  blocking. Investigate first — `kubectl get poddisruptionbudgets -A`
  and `kubectl describe pod <stuck>`. Last-resort override:
  `--skip-drain`.

- **`worker … did not come back over SSH within 5min`.** The node
  rebooted into a broken state. `virsh console <name>` to see early
  boot, or boot the previous deployment from the GRUB menu and
  `bootc rollback`. See [`docs/rollback.md`](rollback.md).

- **`worker … did not reach Ready within 5min`.** SSH is back but
  kubelet hasn't rejoined. `ssh root@<worker-ip> journalctl -u
  kubelet.service` — usually a Cilium / CNI issue or a stale CA cert.

- **`node … bootID did not change after 5min`.** The node came back
  on SSH but its `.status.nodeInfo.bootID` is still the pre-reboot
  value — either kubelet hasn't re-registered yet, or the node was
  somehow returned to SSH without an actual reboot (very rare; check
  `uptime` on the node). See
  [Reboot detection (bootID)](#reboot-detection-bootid).

- **`kube-system DaemonSet pods not Ready on this node after 5min`.**
  Node Ready but Cilium / kube-proxy / coredns on this node is still
  CrashLooping. From the CP: `kubectl get pods -n kube-system
  --field-selector=spec.nodeName=<node>` + `kubectl logs -n kube-system
  <unready-pod>`. See
  [Daemonset readiness gate](#daemonset-readiness-gate).

- **`CP apiserver did not return within 5min`.** The CP reboot
  succeeded but the apiserver isn't responding. `ssh root@<cp-ip>
  journalctl -u kubelet.service` and check that the etcd / apiserver
  static pods are running (`crictl ps`).

- **Mid-flight Ctrl-C.** See [Recovery from
  interruption](#recovery-from-interruption). The cordoned node will
  not be auto-uncordoned.

- **bootc upgrade fails with rc != 0 and rc != 255.** The script
  fails fast. Investigate via `ssh root@<ip> bootc upgrade` directly;
  look at `journalctl -u bootc-fetch-apply-updates` and `bootc status`.

For rollback procedures (manual `bootc rollback` from a serial
console, the auto-rollback path that fires on unhealthy boots), see
[`docs/rollback.md`](rollback.md).

## See also

- [`docs/deploy-cluster.md`](deploy-cluster.md) — the deploy side of
  the same config file.
- [`docs/auto-updates.md`](auto-updates.md) — the per-VM bootc
  timer that this script complements.
- [`docs/argocd.md`](argocd.md) — after an update, refresh the
  ArgoCD-registerable kubeconfig with `make export-argocd`.
- [`docs/rollback.md`](rollback.md) — when an upgrade lands a bad
  image and you need to back out.
- [`docs/makefile.md`](makefile.md) — Makefile target cheatsheet.
- Upstream `bootc-upgrade(8)` — `man bootc-upgrade` on any node, or
  the bootc project README at <https://github.com/containers/bootc>.
