#!/usr/bin/env bats
#
# Unit tests for the build_qcow2() helper in lib/build-common.sh (issue #115).
#
# build_qcow2 is the heart of build-k8s.sh / build-worker.sh: it
# shells out to bootc-image-builder via podman, promotes the staged disk to its
# final qcow2 path under $POOL_DIR, and refreshes any known libvirt pools. The
# function is too thin to be worth e2e-mocking the bib image, but the bits we
# DO want to test are exactly the bits an operator hits when something is off:
#
#   1. Default args -> the right podman invocation (image, bib, mounts).
#   2. POOL_DIR override -> qcow2 lands in the custom path.
#   3. BIB override -> the right bib image is used.
#   4. Failure path -> missing bib-config emits a clear error before podman.
#
# We never actually call podman (the bats container has no podman, and we don't
# want a 2 GB pull in tests anyway). Instead, every test stubs `podman` as a
# shell function that captures argv to a file, then writes the staged
# `disk.qcow2` so the post-podman promotion logic can run. `virsh` is also
# stubbed to a no-op so the best-effort pool-refresh calls don't pollute test
# output or fail in containers without libvirt-clients.
#
# Run via:
#   make test-lib                          # all bats tests
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/lib/build_qcow2.bats

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  LIB="${REPO_ROOT}/lib/build-common.sh"
  FIX="${BATS_TEST_DIRNAME}/fixtures"

  # Isolate HOME so the library never reads developer state.
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  # Wipe library inputs.
  unset SSH_PUBKEY_FILES SSH_PUBKEY_GH_USERS VM_USER VM_USER_GROUPS \
        VM_PASSWORD ENABLE_ROOT_SSH SUDO_USER POOL_DIR BIB BASE_IMAGE \
        STORAGE_DRIVER PODMAN_ROOT PODMAN_RUNROOT

  # Default sandbox locations for every test.
  export POOL_DIR="${BATS_TEST_TMPDIR}/pool"
  export SSH_PUBKEY_FILES="${FIX}/keys/a.pub"
  mkdir -p "$POOL_DIR"

  # Capture file for stubbed-podman argv.
  PODMAN_ARGS="${BATS_TEST_TMPDIR}/podman-args.txt"
  export PODMAN_ARGS
}

# Common stub harness:
#   - podman() writes its argv (one per line) to $PODMAN_ARGS, then drops a
#     fake disk.qcow2 in the staging dir so the promotion logic can succeed.
#     It also accepts an optional `_PODMAN_RC` env var so tests can simulate
#     a bib failure.
#   - virsh() no-ops (build_qcow2's pool-refresh is best-effort).
#   - chown/chmod no-op so we don't need root.
#
# Note: storage-isolation tests (issue #199) wipe STORAGE_DRIVER /
# PODMAN_ROOT / PODMAN_RUNROOT in setup() and set them only inside the
# tests that exercise the isolation path; that way the default-args
# snapshots above keep working even on hosts where an operator happens
# to have these set in their environment.
install_stubs() {
  podman() {
    printf '%s\n' "$@" > "$PODMAN_ARGS"
    # bib stages the output disk at $POOL_DIR/qcow2/disk.qcow2 — fabricate it
    # so the post-bib mv+rmdir can run. Skip this side effect if the test
    # wants to simulate a missing-disk failure mode.
    if [[ "${_BIB_PRODUCE_DISK:-1}" = 1 ]]; then
      mkdir -p "${POOL_DIR}/qcow2"
      : > "${POOL_DIR}/qcow2/disk.qcow2"
    fi
    return "${_PODMAN_RC:-0}"
  }
  export -f podman
  virsh() { :; }
  export -f virsh
  chown() { :; }
  export -f chown
  chmod() { :; }
  export -f chmod
}

source_lib() {
  # shellcheck disable=SC1090
  source "$LIB"
}

# A throwaway bib-config — the contents don't matter because podman is stubbed.
make_cfg() {
  local p="${BATS_TEST_TMPDIR}/bib-config.toml"
  printf '[[customizations.user]]\nname = "test"\n' > "$p"
  echo "$p"
}

# ---------------------------------------------------------------------------
# Snapshot: podman argv shape (default args)
# ---------------------------------------------------------------------------

@test "build_qcow2: default args invoke podman with bib image + expected mounts + qcow2 type" {
  install_stubs
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/hummingbird-k8s:latest hummingbird-k8s "$cfg"
  [ "$status" -eq 0 ]

  # The captured argv must include the bib image, the config + output mounts,
  # the qcow2 type, --local, and the local image ref. Each as its own line in
  # $PODMAN_ARGS.
  grep -qx 'run' "$PODMAN_ARGS"
  grep -qx -- '--rm' "$PODMAN_ARGS"
  grep -qx -- '--privileged' "$PODMAN_ARGS"
  grep -qx -- '--pull=newer' "$PODMAN_ARGS"
  # Default BIB (from build-common.sh defaults).
  grep -qx 'quay.io/centos-bootc/bootc-image-builder:latest' "$PODMAN_ARGS"
  # The config mount uses the cfg path we passed in.
  grep -qFx -- "${cfg}:/config.toml:ro" "$PODMAN_ARGS"
  # The output mount uses POOL_DIR.
  grep -qFx -- "${POOL_DIR}:/output" "$PODMAN_ARGS"
  # bib args.
  grep -qx -- '--type' "$PODMAN_ARGS"
  grep -qx 'qcow2' "$PODMAN_ARGS"
  grep -qx -- '--rootfs' "$PODMAN_ARGS"
  grep -qx 'ext4' "$PODMAN_ARGS"
  grep -qx -- '--local' "$PODMAN_ARGS"
  grep -qx 'localhost/hummingbird-k8s:latest' "$PODMAN_ARGS"

  # When no isolation env vars are set, the legacy host storage path is
  # bind-mounted into the BIB container (so the nested podman finds the
  # image graph the outer podman just populated). And NO top-level
  # --root / --runroot / --storage-driver flags get prepended.
  grep -qFx -- '/var/lib/containers/storage:/var/lib/containers/storage' "$PODMAN_ARGS"
  ! grep -qx -- '--root'           "$PODMAN_ARGS"
  ! grep -qx -- '--runroot'        "$PODMAN_ARGS"
  ! grep -qx -- '--storage-driver' "$PODMAN_ARGS"

  # Per the corrected isolation contract (podman doesn't honor
  # PODMAN_ROOT / PODMAN_RUNROOT as env-var names natively, so passing
  # them via -e would be inert + misleading), the BIB invocation must
  # NOT carry -e PODMAN_ROOT or -e PODMAN_RUNROOT. -e STORAGE_DRIVER is
  # the only -e the BIB container needs (podman recognizes it).
  ! grep -qx 'PODMAN_ROOT'    "$PODMAN_ARGS"
  ! grep -qx 'PODMAN_RUNROOT' "$PODMAN_ARGS"

  # The promoted qcow2 ends up at ${POOL_DIR}/<name>.qcow2 and the staging dir
  # is gone.
  [ -f "${POOL_DIR}/hummingbird-k8s.qcow2" ]
  [ ! -d "${POOL_DIR}/qcow2" ]
  # Stdout should announce the path so operators can grep for it.
  [[ "$output" == *"Built: ${POOL_DIR}/hummingbird-k8s.qcow2"* ]]
}

# ---------------------------------------------------------------------------
# Snapshot: POOL_DIR override
# ---------------------------------------------------------------------------

@test "build_qcow2: POOL_DIR=<custom> places the qcow2 in that directory" {
  install_stubs
  export POOL_DIR="${BATS_TEST_TMPDIR}/custom-pool"
  mkdir -p "$POOL_DIR"
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/hummingbird-k8s:latest hummingbird-k8s "$cfg"
  [ "$status" -eq 0 ]

  # The output mount must point at the custom pool.
  grep -qFx -- "${POOL_DIR}:/output" "$PODMAN_ARGS"
  # The promoted qcow2 lives in the custom pool, not the default one.
  [ -f "${POOL_DIR}/hummingbird-k8s.qcow2" ]
  [ ! -f "/var/lib/libvirt/images/hummingbird-k8s.qcow2" ]
  [[ "$output" == *"Built: ${POOL_DIR}/hummingbird-k8s.qcow2"* ]]
}

# ---------------------------------------------------------------------------
# Snapshot: BIB override
# ---------------------------------------------------------------------------

@test "build_qcow2: BIB=<custom-image> passes that image to podman run" {
  install_stubs
  export BIB="quay.io/example/bib-fork:dev"
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/hummingbird-worker:latest hummingbird-worker "$cfg"
  [ "$status" -eq 0 ]

  # The custom BIB image appears in the captured argv; the default does not.
  grep -qx 'quay.io/example/bib-fork:dev' "$PODMAN_ARGS"
  ! grep -qx 'quay.io/centos-bootc/bootc-image-builder:latest' "$PODMAN_ARGS"
}

# ---------------------------------------------------------------------------
# Failure modes
# ---------------------------------------------------------------------------

@test "build_qcow2: missing bib-config file fails with a clear error before podman" {
  install_stubs
  source_lib

  run build_qcow2 localhost/whatever:latest whatever "${BATS_TEST_TMPDIR}/does-not-exist.toml"
  [ "$status" -ne 0 ]
  [[ "$output" == *"bib config not readable"* ]]
  [[ "$output" == *"render_bib_config"* ]]
  # podman must NOT have been called — the precheck must happen first.
  [ ! -f "$PODMAN_ARGS" ]
}

@test "build_qcow2: missing required args fails with a usage hint" {
  install_stubs
  source_lib

  run build_qcow2 "" "" ""
  [ "$status" -ne 0 ]
  [[ "$output" == *"usage:"* ]]
  [ ! -f "$PODMAN_ARGS" ]
}

@test "build_qcow2: bib exit non-zero surfaces a contextual error (not silent)" {
  install_stubs
  export _PODMAN_RC=42
  export _BIB_PRODUCE_DISK=0
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/img:latest whatever "$cfg"
  [ "$status" -ne 0 ]
  [[ "$output" == *"bootc-image-builder"* ]]
  [[ "$output" == *"failed"* ]]
  # No qcow2 should have been promoted into the pool.
  [ ! -f "${POOL_DIR}/whatever.qcow2" ]
}

@test "build_qcow2: bib exits 0 but produces no disk -> clear error, no stale state" {
  install_stubs
  export _BIB_PRODUCE_DISK=0
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/img:latest whatever "$cfg"
  [ "$status" -ne 0 ]
  [[ "$output" == *"expected"* ]]
  [[ "$output" == *"disk.qcow2"* ]]
}

# ---------------------------------------------------------------------------
# Storage isolation (issue #199): STORAGE_DRIVER / PODMAN_ROOT / PODMAN_RUNROOT
# ---------------------------------------------------------------------------

@test "build_qcow2: STORAGE_DRIVER / PODMAN_ROOT / PODMAN_RUNROOT thread through to podman + BIB" {
  install_stubs
  export STORAGE_DRIVER=vfs
  export PODMAN_ROOT="${BATS_TEST_TMPDIR}/iso-graphroot"
  export PODMAN_RUNROOT="${BATS_TEST_TMPDIR}/iso-runroot"
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/hummingbird-k8s:latest hummingbird-k8s "$cfg"
  [ "$status" -eq 0 ]

  # 1) Top-level podman flags (BEFORE `run`) must include --storage-driver,
  #    --root, --runroot with the values we set.
  grep -qx -- '--storage-driver' "$PODMAN_ARGS"
  grep -qx 'vfs'                  "$PODMAN_ARGS"
  grep -qx -- '--root'            "$PODMAN_ARGS"
  grep -qFx -- "${PODMAN_ROOT}"   "$PODMAN_ARGS"
  grep -qx -- '--runroot'         "$PODMAN_ARGS"
  grep -qFx -- "${PODMAN_RUNROOT}" "$PODMAN_ARGS"

  # 2) Order matters: the flags must appear BEFORE the `run` subcommand
  #    so podman parses them as global options, not as args to `run`.
  local first_run_line first_storage_line first_root_line first_runroot_line
  first_run_line="$(grep -nx 'run' "$PODMAN_ARGS" | head -1 | cut -d: -f1)"
  first_storage_line="$(grep -nx -- '--storage-driver' "$PODMAN_ARGS" | head -1 | cut -d: -f1)"
  first_root_line="$(grep -nx -- '--root' "$PODMAN_ARGS" | head -1 | cut -d: -f1)"
  first_runroot_line="$(grep -nx -- '--runroot' "$PODMAN_ARGS" | head -1 | cut -d: -f1)"
  [ "$first_storage_line" -lt "$first_run_line" ]
  [ "$first_root_line"    -lt "$first_run_line" ]
  [ "$first_runroot_line" -lt "$first_run_line" ]

  # 3) The BIB container must inherit STORAGE_DRIVER via -e so the
  #    nested podman picks the same driver. PODMAN_ROOT and
  #    PODMAN_RUNROOT must NOT be -e'd: podman does not honor them as
  #    env-var names natively, and forwarding them via -e is misleading
  #    (operators reading the invocation might assume they're load-
  #    bearing). The real isolation for the nested podman comes from
  #    the bind-mount remap below.
  grep -qx -- '-e' "$PODMAN_ARGS"
  grep -qx 'STORAGE_DRIVER' "$PODMAN_ARGS"
  ! grep -qx 'PODMAN_ROOT'    "$PODMAN_ARGS"
  ! grep -qx 'PODMAN_RUNROOT' "$PODMAN_ARGS"

  # 4) The /var/lib/containers/storage bind-mount must point at the
  #    isolated graphroot, NOT the host default. Otherwise BIB's nested
  #    podman would still scribble in the shared store. THIS is the real
  #    isolation mechanism for the nested podman.
  grep -qFx -- "${PODMAN_ROOT}:/var/lib/containers/storage" "$PODMAN_ARGS"
  ! grep -qFx -- '/var/lib/containers/storage:/var/lib/containers/storage' "$PODMAN_ARGS"
}

@test "build_qcow2: STORAGE_DRIVER alone (no PODMAN_ROOT/RUNROOT) appends only --storage-driver" {
  install_stubs
  export STORAGE_DRIVER=vfs
  # PODMAN_ROOT / PODMAN_RUNROOT intentionally unset.
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/img:latest name "$cfg"
  [ "$status" -eq 0 ]

  grep -qx -- '--storage-driver' "$PODMAN_ARGS"
  grep -qx 'vfs'                  "$PODMAN_ARGS"
  ! grep -qx -- '--root'    "$PODMAN_ARGS"
  ! grep -qx -- '--runroot' "$PODMAN_ARGS"
  # With PODMAN_ROOT unset, the bind-mount falls back to the host default.
  grep -qFx -- '/var/lib/containers/storage:/var/lib/containers/storage' "$PODMAN_ARGS"
}

@test "build_qcow2: PODMAN_ROOT alone repoints the BIB storage bind-mount" {
  install_stubs
  export PODMAN_ROOT="${BATS_TEST_TMPDIR}/just-graphroot"
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/img:latest name "$cfg"
  [ "$status" -eq 0 ]

  grep -qx -- '--root' "$PODMAN_ARGS"
  grep -qFx -- "${PODMAN_ROOT}" "$PODMAN_ARGS"
  # The bind-mount source must follow PODMAN_ROOT.
  grep -qFx -- "${PODMAN_ROOT}:/var/lib/containers/storage" "$PODMAN_ARGS"
}

# ---------------------------------------------------------------------------
# Pre-flight: PODMAN_ROOT must be a writable directory (mkdir -p if missing)
# ---------------------------------------------------------------------------

@test "build_qcow2: PODMAN_ROOT auto-creates the graphroot when it does not exist" {
  install_stubs
  export PODMAN_ROOT="${BATS_TEST_TMPDIR}/nonexistent/graphroot"
  source_lib

  [ ! -d "$PODMAN_ROOT" ]   # precondition

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/img:latest name "$cfg"
  [ "$status" -eq 0 ]

  # The pre-flight in build_qcow2 must have created PODMAN_ROOT before
  # the podman invocation.
  [ -d "$PODMAN_ROOT" ]
}

@test "build_qcow2: PODMAN_ROOT mkdir-failure fails loudly before invoking podman" {
  install_stubs
  # Point PODMAN_ROOT at a path whose parent is a regular file — mkdir -p
  # will fail with "Not a directory". This is a more portable way to
  # exercise the pre-flight failure path than relying on filesystem perm
  # bits (the bats container runs as root and DAC-overrides 0500 dirs).
  local blocker="${BATS_TEST_TMPDIR}/notadir"
  : > "$blocker"
  export PODMAN_ROOT="${blocker}/cannot-mkdir-here"
  source_lib

  local cfg
  cfg="$(make_cfg)"
  run build_qcow2 localhost/img:latest name "$cfg"
  [ "$status" -ne 0 ]
  [[ "$output" == *"PODMAN_ROOT"* ]]
  # podman must NOT have been called — the precheck fails first.
  [ ! -f "$PODMAN_ARGS" ]
}

# ---------------------------------------------------------------------------
# Helper: podman_storage_opts (consumed by scripts/build-*.sh +
# scripts/deploy-cluster.sh; tested at the helper level here)
# ---------------------------------------------------------------------------

@test "podman_storage_opts: emits nothing when no isolation env vars set" {
  install_stubs
  source_lib

  run podman_storage_opts
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "podman_storage_opts: emits --storage-driver / --root / --runroot when all set" {
  install_stubs
  export STORAGE_DRIVER=vfs
  export PODMAN_ROOT="/tmp/iso-graph"
  export PODMAN_RUNROOT="/tmp/iso-runroot"
  source_lib

  run podman_storage_opts
  [ "$status" -eq 0 ]
  # One flag-token per line, six lines total (3 flag/value pairs).
  [ "$(printf '%s\n' "$output" | wc -l)" -eq 6 ]
  [[ "$output" == *"--storage-driver"* ]]
  [[ "$output" == *"vfs"* ]]
  [[ "$output" == *"--root"* ]]
  [[ "$output" == *"/tmp/iso-graph"* ]]
  [[ "$output" == *"--runroot"* ]]
  [[ "$output" == *"/tmp/iso-runroot"* ]]
}

@test "podman_storage_opts: STORAGE_DRIVER alone emits only --storage-driver pair" {
  install_stubs
  export STORAGE_DRIVER=vfs
  source_lib

  run podman_storage_opts
  [ "$status" -eq 0 ]
  [ "$(printf '%s\n' "$output" | wc -l)" -eq 2 ]
  [[ "$output" == *"--storage-driver"* ]]
  [[ "$output" == *"vfs"* ]]
  [[ "$output" != *"--root"* ]]
  [[ "$output" != *"--runroot"* ]]
}

@test "podman_storage_opts: PODMAN_ROOT alone emits only --root pair" {
  install_stubs
  export PODMAN_ROOT="/tmp/just-root"
  source_lib

  run podman_storage_opts
  [ "$status" -eq 0 ]
  [ "$(printf '%s\n' "$output" | wc -l)" -eq 2 ]
  [[ "$output" == *"--root"* ]]
  [[ "$output" == *"/tmp/just-root"* ]]
  [[ "$output" != *"--storage-driver"* ]]
  [[ "$output" != *"--runroot"* ]]
}
