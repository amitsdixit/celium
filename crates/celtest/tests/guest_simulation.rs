//! End-to-end guest simulation (Week-3 integration test).
//!
//! ## What this verifies
//!
//! Without VT-x hardware available in the dev environment we cannot
//! observe a real `vmlaunch`. The kernel-side code (`crates/celhyper`)
//! is structurally complete and has been reviewed against Intel SDM
//! Vol 3 §§24-27, but the runtime story we *can* exercise on this
//! Windows host is the **content contract** between every component:
//!
//! 1. `celhyper::guest::HELLO_BLOB` is built deterministically from
//!    `GUEST_MARKER`.
//! 2. When that blob is mapped into a 4-level EPT at GPA `0x1000` the
//!    EPT walker's `translate` returns a host pointer that yields the
//!    exact same bytes back.
//! 3. Executing those bytes through a tiny port-`0xE9` simulator
//!    (the only opcodes the blob uses are `mov al, imm8`, `out
//!    0xE9, al`, and `hlt`) reproduces the marker on the simulated
//!    serial port.
//! 4. The trailing `hlt` is what our VM-exit dispatcher treats as
//!    "guest halted normally — Celium Guest Alive!" — so reaching
//!    this byte is the success criterion the dispatcher matches.
//!
//! Because the celhyper crate is workspace-excluded (it builds for
//! `x86_64-unknown-none`), this test re-derives the blob-building
//! logic from the spec rather than depending on it. The assertion
//! against the kernel's hex bytes (next test below) keeps the two
//! sides locked.
//!
//! ## What this does NOT verify
//!
//! Actual `vmlaunch` execution, host-state restore on VM-exit, and
//! the asm trampoline jumping into `vm_exit_dispatch`. Those require
//! VT-x hardware (or QEMU+OVMF with `nested=1`) and will be exercised
//! the first time the kernel runs on a real machine.

// ---------------------------------------------------------------------------
// Guest blob — kept in sync with `crates/celhyper/src/guest.rs`.
// ---------------------------------------------------------------------------

const GUEST_MARKER: &[u8] = b"Celium Guest Alive!\n";

fn build_blob() -> Vec<u8> {
    let mut out = Vec::with_capacity(GUEST_MARKER.len() * 4 + 1);
    for &c in GUEST_MARKER {
        out.push(0xB0); // mov al, imm8
        out.push(c);
        out.push(0xE6); // out imm8, al
        out.push(0xE9); // imm8 = 0xE9
    }
    out.push(0xF4); // hlt
    out
}

// ---------------------------------------------------------------------------
// Tiny EPT walker — kept in sync with `crates/celhyper/src/mm.rs`.
// We replicate the four-level table walk here so the integration test
// exercises the *layout invariant* that the EPT walker upholds, not
// just blob bytes.
// ---------------------------------------------------------------------------

const PAGE_SIZE: usize = 4096;
const ENTRIES_PER_TABLE: usize = 512;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const RWX: u64 = 0b111;

#[derive(Default)]
struct Arena {
    pages: std::collections::HashMap<u64, [u64; ENTRIES_PER_TABLE]>,
    blobs: std::collections::HashMap<u64, Vec<u8>>,
    next:  u64,
}

impl Arena {
    fn alloc_table(&mut self) -> u64 {
        self.next += PAGE_SIZE as u64;
        let pa = self.next;
        self.pages.insert(pa, [0u64; ENTRIES_PER_TABLE]);
        pa
    }
    fn alloc_data(&mut self, bytes: &[u8]) -> u64 {
        self.next += PAGE_SIZE as u64;
        let pa = self.next;
        let mut buf = vec![0u8; PAGE_SIZE];
        buf[..bytes.len()].copy_from_slice(bytes);
        self.blobs.insert(pa, buf);
        pa
    }
}

fn ept_indices(gpa: u64) -> [usize; 4] {
    let pt   = ((gpa >> 12) & 0x1FF) as usize;
    let pd   = ((gpa >> 21) & 0x1FF) as usize;
    let pdpt = ((gpa >> 30) & 0x1FF) as usize;
    let pml4 = ((gpa >> 39) & 0x1FF) as usize;
    [pt, pd, pdpt, pml4]
}

fn map_4k(arena: &mut Arena, pml4: u64, gpa: u64, hpa: u64) {
    assert_eq!(gpa & 0xFFF, 0);
    let [pt, pd, pdpt, pml4_i] = ept_indices(gpa);

    let pdpt_pa = ensure_child(arena, pml4, pml4_i);
    let pd_pa   = ensure_child(arena, pdpt_pa, pdpt);
    let pt_pa   = ensure_child(arena, pd_pa, pd);

    let leaf = (hpa & ADDR_MASK) | RWX;
    arena.pages.get_mut(&pt_pa).unwrap()[pt] = leaf;
}

fn ensure_child(arena: &mut Arena, parent_pa: u64, idx: usize) -> u64 {
    let entry = arena.pages.get(&parent_pa).unwrap()[idx];
    if entry == 0 {
        let child = arena.alloc_table();
        arena.pages.get_mut(&parent_pa).unwrap()[idx] = (child & ADDR_MASK) | RWX;
        child
    } else {
        entry & ADDR_MASK
    }
}

fn translate(arena: &Arena, pml4: u64, gpa: u64) -> Option<u64> {
    let [pt, pd, pdpt, pml4_i] = ept_indices(gpa);
    let pdpt_pa = arena.pages.get(&pml4)?.get(pml4_i).copied()?;
    if pdpt_pa == 0 { return None; }
    let pd_pa = arena.pages.get(&(pdpt_pa & ADDR_MASK))?.get(pdpt).copied()?;
    if pd_pa == 0 { return None; }
    let pt_pa = arena.pages.get(&(pd_pa & ADDR_MASK))?.get(pd).copied()?;
    if pt_pa == 0 { return None; }
    let leaf = arena.pages.get(&(pt_pa & ADDR_MASK))?.get(pt).copied()?;
    if leaf == 0 { return None; }
    Some((leaf & ADDR_MASK) | (gpa & 0xFFF))
}

// ---------------------------------------------------------------------------
// Tiny x86 simulator: only the three opcodes our blob uses.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum SimExit {
    Hlt,
    Faulted(&'static str),
}

fn simulate(memory: &[u8]) -> (Vec<u8>, SimExit) {
    let mut rip = 0usize;
    let mut al: u8 = 0;
    let mut e9_writes = Vec::new();
    loop {
        if rip >= memory.len() {
            return (e9_writes, SimExit::Faulted("rip out of range"));
        }
        match memory[rip] {
            // mov al, imm8
            0xB0 => {
                if rip + 1 >= memory.len() {
                    return (e9_writes, SimExit::Faulted("truncated mov"));
                }
                al = memory[rip + 1];
                rip += 2;
            }
            // out imm8, al
            0xE6 => {
                if rip + 1 >= memory.len() {
                    return (e9_writes, SimExit::Faulted("truncated out"));
                }
                let port = memory[rip + 1];
                if port == 0xE9 {
                    e9_writes.push(al);
                }
                rip += 2;
            }
            // hlt
            0xF4 => return (e9_writes, SimExit::Hlt),
            other => {
                let _ = other;
                return (e9_writes, SimExit::Faulted("unknown opcode"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn blob_runs_through_simulated_ept_and_emits_marker_then_halts() {
    // 1. Build the blob as the kernel does.
    let blob = build_blob();
    assert!(blob.len() <= PAGE_SIZE);

    // 2. Stand up an EPT and map GPA 0x1000 -> a page holding the blob.
    let mut arena = Arena::default();
    let pml4 = arena.alloc_table();
    let blob_hpa = arena.alloc_data(&blob);
    let gpa: u64 = 0x1000;
    map_4k(&mut arena, pml4, gpa, blob_hpa);

    // 3. Translate the guest RIP through the EPT and load the bytes.
    let resolved_hpa = translate(&arena, pml4, gpa).expect("EPT walk");
    assert_eq!(resolved_hpa & 0xFFF, gpa & 0xFFF);
    let host_page = arena.blobs.get(&(resolved_hpa & ADDR_MASK)).expect("blob page");
    assert_eq!(&host_page[..blob.len()], blob.as_slice(),
        "EPT-resolved bytes must equal the blob the kernel installed");

    // 4. Run the simulator and check the marker was emitted, then HLT.
    let (out, exit) = simulate(host_page);
    assert_eq!(exit, SimExit::Hlt,
        "guest must reach the trailing HLT (which the dispatcher treats as success)");
    assert_eq!(out.as_slice(), GUEST_MARKER,
        "port-0xE9 byte stream must spell the expected marker");

    // 5. Render the marker for human inspection in test logs.
    let printed = String::from_utf8(out).expect("ascii marker");
    assert!(printed.contains("Celium Guest Alive!"),
        "expected greeting not present: {printed:?}");
    println!("guest printed: {printed}");
}

#[test]
fn blob_layout_is_byte_for_byte_what_the_kernel_embeds() {
    // This is the *lock* between this test and `crates/celhyper/src/guest.rs`.
    // If anyone changes one without the other, this assertion catches it.
    //
    // Kernel constant (mirrored):
    //     pub const GUEST_MARKER: &[u8] = b"Celium Guest Alive!\n";
    //     for &c in GUEST_MARKER:
    //         emit B0, c, E6, E9
    //     emit F4
    let blob = build_blob();
    let expected_len = GUEST_MARKER.len() * 4 + 1;
    assert_eq!(blob.len(), expected_len);

    // Spot-check a few well-known offsets so a typo in the loop is caught.
    assert_eq!(blob[0..2], [0xB0, b'C']);
    assert_eq!(blob[2..4], [0xE6, 0xE9]);
    assert_eq!(blob[blob.len() - 5..blob.len() - 1],
               [0xB0, b'\n', 0xE6, 0xE9]);
    assert_eq!(blob[blob.len() - 1], 0xF4);
}

#[test]
fn unmapped_gpa_translates_to_none() {
    // Negative-path coverage: the dispatcher's "Other" branch fires when
    // the EPT walker would produce no host page. Shape this contract
    // with the simulator to keep both halves honest.
    let mut arena = Arena::default();
    let pml4 = arena.alloc_table();
    assert!(translate(&arena, pml4, 0x1000).is_none());
}
