//! Unified error type for every Celium component.
//!
//! All fallible APIs in Celium return [`CelResult<T>`]. Variants are coarse on
//! purpose; finer-grained context is attached via `tracing` spans, not through
//! exploding the enum.

use thiserror::Error;

/// Shorthand for `Result<T, CelError>`.
pub type CelResult<T> = core::result::Result<T, CelError>;

/// The single error taxonomy for every Celium crate.
#[derive(Debug, Error)]
pub enum CelError {
    /// A capability check failed on a control path.
    #[error("capability denied: {0}")]
    CapabilityDenied(&'static str),

    /// A resource (vCPU, EPT entry, IOMMU domain, ...) was exhausted.
    #[error("resource exhausted: {0}")]
    Exhausted(&'static str),

    /// An invariant was violated by an external input or peer.
    #[error("invalid input: {0}")]
    Invalid(&'static str),

    /// A piece of hardware reported failure or is missing a required feature.
    #[error("hardware error: {0}")]
    Hardware(&'static str),

    /// An I/O error from the host environment (only used outside `celhyper`).
    #[error("i/o error: {0}")]
    Io(String),

    /// Catch-all for unexpected internal state. Should be unreachable in
    /// production; appearing in logs indicates a bug.
    #[error("internal: {0}")]
    Internal(&'static str),
}

impl CelError {
    /// Stable short code suitable for metrics labels.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::CapabilityDenied(_) => "cap_denied",
            Self::Exhausted(_)        => "exhausted",
            Self::Invalid(_)          => "invalid",
            Self::Hardware(_)         => "hardware",
            Self::Io(_)               => "io",
            Self::Internal(_)         => "internal",
        }
    }
}
