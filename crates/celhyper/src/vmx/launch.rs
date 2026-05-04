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
// Fixed-bit MSRs for CR0/CR4 (SDM §A.7, §A.8). Same allowed-0/allowed-1
// semantics: low half = must-be-1, high half = may-be-1.
const IA32_VMX_CR0_FIXED0:          u32 = 0x486;
const IA32_VMX_CR0_FIXED1:          u32 = 0x487;
const IA32_VMX_CR4_FIXED0:          u32 = 0x488;
const IA32_VMX_CR4_FIXED1:          u32 = 0x489;

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

/// Apply CR fixed-bit constraints. Same allowed-0/allowed-1 layout as
/// `rebase_ctl`, but on a 64-bit value (the high 32 bits of CR0/CR4 are
/// reserved-must-be-zero on current parts but the masks are 64-bit so we
/// leave them alone).
fn rebase_cr(desired: u64, fixed0: u32, fixed1: u32) -> u64 {
    // SAFETY: architectural MSRs available whenever VMX is on.
    let f0 = unsafe { crate::arch::cpu::rdmsr(fixed0) };
    let f1 = unsafe { crate::arch::cpu::rdmsr(fixed1) };
    (desired | f0) & f1
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
    // Real-mode guest — drop IA32E_MODE_GUEST. The blob's encoding is
    // mode-agnostic, so 16-bit real mode runs the same bytes as long mode.
    let entry_ctl = rebase_ctl(0,                                                   entry_msr);

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

    // ---- Guest state (16-bit real mode under unrestricted-guest) ---------
    // SDM §27.3.1.2: with SECONDARY_UNRESTRICTED_GUEST=1, CR0.PE/PG may be
    // 0; the other CR0/CR4 fixed bits still apply via IA32_VMX_CRn_FIXEDx
    // *but* PE and PG are explicitly removed from FIXED0 in this mode.
    // We rebase against the MSRs and then forcibly clear PE+PG.
    let guest_cr0 = rebase_cr(0x30, IA32_VMX_CR0_FIXED0, IA32_VMX_CR0_FIXED1)
        & !(1u64 << 0)   // PE
        & !(1u64 << 31); // PG
    let guest_cr4 = rebase_cr(0,    IA32_VMX_CR4_FIXED0, IA32_VMX_CR4_FIXED1);
    crate::logger::log_kv("guest_cr0", guest_cr0);
    crate::logger::log_kv("guest_cr4", guest_cr4);

    vmcs::vmwrite(f::GUEST_CR0, guest_cr0)?;
    vmcs::vmwrite(f::GUEST_CR3, 0)?;
    vmcs::vmwrite(f::GUEST_CR4, guest_cr4)?;

    // Real-mode segment programming. Selectors are the segment*16 form
    // (CS=0x0000 → base 0); base/limit/AR are the cached values.
    // AR for code: 0x9B (P=1, S=1, type=B accessed/readable code).
    // AR for data: 0x93 (P=1, S=1, type=3 accessed/writable data).
    const CODE_AR: u64 = 0x9B;
    const DATA_AR: u64 = 0x93;
    // LDTR: type=2 (LDT), unusable bit (16) set → marked unusable.
    const LDTR_AR: u64 = 0x1_0082;
    // TR: type=B (busy 16-bit TSS) is the only legal value at entry per
    // SDM §26.3.1.2 when not in long mode.
    const TR_AR:   u64 = 0x008B;

    // ES/CS/SS/DS/FS/GS — selector 0, base 0, limit 0xFFFF.
    let segs: &[(u32, u32, u32, u32, u64)] = &[
        (f::GUEST_ES_SELECTOR, f::GUEST_ES_BASE, f::GUEST_ES_LIMIT, f::GUEST_ES_AR, DATA_AR),
        (f::GUEST_CS_SELECTOR, f::GUEST_CS_BASE, f::GUEST_CS_LIMIT, f::GUEST_CS_AR, CODE_AR),
        (f::GUEST_SS_SELECTOR, f::GUEST_SS_BASE, f::GUEST_SS_LIMIT, f::GUEST_SS_AR, DATA_AR),
        (f::GUEST_DS_SELECTOR, f::GUEST_DS_BASE, f::GUEST_DS_LIMIT, f::GUEST_DS_AR, DATA_AR),
        (f::GUEST_FS_SELECTOR, f::GUEST_FS_BASE, f::GUEST_FS_LIMIT, f::GUEST_FS_AR, DATA_AR),
        (f::GUEST_GS_SELECTOR, f::GUEST_GS_BASE, f::GUEST_GS_LIMIT, f::GUEST_GS_AR, DATA_AR),
    ];
    for &(sel, base, limit, ar_enc, ar_val) in segs {
        vmcs::vmwrite(sel,    0)?;
        vmcs::vmwrite(base,   0)?;
        vmcs::vmwrite(limit,  0xFFFF)?;
        vmcs::vmwrite(ar_enc, ar_val)?;
    }

    // LDTR — unusable.
    vmcs::vmwrite(f::GUEST_LDTR_SELECTOR, 0)?;
    vmcs::vmwrite(f::GUEST_LDTR_BASE,     0)?;
    vmcs::vmwrite(f::GUEST_LDTR_LIMIT,    0xFFFF)?;
    vmcs::vmwrite(f::GUEST_LDTR_AR,       LDTR_AR)?;

    // TR — must be present (P=1) per SDM §26.3.1.2; busy-16-TSS form.
    vmcs::vmwrite(f::GUEST_TR_SELECTOR,   0)?;
    vmcs::vmwrite(f::GUEST_TR_BASE,       0)?;
    vmcs::vmwrite(f::GUEST_TR_LIMIT,      0xFFFF)?;
    vmcs::vmwrite(f::GUEST_TR_AR,         TR_AR)?;

    // GDTR/IDTR — empty but valid.
    vmcs::vmwrite(f::GUEST_GDTR_BASE,  0)?;
    vmcs::vmwrite(f::GUEST_GDTR_LIMIT, 0xFFFF)?;
    vmcs::vmwrite(f::GUEST_IDTR_BASE,  0)?;
    vmcs::vmwrite(f::GUEST_IDTR_LIMIT, 0xFFFF)?;

    // RIP/RSP/RFLAGS. Real-mode RIP is just the offset; CS.base=0 means
    // linear address == offset == GPA via the EPT.
    vmcs::vmwrite(f::GUEST_RIP,    plan.guest_rip)?;
    vmcs::vmwrite(f::GUEST_RSP,    plan.guest_rsp)?;
    vmcs::vmwrite(f::GUEST_RFLAGS, 0x2)?; // reserved bit-1 always set

    // Misc guest state required by entry checks.
    vmcs::vmwrite(f::VMCS_LINK_POINTER,    !0u64)?; // "no shadow VMCS"
    vmcs::vmwrite(f::GUEST_IA32_DEBUGCTL,  0)?;
    vmcs::vmwrite(f::GUEST_DR7,            0x400)?; // architectural reset value
    vmcs::vmwrite(f::GUEST_SYSENTER_CS,    0)?;
    vmcs::vmwrite(f::GUEST_SYSENTER_ESP,   0)?;
    vmcs::vmwrite(f::GUEST_SYSENTER_EIP,   0)?;
    vmcs::vmwrite(f::GUEST_INTERRUPTIBILITY, 0)?;
    vmcs::vmwrite(f::GUEST_ACTIVITY_STATE,   0)?; // Active
    vmcs::vmwrite(f::GUEST_PENDING_DBG_EXC,  0)?;

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
