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
# Run on the CP host or anywhere kubectl works:
#
#   ./scripts/verify-app-deploy.sh
#
# To route through the tunnel from a client, point KUBECTL at kubectl-k8s.sh:
#
#   KUBECTL=./kubectl-k8s.sh ./scripts/verify-app-deploy.sh
#
# Exits 0 on PASS. Always cleans up the smoketest namespace on EXIT.

set -euo pipefail

NS="smoketest-$(date +%s)"
KCTL="${KUBECTL:-kubectl}"

log() { printf '[verify-app-deploy] %s\n' "$*" >&2; }

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
