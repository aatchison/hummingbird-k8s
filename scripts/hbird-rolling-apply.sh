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

# Host-key policy for the unattended root logins. Default `accept-new` keeps
# routine operation working when a node returns on a fresh DHCP lease, at the
# cost of trust-on-first-use for that new address. Set SSH_STRICT=yes (with a
# pre-populated known_hosts) to require verified keys and fail closed instead.
SSH_STRICT="${SSH_STRICT:-accept-new}"
SSH_OPTS=(-o BatchMode=yes -o "StrictHostKeyChecking=${SSH_STRICT}" -o ConnectTimeout=8)
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
    # STATUS is a comma-joined condition list, and the node is still cordoned
    # at this point in the roll — so it reads "Ready,SchedulingDisabled", not
    # "Ready". Match Ready as a whole element, never the entire field.
    cpk "get node $1 --no-headers" 2>/dev/null \
        | awk '{print $2}' | grep -qE '(^|,)Ready(,|$)'
}

node_kubelet_boot_id() {
    # The boot ID the KUBELET registered, i.e. what the apiserver believes.
    # A changed /proc boot ID only proves SSH reached a rebooted OS; the node
    # object can still carry the pre-reboot Ready condition until the node
    # controller ages it out. Gating on this instead makes "Ready" mean "the
    # post-reboot kubelet said so".
    cpk "get node $1 -o jsonpath='{.status.nodeInfo.bootID}'" 2>/dev/null | tr -d "'"
}

node_booted_image() {
    timeout 25 ssh "${SSH_OPTS[@]}" "root@${1}" \
        "bootc status --json | jq -r '.status.booted.image.image.image // empty'" 2>/dev/null
}

apply_node() {
    local node="$1" ip="$2" is_cp="$3" staged boot_id new_id booted_after deadline

    # Distinguish "probe worked, nothing staged" from "probe failed". Folding
    # an unreachable node into the former would silently skip it and let the
    # roll report success over a node nobody checked.
    if ! staged="$(timeout 25 ssh "${SSH_OPTS[@]}" "root@${ip}" \
        "bootc status --json | jq -r '.status.staged.image.image.image // empty'" \
        2>/dev/null)"; then
        echo "[apply] $node ($ip): staged-image probe FAILED (unreachable?) — aborting roll" >&2
        return 1
    fi
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
        [ "$is_cp" = "0" ] && echo "[apply] $node: LEFT CORDONED for inspection" >&2
        return 1
    fi

    # Require BOTH that the kubelet re-registered with the post-reboot boot ID
    # and that the node is Ready — otherwise a stale pre-reboot Ready condition
    # (or a kubelet that never came back) can look like success.
    deadline=$((SECONDS + READY_TIMEOUT))
    until [ "$(node_kubelet_boot_id "$node")" = "$new_id" ] && node_ready "$node"; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            echo "[apply] $node: kubelet did not re-register Ready with the new boot ID within ${READY_TIMEOUT}s — aborting roll" >&2
            [ "$is_cp" = "0" ] && echo "[apply] $node: LEFT CORDONED for inspection (kubectl uncordon $node when healthy)" >&2
            return 1
        fi
        sleep 10
    done

    # Confirm the staged image is the one now running. bootc rolls back to the
    # previous deployment if the new one fails to boot, so "it came back Ready"
    # is not evidence the update took.
    booted_after="$(node_booted_image "$ip")"
    if [ "$booted_after" != "$staged" ]; then
        echo "[apply] $node: expected to boot '$staged' but is running '${booted_after:-<unknown>}' (rollback?) — aborting roll" >&2
        [ "$is_cp" = "0" ] && echo "[apply] $node: LEFT CORDONED for inspection" >&2
        return 1
    fi

    if [ "$is_cp" = "0" ]; then
        cpk "uncordon $node"
    fi
    echo "[apply] $node: now booted $booted_after"
}

# k8s node names equal libvirt domain names in hbird deployments.
apply_node "$CP_NAME" "$CP_IP" 1

for w in "${WORKER_NAMES[@]}"; do
    ip="$(resolve_ip "$w")"
    [ -n "$ip" ] || { echo "[apply] no libvirt IP for $w" >&2; exit 1; }
    apply_node "$w" "$ip" 0
done

echo "[apply] rolling apply complete"
