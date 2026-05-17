//! Castagnoli CRC32C — bit-by-bit reflected (polynomial 0x82F63B78).
//!
//! Identical algorithm to `celimage::util::crc32c` on the host so a
//! CRC computed by the controller matches one computed by the
//! kernel byte-for-byte. The bit-serial form is intentionally tiny
//! (no 1 KiB lookup table) at the cost of ~8x slowdown vs. table
//! lookup; 4 KiB worst case is well under one millisecond on the
//! BSP. If a driver path ever pulls in MB-scale CRCs we'll port the
//! slicing-by-8 variant from celimage at that point.
//!
//! Lives in its own module (not inside [`crate::image_loader`])
//! purely so it can be unit-tested under `#[cfg(test)]` — the
//! image_loader module is `#![cfg(not(test))]` because it pulls
//! kernel-only dependencies (`logger`, `manager`, ...).

#![allow(clippy::cast_possible_truncation)]

/// One-shot CRC32C. Matches `celimage::util::crc32c` for the same
/// input — verified via the well-known `"123456789"` check vector
/// in the unit test.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_continue(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// Streaming form. Callers init `state = 0xFFFF_FFFF`, feed chunks,
/// and finalise with `state ^ 0xFFFF_FFFF`.
#[must_use]
pub fn crc32c_continue(mut state: u32, data: &[u8]) -> u32 {
    for &b in data {
        state ^= u32::from(b);
        let mut i = 0;
        while i < 8 {
            state = if state & 1 != 0 { (state >> 1) ^ 0x82F6_3B78 } else { state >> 1 };
            i += 1;
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks our impl to the host's Castagnoli polynomial. These are
    /// the same vectors `celimage::util::crc32c` is tested against.
    #[test]
    fn crc32c_known_vectors() {
        assert_eq!(crc32c(&[]), 0);
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    /// `crc32c_continue` must be associative so the kernel can later
    /// stream-checksum multi-chunk uploads without buffering the
    /// whole image (W23-F).
    #[test]
    fn crc32c_streaming_matches_oneshot() {
        let bytes: Vec<u8> = (0..200u16).map(|i| (i & 0xff) as u8).collect();
        let oneshot = crc32c(&bytes);
        let mut state: u32 = 0xFFFF_FFFF;
        for c in bytes.chunks(7) {
            state = crc32c_continue(state, c);
        }
        let streamed = state ^ 0xFFFF_FFFF;
        assert_eq!(oneshot, streamed);
    }
}
