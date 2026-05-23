#!/usr/bin/env bash
# Shared SSH/virsh/log helpers live in lib/build-common.sh; see docs/development.md.
set -euo pipefail
# Run from the repo root so the build context (and `lib/`, `containers/`
# paths) resolves consistently regardless of where the operator invokes us.
cd "$(dirname "$(readlink -f "$0")")/.."
# Opt into autoloading config.local.sh — build-* scripts have always
# relied on its env-var side-effects (POOL_DIR, SSH_PUBKEY_FILES, etc).
export HBIRD_AUTOLOAD_CONFIG_LOCAL=1
# shellcheck source=../lib/build-common.sh
source lib/build-common.sh

require_root

LOCAL_IMAGE=localhost/hummingbird-k3s:latest
NAME=hummingbird-k3s

render_bib_config > bib-config.toml

# Isolation contract (issue #199): when STORAGE_DRIVER / PODMAN_ROOT /
# PODMAN_RUNROOT are set, the outer pull/build MUST land in the same
# isolated graphroot that build_qcow2's `--root` will look in below —
# otherwise BIB's `--local` lookup fails image-not-found.
mapfile -t _PODMAN_OPTS < <(podman_storage_opts)
podman "${_PODMAN_OPTS[@]}" pull "$BASE_IMAGE"
podman "${_PODMAN_OPTS[@]}" build \
  --build-arg "ENABLE_CLOUD_INIT=${ENABLE_CLOUD_INIT}" \
  -t "$LOCAL_IMAGE" -f containers/k3s/Containerfile .

build_qcow2 "$LOCAL_IMAGE" "$NAME" "$(pwd)/bib-config.toml"
