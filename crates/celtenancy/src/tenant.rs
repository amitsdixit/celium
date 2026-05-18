//! Tenant identity and on-disk record.

use serde::{Deserialize, Serialize};

use crate::caps::TenantCaps;
use crate::namespace::validate_segment;
use crate::quota::{QuotaUsage, TenantQuotas};
use crate::user::User;
use celcommon::CelResult;

/// Opaque per-store tenant identifier. Allocated monotonically by
/// the store; never re-used after [`crate::TenantStore::delete`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TenantId(pub u64);

impl TenantId {
    /// Raw u64.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for TenantId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "tenant-{}", self.0)
    }
}

/// Operator-supplied tenant specification at creation time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantSpec {
    /// Tenant name (path-segment). Must match `[A-Za-z0-9_-]+`,
    /// 1..=64 bytes.
    pub name: String,
    /// Quotas applied to this tenant.
    pub quotas: TenantQuotas,
    /// Optional parent tenant (W31 — nested tenants).
    ///
    /// When set the store enforces:
    /// * caps must be a subset of parent's `root_caps`
    /// * every quota dimension must be ≤ parent's quota
    /// * charges propagate up the ancestor chain
    /// * parent cannot be deleted while children live
    #[serde(default)]
    pub parent: Option<TenantId>,
}

impl TenantSpec {
    /// Construct + validate (top-level tenant — no parent).
    ///
    /// # Errors
    ///
    /// Returns [`celcommon::CelError::Invalid`] when `name` fails
    /// segment validation (empty / too long / illegal char).
    pub fn new(name: impl Into<String>, quotas: TenantQuotas) -> CelResult<Self> {
        let name = name.into();
        validate_segment(&name)?;
        Ok(Self {
            name,
            quotas,
            parent: None,
        })
    }

    /// Builder: attach this spec to a parent tenant. Use together
    /// with [`crate::TenantStore::create`] (or the convenience
    /// [`crate::TenantStore::create_subtenant`]) to register a
    /// subtenant under `parent`.
    #[must_use]
    pub fn with_parent(mut self, parent: TenantId) -> Self {
        self.parent = Some(parent);
        self
    }
}

/// A live tenant record. Stored verbatim by the on-disk
/// [`crate::FileTenantStore`] (so the JSON layout IS this struct's
/// serde shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    /// Assigned id (monotonic per-store).
    pub id: TenantId,
    /// Tenant name (canonical, validated).
    pub name: String,
    /// `/tenants/<name>` for convenience in audits / logs.
    pub namespace: String,
    /// Root capability bundle this tenant was provisioned with.
    /// Any user inside the tenant gets at most this set.
    pub root_caps: TenantCaps,
    /// Users that belong to this tenant.
    pub users: Vec<User>,
    /// Quotas applied to this tenant.
    pub quotas: TenantQuotas,
    /// Running usage counters; never persisted out of sync with the
    /// resources they describe.
    pub usage: QuotaUsage,
    /// Parent tenant id when this is a subtenant (W31). `None` for
    /// top-level tenants. Migration-safe: existing JSON files
    /// without this field default to `None`.
    #[serde(default)]
    pub parent: Option<TenantId>,
}

impl Tenant {
    /// Validate a candidate tenant name without constructing a full
    /// [`TenantSpec`].
    ///
    /// # Errors
    ///
    /// Returns [`celcommon::CelError::Invalid`] on any rule violation.
    pub fn validate_name(name: &str) -> CelResult<()> {
        validate_segment(name)
    }
}
