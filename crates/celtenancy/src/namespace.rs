//! Tenant namespace conventions.
//!
//! Every tenant lives under `/tenants/<name>/` in the federated
//! namespace. The layout mirrors the Core Layer's per-node tree but
//! is scoped to one tenant:
//!
//! ```text
//! /tenants/<name>/
//!     \u251c\u2500 vms/        # VmId paths
//!     \u251c\u2500 volumes/    # VolumeId paths
//!     \u251c\u2500 networks/   # NetworkId paths
//!     \u251c\u2500 users/<user>
//!     \u2514\u2500 quotas
//! ```
//!
//! The strings produced here are pure data \u2014 the Core Layer never
//! parses them; they exist for operator-visible paths in `celctl
//! tenant show`, audit logs, and the integration test surface.

use celcommon::{CelError, CelResult};

/// Builder for `/tenants/<name>/\u2026` paths.
#[derive(Debug, Clone)]
pub struct TenantNamespace {
    root: String,
}

impl TenantNamespace {
    /// Construct from a validated tenant name. The name must already
    /// have passed [`crate::Tenant::validate_name`]; we re-check here
    /// because this is also the entry point for ad-hoc operator
    /// tooling.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Invalid`] when the tenant name is empty,
    /// longer than 64 bytes, or contains characters outside
    /// `[A-Za-z0-9_-]`.
    pub fn new(tenant_name: &str) -> CelResult<Self> {
        validate_segment(tenant_name)?;
        Ok(Self {
            root: format!("/tenants/{tenant_name}"),
        })
    }

    /// `/tenants/<name>`.
    #[must_use]
    pub fn root(&self) -> &str {
        &self.root
    }

    /// `/tenants/<name>/vms`.
    #[must_use]
    pub fn vms(&self) -> String {
        format!("{}/vms", self.root)
    }

    /// `/tenants/<name>/volumes`.
    #[must_use]
    pub fn volumes(&self) -> String {
        format!("{}/volumes", self.root)
    }

    /// `/tenants/<name>/networks`.
    #[must_use]
    pub fn networks(&self) -> String {
        format!("{}/networks", self.root)
    }

    /// `/tenants/<name>/users`.
    #[must_use]
    pub fn users(&self) -> String {
        format!("{}/users", self.root)
    }

    /// `/tenants/<name>/quotas`.
    #[must_use]
    pub fn quotas(&self) -> String {
        format!("{}/quotas", self.root)
    }

    /// `/tenants/<name>/users/<user>`.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Invalid`] when `user_name` fails the
    /// segment-validation rules.
    pub fn user_path(&self, user_name: &str) -> CelResult<String> {
        validate_segment(user_name)?;
        Ok(format!("{}/users/{}", self.root, user_name))
    }
}

/// Path-segment validation shared by tenants and users. Restrictive
/// on purpose so namespace paths never need URL-escaping.
pub(crate) fn validate_segment(s: &str) -> CelResult<()> {
    if s.is_empty() {
        return Err(CelError::Invalid("tenancy: name empty"));
    }
    if s.len() > 64 {
        return Err(CelError::Invalid("tenancy: name longer than 64 bytes"));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(CelError::Invalid(
            "tenancy: name must match [A-Za-z0-9_-]+",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_well_formed() {
        let ns = TenantNamespace::new("acme").unwrap();
        assert_eq!(ns.root(), "/tenants/acme");
        assert_eq!(ns.vms(), "/tenants/acme/vms");
        assert_eq!(ns.volumes(), "/tenants/acme/volumes");
        assert_eq!(ns.networks(), "/tenants/acme/networks");
        assert_eq!(ns.users(), "/tenants/acme/users");
        assert_eq!(ns.quotas(), "/tenants/acme/quotas");
        assert_eq!(ns.user_path("alice").unwrap(), "/tenants/acme/users/alice");
    }

    #[test]
    fn rejects_bad_names() {
        assert!(TenantNamespace::new("").is_err());
        assert!(TenantNamespace::new("has spaces").is_err());
        assert!(TenantNamespace::new("with/slash").is_err());
        assert!(TenantNamespace::new(&"x".repeat(65)).is_err());
    }
}
