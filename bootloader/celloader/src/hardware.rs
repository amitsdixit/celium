//! Hardware discovery used by stage-0.
//!
//! This is intentionally tiny: just enough to (a) refuse to load on
//! virtualization-incapable CPUs and (b) hand a few well-typed facts to
//! CelHyper. Anything richer belongs in the hypervisor itself.

use core::arch::x86_64::__cpuid;

/// Subset of CPUID facts CelHyper needs at boot time.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct CpuFacts {
    /// 12-byte vendor string (e.g. `"GenuineIntel"`, `"AuthenticAMD"`).
    pub vendor: [u8; 12],
    /// Intel VT-x present (`CPUID.1:ECX[5]`).
    pub vmx:    bool,
    /// AMD-V present (`CPUID.80000001:ECX[2]`).
    pub svm:    bool,
    /// x2APIC present (`CPUID.1:ECX[21]`).
    pub x2apic: bool,
    /// 1 GiB pages supported (`CPUID.80000001:EDX[26]`).
    pub gib_pages: bool,
}

impl CpuFacts {
    /// Returns the vendor string as a `&str` (UTF-8, no NUL).
    #[must_use]
    pub fn vendor_str(&self) -> &str {
        core::str::from_utf8(&self.vendor).unwrap_or("?")
    }

    /// True iff at least one of VMX/SVM is available.
    #[must_use]
    pub fn has_virtualization(&self) -> bool {
        self.vmx || self.svm
    }
}

/// Sentinel returned when discovery fails. Stage-0 treats this as fatal.
#[derive(Debug)]
pub struct ProbeError;

/// Read CPUID leaves 0/1/0x8000_0001 and assemble a [`CpuFacts`].
pub fn probe_cpu() -> Result<CpuFacts, ProbeError> {
    // NOTE: in current Rust `__cpuid` is callable from safe code on
    // x86_64 because the instruction is unconditionally available in
    // long mode (which UEFI guarantees on our target). No `unsafe`
    // block is required.
    let leaf0 = __cpuid(0);
    let leaf1 = __cpuid(1);
    let ext1  = __cpuid(0x8000_0001);

    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&leaf0.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&leaf0.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&leaf0.ecx.to_le_bytes());

    Ok(CpuFacts {
        vendor,
        vmx:       (leaf1.ecx & (1 << 5))  != 0,
        svm:       (ext1.ecx  & (1 << 2))  != 0,
        x2apic:    (leaf1.ecx & (1 << 21)) != 0,
        gib_pages: (ext1.edx  & (1 << 26)) != 0,
    })
}

/// Locate the ACPI 2.0+ RSDP via the UEFI Configuration Table, falling back
/// to ACPI 1.0. Returns the physical address (which equals the virtual
/// address under UEFI's identity mapping).
#[must_use]
pub fn find_acpi_rsdp() -> Option<u64> {
    use uefi::table::cfg;

    let entries = uefi::system::with_config_table(|table| {
        // Prefer 2.0; fall back to 1.0.
        let mut acpi2 = None;
        let mut acpi1 = None;
        for e in table {
            if e.guid == cfg::ACPI2_GUID {
                acpi2 = Some(e.address as u64);
            } else if e.guid == cfg::ACPI_GUID {
                acpi1 = Some(e.address as u64);
            }
        }
        acpi2.or(acpi1)
    });
    entries
}
