#!/usr/bin/env bash
#
# hbird-staged-check.sh — ExecCondition gate for the operator-side auto-reboot
# timer (see docs/auto-reboot.md).
#
# Exit 0  -> at least one node has a STAGED bootc image, so the roll should run.
# Exit 1  -> full scan completed; no node has a staged image.
# Exit 2  -> config file not readable (misconfiguration).
# Exit 3  -> scan incomplete — at least one node could not be probed (no libvirt
#            IP, SSH/timeout, etc.); we deliberately do NOT roll on a partial scan.
# All of 1..3 are in systemd's 1..254 "skip cleanly" range, so as an
# ExecCondition any of them skips the roll *without* marking the unit failed
# (only exit 255 or a signal would fail it, which this script never returns).
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

probe_failed=0

for dom in "${domains[@]}"; do
    ip="$(resolve_ip "$dom")"
    if [[ -z "$ip" ]]; then
        echo "hbird-staged-check: no libvirt IP for $dom, skipping probe" >&2
        probe_failed=1
        continue
    fi
    # Host keys are expected to be trusted already (the deploy populates
    # known_hosts); accept-new is only a TOFU fallback for a freshly re-leased
    # IP. `timeout` bounds the WHOLE probe — ConnectTimeout covers only the SSH
    # handshake, not the remote `bootc status` execution.
    if staged="$(timeout 20 ssh \
                    -o BatchMode=yes \
                    -o StrictHostKeyChecking=accept-new \
                    -o ConnectTimeout=5 \
                    -o ServerAliveInterval=5 -o ServerAliveCountMax=2 \
                    "root@${ip}" \
                    "bootc status --json | jq -r '.status.staged.image.image.image // empty'" \
                 2>/dev/null)"; then
        if [[ -n "$staged" ]]; then
            echo "hbird-staged-check: staged image on ${dom} (${ip}): ${staged} -> running roll"
            exit 0
        fi
    else
        echo "hbird-staged-check: probe failed for $dom ($ip)" >&2
        probe_failed=1
    fi
done

if [[ "$probe_failed" -eq 1 ]]; then
    echo "hbird-staged-check: scan incomplete (a node could not be probed) -> skipping roll" >&2
    exit 3
fi

echo "hbird-staged-check: no staged bootc updates on any node -> skipping roll"
exit 1
