# Rust workspace — `hummingbird-k8s` client-side rewrite

Tracked by epic [#279](https://github.com/aatchison/hummingbird-k8s/issues/279).
Foundation laid in [#280](https://github.com/aatchison/hummingbird-k8s/issues/280)
(PR #313). CI wired in [#281](https://github.com/aatchison/hummingbird-k8s/issues/281)
via `.github/workflows/rust-ci.yml`.

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
├── deny.toml              # cargo-deny license/security policy
└── crates/
    ├── hbird-common/      # placeholder (#280); future crates land per phasing
    ├── hbird-config/      # cluster.local.conf parser (#282)
    ├── hbird-ssh/         # SSH transport — `ssh_opts_array` Rust twin (#285)
    ├── hbird-virt/        # libvirt CLI wrapper + qemu+ssh URI (#284)
    └── hbird-cli/         # `hbird` binary — clap command tree (#283)
```

## Phasing (per [epic #279])

| Phase | Tracked by | Status |
|-------|-----------|--------|
| Foundation — devcontainer + workspace | [#280](https://github.com/aatchison/hummingbird-k8s/issues/280) | landed (PR #313) |
| Foundation — CI workflow | [#281](https://github.com/aatchison/hummingbird-k8s/issues/281) | landed (PR #314) |
| Foundation — ClusterConfig parser | [#282](https://github.com/aatchison/hummingbird-k8s/issues/282) | landed (PR #315) |
| Foundation — clap command tree (`hbird` binary) | [#283](https://github.com/aatchison/hummingbird-k8s/issues/283) | landed (PR #319) |
| Foundation — virt + qemu+ssh URI | [#284](https://github.com/aatchison/hummingbird-k8s/issues/284) | landed (PR #318) |
| Foundation — openssh | [#285](https://github.com/aatchison/hummingbird-k8s/issues/285) | landed (PR #317) |
| Phase 1A — `update-cluster` (dry-run parity + orchestration scaffold) | [#286](https://github.com/aatchison/hummingbird-k8s/issues/286) | landed (PR #321) |
| Phase 1B — `update-cluster` (live-execution slice; cycle 1 = `cp_kubectl` + drain/uncordon) | [#322](https://github.com/aatchison/hummingbird-k8s/issues/322) | cycle 1 landed (PR #325); cycles 2–4 pending |
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

CI (`../.github/workflows/rust-ci.yml`) re-runs fmt + clippy + build +
nextest + `cargo deny` on every PR that touches `rust/**`, plus a
`devcontainer-smoke` job that builds the actual `.devcontainer/Containerfile`
and runs `cargo check` inside it (dev/CI parity gate). A
`lint-inheritance` job enforces that every `rust/crates/*/Cargo.toml`
declares `[lints]\nworkspace = true` so the workspace's
`unsafe_code = "forbid"` / clippy policy cannot be silently bypassed by a
future crate that forgets the stanza.
