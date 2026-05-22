#!/usr/bin/env bash
# Builds the worker image. Refreshes worker-join.env from the running CP
# (so we always bake a current, non-expiring token).
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
# shellcheck source=lib/build-common.sh
source lib/build-common.sh

require_root

LOCAL_IMAGE=localhost/hummingbird-k8s-worker:latest
NAME=hummingbird-k8s-worker

# Refresh the join command from the currently-running CP, if reachable.
# Falls back to the existing worker-join.env (committed placeholder or prior run).
CP_IP=$(virsh -c qemu:///system domifaddr hummingbird-k8s 2>/dev/null \
          | awk '/ipv4/{split($4,a,"/"); print a[1]; exit}' || true)
if [[ -n "$CP_IP" ]]; then
  if sudo -u "$SUDO_USER" ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 \
       "${VM_USER}@${CP_IP}" \
       "${VM_PASSWORD:+echo $VM_PASSWORD | sudo -S }kubeadm token create --ttl 0 --print-join-command 2>/dev/null" \
       | tee worker-join.env.new \
       | grep -q '^kubeadm join'; then
    mv worker-join.env.new worker-join.env
    echo "Refreshed worker-join.env from CP at $CP_IP"
  else
    rm -f worker-join.env.new
    echo "WARN: could not refresh from CP; using existing worker-join.env" >&2
  fi
fi
[[ -s worker-join.env ]] || { echo "No worker-join.env" >&2; exit 1; }

render_bib_config > bib-config-worker.toml

podman pull "$BASE_IMAGE"
podman build -t "$LOCAL_IMAGE" -f Containerfile.k8s-worker .

build_qcow2 "$LOCAL_IMAGE" "$NAME" "$(pwd)/bib-config-worker.toml"
