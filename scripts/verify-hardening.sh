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
# If CP_IP is unset the script tries `resolve_cp_ip "$CP_NAME"` first
# (workstation-aware — uses `ssh $KVM_HOST virsh ...` when set), then
# falls back to auto-detecting the first control-plane node's
# InternalIP via `kubectl get nodes`. See issue #271 (F4).
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
# kubectl transport:
#   The CP-node-IP lookup fallback uses `$KUBECTL` (default:
#   scripts/kubectl-k8s.sh, the SSH-tunnel-through-KVM-host wrapper).
#   Operators on the workstation get a tunneled kubectl for free;
#   operators on the CP itself can pass `KUBECTL=kubectl` to use the
#   native binary. Matches the run-kube-bench.sh pattern. (#271 F4)
#
#   The PodSecurity apply/delete probes go DIRECT via `ssh root@$CP_IP
#   "kubectl --kubeconfig=/etc/kubernetes/admin.conf ..."` — bypassing the
#   wrapper. This is required because the wrapper's port-forward bootstrap
#   consumes stdin from the apply heredoc before kubectl ever runs (#332).
#   Aligns with the Rust twin (#330) which already takes the direct-SSH
#   shape via `cp_kubectl_with_stdin_lenient`.
#
# Env:
#   CONFIG    — Optional path to cluster.local.conf. When set, sourced
#               so CP_NAME / KVM_HOST come from the topology file (the
#               same pattern `make kubectl` uses).
#   CP_NAME   — libvirt domain of the control plane. Default: hummingbird-k8s.
#               Used by resolve_cp_ip when CP_IP is not set.
#   CP_IP     — Explicit override; bypasses resolution.
#   KVM_HOST  — SSH alias of the KVM host (workstation case).
#   KUBECTL   — kubectl command. Default: scripts/kubectl-k8s.sh.
#
# Exits 0 only if all four checks pass. Prints a summary either way.

set -euo pipefail

_VH_REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
# shellcheck source=../lib/build-common.sh
source "${_VH_REPO_ROOT}/lib/build-common.sh"
setup_logging "[verify-hardening]"

# Pull CP_NAME / KVM_HOST from a CONFIG file when provided (matches the
# `make get-kubeconfig` / `make update-cluster` pattern). Falls through
# to config.local.sh + defaults if unset.
if [[ -n "${CONFIG:-}" ]]; then
  [[ -r "$CONFIG" ]] || { log "FAIL: CONFIG not readable: $CONFIG"; exit 2; }
  # shellcheck disable=SC1090
  source "$CONFIG"
elif [[ -r "${_VH_REPO_ROOT}/config.local.sh" ]]; then
  # shellcheck disable=SC1091
  source "${_VH_REPO_ROOT}/config.local.sh"
fi

: "${CP_NAME:=hummingbird-k8s}"
: "${KUBECTL:=${_VH_REPO_ROOT}/scripts/kubectl-k8s.sh}"

# kc() — invoke kubectl. If KUBECTL is the repo's SSH-tunnel wrapper,
# call it directly (it manages its own podman/SSH plumbing); otherwise
# word-split so KUBECTL='kubectl --kubeconfig ...' still works. Mirrors
# the wrapper in scripts/run-kube-bench.sh.
kc() {
  if [[ -x "$KUBECTL" && "$KUBECTL" == *"/kubectl-k8s.sh" ]]; then
    "$KUBECTL" "$@"
  else
    # shellcheck disable=SC2086
    $KUBECTL "$@"
  fi
}

# SSH options: this script does not have an SSH_PUBKEY_FILE in scope — it
# expects the operator's ssh-agent or ~/.ssh/config to surface the right
# identity. ssh_opts_array_no_identity emits the canonical hardened opt
# set without `-i SSH_PRIVKEY_FILE`, which is exactly what we want.
# KVM_HOST (when set) adds ProxyJump so the libvirt NAT subnet doesn't
# need to be routable from the dev machine.
#
# #362: when we're already on the KVM host (e.g. deploy-cluster re-exec,
# or operator running the verifier directly on the hypervisor as root),
# ProxyJump=$KVM_HOST would become `ssh root@KVM_HOST` from KVM_HOST
# itself — sshd typically denies root login and the call hangs on a
# password prompt that never satisfies (exit 255). In that case the
# libvirt NAT subnet IS already routable (we're on it), so we can ssh
# directly to root@CP_IP without a ProxyJump. Hostname detection
# mirrors scripts/lib/ssh-wrap.sh.
_vh_local_host="$(hostname -s 2>/dev/null || hostname)"
if [[ -n "${KVM_HOST:-}" && "${_vh_local_host}" == "${KVM_HOST%%.*}" ]]; then
  log "already on KVM_HOST (${KVM_HOST}); dropping ProxyJump for direct CP SSH (#362)"
  unset KVM_HOST
fi
unset _vh_local_host
if [[ -n "${KVM_HOST:-}" ]]; then
  ssh_opts_array_no_identity SSH_OPTS --proxy-jump="${KVM_HOST}"
else
  ssh_opts_array_no_identity SSH_OPTS
fi

# Helper: run a command "on the CP host". If CP_IP looks like localhost (the
# script is itself executing on the CP, e.g. driven by integration-boot.sh
# via scp+ssh), run locally to avoid an SSH-back-to-self that needs a key.
#
# We log in as root over SSH, so no remote `sudo` is needed for any of
# the on_cp commands below (audit-log read, ps grep). That means we do
# NOT need `ssh -t` here — TTY-on-SSH triggers \r line endings and we'd
# have to strip them downstream. See scripts/kubectl-k8s.sh (issue #247)
# for the inverse case (sudo on the KVM host needs -t, and we strip \r).
# Stdin handling: bash forwards stdin to whatever child process the
# function exec's, so `on_cp "kubectl apply -f -" <<EOF ... EOF` works for
# both the localhost (bash -c) and remote (ssh) branches. This is what the
# PSA-rejection check below relies on after the #332 fix — see comment at
# check 1/3 for the kubectl-k8s.sh-heredoc bug that motivated the switch.
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
#
# Resolution order (issue #271 F4):
#   1. CP_IP env var explicit override (unchanged).
#   2. resolve_cp_ip "$CP_NAME" — workstation-aware libvirt query. Goes
#      through `ssh $KVM_HOST virsh ...` when KVM_HOST is set, or falls
#      back to local virsh on the KVM host itself. No kubectl needed.
#   3. `$KUBECTL get nodes` — last resort. Uses the wrapper (which
#      already tunnels through KVM_HOST), so workstation operators
#      don't need a hand-wired local kubectl context.
if [[ -z "${CP_IP:-}" ]]; then
  log "CP_IP not set, trying resolve_cp_ip ${CP_NAME}"
  if CP_IP_TRY="$(resolve_cp_ip "$CP_NAME" 2>/dev/null)" \
     && [[ -n "$CP_IP_TRY" ]]; then
    CP_IP="$CP_IP_TRY"
  else
    log "resolve_cp_ip failed, falling back to ${KUBECTL} get nodes"
    CP_IP="$(kc get nodes -l node-role.kubernetes.io/control-plane \
      -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' \
      2>/dev/null || true)"
  fi
  if [[ -z "$CP_IP" ]]; then
    log "FAIL: could not resolve CP IP. Set CP_IP=<ip> explicitly, or set"
    log "      KVM_HOST=<ssh-alias> so resolve_cp_ip can query libvirt"
    log "      remotely. CP_NAME=${CP_NAME}"
    exit 2
  fi
fi
log "CP_IP=$CP_IP"

# --- 1. PodSecurity restricted ---------------------------------------------

log "check 1/3: PodSecurity restricted rejects a privileged pod"
# Issue #332: this check used to route through `kc apply -f -` (the
# scripts/kubectl-k8s.sh wrapper). That wrapper's port-forward bootstrap
# phase consumes stdin before kubectl runs, so the heredoc was silently
# swallowed and kubectl reported "no objects passed to apply" — the PSA
# marker never appeared on stderr and the check FAILed against a working
# cluster. Fix: go direct via `on_cp` (ssh root@CP `kubectl ... apply -f -`),
# which preserves heredoc stdin end-to-end. Aligns with the Rust twin's
# `cp_kubectl_with_stdin_lenient` shape (rust/crates/hbird-cli/src/
# cp_kubectl.rs) that PR #330 already had right.
ps_out="$(on_cp 'kubectl --kubeconfig=/etc/kubernetes/admin.conf apply -f -' <<'EOF' 2>&1 || true
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
# Same direct-SSH path as the apply above — keeps the PSA check independent
# of the `kc` wrapper end-to-end.
on_cp 'kubectl --kubeconfig=/etc/kubernetes/admin.conf -n default delete pod verify-hardening-privileged-probe --ignore-not-found=true --wait=false' >/dev/null 2>&1 || true

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
