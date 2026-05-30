# bluefin live boot-test — cycle 4

Final live acceptance criterion for epic **#378** (boot-test mechanism
reform). Epic #378 closed the class of *silent-override* bugs that made the
boot-test gate structurally false-positive — five code fixes (#373, #374,
#375, #376, #377), plus the #377 follow-up regression fix (the CLI-env
snapshot ran *after* `source lib/build-common.sh`, so a sourced-lib default
was captured as a phantom CLI override and clobbered the config value). The
remaining acceptance criterion was a **live** boot-test proving the gate now
(a) actually exercises a local source change in the booted image and
(b) hard-fails on cache drift instead of false-greening.

Run on the **bluefin** test node (single-host KVM, libvirt `qemu:///system`,
passwordless sudo, isolated libvirt NAT). CP-only deploy, `IMAGE_SOURCE=local`
golden path (no override flags), `ENABLE_CLOUD_INIT=1` from config.

## Worktree / image under test

- Repo HEAD: `0281715` (`fix(deploy-cluster): move CLI-env snapshot before lib
  sources (#2)` merged — the #377 regression fix).
- Build: rootful `podman` + `bootc-image-builder` on bluefin
  (`bib hard-requires rootful podman`; the first local qcow2 build must run as
  root — see issue #4 for a proposed pre-flight hint).
- Image: `localhost/hummingbird-k8s:latest` built locally from
  `containers/k8s/Containerfile`, baked into a qcow2 template by bib, booted as
  the control-plane VM.

## AC1 — a local source change is genuinely exercised in the booted image

To prove the booted node came from *this* local build (the #367-class
false-green check), a unique sentinel was injected into the k8s Containerfile
for the run:

```
RUN echo "<run-marker>" > /usr/lib/hbird-cycle4-marker
```

`/usr/lib` is part of the ostree read-only tree, so its contents reflect the
booted image exactly. (A Containerfile `LABEL` was tried first and does **not**
work as a probe on a bootc node — the image lives in ostree, not podman image
storage, and `bootc status` does not surface arbitrary labels.)

After the golden-path deploy, the control-plane came up Ready:

```
$ ssh root@<cp-ip> kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes
NAME          STATUS   ROLES           AGE   VERSION
hbird-c4-cp   Ready    control-plane   24s   v1.31.14
```

and the sentinel was present on the running node:

```
$ ssh root@<cp-ip> cat /usr/lib/hbird-cycle4-marker
<run-marker>          # == the value injected into the Containerfile for this run
```

**AC1 = PASS** — node Ready *and* the local Containerfile change present in the
booted root filesystem. The boot-test exercises the source under test; it does
not false-green on a stale or remote image.

## AC2 — cache-drift gate hard-fails under STRICT_CACHE (no false-green)

Negative control: after a successful deploy (which leaves a cached qcow2
template), the Containerfile was mutated again and the deploy re-run with
`STRICT_CACHE=1`. The #373 freshness gate refused the now-stale template:

```
ERROR: cached CP image (.../hummingbird-k8s-deploy.qcow2) build-ref
  local:<old> != expected local:<new> — STRICT_CACHE=1 refuses to reuse it.
  Set FORCE_REBUILD=1 to rebuild.
[deploy-cluster] ERROR: STRICT_CACHE=1: cached CP image is stale (see ERROR
  above). Rebuild with FORCE_REBUILD=1, or unset STRICT_CACHE to auto-rebuild.
```

(In non-strict mode the same drift emits a `WARN ... forcing rebuild (#373)`
and auto-rebuilds, rather than silently reusing the stale template.)

**AC2 = PASS** — `STRICT_CACHE=1` hard-fails on confirmed cache drift, which is
the anti-false-green behavior epic #378 required.

## Teardown

`make destroy-cluster` returned 0; the test mutation to
`containers/k8s/Containerfile` was reverted (no commit). The libvirt network
and host state were left as prepared.

## Outcome

| AC  | check                                             | result |
|-----|---------------------------------------------------|--------|
| AC1 | local Containerfile change exercised in booted CP | PASS   |
| AC2 | `STRICT_CACHE=1` hard-fails on cache drift         | PASS   |

Epic **#378** acceptance criteria met. The boot-test gate is no longer
structurally false-positive: it runs the local source under test and refuses to
green on a stale cache.

### Notes for future cycles

- bib requires **rootful** podman for the first local qcow2 build; a non-root
  libvirt-group operator can only redeploy when the qcow2 template already
  exists (`lib/build-common.sh` `require_root`). Tracked as a pre-flight UX
  improvement in **issue #4**.
- On a bootc node, verify "did my change ship" via an **on-disk artifact under
  `/usr`** (or a behavioral effect), not a Containerfile `LABEL` — labels are
  not inspectable on the booted ostree system.
- Resolve a freshly-booted VM's IP by matching the **domain's MAC** against the
  libvirt DHCP leases, not "first lease on the subnet" — stale leases from a
  prior VM on the same subnet will otherwise mislead the probe.
