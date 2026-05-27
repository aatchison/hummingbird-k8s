# Rust workspace — `hummingbird-k8s` client-side rewrite

Tracked by epic [#279](https://github.com/aatchison/hummingbird-k8s/issues/279).
Foundation laid in [#280](https://github.com/aatchison/hummingbird-k8s/issues/280)
(this PR).

The bash equivalents under [`../scripts/`](../scripts/) remain canonical
until each Rust subcommand reaches behavioral parity with its bash twin
(error formatting, log shape, exit codes — see the operator-mental-model
contract in the epic). The `Makefile` will dispatch to either the Rust
binary or the bash equivalent on a per-target basis as Rust subcommands
land.

## Build + test

All work happens inside the devcontainer (see [`../.devcontainer/`](../.devcontainer/)).
The host stays clean of Rust toolchain pollution.

```bash
# From the repo root:
devcontainer up --workspace-folder .

# Then any of:
devcontainer exec --workspace-folder . bash -c 'cd rust && cargo nextest run'
devcontainer exec --workspace-folder . bash -c 'cd rust && cargo clippy --workspace --all-targets -- -D warnings'
devcontainer exec --workspace-folder . bash -c 'cd rust && cargo fmt --all -- --check'
```

VS Code users: `F1 → "Dev Containers: Reopen in Container"`.

## Layout

```
rust/
├── Cargo.toml             # workspace root
├── rust-toolchain.toml    # pinned to stable + rustfmt + clippy
└── crates/
    ├── hbird-common/      # placeholder (#280); future crates land per phasing
    └── deny.toml           # cargo-deny license/security policy
```

## Phasing (per [epic #279])

| Phase | Tracked by | Status |
|-------|-----------|--------|
| Foundation — devcontainer + workspace | [#280](https://github.com/aatchison/hummingbird-k8s/issues/280) | this PR |
| Foundation — CI workflow | [#281](https://github.com/aatchison/hummingbird-k8s/issues/281) | pending |
| Foundation — ClusterConfig parser | [#282](https://github.com/aatchison/hummingbird-k8s/issues/282) | pending |
| Transport — clap command tree | [#283](https://github.com/aatchison/hummingbird-k8s/issues/283) | pending |
| Transport — virt + qemu+ssh URI | [#284](https://github.com/aatchison/hummingbird-k8s/issues/284) | pending |
| Transport — openssh | [#285](https://github.com/aatchison/hummingbird-k8s/issues/285) | pending |
| Phase 1 — `update-cluster` | [#286](https://github.com/aatchison/hummingbird-k8s/issues/286) | pending |
| Phase 2 — `verify-*` | [#287](https://github.com/aatchison/hummingbird-k8s/issues/287) | pending |
| Phase 3 — `export-argocd` / `get-kubeconfig` | [#288](https://github.com/aatchison/hummingbird-k8s/issues/288) | pending |
| Phase 4 — `deploy-/destroy-/spawn-` | [#289](https://github.com/aatchison/hummingbird-k8s/issues/289) | pending (gated on #311(d)) |
| Release — cosign + cargo-dist | [#290](https://github.com/aatchison/hummingbird-k8s/issues/290) | pending |
| Migration guide | [#291](https://github.com/aatchison/hummingbird-k8s/issues/291) | pending |

## Lint policy

Workspace defaults (`Cargo.toml`):

- `unsafe_code = "forbid"` — no `unsafe { … }` blocks. Re-enable per-crate
  only with a documented justification.
- `clippy::all = "deny"` — standard clippy lints are errors, not warnings.
- `missing_docs = "warn"` — public items should be documented; warning so
  bootstrap work doesn't bog down on doc churn.

Pre-commit (`../.pre-commit-config.yaml`) enforces `cargo fmt --check` +
`cargo clippy -- -D warnings` on every commit that touches `rust/**`.
