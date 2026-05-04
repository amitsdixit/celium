//! The handoff block CelLoader produces and CelHyper consumes.
//!
//! The exact same `#[repr(C)]` layout is mirrored in
//! `crates/celhyper/src/handoff.rs`. **Both copies must be kept in lock-step.**
//! When this struct grows, bump [`VERSION`].

use crate::hardware::CpuFacts;

/// `b"CELIUM\0\0"` — first 8 bytes CelHyper checks before trusting the block.
pub const MAGIC: u64 = u64::from_le_bytes(*b"CELIUM\0\0");

/// Layout version. CelHyper aborts on mismatch.
pub const VERSION: u32 = 1;

/// Information CelLoader hands to CelHyper at the moment of jump.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CeliumHandoff {
    /// Must equal [`MAGIC`].
    pub magic: u64,
    /// Must equal [`VERSION`].
    pub version: u32,
    /// Reserved, must be zero.
    pub _pad: u32,

    /// CPU facts collected by stage-0.
    pub cpu: CpuFacts,

    /// Physical address of the ACPI 2.0 RSDP, or 0 if not found.
    pub acpi_rsdp_phys: u64,

    /// Physical address of the in-memory CelHyper ELF image (still inside
    /// the UEFI allocation when this is built; CelHyper relocates it).
    pub kernel_image_phys: u64,
    /// Length of `kernel_image_phys` in bytes.
    pub kernel_image_len:  u64,
}

impl CeliumHandoff {
    /// Build a fresh handoff block. The remaining fields (memory map,
    /// framebuffer) are added in the Week-2 patch when we exit boot services.
    #[must_use]
    pub fn new(
        cpu: CpuFacts,
        acpi_rsdp_phys: u64,
        kernel_image_phys: u64,
        kernel_image_len: u64,
    ) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            _pad: 0,
            cpu,
            acpi_rsdp_phys,
            kernel_image_phys,
            kernel_image_len,
        }
    }
}
