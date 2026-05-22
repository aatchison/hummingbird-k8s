#!/usr/bin/env bash
# Builds the worker template image.
#
# The image is a pure template: it does NOT contain a kubeadm join token.
# spawn-workers.sh mints a fresh short-TTL (~2h) token per VM and injects
# it into each cloned qcow2's /etc/hummingbird/worker-join.env just before
# virt-install. See docs/worker-tokens.md for the rationale (no static
# long-lived secret embedded in the published image).
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
# shellcheck source=lib/build-common.sh
source lib/build-common.sh

require_root

LOCAL_IMAGE=localhost/hummingbird-k8s-worker:latest
NAME=hummingbird-k8s-worker

render_bib_config > bib-config-worker.toml

podman pull "$BASE_IMAGE"
podman build -t "$LOCAL_IMAGE" -f Containerfile.k8s-worker .

build_qcow2 "$LOCAL_IMAGE" "$NAME" "$(pwd)/bib-config-worker.toml"
