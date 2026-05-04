//! Strongly-typed identifiers used across the fabric.

use serde::{Deserialize, Serialize};

macro_rules! newtype_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

newtype_id!(/// Identifier for a guest VM. VmId(0) is reserved for the host control plane.
    VmId);
newtype_id!(/// Identifier for a virtual CPU within a [`VmId`].
    VcpuId);
newtype_id!(/// Identifier for an IOMMU isolation domain.
    IommuDomainId);
newtype_id!(/// Identifier for a capability handle held by a guest or higher layer.
    CapId);
