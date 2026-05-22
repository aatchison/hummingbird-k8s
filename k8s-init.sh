#!/bin/bash
set -euo pipefail

MARKER=/var/lib/k8s-init.done
[[ -f "$MARKER" ]] && { echo "k8s-init already ran"; exit 0; }

# Recover from a half-finished previous init.
# If admin.conf is missing but our generated kubeadm config is present,
# a previous kubeadm init died partway. Reset so we can retry cleanly.
if [[ ! -f /etc/kubernetes/admin.conf && -f /etc/kubernetes/kubeadm-init.yaml ]]; then
  kubeadm reset --force --cri-socket=unix:///var/run/crio/crio.sock || true
  rm -f /etc/kubernetes/kubeadm-init.yaml /etc/kubernetes/encryption-config.yaml
fi

# Build-time configuration: APISERVER_EXTRA_SANS is baked into /etc/hummingbird/k8s-init.env
# at image build time (Containerfile.k8s ARG → write env file).
if [[ -r /etc/hummingbird/k8s-init.env ]]; then
  # shellcheck disable=SC1091
  source /etc/hummingbird/k8s-init.env
fi

POD_CIDR="${POD_CIDR:-10.244.0.0/16}"
APISERVER_EXTRA_SANS="${APISERVER_EXTRA_SANS:-127.0.0.1,localhost}"
CONTROL_PLANE_ENDPOINT="${CONTROL_PLANE_ENDPOINT:-}"

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
nodeRegistration:
  criSocket: unix:///var/run/crio/crio.sock
  kubeletExtraArgs:
    - name: protect-kernel-defaults
      value: "true"
---
apiVersion: kubeadm.k8s.io/v1beta4
kind: ClusterConfiguration
${CONTROL_PLANE_ENDPOINT_YAML}networking:
  podSubnet: ${POD_CIDR}
apiServer:
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

kubeadm init --config=/etc/kubernetes/kubeadm-init.yaml

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
# --wait blocks until pods become Ready; --set kubeProxyReplacement=false to
# keep kube-proxy as the L4 plane (matches what flannel did; Cilium's
# kube-proxy-replacement is opt-in via a separate PR).
KUBECONFIG=/etc/kubernetes/admin.conf cilium install \
  --version 1.16.5 \
  --set kubeProxyReplacement=false \
  --wait \
  --wait-duration 5m

touch "$MARKER"
