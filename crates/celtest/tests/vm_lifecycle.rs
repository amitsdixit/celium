//! End-to-end VM lifecycle simulation (Week-5 integration test).
//!
//! The Week-3 sibling test (`guest_simulation.rs`) verifies the
//! **content contract** — that the Celium-Alive blob walks through an
//! EPT and emits its marker through a tiny port-`0xE9` simulator. This
//! file goes one level up: it verifies the **lifecycle contract** that
//! the Week-5 `crates/celhyper/src/vm.rs` and `sched.rs` modules
//! impose:
//!
//! ```text
//!   Created ──start──▶ Running ──HLT exit ──▶ Halted
//!                          │
//!                          └──unknown exit ──▶ Faulted
//! ```
//!
//! The kernel crate is workspace-excluded (it builds for
//! `x86_64-unknown-none`), so this test re-derives a parallel state
//! machine and asserts the same invariants the kernel tests by
//! construction. When the kernel ever runs on real hardware (or QEMU
//! with `nested=1`) the dispatcher transitions are observable through
//! the COM1 log and we can drop this stand-in.

use core::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Parallel state machine — mirrors crates/celhyper/src/vm.rs
// ---------------------------------------------------------------------------

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
    fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::Created,
            1 => Self::Running,
            2 => Self::Halted,
            3 => Self::Stopped,
            _ => Self::Faulted,
        }
    }
    fn is_terminal(self) -> bool {
        matches!(self, Self::Halted | Self::Stopped | Self::Faulted)
    }
}

#[derive(Debug, Clone, Copy)]
enum ExitOutcome {
    Halted,
    Faulted,
}

struct Vm {
    state: AtomicU32,
    last_exit_reason: AtomicU32,
}

impl Vm {
    fn new() -> Self {
        Self {
            state: AtomicU32::new(VmState::Created as u32),
            last_exit_reason: AtomicU32::new(u32::MAX),
        }
    }
    fn state(&self) -> VmState { VmState::from_u32(self.state.load(Ordering::SeqCst)) }
    fn last_exit_reason(&self) -> Option<u32> {
        let v = self.last_exit_reason.load(Ordering::SeqCst);
        if v == u32::MAX { None } else { Some(v) }
    }
    fn cas(&self, from: VmState, to: VmState) -> Result<(), &'static str> {
        self.state.compare_exchange(
            from as u32, to as u32,
            Ordering::SeqCst, Ordering::SeqCst,
        ).map(|_| ()).map_err(|_| "bad source state")
    }
    fn mark_running(&self) -> Result<(), &'static str> { self.cas(VmState::Created, VmState::Running) }
    fn on_exit(&self, reason: u32, outcome: ExitOutcome) -> Result<VmState, &'static str> {
        self.last_exit_reason.store(reason, Ordering::SeqCst);
        let next = match outcome { ExitOutcome::Halted => VmState::Halted, ExitOutcome::Faulted => VmState::Faulted };
        self.cas(VmState::Running, next).map(|_| next)
    }
    fn stop(&self) -> Result<(), &'static str> {
        if self.state.compare_exchange(VmState::Created as u32, VmState::Stopped as u32,
            Ordering::SeqCst, Ordering::SeqCst).is_ok() { return Ok(()); }
        if self.state.compare_exchange(VmState::Running as u32, VmState::Stopped as u32,
            Ordering::SeqCst, Ordering::SeqCst).is_ok() { return Ok(()); }
        Err("not in Created/Running")
    }
}

// Basic exit reasons the kernel cares about.
const EXIT_HLT: u32 = 12;
const EXIT_EPT: u32 = 48;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn happy_path_created_running_halted() {
    let vm = Vm::new();
    assert_eq!(vm.state(), VmState::Created);

    vm.mark_running().expect("Created → Running");
    assert_eq!(vm.state(), VmState::Running);

    let next = vm.on_exit(EXIT_HLT, ExitOutcome::Halted).expect("Running → Halted");
    assert_eq!(next, VmState::Halted);
    assert_eq!(vm.state(), VmState::Halted);
    assert_eq!(vm.last_exit_reason(), Some(EXIT_HLT));
    assert!(vm.state().is_terminal());
}

#[test]
fn cannot_start_twice() {
    let vm = Vm::new();
    vm.mark_running().unwrap();
    assert!(vm.mark_running().is_err());
}

#[test]
fn stop_works_pre_and_post_launch() {
    let a = Vm::new();
    a.stop().expect("stop from Created");
    assert_eq!(a.state(), VmState::Stopped);

    let b = Vm::new();
    b.mark_running().unwrap();
    b.stop().expect("stop from Running");
    assert_eq!(b.state(), VmState::Stopped);
}

#[test]
fn stop_after_halt_is_denied() {
    let vm = Vm::new();
    vm.mark_running().unwrap();
    vm.on_exit(EXIT_HLT, ExitOutcome::Halted).unwrap();
    assert!(vm.stop().is_err());
}

#[test]
fn unknown_exit_transitions_to_faulted() {
    let vm = Vm::new();
    vm.mark_running().unwrap();
    let next = vm.on_exit(EXIT_EPT, ExitOutcome::Faulted).unwrap();
    assert_eq!(next, VmState::Faulted);
    assert_eq!(vm.last_exit_reason(), Some(EXIT_EPT));
    assert!(vm.state().is_terminal());
}

#[test]
fn dispatcher_records_then_halts() {
    // Models the dispatcher's contract: read the basic exit reason,
    // call vm.on_exit, halt the host. Reaching `Halted` after a HLT
    // exit is precisely what the COM1 log line "GUEST OK — Celium
    // Guest Alive!" reports.
    let vm = Vm::new();
    vm.mark_running().unwrap();

    let basic_reason: u32 = EXIT_HLT;
    let outcome = if basic_reason == EXIT_HLT { ExitOutcome::Halted } else { ExitOutcome::Faulted };
    let final_state = vm.on_exit(basic_reason, outcome).unwrap();

    assert_eq!(final_state, VmState::Halted);
    assert!(vm.state().is_terminal());
}
