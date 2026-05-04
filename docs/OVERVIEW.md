# Celium — Project Overview

## Project goal

Celium is a clean-sheet, **living virtualization fabric** written entirely
in Rust. It is not a wrapper around an existing hypervisor and does not
sit on top of Linux + KVM. The objective is a small, auditable stack
that boots from UEFI, runs a Type-1 micro-hypervisor, and forms a
self-healing cluster across many physical nodes — with all parts
written from scratch so every byte on the wire and every page table
entry is owned by this project.

Pillars:

1. **Bare-metal core** — UEFI stage-0 (`celloader`) hands off to a
   `no_std` Type-1 hypervisor (`celhyper`) with paging, EPT/NPT,
   vCPU scheduling, IOMMU, and capability-style IPC.
2. **Strict Rust hygiene** — workspace-wide `Result<T, CelError>`,
   no `unwrap`/`panic` in library code, every `unsafe` block carries a
   `// SAFETY:` justification, `celmesh` and `celvault` are
   `#![forbid(unsafe_code)]`.
3. **Distributed by construction** — every host runs a `celmesh`
   gossip + federation engine. VMs created on any node are visible
   from every node; the lowest-id Alive node acts as supervisor and
   recreates orphans of failed peers.
4. **Living fabric** — persistent volumes (`celvault`) follow VMs
   across nodes; supervisor preserves volume attachment metadata when
   a VM is restarted on a healthy node.

## Repository layout

| Path | Purpose |
| --- | --- |
| `bootloader/celloader/` | UEFI stage-0 loader (≤ 64 KiB, custom target). |
| `crates/celhyper/` | Bare-metal Type-1 micro-hypervisor (`no_std`, `x86_64-unknown-none`). |
| `crates/celcommon/` | Shared `CelError`/`CelResult`, tracing/metrics helpers. |
| `crates/celmesh/` | Gossip, membership, federated namespace, RPC, supervisor. |
| `crates/celvault/` | Persistent Volume API (`VolumeStore` trait + in-memory store). |
| `crates/celcli/` | Operator CLI binary `celctl`. |
| `crates/celtest/` | Integration test harness (in-process and real-UDP multi-node). |
| `docs/` | Architecture and operator documentation. |
| `00_GLOBAL_CONVENTIONS.md` | Non-negotiable engineering rules. |

The std workspace (Cargo.toml `[workspace] members`) builds with the
default host toolchain. `celloader` and `celhyper` are explicitly
**excluded** and built against custom bare-metal targets — see
[Installation](INSTALL.md).

## Current features (W1 → W12)

| Area | What ships today |
| --- | --- |
| **Bootloader / hypervisor** | `celloader` UEFI stage-0 skeleton; `celhyper` boot/paging/EPT skeleton. (Active development.) |
| **Errors / observability** | Workspace-wide `CelError`, structured `tracing` events, Prometheus metrics scaffolding. |
| **Local VM model** | `MemVmHost` deterministic single-step guest model with `Create / Start / Stop / Delete / List`, slot ids, last-exit codes, restart policy `Never \| Always`. |
| **Cluster fabric** | UDP and in-memory transports, JSON-framed gossip (`magic="celmesh/1"`, version=1, MAX_FRAME_BYTES=64 KiB), seed-based discovery, SWIM-style Alive/Suspect/Dead, LWW by `(epoch, hlc)`. |
| **Federation** | Every VM gossiped as a `RemoteVm` row to all peers; visible cluster-wide via `Mesh::list_vms()`; addressable by federated path `/cluster/<node>/vms/<id>`. |
| **Cross-node RPC** | `Mesh::invoke()` and `Mesh::invoke_path()` route any `VmOp` to the owning node; replies are typed `VmOpReply` variants. |
| **Auto-supervisor** | Lowest-id Alive node periodically recreates orphans whose `restart_policy = Always`. Runs on a configurable interval (`MeshConfig::supervisor_interval`; `Duration::ZERO` disables). |
| **Persistent volumes (W12)** | `celvault::VolumeStore` trait + `MemVolumeStore`. Volume id `<owner>/v<n>`; bounds: name ≤ 64, mount ≤ 32, size ≤ 64 MiB. CRUD + byte-range read/write. |
| **Volume RPC (W12)** | `VmOp::{CreateVolume, DeleteVolume, ListVolumes, AttachVolume, DetachVolume}` + matching replies. `RemoteVm.volumes` carried over gossip. |
| **Resilient attachments (W12)** | Supervisor calls `VmHost::attach_preserved` when reviving an orphan, restoring its `Vec<VolumeAttachment>` even when the volume's vault lives on a third (Alive) node. |
| **Operator CLI** | `celctl version / probe / vm * / cluster (start \| members \| vms \| invoke \| invoke-path \| recover \| status)`. Volume ops surfaced through `cluster invoke`. |
| **Test harness** | 73 tests across the workspace including 3 real-UDP multi-node integration tests for federation, supervised restart, and persistent volumes across owner failure. |

## Architecture at a glance

```
                +--------------------+        +--------------------+
                |   node n1          | <----> |   node n2          |
                |  Mesh + VmHost     | gossip |  Mesh + VmHost     |
                |  VolumeStore (n1)  |  UDP   |  VolumeStore (n2)  |
                +--------------------+        +--------------------+
                          ^                              ^
                          |                              |
                          +--------- celctl -------------+
                                  (operator CLI)
```

All inter-node traffic is JSON over UDP today; the wire format is
versioned so the binary frame swap is mechanical. The supervisor is
purely an opinion of `Mesh`: any node may recreate an orphan, but
contention is resolved by `(epoch, hlc)` LWW so recreations are safe
under split-brain.

## Engineering rules (`00_GLOBAL_CONVENTIONS.md`, abridged)

* All public fallible APIs return `CelResult<T>`. No `unwrap` /
  `expect` / `panic` on production paths.
* Every `unsafe` block carries a `// SAFETY:` justification. The
  fabric crates (`celmesh`, `celvault`) `#![forbid(unsafe_code)]`.
* Async paths use Tokio; no blocking IO in async fns.
* Errors are typed via `thiserror`; logs are structured `tracing`
  events; metrics use `prometheus`.

## Status timeline

| Week | Headline |
| --- | --- |
| W1–W3 | UEFI loader + hypervisor skeleton. |
| W4–W7 | Local VM model, errors/metrics, persistence, CLI scaffolding. |
| W8 | First supervisor stub. |
| W9 | Gossip, membership, federated namespace over UDP. |
| W10 | Federated RPC + path routing. |
| W11 | Auto-supervisor + cluster_status + multi-node UDP tests. |
| **W12** | **Persistent volumes; cross-node attach; supervisor preserves attachments across restart; first live multi-node integration test with a guest VM running across nodes.** |

See [USAGE.md](USAGE.md) for operator workflows and
[INSTALL.md](INSTALL.md) for environment setup.
