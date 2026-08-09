#!/usr/bin/env bash
#
# hbird-staged-check.sh — ExecCondition gate for the operator-side auto-reboot
# timer (see docs/auto-reboot.md).
#
# Exit 0  -> at least one node has a STAGED bootc image, so the roll should run.
# Exit 1  -> nothing staged anywhere, so systemd skips the service cleanly
#            (a non-zero ExecCondition is a *skip*, not a failure).
#
# Node IPs are resolved from libvirt on every run so the gate stays correct
# across VM re-leases — never hard-code the DHCP address.
#
# Usage: hbird-staged-check.sh [CONFIG_PATH]
#   CONFIG_PATH defaults to ./cluster.local.conf (relative to the repo root).
set -euo pipefail

CONF="${1:-cluster.local.conf}"

if [[ ! -r "$CONF" ]]; then
    echo "hbird-staged-check: config not readable: $CONF" >&2
    exit 2
fi

# shellcheck disable=SC1090
source "$CONF"   # provides CP_NAME and WORKER_NAMES=(...)

domains=("$CP_NAME" "${WORKER_NAMES[@]}")

resolve_ip() {
    # First IPv4 lease libvirt knows for the domain. Uses the system URI so a
    # libvirt-group operator reaches it without sudo (the VMs are
    # qemu:///system domains; the default session URI sees nothing).
    local out=""
    out="$(virsh -c qemu:///system -q domifaddr "$1" 2>/dev/null || true)"
    printf '%s\n' "$out" \
        | awk '{print $4}' | cut -d/ -f1 | grep -E '^[0-9]+\.' | head -1 || true
}

for dom in "${domains[@]}"; do
    ip="$(resolve_ip "$dom")"
    if [[ -z "$ip" ]]; then
        echo "hbird-staged-check: no IP for $dom, skipping probe" >&2
        continue
    fi
    staged="$(ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
                  -o ConnectTimeout=5 "root@${ip}" \
                  "bootc status --json | jq -r '.status.staged.image.image.image // empty'" \
              2>/dev/null || true)"
    if [[ -n "$staged" ]]; then
        echo "hbird-staged-check: staged image on ${dom} (${ip}): ${staged} -> running roll"
        exit 0
    fi
done

echo "hbird-staged-check: no staged bootc updates on any node -> skipping roll"
exit 1
