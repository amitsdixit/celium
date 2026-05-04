//! Tiny single-VM scheduler.
//!
//! v0.5 holds at most one active VM. This is enough to drive the
//! Week-5 lifecycle (start → running → exit → halted) and gives the
//! VM-exit dispatcher a stable lookup target without forcing a
//! multi-vCPU runqueue today.
//!
//! The active-VM slot is published through a small `spin::Mutex<Option<&'static Vm>>`.
//! On a single-CPU bring-up the lock is uncontended; once we grow per-pCPU
//! state in Week-7 each pCPU will own its own slot and this module
//! becomes per-pCPU rather than global.

use spin::Mutex;

use crate::error::{HyperError, HyperResult};
use crate::vm::{ExitOutcome, Vm, VmState};
use crate::vmx::vmcs;

/// Logical identifier of a host physical CPU. Kept here so callers can
/// migrate from the older `sched::PcpuId` import path with one search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PcpuId(pub u32);

/// Logical identifier of a vCPU, scoped per-VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VcpuId(pub u32);

/// Per-vCPU scheduling policy. Reserved for Week-7; admitting a vCPU
/// today returns [`HyperError::Unimplemented`].
#[derive(Debug, Clone, Copy)]
pub enum Policy {
    /// Time-shared with `weight` proportional shares (1..=10000).
    Proportional {
        /// Relative weight against other proportional vCPUs.
        weight: u32,
    },
    /// Pinned to a host pCPU; never preempted by other guests.
    Dedicated {
        /// Host pCPU this vCPU is bound to.
        pcpu: PcpuId,
    },
}

/// Active-VM slot. `None` = no VM running on this CPU.
static ACTIVE: Mutex<Option<&'static Vm>> = Mutex::new(None);

/// Install `vm` as the active VM on this CPU and `vmptrld` its VMCS.
///
/// The reference must be `'static` — we obtain that by leaking a `Box`
/// during boot. Multi-VM kernels will store these in a slab indexed by
/// [`VmId`].
pub fn set_active(vm: &'static Vm) -> HyperResult<()> {
    vmcs::vmptrld(crate::mm::PhysAddr::new(vm.vmcs_pa()))?;
    *ACTIVE.lock() = Some(vm);
    Ok(())
}

/// Remove the active VM and return whatever was there. Idempotent.
pub fn clear_active() -> Option<&'static Vm> {
    ACTIVE.lock().take()
}

/// Borrow the currently-active VM, if any. The borrow is short-lived;
/// callers should release the guard before any operation that might
/// itself touch the slot.
pub fn with_active<R>(f: impl FnOnce(Option<&'static Vm>) -> R) -> R {
    let g = ACTIVE.lock();
    f(*g)
}

/// Dispatch a VM-exit to the active VM. Records the exit reason and
/// transitions the VM into either [`VmState::Halted`] or
/// [`VmState::Faulted`]. Returns the resulting state so the caller can
/// decide whether to halt the host or schedule the next VM.
pub fn dispatch_exit(basic_reason: u32, outcome: ExitOutcome) -> HyperResult<VmState> {
    let active = ACTIVE.lock().ok_or(HyperError::Denied("scheduler: no active VM"))?;
    active.on_exit(basic_reason, outcome)
}

/// Admit a new vCPU under `policy`. Reserved for Week-7.
pub fn admit_vcpu(vcpu: VcpuId, policy: Policy) -> HyperResult<()> {
    let _ = (vcpu, policy);
    Err(HyperError::Unimplemented("scheduler::admit_vcpu (week-7)"))
}
