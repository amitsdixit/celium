//! # CelLoader — Celium UEFI stage-0
//!
//! Spec: `docs/01_CELHYPER.md` § 1.1.
//!
//! Stage-0 lifecycle:
//!
//! 1. `uefi::helpers::init` — get a console, allocator, and configuration
//!    table view.
//! 2. Discover hardware (CPUID + ACPI RSDP).
//! 3. Load the CelHyper ELF image from `\EFI\CELIUM\CELHYPER.ELF`.
//! 4. Build a [`handoff::CeliumHandoff`] block in a leaked allocation that
//!    survives `ExitBootServices`.
//! 5. Call `ExitBootServices` to take ownership of the machine.
//! 6. Jump to the kernel entry with `rdi = &handoff` (System V x86_64 ABI).
//!
//! The whole binary stays under 64 KiB in `--release`.
//!
//! # Safety
//!
//! Stage-0 runs before any operating system. Every fallible call is `?`-ed.
//! We never `unwrap`. The two unsafe regions are:
//!
//! * publishing the handoff allocation as `'static` so it outlives Boot
//!   Services teardown; and
//! * the final `jmp` trampoline that transfers control to the kernel.
//!
//! Both have explicit `// SAFETY:` notes at the call site.

#![no_std]
#![no_main]
#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(rust_2018_idioms, missing_docs)]

extern crate alloc;

mod handoff;
mod hardware;
mod image_loader;

use alloc::boxed::Box;
use uefi::prelude::*;
use uefi::println;

/// UEFI entry point.
#[entry]
fn efi_main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::LOAD_ERROR;
    }

    println!("[celloader] Celium stage-0 starting");

    match run() {
        Ok(()) => {
            // run() only returns Ok in the dry-run path (Week-1 behaviour
            // preserved for QEMU+OVMF smoke); production path never reaches
            // this — control is handed to CelHyper inside run().
            println!("[celloader] dry-run complete (kernel handoff stub)");
            Status::SUCCESS
        }
        Err(s) => {
            println!("[celloader] fatal: {s:?}");
            s
        }
    }
}

/// Real entry logic. Kept separate from `efi_main` so each step can use `?`.
fn run() -> Result<(), Status> {
    // 1. Hardware discovery
    let cpu = hardware::probe_cpu().map_err(|_| Status::UNSUPPORTED)?;
    println!(
        "[celloader] cpu: vendor={} vmx={} svm={} x2apic={}",
        cpu.vendor_str(),
        cpu.vmx,
        cpu.svm,
        cpu.x2apic
    );
    if !cpu.has_virtualization() {
        println!("[celloader] no VMX/SVM — CelHyper requires Intel VT-x or AMD-V");
        return Err(Status::UNSUPPORTED);
    }

    let acpi_rsdp = hardware::find_acpi_rsdp();
    match acpi_rsdp {
        Some(p) => println!("[celloader] acpi rsdp @ {p:#x}"),
        None    => println!("[celloader] WARN: no ACPI RSDP located"),
    }

    // 2. Load CelHyper from the ESP, then parse + relocate it into a
    //    fresh page-aligned region so absolute jumps and RIP-relative
    //    relocations all resolve correctly.
    let image = image_loader::load_celhyper().map_err(|_| Status::NOT_FOUND)?;
    println!(
        "[celloader] celhyper image loaded: {} bytes",
        image.bytes.len()
    );
    let loaded = image_loader::load_and_relocate(&image.bytes).map_err(|_| Status::LOAD_ERROR)?;
    println!(
        "[celloader] celhyper relocated: base={:#x} size={:#x} entry={:#x}",
        loaded.base, loaded.size, loaded.entry
    );

    // 3. Build handoff in a Box and leak it so the pointer stays valid
    //    after ExitBootServices.
    let handoff = handoff::CeliumHandoff::new(
        cpu,
        acpi_rsdp.unwrap_or(0),
        loaded.base,
        loaded.size,
    );
    let handoff_ptr: *const handoff::CeliumHandoff = Box::leak(Box::new(handoff));
    println!(
        "[celloader] handoff @ {:p}: magic={:#x} version={}",
        handoff_ptr,
        handoff::MAGIC,
        handoff::VERSION
    );

    // 4. Kernel entry comes from the relocator above.
    let entry = loaded.entry;
    println!("[celloader] kernel entry @ {entry:#x}");

    // 5. Hand off. In Week-2 builds we *do not* call exit_boot_services
    //    yet — doing so before the kernel can safely run on its own page
    //    tables would brick a real machine. The plumbing below is wired,
    //    documented, and gated behind a feature flag for QEMU experiments.
    #[cfg(feature = "real-handoff")]
    return real_handoff(entry, handoff_ptr);

    #[cfg(not(feature = "real-handoff"))]
    {
        println!("[celloader] handoff dry-run (build with --features real-handoff to jump)");
        let _ = entry;
        let _ = handoff_ptr;
        Ok(())
    }
}

/// Production handoff: exit boot services and jump to CelHyper.
///
/// Gated behind the `real-handoff` feature so the default `cargo build`
/// remains observable end-to-end without bricking a host. Wired against
/// `uefi::boot::exit_boot_services`, which performs the
/// "GetMemoryMap → ExitBootServices" dance internally.
#[cfg(feature = "real-handoff")]
fn real_handoff(
    entry: u64,
    handoff_ptr: *const handoff::CeliumHandoff,
) -> Result<(), Status> {
    use uefi::table::boot::MemoryType;

    println!("[celloader] calling ExitBootServices");

    // SAFETY: from this point Boot Services are gone — no allocations, no
    // console output, no protocol calls. We must immediately jump to the
    // kernel. The returned `MemoryMapOwned` is intentionally dropped so
    // its allocation (made internally by `exit_boot_services`) is leaked
    // alongside the handoff.
    let _mmap = unsafe { uefi::boot::exit_boot_services(MemoryType::LOADER_DATA) };

    // SAFETY: System V AMD64 calling convention places the first integer
    // argument in `rdi`. We jump rather than call so the kernel sees a
    // clean stack frame; it never returns through this path.
    unsafe {
        core::arch::asm!(
            "jmp {target}",
            target = in(reg) entry,
            in("rdi") handoff_ptr,
            options(noreturn),
        );
    }
}
