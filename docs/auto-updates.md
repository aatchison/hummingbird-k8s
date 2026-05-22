# Automatic bootc updates

`hummingbird-k3s` and `hummingbird-k8s-worker` ship with
`bootc-fetch-apply-updates.timer` **enabled by default**. The
`hummingbird-k8s` control-plane image ships with the timer **disabled by
default** — opt in per host via `systemctl enable --now` (see below). The
rationale: a single-CP cluster suffers a full apiserver outage during the
CP reboot, so auto-update on the CP needs to be an explicit operator
choice (#48).

## Tracking GHCR vs localhost

The local build path (`scripts/build-*.sh` → `bib`) produces qcow2s that
install with their bootc image reference set to `localhost/hummingbird-<flavor>:latest`.
That reference doesn't resolve from inside the VM, so the auto-update
timer fires on schedule but has nothing to pull — every GHCR release sits
on the registry while the VM stays frozen at whatever was last built
locally (#138).

To make auto-updates actually work, the VM has to track a remote ref. The
`redo-*.sh` deploy scripts now run `scripts/switch-to-ghcr.sh` against each
freshly-installed VM as their last step, which does:

```bash
bootc switch ghcr.io/aatchison/hummingbird-<flavor>:latest
```

After that, the next `bootc upgrade` (manual or via the timer) pulls from
GHCR. To verify on any node:

```bash
sudo bootc status --json | jq .status.booted.image.image.image
# → "ghcr.io/aatchison/hummingbird-k3s:latest"   (good)
# → "localhost/hummingbird-k3s:latest"           (bad — timer can't pull)
```

### Switching an already-deployed cluster

For a cluster deployed before this auto-switch existed, run the Makefile
target on the KVM host. It iterates every running `hummingbird-*` VM,
SSHes in, and `bootc switch`es each one. Already-correct VMs are skipped:

```bash
make switch-to-ghcr
```

The script is best-effort per VM: if `bootc switch` fails for one (e.g.
the GHCR image hasn't been published for that flavor yet, or the VM is
unreachable), the others still get switched and the failure is logged.

### Opting out

Set `BOOTC_SWITCH_TO_GHCR=0` in the environment when running `redo-*.sh`
(or `make switch-to-ghcr`) to skip the post-install switch — appropriate
for offline labs where the VM intentionally tracks the local build.

Each booted VM with the timer enabled will, without operator intervention:

1. Wake up on the timer (default cadence: roughly once a day, with a small
   randomized delay built into the upstream unit).
2. Fetch the current image manifest from the registry the VM was installed
   from (or whatever it was last switched to via `bootc switch`).
3. Stage any new layers into a pending deployment (`bootc upgrade`).
4. Reboot to finalize the new deployment.

There is no draining, no cordoning, and no coordination between nodes — the
timer only knows about the local host. See "operational caveats" below.

## Verifying the timer is active

On any node:

```bash
systemctl is-active bootc-fetch-apply-updates.timer    # → active
systemctl list-timers 'bootc-*'                        # shows next + last run
```

The unit ships in the base `quay.io/hummingbird-community/bootc-os` image; our
Containerfiles enable it by dropping a low-numbered systemd preset file
(`/usr/lib/systemd/system-preset/10-*.preset` containing
`enable bootc-fetch-apply-updates.timer`) and running `systemctl preset` at
build time. A plain `ln -sf … timers.target.wants/` doesn't survive: Hummingbird
also ships `99-default-disable.preset`, which strips unenabled symlinks during
bib's qcow2 generation. Lower-numbered preset files win.

## Disabling on a single host

Per-host opt-out (survives reboots; takes effect immediately):

```bash
sudo systemctl disable --now bootc-fetch-apply-updates.timer
```

To re-enable later:

```bash
sudo systemctl enable --now bootc-fetch-apply-updates.timer
```

## Disabling fleet-wide

If you want the entire flavor to ship with the timer off, drop
`enable bootc-fetch-apply-updates.timer` from the corresponding `10-*.preset`
file (and the matching token from the `systemctl preset` line) in that
flavor's Containerfile and rebuild.

## Operational caveats

- **Control planes reboot without draining themselves.** When a CP picks up an
  update the timer triggers `systemctl reboot`. Workloads that depend on the
  apiserver will see a brief blip while the CP is down. On a single-CP cluster
  (everything we currently ship), this is a full apiserver outage of however
  long it takes that VM to reboot.
- **No coordination between nodes.** Workers reboot independently. If two
  workers happen to upgrade in the same window, you can lose multiple replicas
  at once. The randomized delay in the upstream timer (~30 min jitter)
  mitigates this for a small fleet but is not a guarantee.
- **No rollback on failure.** If the new image fails to boot, the VM may need
  manual intervention (`bootc rollback` from a serial console / rescue boot).

### Recommendation for production control planes

The CP image ships with the timer **disabled** for exactly this reason:
on a single-CP cluster, an unattended reboot is a full apiserver outage.
For production CPs, leave the timer off and drive upgrades manually at a
maintenance window:

```bash
sudo bootc upgrade --check       # ad-hoc: see what's available
sudo bootc upgrade && sudo reboot   # apply at your chosen maintenance window
```

To opt in to auto-update on a lab CP, enable the timer per host:

```bash
sudo systemctl enable --now bootc-fetch-apply-updates.timer
```

Workers are generally safer to leave on auto-update once you have more than one
of them, since the kubelet will reschedule pods elsewhere — but staggering is
still your responsibility.

### Important: per-host disable does NOT survive bootc upgrade

If you run `sudo systemctl disable bootc-fetch-apply-updates.timer` on a
deployed VM, then a later `bootc upgrade` rolls in a new image, the preset
in the new image re-enables the timer. `systemctl preset` is re-applied
against units in the new image layer, and the unit will end up enabled
again whether or not the prior host had disabled it.

Workarounds:

1. **Mask the unit** — preset cannot override a masked unit, so this
   sticks across `bootc upgrade`:

   ```bash
   sudo systemctl mask bootc-fetch-apply-updates.timer
   ```

2. **Rebuild the image with the preset entry removed** — drop
   `enable bootc-fetch-apply-updates.timer` from the corresponding
   `10-*.preset` and from the matching `systemctl preset ...` line in
   that flavor's Containerfile. The control-plane image already ships
   this way (see #48).

## Rolling back

If a bad image lands and the VM still boots:

```bash
sudo bootc rollback
sudo reboot
```

If the VM doesn't boot at all, hold shift / select the previous entry from the
boot menu (bootc keeps the prior deployment around) and once back in the
working deployment, `bootc rollback` to make it sticky.
