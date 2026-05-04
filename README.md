# Celium

> A clean-sheet, living virtualization fabric. Written from scratch in Rust. No Linux host, no KVM, no inherited hypervisor.

Celium is composed of cooperating crates:

| Crate | Role |
|-------|------|
| `bootloader/celloader` | UEFI stage-0 (≤ 64 KiB) — loads CelHyper, hands off a measured environment |
| `crates/celhyper`  | Bare-metal Type-1 micro-hypervisor (no_std) — paging, EPT/NPT, vCPU scheduling, IOMMU, capability IPC |
| `crates/celcommon` | Shared types, errors, tracing, metrics |
| `crates/celmesh`   | Gossip + plasticity fabric (later) |
| `crates/celvault`  | Secure storage + networking (later) |
| `crates/celcli`    | Operator CLI (later) |
| `crates/celtest`   | Test harness + chaos (later) |

See [`00_GLOBAL_CONVENTIONS.md`](./00_GLOBAL_CONVENTIONS.md) and [`docs/01_CELHYPER.md`](./docs/01_CELHYPER.md). Both files are non-negotiable.

## Workspace layout

The two bare-metal packages (`celloader`, `celhyper`) are intentionally **excluded** from the std workspace and built against their own targets:

```bash
# Std workspace (everything that links libstd)
cargo check
cargo test

# Bare-metal: UEFI stage-0
cd bootloader/celloader && cargo build --target x86_64-unknown-uefi --release

# Bare-metal: hypervisor core
cd crates/celhyper && cargo build --target x86_64-unknown-none --release
```

Required toolchain components (auto-installed by `rust-toolchain.toml`):
`rust-src`, targets `x86_64-unknown-uefi` and `x86_64-unknown-none`.

## Status

Week 12 — **persistent volumes + cross-node attach + supervisor-preserved attachments across restart, plus the first live multi-node integration test with a guest VM running across nodes**. 73 tests pass workspace-wide.

## Documentation

| Doc | Audience |
| --- | --- |
| [`docs/OVERVIEW.md`](./docs/OVERVIEW.md) | Project goal, current features, architecture, weekly status. |
| [`docs/INSTALL.md`](./docs/INSTALL.md) | Toolchain prerequisites, build, test, install `celctl`. |
| [`docs/USAGE.md`](./docs/USAGE.md) | Operator workflows: local VMs, clustering, federated ops, persistent volumes, supervised restart. |
| [`docs/01_CELHYPER.md`](./docs/01_CELHYPER.md) | Bare-metal hypervisor design notes. |
| [`00_GLOBAL_CONVENTIONS.md`](./00_GLOBAL_CONVENTIONS.md) | Non-negotiable engineering rules. |
