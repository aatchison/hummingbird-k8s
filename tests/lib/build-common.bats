#!/usr/bin/env bats
#
# Unit tests for lib/build-common.sh helpers (issue #106).
#
# Run via:
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/lib/
#
# Notes:
#   * curl is *not* present in the bats container, so the GitHub-fetch tests
#     stub `curl` as a shell function in setup() that emits a fixture file.
#   * openssl is also *not* present, so we deliberately avoid exercising the
#     VM_PASSWORD path of _render_user_block in the unit tests. The
#     password-rendering branch is covered by the render-bib-config-snapshot
#     CI job, which runs on a regular runner with openssl.
#   * Each test wipes the env vars build-common.sh consumes before sourcing
#     the library, so leakage from prior tests cannot mask a bug.

setup() {
  # Discover repo root regardless of where bats is invoked from.
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  LIB="${REPO_ROOT}/lib/build-common.sh"
  FIX="${BATS_TEST_DIRNAME}/fixtures"

  # Isolate $HOME so the library never reads the developer's real ~/.ssh.
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  # Wipe inputs that the library defaults from.
  unset SSH_PUBKEY_FILES SSH_PUBKEY_GH_USERS VM_USER VM_USER_GROUPS \
        VM_PASSWORD ENABLE_ROOT_SSH SUDO_USER POOL_DIR BIB BASE_IMAGE
}

# Helper: source the library in a fresh subshell-safe way. `set +e` first so
# `set -euo pipefail` inside the library doesn't break bats's `run`.
source_lib() {
  # shellcheck disable=SC1090
  source "$LIB"
}

# ---------------------------------------------------------------------------
# ssh_pubkey_blob
# ---------------------------------------------------------------------------

@test "ssh_pubkey_blob: concatenates two file-based pubkeys in order" {
  export SSH_PUBKEY_FILES="${FIX}/keys/a.pub:${FIX}/keys/b.pub"
  source_lib
  run ssh_pubkey_blob
  [ "$status" -eq 0 ]
  # First non-empty line is a.pub's key, second is b.pub's.
  echo "$output" | grep -q 'AAAAIFIXTUREKEYAAAAAAAAAAAAAAAAAAAAAAAAAAA01'
  echo "$output" | grep -q 'AAAAIFIXTUREKEYBBBBBBBBBBBBBBBBBBBBBBBBBBB02'
  first="$(echo "$output" | awk 'NF{print; exit}')"
  [[ "$first" == *"AAAAIFIXTUREKEYAAAAAAAAAAAAAAAAAAAAAAAAAAA01"* ]]
}

@test "ssh_pubkey_blob: empty SSH_PUBKEY_FILES yields empty output" {
  export SSH_PUBKEY_FILES=""
  # No GitHub users either.
  source_lib
  run ssh_pubkey_blob
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "ssh_pubkey_blob: dedupes identical keys from file + GitHub sources" {
  # a.pub and gh-user.keys both contain the same first line in this test —
  # synthesize a GitHub fixture that intentionally overlaps with a.pub.
  overlap="${BATS_TEST_TMPDIR}/overlap.keys"
  cat "${FIX}/keys/a.pub" >"$overlap"   # duplicate of a.pub line
  echo 'ssh-ed25519 AAAAUNIQUEFROMGHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH05 gh-only' >>"$overlap"

  export SSH_PUBKEY_FILES="${FIX}/keys/a.pub"
  export SSH_PUBKEY_GH_USERS="fakeuser"

  # Stub curl so ssh_pubkeys_from_github reads the fixture file instead of
  # hitting github.com. The library calls `curl -fsSL https://...`.
  curl() {
    cat "$overlap"
  }
  export -f curl

  source_lib
  run ssh_pubkey_blob
  [ "$status" -eq 0 ]

  # The a.pub line must appear exactly once.
  count="$(echo "$output" | grep -c 'AAAAIFIXTUREKEYAAAAAAAAAAAAAAAAAAAAAAAAAAA01' || true)"
  [ "$count" -eq 1 ]
  # The GH-only line is present.
  echo "$output" | grep -q 'AAAAUNIQUEFROMGHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH05'
}

@test "ssh_pubkey_blob: unreadable file fails the build (loud, not silent)" {
  export SSH_PUBKEY_FILES="${BATS_TEST_TMPDIR}/does-not-exist.pub"
  source_lib
  run ssh_pubkey_blob
  [ "$status" -ne 0 ]
  [[ "$output" == *"not readable"* ]]
}

# ---------------------------------------------------------------------------
# ssh_pubkeys_from_github
# ---------------------------------------------------------------------------

@test "ssh_pubkeys_from_github: empty SSH_PUBKEY_GH_USERS emits nothing" {
  # unset already in setup
  source_lib
  run ssh_pubkeys_from_github
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "ssh_pubkeys_from_github: iterates comma-separated users and concatenates curl output" {
  export SSH_PUBKEY_GH_USERS="alice,bob"

  curl() {
    # The library invokes curl as: curl -fsSL https://github.com/<user>.keys
    # We inspect the last argument to decide what fixture to serve.
    local url="${!#}"
    case "$url" in
      *alice.keys) echo 'ssh-ed25519 AAAAALICEKEYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA alice@host' ;;
      *bob.keys)   echo 'ssh-ed25519 AAAAABOBKEYBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB bob@host'   ;;
      *) return 22 ;;
    esac
  }
  export -f curl

  source_lib
  run ssh_pubkeys_from_github
  [ "$status" -eq 0 ]
  echo "$output" | grep -q 'AAAAALICEKEY'
  echo "$output" | grep -q 'AAAAABOBKEY'
  # alice before bob (the order SSH_PUBKEY_GH_USERS was given in).
  line_alice="$(echo "$output" | grep -n ALICEKEY | head -1 | cut -d: -f1)"
  line_bob="$(echo "$output"   | grep -n BOBKEY   | head -1 | cut -d: -f1)"
  [ "$line_alice" -lt "$line_bob" ]
}

@test "ssh_pubkeys_from_github: tolerates trailing comma and whitespace tokens" {
  export SSH_PUBKEY_GH_USERS="alice, ,"
  curl() {
    local url="${!#}"
    case "$url" in
      *alice.keys) echo 'ssh-ed25519 AAAAALICEKEY alice@host' ;;
      *) return 22 ;;
    esac
  }
  export -f curl
  source_lib
  run ssh_pubkeys_from_github
  [ "$status" -eq 0 ]
  echo "$output" | grep -q 'AAAAALICEKEY'
}

# ---------------------------------------------------------------------------
# _render_user_block
# ---------------------------------------------------------------------------

@test "_render_user_block: emits a valid TOML user block with VM_USER and groups" {
  export SSH_PUBKEY_FILES="${FIX}/keys/a.pub"
  export VM_USER=foo
  export VM_USER_GROUPS=wheel
  source_lib
  run _render_user_block foo 0
  [ "$status" -eq 0 ]
  [[ "$output" == *"[[customizations.user]]"* ]]
  [[ "$output" == *'name = "foo"'* ]]
  [[ "$output" == *'groups = ["wheel"]'* ]]
  [[ "$output" == *'key = """'* ]]
  # The embedded key is from the fixture.
  echo "$output" | grep -q 'AAAAIFIXTUREKEYAAAAAAAAAAAAAAAAAAAAAAAAAAA01'
}

@test "_render_user_block: multi-group input renders TOML array correctly" {
  export SSH_PUBKEY_FILES="${FIX}/keys/a.pub"
  export VM_USER_GROUPS="wheel, docker"
  source_lib
  run _render_user_block foo 0
  [ "$status" -eq 0 ]
  [[ "$output" == *'groups = ["wheel", "docker"]'* ]]
}

@test "_render_user_block: name=root omits the groups line even when VM_USER_GROUPS is set" {
  export SSH_PUBKEY_FILES="${FIX}/keys/a.pub"
  export VM_USER_GROUPS=wheel
  source_lib
  run _render_user_block root 0
  [ "$status" -eq 0 ]
  [[ "$output" != *"groups = "* ]]
  [[ "$output" == *'name = "root"'* ]]
}

# ---------------------------------------------------------------------------
# render_bib_config — orchestration only (snapshot diff lives in CI)
# ---------------------------------------------------------------------------

@test "render_bib_config: ENABLE_ROOT_SSH=0 emits no root user block" {
  export SSH_PUBKEY_FILES="${FIX}/keys/a.pub"
  export VM_USER=foo
  export ENABLE_ROOT_SSH=0
  source_lib
  run render_bib_config
  [ "$status" -eq 0 ]
  # Exactly one [[customizations.user]] header.
  count="$(echo "$output" | grep -c '\[\[customizations.user\]\]' || true)"
  [ "$count" -eq 1 ]
  [[ "$output" == *'name = "foo"'* ]]
  [[ "$output" != *'name = "root"'* ]]
}

@test "render_bib_config: ENABLE_ROOT_SSH=1 emits both VM_USER and root blocks" {
  export SSH_PUBKEY_FILES="${FIX}/keys/a.pub"
  export VM_USER=core
  export ENABLE_ROOT_SSH=1
  source_lib
  run render_bib_config
  [ "$status" -eq 0 ]
  count="$(echo "$output" | grep -c '\[\[customizations.user\]\]' || true)"
  [ "$count" -eq 2 ]
  [[ "$output" == *'name = "core"'* ]]
  [[ "$output" == *'name = "root"'* ]]
}

# ---------------------------------------------------------------------------
# setup_logging (issue #190)
# ---------------------------------------------------------------------------

@test "setup_logging: log() prints '<prefix> <msg>' to stderr" {
  source_lib
  setup_logging "[unit-test]"
  run bash -c 'source "'"$LIB"'"; setup_logging "[unit-test]"; log "hello"' 3>&-
  [ "$status" -eq 0 ]
  # bats' `run` merges stdout+stderr by default.
  [[ "$output" == "[unit-test] hello" ]]
}

@test "setup_logging: fail() prints ERROR and exits 1" {
  source_lib
  run bash -c 'source "'"$LIB"'"; setup_logging "[unit-test]"; fail "boom"; echo NOT_REACHED'
  [ "$status" -eq 1 ]
  [[ "$output" == "[unit-test] ERROR: boom" ]]
  [[ "$output" != *"NOT_REACHED"* ]]
}

@test "setup_logging: re-invocation switches prefix in place" {
  source_lib
  setup_logging "[first]"
  run bash -c 'source "'"$LIB"'"; setup_logging "[first]"; setup_logging "[second]"; log "msg"'
  [ "$status" -eq 0 ]
  [[ "$output" == "[second] msg" ]]
}

# ---------------------------------------------------------------------------
# ssh_opts_array / ssh_opts_array_no_identity (issue #190)
# ---------------------------------------------------------------------------

@test "ssh_opts_array: includes -i SSH_PRIVKEY_FILE and the canonical hardened opt set" {
  export SSH_PRIVKEY_FILE=/tmp/fake-key
  source_lib
  ssh_opts_array OPTS
  # Spot-check the canonical opts. Each -o appears as a separate element so
  # we can iterate easily.
  [[ " ${OPTS[*]} " == *" -i /tmp/fake-key "* ]]
  [[ " ${OPTS[*]} " == *" StrictHostKeyChecking=no "* ]]
  [[ " ${OPTS[*]} " == *" UserKnownHostsFile=/dev/null "* ]]
  [[ " ${OPTS[*]} " == *" LogLevel=ERROR "* ]]
  [[ " ${OPTS[*]} " == *" ConnectTimeout=10 "* ]]
  [[ " ${OPTS[*]} " == *" BatchMode=yes "* ]]
  # Without --with-controlmaster, ControlMaster must NOT appear.
  [[ " ${OPTS[*]} " != *"ControlMaster"* ]]
}

@test "ssh_opts_array --with-controlmaster: appends ControlMaster/Path/Persist" {
  export SSH_PRIVKEY_FILE=/tmp/fake-key
  source_lib
  ssh_opts_array OPTS --with-controlmaster
  [[ " ${OPTS[*]} " == *" ControlMaster=auto "* ]]
  # Path is rekeyed on $UID + renamed to hbird-ssh-* so two operators on
  # the same host don't collide on the multiplex socket.
  [[ " ${OPTS[*]} " == *" ControlPath=/tmp/hbird-ssh-${UID}-%r@%h:%p "* ]]
  [[ " ${OPTS[*]} " == *" ControlPersist=60s "* ]]
}

@test "ssh_opts_array: missing SSH_PRIVKEY_FILE fails loudly" {
  unset SSH_PRIVKEY_FILE
  source_lib
  run ssh_opts_array OPTS
  [ "$status" -ne 0 ]
  [[ "$output" == *"SSH_PRIVKEY_FILE"* ]]
}

@test "ssh_opts_array_no_identity: omits the -i line" {
  source_lib
  ssh_opts_array_no_identity OPTS
  # No -i flag and no SSH_PRIVKEY_FILE reference at all.
  [[ " ${OPTS[*]} " != *" -i "* ]]
  # Still has the canonical hardened opts.
  [[ " ${OPTS[*]} " == *" BatchMode=yes "* ]]
  [[ " ${OPTS[*]} " == *" StrictHostKeyChecking=no "* ]]
}

@test "ssh_opts_array_no_identity --proxy-jump=HOST: appends ProxyJump option" {
  source_lib
  ssh_opts_array_no_identity OPTS --proxy-jump=geary
  [[ " ${OPTS[*]} " == *" ProxyJump=geary "* ]]
}

@test "ssh_opts_array: unknown flag fails with rc!=0" {
  export SSH_PRIVKEY_FILE=/tmp/fake-key
  source_lib
  run ssh_opts_array OPTS --no-such-flag
  [ "$status" -ne 0 ]
  [[ "$output" == *"unknown flag"* ]]
}

# ---------------------------------------------------------------------------
# resolve_vm_ip (issue #190)
# ---------------------------------------------------------------------------

@test "resolve_vm_ip: parses the first IPv4 from virsh domifaddr output" {
  source_lib
  # Stub virsh so it emits a representative domifaddr line.
  virsh() {
    cat <<'EOF'
 Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 vnet0      52:54:00:aa:bb:cc    ipv4         192.168.122.42/24
EOF
  }
  export -f virsh
  run resolve_vm_ip my-vm
  [ "$status" -eq 0 ]
  [ "$output" = "192.168.122.42" ]
}

@test "resolve_vm_ip: returns non-zero with diagnostic when no IP appears" {
  source_lib
  virsh() { :; }   # emits nothing
  export -f virsh
  run resolve_vm_ip ghost-vm
  [ "$status" -ne 0 ]
  [[ "$output" == *"could not resolve IPv4"* ]]
  [[ "$output" == *"ghost-vm"* ]]
}

@test "resolve_vm_ip: retries until DHCP lease appears" {
  source_lib
  # Stub: empty on call 1, IP on call 2. Counter survives across calls via
  # a file because bash functions can't share state with `run`'s subshell
  # any other way that's portable.
  COUNTER="$BATS_TEST_TMPDIR/counter"
  echo 0 > "$COUNTER"
  virsh() {
    local n
    n=$(cat "$COUNTER")
    n=$((n + 1))
    echo "$n" > "$COUNTER"
    if (( n >= 2 )); then
      echo " vnet0  52:54:00:aa:bb:cc  ipv4  10.0.0.5/24"
    fi
  }
  export -f virsh
  # 5 retries, 0s interval (test should run fast).
  run resolve_vm_ip late-dhcp-vm 5 0
  [ "$status" -eq 0 ]
  [ "$output" = "10.0.0.5" ]
}

# ---------------------------------------------------------------------------
# derive_ssh_privkey_file (issue #190)
# ---------------------------------------------------------------------------

@test "derive_ssh_privkey_file: echoes pub minus .pub when the key file is readable" {
  source_lib
  priv="$BATS_TEST_TMPDIR/id_test"
  pub="${priv}.pub"
  : > "$priv"
  chmod 0600 "$priv"
  : > "$pub"
  run derive_ssh_privkey_file "$pub"
  [ "$status" -eq 0 ]
  [ "$output" = "$priv" ]
}

@test "derive_ssh_privkey_file: errors when the private key is missing" {
  source_lib
  pub="$BATS_TEST_TMPDIR/missing.pub"
  : > "$pub"
  # Note: do NOT create the matching private key.
  run derive_ssh_privkey_file "$pub"
  [ "$status" -ne 0 ]
  [[ "$output" == *"SSH private key not readable"* ]]
}

@test "derive_ssh_privkey_file: hard-fails when input does not end in .pub" {
  source_lib
  # An already-private path (no .pub suffix) — previously this would
  # silently return the input unchanged, then ssh -i would fail with a
  # confusing auth error. Now it must reject up front with rc=2.
  priv="$BATS_TEST_TMPDIR/id_test"
  : > "$priv"
  run derive_ssh_privkey_file "$priv"
  [ "$status" -eq 2 ]
  [[ "$output" == *"must end in .pub"* ]]
}

# ---------------------------------------------------------------------------
# resolve_vm_ip — virsh probe + multi-NIC behavior
# ---------------------------------------------------------------------------

@test "resolve_vm_ip: virsh not on PATH yields a distinct error message" {
  source_lib
  # Strip virsh from PATH so command -v fails. Use a temp dir that
  # contains nothing.
  empty_path="$BATS_TEST_TMPDIR/emptybin"
  mkdir -p "$empty_path"
  # `unset -f virsh` defensively in case a prior test exported one.
  unset -f virsh 2>/dev/null || true
  PATH="$empty_path" run resolve_vm_ip my-vm
  [ "$status" -ne 0 ]
  [[ "$output" == *"virsh not installed"* ]]
}

@test "resolve_vm_ip: multi-NIC VM emits a warning to stderr and returns first IP" {
  source_lib
  virsh() {
    cat <<'EOF'
 Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 vnet0      52:54:00:aa:bb:cc    ipv4         192.168.122.42/24
 vnet1      52:54:00:dd:ee:ff    ipv4         10.0.0.5/24
EOF
  }
  export -f virsh
  run resolve_vm_ip multi-nic-vm
  [ "$status" -eq 0 ]
  # Stdout (in `run`'s merged $output) carries the first IP; the
  # warning is emitted to stderr (also merged). Both must appear.
  [[ "$output" == *"192.168.122.42"* ]]
  [[ "$output" == *"WARNING"* ]]
  [[ "$output" == *"2 IPv4 addresses"* ]]
}

# ---------------------------------------------------------------------------
# HBIRD_AUTOLOAD_CONFIG_LOCAL gating
# ---------------------------------------------------------------------------

@test "HBIRD_AUTOLOAD_CONFIG_LOCAL=0 (default): config.local.sh is NOT sourced" {
  # Create a config.local.sh at the repo root that sets a marker env var.
  # If the lib autoloads it unconditionally, the marker leaks into the
  # post-source environment. We sanity-check by sourcing the lib in a
  # subshell so the marker can't pollute the host bats process.
  marker_file="$BATS_TEST_TMPDIR/local.sh"
  cat >"$marker_file" <<'EOF'
export HBIRD_AUTOLOAD_MARKER=tripped
EOF
  # Symlink it into the repo root as config.local.sh — but the repo
  # already has a real one in development, so we use a sandbox: copy
  # lib/build-common.sh into a temp tree and point it at our marker.
  sandbox="$BATS_TEST_TMPDIR/repo"
  mkdir -p "$sandbox/lib"
  cp "$LIB" "$sandbox/lib/build-common.sh"
  cp "$marker_file" "$sandbox/config.local.sh"

  # Source in a clean subshell with the flag unset.
  result="$(unset HBIRD_AUTOLOAD_CONFIG_LOCAL; bash -c "source '$sandbox/lib/build-common.sh' && printf '%s' \"\${HBIRD_AUTOLOAD_MARKER:-<unset>}\"")"
  [[ "$result" == "<unset>" ]]
}

@test "verify-hardening.sh wires through ssh_opts_array_no_identity (no dead helper)" {
  # Static grep — the helper was introduced for verify-hardening and
  # must remain a live caller to justify its existence. If this fails,
  # either restore the call site OR drop the helper from the lib +
  # docs (see PR #202 review HIGH 1).
  vh="${REPO_ROOT}/scripts/verify-hardening.sh"
  [ -r "$vh" ]
  grep -q 'ssh_opts_array_no_identity' "$vh"
}

@test "HBIRD_AUTOLOAD_CONFIG_LOCAL=1: config.local.sh IS sourced" {
  marker_file="$BATS_TEST_TMPDIR/local.sh"
  cat >"$marker_file" <<'EOF'
export HBIRD_AUTOLOAD_MARKER=tripped
EOF
  sandbox="$BATS_TEST_TMPDIR/repo"
  mkdir -p "$sandbox/lib"
  cp "$LIB" "$sandbox/lib/build-common.sh"
  cp "$marker_file" "$sandbox/config.local.sh"

  result="$(HBIRD_AUTOLOAD_CONFIG_LOCAL=1 bash -c "source '$sandbox/lib/build-common.sh' && printf '%s' \"\${HBIRD_AUTOLOAD_MARKER:-<unset>}\"")"
  [[ "$result" == "tripped" ]]
}
