# Development notes

This file collects developer-facing conventions that don't fit cleanly under
any single feature doc. Operator-facing docs live alongside it
(`docs/deploy-cluster.md`, `docs/ssh-keys.md`, etc).

## Shared shell helpers (`lib/build-common.sh`)

`lib/build-common.sh` is sourced by every build script and by most of the
orchestrator scripts under `scripts/`. It exposes two families of helpers:

1. **Build-only helpers** — `ssh_pubkey_blob`, `render_bib_config`,
   `build_qcow2`, etc. Used by `containers/*/build-*.sh`. Originated in
   issue #98 / #106 / #115.
2. **Shared SSH / virsh / log helpers** — extracted in PR #202 (issue #190)
   so the cluster-orchestration scripts under `scripts/` don't have to
   reimplement the same primitives five times.

### Shared SSH / virsh / log primitives (issue #190)

Five scripts (`deploy-cluster.sh`, `destroy-cluster.sh`,
`update-cluster.sh`, `export-argocd.sh`, `verify-hardening.sh`) historically
hand-rolled the same SSH option array, `virsh domifaddr` IP resolution, and
`log()` / `fail()` primitives. They are now centralized in
`lib/build-common.sh`:

| Helper                       | Contract                                                                                                        |
|------------------------------|-----------------------------------------------------------------------------------------------------------------|
| `setup_logging "<prefix>"`   | Define `log()` / `fail()` in the caller's scope. `log` prints `<prefix> <msg>` to stderr; `fail` adds `ERROR:` and exits 1. |
| `ssh_opts_array OUT [--with-controlmaster] [--proxy-jump=HOST]` | Populate `OUT` with the canonical hardened SSH option set. Requires `SSH_PRIVKEY_FILE` to be set. |
| `ssh_opts_array_no_identity OUT [flags]` | Same as above but omits `-i SSH_PRIVKEY_FILE`. Used by callers that rely on agent auth or `~/.ssh/config`. |
| `resolve_vm_ip <vm> [attempts] [interval]` | Echo first IPv4 from `virsh -c qemu:///system domifaddr <vm>`. Defaults to 1 attempt, 0s interval — pass larger values for the DHCP-not-yet-ready boot-wait case. Probes virsh up front; warns when the VM exposes multiple IPv4 addresses (first is returned). |
| `derive_ssh_privkey_file <pub>` | Echo `${pub%.pub}` and verify it's readable. Hard-fails (rc=2) if `<pub>` does not end in `.pub` (so typos / already-private paths are rejected loudly instead of silently feeding ssh a wrong identity). |

#### Adding a new SSH-driven script

The canonical pattern, mirroring `scripts/export-argocd.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=../lib/build-common.sh
source "$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)/lib/build-common.sh"
setup_logging "[my-script]"

# … config-loading, validation …

# shellcheck disable=SC2034  # consumed by ssh_opts_array
SSH_PRIVKEY_FILE="$(derive_ssh_privkey_file "$SSH_PUBKEY_FILE")" \
  || fail "SSH private key not readable next to $SSH_PUBKEY_FILE"
ssh_opts_array CP_SSH_OPTS

CP_IP="$(resolve_vm_ip "$CP_NAME")" \
  || fail "could not resolve CP_IP for $CP_NAME"

cp_ssh() { ssh "${CP_SSH_OPTS[@]}" "root@${CP_IP}" "$@"; }

cp_ssh "uname -a"
```

#### Why each opt is in `ssh_opts_array`

* `-i SSH_PRIVKEY_FILE` — explicit identity. Nested `sudo` (e.g.
  `sudo make → sudo bash`) resets `SUDO_USER`, so we cannot rely on
  the calling user's agent or `~/.ssh/id_ed25519`. Pinning the identity
  to the key paired with `SSH_PUBKEY_FILE` (the same key bib bakes into
  root's `authorized_keys`) is the only reliable path.
* `StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null` — the
  cluster VMs come and go on DHCP-assigned IPs; refusing to connect on
  host-key mismatch would block every redeploy.
* `LogLevel=ERROR` — suppress the "Warning: Permanently added …" line
  that `UserKnownHostsFile=/dev/null` triggers on every connection.
* `ConnectTimeout=10` — bound the wait when a VM is unreachable so
  retry loops can make progress instead of hanging on TCP backoff.
* `BatchMode=yes` — refuse to prompt for password / passphrase. SSH
  must succeed via the explicit `-i` or fail fast.
* `--with-controlmaster` (multiplexing) — `update-cluster.sh` makes
  ~6-12 ssh calls per node; the multiplex socket amortizes handshake
  cost from ~200ms to ~5ms after the first connection. `update-cluster`
  also calls `ssh -O exit` on EXIT to close the sockets cleanly.

### `HBIRD_AUTOLOAD_CONFIG_LOCAL`

`lib/build-common.sh` can optionally source a repo-local `config.local.sh`
(image-build inputs — `SSH_PUBKEY_FILES`, `POOL_DIR`, resource knobs,
etc.). The autoload is gated behind `HBIRD_AUTOLOAD_CONFIG_LOCAL=1`,
which the calling script must `export` **before** sourcing the lib:

```bash
export HBIRD_AUTOLOAD_CONFIG_LOCAL=1
source lib/build-common.sh
```

The build paths (`scripts/build-k3s.sh`, `scripts/build-k8s.sh`,
`scripts/build-worker.sh`, `scripts/spawn-workers.sh`) opt in. The
cluster-orchestrator scripts (`deploy-cluster.sh`, `destroy-cluster.sh`,
`update-cluster.sh`, `export-argocd.sh`, `verify-hardening.sh`) do
NOT — they take their config from an explicit `CONFIG=<path>` argument
and have no reason to inherit arbitrary side-effects from
`config.local.sh`.

### Adding a new shared helper

Helpers live in `lib/build-common.sh`. When adding one:

1. Define the function with a short docstring above it (contract,
   inputs, outputs, error modes).
2. Add at least two bats cases in `tests/lib/build-common.bats` — one
   happy-path and one error-path — per the two-test rule below.
3. If multiple existing scripts can now use it, wire them through in
   the same PR (dead helpers tend to drift out of sync with callers).
4. Cross-reference back from the consumer script's header
   (`# Shared SSH/virsh/log helpers in lib/build-common.sh; see
   docs/development.md.`).

### Podman storage isolation (issue #199)

`build_qcow2()` reads three optional env vars and threads them through to
the `podman` invocation AND into the `bootc-image-builder` (BIB) container
that podman launches:

| Var | Maps to |
|---|---|
| `STORAGE_DRIVER` | `podman --storage-driver <v>` (e.g. `vfs`) |
| `PODMAN_ROOT`    | `podman --root <path>` (graphroot) |
| `PODMAN_RUNROOT` | `podman --runroot <path>` |

The three vars are also forwarded into the BIB container via `-e` so the
nested podman that BIB spawns uses the same isolated graphroot. The
`/var/lib/containers/storage` bind-mount is automatically repointed to
`$PODMAN_ROOT` when set, so BIB sees the image graph the outer podman
just populated.

Contract: callers that need isolated storage (e.g. two concurrent
integration runs on the same host) set the env vars before invoking
`build_qcow2`; the rest of the build path stays unchanged. When the vars
are unset, `build_qcow2` falls back to the legacy
`/var/lib/containers/storage` host path and emits no extra podman flags
— byte-identical to pre-#199 behavior on developer workstations.

This is the mechanism the integration workflows (`integration-update-
cluster.yml`, `integration-export-argocd.yml`) use to keep concurrent
self-hosted runs from corrupting each other's overlay graph.

### Bats tests

Unit tests for `lib/build-common.sh` live in `tests/lib/build-common.bats`.
Run them with:

```bash
make test-lib
```

This shells out to a pinned bats image; you do not need bats on the host.
When you add a new helper to `lib/build-common.sh`, add at least two test
cases:

1. The happy path (helper returns the expected value).
2. The error path (helper fails loudly with a clear diagnostic).

The PR-validation workflow (`.github/workflows/pr-validate.yml`) runs the
same `make test-lib` invocation, so a green local run is a strong signal.
