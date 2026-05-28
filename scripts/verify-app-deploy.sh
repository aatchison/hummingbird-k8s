#!/usr/bin/env bash
# verify-app-deploy.sh — End-to-end smoke test of a normal workload on a
# running hummingbird-k8s cluster.
#
# What this verifies:
#   1. The cluster can schedule + run an ordinary nginx Deployment under the
#      cluster-default `restricted` Pod Security Standard.
#   2. A ClusterIP Service routes traffic to the Pod (pod-to-pod networking
#      via the CNI works — flannel today, Cilium after #50/#56).
#   3. A second PSA-restricted Pod (busybox probe) can resolve and reach the
#      Service over HTTP.
#
# Run from a workstation through the KVM host (the common topology — see
# issue #271 F3):
#
#   KVM_HOST=geary ./scripts/verify-app-deploy.sh
#   # ...or:
#   KVM_HOST=geary make verify-app-deploy
#
# When KVM_HOST is set (and no explicit KUBECTL override is provided), the
# script defaults KUBECTL to the in-repo scripts/kubectl-k8s.sh wrapper,
# which tunnels kubectl through the KVM host's libvirt NAT — workstation
# operators don't need a local kubectl context wired to the cluster.
#
# Run on the CP host directly (kubectl on PATH, KUBECONFIG resolves):
#
#   ./scripts/verify-app-deploy.sh
#
# Explicit override (any other kubectl wrapper, or to force a path):
#
#   KUBECTL=./scripts/kubectl-k8s.sh ./scripts/verify-app-deploy.sh
#
# Env:
#   KUBECTL    — kubectl command to use. Default: scripts/kubectl-k8s.sh when
#                KVM_HOST is set; otherwise `kubectl` on PATH. Mirrors the
#                run-kube-bench.sh convention so workstation operators get
#                tunneled kubectl for free (issue #271 F3 recommended fix).
#   KVM_HOST   — SSH alias of the KVM host. When set, selects the tunneled
#                kubectl wrapper by default (above). Threaded through to
#                kubectl-k8s.sh, which uses ssh -t to the KVM host for
#                `sudo virsh domifaddr` so a cold sudo cache can prompt
#                for the operator's password (issue #249).
#   CONFIG     — Optional path to cluster.local.conf. When set, sourced so
#                CP_NAME / KVM_HOST come from the topology file (matches the
#                `make get-kubeconfig` / `make kubectl` / `make nodes`
#                pattern). Forwarded to kubectl-k8s.sh.
#
# Exits 0 on PASS. Always cleans up the smoketest namespace on EXIT.

set -euo pipefail

# Resolve repo root from this script's location so the sibling
# kubectl-k8s.sh wrapper resolves no matter what cwd the operator runs us
# from. Matches the path-anchoring pattern from run-kube-bench.sh and
# backup-etcd.sh (issue #271 F6).
REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"

# CONFIG: optional cluster.local.conf — load so KVM_HOST / CP_NAME come from
# the topology file when the operator drives us through `make`. Mirrors
# kubectl-k8s.sh and the C3 SSH-wrap idiom.
if [[ -n "${CONFIG:-}" ]]; then
  [[ -r "$CONFIG" ]] || { echo "${0##*/}: CONFIG not readable: $CONFIG" >&2; exit 2; }
  # shellcheck disable=SC1090
  source "$CONFIG"
fi

log() { printf '[verify-app-deploy] %s\n' "$*" >&2; }

# #362: when we're already running on the KVM host (e.g. inside a
# deploy-cluster re-exec, or when the operator drives `make
# verify-app-deploy` directly on the hypervisor), KVM_HOST=<this-host>
# would route `kubectl-k8s.sh` through `ssh "$KVM_HOST"` from this host
# — i.e. `ssh root@geary` from geary. The KVM host's sshd typically
# denies root login, so the call hangs on a never-answered password
# prompt and exits 255. The cleanup trap then fires the same wrapper
# again, multiplying the password prompts. Net effect: `make
# deploy-cluster` reports a non-zero exit on a successful deploy.
#
# Detection mirrors scripts/lib/ssh-wrap.sh's hostname-short comparison.
# On hit: there is no usable in-place verifier path (plain `kubectl` is
# generally not on the KVM host's PATH; setting up a tunnel-to-self is
# the bug). Skip with a friendly log and exit 0 — the cluster is up,
# the verify can be re-run from a workstation that has SSH access to
# the KVM host:
#
#     make verify-app-deploy CONFIG=cluster.local.conf KVM_HOST=<alias>
#
# This keeps `deploy-cluster.sh`'s "informational" verify tail honest
# (#353 cutover blocker — bash exit code must reflect deploy success).
if [[ -n "${KVM_HOST:-}" ]]; then
  _vad_local_host="$(hostname -s 2>/dev/null || hostname)"
  if [[ "${_vad_local_host}" == "${KVM_HOST%%.*}" ]]; then
    log "already on KVM_HOST (${KVM_HOST}); skipping in-place verify to avoid ssh-to-self loop (#362)"
    log "  hint: re-run from a workstation with: make verify-app-deploy CONFIG=<conf> KVM_HOST=${KVM_HOST}"
    unset _vad_local_host
    exit 0
  fi
  unset _vad_local_host
fi

# Default KUBECTL: pick the tunneled wrapper when KVM_HOST is set, so
# workstation operators don't have to spell out KUBECTL=… on every call.
# When the operator IS on the CP host (or otherwise has a local kubectl
# wired to the cluster), KVM_HOST is unset and we fall back to plain
# `kubectl` — same default as before this change. (issue #271 F3)
if [[ -z "${KUBECTL:-}" ]]; then
  if [[ -n "${KVM_HOST:-}" ]]; then
    KUBECTL="${REPO_ROOT}/scripts/kubectl-k8s.sh"
  else
    KUBECTL="kubectl"
  fi
fi

NS="smoketest-$(date +%s)"
KCTL="$KUBECTL"

cleanup() {
  log "cleanup: deleting namespace $NS"
  $KCTL delete ns "$NS" --wait=false --ignore-not-found >/dev/null 2>&1 || true
}
trap cleanup EXIT

log "creating namespace $NS"
$KCTL create ns "$NS" >/dev/null

# PSA-restricted-compliant Deployment + Service.
#   - nginx-unprivileged listens on 8080 (no root needed).
#   - runAsNonRoot, allowPrivilegeEscalation=false, drop ALL caps,
#     seccompProfile RuntimeDefault — required by `restricted` PSS.
log "applying nginx Deployment + Service in $NS"
$KCTL apply -n "$NS" -f - <<'EOF' >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nginx
spec:
  replicas: 1
  selector:
    matchLabels:
      app: nginx
  template:
    metadata:
      labels:
        app: nginx
    spec:
      automountServiceAccountToken: false
      containers:
      - name: nginx
        image: nginxinc/nginx-unprivileged:stable
        ports:
        - containerPort: 8080
        securityContext:
          runAsNonRoot: true
          allowPrivilegeEscalation: false
          capabilities:
            drop: [ALL]
          seccompProfile:
            type: RuntimeDefault
---
apiVersion: v1
kind: Service
metadata:
  name: nginx
spec:
  selector:
    app: nginx
  ports:
  - port: 8080
    targetPort: 8080
EOF

log "waiting up to 2m for deployment/nginx to become Available"
if ! $KCTL -n "$NS" wait --for=condition=available --timeout=2m deployment/nginx >&2; then
  log "FAIL: deployment/nginx did not become Available"
  log "  recent events:"
  $KCTL -n "$NS" get events --sort-by=.lastTimestamp >&2 || true
  exit 1
fi

# Probe Pod must also be PSA-restricted-compliant. runAsUser=65534 (nobody),
# drop all caps, seccomp default. `kubectl run --rm -i --restart=Never` waits
# for the Pod to terminate and returns its exit code.
log "probing http://nginx:8080 from an in-cluster busybox pod"
probe_out_file="$(mktemp)"
probe_overrides='{
  "spec": {
    "automountServiceAccountToken": false,
    "containers": [
      {
        "name": "probe",
        "image": "busybox:stable",
        "stdin": true,
        "command": ["sh", "-c", "wget -qO- http://nginx:8080"],
        "securityContext": {
          "runAsNonRoot": true,
          "runAsUser": 65534,
          "allowPrivilegeEscalation": false,
          "capabilities": {"drop": ["ALL"]},
          "seccompProfile": {"type": "RuntimeDefault"}
        }
      }
    ]
  }
}'

if ! $KCTL run probe -n "$NS" --rm -i --restart=Never \
        --image=busybox:stable \
        --overrides="$probe_overrides" \
        >"$probe_out_file" 2>&1; then
  log "FAIL: probe pod exited non-zero"
  cat "$probe_out_file" >&2
  rm -f "$probe_out_file"
  exit 1
fi

if grep -q 'Welcome to nginx' "$probe_out_file"; then
  log "PASS: nginx returned the welcome page over ClusterIP"
  rm -f "$probe_out_file"
else
  log "FAIL: probe did not see the nginx welcome page"
  log "  probe output:"
  cat "$probe_out_file" >&2
  rm -f "$probe_out_file"
  exit 1
fi

log "verify-app-deploy: PASS"
