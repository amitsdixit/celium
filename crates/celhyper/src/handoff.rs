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
/// * v3 (W24-A): adds SMP topology (`cpu_count`, `bsp_apic_id`,
///   `ap_apic_ids_phys`) and an optional GOP linear framebuffer block
///   (`fb_phys`, `fb_width`, `fb_height`, `fb_pitch`, `fb_format`).
///   New fields default to zero; the kernel treats that as "single
///   CPU, text-only console".
pub const VERSION: u32 = 3;

/// Framebuffer pixel format. Mirror of `celloader::handoff::FbFormat`.
#[allow(missing_docs)]
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbFormat {
    Unknown = 0,
    Bgra8   = 1,
    Rgba8   = 2,
}

impl FbFormat {
    /// Convert the raw u32 we received from the handoff block back into
    /// a typed `FbFormat`. Unknown values map to [`FbFormat::Unknown`]
    /// rather than producing an error — the framebuffer is purely
    /// optional and an unknown tag just means "don't draw".
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::Bgra8,
            2 => Self::Rgba8,
            _ => Self::Unknown,
        }
    }
}

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

    // ---- v3 (W24-A): SMP topology + framebuffer ----

    /// Total logical CPUs detected via ACPI MADT (or `1` for
    /// "BSP only / unknown"). When `>1`, CelHyper enables
    /// [`crate::smp`] and issues INIT-SIPI-SIPI to the APs listed in
    /// `ap_apic_ids_phys`.
    pub cpu_count: u32,
    /// LAPIC id of the bootstrap processor. Sanity-checked against the
    /// running CPU's APIC id by [`crate::smp::self_check`].
    pub bsp_apic_id: u32,
    /// Physical address of an array of `u32` APIC ids for the
    /// application processors. Length is `cpu_count - 1`; `0` when
    /// `cpu_count <= 1`. The kernel treats the array as read-only.
    pub ap_apic_ids_phys: u64,

    /// Physical address of the GOP linear framebuffer, or `0` when
    /// CelLoader did not negotiate a graphics console.
    pub fb_phys: u64,
    /// Framebuffer width in pixels.
    pub fb_width: u32,
    /// Framebuffer height in pixels.
    pub fb_height: u32,
    /// Framebuffer stride in **bytes** per scanline.
    pub fb_pitch: u32,
    /// Pixel format tag (see [`FbFormat`]).
    pub fb_format: u32,
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
