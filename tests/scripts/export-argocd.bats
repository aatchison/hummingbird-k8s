#!/usr/bin/env bats
#
# Unit tests for scripts/export-argocd.sh (issue #189).
#
# Focus areas:
#   * rewrite_kubeconfig() — YAML rewrite roundtrip against
#     tests/fixtures/admin.conf. Asserts that the kubeadm-default key
#     lines (cluster, context, user, current-context) are rewritten,
#     while non-key lines — YAML comments and base64 PEM blobs that
#     happen to contain the literal substring "kubernetes" — are
#     byte-identical pre/post.
#   * Both the yq path (when Go yq is present) AND the sed fallback
#     (yq stripped from PATH) are exercised.
#   * --server URL validation — hostile inputs (newline, quote,
#     command-substitution) are rejected before any SSH happens.
#   * --force refuse-to-clobber on existing output files.
#
# This test file deliberately sources scripts/export-argocd.sh, relying
# on the sourced-mode short-circuit near the top of the script (it
# returns 0 before the SSH fetch/write flow when BASH_SOURCE[0] != $0).

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  SCRIPT="${REPO_ROOT}/scripts/export-argocd.sh"
  FIX_ADMIN="${REPO_ROOT}/tests/fixtures/admin.conf"
  # Isolated $HOME so we never read the operator's real ~/.ssh.
  export HOME="$BATS_TEST_TMPDIR/home"
  mkdir -p "$HOME"
}

# Strip any yq dir from PATH so the sed fallback is reachable.
_strip_yq_from_path() {
  # Remove every PATH entry whose basename contains "yq"; also remove
  # entries where a `yq` binary is present (Go yq is sometimes installed
  # into /usr/local/bin alongside other tools).
  local newpath="" entry
  IFS=':' read -ra _parts <<<"$PATH"
  for entry in "${_parts[@]}"; do
    [[ -z "$entry" ]] && continue
    if [[ -x "${entry}/yq" ]]; then
      continue
    fi
    if [[ "$entry" == *yq* ]]; then
      continue
    fi
    if [[ -n "$newpath" ]]; then
      newpath="${newpath}:${entry}"
    else
      newpath="$entry"
    fi
  done
  echo "$newpath"
}

# Source the script (triggers the sourced-mode short-circuit; we get
# rewrite_kubeconfig + detect_yq_flavor in our shell without running the
# main flow).
_source_script() {
  # shellcheck disable=SC1090
  source "$SCRIPT"
}

# Helper: extract a single line by key (the leading whitespace + key:
# pattern). Returns the matched line verbatim.
_line_by_key() {
  local file="$1" pattern="$2"
  grep -E "$pattern" "$file" || true
}

# ---------------------------------------------------------------------------
# rewrite_kubeconfig — sed fallback path (always runnable, no yq needed)
# ---------------------------------------------------------------------------

@test "rewrite_kubeconfig (sed path): server URL rewritten to --server value" {
  local saved_path="$PATH"
  PATH="$(_strip_yq_from_path)" _source_script
  PATH="$saved_path"

  local kc="$BATS_TEST_TMPDIR/admin.conf"
  cp "$FIX_ADMIN" "$kc"

  PATH="$(_strip_yq_from_path)" rewrite_kubeconfig "$kc" \
    "https://10.0.0.7:6443" "test-ctx"

  grep -qE '^[[:space:]]+server:[[:space:]]+https://10.0.0.7:6443$' "$kc"
}

@test "rewrite_kubeconfig (sed path): key lines rewritten to context name" {
  local saved_path="$PATH"
  PATH="$(_strip_yq_from_path)" _source_script
  PATH="$saved_path"

  local kc="$BATS_TEST_TMPDIR/admin.conf"
  cp "$FIX_ADMIN" "$kc"

  PATH="$(_strip_yq_from_path)" rewrite_kubeconfig "$kc" \
    "https://1.2.3.4:6443" "my-ctx"

  # cluster name (clusters[0].name) — line is `  name: my-ctx` (2-space indent)
  grep -qE '^[[:space:]]+name:[[:space:]]+my-ctx$' "$kc"
  # cluster ref inside context (contexts[0].context.cluster)
  grep -qE '^[[:space:]]+cluster:[[:space:]]+my-ctx$' "$kc"
  # user ref inside context
  grep -qE '^[[:space:]]+user:[[:space:]]+my-ctx$' "$kc"
  # current-context (no leading whitespace)
  grep -qE '^current-context:[[:space:]]+my-ctx$' "$kc"
  # No kubernetes-admin / kubernetes-admin@kubernetes / cluster-name=kubernetes
  # references should remain on key lines.
  ! grep -qE '^(current-context|[[:space:]]+name|[[:space:]]+cluster|[[:space:]]+user):[[:space:]]+kubernetes(-admin)?(@kubernetes)?$' "$kc"
}

@test "rewrite_kubeconfig (sed path): YAML comment line is byte-identical" {
  local saved_path="$PATH"
  PATH="$(_strip_yq_from_path)" _source_script
  PATH="$saved_path"

  local kc="$BATS_TEST_TMPDIR/admin.conf"
  cp "$FIX_ADMIN" "$kc"

  # Capture the comment BEFORE rewrite — must survive verbatim.
  local before
  before="$(grep -F '# kubernetes-the-platform' "$FIX_ADMIN")"

  PATH="$(_strip_yq_from_path)" rewrite_kubeconfig "$kc" \
    "https://1.2.3.4:6443" "renamed"

  local after
  after="$(grep -F '# kubernetes-the-platform' "$kc")"
  [ "$before" = "$after" ]
}

@test "rewrite_kubeconfig (sed path): base64 PEM-data lines containing 'kubernetes' are byte-identical" {
  local saved_path="$PATH"
  PATH="$(_strip_yq_from_path)" _source_script
  PATH="$saved_path"

  local kc="$BATS_TEST_TMPDIR/admin.conf"
  cp "$FIX_ADMIN" "$kc"

  # Snapshot the three base64 lines pre-rewrite. The fixture embeds the
  # literal substring "kubernetes" inside each — exactly the case the
  # anchored sed must NOT touch.
  local ca_before cc_before ck_before
  ca_before="$(grep -E '^[[:space:]]+certificate-authority-data:' "$FIX_ADMIN")"
  cc_before="$(grep -E '^[[:space:]]+client-certificate-data:'    "$FIX_ADMIN")"
  ck_before="$(grep -E '^[[:space:]]+client-key-data:'            "$FIX_ADMIN")"

  PATH="$(_strip_yq_from_path)" rewrite_kubeconfig "$kc" \
    "https://1.2.3.4:6443" "renamed"

  local ca_after cc_after ck_after
  ca_after="$(grep -E '^[[:space:]]+certificate-authority-data:' "$kc")"
  cc_after="$(grep -E '^[[:space:]]+client-certificate-data:'    "$kc")"
  ck_after="$(grep -E '^[[:space:]]+client-key-data:'            "$kc")"

  [ "$ca_before" = "$ca_after" ]
  [ "$cc_before" = "$cc_after" ]
  [ "$ck_before" = "$ck_after" ]
}

# ---------------------------------------------------------------------------
# rewrite_kubeconfig — yq path (only when Go yq is on PATH)
# ---------------------------------------------------------------------------

@test "rewrite_kubeconfig (yq path): server URL + key lines rewritten (skipped when Go yq absent)" {
  if ! command -v yq >/dev/null 2>&1; then
    skip "yq not installed"
  fi
  if ! yq --version 2>&1 | grep -qiE 'mikefarah|go-yaml'; then
    skip "Go yq (mikefarah) not on PATH (found a different yq flavor)"
  fi

  _source_script

  local kc="$BATS_TEST_TMPDIR/admin.conf"
  cp "$FIX_ADMIN" "$kc"

  rewrite_kubeconfig "$kc" "https://9.9.9.9:6443" "yq-ctx"

  # Server URL is rewritten (yq emits the same indentation as kubeadm).
  yq -e '.clusters[0].cluster.server == "https://9.9.9.9:6443"' "$kc" >/dev/null
  yq -e '.clusters[0].name == "yq-ctx"' "$kc" >/dev/null
  yq -e '.contexts[0].name == "yq-ctx"' "$kc" >/dev/null
  yq -e '.contexts[0].context.cluster == "yq-ctx"' "$kc" >/dev/null
  yq -e '.contexts[0].context.user == "yq-ctx"' "$kc" >/dev/null
  yq -e '."current-context" == "yq-ctx"' "$kc" >/dev/null
  yq -e '.users[0].name == "yq-ctx"' "$kc" >/dev/null
}

@test "rewrite_kubeconfig (yq path): comment + PEM-data lines preserved verbatim (skipped when Go yq absent)" {
  if ! command -v yq >/dev/null 2>&1; then
    skip "yq not installed"
  fi
  if ! yq --version 2>&1 | grep -qiE 'mikefarah|go-yaml'; then
    skip "Go yq (mikefarah) not on PATH (found a different yq flavor)"
  fi

  _source_script

  local kc="$BATS_TEST_TMPDIR/admin.conf"
  cp "$FIX_ADMIN" "$kc"

  # Comment line — yq is supposed to preserve YAML comments, but its
  # behavior here is what we care about asserting (regression catch if a
  # future yq version starts dropping them).
  local cmt_before
  cmt_before="$(grep -F '# kubernetes-the-platform' "$FIX_ADMIN")"

  rewrite_kubeconfig "$kc" "https://1.2.3.4:6443" "renamed"

  grep -F "$cmt_before" "$kc" >/dev/null
  # The base64 blobs must still contain the literal "kubernetes" substring
  # exactly as written in the fixture (yq mustn't normalize / re-encode).
  grep -F 'certificate-authority-data: TEhTfixtureinertCAdatakubernetes' "$kc" >/dev/null
}

# ---------------------------------------------------------------------------
# detect_yq_flavor — pure helper
# ---------------------------------------------------------------------------

@test "detect_yq_flavor: emits '0' when yq is not on PATH" {
  local saved_path="$PATH"
  PATH="$(_strip_yq_from_path)" _source_script
  PATH="$saved_path"

  PATH="$(_strip_yq_from_path)" run detect_yq_flavor
  [ "$status" -eq 0 ]
  [ "$output" = "0" ]
}

# ---------------------------------------------------------------------------
# --server injection rejection — these run the script end-to-end. The
# --server validator fires BEFORE the CONFIG sourcing + SSH path, so a
# bogus URL must yield non-zero + no output file. We provide a minimal
# CONFIG so the script gets past the CONFIG-readability check before
# reaching the validator.
# ---------------------------------------------------------------------------

# Synthesize a minimal-but-valid CONFIG that satisfies the script's
# early checks. SSH_PUBKEY_FILE points at a stub file inside HOME so we
# don't even attempt to look at real ~/.ssh.
_make_stub_config() {
  local cfg="$BATS_TEST_TMPDIR/cluster.conf"
  local stub_pub="$BATS_TEST_TMPDIR/stub.pub"
  : > "$stub_pub"
  : > "${stub_pub%.pub}"
  chmod 0600 "${stub_pub%.pub}"
  cat > "$cfg" <<EOF
CP_NAME=hbird-cp1
SSH_PUBKEY_FILE=${stub_pub}
CP_IP=127.0.0.1
EOF
  echo "$cfg"
}

@test "--server: rejects newline-containing URL before any output is written" {
  local cfg out
  cfg="$(_make_stub_config)"
  out="$BATS_TEST_TMPDIR/out.yaml"
  # Embed a literal newline via $'\n'.
  run env CONFIG="$cfg" bash "$SCRIPT" \
    --server $'https://1.2.3.4:6443\nrm -rf /' --output "$out"
  [ "$status" -ne 0 ]
  [[ "$output" == *"invalid --server URL"* ]]
  [ ! -e "$out" ]
}

@test "--server: rejects URL containing a single-quote" {
  local cfg out
  cfg="$(_make_stub_config)"
  out="$BATS_TEST_TMPDIR/out.yaml"
  run env CONFIG="$cfg" bash "$SCRIPT" \
    --server "https://host'evil:6443" --output "$out"
  [ "$status" -ne 0 ]
  [[ "$output" == *"invalid --server URL"* ]]
  [ ! -e "$out" ]
}

@test "--server: rejects URL containing a command-substitution sigil" {
  local cfg out
  cfg="$(_make_stub_config)"
  out="$BATS_TEST_TMPDIR/out.yaml"
  run env CONFIG="$cfg" bash "$SCRIPT" \
    --server 'https://$(whoami):6443' --output "$out"
  [ "$status" -ne 0 ]
  [[ "$output" == *"invalid --server URL"* ]]
  [ ! -e "$out" ]
}

# ---------------------------------------------------------------------------
# --force refuse-to-clobber
# ---------------------------------------------------------------------------

@test "--force absent + output exists: refuses with diagnostic mentioning --force" {
  local cfg out
  cfg="$(_make_stub_config)"
  out="$BATS_TEST_TMPDIR/existing.yaml"
  # Pre-create the output file.
  : > "$out"
  # NB: provide --server with a valid value so we exit on the --force
  # check, not earlier.
  run env CONFIG="$cfg" bash "$SCRIPT" \
    --server "https://1.2.3.4:6443" --output "$out"
  [ "$status" -ne 0 ]
  [[ "$output" == *"--force"* ]]
  [[ "$output" == *"already exists"* ]]
}

# ---------------------------------------------------------------------------
# --help precedence: must succeed even when CONFIG is unset (and must
# print the usage block including --proxy-jump). Regression catch for the
# original parse-order bug where `: "${CONFIG:?...}"` ran first and
# operators discovered the script via --help would see "CONFIG required"
# and exit 1.
# ---------------------------------------------------------------------------

@test "--help (no CONFIG): succeeds and prints the usage block including --proxy-jump" {
  # Deliberately leave CONFIG unset. The early --help pre-pass must
  # short-circuit before the CONFIG check fires.
  run env -u CONFIG bash "$SCRIPT" --help
  [ "$status" -eq 0 ]
  [[ "$output" == *"Usage:"* ]]
  [[ "$output" == *"--proxy-jump"* ]]
}

@test "-h (no CONFIG): same precedence as --help" {
  run env -u CONFIG bash "$SCRIPT" -h
  [ "$status" -eq 0 ]
  [[ "$output" == *"Usage:"* ]]
}

# ---------------------------------------------------------------------------
# --proxy-jump robustness:
#   1. --proxy-jump (space-form, no value) followed by another flag must
#      fail loudly instead of silently consuming the next flag as a host.
#   2. --proxy-jump= (explicit-empty) with KVM_HOST exported must NOT
#      fall through to KVM_HOST — the operator's explicit "disable"
#      intent has to win.
#   3. --proxy-jump rejects values containing shell metacharacters that
#      escape the -o value (whitespace, semicolon, dollar, etc).
# ---------------------------------------------------------------------------

@test "--proxy-jump (space-form) without a value rejects '--force' as the host" {
  local cfg out
  cfg="$(_make_stub_config)"
  out="$BATS_TEST_TMPDIR/out.yaml"
  # `--proxy-jump --force` — without the `--*` check the space-form arm
  # captures `--force` as PROXY_JUMP, the regex accepts it (`-` is in
  # the char class), and `-o ProxyJump=--force` lands in argv. The
  # explicit `--*` check makes that case loud.
  run env CONFIG="$cfg" bash "$SCRIPT" \
    --proxy-jump --force --output "$out"
  [ "$status" -ne 0 ]
  [[ "$output" == *"did you forget the host"* ]]
  [ ! -e "$out" ]
}

# Helper: build a stub-bin dir with `ssh` that captures argv and a
# `mktemp` that works under busybox (the bats container uses busybox
# coreutils, which doesn't accept GNU's `mktemp -t TEMPLATE` form).
_make_stub_bin() {
  local stub_dir="$BATS_TEST_TMPDIR/stub-bin"
  mkdir -p "$stub_dir"
  cat > "$stub_dir/ssh" <<'EOF'
#!/usr/bin/env bash
# Capture full argv to a known file so the test can grep for ProxyJump=.
printf '%s\n' "$@" > "${SSH_ARGV_CAPTURE}"
# Emit a minimal valid-looking admin.conf body so the post-fetch
# "looks like a kubeconfig" check passes — we want the script to keep
# going past the SSH step (where the ProxyJump decision has already
# been made) so any later failures don't mask the argv we care about.
cat <<'YML'
apiVersion: v1
kind: Config
clusters:
- name: kubernetes
  cluster:
    server: https://1.2.3.4:6443
contexts:
- name: kubernetes-admin@kubernetes
  context:
    cluster: kubernetes
    user: kubernetes-admin
current-context: kubernetes-admin@kubernetes
users:
- name: kubernetes-admin
YML
EOF
  chmod +x "$stub_dir/ssh"
  # Busybox mktemp doesn't grok `mktemp -t TEMPLATE.suffix`. Shim it
  # to a portable path-only form. The script trusts mktemp's stdout
  # as the temp path.
  cat > "$stub_dir/mktemp" <<'EOF'
#!/usr/bin/env bash
# Argv comes in as `mktemp -t argocd-kubeconfig-XXXXXX.yaml`. Just
# emit a unique path in $TMPDIR (or /tmp).
path="${TMPDIR:-/tmp}/argocd-kubeconfig-$$-$RANDOM.yaml"
: > "$path"
chmod 0600 "$path"
printf '%s\n' "$path"
EOF
  chmod +x "$stub_dir/mktemp"
  echo "$stub_dir"
}

@test "--proxy-jump= (explicit-empty) with KVM_HOST exported disables ProxyJump" {
  local cfg out stub_dir argv_capture
  cfg="$(_make_stub_config)"
  out="$BATS_TEST_TMPDIR/out.yaml"
  stub_dir="$(_make_stub_bin)"
  argv_capture="$BATS_TEST_TMPDIR/ssh-argv"

  # KVM_HOST=geary exported globally — explicit --proxy-jump= must
  # override it back to empty for this invocation.
  run env CONFIG="$cfg" \
    KVM_HOST=geary \
    SSH_ARGV_CAPTURE="$argv_capture" \
    PATH="${stub_dir}:${PATH}" \
    bash "$SCRIPT" --proxy-jump= --output "$out"

  # We don't care about the script's final exit code — the ProxyJump
  # decision was made before the SSH stub ran, and we only need the
  # captured argv.
  [ -f "$argv_capture" ]
  ! grep -qF 'ProxyJump=' "$argv_capture"
}

@test "--proxy-jump= (explicit-empty) without KVM_HOST: still no ProxyJump" {
  local cfg out stub_dir argv_capture
  cfg="$(_make_stub_config)"
  out="$BATS_TEST_TMPDIR/out.yaml"
  stub_dir="$(_make_stub_bin)"
  argv_capture="$BATS_TEST_TMPDIR/ssh-argv"

  run env -u KVM_HOST \
    CONFIG="$cfg" \
    SSH_ARGV_CAPTURE="$argv_capture" \
    PATH="${stub_dir}:${PATH}" \
    bash "$SCRIPT" --proxy-jump= --output "$out"

  [ -f "$argv_capture" ]
  ! grep -qF 'ProxyJump=' "$argv_capture"
}

@test "--proxy-jump absent + KVM_HOST exported: ProxyJump=KVM_HOST appears in ssh argv" {
  local cfg out stub_dir argv_capture
  cfg="$(_make_stub_config)"
  out="$BATS_TEST_TMPDIR/out.yaml"
  stub_dir="$(_make_stub_bin)"
  argv_capture="$BATS_TEST_TMPDIR/ssh-argv"

  run env CONFIG="$cfg" \
    KVM_HOST=geary \
    SSH_ARGV_CAPTURE="$argv_capture" \
    PATH="${stub_dir}:${PATH}" \
    bash "$SCRIPT" --output "$out"

  [ -f "$argv_capture" ]
  grep -qF 'ProxyJump=geary' "$argv_capture"
}

# ---------------------------------------------------------------------------
# Symlink OUTPUT rejection
# ---------------------------------------------------------------------------

@test "--output points at a symlink: rejected with readlink-resolved hint" {
  local cfg
  cfg="$(_make_stub_config)"
  local target="$BATS_TEST_TMPDIR/real-kubeconfig.yaml"
  : > "$target"
  local link="$BATS_TEST_TMPDIR/link-kubeconfig.yaml"
  ln -s "$target" "$link"

  # --force so the existing-file refusal doesn't fire first.
  run env CONFIG="$cfg" bash "$SCRIPT" \
    --server "https://1.2.3.4:6443" --output "$link" --force
  [ "$status" -ne 0 ]
  [[ "$output" == *"symlink"* ]]
}
