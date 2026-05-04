//! Host state capture for VM-entry.
//!
//! On VM-exit the CPU reloads host state from VMCS host-state fields (SDM
//! Vol 3 §27). We must therefore *snapshot the running kernel* into those
//! fields before `vmlaunch`, otherwise the CPU will resume into garbage
//! after the very first exit.
//!
//! What we capture:
//!
//! | Resource       | Source                              |
//! |----------------|-------------------------------------|
//! | CR0/CR3/CR4    | `mov rax, crN`                      |
//! | CS/DS/ES/FS/GS/SS selectors | `mov ax, sreg`         |
//! | TR selector + base | `str` + GDT walk for base       |
//! | GDTR/IDTR base | `sgdt` / `sidt`                     |
//! | FS_BASE/GS_BASE | `rdmsr 0xC000_0100 / 0xC000_0101`  |
//!
//! Selector RPL/TI bits are masked off when written to host fields per
//! SDM §26.2.3 (host selectors must have TI=0 and RPL=0).

#![cfg(not(test))]

use crate::arch::x86::msr;
use crate::error::HyperResult;
use crate::vmx::fields as f;
use crate::vmx::vmcs;

/// One capture of the running CPU's host state. Owning it as a struct lets
/// `vm.rs` build it once and pass a borrow to `write_host_state`.
///
/// Field names mirror their architectural counterparts; they are
/// deliberately left without per-field rustdoc — the struct as a whole
/// is the documented surface.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy)]
pub struct HostState {
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cs:  u16,
    pub ds:  u16,
    pub es:  u16,
    pub fs:  u16,
    pub gs:  u16,
    pub ss:  u16,
    pub tr:  u16,
    pub fs_base:   u64,
    pub gs_base:   u64,
    pub tr_base:   u64,
    pub gdtr_base: u64,
    pub idtr_base: u64,
}

/// 10-byte x86_64 descriptor-table pointer (`sgdt` / `sidt` operand).
#[repr(C, packed)]
struct DescriptorTablePtr {
    limit: u16,
    base:  u64,
}

/// Capture the running kernel's host state.
///
/// # Safety
/// Must be called while the kernel is in long mode at CPL 0 with a valid
/// GDT/IDT loaded — i.e. exactly the state CelLoader hands off to us.
pub unsafe fn capture() -> HostState {
    // CR0 / CR3 / CR4
    let cr0: u64;
    let cr3: u64;
    let cr4: u64;
    // SAFETY: privileged but architecturally-defined reads at CPL 0.
    unsafe {
        core::arch::asm!("mov {x}, cr0", x = out(reg) cr0, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {x}, cr3", x = out(reg) cr3, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {x}, cr4", x = out(reg) cr4, options(nomem, nostack, preserves_flags));
    }

    // Segment selectors.
    let cs: u16;
    let ds: u16;
    let es: u16;
    let fs: u16;
    let gs: u16;
    let ss: u16;
    let tr: u16;
    // SAFETY: `mov %sreg, %ax`-style reads are always defined.
    unsafe {
        core::arch::asm!("mov {x:x}, cs", x = out(reg) cs, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {x:x}, ds", x = out(reg) ds, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {x:x}, es", x = out(reg) es, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {x:x}, fs", x = out(reg) fs, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {x:x}, gs", x = out(reg) gs, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {x:x}, ss", x = out(reg) ss, options(nomem, nostack, preserves_flags));
        core::arch::asm!("str {x:x}",     x = out(reg) tr, options(nomem, nostack, preserves_flags));
    }

    // GDTR + IDTR via 10-byte descriptor-table pointers on the stack.
    let mut gdtr = DescriptorTablePtr { limit: 0, base: 0 };
    let mut idtr = DescriptorTablePtr { limit: 0, base: 0 };
    // SAFETY: `sgdt` / `sidt` write 10 bytes through the supplied pointer.
    // The struct above is `repr(C, packed)` and 10 bytes wide.
    unsafe {
        core::arch::asm!(
            "sgdt [{p}]",
            p = in(reg) &mut gdtr,
            options(nostack, preserves_flags),
        );
        core::arch::asm!(
            "sidt [{p}]",
            p = in(reg) &mut idtr,
            options(nostack, preserves_flags),
        );
    }
    let gdtr_base = gdtr.base;
    let idtr_base = idtr.base;

    // FS / GS bases — only available via MSR in long mode.
    // SAFETY: both MSRs are architecturally defined on every long-mode CPU.
    let fs_base = unsafe { msr_rdmsr(msr::IA32_FS_BASE) };
    let gs_base = unsafe { msr_rdmsr(msr::IA32_GS_BASE) };

    // TR base: walk the GDT entry pointed to by TR's selector (high-13 bits).
    // The TSS descriptor in long mode is 16 bytes — base spread across
    // bytes 2..4, 4, 7, and 8..12.
    let tr_base = unsafe { tr_base_from_gdt(gdtr_base, tr) };

    HostState {
        cr0, cr3, cr4,
        cs, ds, es, fs, gs, ss, tr,
        fs_base, gs_base, tr_base,
        gdtr_base, idtr_base,
    }
}

/// Populate the VMCS host-state area from a captured `HostState`.
///
/// Caller must have already loaded the target VMCS via `vmptrld`.
pub fn write_host_state(host: &HostState, host_rip: u64, host_rsp: u64) -> HyperResult<()> {
    // Selector fields require TI=0, RPL=0 (SDM §26.2.3).
    const SEL_MASK: u16 = 0xFFF8;

    crate::logger::log_kv("host_cs", u64::from(host.cs));
    crate::logger::log_kv("host_ss", u64::from(host.ss));
    crate::logger::log_kv("host_ds", u64::from(host.ds));
    crate::logger::log_kv("host_tr", u64::from(host.tr));
    crate::logger::log_kv("host_cr0", host.cr0);
    crate::logger::log_kv("host_cr4", host.cr4);
    crate::logger::log_kv("host_rip", host_rip);
    crate::logger::log_kv("host_rsp", host_rsp);

    vmcs::vmwrite(f::HOST_CR0, host.cr0)?;
    vmcs::vmwrite(f::HOST_CR3, host.cr3)?;
    vmcs::vmwrite(f::HOST_CR4, host.cr4)?;

    vmcs::vmwrite(f::HOST_CS_SELECTOR, u64::from(host.cs & SEL_MASK))?;
    vmcs::vmwrite(f::HOST_DS_SELECTOR, u64::from(host.ds & SEL_MASK))?;
    vmcs::vmwrite(f::HOST_ES_SELECTOR, u64::from(host.es & SEL_MASK))?;
    vmcs::vmwrite(f::HOST_FS_SELECTOR, u64::from(host.fs & SEL_MASK))?;
    vmcs::vmwrite(f::HOST_GS_SELECTOR, u64::from(host.gs & SEL_MASK))?;
    vmcs::vmwrite(f::HOST_SS_SELECTOR, u64::from(host.ss & SEL_MASK))?;
    vmcs::vmwrite(f::HOST_TR_SELECTOR, u64::from(host.tr & SEL_MASK))?;

    vmcs::vmwrite(f::HOST_FS_BASE,   host.fs_base)?;
    vmcs::vmwrite(f::HOST_GS_BASE,   host.gs_base)?;
    vmcs::vmwrite(f::HOST_TR_BASE,   host.tr_base)?;
    vmcs::vmwrite(f::HOST_GDTR_BASE, host.gdtr_base)?;
    vmcs::vmwrite(f::HOST_IDTR_BASE, host.idtr_base)?;

    vmcs::vmwrite(f::HOST_RIP, host_rip)?;
    vmcs::vmwrite(f::HOST_RSP, host_rsp)?;
    Ok(())
}

/// Inline copy of `arch::x86::rdmsr` to avoid a public-API churn just for
/// this module. Same semantics; same safety contract.
///
/// # Safety
/// Caller asserts `addr` is a readable MSR on this CPU.
#[inline]
unsafe fn msr_rdmsr(addr: u32) -> u64 {
    let (high, low): (u32, u32);
    // SAFETY: see arch::x86::rdmsr.
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

/// Decode a 64-bit TSS descriptor from the GDT and return its base.
///
/// The selector's index (bits 15:3) chooses a 16-byte descriptor at
/// `gdt_base + index * 8`. In long mode the TSS descriptor occupies two
/// 8-byte slots; the base is reconstructed from four pieces:
///
/// * bytes 2..4 → base[15:0]
/// * byte  4    → base[23:16]
/// * byte  7    → base[31:24]
/// * bytes 8..12 → base[63:32]
///
/// # Safety
/// Caller must guarantee `gdt_base + index * 8 + 16` is a readable
/// kernel-mapped address (it is — the GDT is part of the loader/kernel).
#[inline]
unsafe fn tr_base_from_gdt(gdt_base: u64, tr_sel: u16) -> u64 {
    let idx = (tr_sel >> 3) as u64;
    if idx == 0 {
        // No TSS installed; report 0 and let SDM checks reject if the
        // host needs one. The kernel's GDT always installs a TSS in
        // production paths, so this branch is defensive only.
        return 0;
    }
    let desc = (gdt_base + idx * 8) as *const u8;
    // SAFETY: caller's contract.
    let bytes: [u8; 16] = unsafe { core::ptr::read_unaligned(desc.cast::<[u8; 16]>()) };
    let lo16 = u64::from(u16::from_le_bytes([bytes[2], bytes[3]]));
    let mid8 = u64::from(bytes[4]);
    let hi8  = u64::from(bytes[7]);
    let hi32 = u64::from(u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]));
    lo16 | (mid8 << 16) | (hi8 << 24) | (hi32 << 32)
}
