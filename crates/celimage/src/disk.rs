//! [`DiskImage`] trait and metadata.

use celcommon::CelResult;

use crate::FormatKind;

/// Summary returned by [`DiskImage::info`] / [`crate::inspect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    /// On-disk format.
    pub format: FormatKind,
    /// Logical (guest-visible) size in bytes.
    pub virtual_size: u64,
    /// Cluster size in bytes if meaningful (qcow2); `None` for raw.
    pub cluster_size: Option<u64>,
    /// Free-form, human-readable backend description; for diagnostics.
    pub backend: &'static str,
}

/// Read-only view of a guest disk.
///
/// Implementations MUST be `Send + Sync` so the caller can hand them
/// to async runtimes or place them behind an `Arc<dyn DiskImage>`.
/// All read offsets are guest-visible (logical) bytes; allocation
/// holes return zeros without raising an error.
pub trait DiskImage: Send + Sync {
    /// Static metadata for the image.
    fn info(&self) -> ImageInfo;

    /// Logical disk size in bytes (same as `info().virtual_size`).
    fn virtual_size(&self) -> u64 { self.info().virtual_size }

    /// Read up to `buf.len()` bytes starting at logical `offset`.
    ///
    /// Reads past `virtual_size()` return `0` bytes (no error).
    /// Partial reads are allowed; callers should loop until they
    /// reach EOF.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> CelResult<usize>;

    /// Convenience: fill `buf` completely or return
    /// [`celcommon::CelError::Invalid`] on short read (typically EOF
    /// before end-of-buffer).
    fn read_exact_at(&self, mut offset: u64, mut buf: &mut [u8]) -> CelResult<()> {
        while !buf.is_empty() {
            let n = self.read_at(offset, buf)?;
            if n == 0 {
                return Err(celcommon::CelError::Invalid("disk: short read"));
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }
}
