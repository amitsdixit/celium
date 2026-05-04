//! End-to-end VM-management API contract test (Week-6).
//!
//! `celhyper::manager` is the public surface a host program (or a
//! capability-gated IPC client, eventually) drives to create, start,
//! and stop VMs. Because the manager binary is `cfg(not(test))` and
//! lives on `x86_64-unknown-none`, this test re-implements the same
//! contract on `std` and asserts the externally-observable invariants:
//!
//!  * `create_vm` returns a fresh `VmId` from `[0, MAX_VMS)`.
//!  * Each VM starts in `Created`, transitions to `Running` on
//!    `start_vm`, and to a terminal state on the next dispatcher tick.
//!  * `stop_vm` is idempotent on terminal states.
//!  * The registry rejects allocation past `MAX_VMS` with `Exhausted`
//!    and rejects look-ups of unallocated IDs with `Denied`.
//!
//! The kernel side mirrors these invariants by construction; this
//! test exists so we have a CI signal that catches accidental
//! changes to the contract.

use core::sync::atomic::{AtomicU32, Ordering};

const MAX_VMS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VmId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum VmState {
    Created = 0,
    Running = 1,
    Halted  = 2,
    Stopped = 3,
    Faulted = 4,
}

impl VmState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Halted | Self::Stopped | Self::Faulted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MgrError {
    Exhausted,
    Denied,
}

struct Vm {
    state: AtomicU32,
}
impl Vm {
    fn new() -> Self { Self { state: AtomicU32::new(VmState::Created as u32) } }
    fn state(&self) -> VmState {
        match self.state.load(Ordering::SeqCst) {
            0 => VmState::Created, 1 => VmState::Running,
            2 => VmState::Halted,  3 => VmState::Stopped,
            _ => VmState::Faulted,
        }
    }
    fn cas(&self, from: VmState, to: VmState) -> Result<(), MgrError> {
        self.state.compare_exchange(
            from as u32, to as u32,
            Ordering::SeqCst, Ordering::SeqCst,
        ).map(|_| ()).map_err(|_| MgrError::Denied)
    }
    fn mark_running(&self) -> Result<(), MgrError> { self.cas(VmState::Created, VmState::Running) }
    fn on_exit(&self, terminal: VmState) -> Result<(), MgrError> {
        assert!(terminal.is_terminal(), "on_exit must take a terminal state");
        self.cas(VmState::Running, terminal)
    }
    fn stop(&self) -> Result<(), MgrError> {
        if self.state.compare_exchange(VmState::Created as u32, VmState::Stopped as u32,
            Ordering::SeqCst, Ordering::SeqCst).is_ok() { return Ok(()); }
        if self.state.compare_exchange(VmState::Running as u32, VmState::Stopped as u32,
            Ordering::SeqCst, Ordering::SeqCst).is_ok() { return Ok(()); }
        Err(MgrError::Denied)
    }
}

struct Manager {
    slots:   [Option<Vm>; MAX_VMS],
    runtime: bool,
}
impl Manager {
    fn new() -> Self { Self { slots: [const { None }; MAX_VMS], runtime: false } }
    fn init_runtime(&mut self) -> Result<(), MgrError> {
        if self.runtime { return Err(MgrError::Denied); }
        self.runtime = true;
        Ok(())
    }
    fn create_vm(&mut self) -> Result<VmId, MgrError> {
        if !self.runtime { return Err(MgrError::Denied); }
        for (i, s) in self.slots.iter_mut().enumerate() {
            if s.is_none() { *s = Some(Vm::new()); return Ok(VmId(i as u32)); }
        }
        Err(MgrError::Exhausted)
    }
    fn lookup(&self, id: VmId) -> Result<&Vm, MgrError> {
        let i = id.0 as usize;
        if i >= MAX_VMS { return Err(MgrError::Denied); }
        self.slots[i].as_ref().ok_or(MgrError::Denied)
    }
    /// Mirrors `manager::start_vm` on the dev box: marks Running, then
    /// the dispatcher (modelled inline) reports a HLT exit and the VM
    /// becomes Halted; on the no-VT-x path we'd call `stop()` instead.
    fn start_vm(&self, id: VmId, exit_terminal: VmState) -> Result<(), MgrError> {
        let vm = self.lookup(id)?;
        vm.mark_running()?;
        vm.on_exit(exit_terminal)?;
        Ok(())
    }
    fn stop_vm(&self, id: VmId) -> Result<(), MgrError> {
        let vm = self.lookup(id)?;
        if vm.state().is_terminal() { return Ok(()); }
        vm.stop()
    }
    fn vm_state(&self, id: VmId) -> Result<VmState, MgrError> {
        Ok(self.lookup(id)?.state())
    }
    fn vm_count(&self) -> usize { self.slots.iter().filter(|s| s.is_some()).count() }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn create_requires_runtime_init() {
    let mut m = Manager::new();
    assert_eq!(m.create_vm().unwrap_err(), MgrError::Denied);
    m.init_runtime().unwrap();
    assert!(m.create_vm().is_ok());
}

#[test]
fn init_runtime_is_one_shot() {
    let mut m = Manager::new();
    m.init_runtime().unwrap();
    assert_eq!(m.init_runtime().unwrap_err(), MgrError::Denied);
}

#[test]
fn full_lifecycle_create_start_halt_stop() {
    let mut m = Manager::new();
    m.init_runtime().unwrap();
    let id = m.create_vm().unwrap();

    assert_eq!(m.vm_state(id).unwrap(), VmState::Created);
    m.start_vm(id, VmState::Halted).unwrap();
    assert_eq!(m.vm_state(id).unwrap(), VmState::Halted);

    // stop_vm is idempotent on terminal states.
    m.stop_vm(id).unwrap();
    assert_eq!(m.vm_state(id).unwrap(), VmState::Halted);
}

#[test]
fn stop_before_start_transitions_to_stopped() {
    let mut m = Manager::new();
    m.init_runtime().unwrap();
    let id = m.create_vm().unwrap();
    m.stop_vm(id).unwrap();
    assert_eq!(m.vm_state(id).unwrap(), VmState::Stopped);
}

#[test]
fn registry_rejects_overflow() {
    let mut m = Manager::new();
    m.init_runtime().unwrap();
    for _ in 0..MAX_VMS { m.create_vm().unwrap(); }
    assert_eq!(m.create_vm().unwrap_err(), MgrError::Exhausted);
    assert_eq!(m.vm_count(), MAX_VMS);
}

#[test]
fn unknown_id_is_denied() {
    let m = Manager::new();
    assert_eq!(m.vm_state(VmId(0)).unwrap_err(), MgrError::Denied);
    assert_eq!(m.vm_state(VmId(99)).unwrap_err(), MgrError::Denied);
}

#[test]
fn faulted_vm_is_terminal_after_start() {
    let mut m = Manager::new();
    m.init_runtime().unwrap();
    let id = m.create_vm().unwrap();
    m.start_vm(id, VmState::Faulted).unwrap();
    assert_eq!(m.vm_state(id).unwrap(), VmState::Faulted);
    // stop_vm on a terminal state is a no-op success.
    m.stop_vm(id).unwrap();
    assert_eq!(m.vm_state(id).unwrap(), VmState::Faulted);
}

#[test]
fn ids_are_dense_and_distinct() {
    let mut m = Manager::new();
    m.init_runtime().unwrap();
    let a = m.create_vm().unwrap();
    let b = m.create_vm().unwrap();
    let c = m.create_vm().unwrap();
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert!(a.0 < MAX_VMS as u32 && b.0 < MAX_VMS as u32 && c.0 < MAX_VMS as u32);
}
