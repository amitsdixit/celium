//! Celium disk-image readers.
//!
//! Phase-2 (W18.2) goal: raw / qcow2 / VMDK monolithicSparse / VHDX
//! (dynamic + fixed, no differencing) all served behind one trait so
//! the rest of the platform can consume them without knowing the
//! on-disk format.
//!
//! ## Surface
//!
//! - [`DiskImage`] — trait every backend implements.
//! - [`FormatKind`] — narrow enum identifying the on-disk format.
//! - [`detect_format`] / [`open`] — sniff a path and return a boxed
//!   reader.
//! - [`RawImage`] — passthrough reader for raw / `.img` files.
//! - [`Qcow2Image`] — read-only reader for QEMU qcow2 v2 and v3 files
//!   (no compression, no encryption, no backing-file follow yet).
//! - [`VmdkImage`] — read-only reader for VMware `monolithicSparse`
//!   files (single-file sparse). `streamOptimized` and multi-extent
//!   layouts are detected and rejected.
//! - [`VhdxImage`] — read-only reader for Microsoft VHDX files that
//!   have no parent (fixed + dynamic). Differencing disks and dirty
//!   logs are rejected; CRC-32C is verified on headers and region
//!   tables.
//!
//! ## Conventions (per `00_GLOBAL_CONVENTIONS.md`)
//!
//! - Every fallible API returns [`celcommon::CelResult`].
//! - No `unwrap()` / `panic!()` on production paths. Tests use them
//!   freely; production code maps invariants to
//!   [`celcommon::CelError::Invalid`] / `Storage` / `Io`.
//! - There are no `unsafe` blocks in this crate.
//!
//! ## Non-goals (deferred)
//!
//! - Writes against any format.
//! - Compressed clusters / streamOptimized VMDKs.
//! - Backing-file chains and differencing VHDX.
//! - VHDX log replay.
//! - Snapshots / dirty bitmaps.
//! - Async I/O.

#![forbid(unsafe_code)]
#![warn(missing_docs, rust_2018_idioms)]
#![deny(rustdoc::broken_intra_doc_links)]

mod disk;
mod format;
mod raw;
mod qcow2;
mod util;
mod vhdx;
mod vmdk;

pub use disk::{DiskImage, ImageInfo};
pub use format::{detect_format, FormatKind};
pub use qcow2::Qcow2Image;
pub use raw::RawImage;
pub use vhdx::VhdxImage;
pub use vmdk::VmdkImage;

use std::path::Path;

use celcommon::CelResult;

/// Castagnoli CRC-32C of `data`.
///
/// Re-exported for host-side callers (boot-blob fingerprinting, etc.)
/// so they can use the same checksum implementation that this crate
/// uses internally to validate VHDX headers and region tables. Pure
/// software, no `unsafe`, no extra dependencies.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    util::crc32c(data)
}

/// Open a disk image at `path`, auto-detecting its format and
/// returning a boxed [`DiskImage`] handle.
pub fn open(path: impl AsRef<Path>) -> CelResult<Box<dyn DiskImage>> {
    let path = path.as_ref();
    let kind = detect_format(path)?;
    match kind {
        FormatKind::Raw   => Ok(Box::new(RawImage::open(path)?)),
        FormatKind::Qcow2 => Ok(Box::new(Qcow2Image::open(path)?)),
        FormatKind::Vmdk  => Ok(Box::new(VmdkImage::open(path)?)),
        FormatKind::Vhdx  => Ok(Box::new(VhdxImage::open(path)?)),
    }
}

/// Convenience: open an image and immediately return its [`ImageInfo`].
pub fn inspect(path: impl AsRef<Path>) -> CelResult<ImageInfo> {
    Ok(open(path)?.info())
}

/// Streaming CRC-32C over the *entire* virtual disk.
///
/// Walks the image in 64 KiB chunks via [`DiskImage::read_at`],
/// hashing the bytes incrementally. Unallocated/sparse regions are
/// hashed as zeros — which is exactly what the trait surface
/// promises for those offsets — so the result is **content-stable**
/// across backends: the same logical bytes always produce the same
/// digest regardless of whether they came from raw, qcow2, VMDK or
/// VHDX storage.
///
/// W19: used by `celctl image checksum` for operator-side image
/// attestation, and by the controller's drift-detection path on
/// `start_vm` to notice silent backing-image swaps.
///
/// # Errors
///
/// Propagates any `read_at` failure from the underlying backend
/// verbatim. Returns [`celcommon::CelError::Invalid`] if the image
/// reports a `virtual_size == 0`.
pub fn full_image_crc32c(image: &dyn DiskImage) -> CelResult<u32> {
    let total = image.virtual_size();
    if total == 0 {
        return Err(celcommon::CelError::Invalid(
            "full_image_crc32c: virtual size is zero",
        ));
    }
    const CHUNK: usize = 64 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let mut crc: u32 = 0xFFFF_FFFF;
    let mut offset: u64 = 0;
    while offset < total {
        let want = core::cmp::min(CHUNK as u64, total - offset) as usize;
        let n = image.read_at(offset, &mut buf[..want])?;
        if n == 0 {
            return Err(celcommon::CelError::Invalid(
                "full_image_crc32c: short read before virtual_size",
            ));
        }
        crc = util::crc32c_continue(crc, &buf[..n]);
        offset += n as u64;
    }
    Ok(crc ^ 0xFFFF_FFFF)
}
