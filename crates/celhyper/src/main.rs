//! Kernel binary entry point. CelLoader jumps here with `rdi = &CeliumHandoff`.
//!
//! Under `cfg(test)` the bin degenerates to a libtest-friendly stub so
//! `cargo test --target <host>` can exercise pure-logic submodules.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), forbid(unsafe_op_in_unsafe_fn))]

#[cfg(not(test))]
use celhyper::{bridge, bringup, handoff::CeliumHandoff, halt, logger};

#[cfg(not(test))]
#[no_mangle]
pub extern "C" fn _start(handoff_phys: *const CeliumHandoff) -> ! {
    // SAFETY: CelLoader contracts that `handoff_phys` points to a fully
    // initialised, properly-aligned `CeliumHandoff` valid for reads. The
    // `from_raw` helper validates magic/version before trusting the rest.
    let handoff = match unsafe { CeliumHandoff::from_raw(handoff_phys) } {
        Ok(h) => h,
        Err(_) => halt(),
    };

    logger::init_serial();
    logger::log("celhyper: alive");
    logger::log_kv("acpi_rsdp", handoff.acpi_rsdp_phys);
    logger::log_kv("kernel_phys", handoff.kernel_image_phys);

    if let Err(e) = bringup::bring_up(&handoff) {
        logger::log("celhyper: bring_up failed");
        let _ = e;
        halt()
    }

    // W22-B-2: bring_up returns once VMX is live and the runtime is
    // ready. From here we enter the kernel-side IPC bridge, which
    // owns COM1 RX and drives `manager::{create,start,stop,delete}_vm`
    // on behalf of the host control plane (`celmesh::SerialHyperLink`).
    bridge::run()
}

// Test build needs an entry symbol of some shape. libtest provides its
// own main when this is empty; keeping the function present satisfies the
// linker for non-`no_main` test configurations.
#[cfg(test)]
fn main() {}
