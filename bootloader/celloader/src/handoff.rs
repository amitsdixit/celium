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
/// * v3 (W24-A): adds SMP topology (`cpu_count`, `bsp_apic_id`,
///   `ap_apic_ids_phys`) and an optional GOP linear framebuffer
///   block (`fb_phys`, `fb_width`, `fb_height`, `fb_pitch`,
///   `fb_format`). All new fields default to zero when CelLoader
///   cannot probe them; the kernel then behaves as a single-CPU
///   text-only boot, identical to W23 behaviour.
pub const VERSION: u32 = 3;

/// Framebuffer pixel format tag. Mirrors `celhyper::handoff::FbFormat`.
#[allow(missing_docs)]
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbFormat {
    Unknown = 0,
    Bgra8   = 1,
    Rgba8   = 2,
}

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

    // ---- v3 (W24-A): SMP topology + framebuffer ----

    /// Total logical CPUs detected via ACPI MADT. `1` means
    /// "BSP only" (CelLoader could not find a MADT or chose not to
    /// probe). Bumping above 1 enables [`crate::smp`] on the
    /// kernel side.
    pub cpu_count: u32,
    /// LAPIC id of the bootstrap processor (BSP). `0` is the
    /// canonical value on every box we've shipped; the field exists
    /// so CelHyper can sanity-check that it is indeed running on the
    /// BSP before issuing INIT-SIPI-SIPI to the APs.
    pub bsp_apic_id: u32,
    /// Physical address of an array of `u32` APIC ids for the
    /// application processors (length = `cpu_count - 1`). `0` when
    /// `cpu_count <= 1`. The array lives in a leaked CelLoader
    /// allocation; the kernel must treat it as read-only.
    pub ap_apic_ids_phys: u64,

    /// Physical address of the GOP linear framebuffer base, or `0`
    /// when CelLoader could not negotiate a graphics console.
    pub fb_phys: u64,
    /// Framebuffer horizontal pixel count.
    pub fb_width: u32,
    /// Framebuffer vertical pixel count.
    pub fb_height: u32,
    /// Framebuffer stride in **bytes** per scanline.
    pub fb_pitch: u32,
    /// Pixel format tag (see [`FbFormat`]). Stored as `u32` so the
    /// FFI ABI is stable across compilers.
    pub fb_format: u32,
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

            // W24-A: defaults — single CPU, no framebuffer. Stage-0
            // overlays real values via the builders below once it
            // has probed the MADT and GOP.
            cpu_count: 1,
            bsp_apic_id: 0,
            ap_apic_ids_phys: 0,
            fb_phys: 0,
            fb_width: 0,
            fb_height: 0,
            fb_pitch: 0,
            fb_format: FbFormat::Unknown as u32,
        }
    }

    /// W24-A: install SMP topology discovered by [`crate::hardware`].
    ///
    /// `ap_apic_ids` is a slice of LAPIC ids for every application
    /// processor (excluding the BSP). The slice must outlive the
    /// handoff block — in practice CelLoader hands us a `Box::leak`'d
    /// region and we just store its pointer.
    #[must_use]
    pub fn with_smp(mut self, bsp_apic_id: u32, ap_apic_ids_phys: u64, cpu_count: u32) -> Self {
        self.cpu_count = cpu_count;
        self.bsp_apic_id = bsp_apic_id;
        self.ap_apic_ids_phys = ap_apic_ids_phys;
        self
    }

    /// W24-A: install the GOP linear framebuffer the kernel can
    /// optionally draw to during early boot.
    #[must_use]
    pub fn with_framebuffer(
        mut self,
        fb_phys: u64,
        fb_width: u32,
        fb_height: u32,
        fb_pitch: u32,
        fb_format: FbFormat,
    ) -> Self {
        self.fb_phys = fb_phys;
        self.fb_width = fb_width;
        self.fb_height = fb_height;
        self.fb_pitch = fb_pitch;
        self.fb_format = fb_format as u32;
        self
    }
}
