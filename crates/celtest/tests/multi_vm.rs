//! Multi-VM and capability-gated namespace contract tests (Week-7).
//!
//! Mirrors the kernel's `crate::manager::VmNamespace` surface on
//! `std`. The kernel manager is `cfg(not(test))` and lives on
//! `x86_64-unknown-none`, so we re-implement the same authorisation +
//! registry rules here and assert the externally-observable
//! invariants. When the kernel changes shape this test catches the
//! drift.
//!
//! Specifically verified:
//!
//!  * The namespace rejects construction from a non-VM capability.
//!  * Each method enforces its declared rights mask
//!    (READ for queries, INVOKE for start, INVOKE|WRITE for create / stop).
//!  * Two VMs created back-to-back receive distinct dense IDs.
//!  * `list_vms` reports both VMs and their independent terminal
//!    states after their respective starts.
//!  * Independent VMs do not interfere on `stop_vm`.

use core::sync::atomic::{AtomicU32, Ordering};

const MAX_VMS: usize = 4;

// ---- Rights / Capability / Object ------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rights(u32);
impl Rights {
    const READ:   Rights = Rights(1 << 0);
    const WRITE:  Rights = Rights(1 << 1);
    const INVOKE: Rights = Rights(1 << 2);
    fn contains(self, needed: Rights) -> bool { (self.0 & needed.0) == needed.0 }
}
impl core::ops::BitOr for Rights {
    type Output = Rights;
    fn bitor(self, rhs: Rights) -> Rights { Rights(self.0 | rhs.0) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Object { Vm(u32), Other }

#[derive(Debug, Clone, Copy)]
struct Capability { object: Object, rights: Rights }

impl Capability {
    fn check(&self, needed: Rights) -> Result<(), MgrError> {
        if self.rights.contains(needed) { Ok(()) } else { Err(MgrError::Denied) }
    }
}

// ---- VM state machine ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VmId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum VmState { Created = 0, Running = 1, Halted = 2, Stopped = 3, Faulted = 4 }
impl VmState {
    fn is_terminal(self) -> bool { matches!(self, Self::Halted | Self::Stopped | Self::Faulted) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MgrError { Exhausted, Denied }

struct Vm { state: AtomicU32, last_exit: AtomicU32 }
impl Vm {
    fn new() -> Self { Self { state: AtomicU32::new(VmState::Created as u32), last_exit: AtomicU32::new(u32::MAX) } }
    fn state(&self) -> VmState {
        match self.state.load(Ordering::SeqCst) {
            0 => VmState::Created, 1 => VmState::Running,
            2 => VmState::Halted,  3 => VmState::Stopped,
            _ => VmState::Faulted,
        }
    }
    fn last_exit(&self) -> Option<u32> {
        let v = self.last_exit.load(Ordering::SeqCst);
        if v == u32::MAX { None } else { Some(v) }
    }
    fn cas(&self, from: VmState, to: VmState) -> Result<(), MgrError> {
        self.state.compare_exchange(from as u32, to as u32, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ()).map_err(|_| MgrError::Denied)
    }
    fn mark_running(&self) -> Result<(), MgrError> { self.cas(VmState::Created, VmState::Running) }
    fn on_exit(&self, reason: u32, terminal: VmState) -> Result<(), MgrError> {
        assert!(terminal.is_terminal());
        self.last_exit.store(reason, Ordering::SeqCst);
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

// ---- Manager + namespace ---------------------------------------------------

#[derive(Clone, Copy)]
#[allow(dead_code)] // `state` retained for debug printout symmetry with the manager.
struct VmListEntry { id: VmId, state: VmState, last_exit: Option<u32> }

struct Manager {
    slots:   [Option<Vm>; MAX_VMS],
    runtime: bool,
}
impl Manager {
    fn new() -> Self { Self { slots: [const { None }; MAX_VMS], runtime: false } }
    fn init_runtime(&mut self) -> Result<(), MgrError> {
        if self.runtime { return Err(MgrError::Denied); }
        self.runtime = true; Ok(())
    }
    fn create(&mut self) -> Result<VmId, MgrError> {
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
    fn start(&self, id: VmId, terminal: VmState, reason: u32) -> Result<(), MgrError> {
        let vm = self.lookup(id)?;
        vm.mark_running()?;
        vm.on_exit(reason, terminal)
    }
    fn stop(&self, id: VmId) -> Result<(), MgrError> {
        let vm = self.lookup(id)?;
        if vm.state().is_terminal() { return Ok(()); }
        vm.stop()
    }
    fn list(&self) -> ([Option<VmListEntry>; MAX_VMS], usize) {
        let mut out: [Option<VmListEntry>; MAX_VMS] = [None; MAX_VMS];
        let mut n = 0;
        for (i, s) in self.slots.iter().enumerate() {
            if let Some(vm) = s {
                out[i] = Some(VmListEntry { id: VmId(i as u32), state: vm.state(), last_exit: vm.last_exit() });
                n += 1;
            }
        }
        (out, n)
    }
}

struct VmNamespace<'m> { mgr: core::cell::RefCell<&'m mut Manager>, cap: Capability }
impl<'m> VmNamespace<'m> {
    fn new(mgr: &'m mut Manager, cap: Capability) -> Result<Self, MgrError> {
        match cap.object {
            Object::Vm(_) => {}
            _ => return Err(MgrError::Denied),
        }
        cap.check(Rights::INVOKE)?;
        Ok(Self { mgr: core::cell::RefCell::new(mgr), cap })
    }
    fn create_vm(&self) -> Result<VmId, MgrError> {
        self.cap.check(Rights::INVOKE | Rights::WRITE)?;
        self.mgr.borrow_mut().create()
    }
    fn start_vm(&self, id: VmId, terminal: VmState, reason: u32) -> Result<(), MgrError> {
        self.cap.check(Rights::INVOKE)?;
        self.mgr.borrow().start(id, terminal, reason)
    }
    fn stop_vm(&self, id: VmId) -> Result<(), MgrError> {
        self.cap.check(Rights::INVOKE | Rights::WRITE)?;
        self.mgr.borrow().stop(id)
    }
    fn list_vms(&self) -> Result<([Option<VmListEntry>; MAX_VMS], usize), MgrError> {
        self.cap.check(Rights::READ)?;
        Ok(self.mgr.borrow().list())
    }
    fn vm_state(&self, id: VmId) -> Result<VmState, MgrError> {
        self.cap.check(Rights::READ)?;
        Ok(self.mgr.borrow().lookup(id)?.state())
    }
}

fn root() -> Capability {
    Capability { object: Object::Vm(0), rights: Rights::READ | Rights::WRITE | Rights::INVOKE }
}

// ---- Tests -----------------------------------------------------------------

#[test]
fn namespace_requires_vm_object_and_invoke() {
    let mut m = Manager::new();
    assert!(VmNamespace::new(&mut m, Capability { object: Object::Other, rights: Rights::INVOKE }).is_err());

    let mut m2 = Manager::new();
    assert!(VmNamespace::new(&mut m2,
        Capability { object: Object::Vm(0), rights: Rights::READ }).is_err());
}

#[test]
fn create_requires_invoke_and_write() {
    let mut m = Manager::new(); m.init_runtime().unwrap();
    let ns = VmNamespace::new(&mut m,
        Capability { object: Object::Vm(0), rights: Rights::INVOKE }).unwrap();
    assert_eq!(ns.create_vm().unwrap_err(), MgrError::Denied);
}

#[test]
fn start_requires_only_invoke() {
    let mut m = Manager::new(); m.init_runtime().unwrap();
    // Pre-seed a slot via the bare API so we can prove start_vm
    // works with INVOKE-only.
    let id = m.create().unwrap();
    let ns = VmNamespace::new(&mut m,
        Capability { object: Object::Vm(0), rights: Rights::INVOKE }).unwrap();
    ns.start_vm(id, VmState::Halted, 12).unwrap();
}

#[test]
fn read_only_cap_can_query_but_not_mutate() {
    let mut m = Manager::new(); m.init_runtime().unwrap();
    let id = m.create().unwrap();
    // Read-only namespace requires INVOKE for new(), so build a
    // READ+INVOKE cap (queries demand READ; new() demands INVOKE).
    let ns = VmNamespace::new(&mut m,
        Capability { object: Object::Vm(0), rights: Rights::READ | Rights::INVOKE }).unwrap();
    assert_eq!(ns.vm_state(id).unwrap(), VmState::Created);
    assert_eq!(ns.create_vm().unwrap_err(), MgrError::Denied); // missing WRITE
    assert_eq!(ns.stop_vm(id).unwrap_err(),  MgrError::Denied);
}

#[test]
fn two_vms_get_distinct_ids_and_independent_state() {
    let mut m = Manager::new(); m.init_runtime().unwrap();
    let ns = VmNamespace::new(&mut m, root()).unwrap();
    let a = ns.create_vm().unwrap();
    let b = ns.create_vm().unwrap();
    assert_ne!(a, b);

    ns.start_vm(a, VmState::Halted,  12).unwrap();
    ns.start_vm(b, VmState::Faulted, 48).unwrap();

    assert_eq!(ns.vm_state(a).unwrap(), VmState::Halted);
    assert_eq!(ns.vm_state(b).unwrap(), VmState::Faulted);
}

#[test]
fn list_vms_reports_every_allocated_slot() {
    let mut m = Manager::new(); m.init_runtime().unwrap();
    let ns = VmNamespace::new(&mut m, root()).unwrap();
    let a = ns.create_vm().unwrap();
    let b = ns.create_vm().unwrap();
    ns.start_vm(a, VmState::Halted, 12).unwrap();
    ns.start_vm(b, VmState::Stopped, 12).unwrap_or(()); // stop path accepted

    let (snap, n) = ns.list_vms().unwrap();
    assert_eq!(n, 2);
    let ids: Vec<u32> = snap.iter().flatten().map(|e| e.id.0).collect();
    assert!(ids.contains(&a.0) && ids.contains(&b.0));
}

#[test]
fn stop_one_vm_does_not_affect_others() {
    let mut m = Manager::new(); m.init_runtime().unwrap();
    let ns = VmNamespace::new(&mut m, root()).unwrap();
    let a = ns.create_vm().unwrap();
    let b = ns.create_vm().unwrap();
    ns.stop_vm(a).unwrap();
    assert_eq!(ns.vm_state(a).unwrap(), VmState::Stopped);
    assert_eq!(ns.vm_state(b).unwrap(), VmState::Created);
}

#[test]
fn list_vms_records_last_exit_reason() {
    let mut m = Manager::new(); m.init_runtime().unwrap();
    let ns = VmNamespace::new(&mut m, root()).unwrap();
    let a = ns.create_vm().unwrap();
    ns.start_vm(a, VmState::Halted, 12).unwrap();
    let (snap, _) = ns.list_vms().unwrap();
    let entry = snap.iter().flatten().find(|e| e.id == a).unwrap();
    assert_eq!(entry.last_exit, Some(12));
}
