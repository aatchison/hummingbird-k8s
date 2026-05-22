# SSH authorized_keys sources

The Hummingbird build embeds an authorized_keys blob into each VM image at
bake time (`lib/build-common.sh`). There is no DNS-driven or runtime-updated
key source — what's baked into the qcow2 is what the VM accepts.

Two sources are supported, and they compose:

## 1. Local pubkey files (default)

`SSH_PUBKEY_FILES` is a colon-separated list of paths. If unset, the build
defaults to `~/.ssh/id_ed25519.pub` of the invoking `SUDO_USER`. Each file
must be readable; an unreadable path fails the build.

```sh
export SSH_PUBKEY_FILES=~/.ssh/id_ed25519.pub:~/.ssh/id_rsa.pub
```

Setting `SSH_PUBKEY_FILES=` (empty) disables file-based sourcing entirely —
useful when you only want GitHub-published keys.

## 2. GitHub user profiles (opt-in)

`SSH_PUBKEY_GH_USERS` is a comma-separated list of GitHub usernames. For each,
the build fetches `https://github.com/<user>.keys` (the public endpoint that
serves a user's published SSH keys) and appends the result to the embedded
authorized_keys.

```sh
export SSH_PUBKEY_GH_USERS=aatchison,teammate
```

`curl -fsSL` is used. A 404 (nonexistent username, typo) **fails the build
loudly** — that's preferable to silently embedding the wrong set of keys.

## Composition + dedup

If both sources are set, file-based keys come first and GitHub keys are
appended. Whole-line duplicates are removed (first-seen order preserved); the
build uses `awk '!NF || !seen[$0]++'` rather than `sort -u`, so ordering stays
stable across runs and diffs.

Caveat: GitHub strips the comment field (`user@host`) from the `.keys`
endpoint, but local pubkey files typically include it. A file-source key and
its GitHub-source twin will therefore have different trailing bytes and will
**not** dedupe against each other. In practice this just means the same key
material appears twice in `authorized_keys`, which SSH handles correctly.

## Trust model

GitHub-published keys are the user's published intent — the same trust model
as Ubuntu's `ssh-import-id gh:<user>` or `cloud-init`'s `ssh_import_id`.
You're saying "I trust whatever pubkey this GitHub user has uploaded, and
I'm willing to re-bake the image when they rotate."

Concretely: the keys are fetched **once** at build time over TLS to
`github.com`. The VMs themselves never call out to GitHub. If a user adds
or removes a key on GitHub after the image is built, the change does not
propagate — rebuild and redeploy to pick it up.

This is intentional. Out-of-scope (deliberately):

- Fetching from arbitrary key servers / non-github.com hosts.
- Runtime/periodic key refresh inside the VM.
- GitHub *org* or *team* membership as a key source (would require an API
  token; needs a separate design).
