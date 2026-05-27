# verify-* live-validate — cycle 1

Gate for epic [#353] (bash → Rust CLI cutover, v0.1.0) — the verify-*
command class needs at least one live-validated cycle on the geary
cluster before the bash twins (`scripts/verify-encryption.sh`,
`scripts/verify-hardening.sh`, `scripts/verify-app-deploy.sh`) can be
removed. Phase 2 implementation landed in [#287] (PR [#330]); this
artifact documents a fresh end-to-end re-run of bash-twin vs Rust-twin
against the live cluster.

Mirrors the cycle-1 pattern established by [#322] (PR [#325]): one
end-to-end pair of executions per command, exit codes + stdout/stderr
captured, divergence noted.

## Cluster state snapshot

Captured 2026-05-27T16:17-07:00 (UTC-7), worktree HEAD
`c3425a030e4bd07ba8154e330151e000abcee633` (post #355).

```
$ ssh -J geary root@192.168.122.212 \
    "kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes -o wide"
NAME        STATUS   ROLES           AGE   VERSION    INTERNAL-IP       OS-IMAGE                  KERNEL-VERSION          CONTAINER-RUNTIME
hbird-cp1   Ready    control-plane   8h    v1.31.14   192.168.122.212   Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
hbird-w1    Ready    <none>          8h    v1.31.14   192.168.122.116   Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
hbird-w2    Ready    <none>          8h    v1.31.14   192.168.122.171   Hummingbird OS 20251124   7.0.9-105.fc43.x86_64   cri-o://1.32.0
```

3/3 Ready before cycle. 3/3 Ready after cycle (re-checked at end).

## Per-command results

All three verifiers ran twice — once via the bash twin, once via the
Rust `hbird` binary built from this worktree
(`rust/target/release/hbird`, profile = release). Both sides used the
same `cluster.local.conf` (copied from geary), `KVM_HOST=geary`,
`CP_NAME=hbird-cp1`.

| command          | bash exit | Rust exit | bash result | Rust result | parity |
|------------------|-----------|-----------|-------------|-------------|--------|
| verify-encryption | 0         | 0         | OK          | OK          | yes    |
| verify-hardening  | 0         | 0         | 4/4 PASS    | 4/4 PASS    | yes    |
| verify-app-deploy | 0         | 0         | PASS        | PASS        | yes    |
| verify all (Rust only — bash equivalent is `make verify-all`) | n/a | 0 | n/a | all 3 PASS | yes (chain order) |

### 1. verify-encryption

**bash invocation:**
```
CONFIG=cluster.local.conf KVM_HOST=geary CP_NAME=hbird-cp1 \
  bash scripts/verify-encryption.sh
```

**bash output (stderr, exit 0):**
```
[verify-encryption] remote mode: ssh root@192.168.122.212 via geary
[verify-encryption] creating probe secret default/etcd-encryption-probe-8669
[verify-encryption] local etcdctl unavailable, falling back to crictl exec on etcd container
/usr/libexec/verify-encryption.sh: line 97: warning: command substitution: ignored null byte in input
[verify-encryption] OK: secret in etcd is encrypted (prefix=k8s:enc:aesgcm:)
```

**Rust invocation:**
```
hbird verify encryption --config cluster.local.conf --kvm-host geary --cp-name hbird-cp1
```

**Rust output (stderr, exit 0):**
```
[verify-encryption] remote mode: ssh root@192.168.122.212 via geary
[verify-encryption] creating probe secret default/etcd-encryption-probe-8724
[verify-encryption] local etcdctl unavailable, falling back to crictl exec on etcd container
/usr/libexec/verify-encryption.sh: line 97: warning: command substitution: ignored null byte in input
[verify-encryption] OK: secret in etcd is encrypted (prefix=k8s:enc:aesgcm:)
 INFO hbird{subcommand="verify"}: close
```

**Diff:** bash and Rust paths identical modulo:
- probe-secret suffix (`-8669` vs `-8724`) — `$$` / `std::process::id()`, different per run.
- Rust trailing ` INFO hbird{subcommand="verify"}: close` — tracing-subscriber span close (from #326); operationally cosmetic.

### 2. verify-hardening

**bash invocation:**
```
CONFIG=cluster.local.conf KVM_HOST=geary CP_NAME=hbird-cp1 \
  bash scripts/verify-hardening.sh
```

**bash output (stderr → diagnostics; stdout → summary; exit 0):**
```
[verify-hardening] CP_IP not set, trying resolve_cp_ip hbird-cp1
[verify-hardening] CP_IP=192.168.122.212
[verify-hardening] check 1/3: PodSecurity restricted rejects a privileged pod
[verify-hardening]   PASS: privileged pod rejected by PodSecurity
[verify-hardening] check 2/3: apiserver audit log is non-empty on the CP host
[verify-hardening]   PASS: audit log present (/var/log/kubernetes/k8s-audit.log)
[verify-hardening] check 3/4: kubelet running with --protect-kernel-defaults=true
[verify-hardening]   PASS: kubelet has --protect-kernel-defaults=true
[verify-hardening] check 4/4: kubelet running with --rotate-certificates=true (#121)
[verify-hardening]   PASS: kubelet has --rotate-certificates=true

[verify-hardening] summary
  PodSecurity restricted    : PASS
  apiserver audit log       : PASS
  kubelet protect-kernel    : PASS
  kubelet rotate-certs      : PASS
[verify-hardening] all checks PASSED
```

**Rust invocation:**
```
hbird verify hardening --config cluster.local.conf --kvm-host geary --cp-name hbird-cp1
```

**Rust output (stderr → diagnostics; stdout → summary; exit 0):**
```
[verify-hardening] CP_IP=192.168.122.212
[verify-hardening] check 1/3: PodSecurity restricted rejects a privileged pod
[verify-hardening]   PASS: privileged pod rejected by PodSecurity
[verify-hardening] check 2/3: apiserver audit log is non-empty on the CP host
[verify-hardening]   PASS: audit log present (/var/log/kubernetes/k8s-audit.log)
[verify-hardening] check 3/4: kubelet running with --protect-kernel-defaults=true
[verify-hardening]   PASS: kubelet has --protect-kernel-defaults=true
[verify-hardening] check 4/4: kubelet running with --rotate-certificates=true (#121)
[verify-hardening]   PASS: kubelet has --rotate-certificates=true

[verify-hardening] summary
  PodSecurity restricted    : PASS
  apiserver audit log       : PASS
  kubelet protect-kernel    : PASS
  kubelet rotate-certs      : PASS
[verify-hardening] all checks PASSED
 INFO hbird{subcommand="verify"}: close
```

**Diff:** bash and Rust identical modulo:
- bash emits one extra preamble line (`CP_IP not set, trying resolve_cp_ip hbird-cp1`); Rust resolves CP_IP via `hbird-config` + libvirt before emitting the resolved value, so the "trying" line is folded out. Both ultimately report `CP_IP=192.168.122.212`.
- Rust trailing ` INFO hbird{subcommand="verify"}: close` (cosmetic, as above).
- All four checks PASS on both sides; summary table byte-identical.

### 3. verify-app-deploy

**bash invocation** (uses port-forward tunnel via `scripts/kubectl-k8s.sh`; needed
to use a non-default port because :16443 had a stale listener from prior session,
matching the pre-existing operator state noted in the Phase 2 fixture
`cycle_verify_app_deploy.txt`):
```
KCFG=/tmp/k8s-kubeconfig-cycle1 LOCAL_PORT=26443 \
  CONFIG=cluster.local.conf KVM_HOST=geary CP_NAME=hbird-cp1 \
  bash scripts/verify-app-deploy.sh
```

**bash output (stderr, exit 0):**
```
[verify-app-deploy] creating namespace smoketest-1779923806
[verify-app-deploy] applying nginx Deployment + Service in smoketest-1779923806
[verify-app-deploy] waiting up to 2m for deployment/nginx to become Available
deployment.apps/nginx condition met
[verify-app-deploy] probing http://nginx:8080 from an in-cluster busybox pod
[verify-app-deploy] PASS: nginx returned the welcome page over ClusterIP
[verify-app-deploy] verify-app-deploy: PASS
[verify-app-deploy] cleanup: deleting namespace smoketest-1779923806
```

**Rust invocation** (no local tunnel needed — `hbird` runs kubectl on the CP
directly via SSH):
```
hbird verify app-deploy --config cluster.local.conf --kvm-host geary --cp-name hbird-cp1
```

**Rust output (stderr, exit 0):**
```
[verify-app-deploy] creating namespace smoketest-1779923815
[verify-app-deploy] applying nginx Deployment + Service in smoketest-1779923815
[verify-app-deploy] waiting up to 2m for deployment/nginx to become Available
[verify-app-deploy] probing http://nginx:8080 from an in-cluster busybox pod
[verify-app-deploy] PASS: nginx returned the welcome page over ClusterIP
[verify-app-deploy] verify-app-deploy: PASS
[verify-app-deploy] cleanup: deleting namespace smoketest-1779923815
 INFO hbird{subcommand="verify"}: close
```

**Diff:** bash and Rust identical modulo:
- namespace epoch suffix (`smoketest-1779923806` vs `smoketest-1779923815`) —
  `$(date +%s)` / `SystemTime::now()`, different per run.
- bash emits one extra `deployment.apps/nginx condition met` line from
  `kubectl wait` direct-to-stderr; Rust path runs the same wait but
  doesn't echo the intermediate kubectl-wait line (the wait status is
  captured and used internally). Functionally equivalent.
- Rust trailing ` INFO hbird{subcommand="verify"}: close` (cosmetic).

### 4. verify all (Rust chain — confirms ordering semantics)

```
hbird verify all --config cluster.local.conf --kvm-host geary --cp-name hbird-cp1
```

Exit 0. Ran encryption → hardening → app-deploy in that order (matches
`make verify-all` chain), all three PASS, cluster ended 3/3 Ready,
smoketest namespace cleaned up.

## Divergence summary

None that are operationally significant. All divergence is one of:

1. **Per-run identifiers** (PIDs, epoch seconds) — expected, untrackable.
2. **Tracing close line** — Rust adds ` INFO hbird{subcommand="verify"}: close`
   from the `tracing-subscriber` wiring landed in [#326]. Bash has no
   equivalent. Cosmetic; no operator-grep impact.
3. **Wait-helper line emission** — `verify-app-deploy.sh` bash twin
   echoes the `kubectl wait` intermediate "condition met" line; the
   Rust path swallows it after a successful return. Both produce
   identical end-to-end PASS/FAIL semantics.
4. **CP-IP-resolution preamble** — `verify-hardening.sh` bash twin
   emits a `"CP_IP not set, trying resolve_cp_ip <name>"` line before
   the resolved IP. Rust folds that into a single CP_IP line. Cosmetic.

All bash-twin operator-grepped strings (`PASS:`, `FAIL:`, `OK:`,
`[verify-*]` prefix, `all checks PASSED`) preserved verbatim in Rust
output — confirms the wire-string contract from #287.

## Cluster post-state

3/3 Ready, no residual workloads, no DaemonSet disturbance. Re-checked
immediately after the `hbird verify all` chain completed; no orphan
smoketest namespaces.

## Gate status

verify-* command class: **cycle-1 live-validate COMPLETE**.
Per epic [#353] checkbox, the verify-* gate can now be marked done.
Bash removal of `scripts/verify-*.sh` (encryption + hardening +
app-deploy) is unblocked pending the remaining gates (Phase 3 + Phase 4
live-validates).

## References

- [#287] — Phase 2 implementation (merged PR [#330])
- [#322] / [#325] — cycle-1 pattern template (update-cluster)
- [#326] — tracing-subscriber wiring (source of the ` INFO ... close` line)
- [#353] — bash-removal epic (this artifact lifts the verify-* gate)

[#287]: https://github.com/aatchison/hummingbird-k8s/issues/287
[#322]: https://github.com/aatchison/hummingbird-k8s/issues/322
[#325]: https://github.com/aatchison/hummingbird-k8s/pull/325
[#326]: https://github.com/aatchison/hummingbird-k8s/issues/326
[#330]: https://github.com/aatchison/hummingbird-k8s/pull/330
[#353]: https://github.com/aatchison/hummingbird-k8s/issues/353
