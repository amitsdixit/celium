# ADR 0001 — CelHyper Design

Status: Accepted (2026-05-04, Week-1 sprint)

## Context

CelHyper is the trusted computing base for Celium. The spec
(`docs/01_CELHYPER.md`) gives us four hard responsibilities and an
auditability budget of < 120 kLOC. Several structural choices needed to be
fixed before any code shipped.

## Decision

1. **Type-1, no host OS.** CelHyper boots directly on bare metal via a tiny
   UEFI stage-0 (CelLoader). No Linux, no KVM, no rump kernel. Justified by
   the spec's auditability + capability goals — every line in our chain of
   trust is ours.

2. **Two crates, two targets.**
   - `bootloader/celloader` → `x86_64-unknown-uefi`, ≤ 64 KiB.
   - `crates/celhyper`      → `x86_64-unknown-none`, no_std.

   They are *excluded* from the std workspace so each can have its own
   `.cargo/config.toml` target. The std-side crates (`celcommon`, `celcli`,
   …) build with the host toolchain and run normal `cargo test`.

3. **Handoff struct duplicated, version-stamped.** The `CeliumHandoff` block
   appears in both crates with identical `#[repr(C)]` layout and a `MAGIC`
   + `VERSION` field. Cross-target sharing via a third crate was rejected
   because it would force one of the two bare-metal crates to depend on the
   other's target conventions. Version bumps are mechanical and reviewed.

4. **Safe Rust by default; unsafe only at hardware seams.**
   `forbid(unsafe_op_in_unsafe_fn)` is set at crate root. The few unsafe
   blocks (`rdmsr`, port I/O, raw handoff read) carry SAFETY comments that
   state the precondition the caller must uphold.

5. **No `unwrap`/`panic` in production paths.** Every fallible API returns
   `HyperResult<T>` (kernel) or `CelResult<T>` (std side). The kernel's
   `panic_handler` exists only as defence in depth; reaching it is a bug.

6. **Stub-with-stable-signatures for Week-2 work.** `Ept::map_4k`,
   `Scheduler::admit`, `Iommu::*`, and `vm::launch_first_guest` are present
   with their final signatures and return `HyperError::Unimplemented`. This
   lets `cap`/`vm`/`logger` integrate against real APIs today and prevents
   churn when the real bodies land.

## Consequences

- Week-1 closes with a fully compiling tree, a CelLoader that runs under
  OVMF/QEMU and prints discovery info, and a CelHyper kernel ELF that
  validates the handoff and reports VMX availability.
- Week-2 work (EPT walker, VMCS bring-up, VMLAUNCH, exit boot services)
  has a clear seam to land in.
- The duplicated handoff struct is the one piece of technical debt. We
  accept it; if it ever falls out of sync it will be caught by the
  `MAGIC`/`VERSION` check at first boot.
