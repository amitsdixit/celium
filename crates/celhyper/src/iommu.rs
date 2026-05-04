//! IOMMU programming: domain creation + device assignment for SR-IOV / PCIe
//! passthrough. Stub for v0.1; real DMAR/IVRS parsing arrives in Week-4.

use crate::error::{HyperError, HyperResult};

/// One IOMMU isolation domain. Backed by a VT-d context table or
/// AMD-Vi device table page in the real implementation.
#[derive(Debug, Clone, Copy)]
pub struct DomainId(pub u32);

/// PCI BDF (bus/device/function).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bdf {
    /// Bus number.
    pub bus:  u8,
    /// Device number (0..32).
    pub dev:  u8,
    /// Function number (0..8).
    pub func: u8,
}

/// IOMMU subsystem façade.
pub struct Iommu;

impl Iommu {
    /// Probe DMAR (Intel) or IVRS (AMD) tables and bring the IOMMU(s) online.
    pub fn init() -> HyperResult<Self> {
        Err(HyperError::Unimplemented("Iommu::init"))
    }

    /// Create a fresh domain with no devices and no mappings.
    pub fn create_domain(&self) -> HyperResult<DomainId> {
        Err(HyperError::Unimplemented("Iommu::create_domain"))
    }

    /// Move `device` into `domain`. Fails closed: a device that fails to
    /// bind stays in the kernel's quarantine domain.
    pub fn assign(&self, domain: DomainId, device: Bdf) -> HyperResult<()> {
        let _ = (domain, device);
        Err(HyperError::Unimplemented("Iommu::assign"))
    }
}
