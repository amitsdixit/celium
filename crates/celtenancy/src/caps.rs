//! Tenancy-layer capability bitset.
//!
//! Mirrors [`celmesh::Capabilities`] one-to-one. The tenancy layer
//! uses its own bitset because:
//!
//! * Core-layer types are deliberately opaque (no `bits()` accessor)
//!   and must stay so per the W26 Tenancy-Layer sign-off contract.
//! * We need a `serde`-friendly, persisted representation for the
//!   tenant store; a raw `u32` is the natural fit.
//! * Projection to [`celmesh::Capabilities`] happens in exactly one
//!   place \u2014 [`TenantCaps::to_mesh_capabilities`] \u2014 so any future
//!   drift is a one-line diff to audit.

use celcommon::{CelError, CelResult};
use celmesh::Capabilities;
use serde::{Deserialize, Serialize};

/// Bitset of tenancy-layer capabilities. Each bit mirrors a single
/// [`celmesh::Capabilities`] constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TenantCaps(u32);

impl TenantCaps {
    /// No capabilities.
    pub const NONE: Self = Self(0);

    // Bit positions intentionally match the comment ordering in
    // celmesh::Capabilities so a side-by-side audit is trivial.

    /// VM lifecycle read (`List`).
    pub const VM_LIFECYCLE_READ: Self = Self(1 << 0);
    /// VM lifecycle write (`Create` / `Start` / `Stop` / `Delete`).
    pub const VM_LIFECYCLE_WRITE: Self = Self(1 << 1);
    /// Volume read.
    pub const VOLUME_READ: Self = Self(1 << 2);
    /// Volume write.
    pub const VOLUME_WRITE: Self = Self(1 << 3);
    /// Volume attach/detach.
    pub const VOLUME_ATTACH: Self = Self(1 << 4);
    /// Snapshot read.
    pub const SNAPSHOT_READ: Self = Self(1 << 5);
    /// Snapshot write.
    pub const SNAPSHOT_WRITE: Self = Self(1 << 6);
    /// Network read.
    pub const NETWORK_READ: Self = Self(1 << 7);
    /// Network write.
    pub const NETWORK_WRITE: Self = Self(1 << 8);
    /// Security-group read.
    pub const SECGROUP_READ: Self = Self(1 << 9);
    /// Security-group write.
    pub const SECGROUP_WRITE: Self = Self(1 << 10);
    /// Load-balancer read.
    pub const LB_READ: Self = Self(1 << 11);
    /// Load-balancer write.
    pub const LB_WRITE: Self = Self(1 << 12);

    /// Every capability granted. The natural default for a tenant
    /// root cap unless the operator wants a read-only tenant.
    pub const ALL: Self = Self(
        Self::VM_LIFECYCLE_READ.0
            | Self::VM_LIFECYCLE_WRITE.0
            | Self::VOLUME_READ.0
            | Self::VOLUME_WRITE.0
            | Self::VOLUME_ATTACH.0
            | Self::SNAPSHOT_READ.0
            | Self::SNAPSHOT_WRITE.0
            | Self::NETWORK_READ.0
            | Self::NETWORK_WRITE.0
            | Self::SECGROUP_READ.0
            | Self::SECGROUP_WRITE.0
            | Self::LB_READ.0
            | Self::LB_WRITE.0,
    );

    /// Raw bits, for serialisation / logging only.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Build from raw bits. Unknown bits are silently truncated so
    /// future expansions of [`TenantCaps::ALL`] remain
    /// forwards-compatible at the store layer.
    #[must_use]
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & Self::ALL.0)
    }

    /// Subset test.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Set union (idempotent).
    pub fn grant(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Set difference (idempotent).
    pub fn revoke(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// Parse a comma-separated tag list. Unknown tags return
    /// [`CelError::Invalid`].
    ///
    /// Accepted tags (each may be prefixed `tenant.` for readability):
    /// `vm.read`, `vm.write`, `vol.read`, `vol.write`, `vol.attach`,
    /// `snap.read`, `snap.write`, `net.read`, `net.write`, `sg.read`,
    /// `sg.write`, `lb.read`, `lb.write`, `all`, `none`.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Invalid`] when an unknown tag appears.
    pub fn parse_tags(s: &str) -> CelResult<Self> {
        let mut out = Self::NONE;
        for raw in s.split(',') {
            let tag = raw.trim();
            if tag.is_empty() {
                continue;
            }
            let normalised = tag.strip_prefix("tenant.").unwrap_or(tag);
            let bit = match normalised {
                "none" => Self::NONE,
                "all" => Self::ALL,
                "vm.read" => Self::VM_LIFECYCLE_READ,
                "vm.write" => Self::VM_LIFECYCLE_WRITE,
                "vol.read" => Self::VOLUME_READ,
                "vol.write" => Self::VOLUME_WRITE,
                "vol.attach" => Self::VOLUME_ATTACH,
                "snap.read" => Self::SNAPSHOT_READ,
                "snap.write" => Self::SNAPSHOT_WRITE,
                "net.read" => Self::NETWORK_READ,
                "net.write" => Self::NETWORK_WRITE,
                "sg.read" => Self::SECGROUP_READ,
                "sg.write" => Self::SECGROUP_WRITE,
                "lb.read" => Self::LB_READ,
                "lb.write" => Self::LB_WRITE,
                _ => return Err(CelError::Invalid("tenant cap tag")),
            };
            out.grant(bit);
        }
        Ok(out)
    }

    /// Render as a deterministic comma-separated tag list. Used by
    /// audit logs and `celctl tenant show`.
    #[must_use]
    pub fn to_tags(self) -> String {
        if self == Self::NONE {
            return "none".to_string();
        }
        if self == Self::ALL {
            return "all".to_string();
        }
        // Order matches the bit ordering so output is stable.
        let entries: [(Self, &'static str); 13] = [
            (Self::VM_LIFECYCLE_READ, "vm.read"),
            (Self::VM_LIFECYCLE_WRITE, "vm.write"),
            (Self::VOLUME_READ, "vol.read"),
            (Self::VOLUME_WRITE, "vol.write"),
            (Self::VOLUME_ATTACH, "vol.attach"),
            (Self::SNAPSHOT_READ, "snap.read"),
            (Self::SNAPSHOT_WRITE, "snap.write"),
            (Self::NETWORK_READ, "net.read"),
            (Self::NETWORK_WRITE, "net.write"),
            (Self::SECGROUP_READ, "sg.read"),
            (Self::SECGROUP_WRITE, "sg.write"),
            (Self::LB_READ, "lb.read"),
            (Self::LB_WRITE, "lb.write"),
        ];
        let mut out = String::new();
        for (bit, tag) in entries {
            if self.contains(bit) {
                if !out.is_empty() {
                    out.push(',');
                }
                out.push_str(tag);
            }
        }
        out
    }

    /// Project to a [`celmesh::Capabilities`] value suitable for
    /// `MemVmHost::with_caps(\u2026)`. This is the only place where
    /// tenancy caps cross the Core-Layer boundary.
    #[must_use]
    pub fn to_mesh_capabilities(self) -> Capabilities {
        let mut c = Capabilities::NONE;
        if self.contains(Self::VM_LIFECYCLE_READ) {
            c |= Capabilities::VM_LIFECYCLE_READ;
        }
        if self.contains(Self::VM_LIFECYCLE_WRITE) {
            c |= Capabilities::VM_LIFECYCLE_WRITE;
        }
        if self.contains(Self::VOLUME_READ) {
            c |= Capabilities::VOLUME_READ;
        }
        if self.contains(Self::VOLUME_WRITE) {
            c |= Capabilities::VOLUME_WRITE;
        }
        if self.contains(Self::VOLUME_ATTACH) {
            c |= Capabilities::VOLUME_ATTACH;
        }
        if self.contains(Self::SNAPSHOT_READ) {
            c |= Capabilities::SNAPSHOT_READ;
        }
        if self.contains(Self::SNAPSHOT_WRITE) {
            c |= Capabilities::SNAPSHOT_WRITE;
        }
        if self.contains(Self::NETWORK_READ) {
            c |= Capabilities::NETWORK_READ;
        }
        if self.contains(Self::NETWORK_WRITE) {
            c |= Capabilities::NETWORK_WRITE;
        }
        if self.contains(Self::SECGROUP_READ) {
            c |= Capabilities::SECGROUP_READ;
        }
        if self.contains(Self::SECGROUP_WRITE) {
            c |= Capabilities::SECGROUP_WRITE;
        }
        if self.contains(Self::LB_READ) {
            c |= Capabilities::LB_READ;
        }
        if self.contains(Self::LB_WRITE) {
            c |= Capabilities::LB_WRITE;
        }
        c
    }
}

impl core::ops::BitOr for TenantCaps {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for TenantCaps {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Compute an attenuated capability set for a user inside a tenant.
///
/// The user's `requested` set must be a subset of the tenant's
/// `root` set. Anything broader returns
/// [`CelError::CapabilityDenied`] with the stable tag
/// `tenant.user.attenuate`.
///
/// # Errors
///
/// Returns [`CelError::CapabilityDenied`] when `requested` carries a
/// bit that `root` does not.
pub fn attenuate(root: TenantCaps, requested: TenantCaps) -> CelResult<TenantCaps> {
    if !root.contains(requested) {
        return Err(CelError::CapabilityDenied("tenant.user.attenuate"));
    }
    Ok(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_round_trips_through_mesh_capabilities() {
        let mesh = TenantCaps::ALL.to_mesh_capabilities();
        assert!(mesh.contains(Capabilities::VM_LIFECYCLE_WRITE));
        assert!(mesh.contains(Capabilities::VOLUME_ATTACH));
        assert!(mesh.contains(Capabilities::LB_WRITE));
    }

    #[test]
    fn none_projects_to_no_mesh_caps() {
        let mesh = TenantCaps::NONE.to_mesh_capabilities();
        assert!(!mesh.contains(Capabilities::VM_LIFECYCLE_READ));
    }

    #[test]
    fn parse_and_render_round_trip() {
        let parsed = TenantCaps::parse_tags("vm.read, vm.write,vol.read").unwrap();
        let tags = parsed.to_tags();
        let reparsed = TenantCaps::parse_tags(&tags).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn parse_unknown_tag_errors() {
        assert!(TenantCaps::parse_tags("vm.read,bogus").is_err());
    }

    #[test]
    fn attenuate_requires_subset() {
        let root = TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE;
        assert!(attenuate(root, TenantCaps::VM_LIFECYCLE_READ).is_ok());
        assert!(attenuate(root, TenantCaps::VOLUME_WRITE).is_err());
    }

    #[test]
    fn from_bits_truncate_drops_unknown_bits() {
        let raw = 0xFFFF_FFFF;
        let parsed = TenantCaps::from_bits_truncate(raw);
        assert_eq!(parsed, TenantCaps::ALL);
    }

    #[test]
    fn all_tag_round_trips() {
        let parsed = TenantCaps::parse_tags("all").unwrap();
        assert_eq!(parsed, TenantCaps::ALL);
        assert_eq!(parsed.to_tags(), "all");
    }

    #[test]
    fn none_tag_round_trips() {
        let parsed = TenantCaps::parse_tags("none").unwrap();
        assert_eq!(parsed, TenantCaps::NONE);
        assert_eq!(parsed.to_tags(), "none");
    }
}
