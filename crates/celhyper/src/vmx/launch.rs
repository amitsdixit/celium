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

// IA32_VMX_* capability MSRs used to rebase control-field reserved bits.
// SDM Vol 3 §A.3, §A.4. For each MSR: low 32 bits = "allowed-0" (must be 1),
// high 32 bits = "allowed-1" (may be 1).
const IA32_VMX_BASIC:               u32 = 0x480;
const IA32_VMX_PINBASED_CTLS:       u32 = 0x481;
const IA32_VMX_PROCBASED_CTLS:      u32 = 0x482;
const IA32_VMX_EXIT_CTLS:           u32 = 0x483;
const IA32_VMX_ENTRY_CTLS:          u32 = 0x484;
const IA32_VMX_PROCBASED_CTLS2:     u32 = 0x48B;
const IA32_VMX_TRUE_PINBASED_CTLS:  u32 = 0x48D;
const IA32_VMX_TRUE_PROCBASED_CTLS: u32 = 0x48E;
const IA32_VMX_TRUE_EXIT_CTLS:      u32 = 0x48F;
const IA32_VMX_TRUE_ENTRY_CTLS:     u32 = 0x490;

/// Apply allowed-0/allowed-1 constraints from `msr` to `desired`.
///
/// SDM §A.3.1: bits set in the low 32 bits MUST be 1 in the VMCS field;
/// bits clear in the high 32 bits MUST be 0. The result is `(desired |
/// allowed_0) & allowed_1`. Bits required to be 1 always win, even if
/// the caller passed them as 0.
fn rebase_ctl(desired: u32, msr_addr: u32) -> u32 {
    // SAFETY: each MSR address above is architecturally readable when
    // CPUID.1:ECX[5] (VMX) is set; manager::init_runtime gates on that.
    let v = unsafe { crate::arch::cpu::rdmsr(msr_addr) };
    let allowed_0 = v as u32;          // must-be-1 mask
    let allowed_1 = (v >> 32) as u32;  // may-be-1 mask
    (desired | allowed_0) & allowed_1
}

/// Pick `IA32_VMX_TRUE_*_CTLS` if the CPU advertises them via
/// `IA32_VMX_BASIC[55]`, otherwise fall back to the legacy MSR.
fn ctl_msr(true_msr: u32, legacy_msr: u32) -> u32 {
    // SAFETY: same as `rebase_ctl`.
    let basic = unsafe { crate::arch::cpu::rdmsr(IA32_VMX_BASIC) };
    if basic & (1u64 << 55) != 0 { true_msr } else { legacy_msr }
}

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
    // Each control field's must-be-1 / may-be-1 reserved bits are encoded
    // in IA32_VMX_*_CTLS (or _TRUE_* on modern CPUs that advertise
    // IA32_VMX_BASIC[55]). Writing a raw value without rebasing through
    // those masks fails entry check #7 ("invalid control field(s)").
    let pin_msr   = ctl_msr(IA32_VMX_TRUE_PINBASED_CTLS,  IA32_VMX_PINBASED_CTLS);
    let proc_msr  = ctl_msr(IA32_VMX_TRUE_PROCBASED_CTLS, IA32_VMX_PROCBASED_CTLS);
    let exit_msr  = ctl_msr(IA32_VMX_TRUE_EXIT_CTLS,      IA32_VMX_EXIT_CTLS);
    let entry_msr = ctl_msr(IA32_VMX_TRUE_ENTRY_CTLS,     IA32_VMX_ENTRY_CTLS);

    let pin_ctl   = rebase_ctl(0,                                                   pin_msr);
    let proc_ctl  = rebase_ctl(f::CPUBASED_HLT_EXITING | f::CPUBASED_ACTIVATE_SECONDARY, proc_msr);
    let sec_ctl   = rebase_ctl(f::SECONDARY_ENABLE_EPT | f::SECONDARY_UNRESTRICTED_GUEST,
                               IA32_VMX_PROCBASED_CTLS2);
    let exit_ctl  = rebase_ctl(f::VMEXIT_HOST_ADDR_SPACE_SIZE,                      exit_msr);
    let entry_ctl = rebase_ctl(f::VMENTRY_IA32E_MODE_GUEST,                         entry_msr);

    crate::logger::log_kv("pin_ctl",   u64::from(pin_ctl));
    crate::logger::log_kv("proc_ctl",  u64::from(proc_ctl));
    crate::logger::log_kv("sec_ctl",   u64::from(sec_ctl));
    crate::logger::log_kv("exit_ctl",  u64::from(exit_ctl));
    crate::logger::log_kv("entry_ctl", u64::from(entry_ctl));

    vmcs::vmwrite(f::PIN_BASED_VM_EXEC_CTL, u64::from(pin_ctl))?;
    vmcs::vmwrite(f::CPU_BASED_VM_EXEC_CTL, u64::from(proc_ctl))?;
    vmcs::vmwrite(f::SECONDARY_VM_EXEC_CTL, u64::from(sec_ctl))?;
    vmcs::vmwrite(f::EXCEPTION_BITMAP, 0)?;
    vmcs::vmwrite(f::VM_EXIT_CTLS,  u64::from(exit_ctl))?;
    vmcs::vmwrite(f::VM_ENTRY_CTLS, u64::from(entry_ctl))?;
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
        crate::logger::log("vmlaunch: VMfailInvalid (no current VMCS)");
        return Err(HyperError::Hardware("vmlaunch: VMfailInvalid"));
    }
    if rflags & (1 << 6) != 0 {
        if let Ok(code) = vmcs::vmread(f::VM_INSTRUCTION_ERROR) {
            crate::logger::log_kv("vmlaunch_instruction_error", code);
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
