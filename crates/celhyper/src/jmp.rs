//! Minimal `setjmp`/`longjmp` pair used as a return path from the
//! VMX exit dispatcher.
//!
//! ## Why
//!
//! `vmlaunch` is *not* a regular function call: control jumps to the
//! guest, and the next time the host sees the CPU is on a VM-exit, with
//! `HOST_RSP` freshly loaded from the VMCS. That puts the dispatcher on
//! a brand-new stack with no link back to whoever called
//! [`crate::vmx::launch::vmlaunch`].
//!
//! Without a return path the dispatcher must `halt()` after every
//! guest exit — bringup can only ever launch *one* VM, the kernel
//! IPC bridge ([`crate::bridge`]) never runs, and there is no way to
//! re-enter the guest. That is exactly the limitation we lift here.
//!
//! ## Contract
//!
//! `setjmp(buf)` saves the SysV callee-saved registers (`rbx`, `rbp`,
//! `r12`..`r15`) together with the caller's stack pointer and return
//! address, then returns `0` on the first call. A subsequent
//! `longjmp(buf, val)` from any context that still has `buf` reachable
//! restores those registers and resumes execution as if `setjmp` had
//! returned `val` (≠ 0 by convention — a value of 0 is rewritten to 1
//! to keep the "ever returned non-zero?" idiom usable).
//!
//! `JmpBuf` is `#[repr(C)]` so the offsets are stable for the asm
//! below; do not reorder.
//!
//! ## Safety
//!
//! `longjmp` is fundamentally unsafe: it unwinds the stack without
//! running drops, and the caller must guarantee that the frame pointed
//! to by `buf` is still live (i.e. the `setjmp` caller has not
//! returned). In CelHyper the only consumer is
//! [`crate::manager::start_vm`], which calls `setjmp`, issues
//! `vmlaunch` from the same frame, and stays in that frame until the
//! dispatcher `longjmp`s back. The frame cannot be popped underneath
//! us because `vmlaunch` does not return through it on success.

#![cfg(not(test))]

use core::cell::UnsafeCell;

/// Saved register window. Field layout is locked by the asm below.
///
/// Slot 0..=5: callee-saved GPRs. Slot 6: stack pointer at the
/// `setjmp` return point (immediately *above* the pushed return
/// address). Slot 7: return RIP.
#[repr(C, align(16))]
#[derive(Default, Clone, Copy)]
pub struct JmpBuf {
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rsp: u64,
    rip: u64,
}

// `JMP_BUF` is the rendezvous between `setjmp` (writer) and
// `longjmp` (reader). It is touched only from a single CPU during the
// boot path — multi-core scheduling refactors will move it per-pCPU
// alongside the active-VM slot in `sched`.
struct SyncJmpBuf(UnsafeCell<JmpBuf>);
// SAFETY: see module docs — single-CPU, single-writer, single-reader.
unsafe impl Sync for SyncJmpBuf {}
impl SyncJmpBuf {
    const fn new() -> Self {
        Self(UnsafeCell::new(JmpBuf {
            rbx: 0, rbp: 0, r12: 0, r13: 0,
            r14: 0, r15: 0, rsp: 0, rip: 0,
        }))
    }
}

static JMP_BUF: SyncJmpBuf = SyncJmpBuf::new();

/// Address of the singleton [`JmpBuf`] used by the VMX exit
/// dispatcher. Stored in `JMP_BUF_PTR` so the global asm below can
/// take its address without depending on a particular code model.
#[inline]
fn jmp_buf_addr() -> u64 {
    JMP_BUF.0.get() as u64
}

/// Pointer cell read by the `celhyper_setjmp` / `celhyper_longjmp`
/// asm to locate [`JMP_BUF`]. Stamped once by [`init`].
#[no_mangle]
static JMP_BUF_PTR: JmpBufPtr = JmpBufPtr::new();

#[repr(transparent)]
struct JmpBufPtr(UnsafeCell<u64>);
// SAFETY: written exactly once on the BSP boot path before any reader.
unsafe impl Sync for JmpBufPtr {}
impl JmpBufPtr {
    const fn new() -> Self {
        Self(UnsafeCell::new(0))
    }
}

/// Stamp the runtime address of [`JMP_BUF`] into the pointer cell the
/// asm uses. Idempotent and re-entrant — call once per boot before
/// any [`crate::manager::start_vm`] invocation.
pub fn init() {
    let addr = jmp_buf_addr();
    // SAFETY: single writer on the BSP boot path; the asm only reads.
    unsafe {
        core::ptr::write_volatile(JMP_BUF_PTR.0.get(), addr);
    }
}

extern "C" {
    /// Save `RBX`, `RBP`, `R12`..`R15`, the caller's `RSP` (above the
    /// pushed return address) and the return RIP into [`JMP_BUF`].
    /// Returns `0` on the save path.
    fn celhyper_setjmp() -> u64;

    /// Restore the window saved by [`celhyper_setjmp`] and resume
    /// execution at the saved RIP with `RAX = val` (or `1` if
    /// `val == 0`).
    fn celhyper_longjmp(val: u64) -> !;
}

/// Save the current callee-saved register window and the caller's
/// return point. Returns `0` on the initial save call and `val` (or
/// `1` if `val` was 0) when a subsequent [`longjmp`] resumes here.
///
/// # Safety
///
/// The window stored in [`JMP_BUF`] is *only* valid as long as the
/// calling stack frame is still live. The caller must not return,
/// unwind, or otherwise pop its frame between this call and a
/// matching [`longjmp`].
#[inline(always)]
pub unsafe fn setjmp() -> u64 {
    // SAFETY: asm wrapper; preconditions documented above. The asm
    // writes through the address cached in `JMP_BUF_PTR`, which is
    // stamped by `init` during bring-up before any `start_vm` runs.
    unsafe { celhyper_setjmp() }
}

/// Unwind to the last [`setjmp`] call site, returning `val` from it.
///
/// # Safety
///
/// Caller must guarantee that the frame paired with the most recent
/// [`setjmp`] has not been popped. Calling `longjmp` without a
/// matching live `setjmp` will jump to whatever address happens to be
/// in [`JMP_BUF::rip`], which is undefined behaviour.
#[inline(always)]
pub unsafe fn longjmp(val: u64) -> ! {
    // SAFETY: see function-level note. The asm clobbers `rax` to hold
    // the resumed return value and then transfers control via `jmp`.
    unsafe { celhyper_longjmp(val) }
}

// ---------------------------------------------------------------------------
// Asm implementation
// ---------------------------------------------------------------------------
//
// `celhyper_setjmp` (no args):
//   rax := &JMP_BUF
//   spill rbx, rbp, r12..r15 into slots 0..=5
//   rcx := rsp + 8     ; caller's rsp (above pushed retaddr)
//   slot 6 := rcx
//   rcx := [rsp]       ; saved return RIP
//   slot 7 := rcx
//   rax := 0           ; first-time return value
//   ret
//
// `celhyper_longjmp` (val in rdi):
//   rax := &JMP_BUF
//   rbx, rbp, r12..r15 := slots 0..=5
//   rsp := slot 6
//   rcx := slot 7
//   rax := rdi
//   test rax, rax       ; if val == 0 → return 1
//   jnz  1f
//   mov  rax, 1
// 1:
//   jmp  rcx
core::arch::global_asm!(
    ".global celhyper_setjmp",
    "celhyper_setjmp:",
    "    mov  rax, qword ptr [rip + {ptr}]",
    "    mov  qword ptr [rax + 0x00], rbx",
    "    mov  qword ptr [rax + 0x08], rbp",
    "    mov  qword ptr [rax + 0x10], r12",
    "    mov  qword ptr [rax + 0x18], r13",
    "    mov  qword ptr [rax + 0x20], r14",
    "    mov  qword ptr [rax + 0x28], r15",
    "    lea  rcx, [rsp + 8]",
    "    mov  qword ptr [rax + 0x30], rcx",
    "    mov  rcx, qword ptr [rsp]",
    "    mov  qword ptr [rax + 0x38], rcx",
    "    xor  eax, eax",
    "    ret",
    "",
    ".global celhyper_longjmp",
    "celhyper_longjmp:",
    "    mov  rax, qword ptr [rip + {ptr}]",
    "    mov  rbx, qword ptr [rax + 0x00]",
    "    mov  rbp, qword ptr [rax + 0x08]",
    "    mov  r12, qword ptr [rax + 0x10]",
    "    mov  r13, qword ptr [rax + 0x18]",
    "    mov  r14, qword ptr [rax + 0x20]",
    "    mov  r15, qword ptr [rax + 0x28]",
    "    mov  rsp, qword ptr [rax + 0x30]",
    "    mov  rcx, qword ptr [rax + 0x38]",
    "    mov  rax, rdi",
    "    test rax, rax",
    "    jnz  1f",
    "    mov  rax, 1",
    "1:",
    "    cld",
    "    jmp  rcx",
    ptr = sym JMP_BUF_PTR,
);
