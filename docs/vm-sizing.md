# VM sizing and resource defaults

VM resource defaults are tuned for a small homelab cluster on a single KVM
host. This doc records the current defaults, the env-var tunables, and the
sizing guidance behind them. Closes #88.

## Defaults

The values below come from `scripts/deploy-cluster.sh` and
`scripts/spawn-workers.sh`. qcow2 disk size is whatever
`bootc-image-builder` produces for the rootfs (no explicit size override
today); the values listed reflect the rootfs size after first boot.

| Flavor | RAM | vCPU | qcow2 size |
| --- | --- | --- | --- |
| `hummingbird-k8s` (control plane) | 8192 MB | 4 | 30 GB |
| `hummingbird-k8s-worker` (each) | 4096 MB | 2 | 20 GB |

Total for a 1 CP + 2 worker lab: 16 GB RAM, 8 vCPU, ~70 GB disk.

## Tunables

Memory and vCPU are env-driven (closes #91). Override via
`cluster.local.conf` (the canonical knob set for `make deploy-cluster`)
or `config.local.sh` for image-build-only flows; defaults match the
pre-knob hardcoded values so unset == unchanged behavior. See
[`cluster.example.conf`](../cluster.example.conf) for the canonical list.

| Variable | Default | Applies to |
| --- | --- | --- |
| `CP_MEMORY` | `8192` | Control plane VM |
| `CP_VCPUS` | `4` | Control plane VM |
| `WORKER_MEMORY` | `4096` | Each worker (`spawn-workers.sh` / `deploy-cluster.sh`) |
| `WORKER_VCPUS` | `2` | Each worker |
| `POOL_DIR` | `/var/lib/libvirt/images` | libvirt storage pool path |

qcow2 disk size is not a wired knob today — bib emits a fixed-size rootfs
per flavor. If you need a larger disk, expand the qcow2 + rootfs after
build, or open an issue requesting a `*_QCOW_SIZE` knob.

## Sizing guidance

- **Lab minimum (1 CP + 2 workers)**: 16 GB RAM, 8 vCPU, 70 GB disk on the
  host. Below 16 GB the CP starts evicting workloads under modest load.
- **Real workloads**: scale workers per workload requirements. Etcd on the
  CP is the typical bottleneck — give it dedicated IO (NVMe-backed qcow2
  noticeably improves write latency vs. spinning disk).
- **Do not drop the CP below 4 GB RAM.** etcd + apiserver alone consume
  ~2 GB at steady state; below 4 GB the OOM killer hits kube-system pods.
- **Each worker reserves ~500 MB** for kubelet/cri-o + Cilium. 4 GB leaves
  ~3 GB for workloads.
- **Swap**: kubelet kills nodes that swap. Ensure swap is off — the
  Hummingbird base image does this by default; verify with
  `swapon --show` returning nothing on each VM.
- **Disk growth over time**: image pulls and audit logs are the main
  consumers. Apiserver audit logs are capped at ~500 MB total
  (100 MB x 5 rotations — see [`security-hardening.md`](security-hardening.md)).
  Container image layers in `/var/lib/containers` grow unboundedly without
  `podman image prune` or cri-o GC; budget +5 GB/year per active workload.

## Multi-host

Not supported today — `qemu:///system` and the libvirt NAT topology assume
a single KVM host runs all VMs. HA control plane across hosts is tracked
in #11.

## Cross-links

- [`auto-updates.md`](auto-updates.md) — bootc auto-update timer behavior;
  reboots affect cluster availability and should be considered when sizing
  for HA-on-one-host (you don't get HA from one host).
- [`security-hardening.md`](security-hardening.md) — audit log disk usage
  (the ~500 MB cap referenced above).
- `backup-restore.md` — etcd snapshot disk space (once #79 lands).
