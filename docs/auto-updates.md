# Automatic bootc updates (semver-aware)

Every Hummingbird flavor (`k8s`, `k8s-worker`) ships with a custom
`bootc-semver-update.timer` enabled by default. The timer fires daily,
resolves the highest semver-tagged image at the flavor's GHCR repo, and
`bootc switch`es to it if it's newer than what's currently booted. The
stock upstream `bootc-fetch-apply-updates.timer` is **disabled** in the
shipped preset (but still present on the image — operators can re-enable
it per host if they want the legacy `:latest` behaviour).

The `hummingbird-k8s-worker` flavor enables auto-updates by default. The
`hummingbird-k8s` control-plane image ships with the timer **disabled by
default** — opt in per host via `systemctl enable --now` (see below).
The rationale: a single-CP cluster suffers a full apiserver outage
during the CP reboot, so auto-update on the CP needs to be an explicit
operator choice (#48).

## Why semver instead of `:latest`

`:latest` is a mutable pointer. A bad push to `:latest` rolls every VM in
the fleet forward to the broken image on the next timer wake — there is
no per-tag immutability and no operator gate between "image built" and
"fleet upgrades". On a single-control-plane lab cluster, that means the
apiserver can go down across every node at once for a single bad rebuild.

The semver-aware path only advances when a **new immutable tag** (e.g.
`v0.4.2`) is published. To pause the fleet, simply don't tag a release.
To roll a fix, tag `v0.4.3`. The timer picks it up on its next wake.

## Discovery mechanism

```bash
skopeo list-tags "docker://${REPO}" \
  | jq -r '.Tags[]' \
  | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' \
  | sort -V \
  | tail -1
```

- `skopeo list-tags` enumerates every tag visible to anonymous pulls.
  GHCR exposes this even for public repos without auth.
- `jq -r '.Tags[]'` flattens the JSON envelope.
- The grep filters out non-semver tags (`:latest`, branch refs,
  short SHAs, alpha/RC tags like `v0.4.0-rc1`). Only strict `vMAJOR.MINOR.PATCH`
  is matched — pre-release tags are intentionally excluded.
- `sort -V` (GNU sort, version-aware) picks the highest.

The script then compares against
`bootc status --json | jq -r '.status.booted.image.image.image'`. If the
target equals the current ref, the unit logs and exits 0. Otherwise it
`bootc switch`es and `bootc upgrade`s — the new deployment is staged and
swaps in at next reboot (which the unit does NOT trigger; the existing
post-upgrade reboot path on `bootc-fetch-apply-updates.service` is what
historically drove the reboot, and that path is out of scope for this
unit). Operators can drive reboots via the existing health-check timer
or manually at a maintenance window.

After a `bootc switch` + `bootc upgrade`, the service touches
`/var/lib/bootc-just-upgraded` (same marker pattern as the legacy
`bootc-fetch-apply-updates.service.d/10-mark-upgrade.conf` drop-in) so
the post-boot health-check-rollback timer fires after the next reboot
and can auto-roll-back if the cluster comes up unhealthy. See
`docs/rollback.md`.

### k3s flavor (deprecated)

The `ghcr.io/aatchison/hummingbird-k3s` and
`ghcr.io/aatchison/hummingbird-k3s-worker` packages are **frozen** as of
PR #216 and tagged `:deprecated` (see PR #219 +
[`docs/k3s-ghcr-deprecation.md`](k3s-ghcr-deprecation.md)). The semver
timer on a k3s VM will keep resolving the highest semver tag already
pushed — but no new tags will be cut, so the timer effectively no-ops
forever. Operators still running k3s VMs have two reasonable paths
forward:

- **Pin and stop tracking.** Disable the timer
  (`systemctl disable --now bootc-semver-update.timer`) and either
  `bootc switch ghcr.io/aatchison/hummingbird-k3s:deprecated` to
  pin the final image explicitly, or leave the VM on its existing
  semver pin. Either way, no further updates flow.
- **Migrate to `hummingbird-k8s`.** `bootc switch` to
  `ghcr.io/aatchison/hummingbird-k8s` and re-deploy the cluster via
  `make deploy-cluster` so the upstream-kubeadm CP/worker layout
  takes over. This is the recommended path.

## Configuration

Each image bakes a flavor-specific default into `/etc/hummingbird/bootc-update.env`:

```ini
# hummingbird-k8s
REPO=ghcr.io/aatchison/hummingbird-k8s
PREFIX=v

# hummingbird-k8s-worker
REPO=ghcr.io/aatchison/hummingbird-k8s-worker
PREFIX=v
```

The hardcoded `aatchison/` namespace is the canonical Hummingbird publish.
Operators forking the repo (publishing their own builds to a different
GHCR namespace, Quay, etc.) override by editing the file directly or
injecting it via cloud-init `write_files`:

```yaml
#cloud-config
write_files:
  - path: /etc/hummingbird/bootc-update.env
    content: |
      REPO=ghcr.io/your-org/your-flavor
      PREFIX=v
    owner: root:root
    permissions: '0644'
```

`PREFIX` defaults to `v` (matching the GitHub-conventional `v0.4.2` tag
style). Set to empty for unprefixed semver:

```ini
REPO=ghcr.io/your-org/your-flavor
PREFIX=
```

## Schedule customization

The timer ships with:

```ini
[Timer]
OnBootSec=15min
OnCalendar=daily
RandomizedDelaySec=30min
Persistent=true
```

To customize per-host, drop a systemd override at
`/etc/systemd/system/bootc-semver-update.timer.d/schedule.conf`. The
empty `OnCalendar=` line is required — without it the default daily
schedule stays in effect and your override is additive:

```ini
[Timer]
OnCalendar=
OnCalendar=Mon *-*-* 04:00:00
```

After dropping the file:

```bash
sudo systemctl daemon-reload
sudo systemctl restart bootc-semver-update.timer
```

To disable per-host (survives `bootc upgrade` only if you mask):

```bash
sudo systemctl disable --now bootc-semver-update.timer
# or, to survive image upgrades:
sudo systemctl mask bootc-semver-update.timer
```

## Verifying

On any node:

```bash
systemctl is-active bootc-semver-update.timer       # → active
systemctl status bootc-semver-update.timer          # full state + last trigger
systemctl status bootc-semver-update.service        # service unit details
systemctl list-timers bootc-semver-update.timer     # next + last run
journalctl -t bootc-semver-update --since today     # latest run output
```

`systemctl status` on both the `.timer` and `.service` is the canonical
"what's the timer doing right now" diagnostic — `is-active` only tells
you the timer is loaded, not whether the most recent service run
succeeded. `status` on the service unit surfaces the last
ExecMainStatus rc and the most recent journal lines, which is what an
operator wants when they suspect the daily run silently failed.

To dry-run the discovery without invoking `bootc switch`:

```bash
sudo source /etc/hummingbird/bootc-update.env
skopeo list-tags "docker://${REPO}" \
  | jq -r '.Tags[]' \
  | grep -E "^${PREFIX-v}[0-9]+\.[0-9]+\.[0-9]+$" \
  | sort -V \
  | tail -1
```

## Diff vs the old `bootc-fetch-apply-updates.timer`

| Behaviour | bootc-fetch-apply-updates (stock) | bootc-semver-update (Hummingbird) |
|---|---|---|
| Tracks | Whatever ref is currently pinned (typically `:latest`) | Highest `vMAJOR.MINOR.PATCH` tag at REPO |
| Mutable image risk | Yes — `:latest` can be silently overwritten | No — once `v0.4.2` is published it doesn't move |
| Operator gate | None — every push to `:latest` rolls the fleet | Tagging a release is the gate; don't tag = no rollout |
| Pre-release handling | Picks up `:latest` regardless | Strict semver only; `-rc1` / `-beta` are skipped |
| Cadence | Inherited from upstream timer | Daily + 30min jitter (overridable, see above) |
| Reboot on apply | Yes (the upstream service reboots) | No — stages only; existing reboot/rollback hooks fire next boot |

## Migration from the legacy `:latest` timer

VMs deployed before this change have `bootc-fetch-apply-updates.timer`
enabled at the host level (the old preset). Upgrading to a new image
that ships this PR's preset disables it via `systemctl preset` — BUT a
host-level enable wins over a preset disable on existing nodes. To
explicitly switch a deployed VM to the semver flow:

```bash
sudo systemctl disable --now bootc-fetch-apply-updates.timer
sudo systemctl enable  --now bootc-semver-update.timer
```

For a freshly-deployed VM from an image that includes this change, the
preset already does the right thing — no manual step needed.

To verify the per-flavor REPO default matches what the host should be
pulling (canonical Hummingbird publish):

```bash
cat /etc/hummingbird/bootc-update.env
# REPO=ghcr.io/aatchison/hummingbird-<flavor>
# PREFIX=v
```

## Tracking GHCR vs localhost

The local build path (`scripts/build-*.sh` → `bib`) produces qcow2s that
install with their bootc image reference set to
`localhost/hummingbird-<flavor>:latest`. That reference doesn't resolve
from inside the VM, so neither the semver timer nor the legacy timer can
pull. `scripts/deploy-cluster.sh` (and the helper
`scripts/switch-to-ghcr.sh`) runs against each freshly-installed VM as
its last step, which does:

```bash
bootc switch ghcr.io/aatchison/hummingbird-<flavor>:latest
```

After that, the semver timer's first wake-up promotes the host from
`:latest` → `:v<highest>` automatically (and from then on, only ever
advances on new semver tags). To verify on any node:

```bash
sudo bootc status --json | jq .status.booted.image.image.image
# → "ghcr.io/aatchison/hummingbird-k8s:v0.4.2"   (good — on a semver tag)
# → "ghcr.io/aatchison/hummingbird-k8s:latest"   (transitional — timer will promote)
# → "localhost/hummingbird-k8s:latest"           (bad — timer can't pull)
```

### Switching an already-deployed cluster

For a cluster deployed before the GHCR-switch existed, run the Makefile
target on the KVM host:

```bash
make switch-to-ghcr
```

The script is best-effort per VM: failures on one VM don't block the
others, and they're logged so you can retry.

### Opting out

To skip the post-install switch — appropriate for offline labs where
the VM intentionally tracks the local build — there are two knobs:

- `SWITCH_TO_GHCR=false` in `cluster.local.conf` keeps freshly-deployed
  VMs pointed at `localhost/hummingbird-*:latest` (consumed by
  `scripts/deploy-cluster.sh`; default is `true`).
- `BOOTC_SWITCH_TO_GHCR=0` in the environment short-circuits
  `scripts/switch-to-ghcr.sh` itself. It applies to either invocation
  path — `make switch-to-ghcr` (which walks every `hummingbird-*`
  domain on the host) and direct `scripts/switch-to-ghcr.sh <vm-name>
  <ref>` calls (used by `deploy-cluster.sh` / `spawn-workers.sh`
  after a fresh deploy). The check is at the top of the script
  (line 30), so it triggers regardless of caller.

## Operational caveats

- **No automatic reboot.** Unlike `bootc-fetch-apply-updates.service`,
  the semver unit only `bootc switch` + `bootc upgrade` (which *stages*
  a new deployment). The new image is not live until the next boot.
  Operators control the reboot — either through the existing
  health-check-rollback path, a maintenance-window `systemctl reboot`,
  or manual `reboot` after `bootc upgrade --check` confirms staging.
- **No coordination between nodes.** Each VM resolves the highest
  semver tag independently. If two workers wake within the same
  jitter window and you have a new tag available, they'll both stage
  the same image — but they reboot independently, so a tag with a
  boot-time regression can still take out multiple nodes if every node
  happens to reboot soon after staging. The randomized 30-min delay
  helps spread this across the fleet for a small lab; staggered
  maintenance reboots are still your responsibility for production.
- **No automatic rollback if the staged image won't boot at all.**
  If the new deployment panics or fails to mount root, the VM still
  needs manual intervention (`bootc rollback` from a serial console
  or rescue boot). The auto-rollback path in `docs/rollback.md` only
  fires *after* a successful boot when the cluster comes up unhealthy.
- **Per-host `systemctl disable` does NOT survive `bootc upgrade`.**
  The preset in the new image layer re-applies on upgrade and re-enables
  the timer. Use `systemctl mask` for a sticky disable, or rebuild the
  image with the timer dropped from the preset for a fleet-wide disable.

## Operator-driven rolling upgrades

The per-VM timer is the unattended path: each node wakes up
independently, fetches whatever its `bootc` image ref points at, and
reboots without coordinating with the rest of the cluster. That's fine
for worker fleets but uncomfortable for a single-CP cluster where you
want drain/uncordon, a defined node ordering, and a clean abort
surface.

For that, the orchestrated complement is
[`scripts/update-cluster.sh`](update-cluster.md), surfaced as
`make update-cluster`:

```bash
make update-cluster CONFIG=cluster.local.conf
```

It walks the cluster one node at a time — CP first (no drain, brief
apiserver outage), then each worker (drained → `bootc upgrade --apply`
→ uncordon). Per-node it stops the in-image
`bootc-fetch-apply-updates.timer` for the duration so the two paths
don't race, then restarts it. See
[`docs/update-cluster.md`](update-cluster.md) for the full flag and
config reference (`--workers-only`, `--node=`, `--skip-drain`,
`--dry-run`, recovery procedure).

The timer described in this document and the `make update-cluster`
script are not exclusive — most production deploys use both: the timer
for routine worker rollouts, the script for image bumps where the
operator wants to be in the loop.

## Rolling back

If a bad tag lands and the VM still boots:

```bash
sudo bootc rollback
sudo reboot
```

If the VM doesn't boot at all, hold shift / select the previous entry
from the boot menu (bootc keeps the prior deployment around) and once
back in the working deployment, `bootc rollback` to make it sticky.

For the auto-rollback behaviour that fires when a staged-and-rebooted
image brings the cluster up unhealthy, see `docs/rollback.md`.

## Re-enabling the legacy timer

The legacy unit is still on the image — only the preset has been flipped.
To opt a host back into `:latest`-tracking:

```bash
sudo systemctl disable --now bootc-semver-update.timer
sudo systemctl enable  --now bootc-fetch-apply-updates.timer
```

This is reasonable for a CI/dev VM that intentionally wants the cutting
edge. Don't do it on production hosts — that's the regression this PR
is defending against.
