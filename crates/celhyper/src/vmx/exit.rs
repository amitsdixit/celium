//! VM-exit trampoline and Rust-level dispatcher.
//!
//! ## What VMX restores for us, and what we restore ourselves
//!
//! On a VM-exit the CPU automatically restores from the VMCS host-state
//! area (SDM Vol 3 §27.5):
//!
//! * `CR0`, `CR3`, `CR4`
//! * Segment selectors (`CS`/`DS`/`ES`/`FS`/`GS`/`SS`/`TR`) and their
//!   hidden descriptor caches built from the host base fields
//! * `GDTR`, `IDTR`
//! * `IA32_EFER` (when the corresponding exit control is set)
//! * `RIP` ← `HOST_RIP`, `RSP` ← `HOST_RSP`
//!
//! What it does **not** restore are the general-purpose registers,
//! `RFLAGS`, `XMM`/AVX state, x87 state, and the FS/GS bases beyond the
//! ones in the VMCS. Those still hold *guest* values when the trampoline
//! is entered. Week-4 covers GPRs + RFLAGS:
//!
//! * the trampoline immediately spills all 16 GPRs and `RFLAGS` into a
//!   single static [`GuestRegs`] so the dispatcher can inspect or log
//!   them and a future patch can resume the guest with `vmresume`;
//! * the dispatcher then runs on the dedicated 16 KiB host stack with a
//!   clean SysV ABI — its own GPR clobbers are managed by the Rust
//!   compiler.
//!
//! ## Stable-Rust constraints
//!
//! We use `core::arch::global_asm!` rather than `#[naked]` because
//! `naked_functions` is still nightly-gated. `global_asm!` has been
//! stable since 1.59 and is sufficient: the trampoline is entered with
//! a CPU-owned `HOST_RSP` and we control every byte of the stack frame
//! it grows.

#![cfg(not(test))]

use crate::error::{HyperError, HyperResult};
use crate::logger;
use crate::vmx::fields as f;
use crate::vmx::vmcs;

// ---------------------------------------------------------------------------
// Host stack
// ---------------------------------------------------------------------------

/// 16-KiB host stack used as the landing pad for `HOST_RSP` on VM-exit.
///
/// `repr(C, align(16))` makes `&HOST_STACK[end]` already satisfy SysV's
/// 16-byte alignment requirement. We pick 16 KiB defensively — the
/// dispatcher itself only consumes a few hundred bytes, but a future
/// patch (Week-5: guest I/O bounce buffers) is expected to grow.
#[repr(C, align(16))]
struct HostStack([u8; 16 * 1024]);

static mut HOST_STACK: HostStack = HostStack([0u8; 16 * 1024]);

/// Top-of-stack pointer suitable for loading into `HOST_RSP`.
///
/// SysV AMD64 requires `(rsp + 8) % 16 == 0` at function entry, so we
/// hand back `top - 8`: the trampoline's first `call` pushes the 8-byte
/// return address and lands at a 16-aligned `rsp` inside Rust.
#[allow(static_mut_refs)] // we want a stable address, not a borrow
pub fn host_rsp_top() -> u64 {
    // SAFETY: `HOST_STACK` is a valid static; we compute an address,
    // never create a reference. The pointer is single-owner — only the
    // trampoline ever reads or writes through it.
    unsafe {
        let base = core::ptr::addr_of!(HOST_STACK) as *const u8;
        let top = base.add(core::mem::size_of::<HostStack>());
        (top as u64) - 8
    }
}

// ---------------------------------------------------------------------------
// Guest register snapshot
// ---------------------------------------------------------------------------

/// Snapshot of the 16 GPRs + RFLAGS captured by the trampoline at the
/// instant of VM-exit. Field layout is fixed by the asm below; do not
/// reorder without updating the spill sequence.
///
/// The snapshot lives in a single global rather than on the stack so the
/// trampoline does not have to allocate before it spills. That makes the
/// asm short enough to audit at a glance.
#[allow(missing_docs)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GuestRegs {
    pub rax: u64, pub rcx: u64, pub rdx: u64, pub rbx: u64,
    pub rsp: u64, pub rbp: u64, pub rsi: u64, pub rdi: u64,
    pub r8:  u64, pub r9:  u64, pub r10: u64, pub r11: u64,
    pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,
    pub rflags: u64,
}

static mut GUEST_REGS: GuestRegs = GuestRegs {
    rax: 0, rcx: 0, rdx: 0, rbx: 0,
    rsp: 0, rbp: 0, rsi: 0, rdi: 0,
    r8:  0, r9:  0, r10: 0, r11: 0,
    r12: 0, r13: 0, r14: 0, r15: 0,
    rflags: 0,
};

/// Address of the guest-register snapshot. The asm trampoline writes
/// through this pointer; Rust readers go through [`guest_regs`].
#[allow(static_mut_refs)]
fn guest_regs_addr() -> u64 {
    // `addr_of!` does not dereference, so this is sound without an
    // `unsafe` block.
    core::ptr::addr_of!(GUEST_REGS) as u64
}

/// Borrow the snapshot for reading. Safe because the trampoline writes
/// to `GUEST_REGS` only while the dispatcher is *not* running, and the
/// dispatcher only reads.
#[allow(static_mut_refs)]
pub fn guest_regs() -> GuestRegs {
    // SAFETY: single-writer (trampoline before `call dispatch`),
    // single-reader (dispatcher after `call`). The two never overlap
    // by construction.
    unsafe { GUEST_REGS }
}

// ---------------------------------------------------------------------------
// Trampoline
// ---------------------------------------------------------------------------

extern "C" {
    /// Symbol exported from the `global_asm!` block below. Loaded into
    /// `HOST_RIP` before `vmlaunch`.
    pub fn vmexit_trampoline();
}

// The trampoline runs with HOST_RSP fresh from the VMCS. GPRs still hold
// guest values; the very first thing we do is spill them into the
// `GUEST_REGS` static. We use `mov [{ptr} + N], rN` directly so no other
// register is clobbered before its content is preserved.
//
// Layout of `GUEST_REGS` (fixed by `#[repr(C)]` above):
//   +0x00 rax  +0x08 rcx  +0x10 rdx  +0x18 rbx
//   +0x20 rsp  +0x28 rbp  +0x30 rsi  +0x38 rdi
//   +0x40 r8   +0x48 r9   +0x50 r10  +0x58 r11
//   +0x60 r12  +0x68 r13  +0x70 r14  +0x78 r15
//   +0x80 rflags
//
// We use rax as a scratch pointer; its original value goes to slot 0
// before we clobber it.
core::arch::global_asm!(
    ".global vmexit_trampoline",
    "vmexit_trampoline:",
    // Save guest rax via the stack; we need a temp before we have a
    // pointer to GUEST_REGS in a register.
    "    push rax",
    "    mov  rax, qword ptr [rip + {regs_addr}]",  // rax = &GUEST_REGS
    // Slot 0: rax (popped from the just-pushed value)
    "    pop  qword ptr [rax + 0x00]",
    "    mov  qword ptr [rax + 0x08], rcx",
    "    mov  qword ptr [rax + 0x10], rdx",
    "    mov  qword ptr [rax + 0x18], rbx",
    // RSP at this point is host RSP, not guest. Guest RSP is recovered
    // from the VMCS by the dispatcher; we record host RSP for the log.
    "    mov  qword ptr [rax + 0x20], rsp",
    "    mov  qword ptr [rax + 0x28], rbp",
    "    mov  qword ptr [rax + 0x30], rsi",
    "    mov  qword ptr [rax + 0x38], rdi",
    "    mov  qword ptr [rax + 0x40], r8",
    "    mov  qword ptr [rax + 0x48], r9",
    "    mov  qword ptr [rax + 0x50], r10",
    "    mov  qword ptr [rax + 0x58], r11",
    "    mov  qword ptr [rax + 0x60], r12",
    "    mov  qword ptr [rax + 0x68], r13",
    "    mov  qword ptr [rax + 0x70], r14",
    "    mov  qword ptr [rax + 0x78], r15",
    // RFLAGS via pushfq.
    "    pushfq",
    "    pop  qword ptr [rax + 0x80]",
    // Pre-align rsp for SysV: the upcoming `call` will push 8 bytes.
    "    sub  rsp, 8",
    "    call {dispatch}",
    // Dispatcher is `-> !`. Defensive halt loop in case it ever changes.
    "    cli",
    "1:  hlt",
    "    jmp  1b",
    regs_addr = sym GUEST_REGS_PTR,
    dispatch  = sym vm_exit_dispatch,
);

// `GUEST_REGS_PTR` is a single 8-byte cell whose only job is to hold the
// runtime address of `GUEST_REGS`. We could `lea`-RIP the static
// directly, but `lea sym` against a `static mut` requires either
// `default` relocation visibility or `nightly` code-model knobs; using
// an `addr_of!`-initialised cell keeps the asm portable across
// `code-model = "kernel"` and `code-model = "small"` builds.
#[no_mangle]
static GUEST_REGS_PTR: GuestRegsPtr = GuestRegsPtr::new();

#[repr(transparent)]
struct GuestRegsPtr(core::cell::UnsafeCell<u64>);
unsafe impl Sync for GuestRegsPtr {}
impl GuestRegsPtr {
    const fn new() -> Self { Self(core::cell::UnsafeCell::new(0)) }
}

/// One-shot init: stamp the runtime address of `GUEST_REGS` into the
/// pointer cell the asm reads. Idempotent; called from `vm::bring_up`
/// before `vmlaunch`.
pub fn init_trampoline() {
    let addr = guest_regs_addr();
    // SAFETY: `GUEST_REGS_PTR` is only written here (single time, on
    // BSP, before any vCPU is brought up) and only read by the asm
    // trampoline after that point. No concurrent access exists.
    unsafe {
        core::ptr::write_volatile(GUEST_REGS_PTR.0.get(), addr);
    }
}

// ---------------------------------------------------------------------------
// Exit classification
// ---------------------------------------------------------------------------

/// Reasons we recognise. Add variants here as the dispatcher grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    /// Guest executed `HLT` (basic exit reason 12).
    Hlt,
    /// Guest accessed a CR (basic exit reason 28).
    CrAccess,
    /// EPT violation (basic exit reason 48).
    EptViolation,
    /// Any other exit reason; carries the basic reason code.
    Other(u32),
}

/// Read `EXIT_REASON` and classify it.
pub fn read_exit() -> HyperResult<ExitKind> {
    let raw = vmcs::vmread(f::EXIT_REASON)?;
    if raw & (1 << 31) != 0 {
        return Err(HyperError::Hardware("vm-exit on entry failure"));
    }
    let basic = (raw & 0xFFFF) as u32;
    Ok(match basic {
        f::EXIT_REASON_HLT           => ExitKind::Hlt,
        f::EXIT_REASON_CR_ACCESS     => ExitKind::CrAccess,
        f::EXIT_REASON_EPT_VIOLATION => ExitKind::EptViolation,
        other                        => ExitKind::Other(other),
    })
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Rust dispatcher invoked from the asm trampoline.
///
/// `extern "C"` for a stable ABI from asm; `-> !` because Week-4 has no
/// resume story yet — `vmresume` is wired in Week-5 once we have
/// per-vCPU state and an interrupt controller.
///
/// On a recognised exit reason we log a structured record:
///   * basic reason
///   * guest RIP at the time of the exit
///   * exit qualification (CR# for CR exits, GPA for EPT violations)
///   * a snapshot of `RAX` (the only GPR our hello blob touches)
///
/// On `Hlt` (the success path) we also emit the marker the integration
/// harness greps for: `"GUEST OK — Celium Guest Alive!"`.
#[no_mangle]
pub extern "C" fn vm_exit_dispatch() -> ! {
    let regs = guest_regs();
    let guest_rip   = vmcs::vmread(f::GUEST_RIP).unwrap_or(0);
    let qualif      = vmcs::vmread(f::EXIT_QUALIFICATION).unwrap_or(0);
    let raw_reason  = vmcs::vmread(f::EXIT_REASON).unwrap_or(0);
    let instr_err   = vmcs::vmread(f::VM_INSTRUCTION_ERROR).unwrap_or(0);

    logger::log_kv("vm_exit_reason_raw", raw_reason);
    logger::log_kv("vm_exit_basic",      raw_reason & 0xFFFF);
    logger::log_kv("vm_exit_entry_fail", (raw_reason >> 31) & 0x1);
    logger::log_kv("vm_instruction_err", instr_err);
    logger::log_kv("guest_rip",          guest_rip);
    logger::log_kv("exit_qualification", qualif);
    logger::log_kv("guest_rax",          regs.rax);
    logger::log_kv("guest_rflags",       regs.rflags);

    match read_exit() {
        Ok(ExitKind::Hlt) => {
            logger::log("celhyper: vm-exit HLT — guest halted normally");
            logger::log("celhyper: GUEST OK — Celium Guest Alive!");
            // Record the exit on the active VM so a future scheduler
            // tick can observe a terminal state rather than a stuck
            // `Running`.
            let _ = crate::sched::dispatch_exit(
                crate::vmx::fields::EXIT_REASON_HLT,
                crate::vm::ExitOutcome::Halted,
            );
        }
        Ok(ExitKind::CrAccess) => {
            logger::log("celhyper: vm-exit CR access (unhandled in week-5)");
            let _ = crate::sched::dispatch_exit(
                crate::vmx::fields::EXIT_REASON_CR_ACCESS,
                crate::vm::ExitOutcome::Faulted,
            );
        }
        Ok(ExitKind::EptViolation) => {
            logger::log("celhyper: vm-exit EPT violation (unhandled in week-5)");
            let _ = crate::sched::dispatch_exit(
                crate::vmx::fields::EXIT_REASON_EPT_VIOLATION,
                crate::vm::ExitOutcome::Faulted,
            );
        }
        Ok(ExitKind::Other(r)) => {
            logger::log_kv("vm_exit_other", u64::from(r));
            let _ = crate::sched::dispatch_exit(r, crate::vm::ExitOutcome::Faulted);
        }
        Err(_) => {
            logger::log("celhyper: vm-exit reason unreadable");
        }
    }

    // Stable termination marker for log-greppers.
    logger::log("celhyper: vm halted");
    crate::halt()
}
