# Verify weekly (orchestrator)

`.github/workflows/verify-weekly.yml` runs the canonical post-deploy
verifiers against the live geary cluster on a weekly cron. Closes #15.

## What it does

On Monday at 07:00 UTC (and on `workflow_dispatch`), the self-hosted
runner on geary:

1. Looks up the `hummingbird-k8s` control-plane VM's IP via
   `virsh -c qemu:///system domifaddr`.
2. Pulls `/etc/kubernetes/admin.conf` from the CP over SSH and rewrites
   the server URL to the CP IP so `kubectl` works from the runner.
3. Runs three verifiers in sequence (each with `continue-on-error`, so a
   single failure doesn't mask the others):
   - `scripts/verify-encryption.sh` — streamed to the CP and executed
     there (needs `etcdctl`/`crictl` on the etcd container's host).
   - `scripts/verify-hardening.sh` — executed on the runner with
     `CP_IP` and `KUBECONFIG` set; SSHes into the CP for the
     audit-log/kubelet-flag probes.
   - `scripts/verify-app-deploy.sh` — executed on the runner with
     `KUBECONFIG` set; deploys an ephemeral PSA-restricted nginx +
     probe pod, asserts pod-to-pod via the ClusterIP, cleans up.
4. If any verifier failed, posts a comment to issue #15 with the list
   of failing checks and the run URL, then exits non-zero so the run
   is visibly red in the Actions UI.

## How to add a new check

1. Drop a `verify-*.sh` (or any test script) under `scripts/`.
2. Add a step to `.github/workflows/verify-weekly.yml`, modeled on the
   existing verifier steps:
   - Give it a unique `id:`.
   - Set `continue-on-error: true` so it doesn't short-circuit later
     verifiers.
   - Provide `KUBECONFIG` and/or `CP_IP` as the script expects.
3. Add the new step's `outcome` to the `Summarize results` step's
   `failed=()` accounting.

## Triggering manually

```bash
gh workflow run verify-weekly.yml --repo aatchison/hummingbird-k8s
gh run list --workflow=verify-weekly.yml --repo aatchison/hummingbird-k8s --limit 1
```

## Constraints

- Runs only when the geary self-hosted runner is online (label set
  `self-hosted, kvm, libvirt`). If the runner is offline at the
  scheduled time, the run will queue until it comes back.
- The orchestrator does **not** introspect issue bodies to discover
  per-issue test scripts. The set of verifiers is hard-coded in the
  workflow. Extending the set is a workflow edit, not config.
- Disabling per host is not supported — this is a repo-level workflow.
  To pause the weekly cron, comment out the `schedule:` block on `main`.
