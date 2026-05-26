# Rollback (manual + auto)

`bootc` keeps the previous deployment around at all times, so any
`hummingbird-k8s` / `hummingbird-k8s-worker` VM can be returned to the
image it was running before its most recent upgrade with a single
command:

```bash
sudo bootc rollback
sudo systemctl reboot
```

That is the manual path. This doc covers two pieces of plumbing built on
top of it:

- **#100 — end-to-end rollback test** (`integration-bootc-rollback.yml`),
  which exercises the manual path on a real KVM VM as part of CI.
- **#104 — auto-rollback on a failed auto-upgrade** (a systemd timer +
  health-check script that calls `bootc rollback` for the operator when a
  timer-driven upgrade lands an image that doesn't bring the cluster back
  up).

## Why we have this

`bootc-fetch-apply-updates.timer` will, on hosts where it's enabled, pull
a new image and reboot the VM without operator intervention (see
`docs/auto-updates.md`). If the new image fails to bring the cluster back
up — kubelet won't start, the apiserver static pod is wedged, a binary
moved between paths — the host is now a degraded node nobody is watching.
The auto-rollback machinery in this doc is the conservative answer: if the
cluster doesn't come back within a fixed post-boot window, return to the
previous deployment automatically.

This is not a substitute for staged rollouts or for operator-supervised
upgrades. It's a "don't lose the fleet to a single bad image" backstop.

## #100 — end-to-end rollback test

The CI workflow `.github/workflows/integration-bootc-rollback.yml` and its
driver `tests/integration-bootc-rollback.sh` cover the manual rollback
path on a freshly-built VM:

1. Build a qcow2 from `from_tag` via bib, `virt-install` it, wait for
   `/var/lib/k8s-init.done` (cluster is up on `from_tag`).
2. Capture pre-upgrade `bootc status` + kubelet version.
3. `bootc switch ${to_tag}` + `bootc upgrade` + reboot. Verify the VM
   comes back on `to_tag`.
4. `bootc rollback` + reboot. Verify the VM comes back on `from_tag`,
   `/var/lib/k8s-init.done` is still present, kubelet is active, and a
   node reaches Ready within 2 minutes.

The workflow is `workflow_dispatch`-only — same rationale as
`integration-bootc-upgrade.yml` (slow; explicit operator gate). To run it:

```bash
gh workflow run integration-bootc-rollback.yml \
  -f from_tag=v0.1.11 -f to_tag=v0.1.12
```

The runner requirements (`kvm`, `libvirt`, podman, virt-install, …) are
the same as the other two integration workflows; see
`docs/integration-tests.md` and `docs/self-hosted-runner.md`.

What this test does **not** cover: a deliberately-broken `to_tag` that
should trigger the #104 auto-rollback path. That scenario needs a
purpose-built broken image (e.g. removed kubelet binary) and is currently
a manual exercise on a lab VM — see "Validating auto-rollback by hand"
below.

## #104 — auto-rollback on a failed auto-upgrade

Three pieces of plumbing, all baked into every flavor of the image:

### `health-check-rollback.timer`

Fires once per boot, 3 minutes after `multi-user.target` (configurable —
see "Tuning" below). Pulls in
`health-check-rollback.service`.

### `health-check-rollback.service`

A `Type=oneshot` unit that runs
`/usr/libexec/health-check-rollback.sh` once. Defined to run `After=`
`kubelet.service network-online.target`, so by the time the script runs,
kubelet has at least been given a chance to start.

### `/usr/libexec/health-check-rollback.sh`

The script. Logic:

1. If `/var/lib/bootc-just-upgraded` does not exist → exit 0. This is the
   most important guard: it means we never auto-rollback on a clean
   install or on an operator-driven reboot. The only way the marker gets
   created is via the drop-in described below.
2. If the marker exists, delete it immediately. A persistent failure
   should never trigger more than one rollback per upgrade event.
3. Check `systemctl is-active kubelet.service`.
4. On the CP image only (detected by the presence of
   `/etc/kubernetes/manifests/kube-apiserver.yaml`), `curl
   https://127.0.0.1:6443/livez` with a 5-second timeout.
5. If any check failed, log to journal, call `bootc rollback`, and
   `systemctl reboot`. Otherwise log success and exit 0.

### The marker drop-in

`/etc/systemd/system/bootc-fetch-apply-updates.service.d/10-mark-upgrade.conf`:

```ini
[Service]
ExecStartPost=/usr/bin/touch /var/lib/bootc-just-upgraded
```

The upstream `bootc-fetch-apply-updates.service` is the unit that stages
a new deployment and triggers a reboot when the timer fires. The drop-in
runs after the unit's main `ExecStart`, so the marker is on disk before
the reboot. After the reboot, the marker is what tells the health-check
script "you should actually run today".

This is the supported override path: drop-ins under
`/etc/systemd/system/<unit>.service.d/` don't require modifying the
upstream unit and survive package updates to it.

## How it composes

Timeline of a timer-driven auto-upgrade with auto-rollback armed:

1. `bootc-fetch-apply-updates.timer` fires on the cadence (~24h with
   jitter).
2. `bootc-fetch-apply-updates.service` runs: `bootc upgrade` stages a
   new deployment.
3. `ExecStartPost` touches `/var/lib/bootc-just-upgraded`.
4. The service triggers `systemctl reboot`.
5. VM comes back on the new deployment.
6. 3 minutes after boot, `health-check-rollback.timer` fires
   `health-check-rollback.service`, which runs the script.
7. Script sees the marker, consumes it, runs health checks.
8. **Healthy path:** script logs success, exits 0. The marker is gone;
   subsequent boots are no-ops for this script.
9. **Unhealthy path:** script calls `bootc rollback`, `systemctl reboot`.
   VM comes back on the previous deployment. Marker is gone, so the
   timer firing again does nothing.

## Tuning

The 3-minute window in `health-check-rollback.timer`'s `OnBootSec=3min`
is deliberately short. On a small lab VM with our default Cilium-based CP
image it's usually enough — kubelet comes up well under 60s and the
apiserver follows shortly after. On heavier hosts (Cilium with more
features, slow disks, larger control plane) it may be too tight. Tune it
by either:

- editing `containers/shared/health-check-rollback.timer` to change
  `OnBootSec=` and rebuilding the image, or
- shipping a drop-in on the deployed host:
  `/etc/systemd/system/health-check-rollback.timer.d/override.conf`:
  ```ini
  [Timer]
  OnBootSec=
  OnBootSec=10min
  ```

## Disabling on a single host

The timer is enabled by default on all three flavors. Operators who
prefer to keep auto-rollback off (e.g. because they're hand-tuning health
during an upgrade window) can disable it per host:

```bash
sudo systemctl disable --now health-check-rollback.timer
```

Note the caveat from `docs/auto-updates.md`: per-host `disable` does NOT
survive a `bootc upgrade`, because the preset in the new image re-enables
the unit. To keep it off across upgrades, `systemctl mask` it instead:

```bash
sudo systemctl mask health-check-rollback.timer
```

## Disabling fleet-wide

Drop `enable health-check-rollback.timer` from the per-flavor preset file
(`containers/k8s/10-k8s.preset`, `containers/k8s-worker/10-k8s-worker.preset`)
and rebuild that flavor.

## Validating auto-rollback by hand

To prove the auto-rollback path on a lab VM (this is currently a manual
exercise, not a CI test):

1. Boot a CP VM on a known-good tag, wait for it to be Ready.
2. Build a deliberately-broken image (e.g. add `RUN rm -f
   /usr/bin/kubelet` to the Containerfile), push it to GHCR as
   `broken-test`.
3. `bootc switch ghcr.io/aatchison/hummingbird-k8s:broken-test`.
4. `touch /var/lib/bootc-just-upgraded` (simulate the marker the
   auto-update path would normally set — this is the only way to invoke
   the rollback logic without actually running the timer).
5. `systemctl reboot`.
6. Wait 5 minutes. The VM should reboot itself a second time and come
   back on the previous (good) tag. `journalctl -t
   health-check-rollback` will show the verdict.

## Caveats

- **Not a CI gate yet.** The auto-rollback flow does not have a CI test
  that builds a known-bad image and exercises the timer path end to end.
  The integration-bootc-rollback.yml workflow only validates the manual
  rollback command. A follow-up should add a "rollback on broken image"
  workflow that uses a purpose-built broken qcow2.
- **etcd state is not rolled back.** `bootc rollback` restores the
  filesystem deployment; it does not roll back the cluster state stored
  in etcd. See `docs/cilium-migration.md`'s "Rollback limitations"
  section for the implications.
- **3 minutes may be too short for some workloads.** See "Tuning"
  above.
- **Worker rollback can lose pod schedule decisions.** A worker that
  auto-rolls back will look to the apiserver like a node that bounced
  twice in quick succession. The kubelet will re-register; pods may be
  rescheduled depending on tolerations.
- **The marker is conservative on purpose.** It does NOT survive across
  the bootc commit. If a future change to the upstream
  `bootc-fetch-apply-updates.service` removes the unit or changes how it
  triggers reboot, the marker drop-in may stop applying — and the
  health-check timer will silently become a no-op on every boot (because
  the marker file will never appear). Re-validate the drop-in after each
  bootc base-image bump.

## Cross-references

- `docs/auto-updates.md` — `bootc-fetch-apply-updates.timer` setup and
  per-host opt-out / mask guidance.
- `docs/cilium-migration.md` (section "Rollback limitations") — etcd
  state implications when rolling back across the CNI swap.
- `docs/integration-tests.md` — full list of integration workflows and
  the self-hosted runner requirements they share.
