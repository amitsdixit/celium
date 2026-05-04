//! 4 KiB VMX regions: VMXON region and per-vCPU VMCS.
//!
//! Both share a layout: a 32-bit revision identifier read from
//! `IA32_VMX_BASIC[30:0]` at offset 0, followed by implementation-defined
//! bytes the CPU manages. We hand out *page-aligned* allocations; alignment
//! is enforced by `#[repr(align(4096))]`.

use crate::error::{HyperError, HyperResult};
use crate::mm::{FrameProvider, PhysAddr, PAGE_SIZE};

/// A 4 KiB VMX page. Same shape for VMXON and VMCS.
#[repr(C, align(4096))]
pub struct VmxPage {
    /// Revision id at offset 0; the rest is opaque to software.
    pub revision_id: u32,
    /// Pad to a full 4 KiB. CPU may write here.
    pub _opaque:     [u8; PAGE_SIZE - 4],
}

impl VmxPage {
    /// Construct a page with a known revision id. Useful in unit tests; the
    /// kernel allocator path uses [`alloc_in_pool`] instead.
    #[must_use]
    pub const fn new(revision_id: u32) -> Self {
        Self {
            revision_id,
            _opaque: [0; PAGE_SIZE - 4],
        }
    }
}

/// Allocate a fresh page through `p` and stamp its revision id. Returns the
/// page's physical address — that is what `vmxon`/`vmptrld` consume.
pub fn alloc_in_pool<P: FrameProvider>(p: &mut P, revision_id: u32) -> HyperResult<PhysAddr> {
    let pa = p.alloc_zeroed()?;
    // The first 4 bytes of the freshly-zeroed frame become the revision id.
    // Each entry slot in `FrameProvider` is a u64; we only consume the low
    // 32 bits and leave the high half zero.
    p.write_entry(pa, 0, u64::from(revision_id));
    Ok(pa)
}

/// Sanity check: the layout we hand to the CPU must be exactly 4 KiB and
/// 4 KiB-aligned. Failure here is a build bug, not a runtime one.
pub const fn assert_layout() -> HyperResult<()> {
    if core::mem::size_of::<VmxPage>() != PAGE_SIZE {
        return Err(HyperError::Internal("VmxPage size != 4 KiB"));
    }
    if core::mem::align_of::<VmxPage>() != PAGE_SIZE {
        return Err(HyperError::Internal("VmxPage align != 4 KiB"));
    }
    Ok(())
}
