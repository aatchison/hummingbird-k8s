# `.devcontainer/`

devcontainer config for the Rust client-side rewrite tracked by epic [#279].
Built per [#280]. Base image: `mcr.microsoft.com/devcontainers/rust:1-bookworm`
plus `musl-tools`, `clang`, `libssl-dev`, `pre-commit`, and a cargo-binstall'd
set of `cargo-{nextest,watch,deny,dist,expand}`.

## Usage

```bash
# Build the image + spin the container (first run takes ~5-10 min on the
# binstall layer; cached on subsequent runs)
devcontainer up --workspace-folder .

# Run cargo commands inside the container
devcontainer exec --workspace-folder . bash -c 'cd rust && cargo nextest run'
devcontainer exec --workspace-folder . bash -c 'cd rust && cargo clippy --workspace --all-targets -- -D warnings'
devcontainer exec --workspace-folder . bash -c 'cd rust && cargo fmt --all -- --check'

# Or open in VS Code: F1 → "Dev Containers: Reopen in Container"
```

`postCreateCommand` installs the project's pre-commit hooks on first attach
so `rustfmt` + `clippy` run on every commit.

## Why a devcontainer

The Rust toolchain — rustup, cargo, the cargo-* tools listed above, plus
musl cross-compile bits — adds up to a non-trivial host-pollution surface.
Per the operator's rule: **install only inside the container; the host
stays clean**.

## CI parity (#281)

CI (#281) consumes the same `Containerfile` so dev + CI produce
byte-identical results. Bumping the base `VARIANT` or any cargo tool here
auto-flows to CI — don't fork the image definitions.

## Bumping the image

- Base image tag: `ARG VARIANT=1-bookworm` (Microsoft's Rust devcontainer,
  tracks Debian 12 + the current stable Rust). Bump when a newer LTS Debian
  or a Rust MSRV bump is needed.
- Cargo tool versions: not pinned by version here — `cargo binstall` resolves
  the latest published binary. Pin via `--version` in the `binstall` call
  when the reproducibility-vs-freshness trade-off needs flipping.
