# Agent review

A GitHub Actions workflow that runs a 10-lens parallel review of every pull
request using Claude Code subagents, then posts an aggregated comment on the
PR with severity-tagged findings.

This automates the manual review pass we have been running from a Claude
Code session and is the implementation of issue #19.

## What it does

On every PR `opened` / `synchronize` / `ready_for_review`:

1. **`gate` job** — checks whether the `ANTHROPIC_API_KEY` repo secret is
   configured. If not, the workflow short-circuits cleanly with a "skipped
   (no API key)" notice and the rest of the jobs do not run. This is the
   default state of the scaffold; enabling the workflow is an explicit,
   reversible action.

2. **`review` matrix** — fans out into ten parallel jobs, one per lens.
   Each job loads its prompt from `.github/agent-review-prompts/<lens>.md`
   and invokes Claude with the PR diff and the lens prompt. Each job writes
   its findings to an artifact.

3. **`aggregate` job** — pulls every lens artifact, concatenates them with a
   header per lens, and either posts a new PR comment or updates the
   existing one (it is matched by the marker
   `<!-- agent-review:aggregated -->`).

## The ten lenses

Each lens is a separate prompt file under
`.github/agent-review-prompts/<lens>.md`. Each prompt is ~150-300 words and
specifies the questions to ask and the output format (severity-tagged
findings, capped at 250 words). The full list:

| Lens                | Concern                                                        |
| ------------------- | -------------------------------------------------------------- |
| `security`          | secrets, attack surface, privilege escalation                  |
| `correctness`       | logic, edge cases, scenario matrix                             |
| `shell-hygiene`     | shellcheck-level issues, quoting, `set -e` interactions        |
| `containerfile`     | layer order, cache, hadolint, bootc `/usr` `/etc` `/var` split |
| `workflow-yaml`     | actionlint, action pins, triggers, expression injection        |
| `docs-accuracy`     | claims vs. behavior in `README.md`, `NOTES.md`, `docs/*.md`    |
| `backwards-compat`  | silent breakage for existing operators                         |
| `defaults-ux`       | sensible defaults, escape hatches, helpful errors              |
| `test-plan`         | verifiability of PR claims, fixture / snapshot coverage        |
| `supply-chain`      | pinned versions, external sources, signature verification      |

## How to enable

The workflow is committed but **dormant by default**. To turn it on, set
the `ANTHROPIC_API_KEY` repo secret:

```bash
# Generate an API key at https://console.anthropic.com/
gh secret set ANTHROPIC_API_KEY -R aatchison/hummingbird-k8s
```

Paste the key when prompted. The next PR event will trigger a real review
run. To turn it off again, delete the secret:

```bash
gh secret delete ANTHROPIC_API_KEY -R aatchison/hummingbird-k8s
```

The `gate` job re-evaluates per run, so deletion is immediate — no workflow
edit required.

## Cost considerations

Each PR review fans out into **10 Claude API calls**, one per lens. Each
call sees the PR diff (input tokens) plus the lens prompt and emits at most
~250 words of findings (output tokens). Budget roughly:

- **Small PRs** (≤ 200 changed lines): ~$0.10 per review round
- **Medium PRs** (200-1000 lines): ~$0.20-$0.30 per review round
- **Large PRs** (> 1000 lines): up to $0.50 per review round

Re-pushing to a PR re-runs the review (concurrency cancels the previous
run, so you only pay for the latest). If cost is a concern, narrow the
matrix in `agent-review.yml` to the most valuable lenses for your repo, or
gate on PR labels.

## How findings are surfaced

A single comment per PR, identified by the marker
`<!-- agent-review:aggregated -->`. On re-run, the existing comment is
updated in place rather than appended to, so the PR conversation does not
fill with stale review rounds.

The comment is organised by lens, with each lens emitting one or more
severity-tagged blocks (`critical` → `info`). Reviewers can skim severity
across all ten lenses in one read.

## Extending

To add a new lens:

1. Drop a new prompt file at
   `.github/agent-review-prompts/<your-lens>.md` following the same shape
   as the existing prompts (purpose, questions to ask, output format,
   ≤ 250-word cap).
2. Add `<your-lens>` to the `matrix.lens` list in
   `.github/workflows/agent-review.yml`.

That is it — the aggregate job picks up the new artifact automatically.

To remove a lens, do the reverse.

## Current status (scaffold)

The workflow file structure is the real deliverable today. The step that
invokes Claude is currently a **stub** that emits a placeholder finding so
the matrix → artifact → aggregate → comment pipeline can be tested without
spending API credits. Once
[`anthropics/claude-code-action`](https://github.com/anthropics/claude-code-action)
is wired up (or an equivalent SDK call), replace the stub with the action
invocation marked in the workflow with `=== STUB ===`.

Until that swap happens, enabling the secret is harmless: the gate passes,
the matrix runs, and you get ten placeholder comments — at zero API cost.
