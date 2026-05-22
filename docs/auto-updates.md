# Automatic bootc updates

All three hummingbird-k8s flavors (`hummingbird-k3s`, `hummingbird-k8s` control
plane, `hummingbird-k8s-worker`) ship with `bootc-fetch-apply-updates.timer`
enabled by default. Each booted VM will, without operator intervention:

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

For anything beyond demo / lab use, disable the timer on the CP and drive
upgrades manually:

```bash
sudo systemctl disable --now bootc-fetch-apply-updates.timer
sudo bootc upgrade --check       # ad-hoc: see what's available
sudo bootc upgrade && sudo reboot   # apply at your chosen maintenance window
```

Workers are generally safer to leave on auto-update once you have more than one
of them, since the kubelet will reschedule pods elsewhere — but staggering is
still your responsibility.

## Rolling back

If a bad image lands and the VM still boots:

```bash
sudo bootc rollback
sudo reboot
```

If the VM doesn't boot at all, hold shift / select the previous entry from the
boot menu (bootc keeps the prior deployment around) and once back in the
working deployment, `bootc rollback` to make it sticky.
