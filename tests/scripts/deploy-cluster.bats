#!/usr/bin/env bats
#
# Unit tests for scripts/deploy-cluster.sh — covering both:
#
# 1. The WORKER_NAMES resolution block (PR #219 round-2 H1).
#
#    README's Migration table promises that `WORKER_NAMES=()` in
#    cluster.local.conf yields a CP-only deploy. The original block
#    treated empty arrays the same as "unset" and silently filled in
#    two default workers, contradicting the README. These tests pin the
#    three-state behavior:
#
#      a. unset             — defaults to (${CP_NAME}-w1 ${CP_NAME}-w2)  [legacy]
#      b. WORKER_NAMES=()   — honored as explicit CP-only intent
#      c. WORKER_NAMES=(…)  — used verbatim
#
#    We can't invoke deploy-cluster.sh end-to-end here (it asserts EUID==0
#    and runs virt-install). Instead, we extract just the resolver block
#    and source it from a harness that supplies the inputs. Keeping the
#    tested code as a literal extract (not a paraphrase) makes the test
#    meaningful: any future edit to the block has to be mirrored here.
#
# 2. The render_cp_user_data function (PR #181 round-2).
#
#    These tests focus on the render_cp_user_data function — extracted from
#    the inline `{ ... } > $CP_USER_DATA` block so we can exercise the
#    AUTO_UPDATE_CP true/false branches without invoking the rest of the
#    script (which requires root + libvirt + bib).
#
#    The script supports a HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY=1 mode that
#    returns from `source` after defining the render function — see the
#    guard near the top of deploy-cluster.sh. That sentinel was added by
#    this same PR (#181 round-2) explicitly so this test could exist.
#
#    Coverage:
#      1. AUTO_UPDATE_CP=true emits enable bootc-semver-update.timer AND
#         disable bootc-fetch-apply-updates.timer.
#      2. AUTO_UPDATE_CP=false emits a disable for bootc-semver-update.timer
#         (the regression #181 round-2 fixes: false used to be a no-op, but
#         the preset enables the timer unconditionally on factory reset).
#      3. SWITCH_TO_GHCR=true / false emits / omits the bootc switch line.
#      4. BOOTC_UPDATE_SCHEDULE emits a write_files override + a restart of
#         the timer in runcmd.
#
# Run via:
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/scripts/deploy-cluster.bats
# OR locally:
#   bats tests/scripts/deploy-cluster.bats

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/deploy-cluster.sh"

  # ---- For #181 render_cp_user_data tests ---------------------------------
  # All tests using `render` (below) run in source-only mode — the script
  # returns from source immediately after defining render_cp_user_data when
  # this env var is 1. No root / libvirt / bib calls happen.
  export HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY=1
  # Minimal env render_cp_user_data needs. Tests override per-case.
  export CP_NAME=hbird-cp1
  export SSH_PUBKEY_CONTENT="ssh-ed25519 AAAA-test-key user@host"
  export GHCR_TAG=v0.4.2
  export SWITCH_TO_GHCR=true
  export AUTO_UPDATE_CP=true
  export BOOTC_UPDATE_SCHEDULE=""
  export BOOTC_UPDATE_REPO_K8S=""
  export POD_CIDR=""
  export SERVICE_CIDR=""

  # ---- For #219 WORKER_NAMES resolver tests -------------------------------
  HARNESS="${BATS_TEST_TMPDIR}/resolve.sh"
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  # Extract the resolver block from deploy-cluster.sh by line markers so
  # the harness stays in lockstep with the script. The block we want
  # spans from the comment "Default WORKER_NAMES" through the matching
  # `fi`. awk grabs everything between the start marker and the next
  # `^fi$` at column 0.
  awk '
    /^# Default WORKER_NAMES/ {capture=1}
    capture {print}
    capture && /^fi$/ {exit}
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/resolver.snippet"

  # Sanity-check the extraction found something — if a refactor moved
  # the comment marker, fail loudly rather than silently passing an
  # empty resolver.
  [ -s "${BATS_TEST_TMPDIR}/resolver.snippet" ] || {
    echo "FATAL: failed to extract WORKER_NAMES resolver block from ${SCRIPT}" >&2
    return 1
  }

  # Extract the IMAGE_SOURCE default+validation block for #231 tests.
  # Spans from the IMAGE_SOURCE default-assign line through the matching
  # `esac` of the validation case. The block we want is the single line
  # `: "${IMAGE_SOURCE:=ghcr}"` plus the multi-line `case "$IMAGE_SOURCE"
  # in ... esac` that follows.
  #
  # awk regex avoids `{`/`}` literals (some awk flavors treat them as
  # interval quantifiers — busybox-awk and POSIX-strict gawk both reject
  # `\{` in a regex). We anchor on the unique substring `IMAGE_SOURCE:=ghcr`
  # via `index()` instead, then on the literal `case "$IMAGE_SOURCE"` line
  # to start emitting the case block.
  awk '
    index($0, "IMAGE_SOURCE:=ghcr") { capture=1; print; next }
    capture && $0 ~ /^case "\$IMAGE_SOURCE"/ { in_case=1; print; next }
    capture && in_case { print }
    capture && in_case && $0 ~ /^esac$/ { exit }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/image-source.snippet"
  [ -s "${BATS_TEST_TMPDIR}/image-source.snippet" ] || {
    echo "FATAL: failed to extract IMAGE_SOURCE block from ${SCRIPT}" >&2
    return 1
  }

  # Extract the CLI-env precedence block for #377 tests. After the #2 fix,
  # the snapshot (HBIRD_CLI_OVERRIDE_KNOBS array + capture loop) lives at the
  # TOP of the script (before any lib sources) while the source "$CONFIG_PATH"
  # + restore loop stay further down. Extract both separately and concatenate
  # so drivers can source a single snippet that exercises the full precedence
  # chain as a unit (snapshot -> source CONFIG -> restore).
  awk '
    index($0, "HBIRD_CLI_OVERRIDE_KNOBS=(") { capture=1 }
    capture { print }
    capture && index($0, "end CLI-env snapshot") { exit }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/cli-snapshot.snippet"
  [ -s "${BATS_TEST_TMPDIR}/cli-snapshot.snippet" ] || {
    echo "FATAL: failed to extract CLI-env snapshot block from ${SCRIPT}" >&2
    return 1
  }
  awk '
    /^source "/ && /CONFIG_PATH/ { capture=1 }
    capture { print }
    capture && index($0, "end CLI-env precedence") { exit }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/cli-restore.snippet"
  [ -s "${BATS_TEST_TMPDIR}/cli-restore.snippet" ] || {
    echo "FATAL: failed to extract CLI-env restore block from ${SCRIPT}" >&2
    return 1
  }
  cat "${BATS_TEST_TMPDIR}/cli-snapshot.snippet" \
      "${BATS_TEST_TMPDIR}/cli-restore.snippet" \
      > "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
  [ -s "${BATS_TEST_TMPDIR}/cli-precedence.snippet" ] || {
    echo "FATAL: failed to assemble cli-precedence.snippet from ${SCRIPT}" >&2
    return 1
  }

  # Extract the run-verify block for #9 item (a) tests. The block spans from
  # "# ---- begin run-verify-block ---" through "# ---- end run-verify-block ---"
  # (exclusive of the marker lines). Tests source it with mock log/fail/hbird.
  awk '
    /# ---- begin run-verify-block ---/ { capture=1; next }
    capture && /# ---- end run-verify-block ---/ { exit }
    capture { print }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/run-verify.snippet"
  [ -s "${BATS_TEST_TMPDIR}/run-verify.snippet" ] || {
    echo "FATAL: failed to extract run-verify-block from ${SCRIPT}" >&2
    return 1
  }

  # Extract the podman-preflight block for #4 tests.
  awk '
    /# ---- begin podman-preflight-block ---/ { capture=1; next }
    capture && /# ---- end podman-preflight-block ---/ { exit }
    capture { print }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/podman-preflight.snippet"
  [ -s "${BATS_TEST_TMPDIR}/podman-preflight.snippet" ] || {
    echo "FATAL: failed to extract podman-preflight-block from ${SCRIPT}" >&2
    return 1
  }

  # Extract the cluster-ready poll block for #9 tests. The block spans from
  # "# ---- begin cluster-ready-poll ---" through "# ---- end cluster-ready-poll ---"
  # (exclusive of the marker lines). Tests source it with a mock cp_ssh() so
  # the per-node check can be driven without a real cluster.
  awk '
    /# ---- begin cluster-ready-poll ---/ { capture=1; next }
    capture && /# ---- end cluster-ready-poll ---/ { exit }
    capture { print }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/ready-poll.snippet"
  [ -s "${BATS_TEST_TMPDIR}/ready-poll.snippet" ] || {
    echo "FATAL: failed to extract cluster-ready-poll block from ${SCRIPT}" >&2
    return 1
  }

  cat > "$HARNESS" <<'HARNESS_EOF'
#!/usr/bin/env bash
set -euo pipefail
# Minimal log() shim — the resolver block calls log(), and we don't
# want to drag in lib/build-common.sh just to print.
log() { printf 'log: %s\n' "$*"; }
: "${CP_NAME:=hbird-cp1}"
# shellcheck disable=SC1091
source "$1"
printf 'count=%d\n' "${#WORKER_NAMES[@]}"
printf 'names=%s\n' "${WORKER_NAMES[*]:-}"
HARNESS_EOF
  chmod +x "$HARNESS"
}

# Helper: source the script (which defines render_cp_user_data) and emit
# the rendered cloud-init output to stdout.
render() {
  # shellcheck disable=SC1090
  source "$SCRIPT"
  render_cp_user_data
}

# ---------------------------------------------------------------------------
# WORKER_NAMES resolution — three-state behavior (#219 H1)
# ---------------------------------------------------------------------------

@test "deploy-cluster: WORKER_NAMES unset -> legacy 2-worker default" {
  # Don't pre-set WORKER_NAMES; the resolver should fill it in.
  run env -u WORKER_NAMES CP_NAME=hbird-cp1 \
    bash "$HARNESS" "${BATS_TEST_TMPDIR}/resolver.snippet"
  [ "$status" -eq 0 ]
  [[ "$output" == *"count=2"* ]]
  [[ "$output" == *"names=hbird-cp1-w1 hbird-cp1-w2"* ]]
  # Operator-visible log line still emitted on the "unset" path.
  [[ "$output" == *"WORKER_NAMES not set"* ]]
}

@test "deploy-cluster: WORKER_NAMES=() -> CP-only (zero workers)" {
  # Simulate cluster.local.conf doing `WORKER_NAMES=()`. Arrays don't
  # cross process boundaries via env, so write a self-contained driver
  # that sets the array then sources the resolver snippet directly.
  local driver="${BATS_TEST_TMPDIR}/driver-cponly.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
log() { printf 'log: %s\n' "\$*"; }
CP_NAME=hbird-cp1
WORKER_NAMES=()
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/resolver.snippet"
printf 'count=%d\n' "\${#WORKER_NAMES[@]}"
printf 'names=%s\n' "\${WORKER_NAMES[*]:-}"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"count=0"* ]]
  # CP-only log line should fire — operator needs to see this.
  [[ "$output" == *"CP-only deploy"* ]]
  # Must NOT have fallen back to the 2-worker default.
  [[ "$output" != *"WORKER_NAMES not set"* ]]
}

@test "deploy-cluster: WORKER_NAMES=(custom names) -> used verbatim" {
  local driver="${BATS_TEST_TMPDIR}/driver-custom.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
log() { printf 'log: %s\n' "\$*"; }
CP_NAME=hbird-cp1
WORKER_NAMES=(hbird-w1 hbird-w2 hbird-w3)
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/resolver.snippet"
printf 'count=%d\n' "\${#WORKER_NAMES[@]}"
printf 'names=%s\n' "\${WORKER_NAMES[*]}"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"count=3"* ]]
  [[ "$output" == *"names=hbird-w1 hbird-w2 hbird-w3"* ]]
  # Neither default-fill nor CP-only branch should fire.
  [[ "$output" != *"WORKER_NAMES not set"* ]]
  [[ "$output" != *"CP-only deploy"* ]]
}

# ---------------------------------------------------------------------------
# render_cp_user_data — AUTO_UPDATE_CP=true (#181)
# ---------------------------------------------------------------------------

@test "deploy-cluster: AUTO_UPDATE_CP=true enables semver timer and disables legacy" {
  AUTO_UPDATE_CP=true
  run render
  [ "$status" -eq 0 ]
  [[ "$output" == *"runcmd:"* ]]
  [[ "$output" == *"systemctl, enable, --now, bootc-semver-update.timer"* ]]
  [[ "$output" == *"systemctl, disable, --now, bootc-fetch-apply-updates.timer"* ]]
  # Must NOT also emit a disable of the semver timer (that's the false-branch).
  ! [[ "$output" == *"systemctl, disable, --now, bootc-semver-update.timer"* ]]
}

# ---------------------------------------------------------------------------
# render_cp_user_data — AUTO_UPDATE_CP=false regression (#181 round-2)
# ---------------------------------------------------------------------------
#
# Pre-round-2, AUTO_UPDATE_CP=false omitted the runcmd entirely. But the
# image's preset enables bootc-semver-update.timer unconditionally on
# factory reset, so the operator's intent ("no auto-updates on the CP")
# was silently ignored. Round-2 emits a disable runcmd in this case.

@test "deploy-cluster: AUTO_UPDATE_CP=false emits disable runcmd for semver timer" {
  AUTO_UPDATE_CP=false
  run render
  [ "$status" -eq 0 ]
  [[ "$output" == *"runcmd:"* ]]
  [[ "$output" == *"systemctl, disable, --now, bootc-semver-update.timer"* ]]
  # Must NOT enable the semver timer when AUTO_UPDATE_CP=false.
  ! [[ "$output" == *"systemctl, enable, --now, bootc-semver-update.timer"* ]]
  # Must NOT touch the legacy timer in the false branch — we leave the
  # legacy state alone on pre-#181 hosts the operator may be deliberately
  # using.
  ! [[ "$output" == *"bootc-fetch-apply-updates.timer"* ]]
}

# ---------------------------------------------------------------------------
# render_cp_user_data — SWITCH_TO_GHCR true / false (#181)
# ---------------------------------------------------------------------------

@test "deploy-cluster: SWITCH_TO_GHCR=true emits bootc switch runcmd with the tag" {
  SWITCH_TO_GHCR=true
  GHCR_TAG=v9.9.9
  run render
  [ "$status" -eq 0 ]
  [[ "$output" == *"bootc, switch, ghcr.io/aatchison/hummingbird-k8s:v9.9.9"* ]]
}

@test "deploy-cluster: SWITCH_TO_GHCR=false omits bootc switch runcmd" {
  SWITCH_TO_GHCR=false
  run render
  [ "$status" -eq 0 ]
  ! [[ "$output" == *"bootc, switch"* ]]
}

# ---------------------------------------------------------------------------
# render_cp_user_data — BOOTC_UPDATE_SCHEDULE override (#181)
# ---------------------------------------------------------------------------

@test "deploy-cluster: BOOTC_UPDATE_SCHEDULE emits override drop-in + restart" {
  BOOTC_UPDATE_SCHEDULE="Mon *-*-* 04:00:00"
  run render
  [ "$status" -eq 0 ]
  # write_files entry to override OnCalendar via a drop-in.
  [[ "$output" == *"bootc-semver-update.timer.d/schedule.conf"* ]]
  [[ "$output" == *"OnCalendar=Mon *-*-* 04:00:00"* ]]
  # Runcmd reloads + restarts the timer so the override takes effect this boot.
  [[ "$output" == *"systemctl, daemon-reload"* ]]
  [[ "$output" == *"systemctl, restart, bootc-semver-update.timer"* ]]
}

# ---------------------------------------------------------------------------
# render_cp_user_data — POD_CIDR / SERVICE_CIDR overrides
# ---------------------------------------------------------------------------
#
# Per-cluster CIDR overrides ride cloud-init write_files into
# /etc/hummingbird/k8s-init-local.env, which k8s-init.sh sources AFTER the
# image-baked k8s-init.env. The same POD_CIDR value also drives the
# explicit ipam.operator.clusterPoolIPv4PodCIDRList pass to cilium install
# (without it Cilium's cluster-pool default 10.0.0.0/8 silently overrides
# kubeadm's podSubnet).

@test "deploy-cluster: POD_CIDR emits k8s-init-local.env write_files entry" {
  POD_CIDR="10.244.0.0/16"
  run render
  [ "$status" -eq 0 ]
  [[ "$output" == *"path: /etc/hummingbird/k8s-init-local.env"* ]]
  [[ "$output" == *"POD_CIDR=10.244.0.0/16"* ]]
  # SERVICE_CIDR unset -> no line for it.
  [[ "$output" != *"SERVICE_CIDR="* ]]
}

@test "deploy-cluster: SERVICE_CIDR alone also emits the env file" {
  SERVICE_CIDR="10.97.0.0/16"
  run render
  [ "$status" -eq 0 ]
  [[ "$output" == *"path: /etc/hummingbird/k8s-init-local.env"* ]]
  [[ "$output" == *"SERVICE_CIDR=10.97.0.0/16"* ]]
  [[ "$output" != *"POD_CIDR="* ]]
}

@test "deploy-cluster: both CIDRs set emits both lines in one entry" {
  POD_CIDR="10.244.0.0/16"
  SERVICE_CIDR="10.97.0.0/16"
  run render
  [ "$status" -eq 0 ]
  [[ "$output" == *"POD_CIDR=10.244.0.0/16"* ]]
  [[ "$output" == *"SERVICE_CIDR=10.97.0.0/16"* ]]
}

@test "deploy-cluster: no CIDR overrides -> no k8s-init-local.env entry" {
  run render
  [ "$status" -eq 0 ]
  [[ "$output" != *"k8s-init-local.env"* ]]
}

# ---------------------------------------------------------------------------
# render_net2_network_config — second-NIC cloud-init network-config (v2)
# ---------------------------------------------------------------------------
#
# The second NIC rides a seed network-config (rendered by cloud-init-local
# BEFORE NetworkManager starts) so NM's auto-default DHCP can never grab a
# default route on it and move node identity. These tests pin the load-
# bearing properties: MAC match, static address, no gateway anywhere, RA
# off, and the primary NIC still declared (providing any network-config
# disables cloud-init's fallback DHCP — omitting primary would kill it).

render_netcfg() {
  # shellcheck disable=SC1090
  source "$SCRIPT"
  render_net2_network_config "$@"
}

@test "deploy-cluster: net2 network-config matches by MAC with static addr" {
  run render_netcfg "52:54:00:d2:00:01" "10.0.0.241/24"
  [ "$status" -eq 0 ]
  [[ "$output" == *'macaddress: "52:54:00:d2:00:01"'* ]]
  [[ "$output" == *'- "10.0.0.241/24"'* ]]
  [[ "$output" == *"dhcp4: false"* ]]
  [[ "$output" == *"accept-ra: false"* ]]
}

@test "deploy-cluster: net2 network-config carries NO gateway/default route" {
  run render_netcfg "52:54:00:d2:00:01" "10.0.0.241/24"
  [ "$status" -eq 0 ]
  [[ "$output" != *"gateway"* ]]
  [[ "$output" != *"routes:"* ]]
  [[ "$output" != *"0.0.0.0/0"* ]]
}

@test "deploy-cluster: net2 network-config keeps primary NIC on DHCP" {
  run render_netcfg "52:54:00:d2:00:01" "10.0.0.241/24"
  [ "$status" -eq 0 ]
  [[ "$output" == *"name: enp1s0"* ]]
  [[ "$output" == *"dhcp4: true"* ]]
}

# ---------------------------------------------------------------------------
# IPv6-off runcmd — the accept-ra/dhcp6 keys never reach NetworkManager
# ---------------------------------------------------------------------------
#
# cloud-init's NM renderer silently drops `accept-ra` and `dhcp6`, leaving
# the keyfile with no [ipv6] section, which NM normalizes to
# ipv6.method=auto (verified on a live node). Without the runcmd below, an
# RA-bearing EXTRA_NETWORK would SLAAC an address and an IPv6 default
# route onto the NIC that must carry no default route at all.

@test "deploy-cluster: EXTRA_NETWORK emits the CP ipv6-off runcmd" {
  EXTRA_NETWORK=sriov-vlan2
  EXTRA_NET_CP_MAC=52:54:00:d2:00:01
  run render
  [ "$status" -eq 0 ]
  [[ "$output" == *"ipv6.method disabled"* ]]
  [[ "$output" == *"52:54:00:d2:00:01"* ]]
  # Resolved MAC -> device -> connection: the NM renderer names the
  # connection after the interface, not the network-config key.
  [[ "$output" == *"GENERAL.CONNECTION"* ]]
}

@test "deploy-cluster: no EXTRA_NETWORK -> no ipv6-off runcmd" {
  run render
  [ "$status" -eq 0 ]
  [[ "$output" != *"ipv6.method"* ]]
}

@test "deploy-cluster: worker gets the ipv6-off runcmd when a net2 MAC is passed" {
  out="${BATS_TEST_TMPDIR}/w.yaml"
  # shellcheck disable=SC1090
  source "$SCRIPT"
  JOIN_CMD="kubeadm join 1.2.3.4:6443 --token abc"
  worker_user_data hbird-w1 "$out" "52:54:00:d2:00:02"
  run cat "$out"
  [ "$status" -eq 0 ]
  [[ "$output" == *"ipv6.method disabled"* ]]
  [[ "$output" == *"52:54:00:d2:00:02"* ]]
}

# ---------------------------------------------------------------------------
# derive_primary_mac — deterministic primary-NIC MAC (#409)
# ---------------------------------------------------------------------------
#
# kubelet's --node-ip is pinned at first boot and never re-derived by the
# init scripts, so the primary NIC's DHCP lease must be stable. A MAC
# derived from the domain name means a rebuilt VM of the same name draws
# the same lease instead of a fresh libvirt-random MAC.

derive_mac() {
  # shellcheck disable=SC1090
  source "$SCRIPT"
  derive_primary_mac "$@"
}

@test "deploy-cluster: derive_primary_mac is deterministic and uses the QEMU OUI" {
  run derive_mac hbird-cp1
  [ "$status" -eq 0 ]
  [[ "$output" =~ ^52:54:00:[0-9a-f]{2}:[0-9a-f]{2}:[0-9a-f]{2}$ ]]
  first="$output"
  run derive_mac hbird-cp1
  [ "$output" = "$first" ]
}

@test "deploy-cluster: derive_primary_mac differs per VM name" {
  run derive_mac hbird-cp1
  a="$output"
  run derive_mac hbird-w1
  [ "$output" != "$a" ]
}

@test "deploy-cluster: worker without a net2 MAC has no ipv6-off runcmd" {
  out="${BATS_TEST_TMPDIR}/w2.yaml"
  # shellcheck disable=SC1090
  source "$SCRIPT"
  JOIN_CMD="kubeadm join 1.2.3.4:6443 --token abc"
  SWITCH_TO_GHCR=false
  BOOTC_UPDATE_SCHEDULE=""
  worker_user_data hbird-w1 "$out" ""
  run cat "$out"
  [ "$status" -eq 0 ]
  [[ "$output" != *"ipv6.method"* ]]
}

# ---------------------------------------------------------------------------
# IMAGE_SOURCE default + validation (#231 — registry-first golden path)
# ---------------------------------------------------------------------------
#
# Pre-#231 deploy-cluster.sh hard-required IMAGE_SOURCE in cluster.local.conf.
# #231 made `ghcr` the fall-through default so a workstation operator can
# `make deploy-cluster` against a minimal config with no IMAGE_SOURCE line.
# `local` is still accepted as a power-user / fast-iteration choice.
#
# Extracted block is the literal `: "${IMAGE_SOURCE:=ghcr}"` line plus the
# `case "$IMAGE_SOURCE" in ghcr|local) ;; *) fail ...` validation that
# follows. Keeping the snippet a verbatim extract (not a paraphrase) means
# any future drift in the script breaks these tests loudly.

@test "deploy-cluster: IMAGE_SOURCE unset defaults to ghcr (#231)" {
  local driver="${BATS_TEST_TMPDIR}/driver-default.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
fail() { printf 'fail: %s\n' "\$*" >&2; exit 1; }
# Don't set IMAGE_SOURCE — that's the whole point of this test.
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/image-source.snippet"
printf 'IMAGE_SOURCE=%s\n' "\$IMAGE_SOURCE"
EOF
  chmod +x "$driver"
  run env -u IMAGE_SOURCE bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"IMAGE_SOURCE=ghcr"* ]]
}

@test "deploy-cluster: IMAGE_SOURCE=local still accepted (#231 regression)" {
  local driver="${BATS_TEST_TMPDIR}/driver-local.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
fail() { printf 'fail: %s\n' "\$*" >&2; exit 1; }
IMAGE_SOURCE=local
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/image-source.snippet"
printf 'IMAGE_SOURCE=%s\n' "\$IMAGE_SOURCE"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"IMAGE_SOURCE=local"* ]]
}

@test "deploy-cluster: IMAGE_SOURCE=ghcr explicit still accepted (#231)" {
  local driver="${BATS_TEST_TMPDIR}/driver-ghcr.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
fail() { printf 'fail: %s\n' "\$*" >&2; exit 1; }
IMAGE_SOURCE=ghcr
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/image-source.snippet"
printf 'IMAGE_SOURCE=%s\n' "\$IMAGE_SOURCE"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"IMAGE_SOURCE=ghcr"* ]]
}

@test "deploy-cluster: IMAGE_SOURCE= (set-but-empty) defaults to ghcr (#231 pin colon semantics)" {
  # The script uses `: "${IMAGE_SOURCE:=ghcr}"` (colon form), which treats
  # both unset AND set-but-empty as "fall through to the default". A future
  # refactor to `${IMAGE_SOURCE=ghcr}` (no colon) would silently regress on
  # the empty case — an operator with `IMAGE_SOURCE=` in cluster.local.conf
  # would land at validation with IMAGE_SOURCE="" and hit the `garbage`
  # branch's failure. This test pins the colon semantics so that regression
  # breaks loudly. (#231 round-2 review H1.)
  local driver="${BATS_TEST_TMPDIR}/driver-empty.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
fail() { printf 'fail: %s\n' "\$*" >&2; exit 1; }
# Mirror a cluster.local.conf line of literal \`IMAGE_SOURCE=\` (empty rvalue).
IMAGE_SOURCE=""
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/image-source.snippet"
printf 'IMAGE_SOURCE=%s\n' "\$IMAGE_SOURCE"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"IMAGE_SOURCE=ghcr"* ]]
  # Must not have hit the validation fail-branch (empty != 'ghcr' or 'local').
  [[ "$output" != *"fail:"* ]]
}

# ---------------------------------------------------------------------------
# HBIRD_OPERATOR_PUBKEY_FILE append (#248)
# ---------------------------------------------------------------------------
#
# When the C3 SSH-wrap shim scp's the operator's workstation pubkey to
# the KVM host's remote tempdir, it forwards the remote path via
# HBIRD_OPERATOR_PUBKEY_FILE. deploy-cluster.sh must APPEND that path
# to SSH_PUBKEY_FILES (colon-separated) so build_qcow2 bakes BOTH the
# KVM host's pubkey (which the script uses to SSH to the freshly-booted
# CP, since SSH_PRIVKEY_FILE = ${SSH_PUBKEY_FILE%.pub}) AND the
# operator's workstation pubkey (which the operator uses for direct
# workstation->CP access).
#
# We can't run the full script (root + libvirt), so extract just the
# append block by awk markers — same pattern as the WORKER_NAMES + IMAGE_SOURCE
# block extractions above.

@test "deploy-cluster: HBIRD_OPERATOR_PUBKEY_FILE set + readable + differs -> appended to SSH_PUBKEY_FILES (#248)" {
  # Extract the #248 block from deploy-cluster.sh.
  awk '
    /^# #248:/ { capture=1 }
    capture { print }
    capture && /^fi$/ { exit }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/pubkey-append.snippet"
  [ -s "${BATS_TEST_TMPDIR}/pubkey-append.snippet" ] || {
    echo "FATAL: failed to extract #248 block from ${SCRIPT}" >&2
    return 1
  }

  # Two real files so [[ -r ]] passes; different paths so the dedup
  # branch doesn't fire.
  local kvm_pubkey="${BATS_TEST_TMPDIR}/kvm-host.pub"
  local op_pubkey="${BATS_TEST_TMPDIR}/operator.pub"
  echo "ssh-ed25519 kvm-host-key kvm@host" > "$kvm_pubkey"
  echo "ssh-ed25519 operator-key user@workstation" > "$op_pubkey"

  local driver="${BATS_TEST_TMPDIR}/driver-append.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
log() { printf 'log: %s\n' "\$*"; }
SSH_PUBKEY_FILE="${kvm_pubkey}"
SSH_PUBKEY_FILES="\${SSH_PUBKEY_FILE}"
export SSH_PUBKEY_FILES
HBIRD_OPERATOR_PUBKEY_FILE="${op_pubkey}"
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/pubkey-append.snippet"
printf 'SSH_PUBKEY_FILES=%s\n' "\$SSH_PUBKEY_FILES"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"SSH_PUBKEY_FILES=${kvm_pubkey}:${op_pubkey}"* ]]
  [[ "$output" == *"appending operator workstation pubkey to bake list: ${op_pubkey}"* ]]
}

@test "deploy-cluster: HBIRD_OPERATOR_PUBKEY_FILE == SSH_PUBKEY_FILE -> no duplicate append (#248)" {
  awk '
    /^# #248:/ { capture=1 }
    capture { print }
    capture && /^fi$/ { exit }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/pubkey-append-dup.snippet"

  local pubkey="${BATS_TEST_TMPDIR}/same.pub"
  echo "ssh-ed25519 shared-key user@host" > "$pubkey"

  local driver="${BATS_TEST_TMPDIR}/driver-dedup.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
log() { printf 'log: %s\n' "\$*"; }
SSH_PUBKEY_FILE="${pubkey}"
SSH_PUBKEY_FILES="\${SSH_PUBKEY_FILE}"
export SSH_PUBKEY_FILES
HBIRD_OPERATOR_PUBKEY_FILE="${pubkey}"
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/pubkey-append-dup.snippet"
printf 'SSH_PUBKEY_FILES=%s\n' "\$SSH_PUBKEY_FILES"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  # SSH_PUBKEY_FILES stays unchanged — no `:` appended, no dup entry.
  [[ "$output" == *"SSH_PUBKEY_FILES=${pubkey}"* ]]
  [[ "$output" != *"SSH_PUBKEY_FILES=${pubkey}:${pubkey}"* ]]
  [[ "$output" != *"appending operator workstation pubkey"* ]]
}

@test "deploy-cluster: HBIRD_OPERATOR_PUBKEY_FILE unset -> unchanged behavior (#248)" {
  awk '
    /^# #248:/ { capture=1 }
    capture { print }
    capture && /^fi$/ { exit }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/pubkey-append-unset.snippet"

  local kvm_pubkey="${BATS_TEST_TMPDIR}/kvm-only.pub"
  echo "ssh-ed25519 kvm-only user@host" > "$kvm_pubkey"

  local driver="${BATS_TEST_TMPDIR}/driver-unset.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
log() { printf 'log: %s\n' "\$*"; }
SSH_PUBKEY_FILE="${kvm_pubkey}"
SSH_PUBKEY_FILES="\${SSH_PUBKEY_FILE}"
export SSH_PUBKEY_FILES
# HBIRD_OPERATOR_PUBKEY_FILE deliberately unset.
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/pubkey-append-unset.snippet"
printf 'SSH_PUBKEY_FILES=%s\n' "\$SSH_PUBKEY_FILES"
EOF
  chmod +x "$driver"
  run env -u HBIRD_OPERATOR_PUBKEY_FILE bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"SSH_PUBKEY_FILES=${kvm_pubkey}"* ]]
  [[ "$output" != *"appending operator workstation pubkey"* ]]
}

@test "deploy-cluster: HBIRD_OPERATOR_PUBKEY_FILE set but UNREADABLE -> no append (#248)" {
  awk '
    /^# #248:/ { capture=1 }
    capture { print }
    capture && /^fi$/ { exit }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/pubkey-append-unreadable.snippet"

  local kvm_pubkey="${BATS_TEST_TMPDIR}/kvm-only2.pub"
  echo "ssh-ed25519 kvm-only user@host" > "$kvm_pubkey"

  local driver="${BATS_TEST_TMPDIR}/driver-unreadable.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
log() { printf 'log: %s\n' "\$*"; }
SSH_PUBKEY_FILE="${kvm_pubkey}"
SSH_PUBKEY_FILES="\${SSH_PUBKEY_FILE}"
export SSH_PUBKEY_FILES
HBIRD_OPERATOR_PUBKEY_FILE="/nonexistent/no/such/key.pub"
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/pubkey-append-unreadable.snippet"
printf 'SSH_PUBKEY_FILES=%s\n' "\$SSH_PUBKEY_FILES"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"SSH_PUBKEY_FILES=${kvm_pubkey}"* ]]
  [[ "$output" != *"appending operator workstation pubkey"* ]]
}

# ---------------------------------------------------------------------------
# worker_user_data — per-worker hostname emission (#254)
# ---------------------------------------------------------------------------
#
# #254: workers were registering in Kubernetes as humbird-worker-<machine-id>
# instead of the names declared in WORKER_NAMES. Pin that the rendered
# worker user-data emits `hostname: <worker_name>` per worker. (The
# in-VM application of that hostname is exercised by the integration
# test below; this bats case covers just the seed render.)

# Helper: source the script and emit worker_user_data to a tmpfile,
# returning its contents on stdout. Mirrors the `render` helper above.
render_worker() {
  local worker_name="$1"
  local tmp="${BATS_TEST_TMPDIR}/worker-userdata-${worker_name}.yaml"
  # shellcheck disable=SC1090
  source "$SCRIPT"
  worker_user_data "$worker_name" "$tmp"
  cat "$tmp"
}

@test "deploy-cluster: worker_user_data emits hostname per worker (#254)" {
  # Minimal env worker_user_data needs.
  export SSH_PUBKEY_CONTENT="ssh-ed25519 AAAA-test-key user@host"
  export JOIN_CMD="kubeadm join 10.0.0.1:6443 --token abc.def --discovery-token-ca-cert-hash sha256:deadbeef"
  export SWITCH_TO_GHCR=true
  export GHCR_TAG=v9.9.9
  export BOOTC_UPDATE_SCHEDULE=""
  export BOOTC_UPDATE_REPO_WORKER=""
  run render_worker "hbird-w1"
  [ "$status" -eq 0 ]
  # The fix: a `hostname:` line per worker, matching the argument.
  [[ "$output" == *"hostname: hbird-w1"* ]]
  # Sanity: write_files for the join cmd is still there.
  [[ "$output" == *"/etc/hummingbird/worker-join.env"* ]]
  [[ "$output" == *"kubeadm join"* ]]
}

@test "deploy-cluster: worker_user_data hostname matches arg, not env (#254)" {
  # Different worker names produce different hostname directives — pin that
  # the function is honoring its argument, not picking up a stale env var.
  export SSH_PUBKEY_CONTENT="ssh-ed25519 AAAA-test-key user@host"
  export JOIN_CMD="kubeadm join 10.0.0.1:6443 --token a.b --discovery-token-ca-cert-hash sha256:deadbeef"
  export SWITCH_TO_GHCR=false
  export GHCR_TAG=latest
  export BOOTC_UPDATE_SCHEDULE=""
  export BOOTC_UPDATE_REPO_WORKER=""
  run render_worker "custom-worker-name-42"
  [ "$status" -eq 0 ]
  [[ "$output" == *"hostname: custom-worker-name-42"* ]]
  # Must NOT contain the other test's name.
  [[ "$output" != *"hostname: hbird-w1"* ]]
}

@test "deploy-cluster: IMAGE_SOURCE=garbage rejected (#231)" {
  local driver="${BATS_TEST_TMPDIR}/driver-garbage.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
fail() { printf 'fail: %s\n' "\$*" >&2; exit 1; }
IMAGE_SOURCE=garbage
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/image-source.snippet"
printf 'should-not-reach\n'
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -ne 0 ]
  [[ "$output" == *"fail:"* ]]
  [[ "$output" == *"IMAGE_SOURCE must be 'ghcr' or 'local'"* ]]
  [[ "$output" != *"should-not-reach"* ]]
}

# ---------------------------------------------------------------------------
# #305 — EUID check accepts root OR libvirt-group membership
#
# Mirrors the pattern PR #272 added to tests/scripts/update-cluster.bats:
# stub `id` on PATH so the script's `id -nG | grep -qx libvirt` hits our
# fake instead of the real one. We force the script to fail AFTER the EUID
# check by giving it a non-existent CONFIG path — that fails with
# "config not readable", which we use as the signal that we made it past
# the EUID gate.
#
# Distinct from the source-only tests above: these exercise the real
# execution path. We UNSET HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY so the script
# reaches the EUID check instead of returning from source early.
# ---------------------------------------------------------------------------

# _stub_id_with_groups groupname [groupname...]
# Build an `id` shim on PATH that emits the given groups when called as
# `id -nG`. Other `id` invocations fall through to the real binary so the
# rest of the script (which may shell out to `id` elsewhere) is unaffected.
_stub_id_with_groups() {
  local stubdir="${BATS_TEST_TMPDIR}/bin-305"
  mkdir -p "$stubdir"
  local groups="$*"
  cat > "${stubdir}/id" <<EOF
#!/usr/bin/env bash
# bats stub: emit a controlled group list for \`id -nG\`; pass other
# invocations through to the real id.
if [[ "\$1" == "-nG" ]]; then
  printf '%s\n' "${groups}"
  exit 0
fi
exec /usr/bin/id "\$@"
EOF
  chmod +x "${stubdir}/id"
  printf '%s' "$stubdir"
}

@test "#305 non-root + libvirt-group membership passes the EUID check (fails later on missing CONFIG)" {
  # Skip when actually running as root — the EUID==0 branch short-circuits
  # the libvirt-group check entirely and we cannot demote ourselves cheaply.
  [ "$EUID" -ne 0 ] || skip "running as root; EUID check short-circuits"
  stubdir="$(_stub_id_with_groups libvirt wheel)"
  # Point CONFIG at a path that doesn't exist so the script bails AFTER
  # the EUID/libvirt check — different diagnostic, distinct signal.
  # Unset HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY so we hit the real EUID path
  # rather than returning from source.
  run env -u HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY PATH="${stubdir}:${PATH}" \
    bash "$SCRIPT" /nonexistent/cluster.local.conf
  [ "$status" -ne 0 ]
  # MUST NOT be the EUID-bail message — we have libvirt group.
  [[ "$output" != *"must be root or a member of the libvirt group"* ]]
  # MUST fail on the config-not-readable check (proves we got past the EUID gate).
  [[ "$output" == *"config not readable"* ]]
  [[ "$output" == *"/nonexistent/cluster.local.conf"* ]]
}

# ---------------------------------------------------------------------------
# #310 — BIB config files routed through mktemp, not REPO_ROOT
#
# Pre-#310 deploy-cluster.sh wrote BIB_CFG_CP/BIB_CFG_WORKER as
# ${REPO_ROOT}/bib-config-deploy-{cp,worker}.toml. On any KVM host that
# had ever run `sudo bash scripts/deploy-cluster.sh`, those files were
# left owned root:root mode 0644, and a subsequent non-root deploy
# (HBIRD_REMOTE_NO_SUDO=1 + libvirt-group operator) failed at the
# rewrite with Permission denied. #310 routes both through mktemp so
# (a) no REPO_ROOT side-effects and (b) per-deploy fresh paths owned
# by the invoking user.
#
# Extracted block is the three-line literal `BIB_CFG_CP=$(mktemp ...)` /
# `BIB_CFG_WORKER=$(mktemp ...)` / `BIB_CFG_TEMPS+=(...)` so a future
# refactor that reintroduces the REPO_ROOT path breaks loudly here.
# ---------------------------------------------------------------------------

@test "deploy-cluster: BIB_CFG_{CP,WORKER} land in mktemp paths, not REPO_ROOT (#310)" {
  # Pin both invariants in a single test (the issue's proposed scope):
  #   1. Pre-create root-owned bib-config-deploy-*.toml in a stub REPO_ROOT,
  #      then assert the resolved paths do NOT collide with them
  #      (i.e. they're outside REPO_ROOT entirely).
  #   2. Both paths are real mktemp outputs (exist, owned by the invoking
  #      user, not nested under the stub REPO_ROOT).
  #
  # Extract the BIB_CFG mktemp block from deploy-cluster.sh by markers.
  # Anchored on the `^# #310:` comment block + the trailing line that
  # appends to BIB_CFG_TEMPS — keeps the snippet a verbatim extract so
  # any drift breaks loudly.
  awk '
    /^# #310:/ { capture=1 }
    capture { print }
    capture && /^BIB_CFG_TEMPS\+=/ { exit }
  ' "$SCRIPT" > "${BATS_TEST_TMPDIR}/bib-cfg-mktemp.snippet"
  [ -s "${BATS_TEST_TMPDIR}/bib-cfg-mktemp.snippet" ] || {
    echo "FATAL: failed to extract #310 BIB_CFG mktemp block from ${SCRIPT}" >&2
    return 1
  }

  # Stub REPO_ROOT with pre-existing (operator-owned here, but
  # filesystem-positionally identical to the root-owned leftover in
  # the field bug) bib-config-deploy-*.toml files. The point of the
  # test isn't to recreate the EPERM — bats can't chown to root —
  # it's to pin that the resolved paths LIVE ELSEWHERE so a root-owned
  # leftover would be irrelevant.
  local stub_repo_root="${BATS_TEST_TMPDIR}/stub-repo-310"
  mkdir -p "$stub_repo_root"
  : > "${stub_repo_root}/bib-config-deploy-cp.toml"
  : > "${stub_repo_root}/bib-config-deploy-worker.toml"

  local driver="${BATS_TEST_TMPDIR}/driver-310.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
REPO_ROOT="${stub_repo_root}"
BIB_CFG_TEMPS=()
# The production script targets a Fedora KVM host (GNU coreutils), where
# \`mktemp -t TEMPLATE\` resolves TEMPLATE under \$TMPDIR. The bats
# container (busybox) parses -t as "use TMPDIR" with TEMPLATE as a
# distinct positional arg, and the GNU spelling errors out. To keep this
# test portable across both runners, shadow \`mktemp\` with a tiny bash
# function that uses the BusyBox positional form (\`mktemp PATH-TEMPLATE\`)
# pointing into BATS_TEST_TMPDIR. Both real \`mktemp\`s honor that form,
# so the shim works under either runtime without diverging from what the
# script does in production. (#310.)
TMPDIR="${BATS_TEST_TMPDIR}"
export TMPDIR
mktemp() {
  # Translate \`mktemp -t TEMPLATE\` into the lowest-common-denominator
  # positional form \`mktemp PATH-TEMPLATE\` under \$TMPDIR. Strip any
  # trailing \`.suffix\` after the Xs because BusyBox \`mktemp\` rejects
  # post-X suffixes; the production script's GNU \`mktemp -t\` is happy
  # with them, but BusyBox is the lowest common denominator we have to
  # satisfy here. The point of the test is to verify the resolved paths
  # live outside REPO_ROOT — the \`.toml\` suffix is cosmetic for bib.
  local template stripped
  if [[ "\${1:-}" == "-t" ]]; then
    template="\$2"
    stripped="\${template%.*}"
    command mktemp "\${TMPDIR}/\${stripped}"
  else
    command mktemp "\$@"
  fi
}
export -f mktemp
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/bib-cfg-mktemp.snippet"
printf 'BIB_CFG_CP=%s\n' "\$BIB_CFG_CP"
printf 'BIB_CFG_WORKER=%s\n' "\$BIB_CFG_WORKER"
printf 'BIB_CFG_TEMPS_COUNT=%d\n' "\${#BIB_CFG_TEMPS[@]}"
# Clean up so the test doesn't leak tempfiles into \$TMPDIR.
rm -f "\$BIB_CFG_CP" "\$BIB_CFG_WORKER"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]

  # Pull the resolved paths out of the driver's output for assertions.
  local cp_path worker_path
  cp_path="$(printf '%s\n' "$output" | sed -n 's/^BIB_CFG_CP=//p')"
  worker_path="$(printf '%s\n' "$output" | sed -n 's/^BIB_CFG_WORKER=//p')"

  # Hard invariant: NEITHER path may live under REPO_ROOT.
  [[ "$cp_path" != "${stub_repo_root}/"* ]]
  [[ "$worker_path" != "${stub_repo_root}/"* ]]

  # Both resolved paths must contain the mktemp template prefix —
  # belt-and-suspenders against a refactor that picks some other
  # non-REPO_ROOT location (e.g. /tmp/static) and silently loses the
  # per-deploy-fresh property.
  [[ "$cp_path" == *"bib-config-deploy-cp."* ]]
  [[ "$worker_path" == *"bib-config-deploy-worker."* ]]

  # Both must be tracked in BIB_CFG_TEMPS so cleanup_on_failure (the
  # failure-trap above the source block) sweeps them on abnormal exit.
  [[ "$output" == *"BIB_CFG_TEMPS_COUNT=2"* ]]

  # Pre-existing stub files must still be there — the script must NOT
  # have touched them. If a future refactor reintroduces the REPO_ROOT
  # write, it would either truncate these (turning them into 0-byte
  # files) or trigger the EPERM in the real field bug.
  [ -e "${stub_repo_root}/bib-config-deploy-cp.toml" ]
  [ -e "${stub_repo_root}/bib-config-deploy-worker.toml" ]
}

@test "#305 non-root + NOT in libvirt group bails with the usermod hint" {
  [ "$EUID" -ne 0 ] || skip "running as root; EUID check short-circuits"
  stubdir="$(_stub_id_with_groups wheel users)"
  run env -u HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY PATH="${stubdir}:${PATH}" \
    bash "$SCRIPT" /nonexistent/cluster.local.conf
  [ "$status" -ne 0 ]
  [[ "$output" == *"must be root or a member of the libvirt group"* ]]
  # The diagnostic must include the one-time setup command so the operator
  # doesn't have to grep docs to recover.
  [[ "$output" == *"usermod -aG libvirt"* ]]
  [[ "$output" == *"newgrp libvirt"* ]]
  # AND must point at the POOL_DIR group-write prerequisite — that's the
  # second half of the no-sudo story and easy to miss without a hint.
  [[ "$output" == *"POOL_DIR"* ]]
}

# ---------------------------------------------------------------------------
# CLI-env precedence over config sourcing — Pattern A (#377)
# ---------------------------------------------------------------------------
#
# `source "$CONFIG_PATH"` performs unconditional `VAR=...` assignment, so a
# config file that hard-assigns an operator-overridable knob silently clobbers
# whatever the operator passed on the CLI (the KEYSTONE bug of the
# silent-override family). deploy-cluster.sh snapshots the CLI values before
# the source and restores any non-empty ones after, so CLI > config > default.
#
# We exercise the real precedence block by extracting it (see setup's
# cli-precedence.snippet) and driving it with a config file that hard-assigns
# the same knobs the "CLI" pre-set — same lockstep-extract approach as the
# WORKER_NAMES / IMAGE_SOURCE / pubkey blocks above.

@test "deploy-cluster: CLI IMAGE_SOURCE/SWITCH_TO_GHCR beat hard-assigning config (#377)" {
  local conf="${BATS_TEST_TMPDIR}/clobber.conf"
  cat > "$conf" <<'CONF_EOF'
# Mirrors a real cluster.local.conf that hard-assigns the knobs.
IMAGE_SOURCE=ghcr
SWITCH_TO_GHCR=true
FORCE_REBUILD=0
CONF_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-precedence.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
# These stand in for operator CLI env (make ... IMAGE_SOURCE=local ...),
# which reach the script via HBIRD_SSH_WRAP_ALLOWED_ENV.
IMAGE_SOURCE=local
SWITCH_TO_GHCR=false
FORCE_REBUILD=1
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'IMAGE_SOURCE=%s\n' "\$IMAGE_SOURCE"
printf 'SWITCH_TO_GHCR=%s\n' "\$SWITCH_TO_GHCR"
printf 'FORCE_REBUILD=%s\n' "\$FORCE_REBUILD"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  # CLI values must survive the source — this is the bug #377 fixes.
  [[ "$output" == *"IMAGE_SOURCE=local"* ]]
  [[ "$output" == *"SWITCH_TO_GHCR=false"* ]]
  [[ "$output" == *"FORCE_REBUILD=1"* ]]
}

@test "deploy-cluster: config value honored when operator passes no CLI override (#377)" {
  # The capture+restore must NOT break the config-only flow: when the operator
  # sets nothing on the CLI, the config's hard assignment is what takes effect.
  local conf="${BATS_TEST_TMPDIR}/configonly.conf"
  cat > "$conf" <<'CONF_EOF'
IMAGE_SOURCE=ghcr
SWITCH_TO_GHCR=true
CONF_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-configonly.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
# No CLI override pre-set (run via \`env -u\` below) — config wins.
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'IMAGE_SOURCE=%s\n' "\$IMAGE_SOURCE"
printf 'SWITCH_TO_GHCR=%s\n' "\$SWITCH_TO_GHCR"
EOF
  chmod +x "$driver"
  run env -u IMAGE_SOURCE -u SWITCH_TO_GHCR bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"IMAGE_SOURCE=ghcr"* ]]
  [[ "$output" == *"SWITCH_TO_GHCR=true"* ]]
}

@test "deploy-cluster: empty CLI override does not clobber config value (#377)" {
  # A set-but-empty CLI var (e.g. \`IMAGE_SOURCE=\` exported into the env) is
  # treated as "no override" — the snapshot's `-n` guard skips restore, so the
  # config's value stands. This pins the empty-vs-unset semantics so a future
  # refactor that restores empties (clobbering config with "") breaks loudly.
  local conf="${BATS_TEST_TMPDIR}/emptycli.conf"
  cat > "$conf" <<'CONF_EOF'
IMAGE_SOURCE=ghcr
CONF_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-emptycli.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
IMAGE_SOURCE=""
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'IMAGE_SOURCE=%s\n' "\$IMAGE_SOURCE"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"IMAGE_SOURCE=ghcr"* ]]
}

@test "deploy-cluster: CLI STRICT_CACHE + KVM_HOST beat hard-assigning config (#373/#377)" {
  # STRICT_CACHE is the new #373 knob; KVM_HOST is the operator-overridable
  # host target consumed AFTER the source. Both must be in the capture array
  # so a config hard-assign cannot clobber the operator's CLI value — this
  # pins them against accidental removal from HBIRD_CLI_OVERRIDE_KNOBS.
  local conf="${BATS_TEST_TMPDIR}/knobs.conf"
  cat > "$conf" <<'CONF_EOF'
STRICT_CACHE=0
KVM_HOST=configbox
CONF_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-knobs.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
STRICT_CACHE=1
KVM_HOST=clibox
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'STRICT_CACHE=%s\n' "\$STRICT_CACHE"
printf 'KVM_HOST=%s\n' "\$KVM_HOST"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"STRICT_CACHE=1"* ]]
  [[ "$output" == *"KVM_HOST=clibox"* ]]
}

# ---------------------------------------------------------------------------
# ENABLE_CLOUD_INIT config-value regression (#2 / #377 precapture fix)
#
# lib/build-common.sh runs `: "${ENABLE_CLOUD_INIT:=0}"` which defaults the
# var to "0" if unset. Before the #2 fix, the snapshot loop ran AFTER that
# lib source, so it captured "0" (the lib default) as a phantom CLI override.
# The restore then clobbered a config's ENABLE_CLOUD_INIT=1 back to 0,
# causing deploy to hard-fail. The fix moves the snapshot to the TOP of the
# script, before any lib source.
#
# These tests exercise the precedence block (cli-precedence.snippet) with
# ENABLE_CLOUD_INIT. The snippet now starts with the snapshot (capturing the
# genuine parent-env state), then sources CONFIG, then restores. No lib is
# sourced inside the snippet, so to prove the ordering fix is load-bearing we
# rely on the contract: snapshot must not see any value not placed there by
# the driver (= genuine CLI/parent-env state). Two cases:
#
#   a. Operator did NOT pass ENABLE_CLOUD_INIT on the CLI → config's "1" wins.
#   b. Operator explicitly passed ENABLE_CLOUD_INIT=0 on the CLI → "0" wins
#      (real CLI overrides config 1 — preserve true CLI precedence).
# ---------------------------------------------------------------------------

@test "deploy-cluster: config ENABLE_CLOUD_INIT=1 survives when CLI has no override (#2 regression)" {
  local conf="${BATS_TEST_TMPDIR}/eci-config.conf"
  printf 'ENABLE_CLOUD_INIT=1\n' > "$conf"
  local driver="${BATS_TEST_TMPDIR}/driver-eci-nooverride.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
# ENABLE_CLOUD_INIT is NOT set (operator passed no CLI override).
# In the OLD code build-common.sh would have run here and set it to 0,
# causing the snapshot to capture 0 as a phantom CLI override. The fix
# ensures the snapshot runs before any lib can default it, so the snapshot
# sees "" (unset) and the restore skips it, letting config's 1 stand.
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'ENABLE_CLOUD_INIT=%s\n' "\$ENABLE_CLOUD_INIT"
EOF
  chmod +x "$driver"
  run env -u ENABLE_CLOUD_INIT bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"ENABLE_CLOUD_INIT=1"* ]]
}

@test "deploy-cluster: CLI ENABLE_CLOUD_INIT=0 still beats config ENABLE_CLOUD_INIT=1 (#2 regression)" {
  local conf="${BATS_TEST_TMPDIR}/eci-config2.conf"
  printf 'ENABLE_CLOUD_INIT=1\n' > "$conf"
  local driver="${BATS_TEST_TMPDIR}/driver-eci-override.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
# Operator explicitly passed ENABLE_CLOUD_INIT=0 on the CLI — a genuine
# override that must win over the config's 1.
ENABLE_CLOUD_INIT=0
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'ENABLE_CLOUD_INIT=%s\n' "\$ENABLE_CLOUD_INIT"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"ENABLE_CLOUD_INIT=0"* ]]
}

# ---------------------------------------------------------------------------
# FORCE_REBUILD + SWITCH_TO_GHCR guard — resolve_switch_to_ghcr (#374)
# ---------------------------------------------------------------------------
#
# When the operator builds/tests a specific image (FORCE_REBUILD=1), a
# first-boot `bootc switch ghcr.io/...` would silently replace those bits with
# the registry's — the second false-positive boot-test mechanism (sibling to
# #373). resolve_switch_to_ghcr auto-disables SWITCH_TO_GHCR in that case
# (loud WARN) unless FORCE_SWITCH=1 opts back in. We exercise the real function
# in source-only mode (it's defined above the source-only guard, like the
# render functions) with a log() shim.

# Helper: source the script source-only with a log() shim, apply the given
# `VAR=VAL` knob assignments, run resolve_switch_to_ghcr, and print the result.
_resolve_switch() {
  local driver="${BATS_TEST_TMPDIR}/resolve-driver.sh"
  {
    printf '#!/usr/bin/env bash\nset -euo pipefail\n'
    printf 'export HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY=1\n'
    printf 'log() { printf "LOG: %%s\\n" "$*" >&2; }\n'
    printf 'source %q\n' "$SCRIPT"
    local kv
    for kv in "$@"; do printf '%s\n' "$kv"; done
    printf 'resolve_switch_to_ghcr\n'
    printf 'printf "RESOLVED=%%s\\n" "$SWITCH_TO_GHCR"\n'
  } > "$driver"
  bash "$driver"
}

@test "deploy-cluster: FORCE_REBUILD=1 + SWITCH_TO_GHCR=true -> auto-disabled + WARN (#374)" {
  run _resolve_switch 'FORCE_REBUILD=1' 'SWITCH_TO_GHCR=true' 'GHCR_TAG=v9.9.9'
  [ "$status" -eq 0 ]
  [[ "$output" == *"RESOLVED=false"* ]]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"#374"* ]]
  [[ "$output" == *"FORCE_SWITCH=1"* ]]   # the WARN tells the operator how to opt back in
}

  @test "deploy-cluster: IMAGE_SOURCE=local + SWITCH_TO_GHCR=true -> auto-disabled + WARN (#9e)" {
    run _resolve_switch 'IMAGE_SOURCE=local' 'SWITCH_TO_GHCR=true' 'GHCR_TAG=v9.9.9'
    [ "$status" -eq 0 ]
    [[ "$output" == *"RESOLVED=false"* ]]
    [[ "$output" == *"WARN"* ]]
    [[ "$output" == *"IMAGE_SOURCE=local"* ]]
    [[ "$output" == *"#374"* ]]
  }

  @test "deploy-cluster: IMAGE_SOURCE=local + SWITCH_TO_GHCR=true + FORCE_SWITCH=1 -> kept, no auto-disable (#9e)" {
    run _resolve_switch 'IMAGE_SOURCE=local' 'SWITCH_TO_GHCR=true' 'FORCE_SWITCH=1'
    [ "$status" -eq 0 ]
    [[ "$output" == *"RESOLVED=true"* ]]
    [[ "$output" != *"Auto-disabling"* ]]
    [[ "$output" == *"FORCE_SWITCH=1"* ]]
  }

  @test "deploy-cluster: FORCE_REBUILD=1 + SWITCH_TO_GHCR=true + FORCE_SWITCH=1 -> kept, no auto-disable (#374)" {
  run _resolve_switch 'FORCE_REBUILD=1' 'SWITCH_TO_GHCR=true' 'FORCE_SWITCH=1'
  [ "$status" -eq 0 ]
  [[ "$output" == *"RESOLVED=true"* ]]
  # Opt-in path logs an explicit notice, NOT the auto-disable WARN.
  [[ "$output" != *"Auto-disabling"* ]]
  [[ "$output" == *"FORCE_SWITCH=1"* ]]
}

@test "deploy-cluster: FORCE_REBUILD=1 + SWITCH_TO_GHCR=false -> no-op, no WARN (#374)" {
  run _resolve_switch 'FORCE_REBUILD=1' 'SWITCH_TO_GHCR=false'
  [ "$status" -eq 0 ]
  [[ "$output" == *"RESOLVED=false"* ]]
  [[ "$output" != *"WARN"* ]]
}

@test "deploy-cluster: SWITCH_TO_GHCR=true without FORCE_REBUILD -> unchanged, no WARN (#374)" {
  # The golden ghcr path (FORCE_REBUILD unset) must NOT be touched by the guard.
  run _resolve_switch 'SWITCH_TO_GHCR=true'
  [ "$status" -eq 0 ]
  [[ "$output" == *"RESOLVED=true"* ]]
  [[ "$output" != *"WARN"* ]]
}

@test "deploy-cluster: #374 guard -> CP cloud-init OMITS bootc switch; FORCE_SWITCH=1 keeps it" {
  # End-to-end: guard mutates SWITCH_TO_GHCR, then render_cp_user_data (which
  # reads it) must drop the `bootc switch` runcmd. Proves the guard->render wire.
  local driver="${BATS_TEST_TMPDIR}/driver-374-cp.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
export HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY=1
log() { printf 'LOG: %s\n' "\$*" >&2; }
export CP_NAME=hbird-cp1
export SSH_PUBKEY_CONTENT="ssh-ed25519 AAAA-test key@host"
export GHCR_TAG=v9.9.9
export AUTO_UPDATE_CP=true
export BOOTC_UPDATE_SCHEDULE=""
export BOOTC_UPDATE_REPO_K8S=""
FORCE_REBUILD=1
SWITCH_TO_GHCR=true
FORCE_SWITCH="\${FORCE_SWITCH:-}"
# shellcheck disable=SC1090
source "${SCRIPT}"
resolve_switch_to_ghcr
render_cp_user_data
EOF
  chmod +x "$driver"

  run bash "$driver"
  [ "$status" -eq 0 ]
  # Guard fired -> the GHCR switch line for the CP image must be absent.
  [[ "$output" != *"bootc, switch, ghcr.io/aatchison/hummingbird-k8s:v9.9.9"* ]]

  # Opt back in -> the switch line returns.
  FORCE_SWITCH=1 run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"bootc, switch, ghcr.io/aatchison/hummingbird-k8s:v9.9.9"* ]]
}

@test "deploy-cluster: #374 guard -> worker cloud-init OMITS bootc switch; FORCE_SWITCH=1 keeps it" {
  local driver="${BATS_TEST_TMPDIR}/driver-374-worker.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
export HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY=1
log() { printf 'LOG: %s\n' "\$*" >&2; }
export SSH_PUBKEY_CONTENT="ssh-ed25519 AAAA-test key@host"
export JOIN_CMD="kubeadm join PLACEHOLDER --token redacted"
export GHCR_TAG=v9.9.9
export BOOTC_UPDATE_SCHEDULE=""
export BOOTC_UPDATE_REPO_WORKER=""
FORCE_REBUILD=1
SWITCH_TO_GHCR=true
FORCE_SWITCH="\${FORCE_SWITCH:-}"
# shellcheck disable=SC1090
source "${SCRIPT}"
resolve_switch_to_ghcr
out="\$(mktemp)"
worker_user_data hbird-w1 "\$out"
cat "\$out"
EOF
  chmod +x "$driver"

  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" != *"bootc, switch, ghcr.io/aatchison/hummingbird-k8s-worker:v9.9.9"* ]]

  FORCE_SWITCH=1 run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"bootc, switch, ghcr.io/aatchison/hummingbird-k8s-worker:v9.9.9"* ]]
}

@test "deploy-cluster: CLI FORCE_SWITCH beats hard-assigning config (#374)" {
  # FORCE_SWITCH must be in HBIRD_CLI_OVERRIDE_KNOBS so a config hard-assign
  # cannot clobber the operator's explicit opt-in/out.
  local conf="${BATS_TEST_TMPDIR}/fs.conf"
  cat > "$conf" <<'CONF_EOF'
FORCE_SWITCH=0
CONF_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-fs.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
FORCE_SWITCH=1
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'FORCE_SWITCH=%s\n' "\$FORCE_SWITCH"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"FORCE_SWITCH=1"* ]]
}

# ---------------------------------------------------------------------------
# BOOTC_UPDATE_SCHEDULE long-term-drift WARN — warn_bootc_update_drift
# (#376 deferred WARN, folded into #374 per bt-374 decided-scope)
# ---------------------------------------------------------------------------

# Helper: source source-only with a log() shim, set BOOTC_UPDATE_SCHEDULE from
# $1, run warn_bootc_update_drift, surfacing its log output.
_warn_drift() {
  local driver="${BATS_TEST_TMPDIR}/warn-drift.sh"
  {
    printf '#!/usr/bin/env bash\nset -euo pipefail\n'
    printf 'export HBIRD_DEPLOY_CLUSTER_SOURCE_ONLY=1\n'
    printf 'log() { printf "LOG: %%s\\n" "$*" >&2; }\n'
    printf 'source %q\n' "$SCRIPT"
    printf 'BOOTC_UPDATE_SCHEDULE=%q\n' "$1"
    printf 'warn_bootc_update_drift\n'
    printf 'printf "DONE\\n"\n'
  } > "$driver"
  bash "$driver"
}

@test "deploy-cluster: BOOTC_UPDATE_SCHEDULE set -> drift WARN (semver wording, not :latest) (#376)" {
  run _warn_drift "*-*-* 04:00:00"
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARN"* ]]
  [[ "$output" == *"BOOTC_UPDATE_SCHEDULE"* ]]
  [[ "$output" == *"bootc-semver-update.timer"* ]]
  [[ "$output" == *"vX.Y.Z"* ]]
  # Must use semver-timer language, NOT imply a literal :latest poll.
  [[ "$output" != *":latest"* ]]
  [[ "$output" == *"DONE"* ]]
}

@test "deploy-cluster: BOOTC_UPDATE_SCHEDULE unset -> no drift WARN (#376)" {
  run _warn_drift ""
  [ "$status" -eq 0 ]
  [[ "$output" != *"WARN"* ]]
  [[ "$output" == *"DONE"* ]]
}

# ---------------------------------------------------------------------------
# Per-node-by-name cluster Ready poll (#9 item d)
# ---------------------------------------------------------------------------
#
# The old aggregate `ready_count >= EXPECTED_NODES` check can be satisfied
# by a stray or duplicate node while a named worker's kubeadm join silently
# failed. The fix iterates CP_NAME + WORKER_NAMES[@] and asserts each node
# individually via `kubectl get node <name>`.
#
# Tests source the real ready-poll snippet (see setup) with a mock cp_ssh()
# so the per-name semantics can be driven without a real cluster.

@test "deploy-cluster: per-node Ready poll — all named nodes Ready -> CLUSTER_READY=1 (#9)" {
  cat > "${BATS_TEST_TMPDIR}/cp-ssh-all-ready.sh" <<'MOCK_EOF'
# All nodes return "yes" — every named node is Ready.
cp_ssh() { echo "yes"; }
MOCK_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-ready-all.sh"
  {
    printf '#!/usr/bin/env bash\nset -euo pipefail\n'
    printf 'log() { printf "LOG: %%s\\n" "$*"; }\n'
    printf '# shellcheck disable=SC1091\n'
    printf 'source %q\n' "${BATS_TEST_TMPDIR}/cp-ssh-all-ready.sh"
    printf 'CP_NAME=hbird-cp\n'
    printf 'WORKER_NAMES=(hbird-w1)\n'
    printf 'CP_READY_RETRIES=1\nCP_READY_SLEEP=0\n'
    printf '# shellcheck disable=SC1091\n'
    printf 'source %q\n' "${BATS_TEST_TMPDIR}/ready-poll.snippet"
    printf 'printf "CLUSTER_READY=%%d\\n" "$CLUSTER_READY"\n'
  } > "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"CLUSTER_READY=1"* ]]
  [[ "$output" == *"all 2 named nodes Ready"* ]]
}

@test "deploy-cluster: per-node Ready poll — named worker NotReady, stray node ignored -> CLUSTER_READY=0 (#9)" {
  # CP is Ready; named worker hbird-w1 is not (simulates a failed kubeadm
  # join). A hypothetical stray node would have satisfied the old aggregate
  # count (>=2). The per-name check must still report CLUSTER_READY=0.
  cat > "${BATS_TEST_TMPDIR}/cp-ssh-stray.sh" <<'MOCK_EOF'
cp_ssh() {
  local cmd="$1"
  # Only the CP 'hbird-cp' responds Ready; 'hbird-w1' returns nothing.
  if printf '%s' "$cmd" | grep -qF "'hbird-cp'"; then
    echo "yes"
  fi
}
MOCK_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-ready-stray.sh"
  {
    printf '#!/usr/bin/env bash\nset -euo pipefail\n'
    printf 'log() { printf "LOG: %%s\\n" "$*"; }\n'
    printf '# shellcheck disable=SC1091\n'
    printf 'source %q\n' "${BATS_TEST_TMPDIR}/cp-ssh-stray.sh"
    printf 'CP_NAME=hbird-cp\n'
    printf 'WORKER_NAMES=(hbird-w1)\n'
    printf 'CP_READY_RETRIES=1\nCP_READY_SLEEP=0\n'
    printf '# shellcheck disable=SC1091\n'
    printf 'source %q\n' "${BATS_TEST_TMPDIR}/ready-poll.snippet"
    printf 'printf "CLUSTER_READY=%%d\\n" "$CLUSTER_READY"\n'
  } > "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"CLUSTER_READY=0"* ]]
  [[ "$output" == *"not Ready"* ]]
}

# ---------------------------------------------------------------------------
# lib-defaulted knobs CLI-env precedence (#8)
# ---------------------------------------------------------------------------
#
# BASE_IMAGE, BIB, ENABLE_ROOT_SSH, VM_USER, VM_USER_GROUPS are lib-defaulted
# by build-common.sh and read after `source CONFIG` in build_qcow2 /
# render_bib_config. They were missing from HBIRD_CLI_OVERRIDE_KNOBS, so a
# config file that hard-assigns them silently clobbered a CLI override — the
# same silent-override class that #2/#377 fixed for other knobs.
#
# These tests mirror the #377 pattern: drive the real cli-precedence.snippet
# (which now includes BASE_IMAGE, BIB, ENABLE_ROOT_SSH, VM_USER,
# VM_USER_GROUPS) to pin CLI > config > default semantics for the newly added
# knobs.

@test "deploy-cluster: CLI BASE_IMAGE/VM_USER beat hard-assigning config (#8)" {
  local conf="${BATS_TEST_TMPDIR}/clobber8.conf"
  cat > "$conf" <<'CONF_EOF'
BASE_IMAGE=quay.io/fedora/fedora-bootc:41
VM_USER=config-user
CONF_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-precedence8.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
BASE_IMAGE=quay.io/centos-bootc/centos-bootc:stream9
VM_USER=cli-user
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'BASE_IMAGE=%s\n' "\$BASE_IMAGE"
printf 'VM_USER=%s\n' "\$VM_USER"
EOF
  chmod +x "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"BASE_IMAGE=quay.io/centos-bootc/centos-bootc:stream9"* ]]
  [[ "$output" == *"VM_USER=cli-user"* ]]
}

@test "deploy-cluster: config BASE_IMAGE/VM_USER honored when no CLI override (#8)" {
  local conf="${BATS_TEST_TMPDIR}/configonly8.conf"
  cat > "$conf" <<'CONF_EOF'
BASE_IMAGE=quay.io/fedora/fedora-bootc:41
VM_USER=config-user
CONF_EOF
  local driver="${BATS_TEST_TMPDIR}/driver-configonly8.sh"
  cat > "$driver" <<EOF
#!/usr/bin/env bash
set -euo pipefail
CONFIG_PATH="${conf}"
# No CLI override — config value must win.
# shellcheck disable=SC1091
source "${BATS_TEST_TMPDIR}/cli-precedence.snippet"
printf 'BASE_IMAGE=%s\n' "\$BASE_IMAGE"
printf 'VM_USER=%s\n' "\$VM_USER"
EOF
  chmod +x "$driver"
  run env -u BASE_IMAGE -u VM_USER bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"BASE_IMAGE=quay.io/fedora/fedora-bootc:41"* ]]
  [[ "$output" == *"VM_USER=config-user"* ]]
}

  # Helper: source the pre-flight snippet with a mock podman, and print the result.
  _preflight_podman() {
    local podman_rc="$1"
    local driver="${BATS_TEST_TMPDIR}/driver-preflight.sh"
    {
      printf '#!/usr/bin/env bash\nset -euo pipefail\n'
      printf 'log() { printf "LOG: %%s\\n" "$*"; }\n'
      printf 'fail() { printf "FAIL: %%s\\n" "$*" >&2; exit 1; }\n'
      # Mock podman: always exits with the given rc.
      printf 'podman() { return %d; }\n' "$podman_rc"
      # Mock sudo: just calls the inner command.
      printf 'sudo() { "$@"; }\n'
      printf 'IMAGE_SOURCE=local\n'
      printf '# shellcheck disable=[]\n'
      printf 'source %q\n' "${BATS_TEST_TMPDIR}/podman-preflight.snippet"
    } > "$driver"
    chmod +x "$driver"
    bash "$driver"
  }

  # ---------------------------------------------------------------------------
  # rootful podman pre-flight check (#4)
  # ---------------------------------------------------------------------------

  @test "deploy-cluster: IMAGE_SOURCE=local + rootful podman OK -> passes pre-flight" {
    run _preflight_podman 0
    [ "$status" -eq 0 ]
    [[ "$output" == *"LOG: pre-flight: checking for rootful podman"* ]]
  }

  @test "deploy-cluster: IMAGE_SOURCE=local + rootful podman FAIL -> bails with hint (#4)" {
    run _preflight_podman 1
    [ "$status" -ne 0 ]
    [[ "$output" == *"FAIL: rootful podman is unavailable"* ]]
    [[ "$output" == *"sudo dnf install -y podman"* ]]
  }

  @test "deploy-cluster: IMAGE_SOURCE=ghcr -> skips podman pre-flight" {
    local driver="${BATS_TEST_TMPDIR}/driver-skip-preflight.sh"
    {
      printf '#!/usr/bin/env bash\nset -euo pipefail\n'
      printf 'log() { printf "LOG: %%s\\n" "$*"; }\n'
      printf 'fail() { printf "FAIL: %%s\\n" "$*" >&2; exit 1; }\n'
      # Mock podman: always fail.
      printf 'podman() { return 1; }\n'
      printf 'sudo() { "$@"; }\n'
      printf 'IMAGE_SOURCE=ghcr\n'
      printf '# shellcheck disable=[]\n'
      printf 'source %q\n' "${BATS_TEST_TMPDIR}/podman-preflight.snippet"
    } > "$driver"
    chmod +x "$driver"
    run bash "$driver"
    [ "$status" -eq 0 ]
    # Must NOT have attempted the podman check.
    [[ "$output" != *"LOG: pre-flight: checking for rootful podman"* ]]
  }

@test "deploy-cluster: RUN_VERIFY=true + STRICT_CACHE=1 + verifier non-zero -> deploy fails (#9)" {
  # Stub hbird to always exit 1 (verifier failure).
  local bindir="${BATS_TEST_TMPDIR}/bin-strict-fail"
  mkdir -p "$bindir"
  printf '#!/usr/bin/env bash\nexit 1\n' > "${bindir}/hbird"
  chmod +x "${bindir}/hbird"
  local driver="${BATS_TEST_TMPDIR}/driver-rv-strict-fail.sh"
  {
    printf '#!/usr/bin/env bash\nset -euo pipefail\n'
    printf 'log() { printf "LOG: %%s\\n" "$*"; }\n'
    printf 'fail() { printf "FAIL: %%s\\n" "$*" >&2; exit 1; }\n'
    printf 'PATH=%q:${PATH}\n' "$bindir"
    printf 'STRICT_CACHE=1\n'
    printf 'CONFIG_PATH=/dev/null\nCP_IP=127.0.0.1\nKVM_HOST=\n'
    printf '# shellcheck disable=SC1091\n'
    printf 'source %q\n' "${BATS_TEST_TMPDIR}/run-verify.snippet"
  } > "$driver"
  run bash "$driver"
  [ "$status" -ne 0 ]
  [[ "$output" == *"FAIL:"* ]]
  [[ "$output" == *"STRICT_CACHE=1"* ]]
}

@test "deploy-cluster: RUN_VERIFY=true + STRICT_CACHE=1 + hbird missing -> deploy fails (#9)" {
  # No hbird on PATH — use an empty bin dir so command -v hbird fails.
  local emptydir="${BATS_TEST_TMPDIR}/bin-empty"
  mkdir -p "$emptydir"
  local driver="${BATS_TEST_TMPDIR}/driver-rv-strict-missing.sh"
  {
    printf '#!/usr/bin/env bash\nset -euo pipefail\n'
    printf 'log() { printf "LOG: %%s\\n" "$*"; }\n'
    printf 'fail() { printf "FAIL: %%s\\n" "$*" >&2; exit 1; }\n'
    printf 'PATH=%q\n' "$emptydir"
    printf 'STRICT_CACHE=1\n'
    printf 'CONFIG_PATH=/dev/null\nCP_IP=127.0.0.1\nKVM_HOST=\n'
    printf '# shellcheck disable=SC1091\n'
    printf 'source %q\n' "${BATS_TEST_TMPDIR}/run-verify.snippet"
  } > "$driver"
  run bash "$driver"
  [ "$status" -ne 0 ]
  [[ "$output" == *"FAIL:"* ]]
  [[ "$output" == *"STRICT_CACHE=1"* ]]
}

@test "deploy-cluster: RUN_VERIFY=true + STRICT_CACHE unset + verifier non-zero -> informational, deploy succeeds (#9)" {
  # Stub hbird to exit 1; without STRICT_CACHE=1 this must remain non-fatal.
  local bindir="${BATS_TEST_TMPDIR}/bin-nonstrict"
  mkdir -p "$bindir"
  printf '#!/usr/bin/env bash\nexit 1\n' > "${bindir}/hbird"
  chmod +x "${bindir}/hbird"
  local driver="${BATS_TEST_TMPDIR}/driver-rv-nonstrict.sh"
  {
    printf '#!/usr/bin/env bash\nset -euo pipefail\n'
    printf 'log() { printf "LOG: %%s\\n" "$*"; }\n'
    printf 'fail() { printf "FAIL: %%s\\n" "$*" >&2; exit 1; }\n'
    printf 'PATH=%q:${PATH}\n' "$bindir"
    printf '# STRICT_CACHE deliberately unset — non-strict path\n'
    printf 'CONFIG_PATH=/dev/null\nCP_IP=127.0.0.1\nKVM_HOST=\n'
    printf '# shellcheck disable=SC1091\n'
    printf 'source %q\n' "${BATS_TEST_TMPDIR}/run-verify.snippet"
    printf 'echo "deploy-ok"\n'
  } > "$driver"
  run bash "$driver"
  [ "$status" -eq 0 ]
  [[ "$output" == *"informational"* ]]
  [[ "$output" == *"deploy-ok"* ]]
  [[ "$output" != *"FAIL:"* ]]
}
