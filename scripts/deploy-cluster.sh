#!/usr/bin/env bash
# Shared SSH/virsh/log helpers live in lib/build-common.sh; see docs/development.md.
# scripts/deploy-cluster.sh — Hybrid bib + cloud-init cluster orchestrator.
#
# Deploys 1 control plane + N workers on the local KVM host from a single
# config file. The "hybrid" design splits state along a clear seam:
#
#   bib-baked (static, in the image):
#     - Default user account + SSH pubkey set
#     - SSH hardening, kubelet protect-kernel-defaults, PSA, audit
#     - All image content (kubeadm, cri-o, kubelet, cilium prep, etc.)
#
#   cloud-init seed (per-VM, dynamic):
#     - Hostname
#     - For workers: /etc/hummingbird/worker-join.env (the kubeadm join
#       command minted from the live CP — short-TTL, never baked)
#     - First-boot runcmd: `bootc switch` to the GHCR ref; enable the
#       auto-update timer on the CP (overrides #48's opt-out)
#
# This is the canonical (and only) supported way to stand
# up a Hummingbird cluster: ENABLE_CLOUD_INIT=1 images, per-VM NoCloud
# seed ISOs attached at virt-install time, worker join via cloud-init's
# write_files (no offline qcow2 mutation, no libguestfs fishing the
# ostree deployment dir out of the bootc image).
#
# Usage:
#   sudo bash scripts/deploy-cluster.sh [path/to/cluster.local.conf]
#
# If no path is given, defaults to ./cluster.local.conf in the repo root.

set -euo pipefail

# ---- Locate self / repo root ------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# render_cp_user_data — emit the CP cloud-init user-data YAML to stdout.
# Inputs (env vars): CP_NAME, SSH_PUBKEY_CONTENT, GHCR_TAG, SWITCH_TO_GHCR,
# AUTO_UPDATE_CP, BOOTC_UPDATE_SCHEDULE, BOOTC_UPDATE_REPO_K8S,
# POD_CIDR, SERVICE_CIDR (optional per-cluster CIDR overrides — written to
# /etc/hummingbird/k8s-init-local.env, sourced by k8s-init.sh after the
# image-baked k8s-init.env).
#
# Extracted from the inline `{ ... } > $CP_USER_DATA` block so
# tests/scripts/deploy-cluster.bats can exercise the rendered output
# without invoking the rest of the script (which requires root + libvirt).
# Defined here, above the root/sudo checks, so the source-only guard
# below can short-circuit and expose the function to tests. (#181 round-2
# review.)
render_cp_user_data() {
  printf '#cloud-config\n'
  printf 'hostname: %s\n' "$CP_NAME"
  # Set the key on root explicitly. The top-level `ssh_authorized_keys`
  # defaults to cloud-init's "default user" which on Fedora is `fedora` —
  # that fights the Hummingbird sudoless-root-only model.
  printf 'disable_root: false\n'
  printf 'users:\n'
  printf '  - name: root\n'
  printf '    ssh_authorized_keys:\n'
  printf '      - %s\n' "$SSH_PUBKEY_CONTENT"
  # write_files for bootc-semver-update + CIDR overrides. Only emit the
  # block when at least one override is set, otherwise the YAML stays
  # clean (no empty write_files array).
  if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" || -n "${BOOTC_UPDATE_REPO_K8S:-}" || -n "${POD_CIDR:-}" || -n "${SERVICE_CIDR:-}" ]]; then
    printf 'write_files:\n'
    if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
      # Drop-in to override the image's OnCalendar=. Empty OnCalendar=
      # first clears the default; the second line sets the override.
      printf '  - path: /etc/systemd/system/bootc-semver-update.timer.d/schedule.conf\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0644'\n"
      printf '    content: |\n'
      printf '      [Timer]\n'
      printf '      OnCalendar=\n'
      printf '      OnCalendar=%s\n' "$BOOTC_UPDATE_SCHEDULE"
    fi
    if [[ -n "${BOOTC_UPDATE_REPO_K8S:-}" ]]; then
      printf '  - path: /etc/hummingbird/bootc-update.env\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0644'\n"
      printf '    content: |\n'
      printf '      REPO=%s\n' "$BOOTC_UPDATE_REPO_K8S"
      printf '      PREFIX=v\n'
    fi
    if [[ -n "${POD_CIDR:-}" || -n "${SERVICE_CIDR:-}" ]]; then
      # Per-cluster CIDR overrides for the first-boot k8s-init script.
      # A SEPARATE file from the image-baked k8s-init.env so a bootc
      # image update never clobbers operator config (and vice versa).
      # k8s-init.sh sources this AFTER the baked env, so these win.
      printf '  - path: /etc/hummingbird/k8s-init-local.env\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0600'\n"
      printf '    content: |\n'
      if [[ -n "${POD_CIDR:-}" ]]; then
        printf '      POD_CIDR=%s\n' "$POD_CIDR"
      fi
      if [[ -n "${SERVICE_CIDR:-}" ]]; then
        printf '      SERVICE_CIDR=%s\n' "$SERVICE_CIDR"
      fi
    fi
  fi
  # Always emit a runcmd block now: the AUTO_UPDATE_CP=false branch needs
  # to actively disable the timer (the image's preset enables it
  # unconditionally on factory reset, so AUTO_UPDATE_CP=false alone was a
  # no-op pre-#181-round-2). #181 round-2 review.
  printf 'runcmd:\n'
  # Before anything else: neutralise the second NIC's IPv6 (the v2 keys
  # never reach NetworkManager — see emit_net2_ipv6_off_runcmd).
  if [[ -n "${EXTRA_NETWORK:-}" && -n "${EXTRA_NET_CP_MAC:-}" ]]; then
    emit_net2_ipv6_off_runcmd "$EXTRA_NET_CP_MAC"
  fi
  if [[ "$SWITCH_TO_GHCR" = "true" ]]; then
    printf '  - [ bootc, switch, ghcr.io/aatchison/hummingbird-k8s:%s ]\n' "$GHCR_TAG"
  fi
  if [[ "$AUTO_UPDATE_CP" = "true" ]]; then
    # Enable the semver-aware updater (this PR's mechanism) — the image's
    # preset already enables it on a fresh boot, but a runtime `systemctl
    # enable --now` is the belt-and-suspenders path that also makes the
    # timer fire its OnBootSec=15min window immediately rather than after
    # the next reboot. The legacy bootc-fetch-apply-updates.timer is
    # disabled in the new preset; re-enabling it here would fight the
    # operator's "advance only on new semver tags" intent.
    printf '  - [ systemctl, enable, --now, bootc-semver-update.timer ]\n'
    # Belt-and-suspenders: disable the legacy timer too, in case a
    # pre-#181 host is being re-cloud-inited (the new image's preset
    # would have already disabled it on first boot, but explicit disable
    # is cheap and idempotent on already-disabled units). (#181 round-2
    # review.)
    printf '  - [ systemctl, disable, --now, bootc-fetch-apply-updates.timer ]\n'
  else
    # AUTO_UPDATE_CP=false: actively counter the preset's unconditional
    # enable so the operator's "no auto-updates on the CP" intent is
    # honored. (#181 round-2 review.)
    printf '  - [ systemctl, disable, --now, bootc-semver-update.timer ]\n'
  fi
  if [[ "$AUTO_UPDATE_CP" = "true" && -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
    # Re-read the drop-in cloud-init just wrote, then restart the timer
    # so the override takes effect this boot (not just next). Gated on
    # AUTO_UPDATE_CP=true so the false-branch's disable above stays sticky
    # (CodeRabbit #181).
    printf '  - [ systemctl, daemon-reload ]\n'
    printf '  - [ systemctl, restart, bootc-semver-update.timer ]\n'
  fi
}

# worker_user_data — emit a worker cloud-init user-data YAML to a file.
# Inputs:
#   $1 — worker_name (becomes the cloud-init hostname:)
#   $2 — out_file path
#   $3 — OPTIONAL second-NIC MAC; when non-empty, emits the IPv6-off runcmd
# Env vars: SSH_PUBKEY_CONTENT, JOIN_CMD, BOOTC_UPDATE_SCHEDULE,
#           BOOTC_UPDATE_REPO_WORKER, SWITCH_TO_GHCR, GHCR_TAG.
#
# Defined here (above the source-only guard, alongside render_cp_user_data)
# so tests/scripts/deploy-cluster.bats can exercise the rendered output
# without invoking the rest of the script (root + libvirt + bib). (#254.)
worker_user_data() {
  local worker_name="$1" out_file="$2" net2_mac="${3:-}"
  {
    printf '#cloud-config\n'
    printf 'hostname: %s\n' "$worker_name"
    # See CP user-data for the disable_root / users-as-root rationale.
    printf 'disable_root: false\n'
    printf 'users:\n'
    printf '  - name: root\n'
    printf '    ssh_authorized_keys:\n'
    printf '      - %s\n' "$SSH_PUBKEY_CONTENT"
    printf 'write_files:\n'
    printf '  - path: /etc/hummingbird/worker-join.env\n'
    printf '    owner: root:root\n'
    printf "    permissions: '0600'\n"
    printf '    content: |\n'
    # Indent the join command exactly 6 spaces to match YAML block scalar
    # under "content: |" at 4-space indent.
    printf '      %s\n' "$JOIN_CMD"
    # bootc-semver-update overrides. Only emit each entry when the
    # corresponding operator var is set.
    if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
      printf '  - path: /etc/systemd/system/bootc-semver-update.timer.d/schedule.conf\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0644'\n"
      printf '    content: |\n'
      printf '      [Timer]\n'
      printf '      OnCalendar=\n'
      printf '      OnCalendar=%s\n' "$BOOTC_UPDATE_SCHEDULE"
    fi
    if [[ -n "${BOOTC_UPDATE_REPO_WORKER:-}" ]]; then
      printf '  - path: /etc/hummingbird/bootc-update.env\n'
      printf '    owner: root:root\n'
      printf "    permissions: '0644'\n"
      printf '    content: |\n'
      printf '      REPO=%s\n' "$BOOTC_UPDATE_REPO_WORKER"
      printf '      PREFIX=v\n'
    fi
    if [[ "$SWITCH_TO_GHCR" = "true" || -n "${BOOTC_UPDATE_SCHEDULE:-}" || -n "$net2_mac" ]]; then
      printf 'runcmd:\n'
      # First: neutralise the second NIC's IPv6 (see the CP path).
      if [[ -n "$net2_mac" ]]; then
        emit_net2_ipv6_off_runcmd "$net2_mac"
      fi
      if [[ "$SWITCH_TO_GHCR" = "true" ]]; then
        printf '  - [ bootc, switch, ghcr.io/aatchison/hummingbird-k8s-worker:%s ]\n' "$GHCR_TAG"
      fi
      if [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" ]]; then
        # Re-read the drop-in cloud-init just wrote, then restart the
        # timer so the override takes effect this boot.
        printf '  - [ systemctl, daemon-reload ]\n'
        printf '  - [ systemctl, restart, bootc-semver-update.timer ]\n'
      fi
    fi
  } > "$out_file"
}

# render_net2_network_config — emit a cloud-init network-config (v2) YAML
# to stdout for a dual-NIC VM.
# Inputs:
#   $1 — MAC of the second NIC (as passed to virt-install --network mac=)
#   $2 — static address for the second NIC in CIDR form (e.g. 10.0.0.241/24)
#
# Why network-config and not a write_files NM keyfile: this file is rendered
# by cloud-init-local BEFORE NetworkManager starts. A keyfile delivered via
# write_files (cloud-config stage) loses the race — NM auto-defaults the new
# NIC with DHCP first, and if that network's DHCP hands out a gateway the
# node grows a second default route and its identity (apiserver
# advertise-address, kubelet node IP) can move. The static stanza below has
# NO gateway and disables RA, so the second NIC can never carry a default
# route: internal traffic and node identity stay on the primary NIC.
#
# The primary NIC is matched by name (enp1s0 — deterministic on the q35
# machine type these VMs use: first pcie-root-port, slot 1). It must be
# declared here because providing ANY network-config disables cloud-init's
# fallback DHCP config; omitting it would leave the primary NIC dead.
render_net2_network_config() {
  local mac="$1" ip="$2"
  printf 'version: 2\n'
  printf 'ethernets:\n'
  printf '  primary:\n'
  printf '    match:\n'
  printf '      name: enp1s0\n'
  printf '    dhcp4: true\n'
  printf '  net2:\n'
  printf '    match:\n'
  printf '      macaddress: "%s"\n' "$mac"
  printf '    dhcp4: false\n'
  printf '    dhcp6: false\n'
  printf '    accept-ra: false\n'
  printf '    addresses:\n'
  printf '      - "%s"\n' "$ip"
}

# emit_net2_ipv6_off_runcmd — emit the runcmd entry that disables IPv6 on
# the second NIC. Takes that NIC's MAC.
#
# WHY THIS EXISTS: render_net2_network_config emits `accept-ra: false` and
# `dhcp6: false`, and cloud-init's NetworkManager renderer SILENTLY DROPS
# BOTH — network_manager.py has no accept-ra handling at all, and with no
# IPv6 subnet it never emits an [ipv6] section, so NetworkManager
# normalizes the keyfile to ipv6.method=auto. VERIFIED on a live node: the
# rendered cloud-init-net2.nmconnection had NO [ipv6] section and
# `nmcli -g ipv6.method` reported `auto`.
#
# Left alone, an EXTRA_NETWORK segment carrying router advertisements
# would SLAAC an address onto the second NIC and install an IPv6 default
# route there — routing node IPv6 egress out the NIC that is supposed to
# carry no default route at all. IPv4 identity (and NODE_IP derivation,
# which is `ip -4`) is unaffected, which is precisely why an all-IPv4 test
# on an RA-free segment cannot catch it.
#
# Resolved MAC -> device -> connection because the NM renderer names the
# connection after the matched INTERFACE (observed: "cloud-init enp2s0"),
# not after the network-config key ("net2") — so the name is not
# predictable from here. Non-fatal on lookup failure: a missing NIC must
# not block the rest of first boot.
emit_net2_ipv6_off_runcmd() {
  local mac="$1"
  printf '  - |\n'
  printf '    # Disable IPv6 on the second NIC (accept-ra/dhcp6 do not survive the NM renderer).\n'
  printf '    dev=$(ip -o link | awk -v m=%s '\''tolower($0) ~ tolower(m) {gsub(/:/,"",$2); print $2; exit}'\'')\n' "$mac"
  printf '    [ -n "$dev" ] || { echo "hbird: no interface with MAC %s" >&2; exit 0; }\n' "$mac"
  printf '    con=$(nmcli -g GENERAL.CONNECTION device show "$dev")\n'
  printf '    [ -n "$con" ] || { echo "hbird: no NM connection on $dev" >&2; exit 0; }\n'
  printf '    nmcli connection modify "$con" ipv6.method disabled\n'
  printf '    nmcli connection up "$con" >/dev/null\n'
}

# derive_primary_mac — deterministic MAC for a VM's PRIMARY NIC, derived
# from its domain name. The same name always yields the same MAC.
#
# WHY (#409): the pinned kubelet --node-ip is a DHCP lease frozen at first
# boot. Without a stable MAC a rebuilt VM gets a libvirt-random MAC, hence
# a different lease, while the pinned --node-ip stays put — leaving kubelet
# with an address on no interface. A name-derived MAC makes dnsmasq hand
# back the same lease (its db is keyed on MAC) and makes an explicit
# reservation possible.
#
# 52:54:00 is the QEMU/KVM OUI libvirt itself uses. The low three bytes are
# the first 6 hex digits of the name's sha256; a collision across the
# handful of VMs on one host is negligible, and validation checks for one
# explicitly against the EXTRA_NET_* family.
derive_primary_mac() {
  local name="$1" h
  h="$(printf '%s' "$name" | sha256sum | cut -c1-6)"
  printf '52:54:00:%s:%s:%s' "${h:0:2}" "${h:2:2}" "${h:4:2}"
}

# ensure_dhcp_reservation — add a <host mac ip> entry to a libvirt network
# so a VM's primary address is a RESERVATION, not merely a sticky lease.
# Idempotent, and never fatal: a host that cannot net-update, or a network
# that already carries the entry, must not abort a deploy.
ensure_dhcp_reservation() {
  local net="$1" mac="$2" ip="$3" name="$4"
  [[ -n "$ip" ]] || return 0
  if virsh -c qemu:///system net-dumpxml "$net" 2>/dev/null | grep -qi "mac='${mac}'"; then
    log "DHCP reservation for ${name} (${mac} -> ${ip}) already present on network '${net}'"
    return 0
  fi
  # Reserving an address another host already holds is worse than no
  # reservation at all.
  if virsh -c qemu:///system net-dumpxml "$net" 2>/dev/null | grep -q "ip='${ip}'"; then
    log "WARN: ${ip} is already reserved on network '${net}' under a different MAC — skipping reservation for ${name}. Pick a free CP_IP/WORKER_IPS value or clear the stale entry."
    return 0
  fi
  if virsh -c qemu:///system net-update "$net" add ip-dhcp-host \
       "<host mac='${mac}' name='${name}' ip='${ip}'/>" --live --config >/dev/null 2>&1; then
    log "added DHCP reservation on '${net}': ${name} ${mac} -> ${ip}"
  else
    log "WARN: could not add a DHCP reservation for ${name} on '${net}' (continuing — the pinned MAC still makes the lease sticky)"
  fi
}

# resolve_switch_to_ghcr: #374 guard against the second false-positive
# boot-test mechanism (sibling to #373's qcow2 cache reuse). FORCE_REBUILD=1
# signals the operator is testing a SPECIFIC freshly-built image (a local
# build, or a forced GHCR re-pull). But when SWITCH_TO_GHCR=true the render
# functions above emit a first-boot `bootc switch ghcr.io/...:$GHCR_TAG` on
# both the CP (~L88) and the workers (~L170) — which would immediately replace
# those just-built bits with whatever the registry currently serves, silently
# defeating the test (this is exactly how PR #367's cycle-2 boot-test came up
# green on the PRE-change Cilium). So when FORCE_REBUILD=1 AND
# SWITCH_TO_GHCR=true, auto-disable the switch with a loud WARN — unless the
# operator opts back in with FORCE_SWITCH=1 (the rare "rebuild, then
# deliberately run the registry image" case). FORCE_SWITCH mirrors the exact
# knob name spawn-workers.sh uses (#375) for silent-override-family
# consistency. Mutates the global SWITCH_TO_GHCR; idempotent; a no-op unless
# both flags are set. (#374.)
resolve_switch_to_ghcr() {
  [[ "${SWITCH_TO_GHCR:-}" == "true" ]] || return 0
  [[ "${FORCE_REBUILD:-}" == "1" || "${IMAGE_SOURCE:-}" == "local" ]] || return 0

  if [[ "${FORCE_SWITCH:-}" == "1" ]]; then
    log "FORCE_SWITCH=1: keeping SWITCH_TO_GHCR=true despite FORCE_REBUILD=1 or IMAGE_SOURCE=local — the freshly-built image WILL be replaced by ghcr.io/...:${GHCR_TAG:-latest} at first boot (explicit opt-in)."
    return 0
  fi

  local reason="FORCE_REBUILD=1"
  [[ "${IMAGE_SOURCE:-}" == "local" ]] && reason="IMAGE_SOURCE=local"
  [[ "${FORCE_REBUILD:-}" == "1" && "${IMAGE_SOURCE:-}" == "local" ]] && reason="FORCE_REBUILD=1 and IMAGE_SOURCE=local"

  log "WARN: ${reason} with SWITCH_TO_GHCR=true would replace your freshly-built qcow2 with ghcr.io/...:${GHCR_TAG:-latest} at first boot, silently defeating a local boot-test (#374). Auto-disabling the GHCR switch; set FORCE_SWITCH=1 to keep it."
  SWITCH_TO_GHCR=false
}

# warn_bootc_update_drift: #376's deferred deploy-time WARN, folded in here per
# bt-374's decided-scope (#380 documented the drift but left the deploy-cluster.sh
# WARN to wave 2 to avoid colliding with #377). BOOTC_UPDATE_SCHEDULE arms
# bootc-semver-update.timer on the deployed nodes, which periodically advances
# them to the highest published vX.Y.Z GHCR tag (semver-aware — NOT a literal
# :latest poll). Across PRs that means a cluster silently drifts off the image
# it was deployed with: fine for a long-lived lab, a footgun for a reproducible
# boot-test. Surface it once at deploy time. See docs/auto-updates.md (#376) and
# the override family #373/#374/#375/#377. No-op unless the schedule is set.
warn_bootc_update_drift() {
  [[ -n "${BOOTC_UPDATE_SCHEDULE:-}" ]] || return 0
  log "WARN: BOOTC_UPDATE_SCHEDULE=${BOOTC_UPDATE_SCHEDULE} arms bootc-semver-update.timer — this cluster will advance to the highest published vX.Y.Z GHCR tag over time, drifting off the image it was deployed with (silent multi-PR mismatch). Leave BOOTC_UPDATE_SCHEDULE unset for a reproducible boot-test. See docs/auto-updates.md (#376)."
}

# Source-only mode for bats: when HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY=1, return
# from `source` here so the test can call render_cp_user_data / worker_user_data
# without the script's root/libvirt orchestration kicking in. (#181 round-2,
# extended for #254 worker_user_data coverage.)
if [[ "${HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY:-0}" = 1 ]]; then
  return 0
fi

# ---- CLI-env snapshot (MUST be before any `source lib/*.sh`) (#377 fix, #2) --
# lib/build-common.sh (and other libs sourced below) run `: "${KNOB:=default}"`,
# which sets KNOB to the lib default when unset. The original #377 capture loop
# ran AFTER those lib sources, so it snapped the lib default (e.g.
# ENABLE_CLOUD_INIT=0 from build-common.sh:139) as a phantom CLI override; the
# restore then clobbered the config's ENABLE_CLOUD_INIT=1 back to 0 and deploy
# hard-failed. Fix: capture the genuine parent-env/CLI state HERE, before any lib
# can default these knobs. The restore loop stays after `source "$CONFIG_PATH"`.
# shellcheck disable=SC2034  # snapshot vars are read indirectly via printf -v
HBIRD_CLI_OVERRIDE_KNOBS=(
  IMAGE_SOURCE GHCR_ORG GHCR_TAG SWITCH_TO_GHCR FORCE_SWITCH AUTO_UPDATE_CP
  FORCE_REBUILD STRICT_CACHE ENABLE_CLOUD_INIT RUN_VERIFY
  BOOTC_UPDATE_SCHEDULE BOOTC_UPDATE_REPO_K8S BOOTC_UPDATE_REPO_WORKER
  BOOTC_SWITCH_TO_GHCR KVM_HOST
  CP_MEMORY CP_VCPUS WORKER_MEMORY WORKER_VCPUS POOL_DIR
  BASE_IMAGE BIB ENABLE_ROOT_SSH VM_USER VM_USER_GROUPS
)
for _hbird_knob in "${HBIRD_CLI_OVERRIDE_KNOBS[@]}"; do
  printf -v "_hbird_cli_${_hbird_knob}" '%s' "${!_hbird_knob:-}"
done
# ---- end CLI-env snapshot ---------------------------------------------------

# ---- Remote KVM-host re-exec shim (C3, #232) -------------------------------
# When KVM_HOST is set and we're NOT on the KVM host, re-exec this script
# on the remote host via SSH. The client never needs sudo or libvirt —
# only ssh + the operator's existing SSH key. Sudo happens on the remote.
#
# Env-var passthrough: EXPLICIT ALLOWLIST. Anything else stays client-side.
# Add new vars to ALLOWED_ENV in hbird_ssh_wrap below if the script grows
# new tunables. The allowlist is pinned by tests/scripts/ssh-wrap.bats.
#
# shellcheck source=lib/ssh-wrap.sh
source "${SCRIPT_DIR}/lib/ssh-wrap.sh"
hbird_ssh_wrap_maybe_reexec "$0" "$@"
# ---- End remote re-exec shim -----------------------------------------------

# shellcheck source=../lib/build-common.sh
source "${REPO_ROOT}/lib/build-common.sh"
# shellcheck source=lib/cloud-init-seed.sh
source "${SCRIPT_DIR}/lib/cloud-init-seed.sh"
# shellcheck source=../lib/cache-utils.sh
source "${REPO_ROOT}/lib/cache-utils.sh"

setup_logging "[deploy-cluster]"

# ---- Root + arg parsing -----------------------------------------------------

# deploy-cluster.sh historically required root for libvirt qemu:///system,
# bootc-image-builder, and for writing qcow2 templates + per-VM clones into
# root-owned $POOL_DIR. With #305 we mirror #272's update-cluster pattern:
# accept either root OR a member of the `libvirt` group on the KVM host.
# libvirt authorizes qemu:///system via the unix-socket group, not sudo;
# bootc-image-builder runs rootless under podman; POOL_DIR writes work for
# libvirt-group operators when the dir is chgrp'd to libvirt + chmod 2775
# (one-time host setup, see docs/deploy-cluster.md#running-without-sudo-libvirt-group-operator-305).
# Non-root + not in libvirt group is still a fail with an actionable hint.
if [[ $EUID -ne 0 ]]; then
  if ! id -nG 2>/dev/null | tr ' ' '\n' | grep -qx libvirt; then
    fail "must be root or a member of the libvirt group on this host. Add yourself with:
  sudo usermod -aG libvirt \$USER && newgrp libvirt
then rerun. POOL_DIR must also be group-writable. One-time setup (substitute your POOL_DIR from cluster.local.conf — defaults to /var/lib/libvirt/images):
  sudo chgrp libvirt /var/lib/libvirt/images
  sudo chmod 2775 /var/lib/libvirt/images
See docs/deploy-cluster.md#running-without-sudo-libvirt-group-operator-305."
  fi
  # Symmetric with update-cluster.sh's #272 log (round-2 review L8 — silent
  # no-sudo path is an asymmetric regression vs the precedent we claim to
  # mirror). Announce which branch ran so an operator debugging "why didn't
  # this chown root:root?" sees the answer in the log.
  log "running as ${USER:-$(id -un)} (libvirt group member); skipping chown root:root on qcow2 clones — libvirt dynamic_ownership chowns to qemu:qemu at VM start (#305)"
fi
# SUDO_USER was historically required so SSH to the freshly-booted CP picks up
# the calling-user's known_hosts/key when invoked via `sudo`. The SSH calls
# themselves use SSH_PRIVKEY_FILE (derived from $SSH_PUBKEY_FILE in CONFIG) and
# UserKnownHostsFile=/dev/null, so SUDO_USER is informational, not load-bearing.
# Only insist on it when EUID==0 — a libvirt-group operator runs in their own
# shell with their own ~/.ssh in place. (#305)
if [[ $EUID -eq 0 ]]; then
  : "${SUDO_USER:?must be invoked via sudo so SSH to the freshly-booted CP uses the calling-user known_hosts/key}"
fi

CONFIG_PATH="${1:-${REPO_ROOT}/cluster.local.conf}"
[[ -r "$CONFIG_PATH" ]] || fail "config not readable: $CONFIG_PATH (start from cluster.example.conf)"

# Track seed ISOs we create so the failure-trap can clean them up without
# touching VMs the operator might still want to debug.
SEED_ISOS=()
# Track BIB config tempfiles (#310) so the failure-trap can clean them up
# too. These are tiny TOMLs build_qcow2 consumes once; persisting them
# would just be litter — and pre-#310 they were written into REPO_ROOT
# where root-owned leftovers blocked subsequent non-root deploys (the
# whole reason for routing through mktemp).
BIB_CFG_TEMPS=()
cleanup_on_failure() {
  local rc=$?
  if (( rc != 0 )); then
    if (( ${#SEED_ISOS[@]} > 0 )); then
      log "deploy failed (rc=${rc}); cleaning up half-built seed ISOs."
      local s
      for s in "${SEED_ISOS[@]}"; do
        [[ -e "$s" ]] && rm -f "$s"
      done
    fi
    if (( ${#BIB_CFG_TEMPS[@]} > 0 )); then
      log "deploy failed (rc=${rc}); cleaning up BIB config tempfiles."
      local t
      for t in "${BIB_CFG_TEMPS[@]}"; do
        [[ -e "$t" ]] && rm -f "$t"
      done
    fi
  fi
}
trap cleanup_on_failure EXIT

# ---- Source config + validate ----------------------------------------------

# CLI-env precedence (Pattern A, #377). `source "$CONFIG_PATH"` performs
# unconditional `VAR=...` assignment, so a config file that hard-assigns an
# operator-overridable knob (e.g. `IMAGE_SOURCE=ghcr`, `SWITCH_TO_GHCR=true`)
# silently clobbers whatever the operator passed on the CLI
# (`make deploy-cluster IMAGE_SOURCE=local SWITCH_TO_GHCR=false`). The
# `: "${VAR:=default}"` defaulting below cannot rescue this — by then the var
# is already set to the config's value. HBIRD_CLI_OVERRIDE_KNOBS is declared
# and the snapshot captured at the TOP of the script (before any lib sources)
# so lib-defaulted values are never mistaken for genuine CLI overrides (#2).
# The restore here wins a non-empty CLI value over the config. Net precedence:
# CLI > config > built-in default.
#
# Scope of the list: every scalar operator knob this script reads AFTER the
# source. A knob that is also forwarded across the C3 KVM-host re-exec must
# ALSO appear in HBIRD_SSH_WRAP_ALLOWED_ENV (scripts/lib/ssh-wrap.sh:200), or
# the remote side never receives the CLI value and the restore below sees an
# empty snapshot. Host-local-only knobs (CP_MEMORY, CP_VCPUS, WORKER_MEMORY,
# WORKER_VCPUS, RUN_VERIFY, ...) are deliberately NOT in the allowlist — they
# act on the machine that actually runs virt-install — so they live ONLY here.
# When adding a knob, decide which category it is and update the right place(s).
#
# KVM_HOST is included because it is operator-overridable and consumed AFTER
# the source (the RUN_VERIFY --kvm-host arg and the closing hint). The separate
# re-exec shim above reads KVM_HOST BEFORE this block to decide whether to hop
# to the remote host; that path is governed by ssh-wrap.sh, not this array.
# WORKER_NAMES is an array, resolved by its own block below — do NOT add it
# here (printf -v on an array name would mangle it).

# shellcheck disable=SC1090
source "$CONFIG_PATH"

# Restore operator CLI overrides — a non-empty CLI value wins over any hard
# assignment the config just made. An empty/unset CLI value leaves the config
# (or downstream default) in place. (#377)
for _hbird_knob in "${HBIRD_CLI_OVERRIDE_KNOBS[@]}"; do
  _hbird_snap="_hbird_cli_${_hbird_knob}"
  [[ -n "${!_hbird_snap:-}" ]] && printf -v "$_hbird_knob" '%s' "${!_hbird_snap}"
done
unset _hbird_knob _hbird_snap
# ---- end CLI-env precedence (#377) ------------------------------------------

# Required scalars.
: "${CP_NAME:?CP_NAME is required in $CONFIG_PATH}"
: "${SSH_PUBKEY_FILE:?SSH_PUBKEY_FILE is required in $CONFIG_PATH}"
# IMAGE_SOURCE defaults to ghcr — the registry-first golden path. A
# workstation operator running `make deploy-cluster` should get a
# GHCR-pulled image with no local-build prerequisite. `IMAGE_SOURCE=local`
# stays a valid power-user / fast-iteration choice (build fresh from
# containers/k8s and containers/k8s-worker) — see cluster.example.conf
# and docs/deploy-cluster.md. (#231)
#
# Track whether the operator set IMAGE_SOURCE explicitly so we can emit
# a one-line stderr notice when we fall through to the default. Pre-#231
# this was a hard-fail (`:?` form) that doubled as a guardrail; the
# silent switch to a default could surprise operators upgrading on a
# config that never had IMAGE_SOURCE in it. The notice is informational,
# not a warning — `ghcr` is the documented default — but it makes the
# resolution visible in the log so an operator who wanted `local` and
# forgot to set it sees what happened. (#231 round-2 review M1.)
_image_source_was_unset=0
[[ -z "${IMAGE_SOURCE:-}" ]] && _image_source_was_unset=1
: "${IMAGE_SOURCE:=ghcr}"

# Defaults for optional knobs.
: "${GHCR_TAG:=latest}"
: "${ENABLE_CLOUD_INIT:=0}"
: "${AUTO_UPDATE_CP:=true}"
: "${SWITCH_TO_GHCR:=true}"
: "${CP_MEMORY:=8192}"
: "${CP_VCPUS:=4}"
: "${WORKER_MEMORY:=4096}"
: "${WORKER_VCPUS:=2}"
: "${POOL_DIR:=/var/lib/libvirt/images}"
: "${RUN_VERIFY:=false}"
# STRICT_CACHE=1 turns the qcow2/image freshness checks below from
# WARN+auto-rebuild into a hard-fail — the fail-closed posture a CI / boot-test
# gate wants (mirrors HBIRD_REMOTE_STRICT). Default 0 (operator-friendly). (#373)
: "${STRICT_CACHE:=0}"

# POOL_DIR preflight for the non-root path (#305 round-2 review L3). Silent
# fail-late-at-virt-install was a real UX problem pre-round-2: an operator
# who skipped the one-time chgrp/chmod would only see "could not resolve CP
# IP" ~10 min later, never the actionable hint that POOL_DIR mode was wrong.
# Fail loud now with the same chgrp/chmod recipe the EUID gate emits, so
# the operator gets one consistent recovery path.
#
# Write-probe form: directly tests what we actually need (can the operator
# create files here?) rather than parsing mode bits — robust against future
# changes to the recipe (e.g. POSIX ACLs instead of chmod 2775).
if [[ $EUID -ne 0 ]]; then
  [[ -d "$POOL_DIR" ]] || fail "POOL_DIR not a directory: $POOL_DIR (create it or set POOL_DIR= in your cluster.local.conf)"
  # mktemp -p (not $$) avoids the PID-reuse collision two reviewers
  # flagged: parallel operator invocations on a wrapped PID would
  # race on the same probe filename. mktemp also fails closed if the
  # dir isn't writable, which is the exact condition we're testing.
  if ! _pool_probe=$(mktemp -p "$POOL_DIR" .hbird-write-probe.XXXXXX 2>/dev/null); then
    fail "POOL_DIR=$POOL_DIR not writable by $(id -un). One-time setup on the KVM host:
  sudo chgrp libvirt $POOL_DIR
  sudo chmod 2775 $POOL_DIR
See docs/deploy-cluster.md#running-without-sudo-libvirt-group-operator-305."
  fi
  rm -f "$_pool_probe"
  unset _pool_probe
fi
: "${KVM_HOST:=}"
# Optional bootc-semver-update overrides. Empty = use the image default
# (timer schedule baked in containers/shared/, repo baked per-flavor at
# build time). When non-empty, cloud-init emits a write_files entry that
# overrides the image default on first boot.
: "${BOOTC_UPDATE_SCHEDULE:=}"
: "${BOOTC_UPDATE_REPO_K8S:=}"
: "${BOOTC_UPDATE_REPO_WORKER:=}"
# Match spawn-workers.sh's retry tuning so CP-readiness waits behave the same.
: "${CP_READY_RETRIES:=60}"
: "${CP_READY_SLEEP:=10}"
: "${TOKEN_TTL:=2h}"

# Default WORKER_NAMES to a 2-element array if completely unset.
# Distinguish "unset" (legacy configs that never declared WORKER_NAMES —
# preserve historical 2-worker default) from "explicit empty array"
# (`WORKER_NAMES=()` — operator's documented way to ask for CP-only;
# README's Migration table promises this honors CP-only intent).
#
# Note: bash's `${arr+x}` parameter expansion returns empty for both
# unset arrays AND empty-but-declared arrays, so it can't tell those
# apart. `declare -p` IS reliable: it exits non-zero for unset names
# and prints `declare -a NAME=()` for explicit empty arrays.
if ! declare -p WORKER_NAMES >/dev/null 2>&1; then
  log "WORKER_NAMES not set — defaulting to (${CP_NAME}-w1 ${CP_NAME}-w2)"
  WORKER_NAMES=("${CP_NAME}-w1" "${CP_NAME}-w2")
elif [[ ${#WORKER_NAMES[@]} -eq 0 ]]; then
  log "WORKER_NAMES=() — CP-only deploy (no workers)"
fi

# Hard validation.
[[ "$ENABLE_CLOUD_INIT" = "1" ]] || fail "ENABLE_CLOUD_INIT must be 1 for this flow (got '$ENABLE_CLOUD_INIT'). The deploy-cluster path requires cloud-init in the image to inject per-VM hostname + worker join + bootc switch."
[[ -r "$SSH_PUBKEY_FILE" ]] || fail "SSH_PUBKEY_FILE not readable: $SSH_PUBKEY_FILE"
case "$IMAGE_SOURCE" in
  ghcr|local) ;;
  *) fail "IMAGE_SOURCE must be 'ghcr' or 'local' (got '$IMAGE_SOURCE')" ;;
esac
case "$AUTO_UPDATE_CP" in true|false) ;; *) fail "AUTO_UPDATE_CP must be true or false (got '$AUTO_UPDATE_CP')" ;; esac
case "$SWITCH_TO_GHCR" in true|false) ;; *) fail "SWITCH_TO_GHCR must be true or false (got '$SWITCH_TO_GHCR')" ;; esac
case "$STRICT_CACHE" in 0|1) ;; *) fail "STRICT_CACHE must be 0 or 1 (got '$STRICT_CACHE')" ;; esac

# #374 guard: FORCE_REBUILD=1 + SWITCH_TO_GHCR=true auto-disables the first-boot
# GHCR switch (unless FORCE_SWITCH=1) so a freshly-built image isn't silently
# replaced by the registry's. Runs BEFORE the render functions read
# SWITCH_TO_GHCR, so both the CP and worker cloud-init inherit the resolved
# value from a single decision + WARN.
resolve_switch_to_ghcr

# #376 (folded in per bt-374 scope): warn once if BOOTC_UPDATE_SCHEDULE will
# drift this cluster off its deployed image over time.
warn_bootc_update_drift

# If IMAGE_SOURCE wasn't in the operator's config (pre-#231 was a
# `:?` hard-fail; #231 made `ghcr` the default), surface the resolution
# in the log so an operator who forgot to set `local` isn't surprised
# by a GHCR pull. Informational — `ghcr` is the documented default in
# cluster.example.conf and docs/deploy-cluster.md. (#231 round-2 review M1.)
if (( _image_source_was_unset )); then
  log "IMAGE_SOURCE unset in $CONFIG_PATH — defaulting to 'ghcr' (set IMAGE_SOURCE=local for unpublished images; GHCR_TAG=${GHCR_TAG})"
fi

SSH_PUBKEY_CONTENT="$(< "$SSH_PUBKEY_FILE")"
[[ -n "$SSH_PUBKEY_CONTENT" ]] || fail "SSH_PUBKEY_FILE is empty: $SSH_PUBKEY_FILE"

# Thread SSH_PUBKEY_FILE through to lib/build-common.sh, which reads
# SSH_PUBKEY_FILES (colon-separated). Without this export, build_qcow2 falls
# back to ~SUDO_USER/.ssh/id_ed25519.pub and gives a confusing error when the
# operator pointed at a different key.
export SSH_PUBKEY_FILES="$SSH_PUBKEY_FILE"

# #248: operator's workstation pubkey forwarded by the C3 SSH-wrap shim
# (scripts/lib/ssh-wrap.sh). Append it to SSH_PUBKEY_FILES so the CP's
# authorized_keys ends up with BOTH the KVM-host's key (used by THIS
# script to SSH to the freshly-booted CP via SSH_PRIVKEY_FILE =
# ${SSH_PUBKEY_FILE%.pub}) AND the operator's workstation key (so the
# operator can SSH to the CP directly from their workstation with their
# normal key). Quietly skipped when the var is unset (on-KVM-host
# operation, no shim involved) or when it equals SSH_PUBKEY_FILE
# (workstation and KVM host happen to share a key path AND content —
# the dedup avoids a redundant entry in authorized_keys).
if [[ -n "${HBIRD_OPERATOR_PUBKEY_FILE:-}" && -r "$HBIRD_OPERATOR_PUBKEY_FILE" ]]; then
  if [[ "$HBIRD_OPERATOR_PUBKEY_FILE" != "$SSH_PUBKEY_FILE" ]]; then
    SSH_PUBKEY_FILES="${SSH_PUBKEY_FILES}:${HBIRD_OPERATOR_PUBKEY_FILE}"
    export SSH_PUBKEY_FILES
    log "appending operator workstation pubkey to bake list: $HBIRD_OPERATOR_PUBKEY_FILE"
  fi
fi

# ---- begin extra-network-validation ---
# Optional second NIC (EXTRA_NETWORK): validate the whole knob family up
# front so a partial config fails here, not as a half-provisioned VM.
# See cluster.example.conf for the schema and the host-side prerequisite
# (the named libvirt network must exist — VF pool, bridge, whatever).
_mac_re='^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$'
# Octet-range-accurate: the loose '^[0-9.]+/[0-9]+$' this replaced accepted
# 10.0.0.256/24, 1.2.3/99 and 10.0.0.241/244 — defeating the fail-early
# guarantee this block exists to provide.
_cidr_re='^((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.){3}(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])/([0-9]|[12][0-9]|3[0-2])$'

# Unset arrays are an error under `set -u`, which aborts with a raw
# "unbound variable" instead of the crafted messages below — and an
# entirely-unset array is the MOST likely misconfiguration.
declare -p EXTRA_NET_WORKER_MACS >/dev/null 2>&1 || EXTRA_NET_WORKER_MACS=()
declare -p EXTRA_NET_WORKER_IPS  >/dev/null 2>&1 || EXTRA_NET_WORKER_IPS=()
# Primary-NIC MAC overrides (#409); optional, and indexed the same way.
declare -p WORKER_MACS >/dev/null 2>&1 || WORKER_MACS=()
declare -p WORKER_IPS  >/dev/null 2>&1 || WORKER_IPS=()

# Primary-NIC MACs, whether operator-set or name-derived, must not collide
# with each other or with the second-NIC family — one duplicate MAC on a
# segment is enough to break both VMs.
declare -A _primary_macs=()
_pm="${CP_MAC:-$(derive_primary_mac "$CP_NAME")}"
[[ "$_pm" =~ $_mac_re ]] || fail "CP_MAC is malformed (need aa:bb:cc:dd:ee:ff): '${_pm}'"
_primary_macs["${_pm,,}"]="$CP_NAME"
for _i in "${!WORKER_NAMES[@]}"; do
  _pm="${WORKER_MACS[$_i]:-$(derive_primary_mac "${WORKER_NAMES[$_i]}")}"
  [[ "$_pm" =~ $_mac_re ]] || fail "WORKER_MACS[$_i] is malformed (need aa:bb:cc:dd:ee:ff): '${_pm}'"
  [[ -z "${_primary_macs[${_pm,,}]:-}" ]] \
    || fail "primary-NIC MAC collision: ${WORKER_NAMES[$_i]} and ${_primary_macs[${_pm,,}]} would both use ${_pm} — set CP_MAC/WORKER_MACS explicitly to break the tie"
  _primary_macs["${_pm,,}"]="${WORKER_NAMES[$_i]}"
done

# POD_CIDR / SERVICE_CIDR are printf'd into cloud-init YAML and sourced as
# root by k8s-init.sh — validate whenever set, independent of EXTRA_NETWORK.
for _knob in POD_CIDR SERVICE_CIDR; do
  _v="${!_knob:-}"
  [[ -z "$_v" || "$_v" =~ $_cidr_re ]] \
    || fail "${_knob} is malformed (need CIDR like 10.244.0.0/16): '${_v}'"
done

# _ip_in_cidr <addr> <cidr> -> 0 when addr is inside cidr.
_ip_in_cidr() {
  local a="$1" c="${2%%/*}" p="${2##*/}" ai ci m o1 o2 o3 o4
  [[ "$2" == */* ]] || return 1
  local IFS=.
  read -r o1 o2 o3 o4 <<<"$a"; ai=$(( (o1<<24)|(o2<<16)|(o3<<8)|o4 ))
  read -r o1 o2 o3 o4 <<<"$c"; ci=$(( (o1<<24)|(o2<<16)|(o3<<8)|o4 ))
  (( p == 0 )) && return 0
  m=$(( (0xFFFFFFFF << (32 - p)) & 0xFFFFFFFF ))
  (( (ai & m) == (ci & m) ))
}

if [[ -n "${EXTRA_NETWORK:-}" ]]; then
  [[ "${EXTRA_NET_CP_MAC:-}" =~ $_mac_re ]] \
    || fail "EXTRA_NETWORK is set but EXTRA_NET_CP_MAC is missing/malformed (need aa:bb:cc:dd:ee:ff): '${EXTRA_NET_CP_MAC:-}'"
  [[ "${EXTRA_NET_CP_IP:-}" =~ $_cidr_re ]] \
    || fail "EXTRA_NETWORK is set but EXTRA_NET_CP_IP is missing/malformed (need CIDR like 10.0.0.241/24): '${EXTRA_NET_CP_IP:-}'"
  [[ ${#EXTRA_NET_WORKER_MACS[@]} -eq ${#WORKER_NAMES[@]} ]] \
    || fail "EXTRA_NET_WORKER_MACS has ${#EXTRA_NET_WORKER_MACS[@]} entries but WORKER_NAMES has ${#WORKER_NAMES[@]} — the arrays must be parallel"
  [[ ${#EXTRA_NET_WORKER_IPS[@]} -eq ${#WORKER_NAMES[@]} ]] \
    || fail "EXTRA_NET_WORKER_IPS has ${#EXTRA_NET_WORKER_IPS[@]} entries but WORKER_NAMES has ${#WORKER_NAMES[@]} — the arrays must be parallel"
  for _i in "${!WORKER_NAMES[@]}"; do
    [[ "${EXTRA_NET_WORKER_MACS[$_i]}" =~ $_mac_re ]] \
      || fail "EXTRA_NET_WORKER_MACS[$_i] malformed: '${EXTRA_NET_WORKER_MACS[$_i]}'"
    [[ "${EXTRA_NET_WORKER_IPS[$_i]}" =~ $_cidr_re ]] \
      || fail "EXTRA_NET_WORKER_IPS[$_i] malformed: '${EXTRA_NET_WORKER_IPS[$_i]}'"
  done

  # Uniqueness. A duplicate MAC or address is silently catastrophic: two
  # VMs on one L2 segment answering for the same identity.
  declare -A _seen_mac=() _seen_ip=()
  for _m in "${EXTRA_NET_CP_MAC}" "${EXTRA_NET_WORKER_MACS[@]}"; do
    _mk="${_m,,}"
    [[ -z "${_seen_mac[$_mk]:-}" ]] \
      || fail "duplicate MAC in the EXTRA_NET_* family: '${_m}' — every NIC needs a unique MAC"
    # Also cross-check against the primary NICs' MACs (#409).
    [[ -z "${_primary_macs[$_mk]:-}" ]] \
      || fail "EXTRA_NET MAC '${_m}' collides with the primary-NIC MAC of ${_primary_macs[$_mk]} — every NIC on the host needs a unique MAC"
    _seen_mac[$_mk]=1
  done
  for _a in "${EXTRA_NET_CP_IP}" "${EXTRA_NET_WORKER_IPS[@]}"; do
    _ak="${_a%%/*}"
    [[ -z "${_seen_ip[$_ak]:-}" ]] \
      || fail "duplicate address in the EXTRA_NET_* family: '${_ak}' — every NIC needs a unique address"
    _seen_ip[$_ak]=1
  done

  # Overlap with the cluster's own ranges — the exact collision class that
  # motivated this work (Cilium's 10.0.0.0/8 default swallowing the LAN).
  for _a in "${EXTRA_NET_CP_IP}" "${EXTRA_NET_WORKER_IPS[@]}"; do
    _bare="${_a%%/*}"
    for _range_name in POD_CIDR SERVICE_CIDR; do
      _range="${!_range_name:-}"
      [[ -n "$_range" ]] || continue
      ! _ip_in_cidr "$_bare" "$_range" \
        || fail "EXTRA_NET address ${_bare} falls inside ${_range_name}=${_range} — traffic to it would be swallowed by the cluster network. Pick ranges that overlap neither."
    done
  done

  virsh -c qemu:///system net-info "$EXTRA_NETWORK" >/dev/null 2>&1 \
    || fail "EXTRA_NETWORK='$EXTRA_NETWORK' is not a defined libvirt network on this host — define it first (see cluster.example.conf for a VF-pool example)"
  # Defined is not enough: an inactive network fails at virt-install time,
  # after the templates are built and the CP qcow2 is cloned.
  virsh -c qemu:///system net-info "$EXTRA_NETWORK" 2>/dev/null | awk '/^Active:/{print $2}' | grep -qi '^yes$' \
    || fail "EXTRA_NETWORK='$EXTRA_NETWORK' is defined but NOT active — run 'virsh net-start ${EXTRA_NETWORK}' (and net-autostart) first"
  # hostdev (SR-IOV VF) pool: ensure enough ports for 1 CP + N workers.
  # Running out otherwise surfaces as a cryptic virt-install failure on the
  # Nth VM, halfway through the deploy.
  if virsh -c qemu:///system net-dumpxml "$EXTRA_NETWORK" 2>/dev/null | grep -q "forward mode='hostdev'"; then
    _vf_count="$(virsh -c qemu:///system net-dumpxml "$EXTRA_NETWORK" 2>/dev/null | grep -c "<address type='pci'")"
    _need=$(( 1 + ${#WORKER_NAMES[@]} ))
    (( _vf_count >= _need )) \
      || fail "EXTRA_NETWORK='$EXTRA_NETWORK' is a hostdev pool with ${_vf_count} VF(s) but this deploy needs ${_need} (1 CP + ${#WORKER_NAMES[@]} worker(s))"
  fi
else
  # Guard against a half-set family: worker arrays without the master knob
  # would be silently ignored — that silence is the #373-family bug class.
  if [[ -n "${EXTRA_NET_CP_MAC:-}${EXTRA_NET_CP_IP:-}${EXTRA_NET_WORKER_MACS[*]:-}${EXTRA_NET_WORKER_IPS[*]:-}" ]]; then
    fail "EXTRA_NET_* values are set but EXTRA_NETWORK is empty — set EXTRA_NETWORK=<libvirt network> or clear the family"
  fi
fi
# ---- end extra-network-validation ---

log "config OK: CP=${CP_NAME}, workers=(${WORKER_NAMES[*]}), source=${IMAGE_SOURCE}, tag=${GHCR_TAG}${EXTRA_NETWORK:+, extra-net=${EXTRA_NETWORK}}"

# ---- begin podman-preflight-block ---
if [[ "$IMAGE_SOURCE" == "local" ]]; then
  log "pre-flight: checking for rootful podman (required for bootc-image-builder)"
  if [[ $EUID -eq 0 ]]; then
    podman info >/dev/null 2>&1
  else
    sudo podman info >/dev/null 2>&1
  fi || fail "rootful podman is unavailable. bootc-image-builder requires rootful podman to build qcow2 templates. Try: sudo dnf install -y podman (or check your rootful podman setup)."
fi
# ---- end podman-preflight-block ---

# ---- Image acquisition ------------------------------------------------------

CP_IMAGE_REF=""
WORKER_IMAGE_REF=""

case "$IMAGE_SOURCE" in
  ghcr)
    CP_IMAGE_REF="ghcr.io/aatchison/hummingbird-k8s:${GHCR_TAG}"
    WORKER_IMAGE_REF="ghcr.io/aatchison/hummingbird-k8s-worker:${GHCR_TAG}"
    # Isolation contract (issue #199): the outer `podman pull` MUST
    # land in the same graphroot build_qcow2 will look in below; bare
    # `podman pull` would land in /var/lib/containers/storage while
    # build_qcow2 uses --root $PODMAN_ROOT and BIB's --local lookup
    # would then fail image-not-found.
    mapfile -t _PODMAN_OPTS < <(podman_storage_opts)
    log "pulling ${CP_IMAGE_REF}"
    podman "${_PODMAN_OPTS[@]}" pull "$CP_IMAGE_REF"
    log "pulling ${WORKER_IMAGE_REF}"
    podman "${_PODMAN_OPTS[@]}" pull "$WORKER_IMAGE_REF"
    # Freshness gate (#373): GHCR `:latest` lags HEAD. If the pulled image was
    # built from a commit that predates the on-disk Containerfile, this deploy
    # would silently boot-test the PUBLISHED bits, not the operator's change —
    # the false-positive-green that surfaced in PR #367. Warn loudly; under
    # STRICT_CACHE=1 (CI/boot-test) refuse outright. The gate acts ONLY on a
    # CONFIRMED drift (image revision known AND the Containerfile changed
    # since it); an unverifiable image — today's default, no revision label —
    # reuses silently in both modes (#373 round-2). No local rebuild path here
    # — the recovery is IMAGE_SOURCE=local FORCE_REBUILD=1 (issue #373 workaround).
    _ghcr_stale=0
    hbird_assess_ghcr_image "$CP_IMAGE_REF" "control-plane image" \
      "${REPO_ROOT}/containers/k8s/Containerfile" || _ghcr_stale=1
    hbird_assess_ghcr_image "$WORKER_IMAGE_REF" "worker image" \
      "${REPO_ROOT}/containers/k8s-worker/Containerfile" || _ghcr_stale=1
    (( _ghcr_stale )) && fail "STRICT_CACHE=1: refusing to deploy a GHCR image whose revision predates the on-disk Containerfile (see ERROR above). Rebuild from source with IMAGE_SOURCE=local FORCE_REBUILD=1."
    ;;
  local)
    CP_IMAGE_REF="localhost/hummingbird-k8s:latest"
    WORKER_IMAGE_REF="localhost/hummingbird-k8s-worker:latest"
    log "building local images with ENABLE_CLOUD_INIT=1"
    ENABLE_CLOUD_INIT=1 make -C "$REPO_ROOT" image-k8s-with-cloud-init
    ENABLE_CLOUD_INIT=1 make -C "$REPO_ROOT" image-worker-with-cloud-init
    ;;
esac

# ---- bib qcow2 per flavor ---------------------------------------------------
# Reuse lib/build-common.sh's render_bib_config + build_qcow2. We render
# once per flavor; the resulting qcow2 lives under POOL_DIR/<NAME>.qcow2
# and gets cloned per-VM below.
#
# #310: route both BIB config files through mktemp instead of REPO_ROOT.
# Pre-#310 these were written to ${REPO_ROOT}/bib-config-deploy-{cp,worker}.toml,
# which (a) leaked deploy state into the operator's git checkout (showed
# up in `git status` every deploy) and (b) on any host that had previously
# run `sudo bash scripts/deploy-cluster.sh` left the files owned by
# root:root mode 0644 — which then blocked subsequent non-root deploys
# (HBIRD_REMOTE_NO_SUDO=1, the no-sudo libvirt-group operator path #305
# added) at the rewrite with Permission denied. mktemp paths sidestep
# both problems: per-deploy fresh paths owned by the invoking user, no
# REPO_ROOT side-effects. Tracked in cleanup_on_failure + rm -f'd after
# build_qcow2 consumes them (they aren't needed past that point).
BIB_CFG_CP="$(mktemp -t bib-config-deploy-cp.XXXXXX.toml)"
BIB_CFG_WORKER="$(mktemp -t bib-config-deploy-worker.XXXXXX.toml)"
BIB_CFG_TEMPS+=("$BIB_CFG_CP" "$BIB_CFG_WORKER")

# The bib config bakes ONE static pubkey set; render once and reuse.
render_bib_config > "$BIB_CFG_CP"
cp "$BIB_CFG_CP" "$BIB_CFG_WORKER"

# Template names live under POOL_DIR; clone per-VM below so the operator
# can redeploy individual nodes without rebuilding the whole template.
CP_TEMPLATE_NAME="hummingbird-k8s-deploy"
WORKER_TEMPLATE_NAME="hummingbird-k8s-worker-deploy"

# build_template: wrap build_qcow2 with the #373 qcow2-template cache check.
# build_qcow2 has a skip-if-exists shortcut that would otherwise reuse a stale
# template even when the source it was baked from has changed. We record the
# build identity in a sidecar (lib/cache-utils.sh) and compare on the next run:
#   * fresh            -> reuse (build_qcow2 skips as before)
#   * stale/unknown    -> force a rebuild for THIS template only (FORCE_REBUILD
#                         is scoped to the build_qcow2 call so the other flavor
#                         is judged independently)
#   * stale + STRICT_CACHE=1 -> hard-fail
# The build identity is source-namespaced (`<source>:<id>`): the pulled
# image's vcs-ref (ghcr) or the on-disk Containerfile content hash (local).
# An unverifiable id (e.g. a GHCR image with no revision label) yields an empty
# build_id, which hbird_assess_qcow2_cache treats as "cannot confirm -> reuse"
# and hbird_cache_write_ref declines to record — so the default ghcr path keeps
# build_qcow2's skip-if-exists fast path intact. (#373 round-2.)
# $1=image-ref $2=template-name $3=bib-cfg $4=containerfile $5=label
build_template() {
  local image_ref="$1" name="$2" cfg="$3" containerfile="$4" label="$5"
  local qcow="${POOL_DIR}/${name}.qcow2"
  local build_id force_this rc=0
  if [[ "$IMAGE_SOURCE" == "ghcr" ]]; then
    build_id="$(hbird_cache_build_id ghcr "$(hbird_image_vcs_ref "$image_ref")")"
  else
    # Local build identity is a composite of the Containerfile content,
    # the BASE_IMAGE, the rendered bib-config, and the cloud-init flag.
    local composite_id
    composite_id="$(printf '%s\n%s\n%s\n%s' \
      "$(hbird_containerfile_ref "$containerfile")" \
      "${BASE_IMAGE:-}" \
      "$(render_bib_config)" \
      "${ENABLE_CLOUD_INIT:-0}" \
      | sha256sum | cut -c1-12)"
    build_id="$(hbird_cache_build_id local "$composite_id")"
  fi
  force_this="${FORCE_REBUILD:-}"
  hbird_assess_qcow2_cache "$qcow" "$build_id" "$label" || rc=$?
  case "$rc" in
    3) fail "STRICT_CACHE=1: cached ${label} is stale (see ERROR above). Rebuild with FORCE_REBUILD=1, or unset STRICT_CACHE to auto-rebuild." ;;
    10) force_this=1 ;;
  esac
  log "building ${label} (qcow2 template)"
  FORCE_REBUILD="$force_this" build_qcow2 "$image_ref" "$name" "$cfg"
  # Record what we just baked so the next deploy can detect drift. Best-effort:
  # a POOL_DIR write failure must not abort an otherwise-successful build.
  hbird_cache_write_ref "$qcow" "$build_id" \
    || log "WARN: could not record cache build-ref sidecar for ${qcow} (cache freshness check will rebuild next run)"
}

build_template "$CP_IMAGE_REF" "$CP_TEMPLATE_NAME" "$BIB_CFG_CP" \
  "${REPO_ROOT}/containers/k8s/Containerfile" "CP image"
build_template "$WORKER_IMAGE_REF" "$WORKER_TEMPLATE_NAME" "$BIB_CFG_WORKER" \
  "${REPO_ROOT}/containers/k8s-worker/Containerfile" "worker image"

# #310: BIB config tempfiles have served their purpose (build_qcow2
# consumed them; they aren't read again). Drop them now so a successful
# deploy doesn't leave litter in $TMPDIR. The failure-trap above covers
# the abnormal-exit path.
rm -f "$BIB_CFG_CP" "$BIB_CFG_WORKER"
BIB_CFG_TEMPS=()

CP_TEMPLATE_QCOW="${POOL_DIR}/${CP_TEMPLATE_NAME}.qcow2"
WORKER_TEMPLATE_QCOW="${POOL_DIR}/${WORKER_TEMPLATE_NAME}.qcow2"

# ---- CP cloud-init user-data + seed -----------------------------------------

CP_QCOW="${POOL_DIR}/${CP_NAME}.qcow2"
CP_SEED="${POOL_DIR}/${CP_NAME}-seed.iso"
CP_USER_DATA="$(mktemp -t hbird-cp-userdata-XXXXXX.yaml)"

# render_cp_user_data is defined near the top of this script (above the
# root/sudo checks) so the source-only test guard can expose it.
render_cp_user_data > "$CP_USER_DATA"

# Second NIC: ship a network-config in the seed so cloud-init-local
# configures both NICs BEFORE NetworkManager starts (no DHCP race, no
# second default route — see render_net2_network_config).
CP_NET_CONFIG=""
if [[ -n "${EXTRA_NETWORK:-}" ]]; then
  CP_NET_CONFIG="$(mktemp -t hbird-cp-netconfig-XXXXXX.yaml)"
  render_net2_network_config "$EXTRA_NET_CP_MAC" "$EXTRA_NET_CP_IP" > "$CP_NET_CONFIG"
fi

log "building CP cloud-init seed ${CP_SEED}"
build_cloud_init_seed "$CP_NAME" "$CP_USER_DATA" "$CP_SEED" "$CP_NET_CONFIG"
SEED_ISOS+=("$CP_SEED")
rm -f "$CP_USER_DATA" ${CP_NET_CONFIG:+"$CP_NET_CONFIG"}

# ---- Stage CP qcow2 + virt-install ------------------------------------------

if virsh -c qemu:///system dominfo "$CP_NAME" >/dev/null 2>&1; then
  fail "CP VM '$CP_NAME' is already defined — refusing to overwrite. Tear down with 'virsh destroy/undefine $CP_NAME' (or 'sudo make clean-vms') and retry."
fi

log "cloning CP qcow2 -> $CP_QCOW"
cp --reflink=auto "$CP_TEMPLATE_QCOW" "$CP_QCOW"
# chown root:root + chmod 0644 only apply when running as root — a
# libvirt-group operator can't chown to root (EPERM) and the cloned file
# already inherits sensible perms from cp + POOL_DIR's setgid. Libvirt's
# dynamic_ownership chowns to qemu:qemu at VM start either way. (#305
# round-2 review L7 — wrapping chmod in the same EUID guard as chown.)
if [[ $EUID -eq 0 ]]; then
  chown root:root "$CP_QCOW"
  chmod 0644 "$CP_QCOW"
fi

# Optional second NIC. mac= pins the guest-visible MAC so the seed's
# network-config (matched by MAC) deterministically finds the right
# interface. For a hostdev/VF-pool network libvirt ignores model=, so
# none is passed. Empty array when the knob is off — expands to nothing.
CP_EXTRA_NET_ARGS=()
if [[ -n "${EXTRA_NETWORK:-}" ]]; then
  CP_EXTRA_NET_ARGS=(--network "network=${EXTRA_NETWORK},mac=${EXTRA_NET_CP_MAC}")
fi

# Primary NIC MAC: deterministic from the domain name so a rebuilt VM
# keeps its DHCP lease, which is what the pinned kubelet --node-ip depends
# on (#409). Overridable per-cluster via CP_MAC.
CP_PRIMARY_MAC="${CP_MAC:-$(derive_primary_mac "$CP_NAME")}"
ensure_dhcp_reservation default "$CP_PRIMARY_MAC" "${CP_IP:-}" "$CP_NAME"

log "virt-install ${CP_NAME} (memory=${CP_MEMORY} vcpus=${CP_VCPUS}, primary mac=${CP_PRIMARY_MAC})"
virt-install --connect qemu:///system \
  --name "$CP_NAME" \
  --memory "$CP_MEMORY" --vcpus "$CP_VCPUS" \
  --disk "$CP_QCOW",format=qcow2,bus=virtio \
  --disk path="$CP_SEED",device=cdrom,readonly=on \
  --import \
  --os-variant fedora-unknown \
  --network "network=default,model=virtio,mac=${CP_PRIMARY_MAC}" \
  "${CP_EXTRA_NET_ARGS[@]}" \
  --graphics vnc,listen=127.0.0.1 \
  --noautoconsole

# ---- Wait for CP Ready ------------------------------------------------------

log "waiting for CP IP to appear via DHCP..."
CP_IP="$(resolve_vm_ip "$CP_NAME" 60 5 || true)"
[[ -n "$CP_IP" ]] || fail "could not resolve CP IP after ~5 minutes — inspect 'virsh -c qemu:///system console $CP_NAME'"
log "CP IP: $CP_IP"

# SSH to the CP using the operator's private key explicitly. Nested sudo
# (sudo make -> sudo bash) resets SUDO_USER to root, breaking the previous
# `sudo -u $SUDO_USER ssh` approach. Pin the identity to the private key
# paired with SSH_PUBKEY_FILE — which is the same key bib bakes into root's
# authorized_keys.
# shellcheck disable=SC2034  # SSH_PRIVKEY_FILE is read by ssh_opts_array (lib/build-common.sh)
SSH_PRIVKEY_FILE="$(derive_ssh_privkey_file "$SSH_PUBKEY_FILE")" \
  || fail "SSH private key not readable next to $SSH_PUBKEY_FILE"
ssh_opts_array CP_SSH_OPTS
cp_ssh() { ssh "${CP_SSH_OPTS[@]}" "root@${CP_IP}" "$@"; }

log "polling 'kubectl get nodes' on CP until it reports Ready (max ~$((CP_READY_RETRIES * CP_READY_SLEEP))s)"
CP_READY=0
for attempt in $(seq 1 "$CP_READY_RETRIES"); do
  if cp_ssh "kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes --no-headers 2>/dev/null | awk '\$2==\"Ready\"' | grep -q ." ; then
    CP_READY=1
    log "CP Ready (after ${attempt} attempt(s))"
    break
  fi
  log "CP not Ready yet (attempt ${attempt}/${CP_READY_RETRIES}); sleeping ${CP_READY_SLEEP}s"
  sleep "$CP_READY_SLEEP"
done
(( CP_READY == 1 )) || fail "CP never reached Ready — inspect 'ssh root@${CP_IP} journalctl -u k8s-init.service'"

# ---- Mint kubeadm join token from the live CP -------------------------------

log "minting ${TOKEN_TTL}-TTL kubeadm join token from CP"
JOIN_CMD="$(cp_ssh "kubeadm token create --ttl ${TOKEN_TTL} --print-join-command" 2>/dev/null || true)"
[[ "$JOIN_CMD" == kubeadm\ join* ]] || fail "did not get a valid 'kubeadm join ...' command from the CP. Got: ${JOIN_CMD:-<empty>}"

# ---- Per-worker seed + virt-install (parallel) ------------------------------
# worker_user_data is defined near the top of this script (above the
# source-only guard) so tests/scripts/deploy-cluster.bats can exercise the
# rendered worker user-data without invoking the rest of the script. (#254.)

WORKER_PIDS=()
for w_idx in "${!WORKER_NAMES[@]}"; do
  w="${WORKER_NAMES[$w_idx]}"
  if virsh -c qemu:///system dominfo "$w" >/dev/null 2>&1; then
    fail "worker VM '$w' is already defined — refusing to overwrite. Tear down first."
  fi

  w_qcow="${POOL_DIR}/${w}.qcow2"
  w_seed="${POOL_DIR}/${w}-seed.iso"
  w_userdata="$(mktemp -t hbird-w-userdata-XXXXXX.yaml)"

  # Second NIC (see the CP block above for the mechanism/rationale).
  w_netconfig=""
  w_extra_net_args=()
  if [[ -n "${EXTRA_NETWORK:-}" ]]; then
    w_netconfig="$(mktemp -t hbird-w-netconfig-XXXXXX.yaml)"
    render_net2_network_config \
      "${EXTRA_NET_WORKER_MACS[$w_idx]}" "${EXTRA_NET_WORKER_IPS[$w_idx]}" > "$w_netconfig"
    w_extra_net_args=(--network "network=${EXTRA_NETWORK},mac=${EXTRA_NET_WORKER_MACS[$w_idx]}")
  fi

  worker_user_data "$w" "$w_userdata" "${EXTRA_NETWORK:+${EXTRA_NET_WORKER_MACS[$w_idx]}}"
  build_cloud_init_seed "$w" "$w_userdata" "$w_seed" "$w_netconfig"
  SEED_ISOS+=("$w_seed")
  rm -f "$w_userdata" ${w_netconfig:+"$w_netconfig"}

  log "cloning worker qcow2 -> $w_qcow"
  cp --reflink=auto "$WORKER_TEMPLATE_QCOW" "$w_qcow"
  # See CP qcow2 block above for the EUID-conditional chown+chmod rationale.
  if [[ $EUID -eq 0 ]]; then
    chown root:root "$w_qcow"
    chmod 0644 "$w_qcow"
  fi

  # Primary NIC MAC + optional reservation — see the CP block (#409).
  w_primary_mac="${WORKER_MACS[$w_idx]:-$(derive_primary_mac "$w")}"
  ensure_dhcp_reservation default "$w_primary_mac" "${WORKER_IPS[$w_idx]:-}" "$w"

  log "virt-install ${w} (memory=${WORKER_MEMORY} vcpus=${WORKER_VCPUS}, primary mac=${w_primary_mac}) [bg]"
  virt-install --connect qemu:///system \
    --name "$w" \
    --memory "$WORKER_MEMORY" --vcpus "$WORKER_VCPUS" \
    --disk "$w_qcow",format=qcow2,bus=virtio \
    --disk path="$w_seed",device=cdrom,readonly=on \
    --import \
    --os-variant fedora-unknown \
    --network "network=default,model=virtio,mac=${w_primary_mac}" \
    "${w_extra_net_args[@]}" \
    --graphics vnc,listen=127.0.0.1 \
    --noautoconsole &
  WORKER_PIDS+=("$!")
done

# Wait for all worker virt-installs to return. virt-install --noautoconsole
# returns once the domain is defined + started — actual k8s join happens
# inside the guest and is observed by the readiness poll below.
log "waiting for ${#WORKER_PIDS[@]} virt-install processes to settle"
WAIT_RC=0
for pid in "${WORKER_PIDS[@]}"; do
  wait "$pid" || WAIT_RC=$?
done
(( WAIT_RC == 0 )) || fail "one or more worker virt-installs failed (rc=${WAIT_RC})"

# ---- Wait for full cluster Ready --------------------------------------------
# Asserts each expected node by NAME rather than an aggregate count — a stray
# or duplicate node cannot satisfy the count while a named worker join failed.
# ---- begin cluster-ready-poll ---
_CLUSTER_POLL_NODES=("$CP_NAME" "${WORKER_NAMES[@]}")
log "polling cluster until all ${#_CLUSTER_POLL_NODES[@]} named nodes Ready: ${_CLUSTER_POLL_NODES[*]}"
CLUSTER_READY=0
for attempt in $(seq 1 "$CP_READY_RETRIES"); do
  _all_ready=1
  for _node in "${_CLUSTER_POLL_NODES[@]}"; do
    _node_ok="$(cp_ssh "kubectl --kubeconfig=/etc/kubernetes/admin.conf get node '${_node}' --no-headers 2>/dev/null | awk '\$2==\"Ready\"{print \"yes\"}'" 2>/dev/null || true)"
    if [[ "${_node_ok//[[:space:]]/}" != "yes" ]]; then
      _all_ready=0
      log "node '${_node}' not Ready (attempt ${attempt}/${CP_READY_RETRIES})"
      break
    fi
  done
  if (( _all_ready == 1 )); then
    CLUSTER_READY=1
    log "cluster Ready: all ${#_CLUSTER_POLL_NODES[@]} named nodes Ready (attempt ${attempt}/${CP_READY_RETRIES})"
    break
  fi
  sleep "$CP_READY_SLEEP"
done
# ---- end cluster-ready-poll ---
(( CLUSTER_READY == 1 )) || fail "cluster never reached Ready for all named nodes (${_CLUSTER_POLL_NODES[*]}) — inspect 'ssh root@${CP_IP} kubectl get nodes' and per-worker 'journalctl -u worker-init.service'"

# ---- Optional verification --------------------------------------------------

if [[ "$RUN_VERIFY" = "true" ]]; then
  # Cross-runtime dependency (v0.1.0 cutover, #353):
  # hbird CLI is required at runtime for the verify-app-deploy step.
  # scripts/verify-app-deploy.sh was removed in v0.1.0; the Rust twin
  # (`hbird verify app-deploy`) is the canonical implementation.
  # If hbird is not on PATH, this step will fail with command-not-found.
  #
  # PR #366 round-2 H3: pass --config / --cp-ip / --kvm-host explicitly
  # rather than relying on env propagation (CP_IP isn't exported from
  # the local block scope above, and a bare `hbird verify app-deploy`
  # would re-resolve via virsh-domifaddr — slower + relies on KVM_HOST
  # being a usable SSH alias from this host). Explicit flags also make
  # the intent grep-able in the deploy log.
  #
  # #9 item (a): when STRICT_CACHE=1 both missing-hbird and verifier
  # non-zero are fatal — the strict gate must be able to fail the deploy.
  # The non-strict (default) path is unchanged (informational).
  # ---- begin run-verify-block ---
  if command -v hbird >/dev/null 2>&1; then
    log "running hbird verify app-deploy"
    _verify_rc=0
    hbird verify app-deploy \
      --config "$CONFIG_PATH" \
      --cp-ip "$CP_IP" \
      --kvm-host "${KVM_HOST:-}" || _verify_rc=$?
    if (( _verify_rc != 0 )); then
      if [[ "${STRICT_CACHE:-0}" == "1" ]]; then
        fail "hbird verify app-deploy exited non-zero (rc=${_verify_rc}) — deploy fails under STRICT_CACHE=1"
      else
        log "hbird verify app-deploy exited non-zero (cluster is up; verifier failure is informational)"
      fi
    fi
  else
    if [[ "${STRICT_CACHE:-0}" == "1" ]]; then
      fail "RUN_VERIFY=true but \`hbird\` CLI not found on PATH (required under STRICT_CACHE=1; install per docs/rust-cli.md)"
    else
      log "RUN_VERIFY=true but \`hbird\` CLI not found on PATH; skipping (install per docs/rust-cli.md)"
    fi
  fi
  # ---- end run-verify-block ---
fi

# ---- Summary ----------------------------------------------------------------

log ""
log "=============================================================="
log "Cluster deployed."
log "  CP:         ${CP_NAME} (${CP_IP})"
log "  Workers:    ${WORKER_NAMES[*]}"
log "  Image src:  ${IMAGE_SOURCE} (tag=${GHCR_TAG})"
log "  Kubeconfig: root@${CP_IP}:/etc/kubernetes/admin.conf"
if [[ -n "$KVM_HOST" ]]; then
  log "  Remote access:"
  log "    KVM_HOST=${KVM_HOST} hbird kubectl get nodes   # (post-#353; kubectl-k8s.sh removed in v0.1.0)"
else
  log "  Local access (on this KVM host):"
  log "    ssh root@${CP_IP} kubectl get nodes"
fi
log "=============================================================="

# Disarm the failure-trap cleanup so the seed ISOs persist for libvirt
# (a `virsh start` after `destroy` re-attaches them).
trap - EXIT
