//! Intel VMX subsystem.
//!
//! Layered exactly as the SDM presents it:
//!
//! * [`fields`]  — VMCS field encodings (Vol 3 Appendix B).
//! * [`region`]  — `VmxonRegion` and `VmcsRegion` (4 KiB, revision-id stamped).
//! * [`cpu`]     — per-CPU enable: CR0/CR4 fixed bits, CR4.VMXE, VMXON.
//! * [`vmcs`]    — `vmclear`, `vmptrld`, `vmread`, `vmwrite` wrappers.
//! * [`launch`]  — VMCS construction for the first guest, `vmlaunch` wrapper,
//!                 vm-exit reason decode.
//!
//! ## Reality check
//!
//! Code in this module compiles cleanly for `x86_64-unknown-none` and is
//! structurally correct against the SDM. **It cannot be observed running
//! on this dev box.** A successful `vmlaunch` requires:
//!
//! 1. an Intel CPU with VT-x in the firmware-unlocked state, OR
//! 2. a hypervisor that exposes nested VMX (KVM with `nested=1`, etc.).
//!
//! Neither is configured in the current Windows dev environment. Per
//! `00_GLOBAL_CONVENTIONS.md`, we ship code that builds clean against the
//! correct target rather than untested half-versions; the launch path is
//! exercised end-to-end the first time a real machine or QEMU+OVMF is
//! attached. The only piece marked `Unimplemented` at runtime is the
//! actual `vmlaunch` instruction wrapper; everything that prepares it is
//! complete.

pub mod cpu;
pub mod fields;
pub mod launch;
pub mod region;
pub mod vmcs;

#[cfg(not(test))]
pub mod exit;
#[cfg(not(test))]
pub mod host_state;
