# Auto-reboot: scheduling `update-cluster` to apply staged images

The cluster already **fetches and stages** new images automatically — the
per-node `bootc-semver-update.timer` resolves the highest `vX.Y.Z` tag at GHCR
daily and does `bootc switch` + `bootc upgrade`, which **stages without
rebooting** (see [`docs/auto-updates.md`](auto-updates.md)). Nothing applies the
staged image on its own, so a node that has fetched a new build keeps running
the old one until someone reboots it. Check with:

```bash
ssh root@<node> "bootc status --json | jq -r .status.staged.image.image.image"
```

A non-null value means "new image fetched, waiting for a reboot".

This doc wires up the missing **trigger**: a nightly systemd timer that applies
whatever is staged, one node at a time, with drain/uncordon and a bootID reboot
gate. It runs `scripts/hbird-rolling-apply.sh` rather than
`hbird update-cluster` — the latter's live path is unimplemented as of v0.0.1
(see [Why not `hbird update-cluster`?](#why-not-hbird-update-cluster)), so the
executor is a stopgap until upstream #322 lands.

For a fully in-cluster alternative (a reboot daemon such as `kured` that watches
a sentinel and coordinates via a cluster-wide lock), see the note at the end.

## Design

One host-side timer on the KVM host (`geary`):

```text
nightly timer ── ExecCondition ─▶ hbird-staged-check.sh
                                     │ exit 1 (nothing staged) ─▶ service skips, ~2s, no churn
                                     │ exit 0 (something staged) ─▶
                 ExecStart ────────▶ hbird-rolling-apply.sh  (CP ▶ w1 ▶ w2, drain/apply/uncordon)
```

- The **gate** (`scripts/hbird-staged-check.sh`) resolves each node's IP from
  libvirt and probes `bootc status`. It exits `0` only if at least one node has
  a staged image, so quiet nights are three quick SSH probes rather than a
  pointless drain/uncordon cycle across the cluster. Its exit codes are:
  `0` = a node has a staged image (run the roll); `1` = full scan, nothing
  staged; `2` = config unreadable; `3` = scan incomplete (a node could not be
  probed — we do **not** roll on a partial scan). systemd treats any
  `ExecCondition` exit in the range `1`–`254` as a clean **skip**, not a
  failure (only `255` or a signal marks the unit failed), and this gate never
  returns `255` — so a failed probe safely skips the roll.
- The **roll** (`scripts/hbird-rolling-apply.sh`) walks the CP first (no drain —
  single-CP topology has nowhere to evict to), then each worker serially:
  cordon → drain → `systemctl reboot` → **bootID-change wait** → Ready-wait →
  uncordon. A node with nothing staged is skipped untouched, so the roll is
  per-node idempotent. Reboot detection compares
  `/proc/sys/kernel/random/boot_id` before and after: a fast-returning SSH on
  the still-up pre-reboot host cannot false-success. Any node that fails to
  come back (no bootID change, or not Ready in time) aborts the roll rather
  than proceeding to the next node — a broken image takes down one node, not
  the cluster.

### Why not `hbird update-cluster`?

[`docs/update-cluster.md`](update-cluster.md) describes `hbird update-cluster`
as the coordinated roll, and it is the right long-term home for this logic —
but as of `hbird` v0.0.1 its **live path is unimplemented**. A live invocation
aborts with:

```text
`timer_stop` requires a remote SSH/kubectl round-trip that is not yet
implemented in the Rust path. ... Until the live-execution slice lands
(tracked by #322), run with `--dry-run` to preview the plan
```

`--dry-run` succeeds and prints a complete, plausible plan (`succeeded (3)`),
which makes this easy to miss when validating an unattended timer — the dry-run
exercises none of the SSH/kubectl surface. Two things fail before that point,
too: live mode does not resolve node IPs from libvirt (it requires `CP_IP` /
`WORKER_IPS` in the config), and the error's own suggested fallback is
circular — `make update-cluster` just calls `hbird update-cluster`, and the
bash twin `scripts/update-cluster.sh` was removed in the v0.1.0 cutover.

So this doc drives the shell executor instead. When #322 lands, point
`ExecStart` back at `hbird update-cluster --config …` and delete the script —
the surrounding gate, unit, and linger setup are unchanged.

### Run it as a *user* service, not a system service

On an SELinux-enforcing host (Fedora default) a **system** service cannot
`exec` files that live under `/home` — they are labelled `user_home_t` /
`home_bin_t`, and the system-service domain is denied execute on them. A system
unit therefore fails at the `ExecCondition`/`ExecStart` step with
`Permission denied`, and would also be blocked reading the operator's
`~/.ssh` keys.

Running it as a **user** service (`systemctl --user`) sidesteps all of this: the
unit runs in the operator's own login domain — the same context as the
interactive shell where `hbird`, `ssh root@<node>`, and `virsh` already work.
The only cost is needing `loginctl enable-linger` so the timer fires while no
session is open.

## Prerequisites

- `jq` on the KVM host, and `jq` + `bootc` on the nodes (both are already in the
  node images). No `hbird` binary is required by the timer itself — the gate and
  the executor are plain shell. (`hbird` is still worth having for
  `--dry-run` previews and the rest of the lifecycle; see
  [`docs/rust-cli.md`](rust-cli.md).)
- The operator is in the `libvirt` group (so `virsh -c qemu:///system` and the
  cluster lifecycle tools work without sudo).
- Passwordless SSH as `root@<node>` to every node (the same key the deploy uses).
- A checkout of this repo at `~/hummingbird-k8s` with a populated
  `cluster.local.conf` (the same config `deploy-cluster`/`update-cluster` read).

## Unit files

Both files are user units. `%h` expands to the operator's home directory, so
they work unmodified for any operator whose checkout is at `~/hummingbird-k8s`.

`~/.config/systemd/user/hbird-update.service`:

```ini
[Unit]
Description=hbird rolling bootc upgrade (apply staged node images)
Documentation=https://github.com/aatchison/hummingbird-k8s/blob/main/docs/auto-reboot.md

[Service]
Type=oneshot
WorkingDirectory=%h/hummingbird-k8s
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin

# Gate: only roll if a node has a STAGED image; otherwise skip cleanly (exit 1).
ExecCondition=%h/hummingbird-k8s/scripts/hbird-staged-check.sh %h/hummingbird-k8s/cluster.local.conf

# Live executor. NOT `hbird update-cluster`: its live path is unimplemented
# (see "Why not hbird update-cluster?" below) — it aborts at timer_stop.
ExecStart=%h/hummingbird-k8s/scripts/hbird-rolling-apply.sh %h/hummingbird-k8s/cluster.local.conf

# A full CP + workers roll is drain + reboot + rejoin per node. The per-node
# drain timeout is 5m, so worst-case a multi-worker roll can approach or
# exceed 30m; 3600s leaves headroom so systemd never SIGTERMs mid-uncordon.
TimeoutStartSec=3600
Nice=10

[Install]
WantedBy=default.target
```

`~/.config/systemd/user/hbird-update.timer`:

```ini
[Unit]
Description=Nightly hbird rolling bootc upgrade

[Timer]
OnCalendar=*-*-* 03:30:00
RandomizedDelaySec=900
Persistent=true
Unit=hbird-update.service

[Install]
WantedBy=timers.target
```

## Setup

```bash
# 1. Install the units (they live in your user systemd dir).
mkdir -p ~/.config/systemd/user
# ...create the two files above...
systemctl --user daemon-reload
systemctl --user enable --now hbird-update.timer

# 2. Let the timer fire while you are not logged in (needs sudo, once).
sudo loginctl enable-linger "$USER"

# 3. Confirm.
systemctl --user list-timers hbird-update.timer
loginctl show-user "$USER" -p Linger        # expect Linger=yes
```

Do **not** install these as system units in `/etc/systemd/system` — see the
SELinux note above.

## Verify

Trigger it by hand; with nothing staged the gate should skip cleanly:

```bash
systemctl --user start hbird-update.service
journalctl --user -u hbird-update.service -n 20 --no-pager
```

Expected on a quiet run:

```text
hbird-staged-check: no staged bootc updates on any node -> skipping roll
hbird-update.service: Skipped due to 'exec-condition'.
```

That is success: the gate **ran** and decided to skip. Contrast with a
`Permission denied` at the exec step, which means the unit was installed as a
system service and hit the SELinux block described above.

To exercise the executor for real, stage something on exactly one node and run
the service by hand — it should roll that node only and skip the others:

```bash
ssh root@<one-worker> bootc switch ghcr.io/aatchison/hummingbird-k8s-worker:<older-tag>
systemctl --user start hbird-update.service
journalctl --user -u hbird-update.service -f     # expect "[apply] <node>: ..." lines
```

`hbird update-cluster --config … --dry-run` still prints a useful plan preview,
but note it validates none of the live SSH/kubectl path (see
[Why not `hbird update-cluster`?](#why-not-hbird-update-cluster) above), so a
green dry-run is not evidence the unattended roll works.

## Tuning

- **Cadence** — `OnCalendar=*-*-* 03:30:00` is nightly. Weekly
  (`OnCalendar=Sun *-*-* 03:30:00`) reboots less often but lets nodes drift
  further behind the published image.
- **Always roll** — dropping the `ExecCondition` is safe but pointless here: the
  executor re-checks `staged` per node and skips untouched nodes anyway, so the
  gate is a cheap short-circuit rather than the only guard.
- **Timeouts** — `REBOOT_TIMEOUT` (default 600s) bounds the bootID-change wait
  and `READY_TIMEOUT` (600s) the post-reboot Ready wait; both are env overrides
  on the executor. Raise them for slow storage, and raise the unit's
  `TimeoutStartSec` alongside.

## Caveats

- **Single-CP API blip.** On a night where the control plane itself has a staged
  update, applying it reboots the CP and the Kubernetes API is unavailable for
  ~1–2 minutes. This is inherent to a single-CP topology; only an HA control
  plane removes it.
- **No auto-reboot chaining.** The executor stages nothing new — it only
  applies what the per-node timer already staged. The two layers are
  independent: staging (in-image timer) and applying (this timer).
- **The in-image semver timer is not paused during the roll.** `update-cluster`
  stops `bootc-semver-update.timer` on each node first (that is the step whose
  absence #322 tracks); this executor does not. In practice the race is benign:
  the semver timer only ever *stages*, and a stage landing mid-roll is picked up
  on the next night. If you want strict serialization, stop the timers by hand
  around a manual roll.
- **SSH host-key trust.** The gate probes `root@<node>` unattended. It assumes
  the node host keys are already trusted (the deploy populates the operator's
  `known_hosts`); `StrictHostKeyChecking=accept-new` is only a TOFU fallback for
  a freshly re-leased DHCP address. If you want strict verification, pre-populate
  `known_hosts` (or a managed `UserKnownHostsFile`) and drop `accept-new`. When a
  key can't be verified the probe fails, which yields a skip (exit 3), not a roll.

## Alternative: in-cluster reboot daemon

Instead of a host-side timer you can run a reboot daemon (e.g. `kured`) as a
DaemonSet. It detects "reboot required" via `--reboot-sentinel-command` keyed on
`bootc status` showing a staged image, takes a cluster-wide lock, and
cordons/drains/reboots/uncordons one node at a time within a maintenance window
— entirely in-cluster, no external scheduler. The trade-off is a new component
to run and trust, and the single-CP API blip still applies when it reboots the
CP. This repo does not ship a `kured` manifest today.
