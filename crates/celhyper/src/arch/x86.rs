//! x86_64 helpers. Re-exports just enough of the `x86_64` crate to keep the
//! rest of CelHyper portable should we ever grow an aarch64 backend.

use crate::error::{HyperError, HyperResult};

/// MSR addresses we care about during VMX bring-up.
pub mod msr {
    /// `IA32_FEATURE_CONTROL` — VMX enable + lock bits.
    pub const IA32_FEATURE_CONTROL: u32 = 0x3A;
    /// `IA32_VMX_BASIC` — revision id + capability hints.
    pub const IA32_VMX_BASIC:       u32 = 0x480;
    /// `IA32_EFER` — long mode / NXE.
    pub const IA32_EFER:            u32 = 0xC000_0080;
    /// `IA32_FS_BASE` — 64-bit FS segment base (long mode).
    pub const IA32_FS_BASE:         u32 = 0xC000_0100;
    /// `IA32_GS_BASE` — 64-bit GS segment base (long mode).
    pub const IA32_GS_BASE:         u32 = 0xC000_0101;
}

/// Read a model-specific register. Wraps `rdmsr` so callers in safe Rust
/// don't have to spell out `unsafe` every time — *but the `unsafe` block
/// here is what gates the audit*: we promise `addr` refers to a readable MSR.
///
/// # Safety
/// Caller must guarantee `addr` is a valid, readable MSR on this CPU.
#[inline]
pub unsafe fn rdmsr(addr: u32) -> u64 {
    let (high, low): (u32, u32);
    // SAFETY: `rdmsr` is a privileged but well-defined instruction; the
    // caller's contract covers the address.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") addr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Returns `Ok(())` if the running CPU has VMX enabled and unlocked
/// (or already enabled and locked). Returns [`HyperError::UnsupportedCpu`]
/// otherwise.
pub fn ensure_vmx_available() -> HyperResult<()> {
    // SAFETY: IA32_FEATURE_CONTROL is architecturally defined on every
    // x86_64 CPU we support.
    let fc = unsafe { rdmsr(msr::IA32_FEATURE_CONTROL) };
    let lock        = fc & 0x1   != 0;
    let vmx_outside = fc & 0x4   != 0;
    if lock && !vmx_outside {
        return Err(HyperError::UnsupportedCpu("VMX disabled by firmware (locked)"));
    }
    Ok(())
}
