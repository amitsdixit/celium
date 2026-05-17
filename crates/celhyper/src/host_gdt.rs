//! Boot-time host TSS installation.
//!
//! UEFI boots us with a perfectly serviceable 64-bit GDT but **no TSS**,
//! so `str` returns 0 — and SDM §26.2.3 forbids `HOST_TR=0` at VM entry,
//! which yields VMfailValid #8 ("invalid host-state field").
//!
//! Strategy: leave the UEFI GDT alone. Allocate a *new* GDT that
//! mirrors UEFI's first N descriptors and appends a 64-bit TSS
//! descriptor at a fresh offset. `lgdt` the new one and `ltr` the new
//! selector. No CS/DS/SS reload needed because the UEFI selectors
//! still resolve to identical descriptors at the same offsets.

#![cfg(not(test))]

/// Architectural 64-bit Task State Segment (TSS).
///
/// We only populate `rsp0` (used as the stack on a ring-transition
/// triggered by a VM-exit trampoline) and `iomap_base` (set to
/// `sizeof(Tss64)` so no I/O permission bitmap is consulted). All
/// other fields are explicitly zero.
#[repr(C, packed)]
pub struct Tss64 {
    /// Reserved, must be zero.
    _reserved0: u32,
    /// Stack pointer used on transitions to ring 0.
    rsp0:    u64,
    /// Stack pointer used on transitions to ring 1 (unused).
    rsp1:    u64,
    /// Stack pointer used on transitions to ring 2 (unused).
    rsp2:    u64,
    /// Reserved, must be zero.
    _reserved1:  u64,
    /// Interrupt-Stack-Table entries 1..=7 (unused).
    ist:     [u64; 7],
    /// Reserved, must be zero.
    _reserved2:  u64,
    /// Reserved, must be zero.
    _reserved3:  u16,
    /// Offset of the I/O permission bitmap from the TSS base. We set
    /// this to `sizeof(Tss64)` to indicate "no bitmap present".
    iomap_base: u16,
}

#[repr(C, packed)]
struct GdtPtr {
    limit: u16,
    base:  u64,
}

const NEW_GDT_BYTES: usize = 256;
// TSS descriptor occupies the last 16 bytes of the new GDT.
const TSS_OFFSET: u16 = (NEW_GDT_BYTES as u16) - 16;

#[repr(C, align(8))]
struct GdtBuf([u8; NEW_GDT_BYTES]);
static mut NEW_GDT: GdtBuf = GdtBuf([0; NEW_GDT_BYTES]);

// 16 KiB host stack used as TSS.RSP0 — the vm-exit trampoline lands here.
#[repr(C, align(16))]
struct Stack16K([u8; 16 * 1024]);
static mut HOST_STACK: Stack16K = Stack16K([0; 16 * 1024]);

static mut TSS: Tss64 = Tss64 {
    _reserved0: 0,
    rsp0: 0, rsp1: 0, rsp2: 0,
    _reserved1: 0,
    ist: [0; 7],
    _reserved2: 0,
    _reserved3: 0,
    iomap_base: core::mem::size_of::<Tss64>() as u16,
};

fn build_tss_descriptor(tss_addr: u64, tss_size: u32) -> (u64, u64) {
    let limit = tss_size - 1;
    let limit_lo = u64::from(limit & 0xFFFF);
    let limit_hi = u64::from((limit >> 16) & 0xF);
    let base_lo16 = (tss_addr & 0xFFFF) << 16;
    let base_mid8 = ((tss_addr >> 16) & 0xFF) << 32;
    let base_hi8  = ((tss_addr >> 24) & 0xFF) << 56;
    // Type 9 = available 64-bit TSS, S=0, DPL=0, P=1 → 0x89 in byte 5.
    let access = 0x89u64 << 40;
    let low  = limit_lo | base_lo16 | base_mid8 | access | (limit_hi << 48) | base_hi8;
    let high = (tss_addr >> 32) & 0xFFFF_FFFF;
    (low, high)
}

/// Install a TSS using a cloned-from-UEFI GDT.
///
/// # Safety
/// Must be called once at boot, CPL 0, single-threaded.
pub unsafe fn install() {
    // SAFETY: single-threaded boot.
    unsafe {
        // 1. sgdt the current (UEFI) GDT.
        let mut cur = GdtPtr { limit: 0, base: 0 };
        core::arch::asm!(
            "sgdt [{p}]",
            p = in(reg) &mut cur,
            options(nostack, preserves_flags),
        );
        let cur_limit = cur.limit;
        let cur_base  = cur.base;
        crate::logger::log_kv("uefi_gdt_base",  cur_base);
        crate::logger::log_kv("uefi_gdt_limit", u64::from(cur_limit));

        // 2. Copy the UEFI GDT verbatim into NEW_GDT.
        let copy_len = core::cmp::min(usize::from(cur_limit) + 1, NEW_GDT_BYTES - 16);
        let dst = (&raw mut NEW_GDT) as *mut u8;
        let src = cur_base as *const u8;
        for i in 0..copy_len {
            dst.add(i).write_volatile(src.add(i).read_volatile());
        }

        // 3. Initialise TSS (set RSP0 to top of HOST_STACK).
        let stack_top = ((&raw const HOST_STACK) as u64).wrapping_add(16 * 1024);
        let tss_p = &raw mut TSS;
        // Field-wise writes to avoid moving an unaligned packed struct.
        (*tss_p).rsp0 = stack_top;
        (*tss_p).iomap_base = core::mem::size_of::<Tss64>() as u16;

        // 4. Build TSS descriptor and write at NEW_GDT[TSS_OFFSET..+16].
        let tss_addr = tss_p as u64;
        let tss_size = core::mem::size_of::<Tss64>() as u32;
        let (lo, hi) = build_tss_descriptor(tss_addr, tss_size);
        let tss_slot = dst.add(usize::from(TSS_OFFSET)) as *mut u64;
        tss_slot.write_unaligned(lo);
        tss_slot.add(1).write_unaligned(hi);

        crate::logger::log_kv("new_gdt_base",  dst as u64);
        crate::logger::log_kv("tss_addr",      tss_addr);
        crate::logger::log_kv("tss_offset",    u64::from(TSS_OFFSET));

        // 5. Lgdt the new table.
        let new_ptr = GdtPtr {
            limit: (NEW_GDT_BYTES - 1) as u16,
            base:  dst as u64,
        };
        core::arch::asm!(
            "lgdt [{p}]",
            p = in(reg) &new_ptr,
            options(nostack, preserves_flags),
        );

        // 6. Ltr the TSS selector. RPL=0, TI=0 ⇒ selector == TSS_OFFSET.
        core::arch::asm!(
            "ltr {sel:x}",
            sel = in(reg) TSS_OFFSET,
            options(nostack, preserves_flags),
        );
    }
}
