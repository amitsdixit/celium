# Celium

> A clean-sheet, **living** virtualization fabric. Written from scratch in Rust.
> No Linux host, no KVM, no inherited hypervisor — Celium owns the silicon
> from UEFI boot all the way up to a Kubernetes-shaped operator surface.

[![Status](https://img.shields.io/badge/status-W15%20complete-brightgreen)]()
[![Tests](https://img.shields.io/badge/tests-96%20pass%20%2F%200%20fail-brightgreen)]()
[![Rust](https://img.shields.io/badge/rust-stable%201.88-orange)]()
[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue)]()

---

## What is Celium?

Celium is a **Type-1 hypervisor + clustered control plane** built as one
project. Most cloud platforms layer a control plane (Kubernetes,
Nomad, ECS) on top of an existing hypervisor (KVM, Hyper-V, ESXi).
Celium does both — and treats them as the same product.

The goal is a virtualization platform that is:

- **Auditable.** Every fallible API returns `Result<T, CelError>`. No
  `unwrap()` on production paths. Every `unsafe` block has a
  `// SAFETY:` comment. Two crates (`celmesh`, `celvault`) declare
  `#![forbid(unsafe_code)]`.
- **Decoupled.** A clean trait split between control plane (mesh,
  vault, capabilities, K8s personality, observability) and data plane
  (the bare-metal `celhyper` Type-1 hypervisor). The control plane is
  testable in pure user-space Rust; the data plane runs `no_std` on
  bare metal under UEFI.
- **Living.** Resources self-heal. A VM with `RestartPolicy::Always`
  whose owner node dies is recreated by the elected supervisor on a
  surviving node, keeping its volume attachments and its capability
  envelope intact.
- **Capability-secure.** Every wire op carries a stable tag
  (`vm.create`, `net.nic.attach`, `lb.create`, …). The receiving
  host enforces a [`Capabilities`](crates/celmesh/src/capabilities.rs)
  bit-set per peer; mutating ops without the right bit return a
  stable `capability denied` error string.

It is **not** a Kubernetes distribution. K8s is a *personality* — one
of several ways to drive Celium VMs. The K8s personality
(`crates/celmesh/src/k8s.rs`) takes a `K8sClusterSpec` and provisions a
network + control-plane VM + worker VMs + load balancer through the
existing mesh RPCs. Future personalities (Lambda-shaped, Nomad-shaped,
plain VM) sit alongside it.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│  celctl (Operator CLI)            ─ clap-driven, JSON state file        │
│  ──────────────────────────────────────────────────────────────────────  │
│  cluster start | status | vms | vm {…} | vol {…} | invoke[-path]        │
└─────────────────────────────────────────────────────────────────────────┘
                                     │  one-shot mesh RPCs (UDP+JSON)
┌────────────────────────────────────▼────────────────────────────────────┐
│  celmesh (Control plane, std, async)                                    │
│                                                                         │
│   ┌─ membership ─┐  ┌─ federation ─┐  ┌─ host ──────────────┐           │
│   │ SWIM-lite    │  │ gossiped     │  │ VmHost trait        │           │
│   │ alive/suspect│  │ namespace    │  │ MemVmHost (caps)    │           │
│   │ /dead/left   │  │ /cluster/<n> │  │ supervisor recovery │           │
│   └──────────────┘  └──────────────┘  └─────────────────────┘           │
│                                                                         │
│   ┌─ k8s ────────┐  ┌─ observability ┐  ┌─ capabilities ────┐           │
│   │ K8sCluster   │  │ ClusterReport  │  │ 13-bit set        │           │
│   │ create /     │  │ NodeReport     │  │ per-op required() │           │
│   │ destroy      │  │ VolumeUsage    │  │ stable op_tag()   │           │
│   └──────────────┘  └────────────────┘  └───────────────────┘           │
│                                                                         │
│   ┌─ proto (wire) ──────────┐  ┌─ transport ─────────┐                  │
│   │ Envelope { magic,       │  │ MemTransport (test) │                  │
│   │   version, hlc, payload}│  │ UdpTransport (live) │                  │
│   │ VmOp / VmOpReply        │  │ HMAC frame auth     │                  │
│   └─────────────────────────┘  └─────────────────────┘                  │
└─────────────────────────────────────────────────────────────────────────┘
                                     │  trait calls (sync)
┌────────────────────────────────────▼────────────────────────────────────┐
│  celvault  (Storage + networking primitives, std, no_unsafe)            │
│                                                                         │
│   ┌─ VolumeStore ──────────────┐  ┌─ NetworkStore ────────────────┐     │
│   │ create / read / write      │  │ create_network / attach_nic   │     │
│   │ snapshot / restore         │  │ create_security_group         │     │
│   │ MemVolumeStore (in-mem)    │  │ create_load_balancer          │     │
│   │ FileVolumeStore (disk)     │  │ MemNetworkStore               │     │
│   └────────────────────────────┘  └───────────────────────────────┘     │
└─────────────────────────────────────────────────────────────────────────┘
                                     │  (will plug in here at W17+)
┌────────────────────────────────────▼────────────────────────────────────┐
│  celhyper  (Bare-metal Type-1 hypervisor — no_std, x86_64-unknown-none) │
│  EPT/NPT, VMX/SVM, vCPU scheduling, IOMMU, capability IPC               │
│  ──────────────────────────────────────────────────────────────────────  │
│  celloader (UEFI stage-0, x86_64-unknown-uefi)                          │
└─────────────────────────────────────────────────────────────────────────┘
```

### Crate map

| Crate | Lines | Target | Role |
|---|---|---|---|
| [`bootloader/celloader`](bootloader/celloader) | small | `x86_64-unknown-uefi` | UEFI stage-0 (≤ 64 KiB) — measures + loads CelHyper. **Workspace-excluded.** |
| [`crates/celhyper`](crates/celhyper) | medium | `x86_64-unknown-none` | Type-1 micro-hypervisor: paging, EPT/NPT, vCPU scheduling, IOMMU, capability IPC. **Workspace-excluded.** |
| [`crates/celcommon`](crates/celcommon) | small | host | Shared `CelError` / `CelResult`, tracing, metrics. |
| [`crates/celvault`](crates/celvault) | ~1.5k | host | Volumes (`VolumeStore`, `MemVolumeStore`, `FileVolumeStore`), snapshots, networking primitives (`NetworkStore`, `Cidr4`, `SecurityGroup`, `LoadBalancer`). `#![forbid(unsafe_code)]`. |
| [`crates/celmesh`](crates/celmesh) | ~6k | host | Membership (SWIM-lite), namespace federation, capability authorisation, K8s personality, cluster observability, UDP+HMAC transport. `#![forbid(unsafe_code)]`. |
| [`crates/celcli`](crates/celcli) | ~1.2k | host | `celctl` operator binary — clap-driven, JSON state file. |
| [`crates/celtest`](crates/celtest) | ~3k | host | Integration + e2e tests across mesh, vault, capabilities, K8s, observability. |

### Wire protocol

Every mesh frame is a length-prefixed JSON envelope so `tcpdump` works
without a decoder:

```json
{
  "magic":   "celmesh/1",
  "version": 1,
  "from":    "n1",
  "cluster": "default",
  "hlc":     { "wall_ms": 1714972800000, "logical": 7 },
  "payload": { "VmOp": { "Create": { "label": "web", "restart_policy": "Always" } } }
}
```

Frames are HMAC-authenticated and capped at 64 KiB. The protocol is
deliberately small: 28 `VmOp` variants + 28 matching `VmOpReply`
variants cover every operator action in the system today.

---

## What's implemented today

**Status:** Week 15 complete. 96 tests pass / 0 fail / 1 ignored across
21 test suites. Validated on Windows + Ubuntu 24.04.

### Control plane

- ✅ **Multi-node clustering.** SWIM-lite membership over UDP with
  alive / suspect / dead / left states, deterministic supervisor
  election, partition tolerance.
- ✅ **Federated namespace.** Every node sees every cluster VM under
  `/cluster/<node>/vms/<n>`. Federation gossips on join.
- ✅ **VM lifecycle.** Create, list, start, stop, delete, with
  `RestartPolicy::{Never, OnFailure, Always}`.
- ✅ **Persistent volumes.** `MemVolumeStore` and disk-backed
  `FileVolumeStore`. Random-access read/write, attach/detach, full
  snapshot lifecycle (create / list / delete / restore). Cross-node
  attachments survive owner failure.
- ✅ **Living restart.** A VM with `Always` whose owner node dies is
  recreated by the supervisor on a surviving node, with its volume
  attachments preserved.
- ✅ **Capability-based security.** 13-bit `Capabilities` envelope:
  `VM_LIFECYCLE_{READ,WRITE}`, `VOLUME_{READ,WRITE,ATTACH}`,
  `SNAPSHOT_{READ,WRITE}`, `NETWORK_{READ,WRITE}`,
  `SECGROUP_{READ,WRITE}`, `LB_{READ,WRITE}`. Stable error string
  `capability denied: <op_tag>`.
- ✅ **Networking primitives (W15).** Virtual networks with IPv4
  CIDRs, deterministic NIC allocator with locally-administered MACs,
  security groups (stateless ACL, ingress + egress, port ranges,
  CIDR matchers, allow/deny), load balancers (`RoundRobin`,
  `LeastConn`).
- ✅ **K8s-as-a-Service groundwork (W15).** `K8sCluster::create`
  provisions a network + control-plane VM + N workers + a front-end
  LB through existing mesh RPCs. `K8sCluster::destroy` tears it
  down. The VIP is allocated at the highest usable host address so
  it never collides with the linear NIC allocator.
- ✅ **Observability (W15).** `Mesh::cluster_report()` returns a
  cluster-wide `ClusterReport` with per-node `vm_count`,
  `volume_count`, `total_volume_bytes`, `network_count`, and a
  `reachable` flag, aggregated via parallel RPCs. Unreachable peers
  reduce to `reachable=false` rather than failing the report.
- ✅ **Structured tracing.** Per-RPC spans, per-applied-op debug
  events, with stable op tags so logs grep cleanly.
- ✅ **Operator CLI.** `celctl`:
  - `cluster start | status | members | vms | recover`
  - `cluster vm {create, list, start, stop, delete, attach-volume, detach-volume}`
  - `cluster vol {create, list, delete, read, write, snapshot, snapshots, restore, delete-snapshot}`
  - `cluster invoke / invoke-path` — generic op dispatch (used today
    for `network`/`secgroup`/`lb`/`k8s` until polished sub-trees
    land in W15.5).

### Data plane

- 🟡 **CelHyper code is written** (W7–W12): VMX setup, EPT
  programming, full guest-state programming for real-mode
  unrestricted-guest, vmlaunch path, register save/restore. **Not
  yet booted on QEMU or bare metal.**
- 🟡 **CelLoader code is written**: UEFI stage-0 boot path. Not yet
  booted in OVMF.

The control plane bridge to celhyper (a `CelhyperVmHost` that
translates `VmOp::Start` into a real `vmlaunch`) does not exist yet.
Today every test runs against `MemVmHost` — see [Testing
philosophy](#testing-philosophy).

### Quick command reference

```bash
# Everything below runs on a normal Linux/Windows host (no KVM).

# 1.  Build the entire control plane.
cargo build --workspace

# 2.  Run every test (96 passing, ~6s).
cargo test  --workspace

# 3.  Bring up a 2-node cluster on localhost (terminal A).
celctl --state-file /tmp/n1/state.json cluster start \
       --node-id n1 --bind 127.0.0.1:7001 --duration 600

# 4.  Join as n2 and inspect (terminal B).
celctl --state-file /tmp/n2/state.json cluster start \
       --node-id n2 --bind 127.0.0.1:7002 --seed 127.0.0.1:7001 \
       --duration 600 &

celctl --state-file /tmp/n2/state.json cluster status \
       --node-id n2 --bind 127.0.0.1:7003 --seed 127.0.0.1:7001
celctl --state-file /tmp/n2/state.json cluster vms \
       --node-id n2 --bind 127.0.0.1:7004 --seed 127.0.0.1:7001
```

Each `cluster <subcmd>` call needs a unique `--bind` because each
spins up a transient mesh node for one RPC. Rust API users
(`celmesh::Mesh`, `K8sCluster::create`) speak directly without that
limitation.

---

## Roadmap

Sequential weekly milestones. Each line of the table corresponds to a
single shippable, fully-tested commit on `main`.

### Done

| W# | Theme | Highlights |
|---|---|---|
| W1–W6  | Foundation        | Workspace, error type, tracing, metrics, base mesh |
| W7–W9  | Hypervisor + cluster | celhyper VMX/EPT skeleton, celloader UEFI, mesh + federation |
| W10    | Federated VM ops | Cross-node create/list/start/stop/delete |
| W11    | Resiliency I    | Supervisor election, restart-on-failure |
| W12    | Volumes I       | `VolumeStore` trait, mem + file store, attach/detach |
| W13    | Volumes II      | Snapshots: create / list / restore / delete |
| W14    | Hardening       | Capabilities, structured logs, polished CLI subtrees, multi-node e2e |
| **W15**| **Networking + K8s + Obs** | **Networks, NICs, SGs, LBs, K8s personality, ClusterReport** |

### Planned (next)

| W# | Theme | Goals |
|---|---|---|
| **W15.5** | **CLI polish for W15 surface** | `celctl cluster network / secgroup / lb / k8s / report` subcommand trees over the existing wire ops |
| **W16** | **celhyper boots on QEMU** | Cross-build celloader + celhyper into a `celium.iso`. Run under `qemu-system-x86_64 -enable-kvm` (nested virt on the build VM). Banner via 0xE9 debug-port + QEMU `isa-debug-exit`. One feature-gated e2e test. |
| **W17** | **First real guest** | Hand-rolled 4 KiB guest payload that writes a magic byte to a port. Wire `CelhyperVmHost::start_vm` → real `vmlaunch`. |
| **W18** | **Replace MemVmHost** | Swap `MemVmHost` for `CelhyperVmHost` in the e2e suite; rerun every W14/W15 scenario against the real hypervisor. |
| W19 | Storage II | Write-through page cache, sparse files, on-restore checksum |
| W20 | Networking data plane | Userspace bridge → tap + ACL enforcement at the celhyper boundary |
| W21 | K8s personality II | k3s image baked into a guest, kubelet join, `kubectl get nodes` works |
| W22 | Observability II | Prometheus metrics endpoint, OTLP traces, alerting rules |
| W23 | Multi-tenant | Per-tenant capability envelopes, network isolation, quota |
| W24 | Live migration | Pre-copy + post-copy across two celhyper nodes |
| W25 | Public preview | Demo: 3-node cluster boots from cold metal, runs a 5-pod K8s personality, survives one node kill |

The split between control-plane (✅ today) and data-plane (🟡
celhyper code written, 🔲 never booted) is deliberate. Building the
control plane first lets us move fast with `cargo test` and `cargo
clippy`; QEMU integration lands as a **separate** rung of the test
pyramid in W16.

---

## Testing philosophy

```
        ┌────────────────────────────┐
        │ E2E: real QEMU boot,       │  W17+ — slow, flaky-tolerant
        │ celhyper + tiny guest      │
        └────────────────────────────┘
       ┌───────────────────────────────┐
       │ Integration: celhyper on QEMU │  W16 — KVM nested-virt
       │ (no guest yet)                │
       └───────────────────────────────┘
      ┌──────────────────────────────────┐
      │ Control-plane integration        │  Today — 96 tests, real UDP,
      │ (mesh, vault, K8s spec)          │           MemVmHost, ~6s
      └──────────────────────────────────┘
     ┌─────────────────────────────────────┐
     │ Unit tests per crate                │  Today — included in 96
     └─────────────────────────────────────┘
```

The 96 control-plane tests use:

- `MemVmHost` / `MemVolumeStore` / `MemNetworkStore` — pure in-memory
  fakes that obey the same trait contracts as the on-disk and
  on-bare-metal implementations.
- `LoopbackTransport` for unit tests, `UdpTransport` over
  `127.0.0.1` for integration tests (so packets really do hit the
  kernel network stack).
- No QEMU, no KVM, no `tokio::test` race avoidance hacks.

This gives us a 6-second feedback loop on the orchestrator logic
without wiring nested virt. QEMU-based tests will be added in W16
behind a `qemu-e2e` feature gate so they don't slow normal `cargo
test`.

---

## Repository layout

```
celium/
├── Cargo.toml                  # std workspace (members + bare-metal exclusions)
├── Cargo.lock
├── rust-toolchain.toml         # stable + rust-src + UEFI/none targets
├── 00_GLOBAL_CONVENTIONS.md    # non-negotiable engineering rules
├── README.md                   # this file
├── docs/
│   ├── 01_CELHYPER.md          # bare-metal hypervisor design notes
│   ├── INSTALL.md              # toolchain prerequisites + build
│   ├── OVERVIEW.md             # narrative overview
│   ├── USAGE.md                # operator workflows
│   └── adr/0001-celhyper-design.md
├── crates/
│   ├── celcommon/              # CelError, tracing helpers
│   ├── celvault/               # Volumes + networking primitives
│   ├── celmesh/                # Cluster fabric (incl. k8s, observability)
│   ├── celcli/                 # celctl operator binary
│   ├── celtest/                # Integration + e2e tests
│   └── celhyper/               # Bare-metal hypervisor (workspace-excluded)
└── bootloader/
    └── celloader/              # UEFI stage-0 (workspace-excluded)
```

## Building

```bash
# 1.  Std workspace — everything that links libstd.
cargo check --workspace
cargo test  --workspace

# 2.  Bare-metal: UEFI stage-0.
cd bootloader/celloader && cargo build --target x86_64-unknown-uefi --release

# 3.  Bare-metal: hypervisor core.
cd crates/celhyper && cargo build --target x86_64-unknown-none --release
```

The `rust-toolchain.toml` pins stable Rust 1.88+ and auto-installs
`rust-src`, `x86_64-unknown-uefi`, and `x86_64-unknown-none`.

## Documentation

| Doc | Audience |
|---|---|
| [`docs/OVERVIEW.md`](docs/OVERVIEW.md)   | Project goal, architecture, weekly status. |
| [`docs/INSTALL.md`](docs/INSTALL.md)     | Toolchain prerequisites, build, install `celctl`. |
| [`docs/USAGE.md`](docs/USAGE.md)         | Operator workflows: VMs, clustering, volumes, supervised restart. |
| [`docs/01_CELHYPER.md`](docs/01_CELHYPER.md) | Bare-metal hypervisor design notes. |
| [`docs/adr/0001-celhyper-design.md`](docs/adr/0001-celhyper-design.md) | Architecture decision record for the hypervisor. |
| [`00_GLOBAL_CONVENTIONS.md`](00_GLOBAL_CONVENTIONS.md) | Non-negotiable engineering rules. |

## License

Dual-licensed under **Apache-2.0 OR MIT** at your option.
