//! Read-only Microsoft VHDX reader.
//!
//! Phase 2 (W18.2) scope:
//!
//! - **Supported** — fixed and dynamic VHDX files with no parent
//!   ("HasParent" metadata flag clear). The reader parses the file
//!   identifier, picks the header with the higher sequence number,
//!   walks the region table to find the BAT and Metadata regions,
//!   parses the required metadata items, then services reads through
//!   the BAT.
//! - **Rejected** with [`CelError::Invalid`] — differencing disks
//!   (parent locator present), images whose `LeaveBlocksAllocated`
//!   or `HasParent` metadata flag is set, and any state that requires
//!   replaying the log to be correct (we only accept logs with
//!   `LogLength == 0` or with the log already known-clean — we play
//!   it safe and refuse anything that claims dirty journal).
//! - CRC-32C (Castagnoli) checksums are verified on the headers and
//!   region tables.
//!
//! Reference: Microsoft "VHDX Format Specification" v1.00,
//! [MS-VHDX]. Section numbers in comments refer to that document.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use celcommon::{CelError, CelResult};

use crate::disk::{DiskImage, ImageInfo};
use crate::format::FormatKind;
use crate::util::{crc32c, le_u16, le_u32, le_u64};

const HEADER1_OFF: u64 = 0x1_0000;
const HEADER2_OFF: u64 = 0x2_0000;
const REGION_TABLE1_OFF: u64 = 0x3_0000;
const REGION_TABLE2_OFF: u64 = 0x4_0000;
const HEADER_LEN: usize = 4096;
const REGION_TABLE_LEN: usize = 64 * 1024;
const MEGABYTE: u64 = 1024 * 1024;

const HEADER_SIG: &[u8; 4] = b"head";
const REGION_SIG: &[u8; 4] = b"regi";
const METADATA_SIG: &[u8; 8] = b"metadata";

/// `BAT` region GUID: `2DC27766-F623-4200-9D64-115E9BFD4A08`
const REGION_BAT: [u8; 16] = [
    0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42,
    0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08,
];
/// Metadata region GUID: `8B7CA206-4790-4B9A-B8FE-575F050F886E`
const REGION_METADATA: [u8; 16] = [
    0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B,
    0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E,
];

/// File Parameters metadata item GUID:
/// `CAA16737-FA36-4D43-B3B6-33F0AA44E76B`
const META_FILE_PARAMETERS: [u8; 16] = [
    0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D,
    0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B,
];
/// Virtual Disk Size metadata item GUID:
/// `2FA54224-CD1B-4876-B211-5DBED83BF4B8`
const META_VIRTUAL_DISK_SIZE: [u8; 16] = [
    0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48,
    0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8,
];
/// Logical Sector Size metadata item GUID:
/// `8141BF1D-A96F-4709-BA47-F233A8FAAB5F`
const META_LOGICAL_SECTOR_SIZE: [u8; 16] = [
    0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47,
    0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F,
];

/// BAT entry state bits 0..=2.
const BAT_PAYLOAD_BLOCK_NOT_PRESENT:    u64 = 0;
const BAT_PAYLOAD_BLOCK_UNDEFINED:      u64 = 1;
const BAT_PAYLOAD_BLOCK_ZERO:           u64 = 2;
const BAT_PAYLOAD_BLOCK_UNMAPPED:       u64 = 3;
const BAT_PAYLOAD_BLOCK_FULLY_PRESENT:  u64 = 6;
const BAT_PAYLOAD_BLOCK_PARTIALLY_PRESENT: u64 = 7;

/// Read-only handle to a non-differencing VHDX file.
pub struct VhdxImage {
    path: PathBuf,
    file: Mutex<File>,
    virtual_size: u64,
    block_size: u32,
    logical_sector_size: u32,
    /// BAT entries, decoded little-endian, in file order (PB and SB
    /// entries interleaved per chunk_ratio). For non-differencing
    /// images SB entries are unused.
    bat: Vec<u64>,
    chunk_ratio: u64,
    /// True if the file is a fixed VHDX (LeaveBlocksAllocated == 1
    /// in File Parameters). Reads still go through the BAT, but every
    /// payload block is FULLY_PRESENT by construction.
    is_fixed: bool,
}

#[derive(Debug, Clone)]
struct Header {
    sequence_number: u64,
    log_length: u32,
}

#[derive(Debug, Clone, Copy)]
struct RegionEntry {
    guid: [u8; 16],
    file_offset: u64,
    length: u32,
    /// Region table "Required" flag (bit 0 of the entry flags word).
    /// We don't currently refuse images that lack a required region
    /// — the BAT/Metadata absence check downstream gives a clearer
    /// error message — but parsing the bit lets future code surface
    /// it.
    #[allow(dead_code)]
    required: bool,
}

impl VhdxImage {
    /// Open `path` as a VHDX file.
    pub fn open(path: impl AsRef<Path>) -> CelResult<Self> {
        let path = path.as_ref();
        let mut f = File::open(path)
            .map_err(|e| CelError::Io(format!("open {}: {e}", path.display())))?;

        // §2.1 File Identifier.
        let mut sig = [0u8; 8];
        f.seek(SeekFrom::Start(0))
            .map_err(|e| CelError::Io(format!("seek 0: {e}")))?;
        f.read_exact(&mut sig)
            .map_err(|e| CelError::Io(format!("read fileid: {e}")))?;
        if &sig != b"vhdxfile" {
            return Err(CelError::Invalid("vhdx: bad file identifier"));
        }

        // §3.1 Pick header with the higher valid sequence number.
        let h = pick_header(&mut f)?;
        if h.log_length != 0 {
            // We don't replay logs in Phase 2. Reject so we never
            // serve stale data from a dirty journal.
            return Err(CelError::Invalid(
                "vhdx: non-empty log not supported (replay journal first)",
            ));
        }

        // §3.3 Region table. Either of the two copies is sufficient
        // when both have valid CRCs; prefer the first.
        let regions = pick_region_table(&mut f)?;
        let bat = regions.iter().find(|r| r.guid == REGION_BAT)
            .ok_or(CelError::Invalid("vhdx: BAT region missing"))?;
        let meta = regions.iter().find(|r| r.guid == REGION_METADATA)
            .ok_or(CelError::Invalid("vhdx: metadata region missing"))?;

        let metadata = read_metadata_region(&mut f, meta.file_offset, meta.length)?;

        if metadata.has_parent {
            return Err(CelError::Invalid(
                "vhdx: differencing images (HasParent) not supported",
            ));
        }
        if metadata.block_size == 0 || (metadata.block_size as u64) % MEGABYTE != 0 {
            return Err(CelError::Invalid("vhdx: block_size must be a multiple of 1 MB"));
        }
        if metadata.logical_sector_size != 512 && metadata.logical_sector_size != 4096 {
            return Err(CelError::Invalid("vhdx: logical_sector_size must be 512 or 4096"));
        }

        // §3.4 Chunk ratio: (2^23 * LogicalSectorSize) / BlockSize.
        // For a non-differencing image we don't care about the
        // interleaved SB entries, but we still have to skip them
        // when indexing the BAT array.
        let chunk_ratio = (1u64 << 23)
            .checked_mul(u64::from(metadata.logical_sector_size))
            .ok_or(CelError::Invalid("vhdx: chunk_ratio overflow"))?
            / u64::from(metadata.block_size);
        if chunk_ratio == 0 {
            return Err(CelError::Invalid("vhdx: chunk_ratio computed as zero"));
        }

        // §3.4 BAT length = ceil(virtual_size / block_size) PB entries
        // plus one SB entry per chunk.
        let pb_count = metadata.virtual_size.div_ceil(u64::from(metadata.block_size));
        let chunk_count = pb_count.div_ceil(chunk_ratio);
        let total_entries = pb_count
            .checked_add(chunk_count)
            .ok_or(CelError::Invalid("vhdx: BAT entry count overflow"))?;
        let bat_bytes = total_entries.checked_mul(8)
            .ok_or(CelError::Invalid("vhdx: BAT byte length overflow"))?;
        if bat_bytes > u64::from(bat.length) {
            return Err(CelError::Invalid(
                "vhdx: BAT region too small for declared virtual size",
            ));
        }

        let mut raw = vec![0u8; bat_bytes as usize];
        f.seek(SeekFrom::Start(bat.file_offset))
            .map_err(|e| CelError::Io(format!("seek bat: {e}")))?;
        f.read_exact(&mut raw)
            .map_err(|e| CelError::Storage(format!("read bat: {e}")))?;
        let mut bat_entries = Vec::with_capacity(total_entries as usize);
        for chunk in raw.chunks_exact(8) {
            let mut b = [0u8; 8];
            b.copy_from_slice(chunk);
            bat_entries.push(u64::from_le_bytes(b));
        }

        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(f),
            virtual_size: metadata.virtual_size,
            block_size: metadata.block_size,
            logical_sector_size: metadata.logical_sector_size,
            bat: bat_entries,
            chunk_ratio,
            is_fixed: metadata.leave_blocks_allocated,
        })
    }

    /// Source file path.
    #[must_use]
    pub fn path(&self) -> &Path { &self.path }

    /// `true` if this is a fixed VHDX (every block pre-allocated).
    #[must_use]
    pub fn is_fixed(&self) -> bool { self.is_fixed }
}

#[derive(Debug, Clone)]
struct Metadata {
    block_size: u32,
    leave_blocks_allocated: bool,
    has_parent: bool,
    virtual_size: u64,
    logical_sector_size: u32,
}

fn pick_header(f: &mut File) -> CelResult<Header> {
    let h1 = read_header(f, HEADER1_OFF).ok();
    let h2 = read_header(f, HEADER2_OFF).ok();
    match (h1, h2) {
        (Some(a), Some(b)) => Ok(if a.sequence_number >= b.sequence_number { a } else { b }),
        (Some(a), None) => Ok(a),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(CelError::Invalid("vhdx: no valid header (both CRCs bad)")),
    }
}

fn read_header(f: &mut File, off: u64) -> CelResult<Header> {
    let mut buf = vec![0u8; HEADER_LEN];
    f.seek(SeekFrom::Start(off))
        .map_err(|e| CelError::Io(format!("seek header: {e}")))?;
    f.read_exact(&mut buf)
        .map_err(|e| CelError::Io(format!("read header: {e}")))?;
    if &buf[0..4] != HEADER_SIG {
        return Err(CelError::Invalid("vhdx: header signature mismatch"));
    }
    let stored = le_u32(&buf, 4)?;
    // Zero the checksum field for verification.
    buf[4..8].copy_from_slice(&[0u8; 4]);
    let calc = crc32c(&buf);
    if calc != stored {
        return Err(CelError::Invalid("vhdx: header CRC mismatch"));
    }
    let sequence_number = le_u64(&buf, 8)?;
    // FileWriteGuid 16 + DataWriteGuid 16 + LogGuid 16 = 48 bytes at offset 16.
    // LogVersion u16 at 64, Version u16 at 66, LogLength u32 at 68, LogOffset u64 at 72.
    let log_length = le_u32(&buf, 68)?;
    Ok(Header { sequence_number, log_length })
}

fn pick_region_table(f: &mut File) -> CelResult<Vec<RegionEntry>> {
    if let Ok(r) = read_region_table(f, REGION_TABLE1_OFF) { return Ok(r); }
    read_region_table(f, REGION_TABLE2_OFF)
}

fn read_region_table(f: &mut File, off: u64) -> CelResult<Vec<RegionEntry>> {
    let mut buf = vec![0u8; REGION_TABLE_LEN];
    f.seek(SeekFrom::Start(off))
        .map_err(|e| CelError::Io(format!("seek region: {e}")))?;
    f.read_exact(&mut buf)
        .map_err(|e| CelError::Io(format!("read region: {e}")))?;
    if &buf[0..4] != REGION_SIG {
        return Err(CelError::Invalid("vhdx: region signature mismatch"));
    }
    let stored = le_u32(&buf, 4)?;
    buf[4..8].copy_from_slice(&[0u8; 4]);
    let calc = crc32c(&buf);
    if calc != stored {
        return Err(CelError::Invalid("vhdx: region CRC mismatch"));
    }
    let entry_count = le_u32(&buf, 8)?;
    if entry_count > 2047 {
        return Err(CelError::Invalid("vhdx: region entry count > 2047"));
    }
    // Reserved u32 at 12. Entries start at offset 16; each is 32 bytes.
    let mut out = Vec::with_capacity(entry_count as usize);
    for i in 0..entry_count as usize {
        let base = 16 + i * 32;
        let mut guid = [0u8; 16];
        let slice = buf.get(base..base + 16)
            .ok_or(CelError::Invalid("vhdx: region entry truncated"))?;
        guid.copy_from_slice(slice);
        let file_offset = le_u64(&buf, base + 16)?;
        let length      = le_u32(&buf, base + 24)?;
        let flags       = le_u32(&buf, base + 28)?;
        out.push(RegionEntry { guid, file_offset, length, required: flags & 1 != 0 });
    }
    Ok(out)
}

fn read_metadata_region(f: &mut File, off: u64, _len: u32) -> CelResult<Metadata> {
    // §4 Metadata table header is 32 bytes; entries are 32 bytes each.
    let mut hdr = [0u8; 32];
    f.seek(SeekFrom::Start(off))
        .map_err(|e| CelError::Io(format!("seek metadata: {e}")))?;
    f.read_exact(&mut hdr)
        .map_err(|e| CelError::Io(format!("read metadata header: {e}")))?;
    if &hdr[0..8] != METADATA_SIG {
        return Err(CelError::Invalid("vhdx: metadata signature mismatch"));
    }
    let entry_count = le_u16(&hdr, 10)?;
    if entry_count == 0 || entry_count > 2047 {
        return Err(CelError::Invalid("vhdx: metadata entry count out of range"));
    }

    let mut block_size: Option<u32> = None;
    let mut flags: Option<u32> = None;
    let mut virtual_size: Option<u64> = None;
    let mut logical_sector_size: Option<u32> = None;

    for i in 0..entry_count as usize {
        let mut entry = [0u8; 32];
        f.seek(SeekFrom::Start(off + 32 + (i as u64) * 32))
            .map_err(|e| CelError::Io(format!("seek meta entry: {e}")))?;
        f.read_exact(&mut entry)
            .map_err(|e| CelError::Io(format!("read meta entry: {e}")))?;
        let mut guid = [0u8; 16];
        guid.copy_from_slice(&entry[0..16]);
        let item_off = le_u32(&entry, 16)?;
        let item_len = le_u32(&entry, 20)?;
        if item_len == 0 || item_len > 1 << 20 {
            return Err(CelError::Invalid("vhdx: metadata item length out of range"));
        }
        let mut item = vec![0u8; item_len as usize];
        f.seek(SeekFrom::Start(off + u64::from(item_off)))
            .map_err(|e| CelError::Io(format!("seek meta item: {e}")))?;
        f.read_exact(&mut item)
            .map_err(|e| CelError::Io(format!("read meta item: {e}")))?;

        if guid == META_FILE_PARAMETERS {
            if item.len() < 8 { return Err(CelError::Invalid("vhdx: file_parameters short")); }
            block_size = Some(le_u32(&item, 0)?);
            flags      = Some(le_u32(&item, 4)?);
        } else if guid == META_VIRTUAL_DISK_SIZE {
            if item.len() < 8 { return Err(CelError::Invalid("vhdx: virtual_disk_size short")); }
            virtual_size = Some(le_u64(&item, 0)?);
        } else if guid == META_LOGICAL_SECTOR_SIZE {
            if item.len() < 4 { return Err(CelError::Invalid("vhdx: logical_sector_size short")); }
            logical_sector_size = Some(le_u32(&item, 0)?);
        }
        // Unknown / non-required items are ignored.
    }

    let block_size = block_size.ok_or(CelError::Invalid("vhdx: FileParameters missing"))?;
    let flags = flags.ok_or(CelError::Invalid("vhdx: FileParameters flags missing"))?;
    let virtual_size = virtual_size
        .ok_or(CelError::Invalid("vhdx: VirtualDiskSize missing"))?;
    let logical_sector_size = logical_sector_size
        .ok_or(CelError::Invalid("vhdx: LogicalSectorSize missing"))?;

    Ok(Metadata {
        block_size,
        leave_blocks_allocated: flags & 1 != 0,
        has_parent: flags & 2 != 0,
        virtual_size,
        logical_sector_size,
    })
}

impl DiskImage for VhdxImage {
    fn info(&self) -> ImageInfo {
        ImageInfo {
            format: FormatKind::Vhdx,
            virtual_size: self.virtual_size,
            cluster_size: Some(u64::from(self.block_size)),
            backend: if self.is_fixed { "vhdx-fixed" } else { "vhdx-dynamic" },
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> CelResult<usize> {
        if buf.is_empty() || offset >= self.virtual_size {
            return Ok(0);
        }
        let max = (self.virtual_size - offset) as usize;
        let want = buf.len().min(max);
        let buf = &mut buf[..want];

        let block_bytes = u64::from(self.block_size);
        let mut filled = 0usize;
        let mut cur = offset;

        while filled < want {
            let intra = cur % block_bytes;
            let chunk = (want - filled).min((block_bytes - intra) as usize);
            let block_idx = cur / block_bytes;
            // Interleave: skip one SB entry after every `chunk_ratio`
            // PB entries.
            let bat_idx = block_idx + (block_idx / self.chunk_ratio);
            let dest = &mut buf[filled..filled + chunk];

            let entry = self.bat
                .get(bat_idx as usize)
                .copied()
                .ok_or(CelError::Invalid("vhdx: BAT index out of range"))?;
            let state = entry & 0x7;
            match state {
                BAT_PAYLOAD_BLOCK_NOT_PRESENT
                | BAT_PAYLOAD_BLOCK_UNDEFINED
                | BAT_PAYLOAD_BLOCK_ZERO
                | BAT_PAYLOAD_BLOCK_UNMAPPED => {
                    dest.fill(0);
                }
                BAT_PAYLOAD_BLOCK_FULLY_PRESENT => {
                    // Upper bits give the file offset, in 1 MB units
                    // — i.e. the bottom 20 bits are zero. The mask
                    // `0xFFFFFFFFFFF00000` extracts it.
                    let block_off = entry & 0xFFFF_FFFF_FFF0_0000;
                    let read_at = block_off + intra;
                    let mut f = self.file.lock()
                        .map_err(|_| CelError::Internal("vhdx: file mutex poisoned"))?;
                    f.seek(SeekFrom::Start(read_at))
                        .map_err(|e| CelError::Io(format!("seek vhdx block: {e}")))?;
                    f.read_exact(dest)
                        .map_err(|e| CelError::Storage(format!("read vhdx block: {e}")))?;
                }
                BAT_PAYLOAD_BLOCK_PARTIALLY_PRESENT => {
                    return Err(CelError::Invalid(
                        "vhdx: partially-present block (differencing) not supported",
                    ));
                }
                _ => return Err(CelError::Invalid("vhdx: unknown BAT state")),
            }

            filled += chunk;
            cur += chunk as u64;
        }
        Ok(filled)
    }

    fn virtual_size(&self) -> u64 { self.virtual_size }
}

// Silence dead-code lints on `logical_sector_size` (recorded for
// future Phase-2 follow-ups: 4K-sector parity, alignment hints).
#[allow(dead_code)]
impl VhdxImage {
    fn logical_sector_size(&self) -> u32 { self.logical_sector_size }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a tiny but spec-valid dynamic VHDX in memory:
    ///
    /// - block_size = 1 MiB,
    /// - logical_sector_size = 512,
    /// - virtual_size = 1 MiB (1 block),
    /// - one FULLY_PRESENT BAT entry pointing at the payload block,
    /// - payload filled with `0x5A`.
    fn synth_vhdx_dynamic() -> Vec<u8> {
        // Layout:
        //   0x00_0000 - 0x00_FFFF : file identifier
        //   0x01_0000 - 0x01_0FFF : header 1
        //   0x02_0000 - 0x02_0FFF : header 2
        //   0x03_0000 - 0x03_FFFF : region table 1
        //   0x04_0000 - 0x04_FFFF : region table 2
        //   0x10_0000             : metadata region (1 MB aligned)
        //   0x20_0000             : BAT region    (1 MB aligned)
        //   0x30_0000             : payload block (1 MB aligned)
        const FILE_LEN: usize = 0x40_0000; // 4 MiB
        let mut img = vec![0u8; FILE_LEN];

        // File identifier.
        img[0..8].copy_from_slice(b"vhdxfile");

        // Headers — both with sequence 1.
        write_header(&mut img, HEADER1_OFF as usize, 1);
        write_header(&mut img, HEADER2_OFF as usize, 1);

        // Region tables — describe two regions: metadata + BAT.
        write_region_table(&mut img, REGION_TABLE1_OFF as usize);
        write_region_table(&mut img, REGION_TABLE2_OFF as usize);

        // Metadata region at 0x10_0000.
        write_metadata_region(&mut img, 0x10_0000);

        // BAT region at 0x20_0000.
        // Entry index 0 = PB[0] → state=6, offset=0x30_0000.
        let bat_off = 0x20_0000usize;
        let pb_entry: u64 = 0x30_0000 | 6;
        img[bat_off..bat_off + 8].copy_from_slice(&pb_entry.to_le_bytes());

        // Payload block at 0x30_0000, fill with 0x5A.
        for b in &mut img[0x30_0000..0x40_0000] { *b = 0x5A; }

        img
    }

    fn write_header(img: &mut [u8], off: usize, seq: u64) {
        let mut hdr = vec![0u8; HEADER_LEN];
        hdr[0..4].copy_from_slice(HEADER_SIG);
        // Checksum at 4..8 zeroed for now.
        hdr[8..16].copy_from_slice(&seq.to_le_bytes());
        // FileWriteGuid (16), DataWriteGuid (16), LogGuid (16) all
        // zero is acceptable to our reader (we don't validate them).
        // LogVersion u16 at 64 = 0, Version u16 at 66 = 1.
        hdr[66..68].copy_from_slice(&1u16.to_le_bytes());
        // LogLength u32 at 68 = 0, LogOffset u64 at 72 = 0.
        let crc = crc32c(&hdr);
        hdr[4..8].copy_from_slice(&crc.to_le_bytes());
        img[off..off + HEADER_LEN].copy_from_slice(&hdr);
    }

    fn write_region_table(img: &mut [u8], off: usize) {
        let mut tbl = vec![0u8; REGION_TABLE_LEN];
        tbl[0..4].copy_from_slice(REGION_SIG);
        // Checksum at 4..8 zeroed.
        tbl[8..12].copy_from_slice(&2u32.to_le_bytes()); // entry_count
        // Reserved u32 at 12.

        // Entry 0: BAT.
        let base = 16usize;
        tbl[base..base + 16].copy_from_slice(&REGION_BAT);
        tbl[base + 16..base + 24].copy_from_slice(&0x20_0000u64.to_le_bytes());
        tbl[base + 24..base + 28].copy_from_slice(&(1u32 << 20).to_le_bytes()); // 1 MB
        tbl[base + 28..base + 32].copy_from_slice(&1u32.to_le_bytes()); // required

        // Entry 1: Metadata.
        let base = 16 + 32;
        tbl[base..base + 16].copy_from_slice(&REGION_METADATA);
        tbl[base + 16..base + 24].copy_from_slice(&0x10_0000u64.to_le_bytes());
        tbl[base + 24..base + 28].copy_from_slice(&(1u32 << 20).to_le_bytes());
        tbl[base + 28..base + 32].copy_from_slice(&1u32.to_le_bytes());

        let crc = crc32c(&tbl);
        tbl[4..8].copy_from_slice(&crc.to_le_bytes());
        img[off..off + REGION_TABLE_LEN].copy_from_slice(&tbl);
    }

    fn write_metadata_region(img: &mut [u8], off: usize) {
        // Header.
        img[off..off + 8].copy_from_slice(METADATA_SIG);
        // Reserved u16 at 8.
        img[off + 10..off + 12].copy_from_slice(&3u16.to_le_bytes()); // entry_count
        // Reserved bytes at 12..32.

        // Place items in a small data area after the entry table.
        // Each entry is 32 bytes; with 3 entries the table ends at
        // off + 32 + 3*32 = off + 128. Put items at +128.
        let item_area: u32 = 128;
        let mut next_off = item_area;

        // Entry 0: FileParameters (8 bytes: block_size=1MiB, flags=0).
        let e0 = off + 32;
        img[e0..e0 + 16].copy_from_slice(&META_FILE_PARAMETERS);
        img[e0 + 16..e0 + 20].copy_from_slice(&next_off.to_le_bytes());
        img[e0 + 20..e0 + 24].copy_from_slice(&8u32.to_le_bytes());
        img[e0 + 24..e0 + 28].copy_from_slice(&0u32.to_le_bytes()); // flags
        let i0 = off + next_off as usize;
        img[i0..i0 + 4].copy_from_slice(&(1u32 << 20).to_le_bytes()); // block_size
        img[i0 + 4..i0 + 8].copy_from_slice(&0u32.to_le_bytes());    // flags
        next_off += 8;

        // Entry 1: VirtualDiskSize (8 bytes = 1 MiB).
        let e1 = off + 32 + 32;
        img[e1..e1 + 16].copy_from_slice(&META_VIRTUAL_DISK_SIZE);
        img[e1 + 16..e1 + 20].copy_from_slice(&next_off.to_le_bytes());
        img[e1 + 20..e1 + 24].copy_from_slice(&8u32.to_le_bytes());
        img[e1 + 24..e1 + 28].copy_from_slice(&0u32.to_le_bytes());
        let i1 = off + next_off as usize;
        img[i1..i1 + 8].copy_from_slice(&(1u64 << 20).to_le_bytes());
        next_off += 8;

        // Entry 2: LogicalSectorSize (4 bytes = 512).
        let e2 = off + 32 + 64;
        img[e2..e2 + 16].copy_from_slice(&META_LOGICAL_SECTOR_SIZE);
        img[e2 + 16..e2 + 20].copy_from_slice(&next_off.to_le_bytes());
        img[e2 + 20..e2 + 24].copy_from_slice(&4u32.to_le_bytes());
        img[e2 + 24..e2 + 28].copy_from_slice(&0u32.to_le_bytes());
        let i2 = off + next_off as usize;
        img[i2..i2 + 4].copy_from_slice(&512u32.to_le_bytes());
    }

    fn write_temp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut t = tempfile::NamedTempFile::new().unwrap();
        t.write_all(bytes).unwrap();
        t.flush().unwrap();
        t
    }

    #[test]
    fn opens_synthetic_vhdx_and_reports_info() {
        let t = write_temp(&synth_vhdx_dynamic());
        let img = VhdxImage::open(t.path()).unwrap();
        let info = img.info();
        assert_eq!(info.format, FormatKind::Vhdx);
        assert_eq!(info.virtual_size, 1 << 20);
        assert_eq!(info.cluster_size, Some(1 << 20));
        assert_eq!(info.backend, "vhdx-dynamic");
    }

    #[test]
    fn reads_fully_present_block() {
        let t = write_temp(&synth_vhdx_dynamic());
        let img = VhdxImage::open(t.path()).unwrap();
        let mut buf = [0u8; 64];
        let n = img.read_at(0, &mut buf).unwrap();
        assert_eq!(n, 64);
        assert!(buf.iter().all(|&b| b == 0x5A));
    }

    #[test]
    fn read_clips_at_virtual_size() {
        let t = write_temp(&synth_vhdx_dynamic());
        let img = VhdxImage::open(t.path()).unwrap();
        let mut buf = [0u8; 100];
        let n = img.read_at(img.virtual_size() - 10, &mut buf).unwrap();
        assert_eq!(n, 10);
    }

    #[test]
    fn rejects_corrupt_header_crc() {
        let mut img = synth_vhdx_dynamic();
        // Corrupt header 1 checksum and header 2 sequence number ->
        // both unusable.
        img[HEADER1_OFF as usize + 4] ^= 0xFF;
        img[HEADER2_OFF as usize + 5] ^= 0xFF;
        let t = write_temp(&img);
        let Err(err) = VhdxImage::open(t.path()) else { panic!("bad header CRC must be rejected") };
        assert_eq!(err.code(), "invalid");
    }

    #[test]
    fn rejects_bad_file_identifier() {
        let mut img = synth_vhdx_dynamic();
        img[0..8].copy_from_slice(b"notvhdxx");
        let t = write_temp(&img);
        let Err(err) = VhdxImage::open(t.path()) else { panic!("bad fileid must be rejected") };
        assert_eq!(err.code(), "invalid");
    }
}
