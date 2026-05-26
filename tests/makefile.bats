#!/usr/bin/env bats
# tests/makefile.bats — Static integration tests for the top-level Makefile.
#
# Exercises `make -n` (dry-run) and `make help` so we can verify recipe
# wiring without touching libvirt / podman / sudo. Designed to run in a
# vanilla Ubuntu/Fedora CI runner — no KVM host required.
#
# Coverage (post-#216, with the k3s flavor and legacy single-VM targets
# retired):
#   1.  `make help` exits 0 and lists at least 10 targets.
#   2.  `make -n deploy-cluster CONFIG=…` -> bash scripts/deploy-cluster.sh
#   3.  `make -n destroy-cluster CONFIG=…` -> bash scripts/destroy-cluster.sh
#   4.  `make -n update-cluster CONFIG=…` -> bash scripts/update-cluster.sh
#   5.  `make -n verify-all`     -> all three verify-* scripts in sequence
#   6.  `make -n backup-etcd`    -> bash scripts/backup-etcd.sh (no LABEL)
#   7.  `make -n switch-to-ghcr` -> bash scripts/switch-to-ghcr.sh
#   8.  `make -n clean`          -> includes both clean-vms and clean-images
#   9.  Every `.PHONY:` target is also a defined target (and vice versa).
#   10. Every target referenced from `make -n <t>` actually exists.
#   11. `make image-*` recipes contain no `sudo` (rootless contract, #230).
#   12. `make push-image-k8s` IMAGE_TAG/GHCR_REGISTRY interpolate correctly.
#   13. `make help` lists push-image-{k8s,worker,all}.

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
  # clean-vms recipe: `virsh ... destroy` loop over hummingbird-* domains.
  [[ "$output" == *"virsh"* ]] && [[ "$output" == *"hummingbird-"* ]]
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
