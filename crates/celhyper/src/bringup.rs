//! Boot-time control loop.
//!
//! Week-7 evolution of `bring_up`:
//!
//! 1. Validates the handoff and mints the root VM capability.
//! 2. Initialises the VMX runtime once via [`crate::manager`].
//! 3. Constructs a [`VmNamespace`] and uses it to create **two**
//!    VMs — VM-A and VM-B — both running the canonical "Celium Guest
//!    Alive!" payload. This exercises the multi-VM slot allocator,
//!    the per-VM EPT, and the namespace permission checks even when
//!    we only ever observe one live launch per boot.
//! 4. Starts each VM in turn. On real VT-x the dispatcher halts the
//!    host after the first guest's HLT (resume policy lands in
//!    Week-8); on the dev box without VT-x, [`manager::start_vm`]
//!    catches the `vmlaunch` failure, logs `vmlaunch deferred`, and
//!    transitions the VM to `Stopped` so the loop can move on.
//! 5. Snapshots the namespace via [`VmNamespace::list_vms`] and logs
//!    every VM's terminal state through COM1 so external tooling
//!    (the run-qemu scripts) can verify the multi-VM bring-up.
//!
//! The function returns `Ok(())` if every VM reached a clean terminal
//! state (Halted or Stopped); a `Faulted` VM in the snapshot is
//! surfaced as [`HyperError::Hardware`].

#![cfg(not(test))]

use crate::cap::{Capability, Object, Rights};
use crate::error::{HyperError, HyperResult};
use crate::handoff::CeliumHandoff;
use crate::logger;
use crate::manager::{self, CreateVmRequest, VmListEntry, VmNamespace};
use crate::vm::VmState;

/// Run the boot-time control loop.
pub fn bring_up(handoff: &CeliumHandoff) -> HyperResult<()> {
    if !(handoff.cpu.vmx || handoff.cpu.svm) {
        return Err(HyperError::UnsupportedCpu("no VMX and no SVM"));
    }
    if !handoff.cpu.vmx {
        return Err(HyperError::Unimplemented("AMD-V bring-up (Week-8)"));
    }

    // 1. Mint a root capability with full rights.
    let root_cap = Capability {
        object: Object::Vm(0),
        rights: Rights::READ | Rights::WRITE | Rights::INVOKE | Rights::GRANT,
    };
    let ns = VmNamespace::new(root_cap)?;
    logger::log("celhyper: vm namespace constructed");

    // 2. Initialise the VMX runtime exactly once.
    manager::init_runtime()?;
    logger::log("celhyper: vmx runtime initialised");

    // 2a. Replace the UEFI GDT with our own (which carries a TSS).
    //     SDM §26.2.3 forbids HOST_TR=0 at VM entry; UEFI's GDT has no
    //     TSS slot and `str` returns the null selector without this.
    logger::log("celhyper: installing host gdt+tss...");
    // SAFETY: single-threaded boot path, CPL 0, called once.
    unsafe { crate::host_gdt::install(); }
    logger::log("celhyper: host gdt+tss installed");

    // 3. Create two VMs.
    let id_a = ns.create_vm(&CreateVmRequest::hello())?;
    logger::log_kv("vm_a_id", u64::from(id_a.0));
    let id_b = ns.create_vm(&CreateVmRequest::hello())?;
    logger::log_kv("vm_b_id", u64::from(id_b.0));
    logger::log_kv("vm_count", ns.vm_count()? as u64);

    // Sanity-check the namespace path round-trip — `/vms/<a>` must
    // resolve back to `id_a`. Done once per boot to catch path-grammar
    // regressions before the runqueue starts churning.
    let mut path_buf = [0u8; 32];
    let path_a = ns.path_for(id_a, &mut path_buf)?;
    let resolved = ns.resolve_path(path_a)?;
    if resolved != id_a {
        return Err(HyperError::Internal("path round-trip mismatch"));
    }
    logger::log("celhyper: vm namespace path round-trip ok");

    // 4. Start every runnable VM in round-robin order. The scheduler
    //    hands us each `Created` VM exactly once; an empty result
    //    means the runqueue is drained.
    manager::reset_runqueue_cursor();
    let mut launched = 0u32;
    while let Some(id) = manager::next_runnable() {
        if let Err(e) = ns.start_vm(id) {
            logger::log_kv("rr_start_failed_id", u64::from(id.0));
            return Err(e);
        }
        log_state(id.0, ns.vm_state(id)?);
        launched += 1;
    }
    logger::log_kv("rr_launched", u64::from(launched));

    // Suppress the now-unused per-VM ids — bringup uses them only for
    // the path round-trip above; the runqueue drove the actual starts.
    let _ = (id_a, id_b);

    // 5. Snapshot and verify every VM ended cleanly.
    let (snapshot, n) = ns.list_vms()?;
    logger::log_kv("vm_list_count", n as u64);
    let mut faulted_any = false;
    for entry in snapshot.iter().flatten() {
        log_entry(entry);
        if matches!(entry.state, VmState::Faulted) {
            faulted_any = true;
        }
    }

    // 6. Best-effort cleanup driven from the snapshot — works for any
    //    number of VMs without naming them. Idempotent on terminals.
    for entry in snapshot.iter().flatten() {
        let _ = ns.stop_vm(entry.id);
    }

    if faulted_any {
        Err(HyperError::Hardware("one or more guest VMs faulted"))
    } else {
        logger::log("celhyper: bring_up complete");
        Ok(())
    }
}

fn log_state(id: u32, state: VmState) {
    logger::log_kv("vm_id",        u64::from(id));
    logger::log_kv("vm_state_raw", state as u64);
    logger::log(state_tag(state));
}

fn log_entry(e: &VmListEntry) {
    logger::log_kv("ls_vm_id",        u64::from(e.id.0));
    logger::log_kv("ls_vm_state_raw", e.state as u64);
    if let Some(r) = e.last_exit {
        logger::log_kv("ls_vm_exit", u64::from(r));
    }
    logger::log(state_tag(e.state));
}

fn state_tag(state: VmState) -> &'static str {
    match state {
        VmState::Created => "vm_state=Created",
        VmState::Running => "vm_state=Running",
        VmState::Halted  => "vm_state=Halted",
        VmState::Stopped => "vm_state=Stopped",
        VmState::Faulted => "vm_state=Faulted",
    }
}
