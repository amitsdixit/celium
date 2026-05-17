//! Mirror of `bootloader/celloader/src/handoff.rs`. **Keep in lock-step.**
//!
//! Defining it twice is deliberate: stage-0 must compile against
//! `x86_64-unknown-uefi` and the kernel against `x86_64-unknown-none`. A
//! shared crate would force one to depend on the other's target conventions.
//! When the layout changes, bump [`VERSION`] in both places.

use crate::error::{HyperError, HyperResult};

/// `b"CELIUM\0\0"`.
pub const MAGIC: u64 = u64::from_le_bytes(*b"CELIUM\0\0");

/// Layout version. Mismatch with the loader is fatal.
///
/// * v1 (W17–W22).
/// * v2 (W23-D): adds `boot_image_phys` / `boot_image_len` /
///   `boot_image_crc32c`. Zero in all three means "no image staged";
///   the kernel then loads its built-in `HELLO_BLOB`.
pub const VERSION: u32 = 2;

/// Subset of CPUID facts collected by stage-0. Layout matches
/// `celloader::hardware::CpuFacts` byte-for-byte.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CpuFacts {
    /// 12-byte vendor string.
    pub vendor: [u8; 12],
    /// Intel VT-x present.
    pub vmx:    bool,
    /// AMD-V present.
    pub svm:    bool,
    /// x2APIC present.
    pub x2apic: bool,
    /// 1 GiB pages supported.
    pub gib_pages: bool,
}

/// CelLoader → CelHyper handoff block. See sibling spec file for layout.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CeliumHandoff {
    /// Must equal [`MAGIC`].
    pub magic: u64,
    /// Must equal [`VERSION`].
    pub version: u32,
    /// Reserved, must be zero.
    pub _pad: u32,
    /// CPU facts.
    pub cpu: CpuFacts,
    /// ACPI 2.0 RSDP physical address (or 0).
    pub acpi_rsdp_phys: u64,
    /// In-memory CelHyper ELF image.
    pub kernel_image_phys: u64,
    /// Length of the image in bytes.
    pub kernel_image_len: u64,

    /// W23-D: physical address of a host-staged guest boot image,
    /// or `0` if no image is staged. CelLoader passes `0` today
    /// (image staging is W23-E); the kernel then falls back to its
    /// built-in `HELLO_BLOB` so existing bring-up regression tests
    /// pass unchanged.
    pub boot_image_phys: u64,
    /// Length of `boot_image_phys` in bytes. Must be `0` iff
    /// `boot_image_phys == 0`.
    pub boot_image_len: u64,
    /// CRC32C (Castagnoli) of the staged boot image, or `0` if none.
    pub boot_image_crc32c: u32,
    /// Reserved, must be zero.
    pub _pad2: u32,
}

impl CeliumHandoff {
    /// Validate and copy the handoff block CelLoader pointed us at.
    ///
    /// # Safety
    /// `ptr` must be non-null, properly aligned for `CeliumHandoff`, and
    /// reference an initialised value valid for reads. The caller (the
    /// kernel entry trampoline) is the only place this contract is
    /// established; everywhere else uses the returned owned copy.
    pub unsafe fn from_raw(ptr: *const Self) -> HyperResult<Self> {
        if ptr.is_null() {
            return Err(HyperError::InvalidHandoff("null pointer"));
        }
        if (ptr as usize) % core::mem::align_of::<Self>() != 0 {
            return Err(HyperError::InvalidHandoff("misaligned"));
        }
        // SAFETY: contract above plus the alignment + non-null checks.
        let h = unsafe { core::ptr::read(ptr) };
        if h.magic != MAGIC {
            return Err(HyperError::InvalidHandoff("bad magic"));
        }
        if h.version != VERSION {
            return Err(HyperError::InvalidHandoff("version mismatch"));
        }
        Ok(h)
    }
}
