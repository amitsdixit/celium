//! W25-C \u2014 PCI configuration-space scanner (legacy port-IO).
//!
//! This module is the prerequisite for every PCI-attached device
//! driver (`virtio_blk`, `virtio_net`, `virtio_console`, `nvme`). It
//! intentionally stops short of MMIO / capability walking: those land
//! in W26 alongside MSI-X programming. W25 ships:
//!
//! 1. The legacy IO-port mechanism (CF8/CFC). x86 PCs have supported
//!    this since 1992; modern boards with PCIe still expose the
//!    same config-space layout via this window.
//! 2. A typed [`PciAddress`] / [`PciDeviceInfo`] surface so callers
//!    can identify candidate devices by `(vendor, device, class)`
//!    without dealing with raw u32s.
//! 3. A bounded bus/device/function enumerator [`scan`] that visits
//!    every endpoint exactly once and hands the caller back the
//!    first match for a `(vendor, device)` pair.
//!
//! ## Why port-IO and not ECAM
//!
//! Enhanced Configuration Access Mechanism (ECAM) is faster (it's
//! straight MMIO) but requires the firmware-supplied MCFG table to
//! locate the base address. CelLoader does not yet expose MCFG via
//! the handoff; doing port-IO here unblocks the driver work without
//! waiting for that. The W26 milestone will add an ECAM fast path
//! and keep this one as the fallback.
//!
//! ## Safety
//!
//! Port-IO at 0xCF8 / 0xCFC is harmless on every x86_64 platform at
//! CPL 0: writing CF8 sets a selector, reading CFC returns the value
//! at that selector. Every `unsafe` block in this module has the
//! same justification.

#![cfg(not(test))]

use x86_64::instructions::port::Port;

use crate::error::{HyperError, HyperResult};

/// Port for the PCI configuration address register.
pub const CONFIG_ADDRESS: u16 = 0x0CF8;
/// Port for the PCI configuration data register.
pub const CONFIG_DATA: u16 = 0x0CFC;

/// `vendor == 0xFFFF` means \"no device present at this address\".
pub const VENDOR_NONE: u16 = 0xFFFF;

/// Maximum bus number we scan. The PCI spec allows 256 but most
/// servers populate \u22648; we walk all 256 because the scan is
/// O(256 \u00d7 32 \u00d7 8) \u2248 65k port-IO ops, well under 50 ms even on
/// slow firmware.
pub const MAX_BUS: u16 = 256;
/// Maximum device number per bus (5-bit field).
pub const MAX_DEVICE: u8 = 32;
/// Maximum function number per device (3-bit field).
pub const MAX_FUNCTION: u8 = 8;

/// A `(bus, device, function)` triple identifying a PCI endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciAddress {
    /// Bus number, 0..=255.
    pub bus: u8,
    /// Device number, 0..=31.
    pub device: u8,
    /// Function number, 0..=7.
    pub function: u8,
}

impl PciAddress {
    /// Pack into the 32-bit `CONFIG_ADDRESS` layout (SDM-equivalent
    /// for x86 PCI; see PCI 3.0 \u00a73.2.2.3.2):
    ///
    /// | bits  | meaning                                   |
    /// |-------|-------------------------------------------|
    /// | 31    | enable (1)                                |
    /// | 30:24 | reserved                                  |
    /// | 23:16 | bus number                                |
    /// | 15:11 | device number                             |
    /// | 10:8  | function number                           |
    /// | 7:2   | register offset (DWORD-aligned)           |
    /// | 1:0   | reserved (0)                              |
    #[must_use]
    pub const fn pack(self, offset: u8) -> u32 {
        let enable = 1u32 << 31;
        let bus = (self.bus as u32) << 16;
        let dev = ((self.device as u32) & 0x1F) << 11;
        let func = ((self.function as u32) & 0x07) << 8;
        let off = (offset as u32) & 0xFC;
        enable | bus | dev | func | off
    }
}

/// One enumerated PCI device. All fields are read from config space
/// at scan time; the kernel never re-reads them later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciDeviceInfo {
    /// Bus/device/function this entry refers to.
    pub address: PciAddress,
    /// `vendor_id` from config offset 0x00.
    pub vendor: u16,
    /// `device_id` from config offset 0x02.
    pub device: u16,
    /// `class_code` from config offset 0x0B.
    pub class: u8,
    /// `subclass` from config offset 0x0A.
    pub subclass: u8,
    /// `prog_if` from config offset 0x09.
    pub prog_if: u8,
    /// `revision_id` from config offset 0x08.
    pub revision: u8,
    /// `header_type` from config offset 0x0E (bit 7 = multifunction).
    pub header_type: u8,
}

/// Read a 32-bit configuration register.
///
/// # Safety contract observed internally
///
/// The two port writes / reads are atomic with respect to each other
/// only on a single CPU \u2014 we therefore only call this from the BSP
/// boot path. APs that need to enumerate PCI must serialise through
/// a future `pci::scan_global()` once W26 introduces a config lock.
fn read_config_u32(addr: PciAddress, offset: u8) -> u32 {
    let packed = addr.pack(offset);
    // SAFETY: 0xCF8/0xCFC are legacy x86 PCI configuration ports;
    // writes have no architectural side effect outside the selector
    // and the read returns the selected register.
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        addr_port.write(packed);
        data_port.read()
    }
}

/// Read the `(vendor, device)` pair at `addr`. Returns `None` when
/// the bus reports `0xFFFF` (no endpoint).
pub fn probe_vendor_device(addr: PciAddress) -> Option<(u16, u16)> {
    let raw = read_config_u32(addr, 0x00);
    let vendor = (raw & 0xFFFF) as u16;
    if vendor == VENDOR_NONE {
        return None;
    }
    let device = (raw >> 16) as u16;
    Some((vendor, device))
}

/// Build a [`PciDeviceInfo`] for a known-present address. Reads two
/// config-space DWORDs.
pub fn describe(addr: PciAddress) -> Option<PciDeviceInfo> {
    let (vendor, device) = probe_vendor_device(addr)?;
    let class_word = read_config_u32(addr, 0x08);
    let header_word = read_config_u32(addr, 0x0C);
    Some(PciDeviceInfo {
        address: addr,
        vendor,
        device,
        revision: (class_word & 0xFF) as u8,
        prog_if: ((class_word >> 8) & 0xFF) as u8,
        subclass: ((class_word >> 16) & 0xFF) as u8,
        class: ((class_word >> 24) & 0xFF) as u8,
        header_type: ((header_word >> 16) & 0xFF) as u8,
    })
}

/// Find the first PCI device matching `(vendor, device)`.
///
/// O(bus \u00d7 device \u00d7 function) port-IO scan; bounded by the constants
/// above. Returns `None` if no match is found.
///
/// Multi-function devices are handled correctly: a function-0 with
/// the `multifunction` bit clear short-circuits all 7 other
/// functions on that device.
#[must_use]
pub fn find_first(vendor_match: u16, device_match: u16) -> Option<PciDeviceInfo> {
    for bus in 0..MAX_BUS {
        for device in 0..MAX_DEVICE {
            let func0 = PciAddress { bus: bus as u8, device, function: 0 };
            let info0 = match describe(func0) {
                Some(i) => i,
                None => continue,
            };
            if info0.vendor == vendor_match && info0.device == device_match {
                return Some(info0);
            }
            // If this isn't a multi-function header we can skip the
            // remaining 7 function slots immediately.
            if info0.header_type & 0x80 == 0 {
                continue;
            }
            for function in 1..MAX_FUNCTION {
                let addr = PciAddress { bus: bus as u8, device, function };
                if let Some(info) = describe(addr) {
                    if info.vendor == vendor_match && info.device == device_match {
                        return Some(info);
                    }
                }
            }
        }
    }
    None
}

/// Run a full bus scan and invoke `visit` for every present device.
/// `visit` may short-circuit by returning `Err`; the typed error is
/// propagated unchanged so callers can encode \"found what I wanted\"
/// without inventing a sentinel.
pub fn scan<F>(mut visit: F) -> HyperResult<u32>
where
    F: FnMut(&PciDeviceInfo) -> HyperResult<()>,
{
    let mut count: u32 = 0;
    for bus in 0..MAX_BUS {
        for device in 0..MAX_DEVICE {
            let func0 = PciAddress { bus: bus as u8, device, function: 0 };
            let info0 = match describe(func0) {
                Some(i) => i,
                None => continue,
            };
            count += 1;
            visit(&info0)?;
            if info0.header_type & 0x80 == 0 {
                continue;
            }
            for function in 1..MAX_FUNCTION {
                let addr = PciAddress { bus: bus as u8, device, function };
                if let Some(info) = describe(addr) {
                    count += 1;
                    visit(&info)?;
                }
            }
        }
    }
    if count == 0 {
        // No PCI bus is *extraordinarily* rare on x86_64; we surface
        // it as Hardware rather than Ok(0) so a misrouted scan is
        // visible in the boot log.
        return Err(HyperError::Hardware("pci: scan found zero devices"));
    }
    Ok(count)
}
