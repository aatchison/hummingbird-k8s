# `make update-cluster` — rolling bootc upgrade across a deployed cluster

`scripts/update-cluster.sh` is the operator-facing "roll the cluster
forward to a new image" entry point. It reads the same
`cluster.local.conf` that `make deploy-cluster` consumes and walks the
cluster one node at a time:

```bash
sudo make update-cluster CONFIG=cluster.local.conf
```

It complements — does not replace — the per-VM
`bootc-fetch-apply-updates.timer` described in
[`docs/auto-updates.md`](auto-updates.md). The timer is best for
unattended worker rollouts; this script is the coordinated path for
single-CP clusters where the operator wants drain/uncordon, ordering,
and a clean abort surface.

## Overview

For each node in order — CP first, then each worker in `WORKER_NAMES`
order — the script:

1. Stops `bootc-fetch-apply-updates.timer` on the node (avoids racing
   the in-image auto-update timer described in
   [`docs/auto-updates.md`](auto-updates.md)).
2. For workers only: `kubectl drain NODE --ignore-daemonsets
   --delete-emptydir-data --timeout=5m`. The CP is **not** drained —
   it's a single-CP topology, there's nowhere to drain to.
3. Snapshots the booted image digest via `bootc status --json` so the
   "no actual update" case can be detected (see
   [bootc no-update detection](#bootc-no-update-detection) below).
4. `ssh root@NODE_IP bootc upgrade --apply` — `--apply` auto-reboots.
5. Waits for SSH to come back, then for `kubectl get node NAME` to
   report `Ready` (the regex covers `Ready,SchedulingDisabled` too —
   workers come back cordoned-but-Ready until step 6 lands).
6. For workers: `kubectl uncordon NODE`.
7. Restarts `bootc-fetch-apply-updates.timer` on the node.
8. Sleeps 5s before moving on (small breather; not load-bearing).

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

| Var | Effect |
| --- | --- |
| `CONFIG` | Path to the cluster config (defaults to `./cluster.local.conf`). |
| `DRY_RUN=1` | Same as `--dry-run` — print actions, don't ssh/kubectl. Useful for unprivileged previews; bypasses the root-required check. |

No other public env knobs exist today. Anything else the script needs
(SSH timeouts, ControlPersist window, per-step waits) is hard-coded;
file an issue if you need to tune one.

## Full flag reference

### `--workers-only`

Skip the CP entirely; only roll workers.

```bash
sudo make update-workers CONFIG=cluster.local.conf
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
sudo make update-node CONFIG=cluster.local.conf NODE=hbird-w1

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
`bootc-fetch-apply-updates.timer` stop/restart dance — appropriate when
you're driving an upgrade manually and don't want the orchestrator
touching the in-image timer.

```bash
sudo -E bash scripts/update-cluster.sh --skip-drain
```

Use sparingly — `kubectl drain` is the only thing keeping workloads
from being killed mid-flight. Reserve this for stuck drains where you
have already accepted the workload disruption.

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

Worker and k3s images ship `bootc-fetch-apply-updates.timer`
**enabled** by default; the k8s CP image ships it **disabled** (see
[`docs/auto-updates.md`](auto-updates.md) for the rationale). On any
node that has the timer enabled, leaving it running during a manual
rolling upgrade risks:

- Two concurrent staged deployments racing each other.
- The timer kicking off a reboot while `kubectl drain` is still
  evicting pods.

To avoid both, `update-cluster.sh` stops the timer on each node before
the per-node upgrade and starts it again afterward. `--skip-drain`
turns the dance off — appropriate when you're already running the
upgrade manually and don't want the orchestrator restarting a timer
you intentionally have stopped.

The EXIT trap also best-effort restarts the timer on the in-flight
node, so an aborted run doesn't leave the cluster with auto-updates
permanently off.

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
sudo make update-node CONFIG=cluster.local.conf NODE=<node>
```

If the script aborted before drain, no recovery is needed beyond
re-running the upgrade.

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
