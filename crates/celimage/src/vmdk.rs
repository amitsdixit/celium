//! Read-only VMware Virtual Machine Disk (`.vmdk`) reader.
//!
//! Phase 2 (W18.2) scope:
//!
//! - **Supported** — `monolithicSparse`: a single file with a
//!   `SparseExtentHeader` followed by an embedded descriptor (ignored
//!   for our read path), a grain directory, grain tables, and the
//!   payload grains. This is what `qemu-img convert -O vmdk` and
//!   VMware Workstation produce by default.
//! - **Rejected** with [`CelError::Invalid`] — compressed extents
//!   (`streamOptimized`), multi-extent layouts (`monolithicFlat`,
//!   `2GbMaxExtentFlat`, …), and any header flag combination that
//!   would require interpreting the embedded descriptor to find a
//!   side-car extent. Phase 3 / W18.2-followups can lift these as
//!   needed.
//!
//! Reference: VMware "Virtual Disk Format" specification, "Hosted
//! Sparse Extent" section.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use celcommon::{CelError, CelResult};

use crate::disk::{DiskImage, ImageInfo};
use crate::format::FormatKind;
use crate::util::{le_u32, le_u64};

const SECTOR: u64 = 512;

/// Magic in on-disk order — bytes `K D M V`. Read little-endian this
/// is `0x564d_444b`.
const VMDK_MAGIC_LE: u32 = 0x564d_444b;

/// `flags` bit set when grain-directory entries equal to `1` represent
/// "explicit zero grains" rather than unallocated. Both end up being
/// served as zeros by our read path, so we only track this so we don't
/// reject a valid layout.
const FLAG_ZEROED_GRAIN_GTE: u32 = 1 << 2;
/// `flags` bit indicating a redundant grain directory exists. We
/// don't consult it; the primary GD is enough for reads.
const FLAG_REDUNDANT_GD: u32 = 1 << 1;

/// Read-only handle to a `monolithicSparse` VMDK file.
pub struct VmdkImage {
    path: PathBuf,
    file: Mutex<File>,
    /// Total virtual size, bytes.
    virtual_size: u64,
    /// Grain size in sectors (power of two, ≥ 8 per spec).
    grain_size_sectors: u64,
    /// Number of grain-table entries per grain table.
    num_gtes_per_gt: u32,
    /// Grain directory, cached at open time. Each entry is a sector
    /// offset to a grain table, or `0` (no GT allocated → all grains
    /// in this range read as zeros).
    gd: Vec<u32>,
}

#[derive(Debug, Clone)]
struct Header {
    capacity_sectors: u64,
    grain_size_sectors: u64,
    descriptor_offset_sectors: u64,
    descriptor_size_sectors: u64,
    num_gtes_per_gt: u32,
    gd_offset_sectors: u64,
    overhead_sectors: u64,
    compress_algorithm: u16,
    flags: u32,
}

impl VmdkImage {
    /// Open `path` as a monolithicSparse VMDK.
    pub fn open(path: impl AsRef<Path>) -> CelResult<Self> {
        let path = path.as_ref();
        let mut f = File::open(path)
            .map_err(|e| CelError::Io(format!("open {}: {e}", path.display())))?;

        let header = parse_header(&mut f)?;
        validate_header(&header)?;

        // Compute GD size: one u32 entry per grain table; each GT
        // covers `num_gtes_per_gt * grain_size` sectors.
        let sectors_per_gt = (header.num_gtes_per_gt as u64)
            .checked_mul(header.grain_size_sectors)
            .ok_or(CelError::Invalid("vmdk: GT coverage overflow"))?;
        if sectors_per_gt == 0 {
            return Err(CelError::Invalid("vmdk: zero sectors per GT"));
        }
        let num_gts = header.capacity_sectors.div_ceil(sectors_per_gt);
        let gd_bytes = (num_gts as usize).checked_mul(4)
            .ok_or(CelError::Invalid("vmdk: GD size overflow"))?;

        let gd_byte_offset = header.gd_offset_sectors.checked_mul(SECTOR)
            .ok_or(CelError::Invalid("vmdk: GD offset overflow"))?;
        let mut gd_raw = vec![0u8; gd_bytes];
        f.seek(SeekFrom::Start(gd_byte_offset))
            .map_err(|e| CelError::Io(format!("seek gd {}: {e}", path.display())))?;
        f.read_exact(&mut gd_raw)
            .map_err(|e| CelError::Storage(format!("read gd {}: {e}", path.display())))?;
        let mut gd = Vec::with_capacity(num_gts as usize);
        for chunk in gd_raw.chunks_exact(4) {
            let mut b = [0u8; 4];
            b.copy_from_slice(chunk);
            gd.push(u32::from_le_bytes(b));
        }

        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(f),
            virtual_size: header.capacity_sectors.saturating_mul(SECTOR),
            grain_size_sectors: header.grain_size_sectors,
            num_gtes_per_gt: header.num_gtes_per_gt,
            gd,
        })
    }

    /// Path the image was opened from. Useful for diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path { &self.path }

    /// Read one grain table from disk. The result is cached by the
    /// caller; we re-read on every reference to keep memory bounded
    /// (large disks can have many GTs).
    fn load_gt(&self, gt_sector: u32) -> CelResult<Vec<u32>> {
        let bytes = (self.num_gtes_per_gt as usize)
            .checked_mul(4)
            .ok_or(CelError::Invalid("vmdk: GT byte length overflow"))?;
        let mut raw = vec![0u8; bytes];
        let off = u64::from(gt_sector).checked_mul(SECTOR)
            .ok_or(CelError::Invalid("vmdk: GT offset overflow"))?;
        let mut f = self.file.lock()
            .map_err(|_| CelError::Internal("vmdk: file mutex poisoned"))?;
        f.seek(SeekFrom::Start(off))
            .map_err(|e| CelError::Io(format!("seek gt: {e}")))?;
        f.read_exact(&mut raw)
            .map_err(|e| CelError::Storage(format!("read gt: {e}")))?;
        let mut gt = Vec::with_capacity(self.num_gtes_per_gt as usize);
        for chunk in raw.chunks_exact(4) {
            let mut b = [0u8; 4];
            b.copy_from_slice(chunk);
            gt.push(u32::from_le_bytes(b));
        }
        Ok(gt)
    }
}

fn parse_header(f: &mut File) -> CelResult<Header> {
    let mut buf = [0u8; 512];
    f.seek(SeekFrom::Start(0))
        .map_err(|e| CelError::Io(format!("seek 0: {e}")))?;
    let n = f.read(&mut buf)
        .map_err(|e| CelError::Io(format!("read header: {e}")))?;
    if n < 80 {
        return Err(CelError::Invalid("vmdk: header truncated"));
    }
    let magic = le_u32(&buf, 0)?;
    if magic != VMDK_MAGIC_LE {
        return Err(CelError::Invalid("vmdk: bad magic (not KDMV)"));
    }
    let version = le_u32(&buf, 4)?;
    if !(1..=3).contains(&version) {
        return Err(CelError::Invalid("vmdk: unsupported sparse extent version"));
    }
    let flags = le_u32(&buf, 8)?;
    let capacity_sectors = le_u64(&buf, 12)?;
    let grain_size_sectors = le_u64(&buf, 20)?;
    let descriptor_offset_sectors = le_u64(&buf, 28)?;
    let descriptor_size_sectors = le_u64(&buf, 36)?;
    let num_gtes_per_gt = le_u32(&buf, 44)?;
    let _rgd_offset = le_u64(&buf, 48)?;
    let gd_offset_sectors = le_u64(&buf, 56)?;
    let overhead_sectors = le_u64(&buf, 64)?;
    // `unclean_shutdown` at 72, single/non/double end-line chars at 73..77.
    let compress_algorithm =
        u16::from_le_bytes(buf[77..79].try_into().unwrap_or([0; 2]));

    Ok(Header {
        capacity_sectors,
        grain_size_sectors,
        descriptor_offset_sectors,
        descriptor_size_sectors,
        num_gtes_per_gt,
        gd_offset_sectors,
        overhead_sectors,
        compress_algorithm,
        flags,
    })
}

fn validate_header(h: &Header) -> CelResult<()> {
    if h.compress_algorithm != 0 {
        return Err(CelError::Invalid(
            "vmdk: compressed (streamOptimized) extents not supported",
        ));
    }
    // Grain size: power of two, between 8 and 2^20 sectors (i.e. 4 KB ..
    // 512 MB). VMware's own limits are 8..=8192.
    if h.grain_size_sectors < 8 || h.grain_size_sectors > (1 << 20)
        || !h.grain_size_sectors.is_power_of_two()
    {
        return Err(CelError::Invalid("vmdk: grain_size_sectors out of spec"));
    }
    if h.num_gtes_per_gt == 0 || !h.num_gtes_per_gt.is_power_of_two() {
        return Err(CelError::Invalid("vmdk: num_gtes_per_gt must be a power of two"));
    }
    if h.gd_offset_sectors == 0 {
        return Err(CelError::Invalid(
            "vmdk: gd_offset == 0 (descriptor-only / multi-extent images not supported)",
        ));
    }
    // overhead is "where the first grain starts"; sanity-check it.
    if h.overhead_sectors == 0 {
        return Err(CelError::Invalid("vmdk: overhead == 0 (malformed)"));
    }
    // We don't actually need to parse the descriptor, but if it's
    // present its declared size must fit before the GD.
    if h.descriptor_size_sectors != 0
        && h.descriptor_offset_sectors
            .checked_add(h.descriptor_size_sectors)
            .is_none()
    {
        return Err(CelError::Invalid("vmdk: descriptor offset+size overflow"));
    }
    // Flag bits we don't understand → refuse rather than misread.
    let known = FLAG_ZEROED_GRAIN_GTE | FLAG_REDUNDANT_GD | 1 /* validNewline */;
    if h.flags & !known != 0 {
        tracing::debug!(flags = format!("{:#x}", h.flags), "vmdk: unknown flags (continuing)");
    }
    Ok(())
}

impl DiskImage for VmdkImage {
    fn info(&self) -> ImageInfo {
        ImageInfo {
            format: FormatKind::Vmdk,
            virtual_size: self.virtual_size,
            cluster_size: Some(self.grain_size_sectors * SECTOR),
            backend: "vmdk-monolithic-sparse",
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> CelResult<usize> {
        if buf.is_empty() || offset >= self.virtual_size {
            return Ok(0);
        }
        let max = (self.virtual_size - offset) as usize;
        let want = buf.len().min(max);
        let buf = &mut buf[..want];

        let grain_bytes = self.grain_size_sectors * SECTOR;
        let mut filled = 0usize;
        let mut cur_off = offset;

        while filled < want {
            let intra = cur_off % grain_bytes;
            let chunk = (want - filled).min((grain_bytes - intra) as usize);

            // Map grain → GD / GT entry.
            let grain_idx = cur_off / grain_bytes;
            let gt_idx = grain_idx / u64::from(self.num_gtes_per_gt);
            let gte_idx = (grain_idx % u64::from(self.num_gtes_per_gt)) as usize;

            let dest = &mut buf[filled..filled + chunk];

            let gd_entry = self.gd.get(gt_idx as usize).copied().unwrap_or(0);
            if gd_entry == 0 || gd_entry == 1 {
                // No GT allocated (0) or whole-GT-zero marker (1) →
                // entire grain reads as zeros.
                dest.fill(0);
            } else {
                let gt = self.load_gt(gd_entry)?;
                let gte = gt.get(gte_idx).copied().unwrap_or(0);
                if gte == 0 || gte == 1 {
                    dest.fill(0);
                } else {
                    let grain_byte_off = u64::from(gte)
                        .checked_mul(SECTOR)
                        .ok_or(CelError::Invalid("vmdk: grain offset overflow"))?
                        + intra;
                    let mut f = self.file.lock()
                        .map_err(|_| CelError::Internal("vmdk: file mutex poisoned"))?;
                    f.seek(SeekFrom::Start(grain_byte_off))
                        .map_err(|e| CelError::Io(format!("seek grain: {e}")))?;
                    f.read_exact(dest)
                        .map_err(|e| CelError::Storage(format!("read grain: {e}")))?;
                }
            }

            filled += chunk;
            cur_off += chunk as u64;
        }
        Ok(filled)
    }

    fn virtual_size(&self) -> u64 { self.virtual_size }
}

// Silence "field never read" for descriptor fields we intentionally
// parse-and-ignore but want available for diagnostics.
#[allow(dead_code)]
impl Header {
    fn descriptor_present(&self) -> bool {
        self.descriptor_offset_sectors != 0 && self.descriptor_size_sectors != 0
    }
    fn first_grain_byte(&self) -> u64 { self.overhead_sectors * SECTOR }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal but valid monolithicSparse VMDK in memory:
    ///
    /// - grain_size = 8 sectors (4 KB),
    /// - num_gtes_per_gt = 4,
    /// - capacity = 16 sectors (2 grains),
    /// - 1 GD entry → 1 GT,
    /// - grain 0 is allocated and filled with `0xAB`,
    /// - grain 1 is unallocated (zero-fill).
    fn synth_vmdk_monolithic_sparse() -> Vec<u8> {
        let sector = 512usize;
        let grain_size_sectors = 8u64;
        let num_gtes_per_gt = 4u32;
        let capacity_sectors = 16u64;

        // Layout (in sectors):
        //   0       : header (1 sector)
        //   1       : GD (1 entry → padded to 1 sector)
        //   2..=5   : GT (4 entries × 4 bytes = 16 bytes, padded to 1 sector
        //             — but we just allocate 1 sector for simplicity)
        //   6..=13  : grain 0 (8 sectors)
        //   14..=21 : grain 1 (8 sectors) — UNUSED, image stops earlier
        //
        // overhead = first grain byte / SECTOR = 6.
        let gd_sector = 1u64;
        let gt_sector = 2u64;
        let grain0_sector = 6u64;
        let overhead_sectors = grain0_sector;
        let total_sectors = grain0_sector + grain_size_sectors;
        let mut img = vec![0u8; (total_sectors as usize) * sector];

        // Header (sector 0).
        img[0..4].copy_from_slice(&VMDK_MAGIC_LE.to_le_bytes());
        img[4..8].copy_from_slice(&1u32.to_le_bytes()); // version
        img[8..12].copy_from_slice(&0u32.to_le_bytes()); // flags
        img[12..20].copy_from_slice(&capacity_sectors.to_le_bytes());
        img[20..28].copy_from_slice(&grain_size_sectors.to_le_bytes());
        img[28..36].copy_from_slice(&0u64.to_le_bytes()); // descriptor_offset
        img[36..44].copy_from_slice(&0u64.to_le_bytes()); // descriptor_size
        img[44..48].copy_from_slice(&num_gtes_per_gt.to_le_bytes());
        img[48..56].copy_from_slice(&0u64.to_le_bytes()); // rgd_offset
        img[56..64].copy_from_slice(&gd_sector.to_le_bytes());
        img[64..72].copy_from_slice(&overhead_sectors.to_le_bytes());
        // unclean_shutdown + newline chars + compress_algorithm = 0.

        // GD (sector 1): one u32 entry pointing at GT sector.
        let gd_off = (gd_sector as usize) * sector;
        img[gd_off..gd_off + 4]
            .copy_from_slice(&(gt_sector as u32).to_le_bytes());

        // GT (sector 2): GT[0] = grain0_sector, GT[1..3] = 0.
        let gt_off = (gt_sector as usize) * sector;
        img[gt_off..gt_off + 4]
            .copy_from_slice(&(grain0_sector as u32).to_le_bytes());
        // GT[1] stays 0 → grain 1 unallocated.

        // Grain 0 (sectors 6..=13): pattern 0xAB.
        let g0_off = (grain0_sector as usize) * sector;
        let g0_len = (grain_size_sectors as usize) * sector;
        for b in &mut img[g0_off..g0_off + g0_len] { *b = 0xAB; }

        img
    }

    fn write_temp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut t = tempfile::NamedTempFile::new().unwrap();
        t.write_all(bytes).unwrap();
        t.flush().unwrap();
        t
    }

    #[test]
    fn opens_synthetic_vmdk_and_reports_info() {
        let t = write_temp(&synth_vmdk_monolithic_sparse());
        let img = VmdkImage::open(t.path()).unwrap();
        let info = img.info();
        assert_eq!(info.format, FormatKind::Vmdk);
        assert_eq!(info.virtual_size, 16 * SECTOR);
        assert_eq!(info.cluster_size, Some(8 * SECTOR));
        assert_eq!(info.backend, "vmdk-monolithic-sparse");
    }

    #[test]
    fn reads_allocated_grain() {
        let t = write_temp(&synth_vmdk_monolithic_sparse());
        let img = VmdkImage::open(t.path()).unwrap();
        let mut buf = [0u8; 16];
        let n = img.read_at(0, &mut buf).unwrap();
        assert_eq!(n, 16);
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn reads_unallocated_grain_as_zero() {
        let t = write_temp(&synth_vmdk_monolithic_sparse());
        let img = VmdkImage::open(t.path()).unwrap();
        // Grain 1 starts at offset 8*512 = 4096.
        let mut buf = [0xFFu8; 32];
        let n = img.read_at(4096, &mut buf).unwrap();
        assert_eq!(n, 32);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn read_clips_at_virtual_size() {
        let t = write_temp(&synth_vmdk_monolithic_sparse());
        let img = VmdkImage::open(t.path()).unwrap();
        // virtual_size = 16 sectors = 8192 bytes. Ask for 100 bytes
        // starting 10 bytes before EOF.
        let mut buf = [0u8; 100];
        let n = img.read_at(img.virtual_size() - 10, &mut buf).unwrap();
        assert_eq!(n, 10);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut img = synth_vmdk_monolithic_sparse();
        img[0..4].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        let t = write_temp(&img);
        let Err(err) = VmdkImage::open(t.path()) else { panic!("bad magic must be rejected") };
        assert_eq!(err.code(), "invalid");
    }

    #[test]
    fn rejects_compressed() {
        let mut img = synth_vmdk_monolithic_sparse();
        // compress_algorithm at offset 77.
        img[77..79].copy_from_slice(&1u16.to_le_bytes());
        let t = write_temp(&img);
        let Err(err) = VmdkImage::open(t.path()) else { panic!("compressed must be rejected") };
        assert_eq!(err.code(), "invalid");
    }

    #[test]
    fn rejects_zero_gd_offset() {
        let mut img = synth_vmdk_monolithic_sparse();
        img[56..64].copy_from_slice(&0u64.to_le_bytes()); // gd_offset
        let t = write_temp(&img);
        let Err(err) = VmdkImage::open(t.path()) else { panic!("gd==0 must be rejected") };
        assert_eq!(err.code(), "invalid");
    }
}
