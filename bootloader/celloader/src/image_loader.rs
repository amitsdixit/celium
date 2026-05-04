//! Loads the CelHyper kernel image from the EFI System Partition.
//!
//! Path: `\EFI\CELIUM\CELHYPER.ELF` on the same volume as the loader image.
//! In Week-1 we load the file into a UEFI allocation and verify it is plausible
//! (non-empty, ELF magic). Real ELF parsing + relocation is a Week-2 task.

use alloc::vec::Vec;
use uefi::CString16;
use uefi::boot;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, FileAttribute, FileMode, RegularFile};
use uefi::proto::media::fs::SimpleFileSystem;

const KERNEL_PATH: &str = "\\EFI\\CELIUM\\CELHYPER.ELF";

/// A loaded CelHyper image, owned by an alloc-backed `Vec` so it lives until
/// we exit boot services and copy it to its final physical location.
pub struct LoadedKernel {
    /// Raw image bytes.
    pub bytes: Vec<u8>,
}

/// Errors specific to image loading. Mapped to `Status` at the call site.
#[derive(Debug)]
pub enum LoadError {
    /// The loaded-image protocol could not be opened.
    NoLoadedImage,
    /// The boot volume's filesystem could not be opened.
    NoFilesystem,
    /// The kernel file was not found at [`KERNEL_PATH`].
    NotFound,
    /// The file existed but was empty or impossibly small.
    Truncated,
    /// File contents do not begin with the ELF magic `\x7FELF`.
    NotElf,
    /// A general UEFI failure during read.
    Io,
}

/// Open the boot volume, read `KERNEL_PATH`, and return the bytes.
pub fn load_celhyper() -> Result<LoadedKernel, LoadError> {
    // 1. Find the device handle of the boot volume via LoadedImage.
    let li_handle = boot::image_handle();
    let loaded_image =
        boot::open_protocol_exclusive::<LoadedImage>(li_handle).map_err(|_| LoadError::NoLoadedImage)?;
    let device = loaded_image.device().ok_or(LoadError::NoLoadedImage)?;

    // 2. Open SimpleFileSystem on that device and the volume root.
    let mut sfs =
        boot::open_protocol_exclusive::<SimpleFileSystem>(device).map_err(|_| LoadError::NoFilesystem)?;
    let mut root = sfs.open_volume().map_err(|_| LoadError::NoFilesystem)?;

    // 3. Open the kernel file read-only.
    let path = CString16::try_from(KERNEL_PATH).map_err(|_| LoadError::NotFound)?;
    let handle = root
        .open(&path, FileMode::Read, FileAttribute::empty())
        .map_err(|_| LoadError::NotFound)?;
    let mut file: RegularFile = handle.into_regular_file().ok_or(LoadError::NotFound)?;

    // 4. Read the file in 64 KiB chunks into a Vec.
    let mut bytes: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|_| LoadError::Io)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }

    if bytes.len() < 64 {
        return Err(LoadError::Truncated);
    }
    if &bytes[0..4] != b"\x7FELF" {
        return Err(LoadError::NotElf);
    }

    Ok(LoadedKernel { bytes })
}

/// Read the ELF entry point (`e_entry`) from a 64-bit ELF header.
///
/// Returns `None` if `bytes` is not a valid ELF64 little-endian header. We
/// only accept ELF64 LE because CelHyper targets `x86_64-unknown-none`.
#[must_use]
pub fn parse_entry_point(bytes: &[u8]) -> Option<u64> {
    // ELF header layout (Vol 1 §3): magic[4], class(1), data(1), version(1),
    // osabi(1), abiversion(1), pad[7], type(2), machine(2), version(4),
    // entry(8) — at offset 24 for ELF64.
    if bytes.len() < 0x18 + 8 {
        return None;
    }
    if &bytes[0..4] != b"\x7FELF" {
        return None;
    }
    if bytes[4] != 2 {
        return None; // class: ELF64
    }
    if bytes[5] != 1 {
        return None; // data: little-endian
    }
    let mut e_entry = [0u8; 8];
    e_entry.copy_from_slice(&bytes[0x18..0x20]);
    Some(u64::from_le_bytes(e_entry))
}
