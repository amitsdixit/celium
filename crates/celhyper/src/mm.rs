//! Memory management: host paging + second-level (EPT) tables.
//!
//! ## Week-2 deliverable
//!
//! A real, recursive 4-level **Extended Page Table** walker for Intel VT-x.
//! Tables are allocated lazily through the [`FrameProvider`] trait, so the
//! same code paths run unmodified in:
//!
//! * the kernel (`KernelFrames` — bump pool over a `.bss` reservation,
//!   identity-mapped while the host's CR3 is the boot page table); and
//! * host-target unit tests (`tests::TestFrames` — backing pages live in a
//!   `HashMap<PA, Box<[u64; 512]>>` so the walker logic is exercisable on a
//!   dev box without VT-x hardware).
//!
//! Layout reference: Intel SDM Vol 3 §28.2 ("EPT Translation Mechanism").
//! Entry format we use:
//!
//! | bits  | meaning                                         |
//! |-------|-------------------------------------------------|
//! | 2:0   | R / W / X (must be set on present non-leaf)     |
//! | 5:3   | leaf memory type (6 = WB)                       |
//! | 6     | leaf "ignore PAT"                               |
//! | 7     | leaf-vs-table flag (0 for the 4 KiB leaf walker)|
//! | 51:12 | PA of next table or final 4 KiB frame           |

use bitflags::bitflags;
#[cfg(not(test))]
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{HyperError, HyperResult};

pub use x86_64::PhysAddr;
pub use x86_64::VirtAddr;

/// Size of a 4 KiB page.
pub const PAGE_SIZE: usize = 4096;

/// Number of 8-byte entries in an EPT/PT page (also PML4/PDPT/PD).
pub const ENTRIES_PER_TABLE: usize = 512;

/// Mask isolating bits 12..52 — the physical-address field of a leaf or
/// non-leaf EPT entry.
pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// `R | W | X`. A non-leaf entry must have all three set or it is treated
/// as not-present by the hardware walker.
pub const NON_LEAF_RWX: u64 = 0b111;

bitflags! {
    /// EPT page-table-entry flags (4 KiB leaf form). See module docs for
    /// the bit assignment.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct EptFlags: u64 {
        /// Guest may read.
        const READ       = 1 << 0;
        /// Guest may write.
        const WRITE      = 1 << 1;
        /// Guest may execute.
        const EXEC       = 1 << 2;
        /// Memory type = write-back.
        const MT_WB      = 6 << 3;
        /// Memory type = uncacheable.
        const MT_UC      = 0 << 3;
        /// Ignore PAT (treat the leaf's MT field as authoritative).
        const IGNORE_PAT = 1 << 6;
    }
}

/// A frame allocator + page-table-frame access provider.
///
/// We bundle "give me a fresh zero page" and "let me read/write entry N of
/// page X" into one trait so the EPT walker can stay loosely coupled to
/// physical-memory management. The kernel impl assumes the host CR3 has
/// installed an identity map over the bump pool; the test impl uses an
/// arena.
///
/// All entry indices are in `0..512`; callers (the walker) guarantee this.
pub trait FrameProvider {
    /// Allocate one zeroed 4 KiB frame. Returns the physical address.
    fn alloc_zeroed(&mut self) -> HyperResult<PhysAddr>;

    /// Read the `idx`'th `u64` of the page at `pa`.
    fn read_entry(&self, pa: PhysAddr, idx: usize) -> u64;

    /// Write `val` into the `idx`'th `u64` of the page at `pa`.
    fn write_entry(&mut self, pa: PhysAddr, idx: usize, val: u64);
}

/// A guest's 4-level Extended Page Table root.
#[derive(Debug)]
pub struct Ept {
    pml4: PhysAddr,
}

impl Ept {
    /// Allocate a fresh, empty PML4.
    pub fn new<P: FrameProvider>(p: &mut P) -> HyperResult<Self> {
        let pml4 = p.alloc_zeroed()?;
        Ok(Self { pml4 })
    }

    /// Physical address of the PML4. Suitable for the EPTP[51:12] field.
    #[must_use]
    pub fn pml4(&self) -> PhysAddr {
        self.pml4
    }

    /// Encoded EPTP value to load into VMCS:
    /// `MT=WB(6) | walk_length=4 (3<<3) | PML4 PA`.
    /// Accessed/dirty bits stay disabled for v0.1.
    #[must_use]
    pub fn eptp(&self) -> u64 {
        const WALK_LEN_4: u64 = 3 << 3;
        const MT_WB: u64 = 6;
        self.pml4.as_u64() | WALK_LEN_4 | MT_WB
    }

    /// Map a single 4 KiB guest physical page to a host physical page.
    ///
    /// `gpa` and `hpa` must each be 4 KiB-aligned. Intermediate tables are
    /// allocated on demand through `p`. Re-mapping an existing leaf is
    /// allowed and overwrites the entry.
    pub fn map_4k<P: FrameProvider>(
        &mut self,
        p: &mut P,
        gpa: PhysAddr,
        hpa: PhysAddr,
        flags: EptFlags,
    ) -> HyperResult<()> {
        if gpa.as_u64() & 0xFFF != 0 || hpa.as_u64() & 0xFFF != 0 {
            return Err(HyperError::Hardware("ept::map_4k: misaligned GPA or HPA"));
        }
        if !flags.intersects(EptFlags::READ | EptFlags::WRITE | EptFlags::EXEC) {
            return Err(HyperError::Hardware("ept::map_4k: leaf must grant some access"));
        }

        let idx = ept_indices(gpa);
        // Walk PML4 (level 3) → PDPT (2) → PD (1), allocating as we go.
        let mut table = self.pml4;
        for level in (1..=3).rev() {
            let i = idx[level];
            let entry = p.read_entry(table, i);
            let next = if entry_is_present(entry) {
                PhysAddr::new(entry & ADDR_MASK)
            } else {
                let new_table = p.alloc_zeroed()?;
                p.write_entry(table, i, new_table.as_u64() | NON_LEAF_RWX);
                #[cfg(not(test))]
                crate::metrics::count_ept_table_alloc();
                new_table
            };
            table = next;
        }
        // Leaf at PT (level 0).
        let leaf = (hpa.as_u64() & ADDR_MASK) | flags.bits();
        p.write_entry(table, idx[0], leaf);
        #[cfg(not(test))]
        crate::metrics::count_ept_map_4k();
        Ok(())
    }

    /// Translate a guest physical address through this EPT.
    ///
    /// Returns the corresponding host physical address (preserving the
    /// 12-bit page offset), or [`HyperError::Hardware`] if any walk step
    /// is absent.
    pub fn translate<P: FrameProvider>(&self, p: &P, gpa: PhysAddr) -> HyperResult<PhysAddr> {
        let idx = ept_indices(gpa);
        let mut table = self.pml4;
        for level in (1..=3).rev() {
            let entry = p.read_entry(table, idx[level]);
            if !entry_is_present(entry) {
                return Err(HyperError::Hardware("ept::translate: walk hit absent non-leaf"));
            }
            table = PhysAddr::new(entry & ADDR_MASK);
        }
        let leaf = p.read_entry(table, idx[0]);
        if leaf & 0b111 == 0 {
            return Err(HyperError::Hardware("ept::translate: leaf absent"));
        }
        let off = gpa.as_u64() & 0xFFF;
        Ok(PhysAddr::new((leaf & ADDR_MASK) | off))
    }
}

/// `true` for any entry that has at least one of R/W/X set.
#[inline]
fn entry_is_present(entry: u64) -> bool {
    entry & 0b111 != 0
}

/// Decompose `gpa` into `[PT, PD, PDPT, PML4]` indices. Returning a 4-array
/// with PT at index 0 lets the walker iterate `(1..=3).rev()` for the
/// non-leaf levels and use `idx[0]` for the leaf.
#[inline]
fn ept_indices(gpa: PhysAddr) -> [usize; 4] {
    let a = gpa.as_u64();
    [
        ((a >> 12) & 0x1FF) as usize,
        ((a >> 21) & 0x1FF) as usize,
        ((a >> 30) & 0x1FF) as usize,
        ((a >> 39) & 0x1FF) as usize,
    ]
}

// ---------- Kernel-side FrameProvider --------------------------------------

/// 16 MiB scratch frame pool used during boot. Backed by `.bss`, so it is
/// zero-initialised by the loader/kernel image. Replaced in Week-3 by a
/// real allocator seeded from the UEFI memory map.
#[cfg(not(test))]
const POOL_PAGES: usize = 4096;

#[cfg(not(test))]
#[repr(C, align(4096))]
struct FramePool([u8; POOL_PAGES * PAGE_SIZE]);

#[cfg(not(test))]
static mut FRAME_POOL: FramePool = FramePool([0; POOL_PAGES * PAGE_SIZE]);

#[cfg(not(test))]
static FRAME_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Bare-metal `FrameProvider` backed by [`FRAME_POOL`].
///
/// Assumes the host CR3 currently identity-maps the pool (true under UEFI's
/// 1:1 boot map and remains true while CelHyper has not yet installed its
/// own page tables).
#[cfg(not(test))]
pub struct KernelFrames;

#[cfg(not(test))]
impl FrameProvider for KernelFrames {
    fn alloc_zeroed(&mut self) -> HyperResult<PhysAddr> {
        let off = FRAME_NEXT.fetch_add(PAGE_SIZE, Ordering::SeqCst);
        if off + PAGE_SIZE > POOL_PAGES * PAGE_SIZE {
            return Err(HyperError::Exhausted("frame pool"));
        }
        // SAFETY: distinct, in-range, page-aligned slice; bytes start zero
        // (.bss) and the bump allocator never hands the same range out twice.
        let pa = unsafe { (&raw const FRAME_POOL.0[off]) as u64 };
        Ok(PhysAddr::new(pa))
    }

    fn read_entry(&self, pa: PhysAddr, idx: usize) -> u64 {
        debug_assert!(idx < ENTRIES_PER_TABLE);
        let ptr = pa.as_u64() as *const u64;
        // SAFETY: identity-mapped pool, ptr is 8-byte aligned because pa is
        // 4 KiB-aligned and idx is in range; volatile read prevents the
        // compiler from caching across vmlaunch boundaries.
        unsafe { core::ptr::read_volatile(ptr.add(idx)) }
    }

    fn write_entry(&mut self, pa: PhysAddr, idx: usize, val: u64) {
        debug_assert!(idx < ENTRIES_PER_TABLE);
        let ptr = pa.as_u64() as *mut u64;
        // SAFETY: see read_entry.
        unsafe { core::ptr::write_volatile(ptr.add(idx), val) }
    }
}

// ---------- Host-target unit tests -----------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box;
    use std::collections::HashMap;

    /// Off-target `FrameProvider` — keeps each allocated 4 KiB page in a
    /// `Box<[u64; 512]>` and looks them up by PA. Lets us exercise the EPT
    /// walker on the host without touching real physical memory.
    struct TestFrames {
        next: u64,
        frames: HashMap<u64, Box<[u64; 512]>>,
    }

    impl TestFrames {
        fn new() -> Self {
            // Start past the conventional zero page so PA == 0 stays sentinel.
            Self {
                next: 0x1_000,
                frames: HashMap::new(),
            }
        }
    }

    impl FrameProvider for TestFrames {
        fn alloc_zeroed(&mut self) -> HyperResult<PhysAddr> {
            let pa = self.next;
            self.next = self
                .next
                .checked_add(PAGE_SIZE as u64)
                .ok_or(HyperError::Exhausted("test arena"))?;
            self.frames.insert(pa, Box::new([0u64; 512]));
            Ok(PhysAddr::new(pa))
        }

        fn read_entry(&self, pa: PhysAddr, idx: usize) -> u64 {
            self.frames
                .get(&pa.as_u64())
                .expect("read of unallocated frame")[idx]
        }

        fn write_entry(&mut self, pa: PhysAddr, idx: usize, val: u64) {
            self.frames
                .get_mut(&pa.as_u64())
                .expect("write of unallocated frame")[idx] = val;
        }
    }

    fn rwx_wb() -> EptFlags {
        EptFlags::READ | EptFlags::WRITE | EptFlags::EXEC | EptFlags::MT_WB
    }

    #[test]
    fn indices_are_canonical() {
        let a = (5u64 << 39) | (4u64 << 30) | (3u64 << 21) | (2u64 << 12) | 0x111;
        let idx = ept_indices(PhysAddr::new(a));
        assert_eq!(idx, [2, 3, 4, 5]);
    }

    #[test]
    fn map_then_translate_4k_round_trips_the_offset() {
        let mut f = TestFrames::new();
        let mut e = Ept::new(&mut f).expect("alloc pml4");

        let gpa = PhysAddr::new(0x0000_dead_b000);
        let hpa = PhysAddr::new(0x0000_beef_a000);
        e.map_4k(&mut f, gpa, hpa, rwx_wb()).expect("map");

        // Translate at GPA + 0x123; HPA should carry the same offset.
        let r = e
            .translate(&f, PhysAddr::new(gpa.as_u64() + 0x123))
            .expect("translate");
        assert_eq!(r.as_u64(), hpa.as_u64() + 0x123);
    }

    #[test]
    fn translate_unmapped_returns_hardware_error() {
        let mut f = TestFrames::new();
        let e = Ept::new(&mut f).unwrap();
        match e.translate(&f, PhysAddr::new(0x4000)) {
            Err(HyperError::Hardware(_)) => {}
            other => panic!("expected Hardware, got {other:?}"),
        }
    }

    #[test]
    fn map_rejects_misaligned() {
        let mut f = TestFrames::new();
        let mut e = Ept::new(&mut f).unwrap();
        let bad_gpa = PhysAddr::new(0x1001);
        let hpa = PhysAddr::new(0x2000);
        assert!(matches!(
            e.map_4k(&mut f, bad_gpa, hpa, rwx_wb()),
            Err(HyperError::Hardware(_))
        ));
    }

    #[test]
    fn map_rejects_no_access_bits() {
        let mut f = TestFrames::new();
        let mut e = Ept::new(&mut f).unwrap();
        assert!(matches!(
            e.map_4k(
                &mut f,
                PhysAddr::new(0x0),
                PhysAddr::new(0x0),
                EptFlags::MT_WB,
            ),
            Err(HyperError::Hardware(_))
        ));
    }

    #[test]
    fn remap_overwrites_leaf() {
        let mut f = TestFrames::new();
        let mut e = Ept::new(&mut f).unwrap();
        let gpa = PhysAddr::new(0x5000);

        e.map_4k(&mut f, gpa, PhysAddr::new(0x10_000), rwx_wb()).unwrap();
        e.map_4k(&mut f, gpa, PhysAddr::new(0x20_000), rwx_wb()).unwrap();

        let r = e.translate(&f, gpa).unwrap();
        assert_eq!(r.as_u64(), 0x20_000);
    }

    #[test]
    fn many_disjoint_pages_all_translate() {
        let mut f = TestFrames::new();
        let mut e = Ept::new(&mut f).unwrap();
        // 64 pages spread across distinct PD entries to force PT allocation.
        for i in 0u64..64 {
            let gpa = PhysAddr::new((i + 1) << 21); // each in its own PD slot
            let hpa = PhysAddr::new(0x100_000 + (i << 12));
            e.map_4k(&mut f, gpa, hpa, rwx_wb()).unwrap();
        }
        for i in 0u64..64 {
            let gpa = PhysAddr::new((i + 1) << 21);
            let want = 0x100_000 + (i << 12);
            assert_eq!(e.translate(&f, gpa).unwrap().as_u64(), want);
        }
    }

    #[test]
    fn eptp_encodes_walk_len_and_memtype() {
        let mut f = TestFrames::new();
        let e = Ept::new(&mut f).unwrap();
        let v = e.eptp();
        assert_eq!(v & 0b111, 0b110, "MT must be WB(6)");
        assert_eq!((v >> 3) & 0b111, 0b011, "walk length must be 4 (encoded as 3)");
        assert_eq!(v & ADDR_MASK, e.pml4().as_u64());
    }
}
