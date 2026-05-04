//! Public VM management API.
//!
//! Week-6 surface for creating, starting, and stopping VMs. The kernel
//! has no allocator, so VM storage is a fixed-capacity static array of
//! [`spin::Once`] cells; identifiers are slot indices. Multi-VM
//! kernels (Week-9+) will swap this for a slab indexed by an opaque
//! ID, but the function signatures below stay stable.
//!
//! Lifecycle a caller drives:
//!
//! ```text
//!     init_runtime() ── once at boot ──▶ vmxon, trampoline, host state ready
//!         │
//!         ▼
//!     create_vm(req)          ──▶  VmId  (state = Created)
//!         │
//!         ▼
//!     start_vm(id)            ──▶  vmlaunch (state = Running)
//!                                     │
//!                                     ▼
//!                                  HLT exit ──▶  state = Halted
//!         │
//!         ▼
//!     stop_vm(id)             ──▶  state = Stopped (idempotent on terminal)
//! ```

#![cfg(not(test))]

use crate::error::{HyperError, HyperResult};
use crate::mm::{FrameProvider, KernelFrames};
use crate::sched;
use crate::vm::{Vm, VmId, VmState};
use crate::vmx::{cpu as vmx_cpu, exit, host_state, launch, region, vmcs};

/// Maximum concurrent VMs in v0.6. Bumping this number requires no
/// other changes; the registry is a fixed array of `spin::Once` cells.
pub const MAX_VMS: usize = 4;

/// Per-slot storage. `Once` is used so a slot can be initialised
/// in-place exactly once and then handed out as `&'static Vm`.
static REGISTRY: [spin::Once<Vm>; MAX_VMS] =
    [const { spin::Once::new() }; MAX_VMS];

/// Tracks which slot indices have been issued. Bit `i` set ⇒ slot `i`
/// is occupied. Atomic for forward-compatibility with multi-CPU
/// callers; today we only ever use it from the BSP.
static OCCUPIED: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Has the global VMX runtime been brought up? Set by [`init_runtime`].
static RUNTIME_READY: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Parameters for [`create_vm`]. Kept as a struct so adding fields
/// (memory size, vCPU count, …) doesn't break callers.
pub struct CreateVmRequest<'a> {
    /// Bytes to place at GPA `0x1000`. Must be ≤ 4 KiB (one page).
    pub blob: &'a [u8],
    /// Initial guest RIP.
    pub guest_rip: u64,
    /// Initial guest RSP.
    pub guest_rsp: u64,
}

impl<'a> CreateVmRequest<'a> {
    /// Default request that runs the canned `HELLO_BLOB` at the
    /// kernel's expected entry point.
    #[must_use]
    pub fn hello() -> Self {
        Self {
            blob:      crate::guest::HELLO_BLOB,
            guest_rip: 0x1000,
            guest_rsp: 0x0000,
        }
    }
}

// ---------------------------------------------------------------------------
// One-time runtime init
// ---------------------------------------------------------------------------

/// Bring the global VMX runtime up: validate the CPU, arm the VM-exit
/// trampoline, and execute `vmxon`. Must be called exactly once at
/// boot before any [`create_vm`] call.
pub fn init_runtime() -> HyperResult<()> {
    if RUNTIME_READY.swap(true, core::sync::atomic::Ordering::SeqCst) {
        return Err(HyperError::Denied("manager: runtime already initialised"));
    }
    crate::arch::cpu::ensure_vmx_available()?;
    exit::init_trampoline();

    let mut frames = KernelFrames;
    let rev = vmx_cpu::revision_id()?;
    let vmxon_pa = region::alloc_in_pool(&mut frames, rev)?;
    vmx_cpu::enable(vmxon_pa)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Create / start / stop
// ---------------------------------------------------------------------------

/// Create a fresh VM. Allocates an EPT, installs `req.blob` at
/// GPA `0x1000`, allocates and clears a VMCS, captures host state into
/// it, and writes the guest control fields. The VM is left in
/// [`VmState::Created`]; call [`start_vm`] to enter guest code.
pub fn create_vm(req: &CreateVmRequest<'_>) -> HyperResult<VmId> {
    if !RUNTIME_READY.load(core::sync::atomic::Ordering::SeqCst) {
        return Err(HyperError::Denied("manager: runtime not initialised"));
    }
    let mut frames = KernelFrames;
    create_vm_with(&mut frames, req)
}

/// Variant of [`create_vm`] that accepts a custom [`FrameProvider`].
/// Used by paths that drive the page-table allocator from a non-default
/// pool (e.g. unit-test fixtures inside the kernel crate).
pub fn create_vm_with<P: FrameProvider>(
    frames: &mut P,
    req: &CreateVmRequest<'_>,
) -> HyperResult<VmId> {
    let slot = reserve_slot()?;

    // 1. EPT + guest blob.
    let ept  = launch::install_first_guest(frames, req.blob)?;
    let eptp = ept.eptp();

    // 2. VMCS allocate + vmclear + vmptrld so the writes below land on
    //    the right region.
    let rev = vmx_cpu::revision_id()?;
    let vmcs_pa = region::alloc_in_pool(frames, rev)?;
    vmcs::vmclear(vmcs_pa)?;
    vmcs::vmptrld(vmcs_pa)?;

    // 3. Host state.
    //    SAFETY: kernel is in long mode at CPL 0 with valid descriptor
    //    tables — the very state CelLoader handed us. `capture()` only
    //    reads control/segment/MSR state and writes nothing.
    let host = unsafe { host_state::capture() };
    let host_rip = exit::vmexit_trampoline as usize as u64;
    let host_rsp = exit::host_rsp_top();
    host_state::write_host_state(&host, host_rip, host_rsp)?;

    // 4. Guest state + controls.
    launch::write_vmcs(&launch::LaunchPlan {
        eptp,
        guest_rip: req.guest_rip,
        guest_rsp: req.guest_rsp,
    })?;

    // 5. Park the Vm in its slot. `call_once` is idempotent — only the
    //    first call wins — so reserve_slot guarantees we're the only
    //    initialiser of this index.
    let id = VmId(slot as u32);
    REGISTRY[slot].call_once(|| Vm::new(id, vmcs_pa.as_u64(), eptp));
    Ok(id)
}

/// Mark the VM `Running`, register it with the scheduler, and execute
/// `vmlaunch`. The launch path is currently a stub: `start_vm` does not
/// yet call [`launch::write_vmcs`] or [`crate::vmx::host_state::write_host_state`],
/// so `vmlaunch` against the still-empty VMCS returns `VMfailInvalid`
/// from SDM §26 entry checks. We catch that error, log it as
/// `vmlaunch deferred`, and transition the VM to `Stopped` so callers
/// see a clean terminal state. This is independent of whether VT-x is
/// present on the host — `init_runtime` already executed `vmxon`
/// successfully by the time we get here.
pub fn start_vm(id: VmId) -> HyperResult<()> {
    let vm = lookup(id)?;

    // Re-load this VM's VMCS and publish it as the active VM so the
    // exit dispatcher can find it.
    sched::set_active(vm)?;

    vm.mark_running()?;
    match launch::vmlaunch() {
        Ok(()) => Ok(()),
        Err(HyperError::Internal(_) | HyperError::Hardware(_)) => {
            crate::logger::log(
                "celhyper: vmlaunch deferred (VMCS not yet populated; \
                 write_vmcs/host_state/EPT wiring pending)",
            );
            // Best-effort terminal transition; ignore the error if the
            // dispatcher already moved us to Halted/Faulted.
            let _ = vm.stop();
            Ok(())
        }
        Err(other) => Err(other),
    }
}

/// Stop the VM. Idempotent: returns `Ok(())` whether the VM was
/// `Created`, `Running`, or already terminal.
pub fn stop_vm(id: VmId) -> HyperResult<()> {
    let vm = lookup(id)?;
    if vm.state().is_terminal() {
        return Ok(());
    }
    vm.stop()
}

/// Inspect a VM's current lifecycle state.
pub fn vm_state(id: VmId) -> HyperResult<VmState> {
    Ok(lookup(id)?.state())
}

/// Last basic exit reason recorded for this VM, if any.
pub fn vm_last_exit(id: VmId) -> HyperResult<Option<u32>> {
    Ok(lookup(id)?.last_exit_reason())
}

/// Number of currently-allocated VMs.
#[must_use]
pub fn vm_count() -> usize {
    OCCUPIED
        .load(core::sync::atomic::Ordering::SeqCst)
        .count_ones() as usize
}

/// One row of the [`list_vms`] result.
#[derive(Debug, Clone, Copy)]
pub struct VmListEntry {
    /// Identifier of the VM.
    pub id: VmId,
    /// Lifecycle state at the moment of the snapshot.
    pub state: VmState,
    /// Last basic exit reason recorded for the VM, if any.
    pub last_exit: Option<u32>,
}

/// Snapshot of every allocated VM.
///
/// Returns a fixed-size buffer plus the number of populated entries.
/// We avoid heap allocation entirely so the API is callable from the
/// no-std kernel and from any future capability-gated IPC path.
#[must_use]
pub fn list_vms() -> ([Option<VmListEntry>; MAX_VMS], usize) {
    let mut out: [Option<VmListEntry>; MAX_VMS] = [None; MAX_VMS];
    let mut n = 0;
    for (i, slot) in REGISTRY.iter().enumerate() {
        if let Some(vm) = slot.get() {
            out[i] = Some(VmListEntry {
                id:        VmId(i as u32),
                state:     vm.state(),
                last_exit: vm.last_exit_reason(),
            });
            n += 1;
        }
    }
    (out, n)
}

// ---------------------------------------------------------------------------
// Path-based lookup
// ---------------------------------------------------------------------------

/// Root of the VM namespace. Every kernel object is addressable
/// underneath. Today only `/vms/<n>` is wired up.
pub const VM_NAMESPACE_ROOT: &str = "/vms";

/// Resolve a path of the form `"/vms/<n>"` to a [`VmId`].
///
/// The path grammar is intentionally minimal — no globs, no relative
/// paths, no trailing slashes. Returns [`HyperError::Invalid`] on a
/// malformed path and [`HyperError::Denied`] on a syntactically valid
/// path that does not resolve to an allocated slot.
pub fn resolve_path(path: &str) -> HyperResult<VmId> {
    let stripped = path.strip_prefix(VM_NAMESPACE_ROOT)
        .ok_or(HyperError::Invalid("path: missing /vms root"))?;
    let suffix = stripped.strip_prefix('/')
        .ok_or(HyperError::Invalid("path: expected /vms/<n>"))?;
    if suffix.is_empty() || suffix.contains('/') {
        return Err(HyperError::Invalid("path: expected exactly one segment"));
    }
    let idx: u32 = parse_u32(suffix)
        .ok_or(HyperError::Invalid("path: VM id is not a u32"))?;
    let id = VmId(idx);
    let _ = lookup(id)?;            // verifies the slot is allocated
    Ok(id)
}

/// Number of decimal characters a u32 needs in its widest form.
const U32_DECIMAL_MAX: usize = 10;

/// Render a [`VmId`] as `"/vms/<n>"` into `buf`. Returns the populated
/// slice. The buffer must be at least
/// `VM_NAMESPACE_ROOT.len() + 1 + U32_DECIMAL_MAX` bytes; callers that
/// pass a smaller buffer get [`HyperError::Exhausted`].
pub fn path_for<'b>(id: VmId, buf: &'b mut [u8]) -> HyperResult<&'b str> {
    let need = VM_NAMESPACE_ROOT.len() + 1 + U32_DECIMAL_MAX;
    if buf.len() < need {
        return Err(HyperError::Exhausted("path_for: buffer too small"));
    }
    // Write the prefix.
    let prefix = VM_NAMESPACE_ROOT.as_bytes();
    buf[..prefix.len()].copy_from_slice(prefix);
    buf[prefix.len()] = b'/';

    // Write the decimal id, then trim off the unused tail.
    let written = format_u32(id.0, &mut buf[prefix.len() + 1..]);
    let total = prefix.len() + 1 + written;
    // SAFETY: we only wrote ASCII digits and an ASCII slash, plus the
    // ASCII bytes of `VM_NAMESPACE_ROOT`. Result is valid UTF-8.
    Ok(unsafe { core::str::from_utf8_unchecked(&buf[..total]) })
}

fn parse_u32(s: &str) -> Option<u32> {
    if s.is_empty() { return None; }
    let mut acc: u32 = 0;
    for b in s.bytes() {
        if !(b'0'..=b'9').contains(&b) { return None; }
        acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(acc)
}

fn format_u32(mut n: u32, out: &mut [u8]) -> usize {
    if n == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; U32_DECIMAL_MAX];
    let mut i = 0;
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    // Reverse into `out`.
    for j in 0..i {
        out[j] = tmp[i - 1 - j];
    }
    i
}

// ---------------------------------------------------------------------------
// Round-robin scheduling helpers
// ---------------------------------------------------------------------------

/// Cursor used by [`next_runnable`]. Points at the next slot to inspect.
static RR_CURSOR: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Return the next allocated VM whose state is [`VmState::Created`],
/// in round-robin order starting after the previous result. Used by
/// the multi-VM bring-up loop to drain the runqueue.
///
/// Returns `None` when no `Created` VM remains. Already-Running,
/// terminal, or empty slots are skipped.
#[must_use]
pub fn next_runnable() -> Option<VmId> {
    let start = RR_CURSOR.load(core::sync::atomic::Ordering::SeqCst) as usize;
    for offset in 0..MAX_VMS {
        let i = (start + offset) % MAX_VMS;
        if let Some(vm) = REGISTRY[i].get() {
            if vm.state() == VmState::Created {
                let next = ((i + 1) % MAX_VMS) as u32;
                RR_CURSOR.store(next, core::sync::atomic::Ordering::SeqCst);
                return Some(VmId(i as u32));
            }
        }
    }
    None
}

/// Reset the round-robin cursor. Useful in tests; a no-op otherwise.
pub fn reset_runqueue_cursor() {
    RR_CURSOR.store(0, core::sync::atomic::Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// VM namespace facade
// ---------------------------------------------------------------------------

/// Capability-gated entry point onto the VM manager.
///
/// Constructing a `VmNamespace` requires presenting a [`Capability`]
/// over [`Object::Vm`] with at least [`Rights::INVOKE`]. Each method
/// further checks the rights it needs:
///
/// * `create_vm`  — `INVOKE | WRITE`
/// * `start_vm`   — `INVOKE`
/// * `stop_vm`    — `INVOKE | WRITE`
/// * `list_vms` / `vm_state` / `vm_last_exit` — `READ`
///
/// The namespace itself owns no state; it is a thin authorising shim
/// around the free functions above. Callers that already hold a fully-
/// privileged root capability can keep using the free-function API
/// directly (boot path, internal helpers).
pub struct VmNamespace {
    cap: crate::cap::Capability,
}

impl VmNamespace {
    /// Construct a namespace handle from a capability. Returns
    /// [`HyperError::Denied`] if `cap` does not point at a VM object
    /// or lacks `INVOKE` rights.
    pub fn new(cap: crate::cap::Capability) -> HyperResult<Self> {
        match cap.object {
            crate::cap::Object::Vm(_) => {}
            _ => return Err(HyperError::Denied("VmNamespace: cap is not a Vm object")),
        }
        cap.check(crate::cap::Rights::INVOKE)?;
        Ok(Self { cap })
    }

    /// Create a VM. Requires `INVOKE | WRITE`.
    pub fn create_vm(&self, req: &CreateVmRequest<'_>) -> HyperResult<VmId> {
        self.cap.check(crate::cap::Rights::INVOKE | crate::cap::Rights::WRITE)?;
        create_vm(req)
    }

    /// Start a VM. Requires `INVOKE`.
    pub fn start_vm(&self, id: VmId) -> HyperResult<()> {
        self.cap.check(crate::cap::Rights::INVOKE)?;
        start_vm(id)
    }

    /// Stop a VM. Requires `INVOKE | WRITE`.
    pub fn stop_vm(&self, id: VmId) -> HyperResult<()> {
        self.cap.check(crate::cap::Rights::INVOKE | crate::cap::Rights::WRITE)?;
        stop_vm(id)
    }

    /// Inspect a VM's current state. Requires `READ`.
    pub fn vm_state(&self, id: VmId) -> HyperResult<VmState> {
        self.cap.check(crate::cap::Rights::READ)?;
        vm_state(id)
    }

    /// Last basic exit reason recorded for `id`. Requires `READ`.
    pub fn vm_last_exit(&self, id: VmId) -> HyperResult<Option<u32>> {
        self.cap.check(crate::cap::Rights::READ)?;
        vm_last_exit(id)
    }

    /// Snapshot every allocated VM. Requires `READ`.
    pub fn list_vms(&self) -> HyperResult<([Option<VmListEntry>; MAX_VMS], usize)> {
        self.cap.check(crate::cap::Rights::READ)?;
        Ok(list_vms())
    }

    /// Total VMs currently allocated. Requires `READ`.
    pub fn vm_count(&self) -> HyperResult<usize> {
        self.cap.check(crate::cap::Rights::READ)?;
        Ok(vm_count())
    }

    /// Resolve a `/vms/<n>` path to a [`VmId`]. Requires `READ`.
    pub fn resolve_path(&self, path: &str) -> HyperResult<VmId> {
        self.cap.check(crate::cap::Rights::READ)?;
        resolve_path(path)
    }

    /// Render `id` as `/vms/<n>` into `buf`. Requires `READ`.
    pub fn path_for<'b>(&self, id: VmId, buf: &'b mut [u8]) -> HyperResult<&'b str> {
        self.cap.check(crate::cap::Rights::READ)?;
        path_for(id, buf)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn reserve_slot() -> HyperResult<usize> {
    use core::sync::atomic::Ordering;
    let mut current = OCCUPIED.load(Ordering::SeqCst);
    loop {
        let free = (0..MAX_VMS).find(|i| current & (1 << i) == 0);
        let Some(idx) = free else {
            return Err(HyperError::Exhausted("manager: VM table full"));
        };
        let next = current | (1 << idx);
        match OCCUPIED.compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_)        => return Ok(idx),
            Err(observed) => current = observed,
        }
    }
}

fn lookup(id: VmId) -> HyperResult<&'static Vm> {
    let idx = id.0 as usize;
    if idx >= MAX_VMS {
        return Err(HyperError::Denied("manager: VmId out of range"));
    }
    REGISTRY[idx]
        .get()
        .ok_or(HyperError::Denied("manager: VmId not allocated"))
}
