//! Host-side **boot-blob staging**: the W18.3 bridge between the
//! `celimage` reader trait and the bare-metal `celhyper` guest loader.
//!
//! ## Why this exists
//!
//! The kernel-side `celhyper::manager::CreateVmRequest` accepts a
//! single small `blob: &[u8]` that gets copied to GPA `0x1000` before
//! the first `vmlaunch`. Today the only blob the kernel ever sees is
//! the canned `HELLO_BLOB`. W18.3 lets the operator say "boot this
//! disk image" by:
//!
//! 1. Opening the image through [`celimage::open`] (which works for
//!    raw, qcow2, VMDK monolithicSparse and VHDX fixed/dynamic).
//! 2. Reading the first [`BOOT_BLOB_LEN`] bytes via the
//!    [`celimage::DiskImage`] trait, which transparently handles
//!    sparse/unallocated clusters.
//! 3. Persisting that page as `<stage_root>/vm-<id>/boot.blob` so
//!    the supervisor (or a future RPC client of `celhyper`) can hand
//!    the file off to the hypervisor without re-running format
//!    detection.
//! 4. Returning a [`BootDigest`] that the controller stamps onto the
//!    `VmRecord`. The digest lets future restarts notice if the
//!    backing image was swapped out from under us.
//!
//! Read-only: this module never opens images for write and never
//! touches the original image file. It only writes the staged blob.
//!
//! ## Conventions
//!
//! - Every fallible path returns [`celcommon::CelResult`].
//! - No `unwrap()` / `panic!()` on production paths.
//! - No `unsafe` (guaranteed by the crate-level `forbid(unsafe_code)`
//!   in `main.rs`).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use celcommon::{CelError, CelResult};

/// Number of bytes copied out of the image's logical offset 0 and
/// written to the staged `boot.blob`. One 4 KiB page is the minimum
/// useful unit (covers the MBR, the BIOS Parameter Block, and most
/// of the first sector of an EFI System Partition's bootloader).
pub const BOOT_BLOB_LEN: usize = 4096;

/// Fingerprint of a staged boot blob.
///
/// The CRC-32C is computed over the *exact* bytes written to the
/// staging file. Stored on the [`crate::vm::VmRecord`] so that a
/// subsequent restart can detect a silently swapped backing image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootDigest {
    /// Number of bytes copied (≤ [`BOOT_BLOB_LEN`]; may be smaller
    /// for tiny synthetic test images).
    pub blob_len: u64,
    /// Castagnoli CRC-32C of the staged bytes.
    pub crc32c: u32,
    /// Absolute path of the staged blob on disk.
    pub blob_path: PathBuf,
}

/// Stage the boot blob for `vm_id` from `image_path` under
/// `stage_root`.
///
/// Writes `<stage_root>/vm-<id>/boot.blob`, creating the directory
/// chain if needed. Returns the resulting [`BootDigest`].
///
/// # Errors
///
/// - [`CelError::Invalid`] if the image's logical size is zero (no
///   bytes to stage).
/// - [`CelError::Io`] for any filesystem error while creating the
///   directory or writing the blob.
/// - Whatever error the [`celimage`] backend surfaces from
///   [`celimage::open`] or `read_exact_at` (bad magic, malformed
///   header, short reads, …).
pub fn stage_boot_blob(
    image_path: &Path,
    stage_root: &Path,
    vm_id: u32,
) -> CelResult<BootDigest> {
    let image = celimage::open(image_path)?;
    let virt = image.virtual_size();
    if virt == 0 {
        return Err(CelError::Invalid(
            "boot blob: image has zero virtual size",
        ));
    }

    // Tiny synthetic images (a few hundred bytes — used by some
    // celimage unit tests) are legal here; clip the request rather
    // than failing.
    let want = core::cmp::min(virt, BOOT_BLOB_LEN as u64) as usize;
    let mut buf = vec![0u8; want];
    image.read_exact_at(0, &mut buf)?;

    let crc = celimage::crc32c(&buf);

    let dir = stage_root.join(format!("vm-{vm_id}"));
    fs::create_dir_all(&dir)
        .map_err(|e| CelError::Io(format!("mkdir {}: {e}", dir.display())))?;
    let blob_path = dir.join("boot.blob");

    // W19 Phase B: crash-safe write. Stage to `<blob>.tmp.<pid>`,
    // fsync, then atomically rename over `boot.blob`. A crash mid-
    // write leaves the *previous* good blob in place (or nothing on
    // a first stage) — never a partial file that would later cause a
    // bogus drift CRC. `fs::rename` is atomic over an existing
    // destination on every supported platform.
    let tmp_path = dir.join(format!("boot.blob.tmp.{}", std::process::id()));
    {
        let mut f = File::create(&tmp_path)
            .map_err(|e| CelError::Io(format!("create {}: {e}", tmp_path.display())))?;
        f.write_all(&buf)
            .map_err(|e| CelError::Io(format!("write {}: {e}", tmp_path.display())))?;
        f.sync_all()
            .map_err(|e| CelError::Io(format!("fsync {}: {e}", tmp_path.display())))?;
    } // close before rename — required on Windows.
    fs::rename(&tmp_path, &blob_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        CelError::Io(format!(
            "rename {} -> {}: {e}",
            tmp_path.display(),
            blob_path.display(),
        ))
    })?;

    tracing::info!(
        image = %image_path.display(),
        stage = %blob_path.display(),
        len = want,
        crc32c = format!("{crc:08x}"),
        "boot blob: staged",
    );

    Ok(BootDigest {
        blob_len: want as u64,
        crc32c: crc,
        blob_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a tiny "raw" image (4 KiB, byte-pattern 0xC3 = `ret`) to
    /// `dir/raw.img` and return its path. `celimage::detect_format`
    /// classifies anything that fails every magic sniff as `Raw`, so
    /// we get a guaranteed-stable raw backend.
    fn synth_raw_image(dir: &Path) -> PathBuf {
        let p = dir.join("raw.img");
        let mut f = File::create(&p).unwrap();
        f.write_all(&[0xC3u8; 4096]).unwrap();
        f.sync_all().unwrap();
        p
    }

    #[test]
    fn stages_full_page_for_4kib_raw_image() {
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw_image(tmp.path());
        let stage = tmp.path().join("stage");
        let d = stage_boot_blob(&img, &stage, 2).unwrap();

        assert_eq!(d.blob_len, BOOT_BLOB_LEN as u64);
        assert_eq!(d.blob_path, stage.join("vm-2").join("boot.blob"));
        // CRC of 4096 bytes of 0xC3 is deterministic; spot-check by
        // round-tripping through the same routine the impl uses.
        assert_eq!(d.crc32c, celimage::crc32c(&[0xC3u8; 4096]));

        let on_disk = std::fs::read(&d.blob_path).unwrap();
        assert_eq!(on_disk.len(), BOOT_BLOB_LEN);
        assert!(on_disk.iter().all(|&b| b == 0xC3));
    }

    #[test]
    fn clips_request_to_virtual_size_for_tiny_image() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("tiny.img");
        // 512 bytes — smaller than BOOT_BLOB_LEN.
        std::fs::write(&p, [0x90u8; 512]).unwrap();

        let stage = tmp.path().join("stage");
        let d = stage_boot_blob(&p, &stage, 0).unwrap();
        assert_eq!(d.blob_len, 512);
        assert_eq!(std::fs::metadata(&d.blob_path).unwrap().len(), 512);
    }

    #[test]
    fn missing_image_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.img");
        let stage = tmp.path().join("stage");
        let err = stage_boot_blob(&missing, &stage, 0).unwrap_err();
        // Backend reports filesystem failure as Io; detect_format
        // also surfaces it as Io. Either is fine; assert it's not a
        // silent success.
        assert!(
            matches!(err, CelError::Io(_) | CelError::Invalid(_)),
            "unexpected error variant: {err:?}",
        );
    }

    #[test]
    fn restage_overwrites_previous_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let img1 = tmp.path().join("a.img");
        std::fs::write(&img1, [0x11u8; 4096]).unwrap();
        let img2 = tmp.path().join("b.img");
        std::fs::write(&img2, [0x22u8; 4096]).unwrap();
        let stage = tmp.path().join("stage");

        let d1 = stage_boot_blob(&img1, &stage, 3).unwrap();
        let d2 = stage_boot_blob(&img2, &stage, 3).unwrap();
        assert_ne!(d1.crc32c, d2.crc32c);
        // Same on-disk path; content reflects the second stage.
        assert_eq!(d1.blob_path, d2.blob_path);
        let bytes = std::fs::read(&d2.blob_path).unwrap();
        assert!(bytes.iter().all(|&b| b == 0x22));
    }

    #[test]
    fn stage_leaves_no_tmp_sidecar_on_success() {
        // W19 Phase B: the atomic-write path stages to
        // `boot.blob.tmp.<pid>` then renames over. A successful
        // stage must not leave the sidecar behind.
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw_image(tmp.path());
        let stage = tmp.path().join("stage");
        let d = stage_boot_blob(&img, &stage, 7).unwrap();

        let dir = d.blob_path.parent().unwrap();
        let stale: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(stale.is_empty(), "leaked tmp files: {stale:?}");

        // Only the final blob exists in the vm-7 directory.
        let entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["boot.blob"], "got: {entries:?}");
    }
}
