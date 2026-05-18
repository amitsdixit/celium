//! Celium Tenancy Layer.
//!
//! Sits cleanly on top of the Core Layer (CelHyper / CelMesh / CelVault) and
//! introduces:
//!
//! * **Tenants** \u2014 named, isolated scopes mounted under
//!   `/tenants/<name>/` in the federated namespace. See [`TenantNamespace`].
//! * **Users** \u2014 named principals within a tenant, each carrying a
//!   subset of the tenant's root capabilities. See [`User`] and
//!   [`attenuate`].
//! * **Capabilities** \u2014 a tenancy-scoped bitset ([`TenantCaps`]) that
//!   mirrors the Core Layer's [`celmesh::Capabilities`] one-to-one
//!   and projects into it via [`TenantCaps::to_mesh_capabilities`].
//!   The Core Layer never learns about tenants \u2014 it only sees the
//!   capability set that the tenancy layer hands its
//!   `MemVmHost::with_caps(\u2026)` constructor.
//! * **Quotas** \u2014 per-tenant ceilings on vCPUs, RAM, storage,
//!   network throughput and IOPS, enforced by [`charge_quota`].
//! * **Tenant store** \u2014 [`TenantStore`] trait with an in-memory
//!   [`MemTenantStore`] (tests, demos) and a JSON-on-disk
//!   [`FileTenantStore`] (production).
//!
//! ## Discipline
//!
//! Per `00_GLOBAL_CONVENTIONS.md`, every fallible API returns
//! [`celcommon::CelResult<T>`]. No `unwrap()` / `panic!()` on production
//! paths. `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]
#![warn(missing_docs, rust_2018_idioms, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod caps;
mod namespace;
mod quota;
mod runtime;
mod store;
mod tenant;
mod user;

pub mod audit;
pub mod auth;
pub mod exec;

pub use audit::{AuditAction, AuditEvent, AuditSink, FileAuditSink, MemAuditSink};
pub use auth::{
    hash_password, hash_token, mint_token, now_ms, verify_password, PasswordHashStr, Session,
    SessionToken, TokenHash, DEFAULT_SESSION_TTL_SECS, TOKEN_BYTES, TOKEN_HEX_LEN,
};
pub use caps::{attenuate, TenantCaps};
pub use namespace::TenantNamespace;
pub use quota::{charge_quota, release_quota, QuotaCharge, QuotaUsage, TenantQuotas};
pub use runtime::TenantVmHost;
pub use store::{DeleteReport, FileTenantStore, MemTenantStore, RotateReport, TenantStore};
pub use tenant::{Tenant, TenantId, TenantSpec};
pub use user::{User, UserId};
