//! Disk-image format detection.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use celcommon::{CelError, CelResult};

/// Recognised on-disk formats. The variants are ordered so older /
/// simpler formats sort first; do not rely on the discriminant value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormatKind {
    /// Raw / `.img` — no header.
    Raw,
    /// QEMU qcow2 v2 or v3 (magic `QFI\xfb`).
    Qcow2,
    /// VMware VMDK (sparse/extent; magic `KDMV` for sparse v1+).
    Vmdk,
    /// Microsoft VHDX (magic `vhdxfile`).
    Vhdx,
}

impl FormatKind {
    /// Stable lowercase identifier suitable for CLI output and metrics
    /// labels.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Raw   => "raw",
            Self::Qcow2 => "qcow2",
            Self::Vmdk  => "vmdk",
            Self::Vhdx  => "vhdx",
        }
    }
}

/// Sniff `path` and return its [`FormatKind`].
///
/// Detection rule:
///
/// 1. If the first 4 bytes are `QFI\xfb` → [`FormatKind::Qcow2`].
/// 2. If the first 4 bytes are `KDMV`    → [`FormatKind::Vmdk`].
/// 3. If the first 8 bytes are `vhdxfile`→ [`FormatKind::Vhdx`].
/// 4. Otherwise → [`FormatKind::Raw`].
///
/// The file is left untouched. Empty files are rejected with
/// [`CelError::Invalid`].
pub fn detect_format(path: impl AsRef<Path>) -> CelResult<FormatKind> {
    let path = path.as_ref();
    let mut f = File::open(path)
        .map_err(|e| CelError::Io(format!("open {}: {e}", path.display())))?;
    let mut header = [0u8; 16];
    let n = f.read(&mut header)
        .map_err(|e| CelError::Io(format!("read {}: {e}", path.display())))?;
    if n == 0 {
        return Err(CelError::Invalid("image is empty"));
    }
    detect_from_bytes(&header[..n])
}

/// Header-bytes variant of [`detect_format`]; exposed for tests.
pub fn detect_from_bytes(header: &[u8]) -> CelResult<FormatKind> {
    if header.is_empty() {
        return Err(CelError::Invalid("image is empty"));
    }
    if header.len() >= 4 && &header[0..4] == b"QFI\xfb" {
        return Ok(FormatKind::Qcow2);
    }
    if header.len() >= 4 && &header[0..4] == b"KDMV" {
        return Ok(FormatKind::Vmdk);
    }
    if header.len() >= 8 && &header[0..8] == b"vhdxfile" {
        return Ok(FormatKind::Vhdx);
    }
    Ok(FormatKind::Raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_qcow2_magic() {
        assert_eq!(detect_from_bytes(b"QFI\xfb\x00\x00\x00\x03").unwrap(),
                   FormatKind::Qcow2);
    }

    #[test]
    fn detects_vmdk_magic() {
        assert_eq!(detect_from_bytes(b"KDMV\x01\x00\x00\x00").unwrap(),
                   FormatKind::Vmdk);
    }

    #[test]
    fn detects_vhdx_magic() {
        assert_eq!(detect_from_bytes(b"vhdxfile\x00\x00\x00\x00").unwrap(),
                   FormatKind::Vhdx);
    }

    #[test]
    fn defaults_to_raw() {
        assert_eq!(detect_from_bytes(b"random bytes here").unwrap(),
                   FormatKind::Raw);
    }

    #[test]
    fn rejects_empty() {
        let err = detect_from_bytes(&[]).unwrap_err();
        assert_eq!(err.code(), "invalid");
    }

    #[test]
    fn tags_are_stable() {
        assert_eq!(FormatKind::Raw.tag(),   "raw");
        assert_eq!(FormatKind::Qcow2.tag(), "qcow2");
        assert_eq!(FormatKind::Vmdk.tag(),  "vmdk");
        assert_eq!(FormatKind::Vhdx.tag(),  "vhdx");
    }
}
