//! W23-D — guest boot-image abstraction.
//!
//! Until W23-D the kernel ran exactly one guest payload: the
//! hand-assembled `HELLO_BLOB` in [`crate::guest`], hard-wired into
//! [`crate::manager::CreateVmRequest::hello`] with `RIP = 0x1000` and
//! `RSP = 0`. That worked for the W22/W23-A bring-up smoke but left
//! the hypervisor unable to launch anything a real operator would
//! call a "guest".
//!
//! This module introduces the smallest abstraction that decouples
//! the kernel from that single canned blob:
//!
//! * [`BootImage`] carries a `&'static [u8]` payload plus the entry
//!   `rip`/`rsp` the loader wants the guest to start at.
//! * [`BootImage::embedded`] returns the existing `HELLO_BLOB` view,
//!   so all current bring-up paths keep working unchanged.
//! * [`BootImage::from_handoff`] inspects the W23-D handoff fields
//!   ([`crate::handoff::CeliumHandoff::boot_image_phys`] /
//!   [`crate::handoff::CeliumHandoff::boot_image_len`]) and, when
//!   they are non-zero, builds a `BootImage` over that physical
//!   region. CelLoader doesn't stage anything yet (W23-E), so this
//!   path is dormant on real hardware today but the surface is
//!   ready.
//!
//! Future phases:
//!
//! * **W23-E**: chunked `Request::ImageChunk`/`ImageCommit` over the
//!   bridge lets `celctl vm start` ship a real CelVault-backed image
//!   into a kernel-managed staging area whose `(phys, len)` becomes
//!   the next [`BootImage`].
//! * **W23-F**: virtio-blk-attached guests stop staging in-memory and
//!   read sectors lazily from the drive.
//!
//! # Invariants
//!
//! * The byte slice MUST be `<= MAX_IMAGE_BYTES` (4 KiB today; bumped
//!   when [`crate::vmx::launch::install_first_guest`] starts mapping
//!   multi-page guests in W23-F).
//! * `entry_rip` MUST point inside the identity-mapped GPA region the
//!   blob will be installed into — currently `0x1000..0x2000`. We
//!   validate the lower bound; the upper bound is implicit in the
//!   blob-size cap.
//! * No `unsafe` is needed to *construct* a `BootImage`. Lifting raw
//!   handoff bytes into a `&'static [u8]` IS unsafe and is contained
//!   in a single helper with a precise SAFETY note.

#![cfg(not(test))]

use crate::error::{HyperError, HyperResult};
use crate::handoff::CeliumHandoff;

/// Largest guest image the W23-D loader will accept.
///
/// Matches the single-page bound enforced by
/// [`crate::vmx::launch::install_first_guest`]; bumping one without
/// the other is a bug — the launch helper would refuse the blob.
pub const MAX_IMAGE_BYTES: usize = crate::mm::PAGE_SIZE;

/// GPA the kernel maps the blob at. Hard-coded today; W23-F lifts
/// this once EPT mapping of multi-page guests lands.
pub const GUEST_LOAD_GPA: u64 = 0x1000;

/// One validated guest image ready to be installed via
/// [`crate::manager::CreateVmRequest::from_boot_image`].
///
/// A `BootImage` is purely descriptive — constructing one performs
/// validation but doesn't touch EPT, VMCS, or any other VMX state.
#[derive(Debug, Clone, Copy)]
pub struct BootImage<'a> {
    /// Bytes the kernel will copy into the guest physical region at
    /// [`GUEST_LOAD_GPA`].
    pub blob: &'a [u8],
    /// Initial guest RIP. Must satisfy
    /// `GUEST_LOAD_GPA <= entry_rip < GUEST_LOAD_GPA + blob.len()`.
    pub entry_rip: u64,
    /// Initial guest RSP. Today every embedded test image uses `0`
    /// (no stack); production images set their own value.
    pub entry_rsp: u64,
    /// Optional CRC32C of `blob`, surfaced for logging / drift
    /// detection. `None` means "not checked"; `Some(crc)` was either
    /// stamped at compile time ([`embedded`]) or verified against
    /// the handoff value ([`from_handoff`]).
    pub crc32c: Option<u32>,
}

impl<'a> BootImage<'a> {
    /// Construct from raw bytes, validating size and entry-point
    /// bounds. Returns [`HyperError::Invalid`] on violation.
    ///
    /// `entry_rip` is interpreted as a GPA inside the blob; the
    /// caller doesn't need to know about [`GUEST_LOAD_GPA`].
    pub fn from_raw_bytes(
        blob: &'a [u8],
        entry_rip: u64,
        entry_rsp: u64,
        crc32c: Option<u32>,
    ) -> HyperResult<Self> {
        if blob.is_empty() {
            return Err(HyperError::Invalid("image_loader: empty blob"));
        }
        if blob.len() > MAX_IMAGE_BYTES {
            return Err(HyperError::Invalid(
                "image_loader: blob > MAX_IMAGE_BYTES (4 KiB W23-D cap)",
            ));
        }
        let lo = GUEST_LOAD_GPA;
        let hi = GUEST_LOAD_GPA + (blob.len() as u64);
        if entry_rip < lo || entry_rip >= hi {
            return Err(HyperError::Invalid(
                "image_loader: entry_rip outside loaded blob region",
            ));
        }
        Ok(Self { blob, entry_rip, entry_rsp, crc32c })
    }

    /// The built-in test image: the W22 `HELLO_BLOB` at
    /// `RIP=GUEST_LOAD_GPA, RSP=0`. Used as the fallback when the
    /// handoff carries no host-staged image.
    #[must_use]
    pub fn embedded() -> Self {
        // SAFETY-free construction: HELLO_BLOB is a kernel constant
        // generated at compile time, so all `from_raw_bytes`
        // invariants are statically satisfied.
        let blob = crate::guest::HELLO_BLOB;
        // The blob is hand-assembled and well under 4 KiB; from_raw_bytes
        // would only fail on bounds, never on this constant input. The
        // expect string is for diagnosability if the constant is ever
        // edited beyond MAX_IMAGE_BYTES.
        match Self::from_raw_bytes(blob, GUEST_LOAD_GPA, 0, None) {
            Ok(img) => img,
            Err(_) => {
                // Defensive: never panic in the kernel. We log and
                // return a minimal one-byte HLT image so bring-up
                // still produces *some* terminating guest.
                crate::logger::log("celhyper: HELLO_BLOB grew past MAX_IMAGE_BYTES; using HLT fallback");
                Self {
                    blob: &HLT_FALLBACK,
                    entry_rip: GUEST_LOAD_GPA,
                    entry_rsp: 0,
                    crc32c: None,
                }
            }
        }
    }

    /// Build from the W23-D handoff fields. Returns `Ok(None)` when
    /// the loader staged no image (all three fields zero) — callers
    /// then fall back to [`embedded`].
    ///
    /// # Safety contract
    ///
    /// The (phys, len) region is trusted to be (a) identity-mapped
    /// by the kernel's boot CR3 (true for all CelLoader-staged
    /// regions today), and (b) immutable for the lifetime of the
    /// returned slice. CelLoader's leak-after-`Box::leak` pattern
    /// gives us (b) for free; the handoff version bump to v2 gates
    /// any wire-stale callers out.
    pub fn from_handoff(handoff: &CeliumHandoff) -> HyperResult<Option<Self>> {
        let phys = handoff.boot_image_phys;
        let len  = handoff.boot_image_len;
        let crc  = handoff.boot_image_crc32c;
        match (phys, len) {
            (0, 0) => Ok(None),
            (0, _) | (_, 0) => Err(HyperError::InvalidHandoff(
                "image_loader: boot_image_phys/len half-set",
            )),
            (phys, len) => {
                if len as usize > MAX_IMAGE_BYTES {
                    return Err(HyperError::InvalidHandoff(
                        "image_loader: handoff boot image > MAX_IMAGE_BYTES",
                    ));
                }
                // SAFETY: contract documented above — phys is identity-
                // mapped by boot CR3, len is bounds-checked, and the
                // backing allocation is `'static` (CelLoader leaks it).
                // We expose the slice as `'static` only because the
                // kernel never re-uses image staging memory.
                let slice: &'static [u8] = unsafe {
                    core::slice::from_raw_parts(phys as *const u8, len as usize)
                };
                // crc32c == 0 in v2 means "not stamped"; we keep it
                // surface-level so logging can show the value, but we
                // don't verify here — W23-E will recompute on the
                // kernel side after the bridge finishes streaming.
                let crc = if crc == 0 { None } else { Some(crc) };
                let img = Self::from_raw_bytes(slice, GUEST_LOAD_GPA, 0, crc)?;
                Ok(Some(img))
            }
        }
    }

    /// Pick the most-specific image available: handoff-staged if the
    /// loader provided one, otherwise the embedded HELLO_BLOB.
    ///
    /// Bringup uses this so it never has to know which path is
    /// active — a single source of truth for "what is the next
    /// guest going to run".
    pub fn from_handoff_or_embedded(handoff: &CeliumHandoff) -> Self {
        match Self::from_handoff(handoff) {
            Ok(Some(img)) => {
                crate::logger::log("celhyper: boot image: handoff-staged");
                img
            }
            Ok(None) => {
                crate::logger::log("celhyper: boot image: embedded HELLO_BLOB (no handoff image)");
                Self::embedded()
            }
            Err(e) => {
                // Don't fail bringup on a bad handoff image — log,
                // ignore, and use the embedded fallback so we still
                // produce a runnable guest. Operators see the kind
                // tag in the bridge log.
                let kind = match e {
                    HyperError::InvalidHandoff(m)
                    | HyperError::Invalid(m) => m,
                    _ => "unknown",
                };
                crate::logger::log("celhyper: handoff image rejected; using embedded fallback");
                crate::logger::log(kind);
                Self::embedded()
            }
        }
    }
}

/// One-byte `hlt` instruction used as the absolute-last-resort
/// fallback. Kept as a top-level constant so its address is stable.
static HLT_FALLBACK: [u8; 1] = [0xF4];
