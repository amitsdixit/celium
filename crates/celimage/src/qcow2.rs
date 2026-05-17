//! Read-only qcow2 v2 / v3 reader.
//!
//! References:
//! - QEMU docs/interop/qcow2.txt
//! - <https://github.com/qemu/qemu/blob/master/docs/interop/qcow2.txt>
//!
//! Supported features (v2 and v3):
//!
//! - Unallocated and zero clusters read as zeros.
//! - Standard allocated clusters served from the data offset stored
//!   in their L2 entry.
//! - Cluster sizes in the spec range (`2^9 .. 2^21`, inclusive).
//!
//! **Not** supported in Phase 1 (returns
//! [`celcommon::CelError::Invalid`] when encountered):
//!
//! - Compressed clusters (L2 bit 62).
//! - Encrypted clusters (header `crypt_method != 0`).
//! - Extended L2 entries (`incompatible_features` bit 4).
//! - Backing-file chains (header `backing_file_offset != 0`). We
//!   surface this as `Invalid` so a Phase-2 chain follower can opt
//!   in deliberately.
//! - Snapshot lookups.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use celcommon::{CelError, CelResult};

use crate::disk::{DiskImage, ImageInfo};
use crate::format::FormatKind;

const MAGIC: &[u8; 4] = b"QFI\xfb";

const L2_ENTRY_COMPRESSED: u64    = 1 << 62;
const L2_ENTRY_ZERO_CLUSTER: u64  = 1 << 0;          // v3 only — bit 0 in entry
const L2_ENTRY_OFFSET_MASK: u64   = 0x00ff_ffff_ffff_fe00;

/// qcow2 v2/v3 reader.
pub struct Qcow2Image {
    path: PathBuf,
    file: Mutex<File>,
    header: Header,
    /// L1 table cached in memory at open time. Each entry is a big-
    /// endian u64; the high bits encode flags, the low bits an offset
    /// to the corresponding L2 table.
    l1: Vec<u64>,
}

#[derive(Debug, Clone)]
struct Header {
    version: u32,
    cluster_bits: u32,
    cluster_size: u64,
    size: u64,
    crypt_method: u32,
    backing_file_offset: u64,
    l1_size: u32,
    l1_table_offset: u64,
    /// v3 only; zero for v2.
    incompatible_features: u64,
}

impl Qcow2Image {
    /// Open `path` as a qcow2 image.
    pub fn open(path: impl AsRef<Path>) -> CelResult<Self> {
        let path = path.as_ref();
        let mut f = File::open(path)
            .map_err(|e| CelError::Io(format!("open {}: {e}", path.display())))?;

        let header = parse_header(&mut f)?;
        validate_header(&header)?;

        // Read L1 table.
        let l1_bytes = header.l1_size as usize * 8;
        let mut raw = vec![0u8; l1_bytes];
        f.seek(SeekFrom::Start(header.l1_table_offset))
            .map_err(|e| CelError::Io(format!("seek l1 {}: {e}", path.display())))?;
        f.read_exact(&mut raw)
            .map_err(|e| CelError::Storage(format!("read l1 {}: {e}", path.display())))?;
        let mut l1 = Vec::with_capacity(header.l1_size as usize);
        for chunk in raw.chunks_exact(8) {
            // We just verified .chunks_exact(8) so this slice indexing
            // is safe (no panic possible).
            let mut b = [0u8; 8];
            b.copy_from_slice(chunk);
            l1.push(u64::from_be_bytes(b));
        }

        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(f),
            header,
            l1,
        })
    }

    /// Header version (2 or 3).
    #[must_use]
    pub fn version(&self) -> u32 { self.header.version }

    /// Cluster size in bytes.
    #[must_use]
    pub fn cluster_size(&self) -> u64 { self.header.cluster_size }

    /// Total number of L1 entries.
    #[must_use]
    pub fn l1_len(&self) -> usize { self.l1.len() }
}

fn parse_header(f: &mut File) -> CelResult<Header> {
    let mut buf = [0u8; 104]; // v3 minimum header length is 104 bytes.
    f.seek(SeekFrom::Start(0))
        .map_err(|e| CelError::Io(format!("seek 0: {e}")))?;
    let n = f.read(&mut buf)
        .map_err(|e| CelError::Io(format!("read header: {e}")))?;
    if n < 72 {
        return Err(CelError::Invalid("qcow2: header truncated"));
    }
    if &buf[0..4] != MAGIC {
        return Err(CelError::Invalid("qcow2: bad magic"));
    }
    let version = u32::from_be_bytes(buf[4..8].try_into().unwrap_or([0; 4]));
    if version != 2 && version != 3 {
        return Err(CelError::Invalid("qcow2: only v2/v3 supported"));
    }

    let backing_file_offset =
        u64::from_be_bytes(buf[8..16].try_into().unwrap_or([0; 8]));
    let cluster_bits = u32::from_be_bytes(buf[20..24].try_into().unwrap_or([0; 4]));
    let size         = u64::from_be_bytes(buf[24..32].try_into().unwrap_or([0; 8]));
    let crypt_method = u32::from_be_bytes(buf[32..36].try_into().unwrap_or([0; 4]));
    let l1_size      = u32::from_be_bytes(buf[36..40].try_into().unwrap_or([0; 4]));
    let l1_table_offset =
        u64::from_be_bytes(buf[40..48].try_into().unwrap_or([0; 8]));

    let incompatible_features = if version >= 3 && n >= 80 {
        u64::from_be_bytes(buf[72..80].try_into().unwrap_or([0; 8]))
    } else {
        0
    };

    let cluster_size = 1u64.checked_shl(cluster_bits)
        .ok_or(CelError::Invalid("qcow2: cluster_bits out of range"))?;

    Ok(Header {
        version,
        cluster_bits,
        cluster_size,
        size,
        crypt_method,
        backing_file_offset,
        l1_size,
        l1_table_offset,
        incompatible_features,
    })
}

fn validate_header(h: &Header) -> CelResult<()> {
    if !(9..=21).contains(&h.cluster_bits) {
        return Err(CelError::Invalid("qcow2: cluster_bits outside spec range 9..=21"));
    }
    if h.crypt_method != 0 {
        return Err(CelError::Invalid("qcow2: encrypted images not supported"));
    }
    if h.backing_file_offset != 0 {
        return Err(CelError::Invalid("qcow2: backing-file chains not supported"));
    }
    if h.l1_table_offset == 0 || h.l1_table_offset & 0x1ff != 0 {
        return Err(CelError::Invalid("qcow2: invalid l1_table_offset"));
    }
    if h.version >= 3 {
        // We only refuse bits we actually can't ignore. Extended L2
        // entries (bit 4) and compression-type (bit 3) change the
        // L2-entry encoding, so they must be supported, not ignored.
        const UNSUPPORTED: u64 = (1 << 3) | (1 << 4);
        if h.incompatible_features & UNSUPPORTED != 0 {
            return Err(CelError::Invalid(
                "qcow2: incompatible_features bit not supported (compress/extended-l2)",
            ));
        }
    }
    Ok(())
}

impl DiskImage for Qcow2Image {
    fn info(&self) -> ImageInfo {
        ImageInfo {
            format: FormatKind::Qcow2,
            virtual_size: self.header.size,
            cluster_size: Some(self.header.cluster_size),
            backend: if self.header.version == 3 { "qcow2-v3" } else { "qcow2-v2" },
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> CelResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if offset >= self.header.size {
            return Ok(0);
        }
        let remaining = self.header.size - offset;
        let max = (buf.len() as u64).min(remaining) as usize;

        let cs = self.header.cluster_size;
        let cluster_idx = offset / cs;
        let intra      = (offset % cs) as usize;
        let take       = max.min(cs as usize - intra);

        // L2 entries per cluster = cluster_size / 8.
        let l2_entries_per_cluster = cs / 8;
        let l1_index = (cluster_idx / l2_entries_per_cluster) as usize;
        let l2_index = (cluster_idx % l2_entries_per_cluster) as usize;

        let l1_entry = *self.l1.get(l1_index)
            .ok_or(CelError::Invalid("qcow2: l1 index past l1 table"))?;
        let l2_offset = l1_entry & L2_ENTRY_OFFSET_MASK;

        if l2_offset == 0 {
            // Whole L2 unallocated — region reads as zero.
            for b in &mut buf[..take] {
                *b = 0;
            }
            return Ok(take);
        }

        let mut entry_bytes = [0u8; 8];
        {
            let mut f = self.file.lock()
                .map_err(|_| CelError::Internal("qcow2: file mutex poisoned"))?;
            f.seek(SeekFrom::Start(l2_offset + (l2_index as u64) * 8))
                .map_err(|e| CelError::Io(format!("seek l2 {}: {e}", self.path.display())))?;
            f.read_exact(&mut entry_bytes)
                .map_err(|e| CelError::Storage(format!("read l2 {}: {e}", self.path.display())))?;
        }
        let l2_entry = u64::from_be_bytes(entry_bytes);

        // Compressed?  Refuse — Phase 1 doesn't decode.
        if l2_entry & L2_ENTRY_COMPRESSED != 0 {
            return Err(CelError::Invalid("qcow2: compressed cluster not supported"));
        }
        // v3 zero-cluster flag.
        if self.header.version >= 3 && l2_entry & L2_ENTRY_ZERO_CLUSTER != 0 {
            for b in &mut buf[..take] {
                *b = 0;
            }
            return Ok(take);
        }

        let data_offset = l2_entry & L2_ENTRY_OFFSET_MASK;
        if data_offset == 0 {
            for b in &mut buf[..take] {
                *b = 0;
            }
            return Ok(take);
        }

        let mut f = self.file.lock()
            .map_err(|_| CelError::Internal("qcow2: file mutex poisoned"))?;
        f.seek(SeekFrom::Start(data_offset + intra as u64))
            .map_err(|e| CelError::Io(format!("seek data {}: {e}", self.path.display())))?;

        let mut total = 0;
        while total < take {
            let n = f.read(&mut buf[total..take])
                .map_err(|e| CelError::Storage(format!("read data {}: {e}", self.path.display())))?;
            if n == 0 {
                break;
            }
            total += n;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Build a hand-rolled, fully-formed qcow2 v3 image with one
    /// allocated cluster (logical offset 0) containing the pattern
    /// `0x42` and the remaining clusters unallocated (zero-fill).
    ///
    /// Layout (cluster_size = 65 536):
    /// - cluster 0: header
    /// - cluster 1: L1 table (1 entry pointing at cluster 2)
    /// - cluster 2: L2 table (entry 0 points at cluster 3)
    /// - cluster 3: data (filled with 0x42)
    /// - cluster 4: refcount table (single entry pointing at cluster 5)
    /// - cluster 5: refcount block
    ///
    /// Virtual size = 2 * cluster_size so we exercise the
    /// unallocated-second-cluster path too.
    fn synth_qcow2_v3() -> tempfile::NamedTempFile {
        const CB: u32 = 16;
        const CS: u64 = 1 << CB; // 65 536
        let mut file = vec![0u8; (6 * CS) as usize];

        // ---- header --------------------------------------------------
        file[0..4].copy_from_slice(b"QFI\xfb");
        file[4..8].copy_from_slice(&3u32.to_be_bytes());              // version
        // backing_file_offset = 0 already
        // backing_file_size = 0 already
        file[20..24].copy_from_slice(&CB.to_be_bytes());              // cluster_bits
        let virtual_size = 2 * CS;
        file[24..32].copy_from_slice(&virtual_size.to_be_bytes());    // size
        // crypt_method = 0 already
        file[36..40].copy_from_slice(&1u32.to_be_bytes());            // l1_size
        file[40..48].copy_from_slice(&CS.to_be_bytes());              // l1_table_offset
        let refcount_table_offset = 4 * CS;
        file[48..56].copy_from_slice(&refcount_table_offset.to_be_bytes()); // refcount_table_offset
        file[56..60].copy_from_slice(&1u32.to_be_bytes());            // refcount_table_clusters
        // nb_snapshots = 0, snapshots_offset = 0
        file[72..80].copy_from_slice(&0u64.to_be_bytes());            // incompatible_features
        file[80..88].copy_from_slice(&0u64.to_be_bytes());            // compatible_features
        file[88..96].copy_from_slice(&0u64.to_be_bytes());            // autoclear_features
        file[96..100].copy_from_slice(&4u32.to_be_bytes());           // refcount_order
        file[100..104].copy_from_slice(&104u32.to_be_bytes());        // header_length

        // ---- L1 table at cluster 1 (file offset CS) -----------------
        // Entry 0 -> L2 cluster at CS*2.
        // High bit (63) = copied flag; not required for reads.
        let l2_offset = 2 * CS;
        file[CS as usize .. CS as usize + 8].copy_from_slice(&l2_offset.to_be_bytes());

        // ---- L2 table at cluster 2 (file offset 2*CS) ---------------
        // Entry 0 -> data cluster at 3*CS.
        let data_offset = 3 * CS;
        let l2_base = (2 * CS) as usize;
        file[l2_base .. l2_base + 8].copy_from_slice(&data_offset.to_be_bytes());

        // ---- data cluster: fill with 0x42 ---------------------------
        for b in &mut file[(3 * CS) as usize .. (4 * CS) as usize] {
            *b = 0x42;
        }

        // ---- refcount table at cluster 4 ----------------------------
        // Single entry -> refcount block at cluster 5.
        let refcount_block_offset = 5 * CS;
        let rt_base = (4 * CS) as usize;
        file[rt_base .. rt_base + 8].copy_from_slice(&refcount_block_offset.to_be_bytes());

        // refcount block (cluster 5): mark clusters 0..6 as refcount=1
        // with refcount_order = 4 -> 16-bit entries.
        let rb_base = (5 * CS) as usize;
        for i in 0..6 {
            let one = 1u16.to_be_bytes();
            file[rb_base + i * 2 .. rb_base + i * 2 + 2].copy_from_slice(&one);
        }

        let mut t = tempfile::NamedTempFile::new().unwrap();
        t.write_all(&file).unwrap();
        t.flush().unwrap();
        t
    }

    #[test]
    fn opens_v3_header_and_reports_info() {
        let t = synth_qcow2_v3();
        let img = Qcow2Image::open(t.path()).unwrap();
        assert_eq!(img.version(), 3);
        assert_eq!(img.cluster_size(), 65_536);
        let info = img.info();
        assert_eq!(info.format, FormatKind::Qcow2);
        assert_eq!(info.virtual_size, 2 * 65_536);
        assert_eq!(info.cluster_size, Some(65_536));
        assert_eq!(info.backend, "qcow2-v3");
    }

    #[test]
    fn reads_allocated_cluster() {
        let t = synth_qcow2_v3();
        let img = Qcow2Image::open(t.path()).unwrap();
        let mut buf = [0u8; 32];
        let n = img.read_at(0, &mut buf).unwrap();
        assert_eq!(n, 32);
        assert!(buf.iter().all(|&b| b == 0x42));
    }

    #[test]
    fn reads_unallocated_cluster_as_zero() {
        let t = synth_qcow2_v3();
        let img = Qcow2Image::open(t.path()).unwrap();
        // Second cluster (offset 65 536) has no L2 entry -> zero fill.
        let mut buf = [0xCC; 64];
        let n = img.read_at(65_536, &mut buf).unwrap();
        assert_eq!(n, 64);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn read_clips_at_virtual_size() {
        let t = synth_qcow2_v3();
        let img = Qcow2Image::open(t.path()).unwrap();
        let mut buf = [1u8; 16];
        // virtual_size == 2 * 65_536; reading at exactly that boundary
        // must return 0 bytes without error.
        let n = img.read_at(2 * 65_536, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut t = tempfile::NamedTempFile::new().unwrap();
        t.write_all(&[0u8; 256]).unwrap();
        t.flush().unwrap();
        let Err(err) = Qcow2Image::open(t.path()) else { panic!("bad magic should be rejected") };
        assert_eq!(err.code(), "invalid");
    }
}
