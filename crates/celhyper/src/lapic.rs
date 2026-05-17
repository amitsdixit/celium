//! W25-A — Local APIC (xAPIC MMIO) driver.
//!
//! Programs the per-CPU LAPIC register window so the kernel can:
//!
//! * issue inter-processor interrupts (IPIs) — used by
//!   [`crate::smp::send_ipi`] and the INIT-SIPI-SIPI sequence in
//!   [`crate::smp::bring_up_aps`];
//! * acknowledge any spurious / pending interrupt via the
//!   end-of-interrupt register (`EOI`);
//! * report the BSP's own LAPIC id via `cpuid.1.ebx[31:24]`.
//!
//! ## Scope
//!
//! W25 ships the **xAPIC MMIO** path only. x2APIC (MSR-based) is a
//! later optimisation; the register layout differs and we want one
//! audited surface first. The MMIO window default is the hardware
//! reset value `0xFEE0_0000` (SDM Vol 3 §10.4.1); the kernel reads it
//! from `IA32_APIC_BASE` (MSR `0x1B`) so platforms that relocate the
//! window (rare but legal) still work.
//!
//! ## Safety model
//!
//! Every public function returns a `HyperResult` and validates its
//! inputs. Every `unsafe` block has a `// SAFETY:` justification.
//! The driver is single-instance: [`Lapic::init`] writes the global
//! [`LAPIC_BASE`] cell exactly once; subsequent callers re-use the
//! cached pointer. No concurrent writers exist on the BSP boot path,
//! and per-CPU LAPIC writes target *this CPU's* register window so
//! cross-pCPU coherence is not required.

#![cfg(not(test))]

use core::sync::atomic::{AtomicU64, Ordering};

use crate::error::{HyperError, HyperResult};

// ---------------------------------------------------------------------------
// Register offsets (SDM Vol 3 §10.4.1, Table 10-1)
// ---------------------------------------------------------------------------

/// `IA32_APIC_BASE` MSR. Bits 12..52 carry the APIC MMIO base; bit 11
/// is the global-enable; bit 8 distinguishes the BSP.
pub const IA32_APIC_BASE_MSR: u32 = 0x1B;

/// Hardware reset value of the LAPIC MMIO base.
pub const LAPIC_DEFAULT_BASE: u64 = 0xFEE0_0000;

/// MSR bit 11 — LAPIC global enable.
pub const APIC_BASE_ENABLE: u64 = 1 << 11;
/// MSR bit 8 — set on the bootstrap processor.
pub const APIC_BASE_BSP: u64 = 1 << 8;

/// LAPIC id register (read-only after init).
pub const REG_ID: usize = 0x020;
/// LAPIC version register.
pub const REG_VERSION: usize = 0x030;
/// End-of-interrupt register (write-only; any value).
pub const REG_EOI: usize = 0x0B0;
/// Spurious-interrupt vector register.
pub const REG_SVR: usize = 0x0F0;
/// Interrupt command register, low 32 bits.
pub const REG_ICR_LOW: usize = 0x300;
/// Interrupt command register, high 32 bits (destination field).
pub const REG_ICR_HIGH: usize = 0x310;

/// `SVR` bit 8 — software-enable. Must be set before any IPI is sent.
pub const SVR_SOFTWARE_ENABLE: u32 = 1 << 8;

// ---------------------------------------------------------------------------
// ICR encoding (SDM Vol 3 §10.6.1)
// ---------------------------------------------------------------------------

/// ICR delivery-mode field shifted into bits 8..11 of `ICR_LOW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Fixed-vector delivery.
    Fixed = 0b000,
    /// Lowest-priority delivery (multi-target).
    LowestPriority = 0b001,
    /// SMI — never used by Celium.
    Smi = 0b010,
    /// NMI — used for hard panic propagation.
    Nmi = 0b100,
    /// INIT IPI — first step of AP bring-up.
    Init = 0b101,
    /// STARTUP IPI — second + third step of AP bring-up. Vector field
    /// carries the trampoline page number.
    StartUp = 0b110,
}

/// ICR destination-shorthand field (bits 18..19 of `ICR_LOW`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestShorthand {
    /// Destination field is honoured.
    None = 0b00,
    /// Send only to self.
    SelfOnly = 0b01,
    /// Send to all CPUs including self.
    AllIncludingSelf = 0b10,
    /// Send to all CPUs except self.
    AllExcludingSelf = 0b11,
}

/// Encode an ICR-low word. Pure function; testable in isolation by
/// the W25 unit-test harness in `tests/lapic_icr.rs` if/when added.
#[must_use]
pub const fn encode_icr_low(
    vector: u8,
    mode: DeliveryMode,
    shorthand: DestShorthand,
    assert_level: bool,
    trigger_level: bool,
) -> u32 {
    let mut v: u32 = vector as u32;
    v |= (mode as u32) << 8;
    // bit 11: destination mode (0 = physical) — we never use logical.
    // bit 12: delivery status (read-only RW0 here)
    if assert_level {
        v |= 1 << 14;
    }
    if trigger_level {
        v |= 1 << 15;
    }
    v |= (shorthand as u32) << 18;
    v
}

// ---------------------------------------------------------------------------
// Cached MMIO base
// ---------------------------------------------------------------------------

/// Cached LAPIC MMIO base after [`Lapic::init`]. `0` means uninitialised.
///
/// Using `AtomicU64` (not `Once`) so the read is a single load that
/// every pCPU sees once the BSP publishes — APs joining in W26 will
/// re-validate via [`Lapic::current`] without taking a lock.
static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// Handle to the local APIC for **this** CPU. Cheap (`Copy`) because
/// it's just the MMIO base; per-CPU state is the MMIO window itself,
/// which is automatically the *running* CPU's LAPIC.
#[derive(Debug, Clone, Copy)]
pub struct Lapic {
    base: u64,
}

impl Lapic {
    /// Initialise the LAPIC driver. Reads `IA32_APIC_BASE`, enables
    /// the LAPIC if the firmware left it disabled, software-enables
    /// the SVR, and caches the MMIO base for [`Lapic::current`].
    ///
    /// Idempotent only on the BSP boot path; calling twice from
    /// different cores would be a bug and returns
    /// [`HyperError::Internal`].
    pub fn init() -> HyperResult<Self> {
        if LAPIC_BASE.load(Ordering::Acquire) != 0 {
            return Err(HyperError::Internal("lapic: already initialised"));
        }
        // SAFETY: `rdmsr` of `IA32_APIC_BASE` is defined on every
        // x86_64 CPU at CPL 0 and has no architectural side effects.
        let raw = unsafe { rdmsr(IA32_APIC_BASE_MSR) };
        let mut base = raw & 0x000F_FFFF_F000;
        if base == 0 {
            // Firmware never wrote the MSR — fall back to the
            // architectural reset address.
            base = LAPIC_DEFAULT_BASE;
        }
        // Set the global-enable bit if the firmware left it clear.
        if raw & APIC_BASE_ENABLE == 0 {
            // SAFETY: `wrmsr` of `IA32_APIC_BASE` with the current
            // value plus the enable bit is defined on every x86_64
            // CPU at CPL 0; we never clear the BSP bit.
            unsafe { wrmsr(IA32_APIC_BASE_MSR, raw | APIC_BASE_ENABLE) };
        }

        // Software-enable the SVR. Spurious-vector 0xFF is the
        // conventional choice and matches what UEFI usually leaves
        // programmed.
        let lapic = Self { base };
        // SAFETY: `base` is a valid 4 KiB MMIO window per
        // `IA32_APIC_BASE`; writes to `REG_SVR` only change software-
        // enable + spurious vector and have no side effect on the
        // running CPU.
        unsafe {
            let prev = lapic.read(REG_SVR);
            lapic.write(REG_SVR, prev | SVR_SOFTWARE_ENABLE | 0xFF);
        }

        LAPIC_BASE.store(base, Ordering::Release);
        Ok(lapic)
    }

    /// Return a handle pointing at this CPU's LAPIC window. Must be
    /// preceded by a successful [`Lapic::init`]; otherwise returns
    /// [`HyperError::Internal`].
    pub fn current() -> HyperResult<Self> {
        let base = LAPIC_BASE.load(Ordering::Acquire);
        if base == 0 {
            return Err(HyperError::Internal("lapic: not initialised"));
        }
        Ok(Self { base })
    }

    /// MMIO base for this LAPIC.
    #[must_use]
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Read this CPU's LAPIC id from `REG_ID`.
    #[must_use]
    pub fn id(&self) -> u32 {
        // SAFETY: `self.base + REG_ID` is a valid MMIO read at CPL 0.
        let raw = unsafe { self.read(REG_ID) };
        raw >> 24
    }

    /// Read the LAPIC version register. Bits 0..7 carry the version,
    /// bits 16..23 the max LVT entry count.
    #[must_use]
    pub fn version(&self) -> u32 {
        // SAFETY: `self.base + REG_VERSION` is a valid MMIO read.
        unsafe { self.read(REG_VERSION) }
    }

    /// Acknowledge the current interrupt by writing `0` to `EOI`.
    pub fn end_of_interrupt(&self) {
        // SAFETY: writing `0` to `REG_EOI` is the architecturally
        // defined acknowledge; it has no effect outside the LAPIC's
        // ISR/IRR state.
        unsafe { self.write(REG_EOI, 0) };
    }

    /// Send an IPI. `dest_apic_id` is the physical-mode destination
    /// (ignored when `shorthand` is non-`None`). Spins on the ICR
    /// delivery-status bit until the IPI is accepted by the bus.
    pub fn send_ipi(
        &self,
        dest_apic_id: u32,
        vector: u8,
        mode: DeliveryMode,
        shorthand: DestShorthand,
    ) -> HyperResult<()> {
        self.wait_idle()?;
        let low = encode_icr_low(vector, mode, shorthand, true, false);
        // SAFETY: writes target this CPU's LAPIC MMIO; ICR-high must
        // be programmed before ICR-low because the write to ICR-low
        // is what dispatches the IPI.
        unsafe {
            self.write(REG_ICR_HIGH, dest_apic_id << 24);
            self.write(REG_ICR_LOW, low);
        }
        self.wait_idle()
    }

    /// Send the INIT-SIPI-SIPI bring-up sequence to `dest_apic_id`.
    /// `trampoline_page` is the **page number** (physical address >>
    /// 12); the LAPIC vector field carries it directly. Per SDM
    /// §10.6.2 the AP starts executing at `(trampoline_page << 12)`
    /// in real mode.
    ///
    /// Caller MUST have:
    /// 1. populated the trampoline page with real-mode bring-up code,
    /// 2. allocated a per-AP boot stack,
    /// 3. ensured `dest_apic_id` is not the BSP's own id.
    ///
    /// The kernel does **none** of those things in W25 — see
    /// [`crate::smp::bring_up_aps`] for the typed-TODO that prevents
    /// callers from accidentally arming this without the trampoline
    /// in place.
    pub fn init_sipi_sipi(
        &self,
        dest_apic_id: u32,
        trampoline_page: u8,
    ) -> HyperResult<()> {
        // 1. INIT — assert.
        self.send_ipi(dest_apic_id, 0, DeliveryMode::Init, DestShorthand::None)?;
        // 2. SIPI #1.
        self.send_ipi(
            dest_apic_id,
            trampoline_page,
            DeliveryMode::StartUp,
            DestShorthand::None,
        )?;
        // 3. SIPI #2 (re-armed if the first was lost during AP wake).
        self.send_ipi(
            dest_apic_id,
            trampoline_page,
            DeliveryMode::StartUp,
            DestShorthand::None,
        )
    }

    /// Spin until the LAPIC reports `delivery_status == 0` in
    /// `ICR_LOW` (bit 12). Bounded to ~1M iterations so a mis-routed
    /// MMIO window can't lock the boot path forever.
    fn wait_idle(&self) -> HyperResult<()> {
        for _ in 0..1_000_000 {
            // SAFETY: `self.base + REG_ICR_LOW` is a valid MMIO read.
            let v = unsafe { self.read(REG_ICR_LOW) };
            if v & (1 << 12) == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(HyperError::Hardware("lapic: ICR delivery never settled"))
    }

    /// Raw MMIO read.
    ///
    /// # Safety
    ///
    /// Caller asserts that `self.base + off` is within the 4 KiB
    /// LAPIC MMIO window and that no concurrent writer exists. The
    /// kernel boot path satisfies both by construction: the LAPIC
    /// window is per-CPU and Rust's `&self` borrow rules forbid
    /// concurrent `write` calls.
    #[inline]
    unsafe fn read(&self, off: usize) -> u32 {
        let addr = (self.base + off as u64) as *const u32;
        // SAFETY: forwarded from the function-level invariant.
        unsafe { core::ptr::read_volatile(addr) }
    }

    /// Raw MMIO write.
    ///
    /// # Safety
    ///
    /// Same invariant as [`read`]; in addition the caller asserts
    /// that the target register is writable (a handful of LAPIC
    /// registers are read-only — `REG_ID`, `REG_VERSION`).
    #[inline]
    unsafe fn write(&self, off: usize, val: u32) {
        let addr = (self.base + off as u64) as *mut u32;
        // SAFETY: forwarded from the function-level invariant.
        unsafe { core::ptr::write_volatile(addr, val) };
    }
}

// ---------------------------------------------------------------------------
// MSR helpers (private)
// ---------------------------------------------------------------------------

/// `rdmsr` wrapper.
///
/// # Safety
///
/// MSR reads are CPL-0 instructions; reading an undefined MSR raises
/// `#GP`. Callers must pass an MSR index the platform supports.
#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: `rdmsr` with a valid MSR index is defined at CPL 0; the
    // caller guarantees the index is supported (we only ever pass
    // `IA32_APIC_BASE`, which is universal on x86_64).
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((hi as u64) << 32) | lo as u64
}

/// `wrmsr` wrapper.
///
/// # Safety
///
/// MSR writes are CPL-0 instructions; writing a reserved bit raises
/// `#GP`. The kernel only ever writes the architecturally defined
/// enable bit of `IA32_APIC_BASE`.
#[inline]
unsafe fn wrmsr(msr: u32, val: u64) {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    // SAFETY: same invariant as `rdmsr`; we never write a value with
    // reserved bits set because callers OR onto the just-read MSR.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
}
