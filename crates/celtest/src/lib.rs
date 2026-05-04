//! CelTest — integration test + chaos harness. Placeholder for the v0.1 sprint.
#![forbid(unsafe_code)]
#![warn(missing_docs, rust_2018_idioms)]

use celcommon::CelResult;

/// Returns `Ok(())` once the test harness has been initialised.
///
/// # Errors
/// Currently infallible; signature reserved for future fixture wiring.
pub fn init() -> CelResult<()> {
    tracing::debug!("celtest::init (stub)");
    Ok(())
}
