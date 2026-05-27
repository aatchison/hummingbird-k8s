# Upstream references

One-stop entry point for the upstream projects this repo composes — `bootc`,
`bootc-image-builder`, the Hummingbird bootc-os base image, cri-o, Cilium, and
the KubeCon talk the pattern was lifted from. Maintained per
[#312](https://github.com/aatchison/hummingbird-k8s/issues/312), which captured
the recurring tax of re-grepping these URLs out of past issues.

Each row lists where the canonical project lives, what role it plays here, and
which in-repo issues are load-bearing on that reference. When a behavioral
constraint in this repo traces back to an upstream design decision, the
"Cross-references" column points at the issue that documents it.

| Reference | URL | What it is | When you need it | Cross-references |
|-----------|-----|------------|------------------|------------------|
| `bootc` project | <https://github.com/containers/bootc> | Source for the `bootc` binary baked into the base image. Tracks `bootc upgrade`, `bootc rollback`, `bootc switch` semantics; release notes; CVEs. | Diagnosing `bootc` CLI behavior, reading release notes before bumping the base image digest, filing upstream issues. | Auto-update timer wiring ([#181](https://github.com/aatchison/hummingbird-k8s/issues/181)); semver-update timer design (`docs/auto-updates.md`). |
| `bootc-image-builder` (BIB) | <https://github.com/osbuild/bootc-image-builder> | Tool that converts a bootable container image into a qcow2 / raw / AMI / ISO / VHD / GCE disk. Invoked by `scripts/build-k8s.sh` and `scripts/build-worker.sh`. | Debugging qcow2 build failures; changing output formats; understanding why `--local` + bind-mounting `/var/lib/containers/storage` is required. | Rootful-podman constraint ([#311](https://github.com/aatchison/hummingbird-k8s/issues/311)); bib produces all formats even when one is requested (NOTES.md "Gotchas"). |
| `bootc-os` base image (registry) | <https://quay.io/repository/hummingbird-community/bootc-os> | The actual image our Containerfiles `FROM` (digest-pinned in `containers/k8s/Containerfile`, `containers/k8s-worker/Containerfile`, and `lib/build-common.sh`'s `BASE_IMAGE` default). | Bumping the base digest; reading the layer history; verifying what the upstream rebuild contains. | Continuous-rebuild cadence + no semver tags ([#298](https://github.com/aatchison/hummingbird-k8s/issues/298)); libguestfs OS-introspection fails on bootc/ostree (operator memory `feedback_libguestfs_ostree.md`). |
| `bootc-os` upstream source | <https://gitlab.com/redhat/hummingbird/containers> | Where the `quay.io/hummingbird-community/bootc-os` image is built from. We pull from quay; the build inputs live on GitLab. | Tracing a package change back to its source commit; filing bugs against the upstream definition; understanding the "no version scheme" model. | Continuous-rebuild discovery ([#298](https://github.com/aatchison/hummingbird-k8s/issues/298)). |
| bootc documentation site | <https://bootc-dev.github.io/bootc/> | Authoritative docs for `bootc upgrade`, `bootc rollback`, `bootc switch`, layering rules, the read-only `/usr` model, and the `bootc-fetch-apply-updates.timer` design. | First stop when debugging update / rollback behavior or designing layering for a new flavor. | `/usr/local` wired to `/var` and read-only-`/usr` gotchas (NOTES.md); auto-update timer design (`docs/auto-updates.md`). |
| `cri-o` project | <https://github.com/cri-o/cri-o> | Container runtime used by kubelet. RPMs come from `pkgs.k8s.io/addons:/cri-o:/stable:/$K8S_VERSION/rpm/`. | Diagnosing CRI errors; pinning a non-default version; understanding why `crio.service` (no hyphen) is the unit name on Fedora. | Pin tracks `K8S_VERSION` via `pkgs.k8s.io` baseurl, no separate cri-o pin ([#299](https://github.com/aatchison/hummingbird-k8s/issues/299)); `crio.service` vs `cri-o.service` (NOTES.md "Gotchas"). |
| Cilium project | <https://github.com/cilium/cilium> | CNI installed at first boot by `k8s-init.sh` via `cilium-cli`. Pinned at **1.16.5** in `containers/k8s/k8s-init.sh:203`, **independently of `K8S_VERSION`**. Per-minor compatibility windows live at <https://docs.cilium.io/en/v1.16/network/kubernetes/compatibility/> (swap the `v1.16` segment to consult another release). | Bumping Cilium; debugging CNI / Hubble; understanding why a K8s version bump doesn't automatically bump Cilium. | Independent-of-K8S_VERSION pin + pre-flight check `make check-cilium-k8s-compat` ([#303](https://github.com/aatchison/hummingbird-k8s/issues/303)); Hubble relay + metrics-server land on podman-CNI IPs first boot ([#259](https://github.com/aatchison/hummingbird-k8s/issues/259)). |
| `pkgs.k8s.io` | <https://pkgs.k8s.io/> | Upstream RPM repos for `kubeadm` / `kubelet` / `kubectl` (`core`) and `cri-o` (`addons:cri-o`). Baseurl pattern is parameterized by `$K8S_VERSION` in `containers/k8s/Containerfile`. | Reading repo metadata; verifying GPG keys; tracing a kubelet/kubeadm RPM version. | K8s version pin lives in `Containerfile` `ARG K8S_VERSION=v1.31`; cri-o version coupling ([#299](https://github.com/aatchison/hummingbird-k8s/issues/299)). |
| KubeCon India 2025 talk — "Build Your K8s Ready Distro With BootC" (Berkus + Kumar) | transcript at [`references/k8s-bootc-talk.transcript.txt`](../references/k8s-bootc-talk.transcript.txt); recording URL <!-- TODO: stable upstream URL — file a follow-up issue and link here --> | The talk this repo's pattern (kubeadm RPMs straight into a fedora-bootc image, then bib → qcow2) is lifted from. | New contributors orienting on **why** the design looks the way it does; the demo repos (linked below) are the closest public reproductions of the live demo. | Pattern citation in NOTES.md "Install style" + "Reference: Praveen Kumar's public bootc demos". |
| Praveen Kumar — `devconfin26` demo | <https://github.com/praveenkumar/devconfin26> | Adjacent public bootc demo from the same author: Gitea dev-platform appliance with `.github/workflows/build.yml`, `Containerfile`, `quadlet/`, `systemd/`, `scripts/`. Best template for a GH Actions build pipeline. | Cross-referencing how a working bootc build pipeline is structured. | NOTES.md "Reference: Praveen Kumar's public bootc demos". |
| Praveen Kumar — `fossasia26` demo | <https://github.com/praveenkumar/fossasia26> | Apache v1 vs v2 (with intentional break in v2) — minimal upgrade/rollback demo. | Smallest end-to-end illustration of `bootc upgrade` + `bootc rollback`. | NOTES.md "Reference: Praveen Kumar's public bootc demos". |

## What's intentionally not here

- `kubeadm` / `kubelet` / `kubectl` GitHub URLs: covered indirectly via the
  `pkgs.k8s.io` row, since RPMs are the actual integration surface, not the
  source repos.
- `libvirt` / `qemu` / `podman`: standard Fedora components, not bootc-specific
  enough to be load-bearing for the repo's design. The `README.md`
  "Prerequisites" section is the right place for those.
- `argo-cd` / `kube-bench` / individual addon docs: those live in the per-feature
  docs (`docs/argocd.md`, `docs/kube-bench.md`). This file stays scoped to the
  upstream **OS-layer** projects.

## Updating this file

When you find yourself re-discovering an upstream URL — especially one that
explains a behavior constraint already captured in an issue — add a row here
and link the issue in the cross-references column. The goal is that the next
fresh agent reading the repo top-down doesn't pay the same tax twice.
