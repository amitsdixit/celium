//! VM lifecycle.
//!
//! A [`Vm`] owns the resources tied to one guest:
//!
//! * its 4-level EPT root (with the guest blob already mapped),
//! * the VMCS region's physical address (the active VMCS at any given
//!   moment is whichever was last `vmptrld`-ed),
//! * a state machine.
//!
//! The state machine is intentionally tiny — five terminal/non-terminal
//! values. Concurrency is single-threaded today (BSP only) so we can
//! get away with a `Cell`-style atomic on each transition; multi-vCPU
//! support arrives in Week-7 alongside per-vCPU stacks.
//!
//! Transitions:
//!
//! ```text
//!   Created ──start──▶ Running ──on_exit(Hlt)──▶ Halted
//!                          │
//!                          ├──on_exit(Other)──▶ Faulted
//!                          │
//!                          └──stop()──────────▶ Stopped
//! ```
//!
//! Reaching `Halted`, `Stopped`, or `Faulted` is terminal in v0.5; a
//! future patch will allow `vmresume` from `Running` to itself, gated
//! on per-exit handlers.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::error::{HyperError, HyperResult};

/// Logical identifier of a [`Vm`]. Globally unique across the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VmId(pub u32);

/// Lifecycle states of a [`Vm`]. Values are encoded as `u32` so we can
/// store them in an [`AtomicU32`] field on `Vm` without a `Mutex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum VmState {
    /// Resources allocated, VMCS clear+ptrld done, host state written.
    Created  = 0,
    /// `vmlaunch` issued; CPU is executing guest code.
    Running  = 1,
    /// Guest reached a clean stop (HLT exit).
    Halted   = 2,
    /// Operator stopped the VM externally.
    Stopped  = 3,
    /// Guest exited with an unrecognised reason.
    Faulted  = 4,
}

impl VmState {
    /// Decode a `u32` (as stored in [`Vm::state`]) back to the enum.
    /// Returns `None` for out-of-range values; reaching that branch
    /// indicates a memory-corruption bug.
    fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => Self::Created,
            1 => Self::Running,
            2 => Self::Halted,
            3 => Self::Stopped,
            4 => Self::Faulted,
            _ => return None,
        })
    }

    /// `true` once the VM can no longer be entered.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Halted | Self::Stopped | Self::Faulted)
    }
}

/// One guest VM and its bookkeeping.
///
/// Construction is a thin shell — actual VMCS programming and the
/// `vmlaunch` happen in [`crate::vmx::launch`] under control of
/// [`Vm::start`]. We keep them separated so unit tests can construct a
/// `Vm` without an `unsafe` block.
pub struct Vm {
    id:       VmId,
    state:    AtomicU32,
    /// Physical address of the VMCS region, used to re-`vmptrld` if
    /// scheduling brings this VM back later.
    vmcs_pa:  u64,
    /// EPTP value (PML4 + walk-length + memtype).
    eptp:     u64,
    /// Last recorded basic exit reason. `u32::MAX` until the first
    /// VM-exit is observed.
    last_exit_reason: AtomicU32,
}

impl Vm {
    /// Construct a freshly-created VM. The caller is responsible for
    /// having already programmed the VMCS that `vmcs_pa` points at.
    #[must_use]
    pub fn new(id: VmId, vmcs_pa: u64, eptp: u64) -> Self {
        Self {
            id,
            state: AtomicU32::new(VmState::Created as u32),
            vmcs_pa,
            eptp,
            last_exit_reason: AtomicU32::new(u32::MAX),
        }
    }

    /// VM identifier.
    #[must_use]
    pub fn id(&self) -> VmId { self.id }

    /// Current lifecycle state.
    #[must_use]
    pub fn state(&self) -> VmState {
        // Out-of-range value would indicate corruption; surface it as
        // `Faulted` rather than panicking.
        VmState::from_u32(self.state.load(Ordering::SeqCst))
            .unwrap_or(VmState::Faulted)
    }

    /// Physical address of this VM's VMCS region.
    #[must_use]
    pub fn vmcs_pa(&self) -> u64 { self.vmcs_pa }

    /// EPTP value programmed into this VM's VMCS.
    #[must_use]
    pub fn eptp(&self) -> u64 { self.eptp }

    /// Last recorded basic exit reason, or `None` if no exit yet.
    #[must_use]
    pub fn last_exit_reason(&self) -> Option<u32> {
        let v = self.last_exit_reason.load(Ordering::SeqCst);
        if v == u32::MAX { None } else { Some(v) }
    }

    /// Mark this VM as `Running`. Called once the dispatcher has armed
    /// itself and we are about to issue `vmlaunch`. Returns
    /// [`HyperError::Denied`] if the VM is already running or terminal.
    pub fn mark_running(&self) -> HyperResult<()> {
        self.transition(VmState::Created, VmState::Running)
    }

    /// Record a VM-exit and transition to the appropriate terminal
    /// state. Called from the exit dispatcher.
    pub fn on_exit(&self, basic_reason: u32, kind: ExitOutcome) -> HyperResult<VmState> {
        self.last_exit_reason.store(basic_reason, Ordering::SeqCst);
        let next = match kind {
            ExitOutcome::Halted  => VmState::Halted,
            ExitOutcome::Faulted => VmState::Faulted,
        };
        self.transition(VmState::Running, next)?;
        Ok(next)
    }

    /// Operator-initiated stop. Only legal from `Created` or `Running`.
    pub fn stop(&self) -> HyperResult<()> {
        // We accept either pre-launch or running.
        let from_created = self.state
            .compare_exchange(
                VmState::Created as u32,
                VmState::Stopped as u32,
                Ordering::SeqCst, Ordering::SeqCst,
            ).is_ok();
        if from_created { return Ok(()); }
        let from_running = self.state
            .compare_exchange(
                VmState::Running as u32,
                VmState::Stopped as u32,
                Ordering::SeqCst, Ordering::SeqCst,
            ).is_ok();
        if from_running { return Ok(()); }
        Err(HyperError::Denied("Vm::stop: not in Created/Running"))
    }

    fn transition(&self, from: VmState, to: VmState) -> HyperResult<()> {
        self.state
            .compare_exchange(
                from as u32, to as u32,
                Ordering::SeqCst, Ordering::SeqCst,
            )
            .map(|_| ())
            .map_err(|_| HyperError::Denied("Vm::transition: bad source state"))
    }
}

/// Outcome the dispatcher hands to [`Vm::on_exit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// Guest reached a clean stop (HLT, etc.).
    Halted,
    /// Guest exited with an unrecognised reason — treat as fault.
    Faulted,
}
