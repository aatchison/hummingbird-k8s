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

# Same invocation, but ALSO drop sudo on the KVM host (operator is in
# the libvirt group there). Default keeps sudo for the other lifecycle
# scripts pending Phase 3 of #269. See "Running without sudo" below.
KVM_HOST=geary HBIRD_REMOTE_NO_SUDO=1 make update-cluster CONFIG=cluster.local.conf

# On the KVM host: either root OR a member of the `libvirt` group
# (post-#269). Operators in the libvirt group don't need sudo at all
# for update-cluster.
make update-cluster CONFIG=cluster.local.conf      # libvirt-group member
sudo make update-cluster CONFIG=cluster.local.conf # root
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
6. Waits for SSH to **drop** (proves the reboot has actually started —
   bootc's `--apply` queues the reboot via systemd-run with a small
   delay, so without this gate the next step would false-success on
   the still-up pre-reboot connection; see
   [SSH-drop gate](#ssh-drop-gate)); then waits for SSH to come back;
   then waits for `bootID` to differ from the pre-reboot value (proves
   a real reboot happened — defeats the stale-apiserver-cache hit on
   Ready); then for `kubectl get node NAME` to report `Ready` (the regex
   covers `Ready,SchedulingDisabled` too — workers come back
   cordoned-but-Ready until step 8 lands); then for every kube-system
   DaemonSet pod on the node to be Ready (see
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
| `DRY_RUN=1` | unset | Same as `--dry-run` — print actions, don't ssh/kubectl. Useful for unprivileged previews; bypasses the root-or-libvirt-group check (see [Running without sudo](#running-without-sudo-libvirt-group-operator-269)). |
| `HBIRD_REMOTE_NO_SUDO=1` | unset | When the C3 shim re-execs on `$KVM_HOST`, drop the `sudo` prefix from the remote command. Appropriate when the operator is in the `libvirt` group on the KVM host. See [Running without sudo](#running-without-sudo-libvirt-group-operator-269). Default keeps `sudo` so the other lifecycle scripts (which need POOL_DIR root) stay correct. |
| `DRAIN_TIMEOUT` | `5m` | `kubectl drain --timeout=` value (Go duration string). Tune up for workloads with slow graceful-shutdown. |
| `READY_TIMEOUT` | `300` | Seconds to wait for `kubectl get node` to report `Ready` post-reboot. Also bounds the [bootID-changed gate](#reboot-detection-bootid). Must be > 0. |
| `DAEMONSET_TIMEOUT` | `${READY_TIMEOUT}` | Seconds to wait for the [DaemonSet readiness gate](#daemonset-readiness-gate). Defaults to `READY_TIMEOUT` for compatibility; set independently when the DS gate needs more (or less) headroom than node-Ready. Must be > 0. |
| `APISERVER_TIMEOUT` | `300` | Seconds to wait for the CP apiserver to answer `/readyz` after the CP reboots. |
| `SSH_TIMEOUT` | `300` | Seconds to wait for `ssh root@<ip>` to come back post-reboot. |
| `SSH_DROP_TIMEOUT` | `30` | Seconds to wait for `ssh root@<ip>` to become unreachable after `bootc upgrade --apply` queues the reboot. Diagnostic gate; a timeout logs WARN but does not fail the run (the bootID-changed gate is the source of truth when not skipped). See [SSH-drop gate](#ssh-drop-gate). Must be > 0. |
| `INTER_NODE_SLEEP` | `5` | Seconds to pause after uncordoning a node before processing the next one (small settle window). |

All knobs honor the standard `VAR=value make …` pattern. Env vars set
on the `make` line survive into the script directly (no `sudo`
boundary on the client; on the KVM host, the C3 SSH-wrap re-execs over
SSH which forwards the allowlisted env vars). See
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

### Running without sudo (libvirt-group operator, #269)

`update-cluster.sh`'s local libvirt footprint is small: `virsh
domifaddr` for IP resolution + ssh known_hosts handling. Both are
reachable to any user in the `libvirt` group on the KVM host —
libvirt authorizes `qemu:///system` via the unix-socket group, not
via sudo. Remote ops (ssh + kubectl on the CP) run as
`root@<node-ip>` via the operator's SSH key and are unaffected by
the local EUID.

One-time setup on the KVM host:

```bash
ssh $KVM_HOST 'sudo usermod -aG libvirt $USER && newgrp libvirt'
# (or just log out + back in to pick up the new group)
```

Then run `update-cluster` without sudo on either side:

```bash
# From a client laptop — the shim re-execs over SSH WITHOUT prefixing
# sudo on the remote when HBIRD_REMOTE_NO_SUDO=1 is set.
KVM_HOST=geary HBIRD_REMOTE_NO_SUDO=1 \
  make update-cluster CONFIG=cluster.local.conf

# On the KVM host directly, as a libvirt-group user:
make update-cluster CONFIG=cluster.local.conf
```

Defaults are conservative: `HBIRD_REMOTE_NO_SUDO` is **off** by
default, and the other lifecycle scripts (`deploy-cluster`,
`destroy-cluster`, `spawn-workers`) still require root locally + sudo
on the remote because they write to the root-owned `POOL_DIR`. That
deferred work is tracked as Phase 3 of #269.

Diagnostic when neither condition holds (non-root + not in the
`libvirt` group):

```text
[update-cluster] FAIL: must be root or a member of the libvirt group on this host. Add yourself with:
  sudo usermod -aG libvirt $USER && newgrp libvirt
then rerun. (Dry-run does not require either: rerun with --dry-run to preview.)
```

Dry-run (`--dry-run` / `DRY_RUN=1`) skips the EUID/group check
entirely so previews work for any user.

### Env-var validation (security)

Operator-shell env vars reach `scripts/update-cluster.sh` directly on
the client and survive the SSH re-exec to the KVM host via the
allowlist in `scripts/lib/ssh-wrap.sh`; they ultimately reach the
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
| `SSH_DROP_TIMEOUT` | `^[1-9][0-9]*$` (strictly positive — 0 would defeat the gate) |
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

### `--node-name-override DOMAIN=NODE`

Pin the k8s node name for a libvirt domain, bypassing the auto-resolution
described in [k8s node name resolution](#k8s-node-name-resolution-260).
Repeatable; also accepts comma-separated `DOMAIN=NODE` pairs in a single
flag value.

```bash
# Single override:
sudo -E bash scripts/update-cluster.sh \
  --node-name-override=hbird-w1=humbird-worker-748f4cf5

# Multiple overrides via repeated flag:
sudo -E bash scripts/update-cluster.sh \
  --node-name-override=hbird-w1=humbird-worker-748f4cf5 \
  --node-name-override=hbird-w2=humbird-worker-baef476c

# Equivalent via comma-separated value:
sudo -E bash scripts/update-cluster.sh \
  --node-name-override=hbird-w1=humbird-worker-748f4cf5,hbird-w2=humbird-worker-baef476c
```

Use when:

- The apiserver isn't reachable for the auto-resolution step (e.g. you're
  rolling forward FROM a known-broken CP and the resolver can't query
  `kubectl get nodes`).
- You're mid-migration to renamed nodes and want to pin a specific target
  without waiting for the IP-based lookup to find them.
- The resolver finds the wrong node (multi-NIC VMs where the address
  kubelet registered with isn't the one `virsh domifaddr` returns).

`DOMAIN` must match `CP_NAME` or one of `WORKER_NAMES`; an unknown
`DOMAIN` is rejected up front so a typo can't silently misroute kubectl.
The override path takes precedence over the kubectl-based resolver, so a
mistaken `NODE` value will be issued to kubectl verbatim — double-check
against `kubectl get nodes` before pinning.

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
require root or `libvirt`-group membership, doesn't acquire the lock,
doesn't touch any VM. The script substitutes `<resolved-at-runtime>`
for IPs it would otherwise look up via `virsh domifaddr`. See
[Running without sudo](#running-without-sudo-libvirt-group-operator-269)
for the non-dry-run unprivileged path.

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

A single `flock`-held lock at `$XDG_RUNTIME_DIR/hbird-update-cluster.lock`
prevents the same operator from racing themselves into a mass-cordon.
On systemd hosts `XDG_RUNTIME_DIR` resolves to `/run/user/$UID` — a
per-user tmpfs that's always writable by the owning user, so this
works for non-root libvirt-group members without any `/run` write
access or system-tmpfiles configuration (#275).

When `XDG_RUNTIME_DIR` is unset (cron / non-interactive sessions
sometimes lack it) the script falls back to `/tmp/hbird-update-cluster.lock`.

The lock is **per-user**: two operators on the same host get distinct
lock files and therefore don't block each other. flock here serializes
a given user's own concurrent invocations, not cross-user runs. If you
need to coordinate across operators on one host, agree out-of-band
before running.

`/run/user/$UID` is tmpfs, so the lock dies with the user session —
no stale-lock cleanup is needed across reboots.

If a run dies hard (kill -9, host crash mid-run, etc.) and the lock
shows held for a process that no longer exists, `flock -n` releases it
when the holding fd closes; clearing the file by hand is safe (no
sudo required — it's in your own runtime dir):

```bash
rm -f "${XDG_RUNTIME_DIR:-/tmp}/hbird-update-cluster.lock"
```

Dry-run mode skips the lock entirely so it can run without any
runtime-dir write access (CI, unprivileged previews).

## k8s node name resolution (#260)

The libvirt domain names in `WORKER_NAMES` / `CP_NAME` are **not
necessarily** the same as the k8s node names that `kubectl get nodes`
reports. On clusters deployed before PR #255, the worker's k8s node
name fell back to a machine-id-derived value like
`humbird-worker-748f4cf5` while the libvirt domain stayed `hbird-w1`.
Pre-#260, `update-cluster.sh` issued `kubectl drain $libvirt_domain`
against those clusters and saw:

```text
[update-cluster] kubectl drain hbird-w1 --ignore-daemonsets --timeout=5m
Error from server (NotFound): nodes "hbird-w1" not found
[update-cluster] ERROR: hbird-w1: drain failed (use --skip-drain to override)
```

Today the script resolves each libvirt domain to a k8s node name
**before** any `kubectl drain` / `kubectl wait` / `kubectl uncordon`
call. The resolution path:

1. If the operator passed `--node-name-override DOMAIN=NODE` for this
   domain, use that value verbatim — the escape hatch.
2. Otherwise, look up the k8s node whose `InternalIP` address matches
   the IP `virsh domifaddr` returned for the domain. We use a
   list-and-grep pattern (one round-trip to the apiserver, awk filter
   client-side):

   ```bash
   kubectl get nodes -o jsonpath=\
     '{range .items[*]}{.metadata.name}{"="}{.status.addresses[?(@.type=="InternalIP")].address}{"\n"}{end}' \
     | awk -F= -v ip="$ip" '$2 == ip { print $1; exit }'
   ```

   The kubectl call emits one `<name>=<internal-ip>` row per node, awk
   matches the row whose IP equals the libvirt-resolved one and prints
   the name. We deliberately filter on `type=InternalIP` because nodes
   can carry multiple addresses (Hostname, ExternalIP, etc.) and
   `virsh domifaddr` returns the NAT'd-VM InternalIP.

   **Why not a nested filter?** PR #263 originally used
   `{range .items[?(@.status.addresses[?(@.address=="X")])]}{.metadata.name}{end}`,
   which is conceptually cleaner but kubectl's JSONPath implementation
   does NOT support a `[?(...)]` filter inside another `[?(...)]` —
   it rejects the expression at parse time with `unterminated filter`.
   The previous code swallowed that error with `2>/dev/null || true`,
   so the resolver always returned empty and `update-cluster` failed at
   the CP-resolve step on every cluster. PR #267 replaced the nested
   filter with the list-and-grep pattern above and a bats drift fence
   (`#267 resolve_k8s_node_name uses list-and-grep, not nested-filter
   jsonpath`) now blocks the bad pattern from re-landing.

3. On a lookup miss, fail loudly with a diagnostic that names the
   libvirt domain, the IP that was searched against, and the override
   flag to use as the escape hatch — plus the raw `name=ip` rows the
   apiserver returned so the operator can see at a glance which IP
   the apiserver thinks each node has:

   ```text
   ERROR: could not resolve k8s node for libvirt domain hbird-w1 (ip=192.168.122.11) — node may not be joined, or apiserver unreachable
          (use --node-name-override hbird-w1=NAME to force a specific k8s node name)
          kubectl rows seen:
            hbird-cp1=192.168.122.206
            hbird-w2=192.168.122.178
   ```

   If kubectl itself fails (e.g. a future jsonpath bug, apiserver
   unreachable), the resolver now logs a `WARN: kubectl call failed
   during k8s node resolution …` with the captured stderr — so a
   parse error like #267's `unterminated filter` is visible in the
   logs instead of presenting as the misleading "node may not be
   joined" diagnostic.

When the resolved k8s node name differs from the libvirt domain, the
startup banner surfaces the mapping so the operator can confirm the
resolver picked the right targets:

```text
[update-cluster] CP=hbird-cp1 (192.168.122.10) k8s-node=hbird-cp1, workers=(hbird-w1 hbird-w2)
[update-cluster]   resolved libvirt domain hbird-w1 -> k8s node humbird-worker-748f4cf5
[update-cluster]   resolved libvirt domain hbird-w2 -> k8s node humbird-worker-baef476c
```

Logs continue to use the libvirt domain as the human-readable identifier
(`WORKER: hbird-w1`) — that's what the operator wrote in
`cluster.local.conf` and what `virsh list` shows. Only the kubectl-side
calls (`drain`, `wait_node_ready`, `wait_node_bootid_changed`,
`wait_node_daemonsets_ready`, `uncordon`) target the resolved k8s name.

The cordoned-recovery hint emitted by the EXIT trap uses the **k8s** name
so the operator can paste the suggested `kubectl uncordon …` command
without translating between libvirt and k8s namespaces.

CP resolution is the same shape (`CP_NAME` → `CP_IP` → CP k8s node
name). PR #255's emit-hostname-in-user-data fix means the CP path
usually keeps the names in sync, but we resolve defensively anyway —
the resolver is cheap (one `kubectl get nodes` round-trip per node, at
startup) and protects against the same divergence sneaking back in via
a different cloud-init regression.

Dry-run skips the apiserver call entirely (no kubectl to invoke) and
returns the libvirt domain verbatim — so dry-run output and the
integration-update-cluster `assert-dry-run-sequence` patterns continue
to hold.

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

> `jq` runs on the bootc host (not the operator), so it has to be
> in the image. It is baked into the primary `dnf install -y` layer
> of `containers/k8s/Containerfile` and
> `containers/k8s-worker/Containerfile` alongside
> `kubeadm`/`kubectl`/`cri-o`. Pre-#257 builds placed `jq` only in
> the optional `bootc-semver-update` install layer, which was easy
> to misread as conditional; moving it to the primary layer keeps
> the dependency obvious and prevents a silent regression of this
> detection (and of `bootc-semver-update.sh`'s own tag parsing).
> A bats drift fence in `tests/containers/containerfile-deps.bats`
> pins `jq` to that line.

| pre digest | post digest | ssh stayed up? | Interpretation |
| --- | --- | --- | --- |
| sha256:A | sha256:A | yes | No update available — short-circuit waits, move on. |
| sha256:A | sha256:B | yes | Odd — digests differ but no reboot. Treat as success, let `wait_node_ready` verify. |
| (any) | (any) | no — ssh torn down by reboot | Expected `--apply` path. Poll `wait_ssh_back`, then `wait_node_ready`. |

Added in commit `9dabcab` from PR #187 round-1 fixes.

## SSH-drop gate

`bootc upgrade --apply` does not reboot synchronously — it queues the
reboot via `systemd-run` (or an equivalent unit) with a small delay,
typically firing within ~5 seconds. Without a "wait for SSH to go DOWN"
prelude, the next gate (`wait_ssh_back`) polls the still-up pre-reboot
connection, finds SSH responding, and reports `SSH back on <ip> after
~0s` — declaring success before the reboot has even started.

The downstream effect: when `--skip-gates` is set (which skips the
[bootID-changed gate](#reboot-detection-bootid)), the script would
proceed with a false success and drain N+1 while N is still actually
rebooting.

To defeat this, after `bootc upgrade --apply` the script polls SSH on
the target IP until the connection fails (sshd has gone down) or
`SSH_DROP_TIMEOUT` seconds elapse:

```text
[update-cluster] ssh root@192.168.122.123 bootc upgrade --apply  (auto-reboots; ...)
[update-cluster] waiting for SSH on 192.168.122.123 to drop (timeout 30s)
[update-cluster]   SSH on 192.168.122.123 dropped after ~6s (reboot in progress)
[update-cluster] waiting for SSH to come back on 192.168.122.123 (timeout 300s)
[update-cluster] SSH back on 192.168.122.123 after ~72s
```

The gate is **diagnostic**, not fatal: if SSH stays up for the full
`SSH_DROP_TIMEOUT` window the script logs a WARN and proceeds.

```text
[update-cluster]   WARN: SSH on 192.168.122.123 still up after 30s — bootc may have queued without rebooting
```

This is deliberate. The bootID-changed gate (when not skipped) is the
source of truth for "did the reboot happen". The SSH-drop gate exists
to surface the anomaly in the log — when running with `--skip-gates`,
the WARN is now the only signal that bootc may have failed to reboot.
Bounded by `SSH_DROP_TIMEOUT` (default 30s; 30 is generous for the
typical ~5s systemd-run delay).

Implementation: `wait_ssh_drop` in `scripts/update-cluster.sh`,
inserted between `bootc_upgrade_apply` and `wait_ssh_back` in both
`update_cp` and `update_worker`. The helper uses a fresh SSH probe
(`-o ControlPath=none -o ConnectTimeout=3 -o BatchMode=yes`) so each
poll iteration actually attempts a new TCP handshake — without this,
the persistent ControlMaster session would short-circuit on the
already-multiplexed connection and always succeed.

Discovered live during the 2026-05-26 roll-forward to `k8s/v0.1.42` +
`worker/v0.1.17` (issue #261).

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
| SSH drop post-`--apply` | 2-10s | `SSH_DROP_TIMEOUT=30` |
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
make update-cluster CONFIG=cluster.local.conf
```

Conversely, loosen them on slow / busy clusters with heavy graceful
shutdowns or large image pulls on first boot:

```bash
DRAIN_TIMEOUT=15m \
READY_TIMEOUT=600 \
make update-cluster CONFIG=cluster.local.conf
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

- **`another update-cluster run is in progress (lock … held)`.** YOU
  (this operator) have another roll in flight — the lock is per-user
  in `$XDG_RUNTIME_DIR/hbird-update-cluster.lock`, so a different user
  on the host could never trigger this for you. Verify nothing is in
  flight (`ps`, `tmux ls`), then `rm -f "${XDG_RUNTIME_DIR:-/tmp}/hbird-update-cluster.lock"`
  (no sudo needed).

- **`could not resolve CP IP for domain … via virsh domifaddr`.** The
  VM doesn't have an active lease. Confirm it's running
  (`virsh list --all`) and that the libvirt default network is up.
  Override with `CP_IP=… make update-cluster`.

- **`could not resolve IP for worker …`.** Same as above for a
  worker. Override with `WORKER_IPS=(ip1 ip2 …)` in
  `cluster.local.conf`, parallel-indexed to `WORKER_NAMES`.

- **`could not resolve k8s node for libvirt domain … (ip=…)`.** The
  libvirt domain resolved to an IP but no k8s node has that IP in its
  `.status.addresses`. Either the node hasn't joined yet
  (`kubectl get nodes` to check), the apiserver is unreachable from the
  CP at the moment, or the kubelet registered with a different address
  (e.g. multi-NIC VM where libvirt sees the management NIC but kubelet
  registered with a pod-network NIC). Force the right k8s name with
  `--node-name-override DOMAIN=NODE` once you've identified it. See
  [k8s node name resolution](#k8s-node-name-resolution-260).

- **`drain failed for … ; refusing to continue`.** A workload has a
  PodDisruptionBudget that can't be satisfied, or a Job pod is
  blocking. Investigate first — `kubectl get poddisruptionbudgets -A`
  and `kubectl describe pod <stuck>`. Last-resort override:
  `--skip-drain`.

- **`WARN: SSH on <ip> still up after Ns — bootc may have queued without rebooting`.**
  The SSH-drop diagnostic gate timed out; bootc returned cleanly from
  `bootc upgrade --apply` but sshd never went away in the
  `SSH_DROP_TIMEOUT` window. The run continues (the bootID gate is the
  authoritative reboot check) — but if you're running with
  `--skip-gates`, this WARN is the only reboot-didn't-start signal
  you'll get. Investigate on the node: `journalctl -u bootc-fetch-apply-updates`
  and `bootc status` to see whether the deployment was actually staged.
  See [SSH-drop gate](#ssh-drop-gate).

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
