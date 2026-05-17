//! W23-D — kernel driver registry (skeleton).
//!
//! This module exists to give every future device driver a single
//! home in the kernel crate. W23-D ships only the *trait surface* and
//! a single skeleton driver ([`virtio_blk`]); concrete probe / IRQ /
//! virtqueue plumbing lands in W23-F.
//!
//! # Why land the skeleton now
//!
//! Two reasons:
//!
//! 1. **Auditable boundary.** Every consumer (manager, image loader,
//!    bridge) can already program against [`BlockDevice`] without
//!    knowing whether the backing implementation is virtio-blk on a
//!    PCI bus, NVMe on a real-hardware install, or a host-shimmed
//!    `MemBlock` for tests. Sequencing the abstraction ahead of the
//!    impl avoids a flag-day refactor later.
//! 2. **`HyperError::Unimplemented` is a *typed* "TODO".** Stubbing
//!    every method with `Err(HyperError::Unimplemented("virtio-blk: W23-F"))`
//!    means a caller that wires this in early gets a structured
//!    `Reply::Error` (W23-C) instead of a silent hang.
//!
//! # Out of scope for W23-D
//!
//! * PCI bus enumeration. The kernel has no PCI scanner yet; the
//!   probe stub returns `Err(Unimplemented)`.
//! * MSI-X / legacy IRQ wiring. Today the bridge UART is the kernel's
//!   only interrupt source.
//! * IOMMU configuration. [`crate::iommu`] is empty pending W26.
//! * Bare-metal SATA / NVMe. Those will live alongside virtio_blk in
//!   later phases; the trait surface is intentionally device-agnostic.

#![cfg(not(test))]

pub mod virtio_blk;

use crate::error::HyperResult;

/// Logical sector size every kernel-side block device exposes.
///
/// Fixed at 512 because that's what every realistic boot image
/// (raw, qcow2, virtio-blk, NVMe) defaults to. Devices with a 4 KiB
/// physical sector size MUST present a 512-byte logical view at this
/// layer; the W23-F virtio-blk implementation does the necessary
/// scatter/gather.
pub const SECTOR_BYTES: usize = 512;

/// Common shape of every kernel-side block device.
///
/// Object-safe: callers hand around `&dyn BlockDevice` so the kernel
/// can store one trait object per attached drive without dragging
/// monomorphisation into the bring-up path.
pub trait BlockDevice {
    /// Stable, human-readable name suitable for log lines.
    fn name(&self) -> &'static str;

    /// Total sector count of the underlying medium.
    ///
    /// W23-D stubs return `0`; real drivers populate this from the
    /// virtio-blk config or NVMe identify response.
    fn sector_count(&self) -> u64;

    /// Read `dst.len() / SECTOR_BYTES` sectors starting at LBA `lba`
    /// into `dst`. `dst` MUST be `SECTOR_BYTES`-aligned in length.
    fn read_sectors(&self, lba: u64, dst: &mut [u8]) -> HyperResult<()>;

    /// Write `src.len() / SECTOR_BYTES` sectors starting at LBA `lba`.
    /// `src` MUST be `SECTOR_BYTES`-aligned in length.
    fn write_sectors(&self, lba: u64, src: &[u8]) -> HyperResult<()>;
}
