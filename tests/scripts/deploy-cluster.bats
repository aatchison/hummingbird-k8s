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
