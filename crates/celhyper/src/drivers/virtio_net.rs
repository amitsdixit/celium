//! W24-C — virtio-net driver (skeleton).
//!
//! Mirror of [`super::virtio_blk`]: pin the MMIO register offsets, PCI
//! vendor / device ids, and the [`NetDevice`] surface today so the
//! rest of the kernel (the future bridge → guest plumbing in W25) can
//! program against a stable abstraction. The deep impl (PCI probe,
//! queue allocation, MSI-X, descriptor management) is gated on the
//! PCI scanner that lands in W25 alongside the W23-F virtio-blk
//! follow-up.
//!
//! Every fallible call returns [`HyperError::Unimplemented`] with a
//! `W25` tag so the W23-C `Reply::Error` path surfaces a structured
//! failure to the host instead of letting the bridge time out.

#![cfg(not(test))]

use crate::error::{HyperError, HyperResult};

/// PCI device id for the modern virtio-net device (Virtio 1.0+).
pub const VIRTIO_PCI_DEVICE_NET_MODERN: u16 = 0x1041;
/// PCI device id for the legacy (transitional) virtio-net device.
pub const VIRTIO_PCI_DEVICE_NET_LEGACY: u16 = 0x1000;

/// Number of bytes in a MAC address.
pub const MAC_BYTES: usize = 6;

/// Maximum Ethernet frame size we will hand to a guest. 1518 covers a
/// classic untagged frame; we will revisit when we wire VLAN / jumbo
/// support in W26+.
pub const MAX_FRAME_BYTES: usize = 1518;

/// Modern virtio-net feature bits we care about. The Virtio v1.2
/// specification defines many more; the W24-C skeleton pins only
/// what we plan to negotiate.
pub mod features {
    /// Device handles checksum offloading.
    pub const CSUM:        u64 = 1 << 0;
    /// Driver handles checksum offloading.
    pub const GUEST_CSUM:  u64 = 1 << 1;
    /// MAC field in config space is valid.
    pub const MAC:         u64 = 1 << 5;
    /// Device supports STATUS field in config space (link up/down).
    pub const STATUS:      u64 = 1 << 16;
    /// Device supports the modern (transport v1) layout.
    pub const VERSION_1:   u64 = 1 << 32;
}

/// Modern virtio-net config-space layout, offsets are byte-relative
/// to the device-config BAR window (NOT the common-config window —
/// they are two separate PCI capabilities).
pub mod config {
    /// 6-byte MAC address (present when [`super::features::MAC`] is
    /// negotiated).
    pub const MAC:         usize = 0x00;
    /// 2-byte link status (1 = link up).
    pub const STATUS:      usize = 0x06;
    /// 2-byte max virtqueue pairs supported by the device.
    pub const MAX_VQ_PAIRS: usize = 0x08;
}

/// Common kernel-side network device surface. Object-safe so the
/// kernel can hold one trait object per attached NIC.
pub trait NetDevice {
    /// Stable, human-readable name for log lines.
    fn name(&self) -> &'static str;

    /// Hardware MAC address. Returns `[0; 6]` for un-probed devices.
    fn mac(&self) -> [u8; MAC_BYTES];

    /// Send an Ethernet frame. The frame MUST be ≤ [`MAX_FRAME_BYTES`].
    /// Returns [`HyperError::Invalid`] for oversize frames and
    /// [`HyperError::Unimplemented`] until the W25 driver lands.
    fn send_frame(&self, frame: &[u8]) -> HyperResult<()>;

    /// Receive an Ethernet frame into `dst`. Returns the number of
    /// bytes written. Returns [`HyperError::Exhausted`] when no frame
    /// is currently queued (the W25 driver will block on the RX
    /// virtqueue's used ring instead; the skeleton fails fast so
    /// callers don't accidentally busy-loop against an empty queue).
    fn recv_frame(&self, dst: &mut [u8]) -> HyperResult<usize>;

    /// Best-effort link state. `Some(true)` = link up, `Some(false)` =
    /// link down, `None` = device does not advertise a status field
    /// (legacy transport).
    fn link_up(&self) -> Option<bool>;
}

/// Skeleton virtio-net device. W25 will replace the `_phantom` shape
/// with real ownership of the MMIO bars, RX/TX virtqueues, and the
/// MSI-X vectors.
#[derive(Debug)]
pub struct VirtioNet {
    /// Negotiated MAC address. `[0; 6]` until probed.
    mac: [u8; MAC_BYTES],
}

impl VirtioNet {
    /// Construct the skeleton driver. Real probing is deferred.
    #[must_use]
    pub const fn skeleton() -> Self {
        Self { mac: [0; MAC_BYTES] }
    }

    /// Probe the PCI bus for a virtio-net device. Deferred to W25
    /// — the kernel has no PCI scanner yet.
    pub fn probe_pci() -> HyperResult<Self> {
        Err(HyperError::Unimplemented(
            "virtio_net: PCI probe not implemented (W25)",
        ))
    }
}

impl NetDevice for VirtioNet {
    fn name(&self) -> &'static str { "virtio-net" }

    fn mac(&self) -> [u8; MAC_BYTES] { self.mac }

    fn send_frame(&self, frame: &[u8]) -> HyperResult<()> {
        if frame.is_empty() {
            return Err(HyperError::Invalid("virtio_net: empty frame"));
        }
        if frame.len() > MAX_FRAME_BYTES {
            return Err(HyperError::Invalid("virtio_net: frame > MAX_FRAME_BYTES"));
        }
        Err(HyperError::Unimplemented(
            "virtio_net: send_frame not implemented (W25)",
        ))
    }

    fn recv_frame(&self, dst: &mut [u8]) -> HyperResult<usize> {
        if dst.len() < MAX_FRAME_BYTES {
            return Err(HyperError::Invalid("virtio_net: rx buf < MAX_FRAME_BYTES"));
        }
        Err(HyperError::Unimplemented(
            "virtio_net: recv_frame not implemented (W25)",
        ))
    }

    fn link_up(&self) -> Option<bool> {
        // Skeleton has no idea; modern guests will read this from the
        // STATUS config field once we negotiate it.
        None
    }
}
