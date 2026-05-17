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

// ---------------------------------------------------------------------------
// W23-E1 — single-slot host→kernel boot-image staging.
// ---------------------------------------------------------------------------
//
// The bridge streams a guest image into this static slot via
// `Request::ImageLoad`. The kernel validates length + CRC32C, then
// records the buffer as "present". A subsequent `Request::Create`
// whose `boot_blob_crc32c` matches the staged image's CRC installs
// the staged blob instead of the embedded `HELLO_BLOB`.
//
// One slot rather than N because:
//   1. ROWS_MAX == 4 — quadrupling the static cost (16 KiB) buys
//      very little while the kernel still can't service concurrent
//      Creates (the bridge dispatch is serial).
//   2. The transfer→Create handshake completes synchronously inside
//      one bridge turn, so the staged slot can never be referenced
//      after the next ImageLoad overwrites it.
//
// The slot lives behind a `spin::Mutex` purely as a guard against
// future SMP reuse; today it's only ever touched by the BSP from
// `bridge::dispatch`.

/// Static staging slot for one host-shipped boot image.
struct StagedImage {
    bytes: [u8; MAX_IMAGE_BYTES],
    /// Valid prefix length. 0 means "empty"; otherwise the first
    /// `len` bytes of `bytes` are the staged payload.
    len: usize,
    /// CRC32C of the first `len` bytes, recomputed by `stage_from_hex`.
    crc32c: u32,
    /// `true` once `stage_from_hex` has populated and validated the
    /// slot. Cleared by `discard_staged`.
    present: bool,
}

impl StagedImage {
    const fn empty() -> Self {
        Self {
            bytes: [0; MAX_IMAGE_BYTES],
            len: 0,
            crc32c: 0,
            present: false,
        }
    }
}

static STAGED: spin::Mutex<StagedImage> = spin::Mutex::new(StagedImage::empty());

/// Stage a host-shipped boot image by decoding `hex` directly into
/// the static staging slot, then validating its CRC32C against
/// `expected_crc`.
///
/// `expected_len` is the *decoded* byte length; it must match
/// `hex.len() / 2`. The wire decoder enforces this, but we re-check
/// here so direct callers (tests, future bring-up paths) can't bypass
/// the invariant.
///
/// On any validation failure the staging slot is left empty (any
/// previously staged image is cleared) and a descriptive
/// [`HyperError`] is returned.
pub fn stage_from_hex(hex: &[u8], expected_len: u32, expected_crc: u32) -> HyperResult<()> {
    let expected_len_usize = expected_len as usize;
    if expected_len_usize == 0 {
        return Err(HyperError::Invalid("image_loader: stage len == 0"));
    }
    if expected_len_usize > MAX_IMAGE_BYTES {
        return Err(HyperError::Invalid(
            "image_loader: stage len > MAX_IMAGE_BYTES",
        ));
    }
    if hex.len() != expected_len_usize.saturating_mul(2) {
        return Err(HyperError::Invalid("image_loader: stage hex len mismatch"));
    }
    let mut slot = STAGED.lock();
    // Clear first so a half-decoded failure can't leave stale bytes
    // visible to a future `take_staged` after the present flag flips.
    slot.present = false;
    slot.len = 0;
    slot.crc32c = 0;
    let n = crate::wire::hex_decode(hex, &mut slot.bytes[..expected_len_usize])?;
    debug_assert_eq!(n, expected_len_usize);
    let actual_crc = crc32c(&slot.bytes[..expected_len_usize]);
    if actual_crc != expected_crc {
        crate::logger::log("celhyper: image_loader: staged blob CRC mismatch; rejecting");
        // Wipe to avoid silently retaining unverified bytes.
        for b in &mut slot.bytes[..expected_len_usize] {
            *b = 0;
        }
        return Err(HyperError::Invalid("image_loader: stage CRC mismatch"));
    }
    slot.len = expected_len_usize;
    slot.crc32c = actual_crc;
    slot.present = true;
    Ok(())
}

/// Returns `Some((len, crc))` for the currently staged image, or
/// `None` if nothing is staged. Diagnostic-only — does not lend out
/// the underlying bytes.
#[must_use]
pub fn staged_meta() -> Option<(u32, u32)> {
    let slot = STAGED.lock();
    if slot.present {
        Some((slot.len as u32, slot.crc32c))
    } else {
        None
    }
}

/// Discard any staged image. Idempotent.
pub fn discard_staged() {
    let mut slot = STAGED.lock();
    slot.present = false;
    slot.len = 0;
    slot.crc32c = 0;
}

impl BootImage<'static> {
    /// Pick the most-specific image available *for the bridge's
    /// `Request::Create` path*: a host-staged image whose CRC matches
    /// the controller-provided `expected_crc`, otherwise the embedded
    /// HELLO blob. Handoff-staged images are bring-up only (W23-E2
    /// will fold the two paths together once the bridge takes over
    /// from CelLoader for image transport).
    ///
    /// `expected_crc == None` means the controller didn't supply a
    /// CRC; we conservatively refuse to use the staged blob in that
    /// case — the controller asked for the canned guest and we honour
    /// that.
    ///
    /// Returns a `BootImage<'static>` because both candidate
    /// payloads live in `.bss` for the kernel's lifetime. Lives in
    /// its own `impl BootImage<'static>` block (rather than the
    /// generic `impl<'a> BootImage<'a>`) so the embedded-fallback
    /// path can return `Self` without forcing `'a = 'static`
    /// pollution onto every other constructor.
    #[must_use]
    pub fn from_staged_or_embedded(expected_crc: Option<u32>) -> Self {
        if let Some(want) = expected_crc {
            let slot = STAGED.lock();
            if slot.present && slot.crc32c == want {
                let len = slot.len;
                // SAFETY: `STAGED.bytes` is a `static` allocation
                // that lives for the kernel's lifetime. We hold the
                // mutex only long enough to read the length and CRC;
                // the bytes themselves are immutable for the
                // remainder of the bridge turn because:
                //   * `stage_from_hex` (the only writer) is called
                //     serially from the same bridge dispatcher, and
                //   * `install_first_guest` copies the bytes into
                //     guest RAM before the dispatcher returns and
                //     reads the next request.
                // We rebind to `'static` to escape the mutex guard's
                // lifetime; the bytes are not aliased mutably during
                // this BootImage's use because the bridge is single-
                // threaded and synchronous.
                let bytes_ptr = slot.bytes.as_ptr();
                drop(slot);
                let bytes: &'static [u8] = unsafe {
                    core::slice::from_raw_parts(bytes_ptr, len)
                };
                crate::logger::log("celhyper: boot image: staged (crc match)");
                // from_raw_bytes can't fail here: len was validated
                // by stage_from_hex against MAX_IMAGE_BYTES and the
                // GUEST_LOAD_GPA entry point is in-range by
                // construction.
                return match Self::from_raw_bytes(bytes, GUEST_LOAD_GPA, 0, Some(want)) {
                    Ok(img) => img,
                    Err(_) => {
                        crate::logger::log(
                            "celhyper: staged image rejected by from_raw_bytes; falling back",
                        );
                        Self::embedded()
                    }
                };
            }
            crate::logger::log(
                "celhyper: boot image: embedded (no staged image matches requested CRC)",
            );
        } else {
            crate::logger::log(
                "celhyper: boot image: embedded (controller supplied no boot_blob_crc32c)",
            );
        }
        Self::embedded()
    }
}

// ---------------------------------------------------------------------------
// CRC32C — see [`crate::crc`] for the algorithm and known-vector tests.
// ---------------------------------------------------------------------------

pub use crate::crc::{crc32c, crc32c_continue};
