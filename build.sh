#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
# shellcheck source=lib/build-common.sh
source lib/build-common.sh

require_root

LOCAL_IMAGE=localhost/hummingbird-k3s:latest
NAME=hummingbird-k3s

render_bib_config > bib-config.toml

podman pull "$BASE_IMAGE"
podman build -t "$LOCAL_IMAGE" -f Containerfile .

build_qcow2 "$LOCAL_IMAGE" "$NAME" "$(pwd)/bib-config.toml"
