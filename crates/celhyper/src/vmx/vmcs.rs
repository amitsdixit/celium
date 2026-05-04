//! VMCS instruction wrappers: `vmclear`, `vmptrld`, `vmread`, `vmwrite`.
//!
//! Each wrapper returns `HyperResult<...>`: a non-zero CF or ZF in `rflags`
//! after the instruction is mapped to [`HyperError::Hardware`] with a stable
//! short string. The actual `vmlaunch` lives in [`super::launch`] because
//! its outcome is a *control-flow* event, not a value.

use crate::error::{HyperError, HyperResult};
use crate::mm::PhysAddr;

/// `VMCLEAR [vmcs_phys]` — flush a VMCS to memory and mark it inactive.
pub fn vmclear(vmcs_phys: PhysAddr) -> HyperResult<()> {
    let phys = vmcs_phys.as_u64();
    let rflags: u64;
    // SAFETY: CR4.VMXE=1 and we are in VMX root operation (caller's contract).
    // `phys` is a stack-resident 8-byte value; instruction reads memory only
    // through the supplied pointer and writes to RFLAGS.
    unsafe {
        core::arch::asm!(
            "vmclear [{ptr}]",
            "pushfq",
            "pop {rflags}",
            ptr = in(reg) &phys,
            rflags = out(reg) rflags,
            options(nostack),
        );
    }
    check_vmx_status(rflags, "vmclear")
}

/// `VMPTRLD [vmcs_phys]` — make this VMCS the current/active one.
pub fn vmptrld(vmcs_phys: PhysAddr) -> HyperResult<()> {
    let phys = vmcs_phys.as_u64();
    let rflags: u64;
    // SAFETY: same as `vmclear`.
    unsafe {
        core::arch::asm!(
            "vmptrld [{ptr}]",
            "pushfq",
            "pop {rflags}",
            ptr = in(reg) &phys,
            rflags = out(reg) rflags,
            options(nostack),
        );
    }
    check_vmx_status(rflags, "vmptrld")
}

/// `VMWRITE field, value`.
pub fn vmwrite(field: u32, value: u64) -> HyperResult<()> {
    let rflags: u64;
    // SAFETY: VMX root, current VMCS loaded; field encoding is from the
    // canonical table in `super::fields`.
    unsafe {
        core::arch::asm!(
            "vmwrite {f}, {v}",
            "pushfq",
            "pop {rflags}",
            f = in(reg) u64::from(field),
            v = in(reg) value,
            rflags = out(reg) rflags,
            options(nomem, nostack),
        );
    }
    check_vmx_status(rflags, "vmwrite")
}

/// `VMREAD field`. Returns the natural-width value; callers must mask if
/// they care about a 32-bit subfield.
pub fn vmread(field: u32) -> HyperResult<u64> {
    let value: u64;
    let rflags: u64;
    // SAFETY: same preconditions as vmwrite.
    unsafe {
        core::arch::asm!(
            "vmread {v}, {f}",
            "pushfq",
            "pop {rflags}",
            f = in(reg) u64::from(field),
            v = out(reg) value,
            rflags = out(reg) rflags,
            options(nomem, nostack),
        );
    }
    check_vmx_status(rflags, "vmread")?;
    Ok(value)
}

/// Decode the post-instruction rflags into a `HyperResult`.
fn check_vmx_status(rflags: u64, what: &'static str) -> HyperResult<()> {
    if rflags & (1 << 0) != 0 {
        // CF=1 → VMfailInvalid (no current VMCS).
        let _ = what;
        return Err(HyperError::Hardware("vmx: VMfailInvalid"));
    }
    if rflags & (1 << 6) != 0 {
        // ZF=1 → VMfailValid; instruction-error code is in
        // VM_INSTRUCTION_ERROR but reading it requires another vmread,
        // which can itself fail. We surface the coarse error and let the
        // caller log via tracing/serial.
        let _ = what;
        return Err(HyperError::Hardware("vmx: VMfailValid"));
    }
    Ok(())
}
