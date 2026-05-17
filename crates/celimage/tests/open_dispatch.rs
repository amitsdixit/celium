//! End-to-end integration tests against the public `celimage::open`
//! API. Exercises format detection + dispatch for every supported
//! backend (raw / qcow2 / vmdk / vhdx).
//!
//! The synthetic-image builders for the non-trivial formats live in
//! unit tests inside their respective backend modules; these tests
//! cover the user-visible `open(path) -> Box<dyn DiskImage>` surface
//! and verify the dispatch picks the right backend per magic.

use std::io::Write;

use celimage::{detect_format, inspect, open, FormatKind};

#[test]
fn open_raw_via_public_api() {
    let mut t = tempfile::NamedTempFile::new().unwrap();
    t.write_all(&[7u8; 512]).unwrap();
    t.flush().unwrap();
    let img = open(t.path()).unwrap();
    let info = img.info();
    assert_eq!(info.format, FormatKind::Raw);
    assert_eq!(info.virtual_size, 512);
    let mut buf = [0u8; 16];
    let n = img.read_at(0, &mut buf).unwrap();
    assert_eq!(n, 16);
    assert!(buf.iter().all(|&b| b == 7));
}

#[test]
fn detect_format_recognises_each_magic() {
    // raw
    let mut t = tempfile::NamedTempFile::new().unwrap();
    t.write_all(&[0u8; 64]).unwrap();
    t.flush().unwrap();
    assert_eq!(detect_format(t.path()).unwrap(), FormatKind::Raw);

    // qcow2
    let mut t = tempfile::NamedTempFile::new().unwrap();
    t.write_all(b"QFI\xfb").unwrap();
    t.write_all(&[0u8; 64]).unwrap();
    t.flush().unwrap();
    assert_eq!(detect_format(t.path()).unwrap(), FormatKind::Qcow2);

    // vmdk
    let mut t = tempfile::NamedTempFile::new().unwrap();
    t.write_all(b"KDMV").unwrap();
    t.write_all(&[0u8; 64]).unwrap();
    t.flush().unwrap();
    assert_eq!(detect_format(t.path()).unwrap(), FormatKind::Vmdk);

    // vhdx
    let mut t = tempfile::NamedTempFile::new().unwrap();
    t.write_all(b"vhdxfile").unwrap();
    t.write_all(&[0u8; 64]).unwrap();
    t.flush().unwrap();
    assert_eq!(detect_format(t.path()).unwrap(), FormatKind::Vhdx);
}

#[test]
fn inspect_rejects_malformed_vmdk_with_invalid() {
    // KDMV magic but no further valid header → backend refuses with
    // `CelError::Invalid`. Confirms the dispatcher actually reaches
    // the VMDK backend instead of stubbing it out.
    let mut t = tempfile::NamedTempFile::new().unwrap();
    t.write_all(b"KDMV").unwrap();
    t.write_all(&[0u8; 512]).unwrap();
    t.flush().unwrap();
    let Err(err) = inspect(t.path()) else { panic!("malformed vmdk must be rejected") };
    assert_eq!(err.code(), "invalid");
}

#[test]
fn inspect_rejects_malformed_vhdx_with_invalid() {
    // "vhdxfile" magic but no headers → backend refuses.
    let mut t = tempfile::NamedTempFile::new().unwrap();
    t.write_all(b"vhdxfile").unwrap();
    t.write_all(&[0u8; 512]).unwrap();
    t.flush().unwrap();
    let Err(err) = inspect(t.path()) else { panic!("malformed vhdx must be rejected") };
    assert_eq!(err.code(), "invalid");
}

// --- W19: full_image_crc32c ----------------------------------------------

#[test]
fn full_image_crc32c_matches_oneshot_over_raw() {
    // For raw images, the streaming digest must equal the simple
    // one-shot CRC over the same bytes. Validates the chunked walker.
    let bytes: Vec<u8> = (0..200_000u32).map(|i| (i & 0xff) as u8).collect();
    let mut t = tempfile::NamedTempFile::new().unwrap();
    t.write_all(&bytes).unwrap();
    t.flush().unwrap();
    let img = open(t.path()).unwrap();
    let streamed = celimage::full_image_crc32c(img.as_ref()).unwrap();
    assert_eq!(streamed, celimage::crc32c(&bytes));
}
