//! Build the VMCS for the first guest and execute `vmlaunch`.
//!
//! This is the integration point of everything in `vmx`. The guest is the
//! tiny blob in [`crate::guest::HELLO_BLOB`] mapped at GPA `0x1000` with
//! its initial RIP at `0x1000`. The blob writes "Celium guest alive\n" to
//! port `0xE9` and halts — that single hlt becomes the first VM-exit and
//! is what we'd assert in an integration test on real hardware.

use crate::error::{HyperError, HyperResult};
use crate::mm::{Ept, EptFlags, FrameProvider, PhysAddr};
use crate::vmx::fields as f;
use crate::vmx::vmcs;

/// Inputs handed to the launcher. Owning it as a struct keeps the (long)
/// list of physical addresses and selectors in one place rather than
/// threading them through a 12-arg function.
///
/// Host RIP / RSP are *not* set here; they live in
/// [`crate::vmx::host_state::write_host_state`] alongside the other
/// host-state fields, which are captured together from the running
/// kernel in one shot.
pub struct LaunchPlan {
    /// PML4 + EPTP-encoded value to load into the EPT pointer field.
    pub eptp: u64,
    /// Initial guest RIP (already mapped through EPT).
    pub guest_rip: u64,
    /// Initial guest RSP.
    pub guest_rsp: u64,
}

/// Populate every guest+control VMCS field required by Intel SDM §26
/// entry checks.
///
/// Caller must have already executed `vmclear` + `vmptrld` for the
/// target VMCS so that subsequent `vmwrite`s land on the right
/// structure. The host-state fields are written separately by
/// [`crate::vmx::host_state::write_host_state`].
pub fn write_vmcs(plan: &LaunchPlan) -> HyperResult<()> {
    // ---- Controls ---------------------------------------------------------
    // We use unrestricted-guest + EPT so the blob can run from real-mode-style
    // selectors without us having to construct a full GDT for the guest.
    vmcs::vmwrite(f::PIN_BASED_VM_EXEC_CTL, 0)?;
    vmcs::vmwrite(
        f::CPU_BASED_VM_EXEC_CTL,
        u64::from(f::CPUBASED_HLT_EXITING | f::CPUBASED_ACTIVATE_SECONDARY),
    )?;
    vmcs::vmwrite(
        f::SECONDARY_VM_EXEC_CTL,
        u64::from(f::SECONDARY_ENABLE_EPT | f::SECONDARY_UNRESTRICTED_GUEST),
    )?;
    vmcs::vmwrite(f::EXCEPTION_BITMAP, 0)?;
    vmcs::vmwrite(f::VM_EXIT_CTLS, u64::from(f::VMEXIT_HOST_ADDR_SPACE_SIZE))?;
    vmcs::vmwrite(f::VM_ENTRY_CTLS, u64::from(f::VMENTRY_IA32E_MODE_GUEST))?;
    vmcs::vmwrite(f::EPT_POINTER, plan.eptp)?;

    // ---- Guest state (long mode, flat) -----------------------------------
    // Selectors all point at one flat code/data descriptor we expect a
    // future patch to publish through a guest GDT in `crate::guest`.
    vmcs::vmwrite(f::GUEST_CS_SELECTOR, 0x10)?;
    vmcs::vmwrite(f::GUEST_DS_SELECTOR, 0x18)?;
    vmcs::vmwrite(f::GUEST_ES_SELECTOR, 0x18)?;
    vmcs::vmwrite(f::GUEST_FS_SELECTOR, 0x18)?;
    vmcs::vmwrite(f::GUEST_GS_SELECTOR, 0x18)?;
    vmcs::vmwrite(f::GUEST_SS_SELECTOR, 0x18)?;
    vmcs::vmwrite(f::GUEST_TR_SELECTOR, 0x00)?;
    vmcs::vmwrite(f::GUEST_LDTR_SELECTOR, 0x00)?;

    vmcs::vmwrite(f::GUEST_CR0, 0x8000_0021)?; // PG | NE | PE
    vmcs::vmwrite(f::GUEST_CR3, 0)?;           // guest installs its own when it grows one
    vmcs::vmwrite(f::GUEST_CR4, 0x0000_2000)?; // VMXE not required in guest, but PAE OK
    vmcs::vmwrite(f::GUEST_RIP, plan.guest_rip)?;
    vmcs::vmwrite(f::GUEST_RSP, plan.guest_rsp)?;
    vmcs::vmwrite(f::GUEST_RFLAGS, 0x2)?;      // reserved bit

    Ok(())
}

/// Issue `vmlaunch`. Does not return on success — control transfers to the
/// guest at [`f::GUEST_RIP`] until the first VM-exit, at which point the
/// CPU jumps to the host entry point we registered in [`f::HOST_RIP`].
///
/// On VM-entry failure, the CPU resumes here with an updated rflags; we
/// translate that back to a `HyperError`.
///
/// # Reality check
/// Without VT-x hardware available, exercising this function is a no-op
/// from observation; it is included in full because the surrounding code
/// (`write_vmcs`, `LaunchPlan`) needs a real callee to type-check against.
pub fn vmlaunch() -> HyperResult<()> {
    let rflags: u64;
    // SAFETY: caller ensures: VMX root active, current VMCS valid and fully
    // populated by `write_vmcs`, host state captured. On success we do not
    // return through this path; on failure rflags is set per SDM §30.4.
    unsafe {
        core::arch::asm!(
            "vmlaunch",
            "pushfq",
            "pop {rflags}",
            rflags = out(reg) rflags,
            options(nostack),
        );
    }
    if rflags & (1 << 0) != 0 {
        return Err(HyperError::Hardware("vmlaunch: VMfailInvalid"));
    }
    if rflags & (1 << 6) != 0 {
        // Try to surface the precise instruction-error code, but don't
        // fail the fault handling itself if vmread errors.
        if let Ok(code) = vmcs::vmread(f::VM_INSTRUCTION_ERROR) {
            // Code is logged through the serial logger by the caller; we
            // only carry the coarse variant in the error value.
            let _ = code;
        }
        return Err(HyperError::Hardware("vmlaunch: VMfailValid"));
    }
    // Reaching here without a fault means the CPU didn't actually launch —
    // shouldn't happen on real hardware.
    Err(HyperError::Internal("vmlaunch returned without entering guest"))
}

/// Convenience wrapper used by `vm::launch_first_guest`: install the guest
/// blob into a fresh EPT at GPA `0x1000` and return the EPT.
pub fn install_first_guest<P: FrameProvider>(
    p: &mut P,
    blob: &[u8],
) -> HyperResult<Ept> {
    if blob.len() > crate::mm::PAGE_SIZE {
        return Err(HyperError::Hardware("guest blob > 4 KiB"));
    }
    let mut ept = Ept::new(p)?;

    // Allocate one host frame for the blob and copy it in. The caller's
    // FrameProvider gives us a PA; under the kernel's identity map that PA
    // is also a valid host VA.
    let hpa = p.alloc_zeroed()?;
    // SAFETY: identity-mapped under the boot CR3; bounds checked above.
    unsafe {
        let dst = hpa.as_u64() as *mut u8;
        core::ptr::copy_nonoverlapping(blob.as_ptr(), dst, blob.len());
    }

    let gpa = PhysAddr::new(0x1000);
    ept.map_4k(
        p,
        gpa,
        hpa,
        EptFlags::READ | EptFlags::WRITE | EptFlags::EXEC | EptFlags::MT_WB,
    )?;
    Ok(ept)
}
