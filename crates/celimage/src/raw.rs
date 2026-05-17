//! Raw disk-image reader.
//!
//! Treats the whole file as the guest's disk byte-for-byte. The
//! logical size is exactly the file length; any read past EOF returns
//! zero bytes (matching the [`DiskImage::read_at`] contract).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use celcommon::{CelError, CelResult};

use crate::disk::{DiskImage, ImageInfo};
use crate::format::FormatKind;

/// Raw / `.img` reader.
pub struct RawImage {
    path: PathBuf,
    size: u64,
    // `File::read_at` is platform-specific (`std::os::unix::fs::FileExt`
    // / `std::os::windows::fs::FileExt`). To stay portable we keep a
    // single `File` behind a `Mutex` and `seek` per read. This is
    // sufficient for Phase 1; we'll revisit for the hot path in
    // Phase 3 alongside `pread`-style I/O on the host.
    file: Mutex<File>,
}

impl RawImage {
    /// Open `path` as a raw image.
    pub fn open(path: impl AsRef<Path>) -> CelResult<Self> {
        let path = path.as_ref();
        let file = File::open(path)
            .map_err(|e| CelError::Io(format!("open {}: {e}", path.display())))?;
        let meta = file.metadata()
            .map_err(|e| CelError::Io(format!("stat {}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            size: meta.len(),
            file: Mutex::new(file),
        })
    }

    /// Path the image was opened from. For diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path { &self.path }
}

impl DiskImage for RawImage {
    fn info(&self) -> ImageInfo {
        ImageInfo {
            format: FormatKind::Raw,
            virtual_size: self.size,
            cluster_size: None,
            backend: "raw",
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> CelResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if offset >= self.size {
            return Ok(0);
        }
        let remaining = self.size - offset;
        let take = (buf.len() as u64).min(remaining) as usize;

        let mut f = self.file.lock()
            .map_err(|_| CelError::Internal("raw: file mutex poisoned"))?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| CelError::Io(format!("seek {}: {e}", self.path.display())))?;
        let mut total = 0;
        while total < take {
            let n = f.read(&mut buf[total..take])
                .map_err(|e| CelError::Io(format!("read {}: {e}", self.path.display())))?;
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

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut t = tempfile::NamedTempFile::new().unwrap();
        t.write_all(bytes).unwrap();
        t.flush().unwrap();
        t
    }

    #[test]
    fn opens_and_reports_size() {
        let t = write_tmp(&[0xAB; 4096]);
        let r = RawImage::open(t.path()).unwrap();
        let info = r.info();
        assert_eq!(info.format, FormatKind::Raw);
        assert_eq!(info.virtual_size, 4096);
        assert!(info.cluster_size.is_none());
    }

    #[test]
    fn read_at_returns_correct_bytes() {
        let mut bytes = vec![0u8; 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        let t = write_tmp(&bytes);
        let r = RawImage::open(t.path()).unwrap();

        let mut buf = [0u8; 16];
        let n = r.read_at(100, &mut buf).unwrap();
        assert_eq!(n, 16);
        assert_eq!(buf[0], 100);
        assert_eq!(buf[15], 115);
    }

    #[test]
    fn read_at_clips_at_eof() {
        let t = write_tmp(&[1u8; 10]);
        let r = RawImage::open(t.path()).unwrap();
        let mut buf = [0u8; 16];
        let n = r.read_at(5, &mut buf).unwrap();
        assert_eq!(n, 5);
        // Past EOF -> zero bytes, no error.
        let n2 = r.read_at(100, &mut buf).unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn read_exact_at_errors_on_short() {
        let t = write_tmp(&[0u8; 8]);
        let r = RawImage::open(t.path()).unwrap();
        let mut buf = [0u8; 16];
        let err = r.read_exact_at(0, &mut buf).unwrap_err();
        assert_eq!(err.code(), "invalid");
    }
}
