//! Smoke test: can the std-side crates be linked together and used?
//! This is the only test the workspace runs today; bare-metal crates have
//! their own (host-target) unit tests landing in Week-2.

use celcommon::{ids::VmId, CelError, CelResult};

fn touch() -> CelResult<VmId> {
    Ok(VmId(1))
}

#[test]
fn celcommon_is_usable() {
    let id = touch().expect("infallible in test");
    assert_eq!(id.0, 1);
    assert_eq!(CelError::Invalid("x").code(), "invalid");
}
