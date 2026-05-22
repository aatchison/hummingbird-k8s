#!/usr/bin/env bash
# health-check-rollback.sh — auto-rollback after a failed bootc upgrade (#104).
#
# Run by health-check-rollback.timer roughly 3 min after every boot. The
# script only takes action when the marker file /var/lib/bootc-just-upgraded
# is present, which is created by the drop-in
# /etc/systemd/system/bootc-fetch-apply-updates.service.d/10-mark-upgrade.conf
# (ExecStartPost on bootc-fetch-apply-updates.service) immediately before the
# unit triggers a reboot to finalize the new deployment. That means:
#
#   - clean install / first boot     → no marker → exit 0
#   - manual `bootc upgrade && reboot` → no marker → exit 0
#   - timer-driven auto-upgrade      → marker → run health checks
#
# We delete the marker on every run so a single bad upgrade never triggers
# more than one rollback (otherwise a flapping health check could ping-pong
# the deployment indefinitely).
#
# If the cluster is unhealthy we call `bootc rollback` and reboot. The
# previous deployment is what we just upgraded from, so the rollback lands
# the node back on the last image that was known-good when it was running.

set -eu

MARKER=/var/lib/bootc-just-upgraded

log() { logger -t health-check-rollback "$*"; printf '[health-check-rollback] %s\n' "$*" >&2; }

# Only act on the first boot after an auto-upgrade. Anything else (fresh
# install, manual reboot, manual upgrade) is operator-driven and not our
# business.
if [ ! -f "${MARKER}" ]; then
  exit 0
fi

# Consume the marker up front so we can't loop on a persistent failure.
rm -f "${MARKER}"

healthy=1

# Always check the kubelet — it's present on every flavor we ship (CP, k3s,
# worker). Use --quiet on the systemctl side since we already log our own
# verdict below.
if ! systemctl is-active --quiet kubelet.service; then
  # k3s ships kubelet inside the k3s.service unit, so on the k3s flavor the
  # standalone kubelet.service unit doesn't exist. Check k3s as a fallback.
  if ! systemctl is-active --quiet k3s.service; then
    log "kubelet/k3s service is not active"
    healthy=0
  fi
fi

# On the CP image the static-pod manifest for kube-apiserver lives at
# /etc/kubernetes/manifests/kube-apiserver.yaml. Use file existence as the
# CP detector so the worker / k3s flavors skip this branch.
if [ -f /etc/kubernetes/manifests/kube-apiserver.yaml ]; then
  if ! curl -fsSk --max-time 5 https://127.0.0.1:6443/livez >/dev/null 2>&1; then
    log "kube-apiserver /livez did not respond OK within 5s"
    healthy=0
  fi
fi

if [ "${healthy}" -eq 0 ]; then
  log "cluster unhealthy after auto-upgrade; calling bootc rollback"
  if ! bootc rollback; then
    log "bootc rollback failed; leaving deployment as-is for operator inspection"
    exit 1
  fi
  log "rollback staged; rebooting"
  systemctl reboot
  exit 0
fi

log "post-upgrade health check passed; staying on new deployment"
exit 0
