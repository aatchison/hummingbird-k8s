# Contributing

Thanks for your interest in improving `hummingbird-k8s`. This document captures
the conventions the maintainer follows so contributions land smoothly.

## Getting started

1. Clone the repo: `git clone https://github.com/aatchison/hummingbird-k8s.git`.
2. Install the prerequisites listed in the
   [README Prerequisites section](README.md#prerequisites) (Fedora 44 host,
   libvirt, `gh`, `shellcheck`, etc.).
3. Copy `config.example.sh` to `config.local.sh` and adjust to your environment
   (used by the build-time scripts). For the deploy/update orchestration scripts,
   also copy `cluster.example.conf` to `cluster.local.conf`.
4. Run `make help` to see the cheatsheet of available targets.

## Code style

- **Shell scripts** under `scripts/` and `lib/`:
  - Must start with `set -euo pipefail`.
  - Quote variable expansions (`"$foo"`, not `$foo`).
  - `shellcheck --severity=warning` must exit 0.
  - Indent with **4 spaces** (no tabs).
- **Containerfiles** under `containers/`:
  - `hadolint` with default rules must exit 0.
- **GitHub Actions workflows** under `.github/workflows/`:
  - `actionlint` with default rules must exit 0.
  - Indent with **2 spaces** (YAML).
- **Markdown** under `docs/`, `README.md`, `NOTES.md`:
  - Indent lists and nested blocks with **2 spaces**.
  - Wrap prose at a comfortable column; long links may exceed.

The `pr-validate` workflow runs these linters in CI. Reproduce locally with
`make test-all` (bats unit suites in `tests/lib/` + `tests/scripts/`) plus
the lint commands directly: `shellcheck --severity=warning scripts/*.sh lib/*.sh`,
`hadolint containers/*/Containerfile`, `actionlint .github/workflows/*.yml`.
(`make verify-all` is the **cluster** verifier sequence — runs
`scripts/verify-{encryption,hardening,app-deploy}.sh` against a live deploy;
not a local lint target.)

## PR flow

- Open one PR per issue when feasible. Reference the issue in the PR body with
  `closes #N` (or `refs #N` if it only touches part of the work).
- **Declare file scope** in the PR description. List the directories or files
  the PR touches. Parallel contributors check this to avoid merge conflicts.
- Wait for `pr-validate` to go green before requesting review. Pushing fixups
  is preferred over force-pushing during review.
- Substantive PRs (runtime behaviour, security posture, CI plumbing, image
  contents) get the maintainer's 10-agent review treatment in two rounds. See
  `docs/agent-review.md` for the prompt set.
- CodeRabbit auto-reviews every PR. Treat its findings like a human reviewer:
  resolve, dismiss with rationale, or defer with a tracking issue before merge.
- Commits should be SSH-signed. Configure once with
  `git config --global commit.gpgsign true` and register your signing key on
  GitHub under Settings -> SSH and GPG keys -> New SSH key (Signing key).

## Image bumps

The container images live under `containers/<flavor>/`. After your PR merges:

- If the change touched a `Containerfile` or any in-image asset (anything
  baked into the OCI layer), the maintainer cuts a fresh tag
  `<flavor>/vX.Y.Z`, which fires the GHCR build.
- Pure scripts/docs changes do **not** require a re-tag.

If you are unsure whether a change requires a re-tag, call it out in the PR
description so the maintainer can decide.

## Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/):

```text
<type>(<scope>): <summary>
```

- **Types**: `feat`, `fix`, `docs`, `refactor`, `ci`, `chore`, `test`.
- **Scope** examples: `k8s-init`, `Makefile`, `containers`, `verify`,
  `worker-init`, `workflows`.
- Keep the summary line under ~72 characters; add a body when the change
  needs context.

Example:

```text
fix(k8s-init): use cri-socket flag with kubeadm join on worker nodes
```

## Reporting issues

- For bugs and feature requests: open a GitHub issue. Issue templates are not
  yet in place, so include reproduction steps, observed vs expected
  behaviour, and any relevant logs.
- For security-sensitive reports: do **not** post publicly. Email the
  maintainer or use a private channel; we will coordinate disclosure.

## License

By contributing, you agree that your contributions are licensed under the
Apache License 2.0, the same license as the rest of the project. See
[`LICENSE`](LICENSE) and [`NOTICE`](NOTICE) for the full terms.
