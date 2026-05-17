//! W24-B — SMP bring-up + per-pCPU scheduler state.
//!
//! Today CelHyper assumes a single physical CPU and stores the
//! currently-active VM in a global `spin::Mutex` (see [`crate::sched`]).
//! Once we move to multi-vCPU guests every pCPU needs its own active
//! slot, and the boot path needs to issue INIT-SIPI-SIPI to the
//! application processors listed in [`crate::handoff::CeliumHandoff`]
//! v3.
//!
//! This module is the **public surface** that the rest of the kernel
//! programs against today. Two pieces are implemented for real:
//!
//! 1. [`MAX_PCPUS`] / [`PcpuState`] — the per-pCPU table the
//!    scheduler indexes into. Each entry holds an active-VM slot and
//!    a small heartbeat counter for diagnostics. Today we only ever
//!    populate index `0` (the BSP); the rest land in W25 when we
//!    actually trampoline an AP.
//! 2. [`Topology`] / [`Topology::from_handoff`] — typed view of the
//!    handoff's `cpu_count` / `bsp_apic_id` / `ap_apic_ids_phys`
//!    triple. Read-only; populated by CelLoader.
//!
//! The rest ([`bring_up_aps`], [`send_ipi`], real per-pCPU stack
//! allocation) returns [`HyperError::Unimplemented`] with a `W25`
//! tag so callers fail closed instead of pretending an AP started.
//!
//! The pattern is the same one we used for `drivers::virtio_blk`:
//! ship the trait surface + typed-TODO returns now so consumers can
//! program against the final API while the deep VMX / IPI / stack
//! plumbing matures.

#![cfg(not(test))]

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::error::{HyperError, HyperResult};
use crate::handoff::CeliumHandoff;
use crate::vm::Vm;

/// Maximum logical pCPUs CelHyper will ever bring online.
///
/// 8 is a deliberate compromise: it covers every laptop and most
/// developer desktops, fits a single 4 KiB page once we lay out the
/// per-pCPU state, and keeps the static table small enough to
/// initialise in a `const` context.
pub const MAX_PCPUS: usize = 8;

/// Per-pCPU state. One row per physical CPU; index = pCPU id.
///
/// Today only the BSP (index 0) is populated. The fields are
/// `AtomicUsize` / `AtomicU32` so the table is safe to publish across
/// every pCPU once they boot.
#[derive(Debug)]
pub struct PcpuState {
    /// LAPIC id reported by `cpuid.1.ebx[31:24]` on this CPU, or `0`
    /// before the CPU has registered itself.
    pub apic_id: AtomicU32,
    /// `1` if the pCPU has executed `vmxon` and is ready to run
    /// guest code, `0` otherwise.
    pub vmxon_ready: AtomicU32,
    /// Pointer to the currently-active VM. Stored as `usize` because
    /// `&'static Vm` is not `Atomic`. A value of `0` means the slot
    /// is idle. Migration in/out of this slot must go through
    /// [`enter_guest`] / [`leave_guest`].
    active_vm: AtomicUsize,
}

impl PcpuState {
    /// Build a fresh (idle) per-pCPU state. `const` so the static
    /// initialiser below stays trivial.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            apic_id: AtomicU32::new(0),
            vmxon_ready: AtomicU32::new(0),
            active_vm: AtomicUsize::new(0),
        }
    }

    /// Publish `vm` as the active VM on this pCPU. Returns
    /// [`HyperError::Denied`] if the slot is already occupied — the
    /// caller must drain it first.
    pub fn enter_guest(&self, vm: &'static Vm) -> HyperResult<()> {
        let new = vm as *const _ as usize;
        match self.active_vm.compare_exchange(0, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Ok(()),
            Err(_) => Err(HyperError::Denied("smp: pcpu already has an active vm")),
        }
    }

    /// Remove and return the active VM, if any.
    pub fn leave_guest(&self) -> Option<&'static Vm> {
        let raw = self.active_vm.swap(0, Ordering::AcqRel);
        if raw == 0 {
            None
        } else {
            // SAFETY: the only path that writes a non-zero value into
            // `active_vm` is `enter_guest`, which takes a `&'static Vm`
            // (i.e. a pointer to a `spin::Once`-anchored kernel VM).
            // Such pointers stay valid for the kernel's lifetime, so
            // re-materialising the reference is sound.
            Some(unsafe { &*(raw as *const Vm) })
        }
    }

    /// Cheap "what VM is running here?" peek without taking the slot.
    #[must_use]
    pub fn current_vm(&self) -> Option<&'static Vm> {
        let raw = self.active_vm.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // SAFETY: same invariant as `leave_guest`.
            Some(unsafe { &*(raw as *const Vm) })
        }
    }
}

impl Default for PcpuState {
    fn default() -> Self { Self::new() }
}

/// Per-pCPU table. Statically sized so we never allocate during AP
/// bring-up.
pub static PCPUS: [PcpuState; MAX_PCPUS] = [const { PcpuState::new() }; MAX_PCPUS];

/// Number of pCPUs the kernel has *successfully* brought online.
/// Starts at 1 (the BSP) once [`mark_bsp_online`] runs; AP bring-up
/// increments it as each AP reports in. Used by [`online_count`].
static ONLINE: AtomicU32 = AtomicU32::new(0);

/// Typed view of the SMP topology described by the handoff.
#[derive(Debug, Clone, Copy)]
pub struct Topology {
    /// Total logical CPUs (including BSP).
    pub cpu_count: u32,
    /// LAPIC id of the BSP.
    pub bsp_apic_id: u32,
    /// Physical address of the AP APIC id array; `0` when there are
    /// no APs (cpu_count <= 1).
    pub ap_apic_ids_phys: u64,
}

impl Topology {
    /// Extract the topology fields from the handoff and validate
    /// invariants. Rejects degenerate combinations (e.g. cpu_count > 1
    /// but `ap_apic_ids_phys == 0`).
    pub fn from_handoff(h: &CeliumHandoff) -> HyperResult<Self> {
        if h.cpu_count == 0 {
            return Err(HyperError::InvalidHandoff("smp: cpu_count == 0"));
        }
        if h.cpu_count as usize > MAX_PCPUS {
            return Err(HyperError::Exhausted("smp: cpu_count exceeds MAX_PCPUS"));
        }
        if h.cpu_count > 1 && h.ap_apic_ids_phys == 0 {
            return Err(HyperError::InvalidHandoff(
                "smp: cpu_count > 1 but ap_apic_ids_phys == 0",
            ));
        }
        if h.cpu_count == 1 && h.ap_apic_ids_phys != 0 {
            return Err(HyperError::InvalidHandoff(
                "smp: cpu_count == 1 but ap_apic_ids_phys is set",
            ));
        }
        Ok(Self {
            cpu_count: h.cpu_count,
            bsp_apic_id: h.bsp_apic_id,
            ap_apic_ids_phys: h.ap_apic_ids_phys,
        })
    }
}

/// Register the bootstrap processor in [`PCPUS`] slot 0 and bump
/// [`online_count`]. Called once from `bringup::bring_up` after the
/// scheduler installs the host GDT.
///
/// Returns [`HyperError::Internal`] if `online_count() != 0` — the BSP
/// must be the first CPU to register.
pub fn mark_bsp_online(topology: &Topology) -> HyperResult<()> {
    if ONLINE.load(Ordering::Acquire) != 0 {
        return Err(HyperError::Internal("smp: BSP already marked online"));
    }
    PCPUS[0].apic_id.store(topology.bsp_apic_id, Ordering::Release);
    PCPUS[0].vmxon_ready.store(1, Ordering::Release);
    ONLINE.store(1, Ordering::Release);
    Ok(())
}

/// Number of pCPUs successfully online. `1` after [`mark_bsp_online`],
/// climbing as APs join (W25).
#[must_use]
pub fn online_count() -> u32 {
    ONLINE.load(Ordering::Acquire)
}

/// Bring every AP listed in `topology` online.
///
/// W25 wires the IPI half of the bring-up: the BSP issues a real
/// INIT-SIPI-SIPI through [`crate::lapic::Lapic::init_sipi_sipi`] to
/// every AP id, BUT the kernel still does not allocate a real-mode
/// trampoline page or a per-AP boot stack. We refuse to dispatch the
/// SIPI without that prerequisite — landing a SIPI with no
/// trampoline page mapped would put the AP into an undefined state.
///
/// The W25 contract therefore is:
///
/// * `cpu_count <= 1`            → `Ok(())` (no-op, happy path).
/// * `cpu_count > 1`             → [`HyperError::Unimplemented`] with
///   the explicit `trampoline pending (W26)` tag, after logging that
///   we *would* IPI every AP id. This means a multi-CPU box keeps
///   booting single-CPU instead of half-booting an AP.
///
/// The actual IPI helper [`send_ipi`] *is* live this week and is
/// used by the scheduler for cross-pCPU wake-ups once W26 lands the
/// AP trampoline.
pub fn bring_up_aps(topology: &Topology) -> HyperResult<()> {
    if topology.cpu_count <= 1 {
        return Ok(());
    }
    crate::logger::log_kv("smp_ap_count", u64::from(topology.cpu_count - 1));
    Err(HyperError::Unimplemented(
        "smp::bring_up_aps: trampoline + AP stacks pending (W26)",
    ))
}

/// IPI vector tag. The actual byte we write into the ICR low word is
/// determined per-message-type; see SDM Vol 3 §10.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ipi {
    /// Wake a vCPU pinned to a different pCPU. Vector 0x40.
    Wakeup,
    /// Request the target pCPU drain its run queue and halt. Vector 0x41.
    Drain,
    /// Hard panic propagation — delivered as NMI so the target halts
    /// even with interrupts masked.
    Panic,
}

impl Ipi {
    /// Vector byte written into `ICR_LOW[7:0]`.
    #[must_use]
    pub fn vector(self) -> u8 {
        match self {
            Self::Wakeup => 0x40,
            Self::Drain => 0x41,
            Self::Panic => 0,
        }
    }

    /// Delivery mode for this IPI kind.
    #[must_use]
    pub fn delivery_mode(self) -> crate::lapic::DeliveryMode {
        match self {
            Self::Wakeup | Self::Drain => crate::lapic::DeliveryMode::Fixed,
            Self::Panic => crate::lapic::DeliveryMode::Nmi,
        }
    }
}

/// Send an inter-processor interrupt to `target_pcpu`.
///
/// `target_pcpu` indexes into [`PCPUS`]; the LAPIC id is read from
/// the per-pCPU state populated by [`mark_bsp_online`] (today only
/// slot 0 is populated, so cross-pCPU IPIs return `Denied` until
/// W26 brings APs online).
///
/// Errors:
///
/// * [`HyperError::Invalid`] — `target_pcpu` ≥ [`MAX_PCPUS`].
/// * [`HyperError::Denied`]  — target pCPU has never registered.
/// * [`HyperError::Hardware`] — LAPIC reported a delivery-status
///   hang (1M-iteration timeout).
pub fn send_ipi(target_pcpu: u32, kind: Ipi) -> HyperResult<()> {
    if (target_pcpu as usize) >= MAX_PCPUS {
        return Err(HyperError::Invalid("smp: target_pcpu >= MAX_PCPUS"));
    }
    let entry = &PCPUS[target_pcpu as usize];
    let apic_id = entry.apic_id.load(Ordering::Acquire);
    if entry.vmxon_ready.load(Ordering::Acquire) == 0 {
        return Err(HyperError::Denied("smp: target pCPU not online"));
    }
    let lapic = crate::lapic::Lapic::current()?;
    lapic.send_ipi(
        apic_id,
        kind.vector(),
        kind.delivery_mode(),
        crate::lapic::DestShorthand::None,
    )?;
    crate::metrics::count_ipi_sent();
    Ok(())
}

#[cfg(test)]
mod _doc_only {
    // The module is gated to `cfg(not(test))` at file scope, so this
    // stays empty — present only so `cargo test --lib` doesn't try to
    // collect host-side tests from a module that won't compile under
    // the host target.
}
