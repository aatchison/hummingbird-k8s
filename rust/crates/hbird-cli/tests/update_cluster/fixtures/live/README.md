# Live-validate fixtures for `hbird update-cluster`

This directory holds **destructive live-validate diffs** historically
captured (pre-v0.1.0, when both runtimes coexisted) by running
`scripts/update-cluster.sh` (the bash twin, removed in #353) against a
real hummingbird-k8s cluster, rolling back via `bootc rollback`, then
running `hbird update-cluster` (the Rust twin) with identical args and
diffing the post-state.

Post-#353 the bash twin is gone; any future re-captures pin Rust-vs-
Rust behavior (run-to-run + across versions) rather than Rust-vs-bash
parity. See "Capture procedure" below — step 2 / step 4 / step 5 are
now historical (bash invocation + bootc rollback dance + Rust re-run)
and a fresh capture procedure for the Rust-only world should be
defined as part of #322's live-execution slice landing.

## Scope of the live-validate corpus

The dry-run fixtures one directory up (`fixtures/dry_run_*.txt`) pin
byte-for-byte log parity for the **non-destructive** code path. They run
in CI on every push.

These live fixtures pin **behavioral parity** for the destructive code
path: drain, uncordon, bootID transition, DaemonSet readiness gate, etcd
snapshot, apiserver back-up wait. They don't run in CI — they're
captured by hand against geary's `hbird-cp1` + `hbird-w1`/`hbird-w2`
cluster and committed as evidence the live execution path matches.

## Status (PR #321 round-2)

The Rust live-execution helpers in `update_cluster.rs` are **stubs** that
surface a `live-mode update-cluster: \`<helper>\` requires a remote
SSH/kubectl round-trip that is not yet implemented in the Rust path…`
diagnostic when invoked outside dry-run. Implementing them is tracked
by [#322]. Capturing this directory's contents is a sub-task of #322
that requires an operator-driven tmux session (the destructive cycles
must be observable so a regression can be caught before the cluster
becomes unrecoverable).

The 4 blocks targeted for the initial capture (per #321 round-2
operator override):

1. **drain_uncordon.txt** — `--node=hbird-w2 --skip-gates`: bash drain
   + immediate uncordon vs Rust, on a known-Ready worker.
2. **bootid_gate.txt** — `--node=hbird-w1`: full upgrade cycle with the
   bootID change as the gate, with `bootc rollback` between bash and
   Rust runs.
3. **daemonset_ready.txt** — `--node=hbird-w1` post-reboot: the
   kube-system DaemonSet readiness wait (Cilium / kube-proxy /
   coredns).
4. **etcd_backup.txt** — pre-CP-upgrade etcd snapshot via
   `scripts/backup-etcd.sh` vs the Rust equivalent (if implemented in
   the live slice; otherwise substitute with `wait_apiserver_back` post-CP-reboot).

## Capture procedure (when #322 lands the live execution slice)

For each block:

1. `kubectl get nodes -o wide > pre.txt; kubectl get pods -A -o wide >> pre.txt; virsh -c qemu+ssh://geary/system dominfo <target> >> pre.txt`
2. `make update-cluster CONFIG=cluster.local.conf KVM_HOST=geary FLAGS='<block flags>'`
3. Capture post-state → `bash_post.txt`
4. `ssh <target> sudo bootc rollback && wait for Ready`
5. `HBIRD_REMOTE_NO_SUDO=1 hbird update-cluster --config ~/cluster.local.conf --kvm-host geary <block flags>`
6. Capture post-state → `rust_post.txt`
7. `diff bash_post.txt rust_post.txt` — expected: empty modulo timestamps.
8. Save the unified diff (or "empty diff modulo timestamps") to
   `<block>.txt` in this directory.

If `diff` reveals genuine behavioral discrepancy, **STOP** and surface
back to operator before deciding whether bash or Rust is "more correct".

[#322]: https://github.com/aatchison/hummingbird-k8s/issues/322
