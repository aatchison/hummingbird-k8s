#!/usr/bin/env bats
#
# Regression coverage for .github/workflows/release.yml (#290).
#
# The release workflow can't be unit-tested by running it (would need to
# actually push tags + release artifacts), but we can lock in the shape
# invariants that the operator-facing contract requires:
#
# 1. Trigger surface — `v*` tag push AND workflow_dispatch with a
#    `dry_run` input. If a future edit drops workflow_dispatch, the
#    pre-tag-push smoke-test path documented in docs/rust-cli.md
#    silently stops working.
# 2. SHA-pinned third-party actions — matches the repo convention
#    (build-worker.yml, rust-ci.yml). A regression that swaps a pin for
#    a floating tag is a security finding.
# 3. Cosign keyless OIDC steps present — both binary blob and OCI image.
#    Dropping either is a release-pipeline regression.
# 4. Containerfile.hbird referenced (release image artifact).
# 5. `id-token: write` permission — required for cosign keyless OIDC.
#    A regression that drops this would break cosign at runtime, but
#    silently (the error surfaces only when the workflow runs).
#
# These checks live as bats (the project's lingua franca for shell-side
# tests) and shell out to `grep`/`yq` so they keep running even if the
# operator's CI changes lanes (no Rust toolchain dep, no GHA harness).

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  WF="${REPO_ROOT}/.github/workflows/release.yml"
  CF="${REPO_ROOT}/Containerfile.hbird"
}

@test "release.yml exists" {
  [ -f "$WF" ]
}

@test "Containerfile.hbird exists" {
  [ -f "$CF" ]
}

@test "release.yml triggers on v* tag push" {
  # Match the YAML key on its own line + the tags entry on a following
  # line. `grep -A3` so we only match within the `on:` block.
  grep -E "tags:\s*\['v\*'\]" "$WF"
}

@test "release.yml supports workflow_dispatch with dry_run input" {
  grep -q "workflow_dispatch:" "$WF"
  grep -q "dry_run:" "$WF"
}

@test "release.yml has dry_run gating on publish steps" {
  # Every publish step (gh release create, GHCR push, cosign sign image)
  # must be gated on dry_run == 'false'. Count the gates so a future
  # edit that drops one fails loudly. There are 5 publish steps:
  # podman-login, push-to-registry, resolve-digest, cosign-sign-image,
  # gh-release-create.
  gates="$(grep -c "dry_run == 'false'" "$WF" || true)"
  [ "$gates" -ge 5 ]
}

@test "release.yml requires id-token: write for cosign keyless OIDC" {
  grep -E "^\s*id-token:\s*write" "$WF"
}

@test "release.yml installs cosign and signs the binary blob" {
  grep -q "sigstore/cosign-installer@" "$WF"
  grep -q "cosign sign-blob" "$WF"
}

@test "release.yml signs the OCI image with cosign" {
  # The image-sign step uses bare `cosign sign --yes` against IMAGE@DIGEST.
  grep -E "cosign sign --yes" "$WF"
}

@test "release.yml verifies the binary signature before publish" {
  # Local verify-blob before upload is a hard gate — prevents shipping
  # an unverifiable signature.
  grep -q "cosign verify-blob" "$WF"
}

@test "release.yml builds for x86_64-unknown-linux-musl" {
  grep -q "x86_64-unknown-linux-musl" "$WF"
}

@test "release.yml references Containerfile.hbird" {
  grep -q "Containerfile.hbird" "$WF"
}

@test "release.yml SHA-pins every third-party action" {
  # All `uses: <owner>/<repo>@<ref>` lines must use a 40-char SHA, not a
  # vX.Y.Z tag or branch name. The repo convention is documented in
  # rust-ci.yml's header comment. The trailing `# vX.Y.Z` comment is
  # informational only.
  bad="$(grep -E "uses:\s+[^@]+@[^a-f0-9]" "$WF" | grep -vE "uses:\s+[^@]+@[0-9a-f]{40}\b" || true)"
  if [ -n "$bad" ]; then
    echo "Unpinned third-party action(s):"
    echo "$bad"
    return 1
  fi
}

@test "Containerfile.hbird uses FROM scratch (single-binary image)" {
  grep -E "^FROM scratch\b" "$CF"
}

@test "Containerfile.hbird sets ENTRYPOINT to /hbird (exec form)" {
  grep -E '^ENTRYPOINT \["/hbird"\]' "$CF"
}

@test "Containerfile.hbird declares OCI image labels (title, source, version)" {
  grep -q "org.opencontainers.image.title" "$CF"
  grep -q "org.opencontainers.image.source" "$CF"
  grep -q "org.opencontainers.image.version" "$CF"
}
