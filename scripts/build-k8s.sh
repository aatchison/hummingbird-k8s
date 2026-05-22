#!/usr/bin/env bash
set -euo pipefail
# Run from the repo root so the build context (and `lib/`, `containers/`
# paths) resolves consistently regardless of where the operator invokes us.
cd "$(dirname "$(readlink -f "$0")")/.."
# shellcheck source=../lib/build-common.sh
source lib/build-common.sh

require_root

LOCAL_IMAGE=localhost/hummingbird-k8s:latest
NAME=hummingbird-k8s

: "${APISERVER_EXTRA_SANS:=127.0.0.1,localhost}"

render_bib_config > bib-config.toml

podman pull "$BASE_IMAGE"
podman build \
  --build-arg "APISERVER_EXTRA_SANS=${APISERVER_EXTRA_SANS}" \
  --build-arg "ENABLE_CLOUD_INIT=${ENABLE_CLOUD_INIT}" \
  -t "$LOCAL_IMAGE" -f containers/k8s/Containerfile .

build_qcow2 "$LOCAL_IMAGE" "$NAME" "$(pwd)/bib-config.toml"
