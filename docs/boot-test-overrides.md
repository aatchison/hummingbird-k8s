# Boot-test override knobs

One operator reference for the environment knobs that control **which image a deploy
actually boots**. They exist because the fast default path is built to *reuse* work —
a previously-pulled GHCR image and a previously-built qcow2 — and that reuse can silently
defeat a boot-test of a local `Containerfile` (or `scripts/` / `lib/`) change: the VM boots
the old image and the test "passes" against code that never ran.

A boot-test is the only gate that catches first-boot bugs (`k8s-init.sh`, `worker-init.sh`,
kubeadm/k3s config) that `pr-validate` (build-only) cannot. To trust its result you have to
force the deploy off the cached/published path and onto your working tree.

The knobs below are read by `scripts/deploy-cluster.sh`, `scripts/spawn-workers.sh`, and
`scripts/switch-to-ghcr.sh` (cache freshness lives in `lib/cache-utils.sh`). They are
environment-overridable and forwarded across the `KVM_HOST=` re-exec via the
`scripts/lib/ssh-wrap.sh` allowlist. CLI env wins over sourced config (#377/#381).

See [deploy-cluster.md](deploy-cluster.md) for the full config surface and
[auto-updates.md](auto-updates.md) for the image-side auto-update contract.

## The knobs

### `IMAGE_SOURCE`

- **What it does:** selects where the node image comes from — `ghcr` pulls the published
  image from the registry (the golden path); `local` builds the qcow2 from this repo via
  `bootc-image-builder`.
- **Default:** `ghcr`
- **Flip it for a boot-test:** set `IMAGE_SOURCE=local` so the VM boots an image built from
  your working tree instead of a published one. There is no local-rebuild path under
  `ghcr` — a Containerfile change can only be exercised on the `local` source.

### `FORCE_REBUILD`

- **What it does:** opts out of `build_qcow2`'s skip-if-exists shortcut and forces a fresh
  `bib` invocation even when a qcow2 already exists in `$POOL_DIR` (#311). It also gates the
  post-spawn / single-VM GHCR switch: in `switch-to-ghcr.sh` and `spawn-workers.sh`,
  `FORCE_REBUILD=1` **skips** re-pointing the freshly-built VM at GHCR so it keeps tracking
  the local image you're testing (#375/#382) — unless `FORCE_SWITCH=1` overrides (below).
- **Default:** off (unset / `0`)
- **Flip it for a boot-test:** set `FORCE_REBUILD=1` (with `IMAGE_SOURCE=local`) so your
  `Containerfile` change is actually compiled into the image under test rather than masked by
  a cached qcow2. `bib` needs rootful podman, so this path requires sudo / root on the KVM host.

### `STRICT_CACHE`

- **What it does:** makes the qcow2/image freshness check fail-closed. The deploy records a
  build-identity sidecar per template (`<template>.qcow2.build-ref`, `lib/cache-utils.sh`)
  and acts only on a *confirmed same-source mismatch*: under `IMAGE_SOURCE=local` a mismatch
  auto-rebuilds with a `WARN`; `STRICT_CACHE=1` turns that confirmed-stale `WARN` into a hard
  failure instead (mirrors `HBIRD_REMOTE_STRICT`). An *unverifiable* identity (e.g. a published
  image with no revision label) is reused silently and never fails.
- **Default:** `0`
- **Status:** **live** — merged via #373/#384.
- **Flip it for a boot-test:** set `STRICT_CACHE=1` as the fail-closed seatbelt a CI / boot-test
  gate wants — a confirmed-stale template aborts the deploy instead of quietly booting old bits.

### `SWITCH_TO_GHCR`

- **What it does:** when `true`, `deploy-cluster.sh` emits a first-boot
  `bootc switch ghcr.io/...:$GHCR_TAG` runcmd so each node leaves its install-time image and
  starts tracking the GHCR stream. From there the semver update timer
  (`bootc-semver-update.timer`, which resolves the highest published `vX.Y.Z`) takes over.
- **Default:** `true`
- **Flip it for a boot-test:** set `SWITCH_TO_GHCR=false` when boot-testing a `local` image so
  the freshly-deployed node stays on `localhost/hummingbird-*` instead of being switched back
  to the published stream — otherwise the switch undoes the point of booting your local build.

### `FORCE_SWITCH`

- **What it does:** overrides the `FORCE_REBUILD` skip in `switch-to-ghcr.sh`'s single-VM /
  post-spawn path. Normally `FORCE_REBUILD=1` skips the GHCR switch (to preserve the local
  image being tested); `FORCE_SWITCH=1` forces the switch to happen anyway. Allowlisted for
  `KVM_HOST=` forwarding by #382/#383.
- **Default:** off (unset / `0`)
- **Flip it for a boot-test:** usually **leave it off** — its whole job is to *defeat* the
  local-image preservation that `FORCE_REBUILD=1` gives you. Set it only when you've rebuilt
  locally but deliberately want the node handed to the GHCR stream anyway.

### `BOOTC_SWITCH_TO_GHCR`

- **What it does:** a hard escape hatch inside `scripts/switch-to-ghcr.sh` itself.
  `BOOTC_SWITCH_TO_GHCR=0` short-circuits the script at the top, so it switches nothing —
  applies to both `make switch-to-ghcr` (which walks every `hummingbird-*` domain) and the
  direct `switch-to-ghcr.sh <vm> <ref>` calls made by `deploy-cluster.sh` / `spawn-workers.sh`.
- **Default:** `1` (switch enabled)
- **Flip it for a boot-test:** set `BOOTC_SWITCH_TO_GHCR=0` for a belt-and-braces guarantee
  that *no* code path re-points your node at GHCR during the test, regardless of caller. It's
  broader than `SWITCH_TO_GHCR=false` (which only governs the deploy-time runcmd).

## Recipe: testing a Containerfile change

Build from your working tree, fail closed on a stale cache, and keep the node on the local
image (do **not** let any path switch it to GHCR):

```bash
IMAGE_SOURCE=local FORCE_REBUILD=1 STRICT_CACHE=1 SWITCH_TO_GHCR=false \
  make deploy-cluster CONFIG=cluster.local.conf
```

Notes:

- `IMAGE_SOURCE=local` + `FORCE_REBUILD=1` is the pair that actually compiles your
  `Containerfile` change into the qcow2 under test. (`bib` needs rootful podman — run with
  sudo / root on the KVM host.)
- `STRICT_CACHE=1` is the seatbelt: a confirmed-stale template aborts the deploy rather than
  quietly booting old bits.
- `SWITCH_TO_GHCR=false` keeps the node on `localhost/hummingbird-*`. Under `FORCE_REBUILD=1`
  the post-spawn switch is already skipped (#375) unless `FORCE_SWITCH=1`; add
  `BOOTC_SWITCH_TO_GHCR=0` if you want a hard guarantee no caller switches it.
- Once your change has merged and a `vX.Y.Z` tag is published, hand nodes back to auto-updates
  by deploying with the defaults (`SWITCH_TO_GHCR=true`); the `bootc-semver-update.timer` then
  tracks the highest published `vX.Y.Z`. See [auto-updates.md](auto-updates.md).

## Related

- [deploy-cluster.md](deploy-cluster.md) — full config surface + the `FORCE_SWITCH` note
- [auto-updates.md](auto-updates.md) — semver-update timer, `BOOTC_SWITCH_TO_GHCR` escape hatch
