#!/usr/bin/env bats
#
# Unit tests for issue #271 finding F6 — `scripts/backup-etcd.sh`,
# `scripts/restore-etcd.sh`, and `scripts/rotate-etcd-encryption-key.sh`
# need REPO_ROOT to anchor when sourced from any cwd.
#
# Originally these tests pinned that the etcd scripts called
# `${REPO_ROOT}/scripts/kubectl-k8s.sh` (anchored via REPO_ROOT). After
# the v0.1.0 partial bash->Rust cutover (#353), `kubectl-k8s.sh` was
# removed and the etcd scripts now invoke `hbird kubectl` directly.
# REPO_ROOT is still anchored for the (now smaller) set of sibling
# script references the etcd scripts retain (none today, but the
# preamble remains as boilerplate parity with run-kube-bench.sh).
#
# What these tests check (post-cutover):
#   1. Source-level regression guard: no surviving reference to the
#      deleted scripts/kubectl-k8s.sh; all kubectl calls go via `hbird
#      kubectl` (Rust twin).
#   2. Each script declares `REPO_ROOT=$(cd "$(dirname "$(readlink -f
#      "$0")")/.." && pwd)` near the top of its body — preserved for
#      future sibling-script references.
#   3. Functional: REPO_ROOT preamble still resolves correctly from /tmp.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  BACKUP="${REPO_ROOT}/scripts/backup-etcd.sh"
  RESTORE="${REPO_ROOT}/scripts/restore-etcd.sh"
  ROTATE="${REPO_ROOT}/scripts/rotate-etcd-encryption-key.sh"

  # Isolated $HOME so we never read the operator's real ~/.ssh.
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"

  STUB_DIR="$BATS_TEST_TMPDIR/stub-bin"
  mkdir -p "$STUB_DIR"

  # ssh / scp stubs: record argv, succeed silently. The CP_IP probe
  # in the scripts is satisfied via CP_IP=... in the env (we don't
  # need to stub kubectl-k8s.sh's network path).
  cat > "$STUB_DIR/ssh" <<'EOF'
#!/usr/bin/env bash
# Always succeed; emit nothing.
exit 0
EOF
  cat > "$STUB_DIR/scp" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "$STUB_DIR/ssh" "$STUB_DIR/scp"
}

# ---------------------------------------------------------------------------
# Source-level regression guards: no surviving reference to the deleted
# kubectl-k8s.sh; REPO_ROOT anchor preserved as preamble boilerplate.
# ---------------------------------------------------------------------------

# Source-level regression guards: the deleted kubectl-k8s.sh must not be
# invoked anywhere. We allow comment references that document the
# removal (lines starting with `#` are matched and excluded) but reject
# any executable line that names the script.

@test "#353 cutover: backup-etcd.sh does not invoke scripts/kubectl-k8s.sh" {
  run grep -nE '^[^#]*scripts/kubectl-k8s\.sh' "$BACKUP"
  [ "$status" -ne 0 ]
}

@test "#353 cutover: restore-etcd.sh does not invoke scripts/kubectl-k8s.sh" {
  run grep -nE '^[^#]*scripts/kubectl-k8s\.sh' "$RESTORE"
  [ "$status" -ne 0 ]
}

@test "#353 cutover: rotate-etcd-encryption-key.sh does not invoke scripts/kubectl-k8s.sh" {
  run grep -nE '^[^#]*scripts/kubectl-k8s\.sh' "$ROTATE"
  [ "$status" -ne 0 ]
}

@test "F6: backup-etcd.sh anchors REPO_ROOT via readlink -f \$0" {
  grep -qE 'REPO_ROOT="\$\(cd "\$\(dirname "\$\(readlink -f "\$0"\)"\)/\.\." && pwd\)"' "$BACKUP"
}

@test "F6: restore-etcd.sh anchors REPO_ROOT via readlink -f \$0" {
  grep -qE 'REPO_ROOT="\$\(cd "\$\(dirname "\$\(readlink -f "\$0"\)"\)/\.\." && pwd\)"' "$RESTORE"
}

@test "F6: rotate-etcd-encryption-key.sh anchors REPO_ROOT via readlink -f \$0" {
  grep -qE 'REPO_ROOT="\$\(cd "\$\(dirname "\$\(readlink -f "\$0"\)"\)/\.\." && pwd\)"' "$ROTATE"
}

@test "#353 cutover: backup-etcd.sh references hbird kubectl" {
  grep -qE 'hbird +kubectl' "$BACKUP"
}

@test "#353 cutover: restore-etcd.sh references hbird kubectl" {
  grep -qE 'hbird +kubectl' "$RESTORE"
}

@test "#353 cutover: rotate-etcd-encryption-key.sh references hbird kubectl" {
  grep -qE 'hbird +kubectl' "$ROTATE"
}

# ---------------------------------------------------------------------------
# Functional: invoke each script from /tmp (NOT the repo root) and
# confirm REPO_ROOT resolves to the actual repo location. We extract the
# resolved REPO_ROOT by replacing the body with `echo "$REPO_ROOT"; exit`
# via a `set -x` trace — but that's fragile, so we instead source the
# scripts in a controlled way that triggers ONLY the path-resolution
# preamble (every script has a `set -euo pipefail` shortly after the
# preamble; we can stop execution by feeding an arg that causes early
# exit).
#
# Simpler approach: cd /tmp, run `bash -c 'REPO_ROOT="$(...)"; echo ...'`
# using the exact preamble verbatim. This verifies the resolution
# expression itself is cwd-independent.
# ---------------------------------------------------------------------------

@test "F6: REPO_ROOT preamble resolves correctly when cwd is /tmp (backup-etcd.sh)" {
  cd /tmp
  local resolved
  resolved="$(bash -c 'REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"; printf "%s" "$REPO_ROOT"' "$BACKUP")"
  [ "$resolved" = "$REPO_ROOT" ]
}

@test "F6: REPO_ROOT preamble resolves correctly when cwd is /tmp (restore-etcd.sh)" {
  cd /tmp
  local resolved
  resolved="$(bash -c 'REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"; printf "%s" "$REPO_ROOT"' "$RESTORE")"
  [ "$resolved" = "$REPO_ROOT" ]
}

@test "F6: REPO_ROOT preamble resolves correctly when cwd is /tmp (rotate-etcd-encryption-key.sh)" {
  cd /tmp
  local resolved
  resolved="$(bash -c 'REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"; printf "%s" "$REPO_ROOT"' "$ROTATE")"
  [ "$resolved" = "$REPO_ROOT" ]
}

# ---------------------------------------------------------------------------
# Functional, end-to-end-ish: backup-etcd.sh with CP_IP injected (so it
# never calls kubectl-k8s.sh) runs to completion from /tmp. This proves
# the post-fix script doesn't trip on cwd for any OTHER reason — e.g.
# OUTDIR defaults to ./backups which is cwd-relative-by-design (the
# operator chose that), so we pass an absolute OUTDIR.
# ---------------------------------------------------------------------------

@test "F6: backup-etcd.sh runs from cwd=/tmp with CP_IP injected and stubbed ssh/scp" {
  cd /tmp
  local outdir="$BATS_TEST_TMPDIR/out"
  mkdir -p "$outdir"
  # Pre-create the snapshot file the stubbed scp would have copied.
  # backup-etcd.sh runs `scp ... root@CP:/tmp/snapshot.db "$DST"` and
  # then `du -h "$DST"`. With scp stubbed-to-exit-0, $DST doesn't
  # exist post-scp, so `du` fails. We side-step by stubbing du too
  # (so the script's final `du -h "$DST" | cut -f1` succeeds).
  cat > "$STUB_DIR/du" <<'EOF'
#!/usr/bin/env bash
printf '0\t%s\n' "$2"
EOF
  chmod +x "$STUB_DIR/du"
  # PR #366 round-2 M1 added a `command -v hbird` preflight to
  # backup-etcd.sh. Stub `hbird` so the preflight passes; CP_IP is
  # already pinned via env so the kubectl fallback is never reached.
  cat > "$STUB_DIR/hbird" <<'EOF'
#!/usr/bin/env bash
# Stub: preflight only checks for presence on PATH; never invoked
# with CP_IP pinned.
exit 0
EOF
  chmod +x "$STUB_DIR/hbird"

  run env -i \
    PATH="${STUB_DIR}:/usr/bin:/bin" \
    HOME="$HOME" \
    CP_IP=192.0.2.10 \
    BASH_ENV="" \
    bash "$BACKUP" "$outdir"
  # Output should mention Snapshotting + Saved; exit 0.
  [ "$status" -eq 0 ]
  [[ "$output" == *"Snapshotting etcd on 192.0.2.10"* ]]
  [[ "$output" == *"Saved:"* ]]
}
