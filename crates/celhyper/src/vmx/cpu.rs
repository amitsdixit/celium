//! Per-CPU VMX bring-up: CR0/CR4 fixed bits, CR4.VMXE, `vmxon`.
//!
//! This is the first place we leave "safe Rust talking to memory" and
//! actually issue VMX instructions. Every unsafe block below carries a
//! SAFETY note that names the precondition the caller must establish; the
//! single caller is [`crate::vm::bring_up`], which itself runs only after
//! handoff validation and CPU-feature checks.

use crate::arch::cpu;
use crate::error::{HyperError, HyperResult};
use crate::mm::PhysAddr;

/// `IA32_VMX_CR0_FIXED0` / `_FIXED1`: bits that must be 1 / may be 1 in CR0.
pub const IA32_VMX_CR0_FIXED0: u32 = 0x486;
/// See [`IA32_VMX_CR0_FIXED0`].
pub const IA32_VMX_CR0_FIXED1: u32 = 0x487;
/// `IA32_VMX_CR4_FIXED0` / `_FIXED1`: bits that must be 1 / may be 1 in CR4.
pub const IA32_VMX_CR4_FIXED0: u32 = 0x488;
/// See [`IA32_VMX_CR4_FIXED0`].
pub const IA32_VMX_CR4_FIXED1: u32 = 0x489;

/// CR4.VMXE.
pub const CR4_VMXE: u64 = 1 << 13;

/// Returns the 32-bit VMX revision ID from `IA32_VMX_BASIC[30:0]`. Required
/// to stamp the VMXON region and every VMCS.
///
/// # Errors
/// [`HyperError::UnsupportedCpu`] when bit 31 of `IA32_VMX_BASIC` is set,
/// indicating an out-of-spec revision id (would-be 32-bit width).
pub fn revision_id() -> HyperResult<u32> {
    // SAFETY: IA32_VMX_BASIC is architecturally defined when CPUID.1:ECX[5]
    // (VMX) is set; the caller has already gated on that.
    let basic = unsafe { cpu::rdmsr(crate::arch::cpu::msr::IA32_VMX_BASIC) };
    if basic & (1 << 31) != 0 {
        return Err(HyperError::UnsupportedCpu("IA32_VMX_BASIC width bit set"));
    }
    Ok((basic & 0x7FFF_FFFF) as u32)
}

/// Apply the CR0/CR4 fixed-bits constraints, set CR4.VMXE, and issue
/// `vmxon` against `vmxon_phys`.
///
/// On success the CPU is in VMX root operation; subsequent `vmclear`,
/// `vmptrld`, `vmlaunch`, etc. are legal.
pub fn enable(vmxon_phys: PhysAddr) -> HyperResult<()> {
    // 1. CR0 fixed bits.
    let cr0_fixed0 = unsafe { cpu::rdmsr(IA32_VMX_CR0_FIXED0) };
    let cr0_fixed1 = unsafe { cpu::rdmsr(IA32_VMX_CR0_FIXED1) };
    // SAFETY: CR0 read/write is privileged; we are at CPL 0 in long mode.
    let mut cr0 = unsafe { read_cr0() };
    cr0 |= cr0_fixed0;
    cr0 &= cr0_fixed1;
    unsafe { write_cr0(cr0) };

    // 2. CR4: set VMXE, then constrain by fixed bits.
    let cr4_fixed0 = unsafe { cpu::rdmsr(IA32_VMX_CR4_FIXED0) };
    let cr4_fixed1 = unsafe { cpu::rdmsr(IA32_VMX_CR4_FIXED1) };
    // SAFETY: same as CR0.
    let mut cr4 = unsafe { read_cr4() };
    cr4 |= CR4_VMXE | cr4_fixed0;
    cr4 &= cr4_fixed1;
    unsafe { write_cr4(cr4) };

    // 3. vmxon.
    let phys = vmxon_phys.as_u64();
    let rflags: u64;
    // SAFETY: `vmxon` requires CR4.VMXE=1 (just set), CR0 within the fixed
    // window (just enforced), and a 4 KiB-aligned, properly-stamped VMXON
    // region (`region::alloc_in_pool` guarantees this). The instruction
    // updates RFLAGS and does not access user memory.
    unsafe {
        core::arch::asm!(
            "vmxon [{ptr}]",
            "pushfq",
            "pop {rflags}",
            ptr = in(reg) &phys,
            rflags = out(reg) rflags,
            options(nostack),
        );
    }
    if rflags & (1 << 0) != 0 {
        return Err(HyperError::Hardware("vmxon: VMfailInvalid (CF=1)"));
    }
    if rflags & (1 << 6) != 0 {
        return Err(HyperError::Hardware("vmxon: VMfailValid (ZF=1)"));
    }
    Ok(())
}

// ---- Tiny CR0/CR4 wrappers (CPL-0, long mode) -----------------------------

#[inline]
unsafe fn read_cr0() -> u64 {
    let v: u64;
    // SAFETY: privileged read; caller must be at CPL 0.
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}
#[inline]
unsafe fn write_cr0(v: u64) {
    // SAFETY: privileged write; caller must be at CPL 0 and supply a value
    // satisfying the architecturally-required constraints (PE, NE, ...).
    unsafe { core::arch::asm!("mov cr0, {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
}
#[inline]
unsafe fn read_cr4() -> u64 {
    let v: u64;
    // SAFETY: see read_cr0.
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}
#[inline]
unsafe fn write_cr4(v: u64) {
    // SAFETY: see write_cr0.
    unsafe { core::arch::asm!("mov cr4, {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
}
