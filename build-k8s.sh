#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
# shellcheck source=lib/build-common.sh
source lib/build-common.sh

require_root

LOCAL_IMAGE=localhost/hummingbird-k8s:latest
NAME=hummingbird-k8s

: "${APISERVER_EXTRA_SANS:=127.0.0.1,localhost}"

render_bib_config > bib-config.toml

podman pull "$BASE_IMAGE"
podman build \
  --build-arg "APISERVER_EXTRA_SANS=${APISERVER_EXTRA_SANS}" \
  -t "$LOCAL_IMAGE" -f Containerfile.k8s .

build_qcow2 "$LOCAL_IMAGE" "$NAME" "$(pwd)/bib-config.toml"
