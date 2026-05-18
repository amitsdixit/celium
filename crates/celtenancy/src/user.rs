//! Per-tenant users.

use serde::{Deserialize, Serialize};

use crate::auth::PasswordHashStr;
use crate::caps::TenantCaps;

/// Opaque per-tenant user identifier. Allocated monotonically by
/// the store; never re-used after a user is removed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct UserId(pub u64);

impl UserId {
    /// Raw u64.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for UserId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "user-{}", self.0)
    }
}

/// A user inside a tenant. The user's `caps` are guaranteed to be a
/// subset of the parent tenant's `root_caps` (enforced at insertion
/// time by [`crate::TenantStore::add_user`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Monotonic per-tenant id.
    pub id: UserId,
    /// User-visible name; must match the same `[A-Za-z0-9_-]+`
    /// rules as tenant names.
    pub name: String,
    /// Attenuated capability set.
    pub caps: TenantCaps,
    /// Optional Argon2id PHC password hash (W32). `None` means the
    /// user has no password set yet and cannot log in via
    /// [`crate::TenantStore::authenticate`]. `#[serde(default)]`
    /// keeps W27..W31 store files reopenable unchanged.
    #[serde(default)]
    pub password_hash: Option<PasswordHashStr>,
}
