//! Error type for the bare-metal kernel. Distinct from `celcommon::CelError`
//! because that one is `std`-flavoured.

/// Result alias used throughout `celhyper`.
pub type HyperResult<T> = core::result::Result<T, HyperError>;

/// Coarse error taxonomy for the kernel. Fine-grained context is logged
/// via the serial logger, never carried in the error value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperError {
    /// CelLoader handoff block failed validation.
    InvalidHandoff(&'static str),
    /// CPU lacks a feature CelHyper needs (VMX, EPT, etc.).
    UnsupportedCpu(&'static str),
    /// Required hardware (IOMMU, APIC, ...) is missing or refused setup.
    Hardware(&'static str),
    /// Out of a fixed-size kernel resource pool.
    Exhausted(&'static str),
    /// Capability check failed.
    Denied(&'static str),
    /// Caller-supplied input failed validation (malformed path, bad
    /// argument, ...). Distinct from `Denied` (which is rights-driven)
    /// and `InvalidHandoff` (which is the boot block specifically).
    Invalid(&'static str),
    /// Code path not yet implemented (Week-1 placeholder).
    Unimplemented(&'static str),
    /// An invariant the kernel itself was supposed to maintain has been
    /// violated. Reaching this is a bug.
    Internal(&'static str),
}
