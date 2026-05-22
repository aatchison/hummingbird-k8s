FROM quay.io/hummingbird-community/bootc-os:latest

ARG K3S_VERSION=v1.31.4+k3s1

# Drop in the sshd config early so changes here don't bust the (much bigger)
# package-install layer cache below.
COPY etc/ssh/sshd_config.d/99-no-passwords.conf /etc/ssh/sshd_config.d/99-no-passwords.conf

# Install k3s into /usr/bin (bootc keeps /usr in the OCI layer; /usr/local
# is wired to /var on Hummingbird and would not survive into the image).
RUN curl -fsSL https://get.k3s.io | \
      INSTALL_K3S_VERSION="${K3S_VERSION}" \
      INSTALL_K3S_BIN_DIR=/usr/bin \
      INSTALL_K3S_SKIP_ENABLE=true \
      INSTALL_K3S_SKIP_START=true \
      INSTALL_K3S_SELINUX_WARN=true \
      sh - \
 && /usr/bin/k3s --version

# Default config: world-readable kubeconfig so the wheel user can use kubectl.
RUN install -d /etc/rancher/k3s \
 && printf 'write-kubeconfig-mode: "0644"\n' > /etc/rancher/k3s/config.yaml

RUN install -d /etc/systemd/system/multi-user.target.wants \
 && ln -sf /etc/systemd/system/k3s.service \
       /etc/systemd/system/multi-user.target.wants/k3s.service

# Enable bootc auto-update timer (daily, applies + reboots automatically).
# Operators who want manual control should override via systemctl disable.
RUN install -d /etc/systemd/system/timers.target.wants \
 && ln -sf /usr/lib/systemd/system/bootc-fetch-apply-updates.timer \
       /etc/systemd/system/timers.target.wants/bootc-fetch-apply-updates.timer

LABEL containers.bootc=1
LABEL org.opencontainers.image.source=https://github.com/aatchison/hummingbird-k8s
LABEL org.opencontainers.image.description="Fedora Hummingbird bootc image with k3s"
