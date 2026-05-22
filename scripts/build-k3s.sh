#!/usr/bin/env bash
set -euo pipefail
# Run from the repo root so the build context (and `lib/`, `containers/`
# paths) resolves consistently regardless of where the operator invokes us.
cd "$(dirname "$(readlink -f "$0")")/.."
# shellcheck source=../lib/build-common.sh
source lib/build-common.sh

require_root

LOCAL_IMAGE=localhost/hummingbird-k3s:latest
NAME=hummingbird-k3s

render_bib_config > bib-config.toml

podman pull "$BASE_IMAGE"
podman build -t "$LOCAL_IMAGE" -f containers/k3s/Containerfile .

build_qcow2 "$LOCAL_IMAGE" "$NAME" "$(pwd)/bib-config.toml"
