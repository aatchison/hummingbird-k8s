#!/usr/bin/env bash
#
# run-kube-bench.sh — run aquasec/kube-bench against the live cluster,
# capturing both the master-target and node-target scans, and surface
# FAIL + WARN items. Use the captured output to seed/refresh
# scripts/kube-bench-baseline.txt.
#
# kube-bench scans a running cluster for CIS Kubernetes Benchmark
# compliance. It must run *in* the cluster (it inspects host paths like
# /etc/kubernetes, /var/lib/etcd, /var/lib/kubelet via hostPath mounts
# and uses hostPID), which is why we apply it as a Job rather than
# running the binary locally.
#
# We run two Jobs in sequence:
#   * job-master.yaml — has nodeAffinity for
#     node-role.kubernetes.io/control-plane and tolerates the master
#     NoSchedule taint, so its sections cover 1.x master / 2.x etcd /
#     3.x control-plane-config / 4.x kubelet-on-CP / 5.x policies.
#   * job-node.yaml   — unconstrained, lands on whichever worker node
#     wins scheduling. Covers 4.x worker / 5.x policies again.
#
# Running just job.yaml (the combined one) makes the scan land on
# whichever node happens to be picked, which on a real cluster usually
# means a worker — and you silently lose the control-plane sections.
#
# Usage:
#   KVM_HOST=thegeary bash scripts/run-kube-bench.sh
#   # ...or, if you already have a working kubectl in PATH:
#   KUBECTL=kubectl bash scripts/run-kube-bench.sh
#
# Env:
#   KUBECTL              — kubectl command to use. Default: scripts/kubectl-k8s.sh
#                          (the SSH-tunnel-through-KVM-host wrapper).
#   KUBE_BENCH_VERSION   — kube-bench release tag. Default: v0.15.5.
#   KUBE_BENCH_TIMEOUT   — `kubectl wait` timeout. Default: 5m.
#   KUBE_BENCH_NS        — namespace to run the Jobs in. Default: default.
#   KUBE_BENCH_TARGETS   — space-separated subset of {master, node}.
#                          Default: "master node".
#
# Exit codes:
#   0  All requested target Jobs ran. Combined report on stdout. NOTE:
#      this does NOT reflect whether kube-bench found violations — the
#      baseline is informational, not gating. The script only fails on
#      infrastructure errors (apply / wait / logs).
#   non-zero  A Job failed to schedule / complete / produce logs.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

: "${KUBE_BENCH_VERSION:=v0.15.5}"
: "${KUBE_BENCH_TIMEOUT:=5m}"
: "${KUBE_BENCH_NS:=default}"
: "${KUBE_BENCH_TARGETS:=master node}"
: "${KUBECTL:=${REPO_ROOT}/scripts/kubectl-k8s.sh}"

BASE_URL="https://raw.githubusercontent.com/aquasecurity/kube-bench/${KUBE_BENCH_VERSION}"

log() { printf '[run-kube-bench] %s\n' "$*" >&2; }

# Run kubectl. If KUBECTL is the repo's wrapper, invoke it directly
# (it manages its own podman/SSH-tunnel plumbing); otherwise word-split.
kc() {
  if [[ -x "$KUBECTL" && "$KUBECTL" == *"/kubectl-k8s.sh" ]]; then
    "$KUBECTL" "$@"
  else
    # shellcheck disable=SC2086
    $KUBECTL "$@"
  fi
}

# We track the set of Jobs we created so the EXIT trap can clean them
# up even if we bail partway through.
CREATED_JOBS=()

cleanup() {
  local j
  for j in "${CREATED_JOBS[@]:-}"; do
    [[ -z "$j" ]] && continue
    log "cleaning up Job/${j} in ns/${KUBE_BENCH_NS}"
    kc -n "$KUBE_BENCH_NS" delete job "$j" --ignore-not-found=true \
      --wait=false >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

log "kube-bench version: ${KUBE_BENCH_VERSION}"
log "kubectl:           ${KUBECTL}"
log "namespace:         ${KUBE_BENCH_NS}"
log "targets:           ${KUBE_BENCH_TARGETS}"

# Sanity: make sure we can reach the API server before doing anything
# destructive.
if ! kc version --request-timeout=10s >/dev/null 2>&1; then
  log "FAIL: kubectl can't reach the cluster (KUBECTL=${KUBECTL})"
  log "      try: KVM_HOST=<host> bash scripts/run-kube-bench.sh"
  exit 2
fi

# Informational: name the control-plane node so the operator knows what
# got scanned for the master target.
CP_NODE=$(kc get nodes -l node-role.kubernetes.io/control-plane -o name 2>/dev/null | head -n1 || true)
if [[ -n "$CP_NODE" ]]; then
  log "control-plane node: ${CP_NODE}"
else
  log "WARN: no node labeled node-role.kubernetes.io/control-plane found"
fi

# Run one kube-bench Job and capture its logs. Echoes the captured log
# block (with a header banner) on stdout.
#
# $1 — target ("master" or "node")
run_target() {
  local target="$1"
  local job_name="kube-bench-${target}"
  local job_url="${BASE_URL}/job-${target}.yaml"

  # Track for cleanup before we even apply, so a failed apply still
  # gets a delete attempt (no-op if it never existed).
  CREATED_JOBS+=("$job_name")

  # Wipe any leftover Job from a previous run.
  kc -n "$KUBE_BENCH_NS" delete job "$job_name" --ignore-not-found=true >/dev/null 2>&1 || true

  log "[${target}] applying ${job_url}"
  # kubectl can fetch HTTP(S) manifests itself, which sidesteps stdin
  # plumbing through the podman-wrapped kubectl (the wrapper doesn't
  # pass -i, so `kubectl apply -f -` would see an empty stdin).
  kc -n "$KUBE_BENCH_NS" apply -f "$job_url" >&2

  log "[${target}] waiting up to ${KUBE_BENCH_TIMEOUT} for Job/${job_name}"
  if ! kc -n "$KUBE_BENCH_NS" wait --for=condition=complete \
         --timeout="$KUBE_BENCH_TIMEOUT" "job/${job_name}" >&2; then
    log "[${target}] FAIL: Job did not complete within ${KUBE_BENCH_TIMEOUT}"
    log "[${target}] --- pod status ---"
    kc -n "$KUBE_BENCH_NS" get pods -l "job-name=${job_name}" -o wide >&2 || true
    log "[${target}] --- last logs ---"
    kc -n "$KUBE_BENCH_NS" logs "job/${job_name}" --tail=100 >&2 || true
    return 1
  fi

  log "[${target}] fetching logs"
  local logs
  logs=$(kc -n "$KUBE_BENCH_NS" logs "job/${job_name}")
  if [[ -z "$logs" ]]; then
    log "[${target}] FAIL: kube-bench produced no log output"
    return 1
  fi

  # Banner so the combined baseline file is readable.
  printf '############################################################\n'
  printf '# kube-bench target: %s\n' "$target"
  printf '############################################################\n'
  printf '%s\n' "$logs"
  printf '\n'
}

# Combined output captured here so we can grep it for the stderr
# summary at the end without re-fetching logs.
ALL_LOGS=""
for target in $KUBE_BENCH_TARGETS; do
  case "$target" in
    master|node) ;;
    *) log "FAIL: unknown target '${target}' (expected master or node)"; exit 2 ;;
  esac
  out=$(run_target "$target")
  ALL_LOGS+="$out"$'\n'
  printf '%s' "$out"
done

# Summary to stderr so it doesn't pollute the captured baseline if the
# caller is redirecting stdout to a file.
{
  printf '\n=== kube-bench summary ===\n'
  # The kube-bench report has lines like:
  #   [FAIL] 1.2.5 ...
  #   [WARN] 1.2.6 ...
  #   [PASS] 1.2.7 ...
  # plus a final tally per section:
  #   == Summary master ==
  #   N checks PASS
  #   ...
  printf '%s\n' "$ALL_LOGS" | grep -E '^\[(FAIL|WARN)\]' || true
  printf -- '---\n'
  printf '%s\n' "$ALL_LOGS" | grep -E '(checks PASS|checks FAIL|checks WARN|checks INFO)' || true
} >&2

log "done"
