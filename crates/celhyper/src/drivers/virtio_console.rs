//! W25-D \u2014 virtio-console driver (skeleton).
//!
//! Same shape as [`super::virtio_blk`] and [`super::virtio_net`]:
//! pin the wire layout and the [`ConsoleDevice`] trait surface this
//! week, defer the actual probe / virtqueue / IRQ plumbing to W26.
//!
//! virtio-console is the cleanest path for a future `celctl shell`
//! style operator pipe \u2014 it is a simple byte-stream device, no
//! framing, no checksums, ideal for surfacing the bridge logger
//! over a fast in-guest channel instead of COM1.

#![cfg(not(test))]

use crate::error::{HyperError, HyperResult};

/// PCI device id for the modern virtio-console device (Virtio 1.0+).
pub const VIRTIO_PCI_DEVICE_CONSOLE_MODERN: u16 = 0x1043;
/// PCI device id for the legacy virtio-console device.
pub const VIRTIO_PCI_DEVICE_CONSOLE_LEGACY: u16 = 0x1003;

/// Maximum number of console ports the W25 driver advertises.
///
/// Pinned to 1 today because the bridge is single-tenant; W26 will
/// raise this when we land per-VM console multiplexing.
pub const MAX_PORTS: usize = 1;

/// Modern virtio-console feature bits (Virtio v1.2 \u00a75.3.3).
pub mod features {
    /// Device supports the standard config layout.
    pub const SIZE:       u64 = 1 << 0;
    /// Device supports multi-port (we negotiate this in W26 when
    /// [`super::MAX_PORTS`] grows).
    pub const MULTIPORT:  u64 = 1 << 1;
    /// Device emits a notification when a port appears.
    pub const EMERG_WRITE: u64 = 1 << 2;
}

/// Modern config-space layout (offsets are byte-relative to the
/// device-config BAR window).
pub mod config {
    /// 2-byte console columns.
    pub const COLS:       usize = 0x00;
    /// 2-byte console rows.
    pub const ROWS:       usize = 0x02;
    /// 4-byte max number of ports.
    pub const MAX_PORTS:  usize = 0x04;
    /// 4-byte emergency-write port hook.
    pub const EMERG_WR:   usize = 0x08;
}

/// Common kernel-side console-device surface. Object-safe so the
/// kernel can hold one trait object per attached console.
pub trait ConsoleDevice {
    /// Stable, human-readable name suitable for log lines.
    fn name(&self) -> &'static str;

    /// Whether the device is probed and usable. Skeletons return
    /// `false`; the W26 probe path flips this once the virtqueue is
    /// armed.
    fn is_ready(&self) -> bool;

    /// Write a byte slice to the console. The W26 driver will push
    /// `bytes` onto the TX virtqueue; the skeleton returns
    /// [`HyperError::Unimplemented`] after validating the input.
    fn write_bytes(&self, bytes: &[u8]) -> HyperResult<()>;

    /// Read into `dst`. Returns the number of bytes copied (zero is
    /// a valid \"nothing pending\" answer once the W26 driver is
    /// live).
    fn read_bytes(&self, dst: &mut [u8]) -> HyperResult<usize>;
}

/// Skeleton virtio-console device.
#[derive(Debug, Default)]
pub struct VirtioConsole {
    /// True once `probe_pci` succeeds. Always `false` today.
    ready: bool,
}

impl VirtioConsole {
    /// Construct the skeleton driver.
    #[must_use]
    pub const fn skeleton() -> Self {
        Self { ready: false }
    }

    /// Probe the PCI bus for a virtio-console device. Deferred to
    /// W26 \u2014 the W25 PCI scanner can find the device but the
    /// virtqueue / MSI-X plumbing isn't ready to drive it yet.
    pub fn probe_pci() -> HyperResult<Self> {
        Err(HyperError::Unimplemented(
            "virtio_console: PCI probe not implemented (W26)",
        ))
    }
}

impl ConsoleDevice for VirtioConsole {
    fn name(&self) -> &'static str { "virtio-console" }

    fn is_ready(&self) -> bool { self.ready }

    fn write_bytes(&self, bytes: &[u8]) -> HyperResult<()> {
        if bytes.is_empty() {
            return Err(HyperError::Invalid("virtio_console: empty write"));
        }
        if !self.ready {
            return Err(HyperError::Denied("virtio_console: not ready"));
        }
        Err(HyperError::Unimplemented(
            "virtio_console: write_bytes not implemented (W26)",
        ))
    }

    fn read_bytes(&self, dst: &mut [u8]) -> HyperResult<usize> {
        if dst.is_empty() {
            return Err(HyperError::Invalid("virtio_console: empty read"));
        }
        if !self.ready {
            return Err(HyperError::Denied("virtio_console: not ready"));
        }
        Err(HyperError::Unimplemented(
            "virtio_console: read_bytes not implemented (W26)",
        ))
    }
}
