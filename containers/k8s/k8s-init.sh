#!/bin/bash
set -euo pipefail

# Regenerate SSH host keys on first boot so two VMs from the same image
# don't share identical host keys (#80). Must run BEFORE any other init
# work — if SSH is restarted later the operator's first connection would
# otherwise pin the baked-in keys in their known_hosts.
SSH_HOSTKEY_MARKER=/var/lib/ssh-host-keys-regenerated
if [[ ! -f "$SSH_HOSTKEY_MARKER" ]]; then
  rm -f /etc/ssh/ssh_host_*
  ssh-keygen -A
  systemctl restart sshd
  touch "$SSH_HOSTKEY_MARKER"
fi

MARKER=/var/lib/k8s-init.done
[[ -f "$MARKER" ]] && { echo "k8s-init already ran"; exit 0; }

# cloud-init's write_files/network/config stages all run BEFORE
# multi-user.target — which is when this service starts — so user-data
# write_files (e.g. worker-join.env) and hostname have already landed.
# cloud-final.service (runcmd, packages) runs AFTER multi-user.target
# by design; we do NOT wait for it (waiting deadlocks against
# multi-user.target itself — see #171, #172).


# cilium install (cilium-cli) needs a writable cache dir; systemd services
# have no $HOME by default. Set both XDG_CACHE_HOME and HOME so cilium-cli's
# helm cache + kubeconfig path resolution work.
export HOME=/root
export XDG_CACHE_HOME=/var/cache

# Recover from a half-finished previous init.
# If a previous run produced the kubeadm config but didn't set the done
# marker, something failed mid-init (kubeadm init crashed, or kubeadm init
# finished but `cilium install` later failed and we exited non-zero before
# touching $MARKER). Either way, reset so the next attempt is clean. The
# pki / manifests / *.conf cleanup is needed because kubeadm refuses to
# re-init if those exist.
if [[ ! -f "$MARKER" && -f /etc/kubernetes/kubeadm-init.yaml ]]; then
  kubeadm reset --force --cri-socket=unix:///var/run/crio/crio.sock || true
  rm -f /etc/kubernetes/kubeadm-init.yaml /etc/kubernetes/encryption-config.yaml
  rm -rf /etc/kubernetes/pki /etc/kubernetes/manifests /etc/kubernetes/*.conf || true
fi

# Build-time configuration: APISERVER_EXTRA_SANS is baked into /etc/hummingbird/k8s-init.env
# at image build time (containers/k8s/Containerfile ARG → write env file).
if [[ -r /etc/hummingbird/k8s-init.env ]]; then
  # shellcheck disable=SC1091
  source /etc/hummingbird/k8s-init.env
fi

# Per-cluster overrides: cloud-init write_files lands
# /etc/hummingbird/k8s-init-local.env before this service starts (see
# render_cp_user_data in scripts/deploy-cluster.sh). Sourced AFTER the
# baked env so a cluster.local.conf POD_CIDR/SERVICE_CIDR wins over the
# image-build ARG defaults. Kept as a separate file so a bootc image
# update never clobbers operator config (and vice versa).
if [[ -r /etc/hummingbird/k8s-init-local.env ]]; then
  # shellcheck disable=SC1091
  source /etc/hummingbird/k8s-init-local.env
fi

POD_CIDR="${POD_CIDR:-10.244.0.0/16}"
SERVICE_CIDR="${SERVICE_CIDR:-10.96.0.0/12}"
APISERVER_EXTRA_SANS="${APISERVER_EXTRA_SANS:-127.0.0.1,localhost}"
CONTROL_PLANE_ENDPOINT="${CONTROL_PLANE_ENDPOINT:-}"

# ---- Node identity pinning --------------------------------------------------
# NODE_IP pins BOTH the kubelet's --node-ip and the apiserver's
# advertise-address. Without it kubelet AUTODETECTS the node address, and
# on a dual-NIC VM (EXTRA_NETWORK, deploy-cluster.sh) that autodetection
# is not guaranteed to pick the primary NIC — observed live: kubelet
# selected the second NIC's static address as InternalIP on every node
# even though that NIC carried no default route.
#
# Derivation: the `src` of the default route. On these VMs that is the
# primary NIC BY CONSTRUCTION — the second NIC's cloud-init
# network-config ships no gateway and no DHCP, and its IPv6 is disabled
# by a first-boot runcmd, so it can never own a default route. Using the
# route's own `src` rather than the interface's first address matches
# what kubelet/kubeadm autodetection would pick when the NIC carries
# several global addresses. An operator can override by setting NODE_IP
# in k8s-init-local.env.
if [[ -z "${NODE_IP:-}" ]]; then
  # Bounded retry: at first boot the DHCP lease and default route may not
  # have landed yet, and this unit is Type=oneshot with no Restart — a
  # single-shot check would leave the node permanently uninitialised.
  for _ in $(seq 1 30); do
    NODE_IP="$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}')"
    [[ -n "$NODE_IP" ]] && break
    sleep 1
  done
fi
if [[ -z "${NODE_IP:-}" ]]; then
  # Fall back to the default-route interface's first global address for
  # routes that carry no src hint.
  _def_if="$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')"
  [[ -n "$_def_if" ]] && NODE_IP="$(ip -4 -o addr show dev "$_def_if" scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"
fi
if [[ -z "${NODE_IP:-}" ]]; then
  echo "FATAL: could not derive NODE_IP (no IPv4 default route after 30s, or no global IPv4 on its interface). Set NODE_IP in /etc/hummingbird/k8s-init-local.env." >&2
  exit 1
fi
echo "node identity pinned to ${NODE_IP} (advertise-address + kubelet node-ip)"

swapoff -a || true
modprobe overlay
modprobe br_netfilter
sysctl --system >/dev/null

# Wait briefly for cri-o socket
for _ in $(seq 1 30); do
  [[ -S /var/run/crio/crio.sock ]] && break
  sleep 1
done

# Generate a random 32-byte base64 key for etcd encryption at rest.
# The key lives only on disk inside the VM; it is NOT baked into the image.
ENC_KEY="$(head -c 32 /dev/urandom | base64 -w0)"

# /etc/kubernetes itself must be world-traversable (0755) so the unprivileged
# wheel user can read admin.conf (mode 0644) for sudoless `kubectl get nodes`.
# Sensitive material under /etc/kubernetes/pki/ is locked down by kubeadm
# itself, and encryption-config.yaml below is explicitly chmod'd 0600.
install -d -m 0755 -o root -g root /etc/kubernetes
umask 077
# NOTE: this heredoc is unquoted on purpose so ${ENC_KEY} expands.
# ENC_KEY is base64 (no YAML metacharacters) — safe.
cat >/etc/kubernetes/encryption-config.yaml <<EOF
apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources:
      - secrets
      - configmaps
    providers:
      - aesgcm:
          keys:
            - name: bootstrap
              secret: ${ENC_KEY}
      - identity: {}
EOF
chmod 0600 /etc/kubernetes/encryption-config.yaml
chown root:root /etc/kubernetes/encryption-config.yaml
unset ENC_KEY

# Build certSANs YAML list from comma-separated APISERVER_EXTRA_SANS.
# Caller is responsible for ensuring SANs are plain DNS/IP tokens
# (no YAML metacharacters). Defaults are safe.
CERT_SANS_YAML=""
IFS=',' read -r -a _sans <<<"$APISERVER_EXTRA_SANS"
for s in "${_sans[@]}"; do
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  [[ -z "$s" ]] && continue
  CERT_SANS_YAML+="    - ${s}"$'\n'
done

# Build controlPlaneEndpoint line (with its own trailing newline) when set,
# so the next key (`networking:`) always lands on a fresh line whether or
# not CONTROL_PLANE_ENDPOINT is empty.
CONTROL_PLANE_ENDPOINT_YAML=""
if [[ -n "$CONTROL_PLANE_ENDPOINT" ]]; then
  CONTROL_PLANE_ENDPOINT_YAML="controlPlaneEndpoint: ${CONTROL_PLANE_ENDPOINT}"$'\n'
fi

cat >/etc/kubernetes/kubeadm-init.yaml <<EOF
apiVersion: kubeadm.k8s.io/v1beta4
kind: InitConfiguration
localAPIEndpoint:
  advertiseAddress: ${NODE_IP}
nodeRegistration:
  criSocket: unix:///var/run/crio/crio.sock
  kubeletExtraArgs:
    - name: protect-kernel-defaults
      value: "true"
    - name: rotate-certificates
      value: "true"
    - name: node-ip
      value: "${NODE_IP}"
---
apiVersion: kubeadm.k8s.io/v1beta4
kind: ClusterConfiguration
${CONTROL_PLANE_ENDPOINT_YAML}networking:
  podSubnet: ${POD_CIDR}
  serviceSubnet: ${SERVICE_CIDR}
apiServer:
  timeoutForControlPlane: 5m0s
  extraArgs:
    - name: encryption-provider-config
      value: /etc/kubernetes/encryption-config.yaml
    - name: admission-control-config-file
      value: /etc/kubernetes/admission-control-config.yaml
    - name: audit-policy-file
      value: /etc/kubernetes/audit-policy.yaml
    - name: audit-log-path
      value: /var/log/kubernetes/k8s-audit.log
    - name: audit-log-maxsize
      value: "100"
    - name: audit-log-maxbackup
      value: "5"
    - name: request-timeout
      value: "5m"
  extraVolumes:
    - name: encryption-config
      hostPath: /etc/kubernetes/encryption-config.yaml
      mountPath: /etc/kubernetes/encryption-config.yaml
      readOnly: true
      pathType: File
    - name: admission-control-config
      hostPath: /etc/kubernetes/admission-control-config.yaml
      mountPath: /etc/kubernetes/admission-control-config.yaml
      readOnly: true
      pathType: File
    - name: audit-policy
      hostPath: /etc/kubernetes/audit-policy.yaml
      mountPath: /etc/kubernetes/audit-policy.yaml
      readOnly: true
      pathType: File
    - name: audit-log
      hostPath: /var/log/kubernetes
      mountPath: /var/log/kubernetes
      readOnly: false
      pathType: DirectoryOrCreate
  certSANs:
${CERT_SANS_YAML}
EOF
chmod 0600 /etc/kubernetes/kubeadm-init.yaml
chown root:root /etc/kubernetes/kubeadm-init.yaml

kubeadm init --config=/etc/kubernetes/kubeadm-init.yaml --skip-phases=addon/kube-proxy

# Single-node: let workloads schedule on the control-plane
KUBECONFIG=/etc/kubernetes/admin.conf kubectl taint nodes --all \
  node-role.kubernetes.io/control-plane- 2>/dev/null || true

# World-readable admin.conf for the wheel user to use kubectl.
# kubeadm v1.31 may tighten /etc/kubernetes to 0755 (or even 0700 in some
# distros' kubelet packaging) during init — re-assert traversability so the
# wheel user can actually read admin.conf. See issue #36.
chmod 0755 /etc/kubernetes
install -m 0644 /etc/kubernetes/admin.conf /etc/kubernetes/admin.conf.world
ln -sf /etc/kubernetes/admin.conf.world /etc/profile.d/kubeconfig-symlink-target
chmod 0644 /etc/kubernetes/admin.conf

# CNI: Cilium installed via cilium-cli (baked at image build time).
# --wait blocks until pods become Ready. kubeProxyReplacement=true makes
# Cilium handle all L4 service routing in eBPF; kube-proxy is therefore
# skipped at kubeadm init time (--skip-phases=addon/kube-proxy above).
# k8sServiceHost/Port are required when kpr=true since Cilium can no longer
# discover the apiserver via the (absent) kube-proxy ClusterIP — "auto"
# resolves to the kubeadm-published control-plane endpoint.
# Hubble + relay are enabled for flow visibility (#78); the UI is left off
# to keep the install lean — operators reach Hubble via
# `cilium hubble port-forward` + `hubble observe`.
# ipam.operator.clusterPoolIPv4PodCIDRList MUST be passed explicitly:
# with ipam=cluster-pool (the chart default) Cilium IGNORES kubeadm's
# networking.podSubnet and falls back to its own chart default of
# 10.0.0.0/8 — silently swallowing any RFC-1918 10.x LAN the nodes can
# reach, while `kubectl get nodes -o jsonpath={.spec.podCIDR}` keeps
# reporting the (unused) kubeadm allocation. Observed live on
# hbird-geary: pods allocated from 10.0.{0,1,2}.0/24 while node.spec
# claimed 10.244.x — a latent collision with the 10.0.0.0/24 LAN.
KUBECONFIG=/etc/kubernetes/admin.conf cilium install \
  --version 1.17.16 \
  --set ipam.operator.clusterPoolIPv4PodCIDRList="${POD_CIDR}" \
  --set ipam.operator.clusterPoolIPv4MaskSize=24 \
  --set kubeProxyReplacement=true \
  --set k8sServiceHost=auto \
  --set k8sServicePort=6443 \
  --set hubble.enabled=true \
  --set hubble.relay.enabled=true \
  --set hubble.ui.enabled=false \
  --wait \
  --wait-duration 5m

echo "applying baseline cluster posture (metrics-server, quotas, SA token restriction, NFS CSI)..."
KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f /etc/kubernetes/metrics-server.yaml
KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f /etc/kubernetes/default-ns-quota.yaml
KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f /etc/kubernetes/restrict-sa-token-mount.yaml

# csi-driver-nfs. Inert until a StorageClass names a server + share (the shipped
# nfs-storageclass.example.yaml is NOT applied — see docs/nfs-storage.md), so
# installing it unconditionally costs one small controller Deployment plus a
# node DaemonSet and commits the cluster to nothing.
KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f /etc/kubernetes/csi-driver-nfs.yaml

# Best-effort wait for metrics-server to become Ready. Don't fail the whole
# init if it's slow — the deployment is applied and will reconcile.
KUBECONFIG=/etc/kubernetes/admin.conf kubectl -n kube-system rollout status \
  deployment/metrics-server --timeout=120s || true

touch "$MARKER"
