# hummingbird-k8s — top-level Makefile.
#
# This Makefile is the canonical operator entry point: every recipe below
# delegates to a script under scripts/ rather than open-coding podman /
# virt-install / kubectl invocations. The same scripts are also invoked
# directly by GitHub Actions workflows that need to build images or run
# verifiers, so what works locally is what runs in CI.
#
# Targets are grouped:
#   * image-*       — `podman build` only, no qcow2 (cheap to iterate)
#   * k3s/k8s/...   — full operator lifecycle (image -> qcow2 -> VM)
#   * verify-*      — post-deploy verifiers (PSA, audit, encryption, etc.)
#   * clean-*       — tear down local VMs / images
#
# Most VM-touching targets require sudo (libvirt qemu:///system, bib
# loopback mounts). Image-only targets can run as the invoking user as
# long as their podman is configured.
#
# Run `make help` to see the discoverable target list.

SHELL := /usr/bin/env bash

# ---- Worker count knob (#96) -------------------------------------------
# Number of workers spawn / re-spawn targets should create. Override on
# the command line: `sudo make workers COUNT=3`. Threads through:
#   make workers COUNT=N
#     -> bash scripts/redo-workers.sh N
#       -> bash scripts/spawn-workers.sh N
#         -> for i in 1..N: virt-install hummingbird-k8s-worker-${i}
# Same chain applies to `make spawn COUNT=N`, which skips the template
# rebuild and only spawns additional workers.
#
# Per-worker memory / vCPUs are knobbed separately via WORKER_MEMORY /
# WORKER_VCPUS env vars — see config.example.sh.
COUNT ?= 2

# Mirror the LOCAL_IMAGE names build-*.sh use, so image-* targets agree
# with the rest of the toolchain.
IMAGE_K3S    := localhost/hummingbird-k3s:latest
IMAGE_K8S    := localhost/hummingbird-k8s:latest
IMAGE_WORKER := localhost/hummingbird-k8s-worker:latest

# Repo-root-relative Containerfile paths — referenced by both
# scripts/build-*.sh AND the CI workflows under .github/workflows/, so
# they live here as the single source of truth.
CONTAINERFILE_K3S    := containers/k3s/Containerfile
CONTAINERFILE_K8S    := containers/k8s/Containerfile
CONTAINERFILE_WORKER := containers/k8s-worker/Containerfile

.DEFAULT_GOAL := help
.PHONY: help \
        image-k3s image-k8s image-worker image-all \
        image-k3s-with-cloud-init image-k8s-with-cloud-init image-worker-with-cloud-init \
        k3s k8s workers spawn \
        switch-to-ghcr \
        nodes kubectl \
        verify-encryption verify-hardening verify-app-deploy verify-all \
        kube-bench \
        backup-etcd restore-etcd \
        ci-build-k3s ci-build-k8s ci-build-worker \
        print-containerfile-k3s print-containerfile-k8s print-containerfile-worker \
        clean-vms clean-images clean

# ---- help ---------------------------------------------------------------

help: ## Show this target list
	@awk 'BEGIN { FS = ":.*##" } \
	      /^[a-zA-Z0-9_-]+:.*##/ { printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2 }' \
	      $(MAKEFILE_LIST)

# ---- image build (no qcow2) --------------------------------------------
# podman-build only; useful for fast lint/iteration and as a smoke test
# without the bib qcow2 step. CI's `containerfile-build` matrix exercises
# the same path.

image-k3s: ## podman build the k3s OCI image
	podman build -t $(IMAGE_K3S) -f $(CONTAINERFILE_K3S) .

image-k8s: ## podman build the k8s control-plane OCI image
	podman build -t $(IMAGE_K8S) -f $(CONTAINERFILE_K8S) .

image-worker: ## podman build the k8s-worker template OCI image
	# Worker image COPYs worker-join.env via build context. Stub a
	# placeholder when none exists so `make image-worker` works
	# stand-alone (mirrors the pr-validate.yml convention).
	@if [ ! -s worker-join.env ]; then \
	  echo 'kubeadm join 127.0.0.1:6443 --token aaaaaa.bbbbbbbbbbbbbbbb --discovery-token-ca-cert-hash sha256:0000000000000000000000000000000000000000000000000000000000000000' \
	    > worker-join.env; \
	fi
	podman build -t $(IMAGE_WORKER) -f $(CONTAINERFILE_WORKER) .

image-all: image-k3s image-k8s image-worker ## podman build all three OCI images

# ---- opt-in cloud-init variants ----------------------------------------
# Convenience targets that flip ENABLE_CLOUD_INIT=1 for a single podman
# build. The resulting image carries the cloud-init package + NoCloud
# datasource so operators can inject per-VM user-data (SSH keys, runcmd,
# packages) via `virt-install --cloud-init` or a libvirt seed ISO. The
# default image-* targets stay byte-identical to pre-cloud-init builds.
# See docs/cloud-init.md.

image-k3s-with-cloud-init: ## podman build the k3s image with cloud-init opted in
	podman build --build-arg ENABLE_CLOUD_INIT=1 -t $(IMAGE_K3S) -f $(CONTAINERFILE_K3S) .

image-k8s-with-cloud-init: ## podman build the k8s control-plane image with cloud-init opted in
	podman build --build-arg ENABLE_CLOUD_INIT=1 -t $(IMAGE_K8S) -f $(CONTAINERFILE_K8S) .

image-worker-with-cloud-init: ## podman build the worker template image with cloud-init opted in
	@if [ ! -s worker-join.env ]; then \
	  echo 'kubeadm join 127.0.0.1:6443 --token aaaaaa.bbbbbbbbbbbbbbbb --discovery-token-ca-cert-hash sha256:0000000000000000000000000000000000000000000000000000000000000000' \
	    > worker-join.env; \
	fi
	podman build --build-arg ENABLE_CLOUD_INIT=1 -t $(IMAGE_WORKER) -f $(CONTAINERFILE_WORKER) .

# ---- VM lifecycle (sudo) -----------------------------------------------
# These run the full image -> qcow2 -> virt-install dance via the same
# scripts an operator would call manually.

k3s: ## Build + define + start the hummingbird-k3s VM (sudo)
	bash scripts/redo-k3s.sh

k8s: ## Build + define + start the hummingbird-k8s control plane VM (sudo)
	bash scripts/redo-k8s.sh

workers: ## Rebuild worker template and spawn $(COUNT) workers (sudo)
	bash scripts/redo-workers.sh $(COUNT)

spawn: ## Spawn $(COUNT) more workers without rebuilding the template (sudo)
	bash scripts/spawn-workers.sh $(COUNT)

switch-to-ghcr: ## Switch all deployed VMs to track ghcr.io/aatchison/hummingbird-<flavor>:latest (#138)
	bash scripts/switch-to-ghcr.sh

# ---- convenience -------------------------------------------------------

nodes: ## kubectl get nodes via the SSH-tunnel wrapper
	bash scripts/kubectl-k8s.sh get nodes

kubectl: ## kubectl pass-through; use ARGS='get pods -A' to forward args
	bash scripts/kubectl-k8s.sh $(ARGS)

# ---- verification ------------------------------------------------------

verify-encryption: ## Verify etcd encryption-at-rest on the control plane
	bash scripts/verify-encryption.sh

verify-hardening: ## Verify PSA + audit + kubelet protect-kernel-defaults
	bash scripts/verify-hardening.sh

verify-app-deploy: ## End-to-end PSA-restricted nginx + pod-to-pod test
	bash scripts/verify-app-deploy.sh

verify-all: verify-encryption verify-hardening verify-app-deploy ## All three verifiers in sequence

kube-bench: ## Run CIS Kubernetes Benchmark (kube-bench) against the cluster
	bash scripts/run-kube-bench.sh

# ---- backup / restore --------------------------------------------------
# etcd snapshot lifecycle. See docs/backup-restore.md for cadence,
# encryption-key handling, and full DR walkthrough.

backup-etcd: ## Snapshot etcd; optional LABEL=<text> appends to filename
	bash scripts/backup-etcd.sh $(if $(LABEL),--label $(LABEL),)

restore-etcd: ## Restore etcd from a snapshot (SNAP=path.db required)
	@[ -n "$(SNAP)" ] || { echo 'SNAP=<path-to-snapshot.db> required' >&2; exit 2; }
	bash scripts/restore-etcd.sh "$(SNAP)"

# ---- CI integration ----------------------------------------------------
# The redhat-actions/buildah-build action in .github/workflows/build-*.yml
# is what actually publishes signed images to GHCR — these targets are
# the local-equivalent shell path, identical to `image-*` above. They
# exist so CI workflows can keep one of two contracts (action vs. shell)
# without divergence creeping in. See pr-validate.yml's matrix entries
# which call podman build with the same Containerfile path variables.

ci-build-k3s: image-k3s ## Local-equivalent of build-k3s.yml's buildah step

ci-build-k8s: image-k8s ## Local-equivalent of build-k8s.yml's buildah step

ci-build-worker: image-worker ## Local-equivalent of build-worker.yml's buildah step

# Path-emitting helpers — used by CI to discover the canonical
# Containerfile location without grepping the Makefile. Example:
#   make -s print-containerfile-k8s   →   containers/k8s/Containerfile

print-containerfile-k3s: ## Print Containerfile path for the k3s flavor
	@echo $(CONTAINERFILE_K3S)

print-containerfile-k8s: ## Print Containerfile path for the k8s flavor
	@echo $(CONTAINERFILE_K8S)

print-containerfile-worker: ## Print Containerfile path for the k8s-worker flavor
	@echo $(CONTAINERFILE_WORKER)

# ---- cleanup -----------------------------------------------------------

clean-vms: ## Destroy + undefine all hummingbird-* VMs (sudo)
	@for d in $$(virsh -c qemu:///system list --all --name 2>/dev/null \
	             | grep '^hummingbird-'); do \
	  echo "Destroying $$d"; \
	  virsh -c qemu:///system destroy "$$d" 2>/dev/null || true; \
	  virsh -c qemu:///system undefine "$$d" 2>/dev/null || true; \
	done

clean-images: ## Remove the local OCI build outputs
	-podman image rm $(IMAGE_K3S) $(IMAGE_K8S) $(IMAGE_WORKER) 2>/dev/null || true

clean: clean-vms clean-images ## Destroy all VMs and remove local images

