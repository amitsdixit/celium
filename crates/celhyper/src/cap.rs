//! Capability-based IPC. Every control-plane operation (start vCPU, map EPT,
//! assign device, send to a port) goes through [`Capability::check`].
//!
//! The model is the classical seL4-style: capabilities are unforgeable
//! references to kernel objects, parameterised by a rights mask. v0.1 keeps
//! the table flat; v0.2 introduces capability spaces (cspaces).

use bitflags::bitflags;

bitflags! {
    /// Rights a capability may convey. Composable via bitwise OR.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Rights: u32 {
        /// Read the underlying object.
        const READ   = 1 << 0;
        /// Mutate the underlying object.
        const WRITE  = 1 << 1;
        /// Invoke / launch the underlying object.
        const INVOKE = 1 << 2;
        /// Delegate this capability further.
        const GRANT  = 1 << 3;
    }
}

/// Object kinds a capability can reference. Extensible — add a variant when
/// a new kernel object type appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    /// Whole VM.
    Vm(u32),
    /// vCPU within a VM.
    Vcpu {
        /// Owning VM id.
        vm: u32,
        /// vCPU index within that VM.
        vcpu: u32,
    },
    /// EPT mapping range within a VM.
    EptRange {
        /// Owning VM id.
        vm: u32,
    },
    /// IOMMU domain.
    IommuDomain(u32),
}

/// An immutable capability handle.
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    /// What this capability points at.
    pub object: Object,
    /// What you may do with it.
    pub rights: Rights,
}

impl Capability {
    /// Returns `Ok(())` iff `self` permits every right in `needed`.
    ///
    /// # Errors
    /// [`crate::HyperError::Denied`] when any required right is missing.
    pub fn check(&self, needed: Rights) -> crate::HyperResult<()> {
        if self.rights.contains(needed) {
            Ok(())
        } else {
            Err(crate::HyperError::Denied("rights mask"))
        }
    }
}
