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

This doc wires up the missing **trigger**: a scheduled, coordinated
`hbird update-cluster` that applies whatever is staged, one node at a time,
with drain/uncordon and the bootID reboot-gate. It reuses the tool documented
in [`docs/update-cluster.md`](update-cluster.md) — nothing new orchestrates the
roll; the timer just runs it on a schedule.

For a fully in-cluster alternative (a reboot daemon such as `kured` that watches
a sentinel and coordinates via a cluster-wide lock), see the note at the end.

## Design

One host-side timer on the KVM host (`geary`):

```
nightly timer ── ExecCondition ─▶ hbird-staged-check.sh
                                     │ exit 1 (nothing staged) ─▶ service skips, ~2s, no churn
                                     │ exit 0 (something staged) ─▶
                 ExecStart ────────▶ hbird update-cluster  (CP ▶ w1 ▶ w2, drain/apply/uncordon)
```

- The **gate** (`scripts/hbird-staged-check.sh`) resolves each node's IP from
  libvirt and probes `bootc status`. It exits `0` only if at least one node has
  a staged image, so quiet nights are three quick SSH probes rather than a
  pointless drain/uncordon cycle across the cluster. A non-zero `ExecCondition`
  makes systemd skip the unit cleanly — it is not a failure.
- The **roll** is plain `hbird update-cluster`. On a night where nothing is
  staged the gate skips it entirely; even if forced, `bootc upgrade --apply` is
  itself a no-op when there is no staged image.

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

- The `hbird` CLI installed for the operator, e.g. at `~/.local/bin/hbird`
  (build with `cargo build --release --bin hbird` under `rust/`, or install a
  release binary per [`docs/rust-cli.md`](rust-cli.md)).
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

# Call hbird directly with --config. NOTE: `make update-cluster CONFIG=...` does
# not work with the current CLI — the Makefile only exports CONFIG in the env,
# but `hbird update-cluster` requires the --config flag.
ExecStart=%h/.local/bin/hbird update-cluster --config %h/hummingbird-k8s/cluster.local.conf

# A full CP + workers roll (drain + reboot + rejoin each) fits comfortably.
TimeoutStartSec=1800
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

```
hbird-staged-check: no staged bootc updates on any node -> skipping roll
hbird-update.service: Skipped due to 'exec-condition'.
```

That is success: the gate **ran** and decided to skip. Contrast with a
`Permission denied` at the exec step, which means the unit was installed as a
system service and hit the SELinux block described above.

To dry-run the underlying roll itself (plan only, changes nothing):

```bash
hbird update-cluster --config ~/hummingbird-k8s/cluster.local.conf --dry-run
```

## Tuning

- **Cadence** — `OnCalendar=*-*-* 03:30:00` is nightly. Weekly
  (`OnCalendar=Sun *-*-* 03:30:00`) reboots less often but lets nodes drift
  further behind the published image.
- **Always roll** — drop the `ExecCondition` line to run `update-cluster` every
  night regardless. It is still safe (bootc no-update detection skips the actual
  reboot), but each run drains every worker first, so you trade a little nightly
  pod churn for not depending on the gate.

## Caveats

- **Single-CP API blip.** On a night where the control plane itself has a staged
  update, applying it reboots the CP and the Kubernetes API is unavailable for
  ~1–2 minutes. This is inherent to a single-CP topology; only an HA control
  plane removes it.
- **No auto-reboot chaining.** `update-cluster` stages nothing new — it only
  applies what the per-node timer already staged. The two layers are
  independent: staging (in-image timer) and applying (this timer).

## Alternative: in-cluster reboot daemon

Instead of a host-side timer you can run a reboot daemon (e.g. `kured`) as a
DaemonSet. It detects "reboot required" via `--reboot-sentinel-command` keyed on
`bootc status` showing a staged image, takes a cluster-wide lock, and
cordons/drains/reboots/uncordons one node at a time within a maintenance window
— entirely in-cluster, no external scheduler. The trade-off is a new component
to run and trust, and the single-CP API blip still applies when it reboots the
CP. This repo does not ship a `kured` manifest today.
