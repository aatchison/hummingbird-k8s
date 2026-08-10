#!/usr/bin/env bash
#
# hbird-rolling-apply.sh — apply STAGED bootc images across an hbird
# cluster: CP first (no drain — single-CP topology), then each worker
# serially (cordon → drain → reboot → Ready-wait → uncordon).
#
# Exists because `hbird update-cluster` (v0.0.1) cannot execute a live
# roll — its remote SSH/kubectl slice is unimplemented (upstream #322;
# it errors at timer_stop) and the bash twin was removed in the v0.1.0
# cutover. This script is the minimal live executor for the nightly
# apply timer; nodes with nothing staged are skipped untouched.
#
# Reboot detection is bootID-based (same idea as the documented tool):
# /proc/sys/kernel/random/boot_id must CHANGE before we trust that the
# node actually rebooted, so a fast-returning SSH can't false-success.
#
# Usage: hbird-rolling-apply.sh <cluster.conf>
set -euo pipefail

CONF="${1:?usage: hbird-rolling-apply.sh <cluster.conf>}"
# shellcheck disable=SC1090
source "$CONF"   # provides CP_NAME, WORKER_NAMES=(...)

SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8)
REBOOT_TIMEOUT="${REBOOT_TIMEOUT:-600}"
READY_TIMEOUT="${READY_TIMEOUT:-600}"

resolve_ip() {
    virsh -c qemu:///system -q domifaddr "$1" 2>/dev/null \
        | awk '{print $4}' | cut -d/ -f1 | grep -E '^[0-9]+\.' | head -1 || true
}

CP_IP="$(resolve_ip "$CP_NAME")"
[ -n "$CP_IP" ] || { echo "[apply] no libvirt IP for $CP_NAME" >&2; exit 1; }

cpk() {
    # kubectl on the CP via its baked admin.conf (no local kubeconfig needed)
    ssh "${SSH_OPTS[@]}" "root@${CP_IP}" \
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf $*"
}

node_ready() {
    cpk "get node $1 --no-headers" 2>/dev/null | awk '{print $2}' | grep -qx "Ready"
}

apply_node() {
    local node="$1" ip="$2" is_cp="$3" staged boot_id new_id deadline

    staged="$(timeout 25 ssh "${SSH_OPTS[@]}" "root@${ip}" \
        "bootc status --json | jq -r '.status.staged.image.image.image // empty'" \
        2>/dev/null || true)"
    if [ -z "$staged" ]; then
        echo "[apply] $node: nothing staged — skipping"
        return 0
    fi
    echo "[apply] $node ($ip): staged $staged"

    boot_id="$(timeout 25 ssh "${SSH_OPTS[@]}" "root@${ip}" \
        cat /proc/sys/kernel/random/boot_id)"

    if [ "$is_cp" = "0" ]; then
        echo "[apply] $node: cordon + drain"
        cpk "cordon $node"
        if ! cpk "drain $node --ignore-daemonsets --delete-emptydir-data --force --timeout=300s"; then
            echo "[apply] $node: drain failed — uncordoning and aborting" >&2
            cpk "uncordon $node" || true
            return 1
        fi
    else
        echo "[apply] $node: single-CP — no drain; apiserver blips during reboot"
    fi

    echo "[apply] $node: rebooting"
    ssh "${SSH_OPTS[@]}" "root@${ip}" "systemctl reboot" || true

    deadline=$((SECONDS + REBOOT_TIMEOUT))
    new_id=""
    while [ "$SECONDS" -lt "$deadline" ]; do
        sleep 10
        new_id="$(timeout 15 ssh "${SSH_OPTS[@]}" "root@${ip}" \
            cat /proc/sys/kernel/random/boot_id 2>/dev/null || true)"
        if [ -n "$new_id" ] && [ "$new_id" != "$boot_id" ]; then
            break
        fi
        new_id=""
    done
    if [ -z "$new_id" ]; then
        echo "[apply] $node: no bootID change within ${REBOOT_TIMEOUT}s — aborting roll" >&2
        return 1
    fi

    deadline=$((SECONDS + READY_TIMEOUT))
    until node_ready "$node"; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            echo "[apply] $node: not Ready within ${READY_TIMEOUT}s after reboot — aborting roll" >&2
            return 1
        fi
        sleep 10
    done

    if [ "$is_cp" = "0" ]; then
        cpk "uncordon $node"
    fi
    echo "[apply] $node: now booted $(timeout 25 ssh "${SSH_OPTS[@]}" "root@${ip}" \
        "bootc status --json | jq -r .status.booted.image.image.image" 2>/dev/null)"
}

# k8s node names equal libvirt domain names in hbird deployments.
apply_node "$CP_NAME" "$CP_IP" 1

for w in "${WORKER_NAMES[@]}"; do
    ip="$(resolve_ip "$w")"
    [ -n "$ip" ] || { echo "[apply] no libvirt IP for $w" >&2; exit 1; }
    apply_node "$w" "$ip" 0
done

echo "[apply] rolling apply complete"
