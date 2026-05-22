# Multi-arch image support

As of k8s/v0.1.34, k3s/v0.1.13, and worker/v0.1.10, all three Hummingbird
flavors publish a multi-arch OCI manifest index covering both
`linux/amd64` and `linux/arm64`. Closes #107, #111.

## What's published

Each tag (`vX.Y.Z` and `latest`) is a manifest index referencing one child
manifest per architecture:

```text
ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z          ← manifest index (sha256:…)
├── linux/amd64                                    ← child manifest (sha256:…)
└── linux/arm64                                    ← child manifest (sha256:…)
```

Both the index and each per-arch child manifest are cosign-signed via
`cosign sign --recursive`, so `cosign verify` works against the tag,
the index digest, or any child digest.

## Consuming the multi-arch image

`bootc switch` resolves the right child manifest automatically based on
the host's architecture — no flag needed:

```bash
# On an amd64 KVM host: pulls the linux/amd64 child
# On an arm64 KVM host (e.g. Raspberry Pi, Ampere): pulls linux/arm64
sudo bootc switch ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z
sudo systemctl reboot
```

The same applies to `make k3s` / `make k8s` builds on the operator's host:
`podman build` selects the host arch by default, and `bootc-image-builder`
honors the same selection when turning the OCI image into a qcow2.

## Verifying multi-arch locally

Inspect the manifest index to confirm both architectures are present:

```bash
podman manifest inspect ghcr.io/aatchison/hummingbird-k8s:latest \
  | jq '.manifests[] | {arch: .platform.architecture, digest}'
```

Expected output:

```json
{ "arch": "amd64", "digest": "sha256:…" }
{ "arch": "arm64", "digest": "sha256:…" }
```

cosign verification (works against the index OR any child digest):

```bash
cosign verify \
  --certificate-identity-regexp 'https://github.com/aatchison/hummingbird-k8s/' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z
```

## How the build works

The `.github/workflows/build-*.yml` workflows pass
`platforms: linux/amd64,linux/arm64` to `redhat-actions/buildah-build`,
which emits a single manifest index that's pushed to GHCR in one step.

ARM64 RUN steps execute on the GH-hosted amd64 runner via qemu-user-static
(installed by `docker/setup-qemu-action`), so cross-builds are slower than
native — typically 2–3× the amd64-only wall clock. If a tagged build
exceeds the ubuntu-latest timeout, the next step is to split the workflow
into a per-arch matrix on native runners and merge manifests at the end.

The k8s flavor's Containerfile (`containers/k8s/Containerfile`) uses
`ARG TARGETARCH` to fetch the architecture-matching `cilium-cli` tarball
(`cilium-linux-${TARGETARCH}.tar.gz`). `TARGETARCH` is set automatically
by buildah/podman when `--platform` is passed (linux/amd64 → `amd64`,
linux/arm64 → `arm64`). The k3s and worker Containerfiles have no
arch-specific download steps; their RPM installs are arch-aware via dnf.

## Boot-test coverage

`.github/workflows/integration-boot*.yml` exercises a real qcow2 boot via
the geary-docker self-hosted runner. That runner is amd64-only today —
**arm64 child manifests are built and signed on every release but not
boot-tested in CI**. ARM64 boot verification requires registering a
self-hosted ARM64 runner; tracked in a follow-up issue.

If you operate the arm64 flavor in production, smoke-test the qcow2 on
your own arm64 KVM host (a recent Raspberry Pi 5 or Ampere Altra works)
before promoting `:latest` for arm64 consumers.
