#!/bin/bash
set -euo pipefail

MARKER=/var/lib/k8s-init.done
[[ -f "$MARKER" ]] && { echo "k8s-init already ran"; exit 0; }

POD_CIDR="${POD_CIDR:-10.244.0.0/16}"

swapoff -a || true
modprobe overlay
modprobe br_netfilter
sysctl --system >/dev/null

# Wait briefly for cri-o socket
for _ in $(seq 1 30); do
  [[ -S /var/run/crio/crio.sock ]] && break
  sleep 1
done

kubeadm init \
  --pod-network-cidr="$POD_CIDR" \
  --cri-socket=unix:///var/run/crio/crio.sock \
  --apiserver-cert-extra-sans=geary,geary.mtcr.lan,127.0.0.1,localhost

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
