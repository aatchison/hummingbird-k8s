# Operator runbook: deprecate the k3s GHCR packages

This runbook captures the **operator-run** half of the
`ghcr.io/aatchison/hummingbird-k3s` (and `hummingbird-k3s-worker`)
deprecation. The in-tree docs side landed in PR #219 +
[the README's "Deprecated images" subsection](../README.md#deprecated-images).
This file documents the registry-side closeout that the package owner
runs from their workstation. Tracked in issue #226.

## Why this is operator-run

GHCR package writes (tag pushes, package-metadata edits, archive
toggle) require write access to the package at the org level. A
PR-author subagent typically doesn't have that scope; the package
owner does. There's no CI-driven path for these steps — they're
manual, one-off operations done once at the end of the deprecation.

## Prerequisites

- `skopeo` installed locally (`dnf install skopeo` on Fedora,
  `apt install skopeo` on Debian-derived).
- A GHCR access token with `write:packages` scope on
  `aatchison/hummingbird-k3s` and `aatchison/hummingbird-k3s-worker`.
  Either a classic PAT or a fine-grained token scoped to those
  packages.
- `gh` CLI authenticated with the same identity (`gh auth status`
  should show `read:packages` at minimum; the `gh api` calls below
  use the token for package-metadata edits).

## Step 1 — log skopeo into GHCR

```bash
echo "$GHCR_TOKEN" | skopeo login --username "$GHCR_USER" --password-stdin ghcr.io
```

Verify the login persisted:

```bash
skopeo inspect docker://ghcr.io/aatchison/hummingbird-k3s:latest \
  | jq -r '.RepoTags[]' | sort -V | tail -5
```

You should see the current tag list. If skopeo returns
`unauthorized: authentication required`, the token doesn't have read
on the package — fix that before continuing.

## Step 2 — apply the `:deprecated` tag

Pick a source ref. Two reasonable choices:

- **`:latest`** — whatever the registry currently considers latest.
  This is the simplest path and matches what most consumers already
  pull by default.
- **A specific `v0.X.Y` tag** — pin the deprecation to a known-good
  release. Use this if `:latest` has drifted to something you don't
  want consumers landing on.

Then `skopeo copy` it to `:deprecated` for **both** packages:

```bash
# Control plane
skopeo copy \
  docker://ghcr.io/aatchison/hummingbird-k3s:latest \
  docker://ghcr.io/aatchison/hummingbird-k3s:deprecated

# Worker
skopeo copy \
  docker://ghcr.io/aatchison/hummingbird-k3s-worker:latest \
  docker://ghcr.io/aatchison/hummingbird-k3s-worker:deprecated
```

(If you're pinning to a specific tag, substitute `:vX.Y.Z` for
`:latest` on the left of each pair.)

Verify the new tag points at the same digest as the source:

```bash
skopeo inspect docker://ghcr.io/aatchison/hummingbird-k3s:latest \
  | jq -r '.Digest'
skopeo inspect docker://ghcr.io/aatchison/hummingbird-k3s:deprecated \
  | jq -r '.Digest'
# Both should print the same sha256:... digest.
```

Repeat the digest check for the worker package.

## Step 3 — update the GHCR package description

Both packages should have their description prefixed with a
deprecation banner that points consumers at the live successor.
Recommended text:

```
[DEPRECATED — retired in #216, see hummingbird-k8s] Original short
description here. Operators looking for the last good image: pull
:deprecated. Migration path: README → "Deprecated images".
```

You can do this two ways:

### Via the gh API (scriptable)

```bash
gh api \
  --method PATCH \
  -H "Accept: application/vnd.github+json" \
  /user/packages/container/hummingbird-k3s \
  -f description="[DEPRECATED — retired in #216, see hummingbird-k8s] …"

gh api \
  --method PATCH \
  -H "Accept: application/vnd.github+json" \
  /user/packages/container/hummingbird-k3s-worker \
  -f description="[DEPRECATED — retired in #216, see hummingbird-k8s] …"
```

If the package lives under an org rather than a user, swap
`/user/packages/...` for `/orgs/{ORG}/packages/...`.

### Via the GHCR UI

1. Open
   <https://github.com/users/aatchison/packages/container/package/hummingbird-k3s>
   (and the worker equivalent).
2. Click **Package settings** on the right rail.
3. Edit the **Description** field with the deprecation banner.
4. Save.

## Step 4 — mark the package archived

GHCR's package settings page exposes a **"Archive this package"**
checkbox (under Danger Zone, same panel as Delete). Tick it on **both**
packages. Archiving:

- Keeps all existing tags pullable (so live deploys don't break),
- Marks the package read-only in the UI (no further pushes),
- Surfaces a banner on the package page telling consumers it's
  archived.

This is the most consumer-visible signal that the package is frozen,
so don't skip it.

## Step 5 — confirm README is in sync

After the registry edits land, re-read the README's
[Deprecated images](../README.md#deprecated-images) subsection and
confirm:

- It still names both packages.
- It mentions the `:deprecated` tag.
- It links here (`docs/k3s-ghcr-deprecation.md`).

If the operator pinned `:deprecated` to a specific `vX.Y.Z` rather
than `:latest`, you may want to call out the specific version in the
README so consumers running `skopeo inspect ... :deprecated` can
sanity-check what they're getting.

## Verification

A consumer should be able to:

```bash
# See the :deprecated tag exists
skopeo list-tags docker://ghcr.io/aatchison/hummingbird-k3s \
  | jq -r '.Tags[]' | grep '^deprecated$'

# Inspect it and find the deprecation banner in package metadata
gh api /users/aatchison/packages/container/hummingbird-k3s \
  | jq -r '.description'
# → "[DEPRECATED — retired in #216, ...]"

# Confirm the archived flag (GHCR doesn't expose this via the public
# packages API today; the UI banner is the canonical signal).
```

If all three are visible, the deprecation closeout is complete and
issue #226 can be closed.

## Rollback

If the deprecation needs to be reversed (e.g. a security CVE in
hummingbird-k8s makes k3s the temporary fallback path):

- Un-archive the package via the same GHCR UI checkbox.
- Edit the description back to the original (or to a "temporarily
  un-deprecated" banner).
- Leave the `:deprecated` tag in place — it costs nothing and
  remains useful as a "last good frozen image" pointer even if the
  package starts moving again.

Reverting is a deliberate decision, not an emergency procedure;
nothing about archiving breaks running deploys, so there's no
time-pressure rollback scenario.
