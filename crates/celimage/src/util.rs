//! Internal byte-parsing and checksum helpers shared by the
//! format-specific readers.
//!
//! Keeping these here means the format modules can stay focused on
//! their on-disk layouts and don't have to repeat the same
//! length-checked slice-into-array patterns.

use celcommon::{CelError, CelResult};

/// Read a big-endian `u32` at `off` in `buf`. Returns
/// [`CelError::Invalid`] if `buf` is too short.
#[allow(dead_code)] // kept for future qcow2 refactor onto this helper
#[inline]
pub(crate) fn be_u32(buf: &[u8], off: usize) -> CelResult<u32> {
    let end = off
        .checked_add(4)
        .ok_or(CelError::Invalid("util: be_u32 overflow"))?;
    let slice = buf
        .get(off..end)
        .ok_or(CelError::Invalid("util: be_u32 short read"))?;
    let mut b = [0u8; 4];
    b.copy_from_slice(slice);
    Ok(u32::from_be_bytes(b))
}

/// Read a little-endian `u16` at `off` in `buf`.
#[inline]
pub(crate) fn le_u16(buf: &[u8], off: usize) -> CelResult<u16> {
    let end = off
        .checked_add(2)
        .ok_or(CelError::Invalid("util: le_u16 overflow"))?;
    let slice = buf
        .get(off..end)
        .ok_or(CelError::Invalid("util: le_u16 short read"))?;
    let mut b = [0u8; 2];
    b.copy_from_slice(slice);
    Ok(u16::from_le_bytes(b))
}

/// Read a little-endian `u32` at `off` in `buf`.
#[inline]
pub(crate) fn le_u32(buf: &[u8], off: usize) -> CelResult<u32> {
    let end = off
        .checked_add(4)
        .ok_or(CelError::Invalid("util: le_u32 overflow"))?;
    let slice = buf
        .get(off..end)
        .ok_or(CelError::Invalid("util: le_u32 short read"))?;
    let mut b = [0u8; 4];
    b.copy_from_slice(slice);
    Ok(u32::from_le_bytes(b))
}

/// Read a little-endian `u64` at `off` in `buf`.
#[inline]
pub(crate) fn le_u64(buf: &[u8], off: usize) -> CelResult<u64> {
    let end = off
        .checked_add(8)
        .ok_or(CelError::Invalid("util: le_u64 overflow"))?;
    let slice = buf
        .get(off..end)
        .ok_or(CelError::Invalid("util: le_u64 short read"))?;
    let mut b = [0u8; 8];
    b.copy_from_slice(slice);
    Ok(u64::from_le_bytes(b))
}

/// Castagnoli CRC-32C (polynomial 0x1EDC6F41, reflected 0x82F63B78).
///
/// Used by VHDX header / region-table / log-entry checksums (per
/// §3 of the VHDX specification). Software-only implementation: no
/// SSE 4.2 intrinsics, no `unsafe`, no extra dependencies.
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    crc32c_continue(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// Continue a streaming CRC-32C from an in-progress state.
///
/// Callers initialise `state = 0xFFFF_FFFF`, feed any number of
/// chunks through this routine, and finalise with `state ^ 0xFFFF_FFFF`.
/// Used by [`crate::full_image_crc32c`] so we don't have to buffer
/// the whole virtual disk in memory.
pub(crate) fn crc32c_continue(mut state: u32, data: &[u8]) -> u32 {
    for &b in data {
        state ^= u32::from(b);
        for _ in 0..8 {
            state = if state & 1 != 0 { (state >> 1) ^ 0x82F6_3B78 } else { state >> 1 };
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vectors() {
        // From RFC 3720 appendix B (iSCSI uses CRC-32C).
        assert_eq!(crc32c(&[]), 0);
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
        // "123456789" check value per the Castagnoli reference.
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn crc32c_streaming_matches_oneshot() {
        // Feeding the same bytes in 1/3/7/13-byte chunks must
        // produce the same digest as a single call. Validates the
        // associativity required by `crate::full_image_crc32c`.
        let bytes: Vec<u8> = (0..512u16).map(|i| (i & 0xff) as u8).collect();
        let oneshot = crc32c(&bytes);
        for chunk in [1usize, 3, 7, 13, 64, 511] {
            let mut state: u32 = 0xFFFF_FFFF;
            for c in bytes.chunks(chunk) {
                state = crc32c_continue(state, c);
            }
            let streamed = state ^ 0xFFFF_FFFF;
            assert_eq!(streamed, oneshot, "mismatch at chunk={chunk}");
        }
    }

    #[test]
    fn endian_helpers_round_trip() {
        let mut buf = [0u8; 8];
        buf[0..2].copy_from_slice(&0x1234u16.to_le_bytes());
        assert_eq!(le_u16(&buf, 0).unwrap(), 0x1234);
        buf[0..4].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        assert_eq!(le_u32(&buf, 0).unwrap(), 0xdead_beef);
        buf[0..8].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(le_u64(&buf, 0).unwrap(), 0x0102_0304_0506_0708);
        buf[0..4].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        assert_eq!(be_u32(&buf, 0).unwrap(), 0xdead_beef);
    }

    #[test]
    fn endian_helpers_short_read_is_invalid() {
        let buf = [0u8; 3];
        assert!(le_u32(&buf, 0).is_err());
        assert!(le_u64(&buf, 0).is_err());
        assert!(le_u32(&buf, 8).is_err());
    }
}
