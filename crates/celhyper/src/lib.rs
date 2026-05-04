//! # CelHyper — Celium micro-hypervisor (bare-metal core)
//!
//! Spec: `docs/01_CELHYPER.md`. The crate is `no_std` *except* under
//! `cfg(test)`, where the host's `std` is permitted so pure-logic submodules
//! (notably the EPT walker) can be unit-tested on the dev box.
//!
//! Four hard responsibilities, four submodules:
//!
//! 1. Second-level page tables (EPT/NPT) → [`mm`].
//! 2. vCPU scheduling                    → [`sched`].
//! 3. IOMMU programming                  → [`iommu`].
//! 4. Capability-based IPC               → [`cap`].
//!
//! VMX bring-up + VMCS state machine live under [`vmx`]; the boot path
//! that drives them is [`bringup`]; the per-guest lifecycle type is
//! [`vm::Vm`].

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), forbid(unsafe_op_in_unsafe_fn))]
#![warn(rust_2018_idioms, missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_safety_doc,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

pub mod arch;
pub mod cap;
pub mod error;
pub mod guest;
pub mod handoff;
pub mod iommu;
pub mod logger;
pub mod mm;
pub mod sched;
pub mod vm;
pub mod vmx;

#[cfg(not(test))]
pub mod bringup;
#[cfg(not(test))]
pub mod host_gdt;
#[cfg(not(test))]
pub mod manager;

pub use error::{HyperError, HyperResult};

/// Disable interrupts and `hlt` forever. Bare-metal final resting state.
#[cfg(not(test))]
#[cold]
#[inline(never)]
pub fn halt() -> ! {
    loop {
        // SAFETY: `cli; hlt` is always defined on x86_64 in long mode at CPL 0.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(not(test))]
#[panic_handler]
fn on_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    logger::log("celhyper: PANIC");
    let _ = info;
    halt()
}
