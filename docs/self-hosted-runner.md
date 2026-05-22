# Self-hosted GitHub runner for KVM workflows

## Why

Some workflows in this repo need real KVM:

- the orchestrator integration test (spawns actual VMs via libvirt),
- the bootc upgrade end-to-end test.

GitHub-hosted runners don't expose `/dev/kvm` and nested virt on the public
cloud runners is either disabled or unusably slow. We therefore register the
operator's existing KVM host as a self-hosted runner, gated by the `kvm` and
`libvirt` labels.

## Prerequisites

The runner machine must already be a working KVM host for this project:

- `libvirt` + `qemu-kvm` installed, the operator's user in the `libvirt` group,
  and `virsh -c qemu:///system list` works without sudo.
- `podman` installed (used to build the bootc images).
- The operator's SSH public key is in the host's authorized list (see
  `containers/shared/ssh/`).
- `curl`, `jq`, `tar`, `sudo`, and `gh` on `PATH`.
- `gh auth status` is logged in as a user with `repo` scope on
  `aatchison/hummingbird-k8s` (needed to mint a runner registration token).

## Register the runner

1. On any machine with `gh` logged in, mint a registration token:

   ```bash
   gh api repos/aatchison/hummingbird-k8s/actions/runners/registration-token \
     -X POST --jq .token
   ```

   The token is single-use and expires after ~1 hour.

2. On the KVM host, as the operator's normal (non-root) user, from a checkout
   of this repo:

   ```bash
   bash scripts/setup-runner.sh <registration-token>
   ```

   The script:

   - downloads the latest `actions/runner` linux-x64 tarball into
     `$HOME/actions-runner/`,
   - runs `./config.sh ... --labels kvm,libvirt --unattended --replace`,
   - `sudo ./svc.sh install <user>` to wire up a systemd unit (this is the
     only step that needs root),
   - `sudo ./svc.sh start`,
   - calls `gh api .../actions/runners` to confirm the runner is online.

## Verify

```bash
gh api repos/aatchison/hummingbird-k8s/actions/runners \
  --jq '.runners[] | "\(.name)\t\(.status)\t\(.labels | map(.name) | join(","))"'
```

Expected: your host's short hostname, status `online`, labels including
`kvm,libvirt`.

Then trigger any KVM-needing workflow (for example, re-run the orchestrator
integration job) and confirm it picks up on your runner instead of sitting
queued.

## Security caveat: PRs from forks

Self-hosted runners executing PRs from forks is a known attack surface — a
fork can ship arbitrary code that runs on your machine with whatever access
the runner user has.

Any workflow that targets the `kvm`/`libvirt` labels MUST guard with:

```yaml
if: github.event.pull_request.head.repo.full_name == github.repository
```

so it only runs for branches inside this repository, not from forks. This
repo is single-owner and forks are vanishingly unlikely, but keep the guard
in place — it costs nothing and removes an entire class of risk.

## Removing the runner

On the host:

```bash
cd ~/actions-runner
sudo ./svc.sh stop
sudo ./svc.sh uninstall
./config.sh remove --token "$(gh api \
  repos/aatchison/hummingbird-k8s/actions/runners/remove-token \
  -X POST --jq .token)"
```
