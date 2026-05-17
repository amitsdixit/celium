//! W25-E \u2014 NVMe driver (skeleton).
//!
//! NVMe is the bare-metal counterpart to virtio-blk: real servers
//! ship NVMe drives, real production validation will run against
//! one. The W25 skeleton pins:
//!
//! * the PCI class/subclass/prog-IF identity (0x01 / 0x08 / 0x02),
//! * the standard admin submission/completion queue depths,
//! * the [`BlockDevice`] surface so the kernel can swap NVMe for
//!   virtio-blk without rewriting the bridge / image loader.
//!
//! Deep plumbing (MMIO bar walk, admin-queue construction, identify-
//! namespace, I/O queue creation, MSI-X) is W26+ \u2014 same discipline
//! as the virtio drivers.

#![cfg(not(test))]

use super::{BlockDevice, SECTOR_BYTES};
use crate::error::{HyperError, HyperResult};

/// PCI class code for mass-storage controllers.
pub const PCI_CLASS_STORAGE: u8 = 0x01;
/// PCI subclass for non-volatile memory controllers (NVMe + AHCI).
pub const PCI_SUBCLASS_NVM: u8 = 0x08;
/// PCI programming-interface byte that identifies NVMe specifically.
pub const PCI_PROGIF_NVME: u8 = 0x02;

/// Admin submission-queue depth NVMe controllers MUST accept (NVMe
/// spec \u00a73.1.13).
pub const ADMIN_QUEUE_ENTRIES: u16 = 64;

/// NVMe controller register layout offsets we will program in W26.
/// Offsets are byte-relative to the BAR0 MMIO window.
pub mod regs {
    /// Controller capabilities.
    pub const CAP:    usize = 0x00;
    /// Controller version.
    pub const VS:     usize = 0x08;
    /// Interrupt mask set.
    pub const INTMS:  usize = 0x0C;
    /// Interrupt mask clear.
    pub const INTMC:  usize = 0x10;
    /// Controller configuration.
    pub const CC:     usize = 0x14;
    /// Controller status.
    pub const CSTS:   usize = 0x1C;
    /// Admin queue attributes.
    pub const AQA:    usize = 0x24;
    /// Admin submission queue base address.
    pub const ASQ:    usize = 0x28;
    /// Admin completion queue base address.
    pub const ACQ:    usize = 0x30;
}

/// Skeleton NVMe controller. The W26 driver will replace the
/// `_phantom`-style fields with real ownership of the MMIO bar,
/// admin queues, and namespace identification responses.
#[derive(Debug, Default)]
pub struct Nvme {
    /// Logical sectors reported by `identify namespace`. `0` until
    /// probe completes.
    sectors: u64,
    /// `true` once `probe_pci` succeeds and `CC.EN` is `1`.
    ready: bool,
}

impl Nvme {
    /// Construct the skeleton driver.
    #[must_use]
    pub const fn skeleton() -> Self {
        Self { sectors: 0, ready: false }
    }

    /// Probe the PCI bus for an NVMe controller. Deferred to W26.
    pub fn probe_pci() -> HyperResult<Self> {
        Err(HyperError::Unimplemented(
            "nvme: PCI probe not implemented (W26)",
        ))
    }
}

impl BlockDevice for Nvme {
    fn name(&self) -> &'static str { "nvme" }

    fn sector_count(&self) -> u64 { self.sectors }

    fn read_sectors(&self, _lba: u64, dst: &mut [u8]) -> HyperResult<()> {
        if dst.is_empty() {
            return Err(HyperError::Invalid("nvme: empty read buffer"));
        }
        if dst.len() % SECTOR_BYTES != 0 {
            return Err(HyperError::Invalid("nvme: read len % 512 != 0"));
        }
        if !self.ready {
            return Err(HyperError::Denied("nvme: device not ready"));
        }
        Err(HyperError::Unimplemented(
            "nvme: read_sectors not implemented (W26)",
        ))
    }

    fn write_sectors(&self, _lba: u64, src: &[u8]) -> HyperResult<()> {
        if src.is_empty() {
            return Err(HyperError::Invalid("nvme: empty write buffer"));
        }
        if src.len() % SECTOR_BYTES != 0 {
            return Err(HyperError::Invalid("nvme: write len % 512 != 0"));
        }
        if !self.ready {
            return Err(HyperError::Denied("nvme: device not ready"));
        }
        Err(HyperError::Unimplemented(
            "nvme: write_sectors not implemented (W26)",
        ))
    }

    fn flush(&self) -> HyperResult<()> {
        if !self.ready {
            return Err(HyperError::Denied("nvme: device not ready"));
        }
        Err(HyperError::Unimplemented(
            "nvme: flush not implemented (W26)",
        ))
    }

    fn is_ready(&self) -> bool { self.ready }
}
