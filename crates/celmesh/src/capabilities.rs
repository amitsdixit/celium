//! Capability-based authorisation for [`crate::host::VmHost`] ops.
//!
//! W14 introduces a small, audit-friendly capability set that gates
//! every host-side operation. The check happens **inside the host**,
//! after the request has been decoded — the wire protocol stays
//! unchanged so old peers still federate.
//!
//! Capabilities are coarse on purpose. The four buckets — VM
//! lifecycle, volume CRUD, volume IO, and snapshots — match the
//! audit-log granularity an operator actually wants. Finer-grained
//! per-VM ACLs can layer on later without changing this surface.
//!
//! ```ignore
//! use celmesh::{Capabilities, MemVmHost};
//! // Read-only host: peers can list VMs and read volumes, nothing
//! // else.
//! let caps = Capabilities::VM_LIFECYCLE_READ
//!     | Capabilities::VOLUME_READ
//!     | Capabilities::SNAPSHOT_READ;
//! let host = MemVmHost::new().with_caps(caps);
//! ```
//!
//! The default for [`MemVmHost::new`] is [`Capabilities::ALL`] so
//! existing call sites and tests continue to work; downstream
//! integrators wire restrictive sets explicitly.

use std::ops::{BitOr, BitOrAssign};

use crate::proto::VmOp;

/// Bit-set of capabilities granted to a [`crate::host::VmHost`].
///
/// The internal representation is a `u32` for cheap copy / OR; the
/// public surface is a small set of `pub const`s plus `contains`,
/// `grant`, `revoke`, and a `BitOr` impl so users can compose sets
/// with `|`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities(u32);

impl Capabilities {
    /// No capabilities. A host with [`Capabilities::NONE`] rejects
    /// every op except `List` / `ListVolumes` / `ListSnapshots` —
    /// those still require [`Capabilities::VM_LIFECYCLE_READ`] /
    /// [`Capabilities::VOLUME_READ`] / [`Capabilities::SNAPSHOT_READ`].
    pub const NONE:                 Self = Self(0);

    /// Read-only VM lifecycle: `List`.
    pub const VM_LIFECYCLE_READ:    Self = Self(1 << 0);
    /// Mutating VM lifecycle: `Create`, `Start`, `Stop`, `Delete`.
    pub const VM_LIFECYCLE_WRITE:   Self = Self(1 << 1);
    /// Read-only volume CRUD: `ListVolumes`, `ReadVolume`.
    pub const VOLUME_READ:          Self = Self(1 << 2);
    /// Mutating volume CRUD: `CreateVolume`, `DeleteVolume`,
    /// `WriteVolume`.
    pub const VOLUME_WRITE:         Self = Self(1 << 3);
    /// Volume → VM attachment ops: `AttachVolume`, `DetachVolume`.
    pub const VOLUME_ATTACH:        Self = Self(1 << 4);
    /// Read-only snapshot ops: `ListSnapshots`.
    pub const SNAPSHOT_READ:        Self = Self(1 << 5);
    /// Mutating snapshot ops: `CreateSnapshot`, `DeleteSnapshot`,
    /// `RestoreSnapshot`.
    pub const SNAPSHOT_WRITE:       Self = Self(1 << 6);

    /// Every capability granted. The default for [`MemVmHost::new`]
    /// so back-compat tests don't have to wire caps explicitly.
    pub const ALL: Self = Self(
        Self::VM_LIFECYCLE_READ.0
            | Self::VM_LIFECYCLE_WRITE.0
            | Self::VOLUME_READ.0
            | Self::VOLUME_WRITE.0
            | Self::VOLUME_ATTACH.0
            | Self::SNAPSHOT_READ.0
            | Self::SNAPSHOT_WRITE.0,
    );

    /// Test for a single capability (or any bit-set).
    #[must_use]
    pub fn contains(self, c: Self) -> bool { (self.0 & c.0) == c.0 }

    /// Add capabilities. Idempotent.
    pub fn grant(&mut self, c: Self) { self.0 |= c.0; }

    /// Remove capabilities. Idempotent.
    pub fn revoke(&mut self, c: Self) { self.0 &= !c.0; }

    /// Capability required to apply `op`. Used by the host to
    /// authorise incoming requests.
    #[must_use]
    pub fn required(op: &VmOp) -> Self {
        match op {
            VmOp::Create { .. }
            | VmOp::Start  { .. }
            | VmOp::Stop   { .. }
            | VmOp::Delete { .. }                  => Self::VM_LIFECYCLE_WRITE,
            VmOp::List                             => Self::VM_LIFECYCLE_READ,
            VmOp::CreateVolume   { .. }
            | VmOp::DeleteVolume { .. }
            | VmOp::WriteVolume  { .. }            => Self::VOLUME_WRITE,
            VmOp::ListVolumes
            | VmOp::ReadVolume   { .. }            => Self::VOLUME_READ,
            VmOp::AttachVolume   { .. }
            | VmOp::DetachVolume { .. }            => Self::VOLUME_ATTACH,
            VmOp::CreateSnapshot  { .. }
            | VmOp::DeleteSnapshot { .. }
            | VmOp::RestoreSnapshot { .. }         => Self::SNAPSHOT_WRITE,
            VmOp::ListSnapshots { .. }             => Self::SNAPSHOT_READ,
        }
    }

    /// Stable short name used in logs / error strings.
    #[must_use]
    pub fn op_tag(op: &VmOp) -> &'static str {
        match op {
            VmOp::Create          { .. } => "vm.create",
            VmOp::Start           { .. } => "vm.start",
            VmOp::Stop            { .. } => "vm.stop",
            VmOp::Delete          { .. } => "vm.delete",
            VmOp::List                   => "vm.list",
            VmOp::CreateVolume    { .. } => "vol.create",
            VmOp::DeleteVolume    { .. } => "vol.delete",
            VmOp::ListVolumes            => "vol.list",
            VmOp::ReadVolume      { .. } => "vol.read",
            VmOp::WriteVolume     { .. } => "vol.write",
            VmOp::AttachVolume    { .. } => "vol.attach",
            VmOp::DetachVolume    { .. } => "vol.detach",
            VmOp::CreateSnapshot  { .. } => "snap.create",
            VmOp::ListSnapshots   { .. } => "snap.list",
            VmOp::DeleteSnapshot  { .. } => "snap.delete",
            VmOp::RestoreSnapshot { .. } => "snap.restore",
        }
    }
}

impl BitOr for Capabilities {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}

impl BitOrAssign for Capabilities {
    fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
}

#[cfg(test)]
mod tests {
    use super::*;
    use celvault::VolumeId;

    #[test]
    fn all_includes_every_named_cap() {
        let all = Capabilities::ALL;
        assert!(all.contains(Capabilities::VM_LIFECYCLE_READ));
        assert!(all.contains(Capabilities::VM_LIFECYCLE_WRITE));
        assert!(all.contains(Capabilities::VOLUME_READ));
        assert!(all.contains(Capabilities::VOLUME_WRITE));
        assert!(all.contains(Capabilities::VOLUME_ATTACH));
        assert!(all.contains(Capabilities::SNAPSHOT_READ));
        assert!(all.contains(Capabilities::SNAPSHOT_WRITE));
    }

    #[test]
    fn grant_revoke_is_idempotent() {
        let mut c = Capabilities::NONE;
        c.grant(Capabilities::VOLUME_READ);
        c.grant(Capabilities::VOLUME_READ);
        assert!(c.contains(Capabilities::VOLUME_READ));
        c.revoke(Capabilities::VOLUME_READ);
        c.revoke(Capabilities::VOLUME_READ);
        assert!(!c.contains(Capabilities::VOLUME_READ));
    }

    #[test]
    fn required_for_each_op() {
        let v = VolumeId::from("n1/v1");
        assert_eq!(
            Capabilities::required(&VmOp::List),
            Capabilities::VM_LIFECYCLE_READ
        );
        assert_eq!(
            Capabilities::required(&VmOp::Create {
                label: "x".into(),
                restart_policy: crate::federation::RestartPolicy::Never,
            }),
            Capabilities::VM_LIFECYCLE_WRITE
        );
        assert_eq!(
            Capabilities::required(&VmOp::WriteVolume {
                volume_id: v.clone(),
                offset: 0,
                bytes: vec![],
            }),
            Capabilities::VOLUME_WRITE
        );
        assert_eq!(
            Capabilities::required(&VmOp::ReadVolume {
                volume_id: v,
                offset: 0,
                len: 0,
            }),
            Capabilities::VOLUME_READ
        );
    }
}
