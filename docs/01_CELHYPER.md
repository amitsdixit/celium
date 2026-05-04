# 01 — CelHyper (Micro-Hypervisor)

**Scope:** A (foundation) | **Estimated effort:** 6-10 weeks, 1-2 senior engineers  
**Depends on:** None (this is the root of Celium)

## Purpose

CelHyper is the clean-sheet Type-1 micro-hypervisor that boots directly on bare metal. It is the trusted computing base for guest isolation and the foundation of the entire Celium fabric. It must be small enough to be auditable in its entirety (< 120,000 lines total).

It performs four functions and only four:
1. Manages second-level page tables (EPT/NPT) for private guest address spaces.
2. Schedules vCPUs onto physical cores.
3. Programs the IOMMU for direct device passthrough (SR-IOV, PCIe).
4. Provides capability-based IPC for higher layers (CelMesh, CelVault).

## Implementation Strategy

CelHyper is written from scratch in safe Rust. No Linux, no KVM, no host OS underneath. It uses modern hardware virtualization extensions directly.

## Component Structure

### 1.1 CelLoader (64 KiB stage-0)
- UEFI Secure Boot compatible
- Loads CelHyper binary
- Performs initial hardware discovery (ACPI, CPUID, memory map)
- Hands off control with a clean, measured environment

### 1.2 CelHyper Core (Rust)
- Boot to long mode, paging, and EPT/NPT setup
- Minimal scheduler (proportional share + dedicated-core reservation)
- IOMMU domain creation and device assignment
- Capability verification on all control paths

## Acceptance Criteria

1. Boots on real 2025-era AMD/Intel hardware via UEFI.
2. Launches an unmodified Linux guest that reaches userspace.
3. Launches an unmodified Windows guest (virtio drivers) to login screen.
4. Capability enforcement works on all control paths.
5. Multi-VM isolation holds (guests cannot read each other’s memory).
6. IOMMU isolation holds (passed-through devices cannot DMA outside their domain).
7. Scheduling policies are observable under load.
8. Survives failure injection (VMM process killed, etc.).
9. Full documentation and runbook exist.

## Explicit Non-Goals for v0.1
- Confidential computing (SEV-SNP/TDX) — deferred to v0.2
- Full live migration — deferred
- GPU passthrough beyond basic SR-IOV — deferred

## Risks and Mitigations
| Risk | Mitigation |
|------|------------|
| Boot loader fragility | Heavy testing on real hardware from day 1 |
| EPT/NPT correctness bugs | Formal verification of memory management paths |
| Performance regression | Continuous benchmarking against bare metal |

## Borrowed Components
- None for the core. We write it ourselves.

## Files this component owns
- `bootloader/`
- `crates/celhyper/`
- `docs/adr/0001-celhyper-design.md`

## Order of Implementation (Week 1 Sprint)
1. CelLoader UEFI boot + handoff
2. Basic Rust runtime + paging
3. EPT/NPT setup + single test VM launch
4. Capability hook skeleton
5. First integration test (guest reaches “hello world”)

## Human Review Checkpoints
1. After CelLoader is working on real hardware
2. After first guest VM boots
3. Before declaring Week 1 complete
