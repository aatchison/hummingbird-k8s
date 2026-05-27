#!/usr/bin/env bats
#
# Static drift-fence: worker-init.sh must NOT call `cloud-init status --wait`.
#
# Background — PR #255 → issue #265:
#   PR #255 added `cloud-init status --wait` at the top of
#   containers/k8s-worker/worker-init.sh so the script could read the
#   persistent hostname (`hostnamectl --static`) AFTER cloud-init's
#   hostname module had committed. But:
#
#     - worker-init.service is WantedBy=multi-user.target.
#     - cloud-final.service has After=multi-user.target by default.
#     - `cloud-init status --wait` blocks until cloud-final finishes.
#
#   So worker-init blocks waiting for cloud-final, cloud-final blocks
#   waiting for multi-user.target, multi-user.target blocks waiting for
#   worker-init — three-way deadlock. The cluster hangs at 1/3 Ready
#   until the wait is killed by hand. Same bug class as the
#   k8s-init.sh deadlock fixed by PR #171/#172/#173.
#
#   PR #265 dropped the wait: cloud-init's hostname module runs at the
#   init stage, which completes BEFORE multi-user.target activates, so
#   /etc/hostname is already set by the time worker-init.service runs
#   and `hostnamectl --static` returns the seeded value with no wait.
#
# This fence keeps the bug from sneaking back in via a refactor.
#
# Run via:
#   bats tests/containers/worker-init-no-cloud-init-wait.bats
# OR:
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/containers/

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  WORKER_INIT_SH="$REPO_ROOT/containers/k8s-worker/worker-init.sh"
}

@test "worker-init.sh does not call cloud-init status --wait (#265 regression fence)" {
  [ -f "$WORKER_INIT_SH" ] || {
    echo "worker-init.sh missing at $WORKER_INIT_SH"
    return 1
  }
  # Use grep -F so the pattern is matched literally (no regex surprises).
  # If the string appears anywhere — including a comment — fail loudly so
  # we relitigate the comment wording at review time. Today's worker-init.sh
  # mentions the wait only in a comment explaining WHY we don't call it; if
  # that comment is rewritten and the bare command sneaks back into actual
  # script text, this fence fires. To keep the explanatory comments while
  # still fencing the command, we look for it on a non-comment line.
  if grep -nE '^[^#]*cloud-init status --wait' "$WORKER_INIT_SH"; then
    echo "FAIL: 'cloud-init status --wait' appears on a non-comment line in worker-init.sh"
    echo "      This deadlocks against multi-user.target — see issue #265."
    return 1
  fi
}

@test "worker-init.sh still reads hostnamectl --static (preserves #254 fix)" {
  # PR #265's drop of the wait is only safe because hostnamectl --static
  # is still the source of the persistent hostname. Make sure we didn't
  # accidentally rip that out alongside the wait.
  grep -qF 'hostnamectl --static' "$WORKER_INIT_SH" || {
    echo "FAIL: worker-init.sh no longer reads 'hostnamectl --static'"
    echo "      This would regress the #254 hostname-authority fix."
    return 1
  }
}
