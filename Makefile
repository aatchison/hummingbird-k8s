# Hummingbird-k8s Makefile
#
# Stable entrypoint over the various driver scripts (build*.sh, define-vm*.sh,
# redo*.sh, spawn-workers.sh, kubectl-k8s.sh, scripts/verify-*.sh). The scripts
# remain the source of truth — this Makefile is sugar over them.
#
# Usage:
#   make help                       # show the cheatsheet
#   sudo make k8s                   # build + define + start the CP VM
#   sudo make workers COUNT=3       # destroy any existing workers, spawn 3 fresh
#   make kubectl-k8s ARGS='get pods -A'
#
# Honored env vars: COUNT, FLAVOR, KVM_HOST, POOL_DIR, and anything else the
# underlying scripts read from config.local.sh.

SHELL := /usr/bin/env bash

# Tunables surfaced on the make command line.
COUNT  ?= 2
FLAVOR ?=
ARGS   ?=

# libvirt pool dir; matches the default in the scripts.
POOL_DIR ?= /var/lib/libvirt/images

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk -F':.*?## ' '{ printf "  %-20s %s\n", $$1, $$2 }'

# ---- Build + start ---------------------------------------------------------

.PHONY: k3s
k3s: ## Build + define + start the k3s single-node VM (sudo)
	sudo bash redo.sh

.PHONY: k8s
k8s: ## Build + define + start the upstream k8s control-plane VM (sudo)
	sudo bash redo-k8s.sh

.PHONY: workers
workers: ## Destroy + rebuild + spawn COUNT workers (default 2) (sudo)
	sudo bash redo-workers.sh $(COUNT)

.PHONY: spawn
spawn: ## Spawn COUNT additional workers without destroying existing (sudo)
	sudo bash spawn-workers.sh $(COUNT)

# ---- kubectl from the client ----------------------------------------------

.PHONY: kubectl-k8s
kubectl-k8s: ## Run kubectl via the SSH tunnel: make kubectl-k8s ARGS='get pods -A'
	./kubectl-k8s.sh $(ARGS)

.PHONY: nodes
nodes: ## Shortcut for `kubectl get nodes -o wide`
	$(MAKE) kubectl-k8s ARGS='get nodes -o wide'

# ---- Verifiers -------------------------------------------------------------

.PHONY: verify-encryption
verify-encryption: ## Verify etcd at-rest encryption is active
	./scripts/verify-encryption.sh

.PHONY: verify-hardening
verify-hardening: ## Verify PSA / audit / kubelet hardening
	./scripts/verify-hardening.sh

.PHONY: verify-app-deploy
verify-app-deploy: ## End-to-end PSA-restricted nginx smoke test
	./scripts/verify-app-deploy.sh

.PHONY: verify-all
verify-all: verify-encryption verify-hardening verify-app-deploy ## Run all three verifiers in order

.PHONY: kube-bench
kube-bench: ## Run CIS kube-bench against the cluster
	./scripts/run-kube-bench.sh

# ---- Cleanup ---------------------------------------------------------------

.PHONY: clean-vms
clean-vms: ## Destroy + undefine all hummingbird VMs and remove their qcow2s (sudo)
	@set -euo pipefail; \
	for d in $$(sudo virsh -c qemu:///system list --all --name 2>/dev/null \
	            | grep -E '^(hummingbird-k3s|hummingbird-k8s|hummingbird-k8s-worker|hummingbird)' || true); do \
	  echo "destroying $$d"; \
	  sudo virsh -c qemu:///system destroy "$$d" 2>/dev/null || true; \
	  sudo virsh -c qemu:///system undefine "$$d" 2>/dev/null || true; \
	done; \
	sudo rm -f $(POOL_DIR)/hummingbird-k3s.qcow2 \
	           $(POOL_DIR)/hummingbird-k8s.qcow2 \
	           $(POOL_DIR)/hummingbird-k8s-worker.qcow2 \
	           $(POOL_DIR)/hummingbird-k8s-worker-*.qcow2 \
	           $(POOL_DIR)/hummingbird.qcow2

.PHONY: clean-images
clean-images: ## Remove local podman images named localhost/hummingbird-*
	@set -euo pipefail; \
	imgs=$$(sudo podman images --format '{{.Repository}}:{{.Tag}}' \
	        | grep -E '^localhost/hummingbird-' || true); \
	if [[ -n "$$imgs" ]]; then \
	  echo "$$imgs" | xargs -r sudo podman rmi -f; \
	else \
	  echo "no localhost/hummingbird-* images to remove"; \
	fi

.PHONY: clean
clean: clean-vms clean-images ## clean-vms + clean-images
