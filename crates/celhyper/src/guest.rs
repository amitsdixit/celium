//! Guest payloads embedded in the kernel image.
//!
//! Week-3 "hello world" guest. A position-independent x86 routine that
//! writes a fixed greeting to the QEMU debug-console port (`0xE9`) one
//! byte at a time and halts. The blob runs in long mode (we set
//! `VMENTRY_IA32E_MODE_GUEST` on entry) but uses only single-byte and
//! port-immediate forms whose encoding is identical across 16/32/64-bit
//! modes, so it works without us having to write a guest GDT yet.
//!
//! Per byte of greeting we emit:
//!
//! ```text
//!     B0 ??     mov al, <byte>      ; 2 bytes
//!     E6 E9     out 0xE9, al        ; 2 bytes
//! ```
//!
//! followed by a single trailing `F4` (`hlt`). The first VM-exit
//! (reason = 12 — "HLT") fires on the trailing `hlt`. Our exit handler
//! recognises that reason and treats it as the integration test's
//! liveness signal.
//!
//! Why port `0xE9`? It's the convention QEMU uses for its debug
//! console (`-debugcon stdio`). Real hardware silently ignores writes,
//! so the same blob is harmless on a physical machine.

/// The text the guest prints. Keep terminating newline so a real-mode
/// terminal flushes the line cleanly.
pub const GUEST_MARKER: &[u8] = b"Celium Guest Alive!\n";

/// Length of the assembled blob: 4 bytes per character + 1 byte for `hlt`.
const BLOB_LEN: usize = GUEST_MARKER.len() * 4 + 1;

/// Build the blob at compile time so callers can borrow it as a slice.
const fn build_blob() -> [u8; BLOB_LEN] {
    let mut out = [0u8; BLOB_LEN];
    let mut i = 0;
    while i < GUEST_MARKER.len() {
        out[i * 4]     = 0xB0;            // mov al, imm8
        out[i * 4 + 1] = GUEST_MARKER[i];
        out[i * 4 + 2] = 0xE6;            // out imm8, al
        out[i * 4 + 3] = 0xE9;            // imm8 = 0xE9
        i += 1;
    }
    out[GUEST_MARKER.len() * 4] = 0xF4;   // hlt
    out
}

/// Concrete byte storage. Held as a `static` so the blob's address is
/// stable for `core::ptr::copy_nonoverlapping`.
static HELLO_BLOB_BYTES: [u8; BLOB_LEN] = build_blob();

/// Hand-assembled "Celium Guest Alive!" routine. Fits well inside one
/// 4 KiB page (`PAGE_SIZE` enforced by `install_first_guest`).
pub const HELLO_BLOB: &[u8] = &HELLO_BLOB_BYTES;
