//! The handoff block CelLoader produces and CelHyper consumes.
//!
//! The exact same `#[repr(C)]` layout is mirrored in
//! `crates/celhyper/src/handoff.rs`. **Both copies must be kept in lock-step.**
//! When this struct grows, bump [`VERSION`].

use crate::hardware::CpuFacts;

/// `b"CELIUM\0\0"` — first 8 bytes CelHyper checks before trusting the block.
pub const MAGIC: u64 = u64::from_le_bytes(*b"CELIUM\0\0");

/// Layout version. CelHyper aborts on mismatch.
///
/// * v1 (W17–W22): magic + version + cpu + acpi + kernel_image.
/// * v2 (W23-D): adds the optional host-staged boot image triple
///   (`boot_image_phys` / `boot_image_len` / `boot_image_crc32c`).
///   The fields are zero when no image is staged; the kernel then
///   falls back to its built-in `HELLO_BLOB` so today's bring-up
///   keeps working unchanged.
pub const VERSION: u32 = 2;

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

    /// W23-D: physical address of a host-staged guest boot image, or
    /// `0` if no image is staged. CelLoader passes `0` today; image
    /// staging across `ExitBootServices` lands in W23-E.
    pub boot_image_phys: u64,
    /// Length of `boot_image_phys` in bytes. Must be `0` iff
    /// `boot_image_phys == 0`.
    pub boot_image_len:  u64,
    /// CRC32C (Castagnoli) of the staged boot image, or `0` if none.
    /// CelHyper validates this before mapping the image into EPT.
    pub boot_image_crc32c: u32,
    /// Reserved, must be zero. Padding so the struct stays aligned
    /// without forcing every consumer to import `repr(packed)`.
    pub _pad2: u32,
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
            // W23-D: no host-staged image yet; CelHyper will fall
            // back to its built-in HELLO_BLOB.
            boot_image_phys: 0,
            boot_image_len: 0,
            boot_image_crc32c: 0,
            _pad2: 0,
        }
    }
}
