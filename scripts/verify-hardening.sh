#!/usr/bin/env bash
# Shared SSH/virsh/log helpers live in lib/build-common.sh; see docs/development.md.
# verify-hardening.sh — Verify three control-plane hardening controls on a
# running hummingbird-k8s cluster:
#
#   1. PodSecurity Admission rejects a privileged pod in `default`
#      (defaults to `restricted`, see docs/security-hardening.md #1).
#   2. apiserver audit log is being written on the CP host.
#   3. kubelet is running with `--protect-kernel-defaults=true`.
#   4. kubelet is running with `--rotate-certificates=true` (#121).
#
# Run from anywhere with kubectl + SSH access to the CP:
#
#   CP_IP=192.168.122.42 ./scripts/verify-hardening.sh
#
# If CP_IP is unset the script tries to auto-detect the first control-plane
# node's InternalIP via `kubectl get nodes`.
#
# SSH transport:
#   The CP normally lives on the libvirt default NAT subnet
#   (192.168.122.0/24), which is not routable from a dev host. Set
#   `KVM_HOST` to the SSH alias of the KVM hypervisor and the script
#   will tunnel SSH through it with `ssh -o ProxyJump=$KVM_HOST`:
#
#     KVM_HOST=thegeary ./scripts/verify-hardening.sh
#
#   This is the same env var `kubectl-k8s.sh` uses. If `KVM_HOST` is
#   unset the script falls back to direct SSH (works when run from the
#   KVM host itself or from any host with a route to the CP).
#
# Exits 0 only if all three checks pass. Prints a summary either way.

set -euo pipefail

# shellcheck source=../lib/build-common.sh
source "$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)/lib/build-common.sh"
setup_logging "[verify-hardening]"

# SSH options: this script does not have an SSH_PUBKEY_FILE in scope — it
# expects the operator's ssh-agent or ~/.ssh/config to surface the right
# identity. ssh_opts_array_no_identity emits the canonical hardened opt
# set without `-i SSH_PRIVKEY_FILE`, which is exactly what we want.
# KVM_HOST (when set) adds ProxyJump so the libvirt NAT subnet doesn't
# need to be routable from the dev machine.
if [[ -n "${KVM_HOST:-}" ]]; then
  ssh_opts_array_no_identity SSH_OPTS --proxy-jump="${KVM_HOST}"
else
  ssh_opts_array_no_identity SSH_OPTS
fi

# Helper: run a command "on the CP host". If CP_IP looks like localhost (the
# script is itself executing on the CP, e.g. driven by integration-boot.sh
# via scp+ssh), run locally to avoid an SSH-back-to-self that needs a key.
on_cp() {
  if [[ "${CP_IP}" == "127.0.0.1" || "${CP_IP}" == "localhost" ]]; then
    bash -c "$1"
  else
    ssh "${SSH_OPTS[@]}" "root@${CP_IP}" "$1"
  fi
}

# Tracks each check independently so the summary shows partial state.
ps_ok=0
audit_ok=0
kubelet_ok=0

# --- resolve CP IP ----------------------------------------------------------

if [[ -z "${CP_IP:-}" ]]; then
  log "CP_IP not set, auto-detecting via kubectl"
  if ! command -v kubectl >/dev/null 2>&1; then
    log "FAIL: kubectl not on PATH and CP_IP not set"
    exit 2
  fi
  CP_IP="$(kubectl get nodes -l node-role.kubernetes.io/control-plane \
    -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' \
    2>/dev/null || true)"
  if [[ -z "$CP_IP" ]]; then
    log "FAIL: could not auto-detect CP InternalIP; set CP_IP=<ip> explicitly"
    exit 2
  fi
fi
log "CP_IP=$CP_IP"

# --- 1. PodSecurity restricted ---------------------------------------------

log "check 1/3: PodSecurity restricted rejects a privileged pod"
ps_out="$(kubectl apply -f - <<'EOF' 2>&1 || true
apiVersion: v1
kind: Pod
metadata:
  name: verify-hardening-privileged-probe
  namespace: default
spec:
  hostPID: true
  containers:
  - name: x
    image: busybox
    securityContext:
      privileged: true
EOF
)"
# Best-effort cleanup in case it somehow got admitted (it shouldn't).
kubectl -n default delete pod verify-hardening-privileged-probe \
  --ignore-not-found=true --wait=false >/dev/null 2>&1 || true

if printf '%s' "$ps_out" | grep -q 'violates PodSecurity'; then
  log "  PASS: privileged pod rejected by PodSecurity"
  ps_ok=1
else
  log "  FAIL: privileged pod was not rejected by PodSecurity"
  log "  apiserver output: $ps_out"
fi

# --- 2. apiserver audit log -------------------------------------------------

# After #50 (Cilium PR) the path is /var/log/kubernetes/k8s-audit.log; for
# now the apiserver writes to /var/log/k8s-audit.log. Check the new path
# first, fall back to the legacy one.
log "check 2/3: apiserver audit log is non-empty on the CP host"
audit_cmd='for p in /var/log/kubernetes/k8s-audit.log /var/log/k8s-audit.log; do
  if [ -s "$p" ]; then printf "OK %s\n" "$p"; exit 0; fi
done
exit 1'
if audit_path="$(on_cp "$audit_cmd" 2>/dev/null)"; then
  log "  PASS: audit log present (${audit_path#OK })"
  audit_ok=1
else
  log "  FAIL: audit log missing or empty"
  log "  expected /var/log/kubernetes/k8s-audit.log (post-#50) or /var/log/k8s-audit.log"
fi

# --- 3. kubelet --protect-kernel-defaults=true ------------------------------

log "check 3/4: kubelet running with --protect-kernel-defaults=true"
# `ps -ef | grep | head` lets ssh return the matched argv line, which the
# local shell then asserts is non-empty.
kubelet_cmd="ps -ef | grep -- '--protect-kernel-defaults=true' | grep -v grep | head -1"
if kubelet_line="$(on_cp "$kubelet_cmd" 2>/dev/null)" \
   && [[ -n "$kubelet_line" ]]; then
  log "  PASS: kubelet has --protect-kernel-defaults=true"
  kubelet_ok=1
else
  log "  FAIL: kubelet is not running with --protect-kernel-defaults=true"
fi

# --- 4. kubelet --rotate-certificates=true ----------------------------------

log "check 4/4: kubelet running with --rotate-certificates=true (#121)"
rotate_cmd="ps -ef | grep -- '--rotate-certificates=true' | grep -v grep | head -1"
rotate_ok=0
if rotate_line="$(on_cp "$rotate_cmd" 2>/dev/null)" \
   && [[ -n "$rotate_line" ]]; then
  log "  PASS: kubelet has --rotate-certificates=true"
  rotate_ok=1
else
  log "  FAIL: kubelet is not running with --rotate-certificates=true"
fi

# --- summary ----------------------------------------------------------------

pass_label() { [[ "$1" -eq 1 ]] && printf PASS || printf FAIL; }

printf '\n[verify-hardening] summary\n'
printf '  PodSecurity restricted    : %s\n' "$(pass_label "$ps_ok")"
printf '  apiserver audit log       : %s\n' "$(pass_label "$audit_ok")"
printf '  kubelet protect-kernel    : %s\n' "$(pass_label "$kubelet_ok")"
printf '  kubelet rotate-certs      : %s\n' "$(pass_label "$rotate_ok")"

if [[ "$ps_ok" -eq 1 && "$audit_ok" -eq 1 && "$kubelet_ok" -eq 1 && "$rotate_ok" -eq 1 ]]; then
  printf '[verify-hardening] all checks PASSED\n'
  exit 0
fi
printf '[verify-hardening] one or more checks FAILED\n'
exit 1
