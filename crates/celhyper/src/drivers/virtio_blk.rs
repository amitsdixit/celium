//! W23-D — virtio-blk driver (skeleton).
//!
//! Implementation is deferred to W23-F. This file pins down:
//!
//! * the MMIO register offsets we will program (Virtio v1.2 §4.2.2),
//! * the PCI vendor/device id we will probe,
//! * a [`VirtioBlk`] struct that implements [`super::BlockDevice`]
//!   today by returning [`HyperError::Unimplemented`] from every
//!   fallible call — typed-TODO, not a silent stub.
//!
//! W23-F will fill in:
//!
//! * legacy / modern PCI capability walking to find the device's
//!   MMIO bar,
//! * virtqueue allocation (one request queue, descriptor table +
//!   available + used ring),
//! * MSI-X table programming and a `request_complete` IRQ handler,
//! * driver-side feature negotiation (RW, FLUSH, no SCSI commands).
//!
//! All of that requires the kernel-side PCI scanner (paused — see
//! W23 roadmap) and an interrupt controller surface, so doing it
//! before those land would either embed brittle workarounds or
//! duplicate code we'd delete next phase.

#![cfg(not(test))]

use super::{BlockDevice, SECTOR_BYTES};
use crate::error::{HyperError, HyperResult};

/// PCI vendor id assigned to virtio devices.
pub const VIRTIO_PCI_VENDOR: u16 = 0x1AF4;

/// PCI device id for the modern virtio-blk device (Virtio 1.0+).
pub const VIRTIO_PCI_DEVICE_BLK_MODERN: u16 = 0x1042;
/// PCI device id for the legacy virtio-blk device.
pub const VIRTIO_PCI_DEVICE_BLK_LEGACY: u16 = 0x1001;

/// Modern virtio MMIO register layout (Virtio v1.2 §4.2.2). Offsets
/// are byte-relative to the device's common-config BAR base.
pub mod mmio {
    /// Device feature select.
    pub const DEVICE_FEATURE_SELECT: usize = 0x00;
    /// Device feature window.
    pub const DEVICE_FEATURE:        usize = 0x04;
    /// Driver feature select.
    pub const DRIVER_FEATURE_SELECT: usize = 0x08;
    /// Driver feature window.
    pub const DRIVER_FEATURE:        usize = 0x0C;
    /// MSI-X vector for config changes.
    pub const MSIX_CONFIG_VECTOR:    usize = 0x10;
    /// Number of queues advertised by the device.
    pub const NUM_QUEUES:            usize = 0x12;
    /// Device status (DRIVER_OK et al.).
    pub const DEVICE_STATUS:         usize = 0x14;
    /// Config-space generation counter.
    pub const CONFIG_GENERATION:     usize = 0x15;
    /// Selects the queue subsequent QUEUE_* writes act on.
    pub const QUEUE_SELECT:          usize = 0x16;
    /// Max ring size for the selected queue.
    pub const QUEUE_SIZE:            usize = 0x18;
    /// MSI-X vector for the selected queue.
    pub const QUEUE_MSIX_VECTOR:     usize = 0x1A;
    /// Enable / disable the selected queue.
    pub const QUEUE_ENABLE:          usize = 0x1C;
    /// Notify-offset for the selected queue.
    pub const QUEUE_NOTIFY_OFF:      usize = 0x1E;
    /// Physical address of the descriptor table.
    pub const QUEUE_DESC:            usize = 0x20;
    /// Physical address of the driver area (available ring).
    pub const QUEUE_DRIVER:          usize = 0x28;
    /// Physical address of the device area (used ring).
    pub const QUEUE_DEVICE:          usize = 0x30;
}

/// Virtio device-status bit `DRIVER_OK`. Asserted by the driver when
/// it's ready to start submitting requests.
pub const DEVICE_STATUS_DRIVER_OK: u8 = 0x04;

/// Virtio-blk request type tags (Virtio v1.2 §5.2.6.2).
pub mod req_type {
    /// Read sectors from device into guest memory.
    pub const IN:    u32 = 0;
    /// Write sectors from guest memory to device.
    pub const OUT:   u32 = 1;
    /// Flush the device write-back cache.
    pub const FLUSH: u32 = 4;
}

/// Maximum in-flight requests the W25 driver will keep on the
/// virtqueue. Pinned here so the request-tracker allocator is bounded
/// at compile time; an actual `submit()` that exceeds this returns
/// [`HyperError::Exhausted`] rather than blocking.
pub const MAX_INFLIGHT: usize = 16;

/// Wire-shape of a virtio-blk request header (Virtio v1.2 §5.2.6.2).
/// Kept as a typed POD so the W25 implementation can fill it in
/// without inventing the layout from scratch.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioBlkReqHeader {
    /// One of [`req_type::IN`] / [`req_type::OUT`] / [`req_type::FLUSH`].
    pub req_type: u32,
    /// Reserved priority field; must be `0` for modern transport.
    pub reserved: u32,
    /// Starting LBA. Sector size is fixed at [`SECTOR_BYTES`].
    pub sector: u64,
}

/// Skeleton driver instance. W25 will replace the simple `sectors`
/// field with real ownership of the MMIO window, queue allocations,
/// MSI-X vectors, and an in-flight request tracker capped at
/// [`MAX_INFLIGHT`].
#[derive(Debug)]
pub struct VirtioBlk {
    /// Total sectors reported by the device on probe. `0` in the
    /// skeleton because we never probe.
    sectors: u64,
    /// `true` once `probe_pci()` succeeds. Always `false` today.
    ready: bool,
}

impl VirtioBlk {
    /// Construct the skeleton driver. Real probing is deferred.
    #[must_use]
    pub const fn skeleton() -> Self {
        Self { sectors: 0, ready: false }
    }

    /// Probe the PCI bus for a virtio-blk device. Deferred to W25.
    pub fn probe_pci() -> HyperResult<Self> {
        Err(HyperError::Unimplemented(
            "virtio_blk: PCI probe not implemented (W25)",
        ))
    }

    /// Build a request header for the W25 submit path. Validates the
    /// shape today so the typed-TODO returns a meaningful error to a
    /// caller that prematurely tries to drive the device.
    pub fn build_header(req_type: u32, sector: u64) -> HyperResult<VirtioBlkReqHeader> {
        match req_type {
            req_type::IN | req_type::OUT | req_type::FLUSH => {}
            _ => return Err(HyperError::Invalid("virtio_blk: bad req_type")),
        }
        Ok(VirtioBlkReqHeader { req_type, reserved: 0, sector })
    }
}

impl BlockDevice for VirtioBlk {
    fn name(&self) -> &'static str { "virtio-blk" }

    fn sector_count(&self) -> u64 { self.sectors }

    fn read_sectors(&self, _lba: u64, dst: &mut [u8]) -> HyperResult<()> {
        if dst.is_empty() {
            return Err(HyperError::Invalid("virtio_blk: empty read buffer"));
        }
        if dst.len() % SECTOR_BYTES != 0 {
            return Err(HyperError::Invalid("virtio_blk: read len % 512 != 0"));
        }
        if !self.ready {
            return Err(HyperError::Denied("virtio_blk: device not ready"));
        }
        Err(HyperError::Unimplemented(
            "virtio_blk: read_sectors not implemented (W25)",
        ))
    }

    fn write_sectors(&self, _lba: u64, src: &[u8]) -> HyperResult<()> {
        if src.is_empty() {
            return Err(HyperError::Invalid("virtio_blk: empty write buffer"));
        }
        if src.len() % SECTOR_BYTES != 0 {
            return Err(HyperError::Invalid("virtio_blk: write len % 512 != 0"));
        }
        if !self.ready {
            return Err(HyperError::Denied("virtio_blk: device not ready"));
        }
        Err(HyperError::Unimplemented(
            "virtio_blk: write_sectors not implemented (W25)",
        ))
    }

    fn flush(&self) -> HyperResult<()> {
        if !self.ready {
            return Err(HyperError::Denied("virtio_blk: device not ready"));
        }
        Err(HyperError::Unimplemented(
            "virtio_blk: flush not implemented (W25)",
        ))
    }

    fn is_ready(&self) -> bool { self.ready }
}
