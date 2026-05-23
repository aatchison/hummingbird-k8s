#!/usr/bin/env bash
# scripts/lib/cloud-init-seed.sh — Build a NoCloud seed ISO from a
# user-data file + a synthesized meta-data blob. Sourced by
# scripts/deploy-cluster.sh; not meant to be executed directly.
#
# Why a separate helper:
#   - virt-install's --cloud-init builds an ephemeral seed under
#     /var/lib/libvirt/qemu/nvram and removes it on shutdown. We want a
#     persistent seed on disk so a `virsh start` after `virsh destroy`
#     still has its NoCloud datasource — otherwise cloud-init would
#     re-run with a different / missing instance-id and the worker join
#     token would vanish.
#   - Centralizing the cloud-localds vs. genisoimage fallback in one
#     place keeps deploy-cluster.sh focused on orchestration.
#
# Public surface:
#
#   build_cloud_init_seed <hostname> <user-data-file> <out-iso>
#       hostname        — becomes meta-data's local-hostname.
#       user-data-file  — path to a #cloud-config YAML file.
#       out-iso         — destination ISO path; parent dir must exist.
#
# The synthesized meta-data has a unique instance-id of
# hbird-<epoch>-<random> so re-running the deploy regenerates a fresh
# instance-id and cloud-init re-applies the user-data. (cloud-init's
# "skip on subsequent boots with the same instance-id" rule would
# otherwise make redeploys silently no-op.)

# shellcheck shell=bash

# build_cloud_init_seed <hostname> <user-data-file> <out-iso>
build_cloud_init_seed() {
  local hostname="$1" user_data="$2" out_iso="$3"

  if [[ -z "$hostname" || -z "$user_data" || -z "$out_iso" ]]; then
    echo "build_cloud_init_seed: usage: build_cloud_init_seed <hostname> <user-data-file> <out-iso>" >&2
    return 2
  fi
  if [[ ! -r "$user_data" ]]; then
    echo "build_cloud_init_seed: user-data file not readable: $user_data" >&2
    return 2
  fi

  local tmp
  tmp="$(mktemp -d -t hbird-ci-XXXXXX)"
  # Local cleanup; the caller owns the output ISO. Build the trap string
  # with the path pre-expanded but quoted into single-quotes so the trap
  # handler itself does no further expansion. shellcheck-clean (SC2064).
  # shellcheck disable=SC2064
  trap "rm -rf -- '${tmp}'" RETURN

  cp "$user_data" "$tmp/user-data"
  cat > "$tmp/meta-data" <<EOF
instance-id: hbird-$(date +%s)-${RANDOM}
local-hostname: ${hostname}
EOF

  if command -v cloud-localds >/dev/null 2>&1; then
    if ! cloud-localds "$out_iso" "$tmp/user-data" "$tmp/meta-data" >/dev/null 2>&1; then
      echo "build_cloud_init_seed: cloud-localds failed for ${hostname} -> ${out_iso}" >&2
      return 1
    fi
  elif command -v genisoimage >/dev/null 2>&1; then
    if ! genisoimage -output "$out_iso" -volid cidata -joliet -rock \
        "$tmp/user-data" "$tmp/meta-data" >/dev/null 2>&1; then
      echo "build_cloud_init_seed: genisoimage failed for ${hostname} -> ${out_iso}" >&2
      return 1
    fi
  elif command -v mkisofs >/dev/null 2>&1; then
    if ! mkisofs -output "$out_iso" -volid cidata -joliet -rock \
        "$tmp/user-data" "$tmp/meta-data" >/dev/null 2>&1; then
      echo "build_cloud_init_seed: mkisofs failed for ${hostname} -> ${out_iso}" >&2
      return 1
    fi
  else
    echo "build_cloud_init_seed: need one of cloud-localds / genisoimage / mkisofs on PATH" >&2
    echo "build_cloud_init_seed: install cloud-utils (provides cloud-localds) or genisoimage on this KVM host and retry." >&2
    return 1
  fi
}
