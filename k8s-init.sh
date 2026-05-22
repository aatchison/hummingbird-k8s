#!/bin/bash
set -euo pipefail

MARKER=/var/lib/k8s-init.done
[[ -f "$MARKER" ]] && { echo "k8s-init already ran"; exit 0; }

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

install -d -m 0700 -o root -g root /etc/kubernetes
umask 077
cat >/etc/kubernetes/encryption-config.yaml <<EOF
apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources:
      - secrets
      - configmaps
    providers:
      - aescbc:
          keys:
            - name: bootstrap
              secret: ${ENC_KEY}
      - identity: {}
EOF
chmod 0600 /etc/kubernetes/encryption-config.yaml
chown root:root /etc/kubernetes/encryption-config.yaml
unset ENC_KEY

# Build certSANs YAML list from comma-separated APISERVER_EXTRA_SANS.
CERT_SANS_YAML=""
IFS=',' read -r -a _sans <<<"$APISERVER_EXTRA_SANS"
for s in "${_sans[@]}"; do
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  [[ -z "$s" ]] && continue
  CERT_SANS_YAML+="    - ${s}"$'\n'
done

CONTROL_PLANE_ENDPOINT_YAML=""
if [[ -n "$CONTROL_PLANE_ENDPOINT" ]]; then
  CONTROL_PLANE_ENDPOINT_YAML="controlPlaneEndpoint: ${CONTROL_PLANE_ENDPOINT}"$'\n'
fi

cat >/etc/kubernetes/kubeadm-init.yaml <<EOF
apiVersion: kubeadm.k8s.io/v1beta3
kind: InitConfiguration
nodeRegistration:
  criSocket: unix:///var/run/crio/crio.sock
---
apiVersion: kubeadm.k8s.io/v1beta3
kind: ClusterConfiguration
${CONTROL_PLANE_ENDPOINT_YAML}networking:
  podSubnet: ${POD_CIDR}
apiServer:
  extraArgs:
    encryption-provider-config: /etc/kubernetes/encryption-config.yaml
  extraVolumes:
    - name: encryption-config
      hostPath: /etc/kubernetes/encryption-config.yaml
      mountPath: /etc/kubernetes/encryption-config.yaml
      readOnly: true
      pathType: File
  certSANs:
${CERT_SANS_YAML}
EOF
chmod 0600 /etc/kubernetes/kubeadm-init.yaml
chown root:root /etc/kubernetes/kubeadm-init.yaml

kubeadm init --config=/etc/kubernetes/kubeadm-init.yaml --cri-socket=unix:///var/run/crio/crio.sock

# Single-node: let workloads schedule on the control-plane
KUBECONFIG=/etc/kubernetes/admin.conf kubectl taint nodes --all \
  node-role.kubernetes.io/control-plane- 2>/dev/null || true

# World-readable admin.conf for the wheel user to use kubectl
install -m 0644 /etc/kubernetes/admin.conf /etc/kubernetes/admin.conf.world
ln -sf /etc/kubernetes/admin.conf.world /etc/profile.d/kubeconfig-symlink-target
chmod 0644 /etc/kubernetes/admin.conf

# Install flannel CNI
KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply -f \
  https://raw.githubusercontent.com/flannel-io/flannel/master/Documentation/kube-flannel.yml

touch "$MARKER"
