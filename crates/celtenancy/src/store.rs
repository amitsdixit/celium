//! Tenant persistence \u2014 [`TenantStore`] trait, [`MemTenantStore`]
//! (in-memory, for tests + demos) and [`FileTenantStore`]
//! (atomic-rename JSON file, for production).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

use crate::auth::{
    hash_password, hash_token, mint_token, now_ms, verify_password, Session, SessionToken,
    DEFAULT_SESSION_TTL_SECS,
};
use crate::caps::{attenuate, TenantCaps};
use crate::namespace::{validate_segment, TenantNamespace};
use crate::quota::{charge_quota, release_quota, QuotaCharge, QuotaUsage};
use crate::tenant::{Tenant, TenantId, TenantSpec};
use crate::user::{User, UserId};

/// Tenancy persistence trait. Implementors are responsible for
/// atomicity \u2014 the in-memory store is straight-line; the file
/// store uses write-to-tmp + rename.
pub trait TenantStore: Send + Sync {
    /// Create a tenant with the given specification and root caps.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on a bad name.
    /// * [`CelError::Storage`] if the name already exists or the
    ///   underlying persistence layer cannot durably commit.
    fn create(&self, spec: TenantSpec, root_caps: TenantCaps) -> CelResult<Tenant>;

    /// Look up by id.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Invalid`] when no tenant matches.
    fn get(&self, id: TenantId) -> CelResult<Tenant>;

    /// Look up by name.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Invalid`] when no tenant matches.
    fn get_by_name(&self, name: &str) -> CelResult<Tenant>;

    /// Snapshot every tenant.
    ///
    /// # Errors
    ///
    /// Implementations that touch shared state may surface
    /// [`CelError::Storage`] if the snapshot cannot be taken
    /// consistently.
    fn list(&self) -> CelResult<Vec<Tenant>>;

    /// Remove a tenant. Refuses if `usage` is non-default
    /// (`CelError::Invalid("tenant in use")`) so operators don't
    /// accidentally orphan resources.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] when the id is unknown or usage is non-zero.
    /// * [`CelError::Storage`] when the underlying persistence layer fails.
    fn delete(&self, id: TenantId) -> CelResult<()>;

    /// Add a user. `requested_caps` must be a subset of the
    /// tenant's root caps; otherwise
    /// [`CelError::CapabilityDenied`] surfaces.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on a bad user name or unknown tenant.
    /// * [`CelError::CapabilityDenied`] on cap escalation attempts.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn add_user(
        &self,
        tenant: TenantId,
        user_name: String,
        requested_caps: TenantCaps,
    ) -> CelResult<User>;

    /// Remove a user. Idempotent: a missing user is `Ok(())`.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on an unknown tenant.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn remove_user(&self, tenant: TenantId, user: UserId) -> CelResult<()>;

    /// Charge an allocation against the tenant's quotas, returning
    /// the new usage. See [`crate::charge_quota`].
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on an unknown tenant.
    /// * [`CelError::Exhausted`] when the allocation would exceed a ceiling.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn charge(&self, tenant: TenantId, charge: QuotaCharge) -> CelResult<QuotaUsage>;

    /// Release a previously charged allocation. Saturating; never fails on math.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on an unknown tenant.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn release(&self, tenant: TenantId, charge: QuotaCharge) -> CelResult<QuotaUsage>;

    /// Convenience: create a subtenant under `parent` (W31).
    ///
    /// Equivalent to `self.create(spec.with_parent(parent), caps)`.
    /// The store enforces caps ⊆ parent caps and per-dimension
    /// quotas ≤ parent quotas at creation time, then propagates
    /// charge/release calls up the ancestor chain.
    ///
    /// # Errors
    ///
    /// Same as [`Self::create`], plus
    /// [`CelError::CapabilityDenied`] on cap escalation and
    /// [`CelError::Invalid`] when a quota dimension exceeds the
    /// parent ceiling.
    fn create_subtenant(
        &self,
        parent: TenantId,
        spec: TenantSpec,
        root_caps: TenantCaps,
    ) -> CelResult<Tenant> {
        self.create(spec.with_parent(parent), root_caps)
    }

    /// Direct children of `parent` (W31). Default implementation
    /// snapshots [`Self::list`] and filters by `parent` field.
    ///
    /// # Errors
    ///
    /// Surfaces any error from [`Self::list`].
    fn children(&self, parent: TenantId) -> CelResult<Vec<Tenant>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|t| t.parent == Some(parent))
            .collect())
    }

    /// Walk the ancestor chain from `id` toward the root (W31).
    /// Returned vector excludes `id` itself; the first element is
    /// the direct parent.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on an unknown tenant.
    /// * [`CelError::Internal`] if the chain exceeds 64 levels
    ///   (cycle or corruption guard).
    fn ancestors(&self, id: TenantId) -> CelResult<Vec<Tenant>> {
        let mut out = Vec::new();
        let mut cur = self.get(id)?.parent;
        let mut depth = 0u32;
        while let Some(p) = cur {
            if depth > 64 {
                return Err(CelError::Internal("tenant hierarchy too deep"));
            }
            let t = self.get(p)?;
            cur = t.parent;
            out.push(t);
            depth += 1;
        }
        Ok(out)
    }

    // -----------------------------------------------------------
    // W32 — authentication & sessions
    //
    // Default impls return `CelError::Internal` so any third-party
    // `TenantStore` written against W27..W31 keeps compiling but
    // surfaces a clear error when auth is invoked. The two
    // first-party stores (`MemTenantStore`, `FileTenantStore`)
    // override every method below.
    // -----------------------------------------------------------

    /// Set or replace the Argon2id password hash for `user`.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on unknown tenant / user / empty password.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn set_password(
        &self,
        _tenant: TenantId,
        _user: UserId,
        _plain: &str,
    ) -> CelResult<()> {
        Err(CelError::Internal("auth not supported by this store"))
    }

    /// Verify `(tenant_name, user_name, password)` and return the
    /// authenticated user's caps. All failure paths surface the
    /// uniform error `CelError::CapabilityDenied("auth.credentials")`.
    ///
    /// # Errors
    ///
    /// See above.
    fn authenticate(
        &self,
        _tenant_name: &str,
        _user_name: &str,
        _password: &str,
    ) -> CelResult<(TenantId, UserId, TenantCaps)> {
        Err(CelError::Internal("auth not supported by this store"))
    }

    /// Mint a fresh session for `(tenant, user)` with `requested_caps`
    /// **intersected** with the user's caps. `ttl_secs` defaults to
    /// [`crate::auth::DEFAULT_SESSION_TTL_SECS`].
    ///
    /// Returns the **plaintext token** (the caller must persist /
    /// transmit it; the store only keeps its SHA-256 hash) and the
    /// resulting [`Session`] record.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on unknown tenant or user.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn create_session(
        &self,
        _tenant: TenantId,
        _user: UserId,
        _requested_caps: TenantCaps,
        _ttl_secs: Option<u64>,
    ) -> CelResult<(SessionToken, Session)> {
        Err(CelError::Internal("auth not supported by this store"))
    }

    /// Look up a token's session record, rejecting expired or
    /// unknown tokens with the uniform error
    /// `CelError::CapabilityDenied("auth.session")`.
    ///
    /// # Errors
    ///
    /// See above.
    fn validate_token(&self, _token: &SessionToken) -> CelResult<Session> {
        Err(CelError::Internal("auth not supported by this store"))
    }

    /// Revoke a token. Idempotent: revoking an unknown / already-
    /// revoked token is `Ok(())`. The uniform return value
    /// prevents an attacker from probing which tokens were ever
    /// valid.
    ///
    /// # Errors
    ///
    /// Surfaces [`CelError::Storage`] when the persistence layer fails.
    fn revoke_token(&self, _token: &SessionToken) -> CelResult<()> {
        Err(CelError::Internal("auth not supported by this store"))
    }

    /// Drop every session whose `expires_ms` is in the past.
    /// Returns the number purged. Useful from a periodic cleanup
    /// task; not required for correctness because
    /// [`Self::validate_token`] already rejects expired entries.
    ///
    /// # Errors
    ///
    /// Surfaces [`CelError::Storage`] when the persistence layer fails.
    fn purge_expired_sessions(&self) -> CelResult<usize> {
        Err(CelError::Internal("auth not supported by this store"))
    }

    // -----------------------------------------------------------
    // W33 — bulk revocation, cap rotation, recursive delete
    //
    // These are administrative operations on top of the W31/W32
    // foundations. Default impls fall back to `CelError::Internal`
    // so third-party stores keep compiling; the two first-party
    // stores (`MemTenantStore`, `FileTenantStore`) override every
    // method below.
    // -----------------------------------------------------------

    /// Revoke every live session belonging to `(tenant, user)`.
    /// Idempotent; returns the number of sessions actually
    /// dropped. Used internally by `set_password` and
    /// `remove_user`, but also exposed so operators can force-
    /// logout a compromised user.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on an unknown tenant.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn revoke_user_sessions(
        &self,
        _tenant: TenantId,
        _user: UserId,
    ) -> CelResult<usize> {
        Err(CelError::Internal("auth not supported by this store"))
    }

    /// Revoke every live session belonging to `tenant`.
    /// Idempotent. Used internally by `rotate_root_caps` and
    /// `delete_tenant_recursive`; also exposed for "kick everyone
    /// out of tenant X" admin actions.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on an unknown tenant.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn revoke_tenant_sessions(&self, _tenant: TenantId) -> CelResult<usize> {
        Err(CelError::Internal("auth not supported by this store"))
    }

    /// Replace a tenant's `root_caps` with `new_caps` and bring
    /// every dependent state in line:
    ///
    /// * Re-attenuates every user's caps as `u.caps & new_caps`
    ///   (users never gain caps from a rotation; only lose them).
    /// * Revokes every live session for the tenant (the prior
    ///   tokens carried caps the operator just renounced).
    /// * If the tenant has a parent, refuses when `new_caps` is
    ///   not a subset of the parent's `root_caps` — you cannot
    ///   rotate a subtenant up.
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on an unknown tenant.
    /// * [`CelError::CapabilityDenied`] when `new_caps` escapes
    ///   the parent's ceiling.
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn rotate_root_caps(
        &self,
        _tenant: TenantId,
        _new_caps: TenantCaps,
    ) -> CelResult<RotateReport> {
        Err(CelError::Internal("admin not supported by this store"))
    }

    /// Delete `tenant` and every descendant in a single atomic
    /// transaction. The whole subtree must have zero usage — the
    /// store refuses the call if any node in the subtree carries
    /// a non-default [`QuotaUsage`] so operators cannot orphan
    /// live VMs / volumes by accident.
    ///
    /// Walks post-order so children are removed before parents,
    /// matching the order of namespace cleanup. Every revoked
    /// session is counted into the returned [`DeleteReport`].
    ///
    /// # Errors
    ///
    /// * [`CelError::Invalid`] on an unknown tenant or when any
    ///   node in the subtree has non-zero usage (the message
    ///   names which node).
    /// * [`CelError::Storage`] when the persistence layer fails.
    fn delete_tenant_recursive(
        &self,
        _tenant: TenantId,
    ) -> CelResult<DeleteReport> {
        Err(CelError::Internal("admin not supported by this store"))
    }
}

/// Outcome of [`TenantStore::rotate_root_caps`]. Returned for
/// audit + UX — every field is informational.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotateReport {
    /// The tenant whose caps were rotated.
    pub tenant: TenantId,
    /// Tenant name (cached so the caller doesn't need a second lookup).
    pub tenant_name: String,
    /// Caps before the rotation.
    pub old_caps: TenantCaps,
    /// Caps after the rotation.
    pub new_caps: TenantCaps,
    /// Number of users whose caps were narrowed by the rotation.
    pub attenuated_users: usize,
    /// Number of live sessions that were revoked.
    pub revoked_sessions: usize,
}

/// Outcome of [`TenantStore::delete_tenant_recursive`]. Returned
/// for audit + UX.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DeleteReport {
    /// Every tenant that was deleted, in post-order
    /// (children before parents). Each entry is `(id, name)`.
    pub deleted_tenants: Vec<(TenantId, String)>,
    /// Total live sessions revoked across the whole subtree.
    pub revoked_sessions: usize,
    /// Total users dropped across the whole subtree.
    pub dropped_users: usize,
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Serialize, Deserialize)]
struct StoreState {
    next_tenant_id: u64,
    next_user_id: u64,
    tenants: HashMap<u64, Tenant>,
    by_name: HashMap<String, u64>,
    /// W32 — live sessions. Plaintext tokens are never persisted;
    /// each entry carries only the SHA-256 fingerprint. Lookup is
    /// O(n) in the session count, which is fine for the operator
    /// scale we target (tens to low-hundreds of live sessions per
    /// store). `#[serde(default)]` keeps pre-W32 JSON files
    /// reopenable unchanged.
    #[serde(default)]
    sessions: Vec<Session>,
}

impl StoreState {
    fn create(&mut self, spec: TenantSpec, root_caps: TenantCaps) -> CelResult<Tenant> {
        if self.by_name.contains_key(&spec.name) {
            return Err(CelError::Storage(format!(
                "tenant already exists: {}",
                spec.name
            )));
        }
        // W31 — subtenant validation: parent must exist; caps must
        // be a subset of parent's root_caps; each quota dimension
        // must be ≤ parent's corresponding ceiling.
        if let Some(parent_id) = spec.parent {
            let parent = self
                .tenants
                .get(&parent_id.0)
                .ok_or(CelError::Invalid("parent tenant id unknown"))?;
            if !parent.root_caps.contains(root_caps) {
                return Err(CelError::CapabilityDenied(
                    "subtenant caps exceed parent",
                ));
            }
            let q = &spec.quotas;
            let pq = &parent.quotas;
            if q.max_vcpus > pq.max_vcpus
                || q.max_memory_mib > pq.max_memory_mib
                || q.max_storage_bytes > pq.max_storage_bytes
                || q.max_network_mbps > pq.max_network_mbps
                || q.max_iops > pq.max_iops
            {
                return Err(CelError::Invalid(
                    "subtenant quotas exceed parent quotas",
                ));
            }
        }
        let id_raw = self.next_tenant_id.checked_add(1).ok_or(
            CelError::Internal("tenant id overflow"),
        )?;
        self.next_tenant_id = id_raw;
        let ns = TenantNamespace::new(&spec.name)?;
        let tenant = Tenant {
            id: TenantId(id_raw),
            name: spec.name.clone(),
            namespace: ns.root().to_string(),
            root_caps,
            users: Vec::new(),
            quotas: spec.quotas,
            usage: QuotaUsage::default(),
            parent: spec.parent,
        };
        self.by_name.insert(spec.name, id_raw);
        self.tenants.insert(id_raw, tenant.clone());
        Ok(tenant)
    }

    fn delete(&mut self, id: TenantId) -> CelResult<()> {
        let t = self
            .tenants
            .get(&id.0)
            .ok_or(CelError::Invalid("tenant id unknown"))?;
        // W31 — refuse if any live subtenant points at us. Run this
        // check ahead of the usage guard because a parent's usage
        // is propagated from its children, so deleting a parent
        // while a child lives would otherwise surface as the less
        // actionable "tenant in use".
        if self
            .tenants
            .values()
            .any(|child| child.parent == Some(id))
        {
            return Err(CelError::Invalid("tenant has subtenants"));
        }
        if t.usage != QuotaUsage::default() {
            return Err(CelError::Invalid("tenant in use"));
        }
        let name = t.name.clone();
        self.tenants.remove(&id.0);
        self.by_name.remove(&name);
        Ok(())
    }

    fn add_user(
        &mut self,
        tenant: TenantId,
        user_name: String,
        requested_caps: TenantCaps,
    ) -> CelResult<User> {
        validate_segment(&user_name)?;
        let t = self
            .tenants
            .get_mut(&tenant.0)
            .ok_or(CelError::Invalid("tenant id unknown"))?;
        if t.users.iter().any(|u| u.name == user_name) {
            return Err(CelError::Storage(format!(
                "user already exists: {user_name}"
            )));
        }
        let caps = attenuate(t.root_caps, requested_caps)?;
        let id_raw = self
            .next_user_id
            .checked_add(1)
            .ok_or(CelError::Internal("user id overflow"))?;
        self.next_user_id = id_raw;
        let user = User {
            id: UserId(id_raw),
            name: user_name,
            caps,
            password_hash: None,
        };
        t.users.push(user.clone());
        Ok(user)
    }

    fn remove_user(&mut self, tenant: TenantId, user: UserId) -> CelResult<()> {
        let t = self
            .tenants
            .get_mut(&tenant.0)
            .ok_or(CelError::Invalid("tenant id unknown"))?;
        t.users.retain(|u| u.id != user);
        // W33 — kill the removed user's sessions so a still-cached
        // token cannot keep speaking for a principal that no
        // longer exists.
        self.sessions
            .retain(|s| !(s.tenant == tenant && s.user == user));
        Ok(())
    }

    fn charge(&mut self, tenant: TenantId, charge: QuotaCharge) -> CelResult<QuotaUsage> {
        // W31 — charge propagates up the ancestor chain. The whole
        // chain must accept the charge or none of it is applied
        // (we validate in pass 1, mutate in pass 2; the mutex
        // makes this atomic against concurrent callers).
        let chain = self.chain_from(tenant)?;
        for tid in &chain {
            let t = self
                .tenants
                .get(&tid.0)
                .ok_or(CelError::Internal("tenant disappeared mid-charge"))?;
            // Surface ancestor exhaustion with a tag that includes
            // the ancestor name so operators can tell which level
            // ran out.
            charge_quota(t.usage, t.quotas, charge).map_err(|e| match e {
                CelError::Exhausted(tag) if *tid != tenant => {
                    CelError::Exhausted(tag)
                }
                other => other,
            })?;
        }
        let mut new_self = QuotaUsage::default();
        for (i, tid) in chain.iter().enumerate() {
            let t = self
                .tenants
                .get_mut(&tid.0)
                .ok_or(CelError::Internal("tenant disappeared mid-charge"))?;
            let new = charge_quota(t.usage, t.quotas, charge)
                .map_err(|_| CelError::Internal("quota validation race"))?;
            t.usage = new;
            if i == 0 {
                new_self = new;
            }
        }
        Ok(new_self)
    }

    fn release(&mut self, tenant: TenantId, charge: QuotaCharge) -> CelResult<QuotaUsage> {
        // W31 — release propagates the same way (saturating, so
        // never fails on math). A double-release silently floors
        // at zero on every level.
        let chain = self.chain_from(tenant)?;
        let mut new_self = QuotaUsage::default();
        for (i, tid) in chain.iter().enumerate() {
            let t = self
                .tenants
                .get_mut(&tid.0)
                .ok_or(CelError::Internal("tenant disappeared mid-release"))?;
            t.usage = release_quota(t.usage, charge);
            if i == 0 {
                new_self = t.usage;
            }
        }
        Ok(new_self)
    }

    /// Walk `tenant → parent → grandparent → …` returning ids
    /// in that order. Refuses chains deeper than 64 levels to
    /// catch accidental cycles or corruption.
    fn chain_from(&self, tenant: TenantId) -> CelResult<Vec<TenantId>> {
        let mut out = Vec::new();
        let mut cur = Some(tenant);
        let mut depth = 0u32;
        while let Some(tid) = cur {
            if depth > 64 {
                return Err(CelError::Internal("tenant hierarchy too deep"));
            }
            let t = self
                .tenants
                .get(&tid.0)
                .ok_or(CelError::Invalid("tenant id unknown"))?;
            out.push(tid);
            cur = t.parent;
            depth += 1;
        }
        Ok(out)
    }

    // -----------------------------------------------------------
    // W32 — authentication & sessions
    // -----------------------------------------------------------

    fn set_password(
        &mut self,
        tenant: TenantId,
        user: UserId,
        plain: &str,
    ) -> CelResult<()> {
        let hash = hash_password(plain)?;
        let t = self
            .tenants
            .get_mut(&tenant.0)
            .ok_or(CelError::Invalid("tenant id unknown"))?;
        let u = t
            .users
            .iter_mut()
            .find(|u| u.id == user)
            .ok_or(CelError::Invalid("user id unknown"))?;
        u.password_hash = Some(hash);
        // W33 — a password change invalidates every prior session
        // for this user. Without this, a stolen token would
        // outlive the rotation that was supposed to defend
        // against it.
        self.sessions
            .retain(|s| !(s.tenant == tenant && s.user == user));
        Ok(())
    }

    /// Verify `(tenant_name, user_name, password)`. On any failure
    /// — unknown tenant, unknown user, no password set, or hash
    /// mismatch — returns the **same** error
    /// `CelError::CapabilityDenied("auth.credentials")` so the
    /// caller cannot distinguish them via a side channel.
    fn authenticate(
        &self,
        tenant_name: &str,
        user_name: &str,
        password: &str,
    ) -> CelResult<(TenantId, UserId, TenantCaps)> {
        // Uniform-error helper. Constructed per branch (no Clone
        // on CelError) so we don't accidentally pre-allocate.
        let denied = || CelError::CapabilityDenied("auth.credentials");
        let tid = self
            .by_name
            .get(tenant_name)
            .copied()
            .ok_or_else(denied)?;
        let t = self.tenants.get(&tid).ok_or_else(denied)?;
        let u = t
            .users
            .iter()
            .find(|u| u.name == user_name)
            .ok_or_else(denied)?;
        let hash = u.password_hash.as_ref().ok_or_else(denied)?;
        verify_password(password, hash).map_err(|_| denied())?;
        Ok((TenantId(tid), u.id, u.caps))
    }

    fn create_session(
        &mut self,
        tenant: TenantId,
        user: UserId,
        requested_caps: TenantCaps,
        ttl_secs: Option<u64>,
    ) -> CelResult<(SessionToken, Session)> {
        // Resolve user + cache its name for the session record.
        let t = self
            .tenants
            .get(&tenant.0)
            .ok_or(CelError::Invalid("tenant id unknown"))?;
        let u = t
            .users
            .iter()
            .find(|u| u.id == user)
            .ok_or(CelError::Invalid("user id unknown"))?;
        // Clamp requested caps to the user's caps. Unlike
        // `attenuate()` (which is strict and rejects on overflow),
        // session minting silently intersects: the caller gets
        // what they asked for, capped by what the user actually
        // has. This mirrors the semantics of POSIX-style token
        // scope reduction and keeps the CLI friendly (`login`
        // doesn't need to know the user's exact caps).
        let caps = TenantCaps::from_bits_truncate(u.caps.bits() & requested_caps.bits());
        let user_name = u.name.clone();
        let (token, token_hash) = mint_token();
        let created_ms = now_ms();
        let ttl_ms = ttl_secs
            .unwrap_or(DEFAULT_SESSION_TTL_SECS)
            .saturating_mul(1_000);
        let expires_ms = created_ms.saturating_add(ttl_ms);
        let session = Session {
            token_hash,
            tenant,
            user,
            user_name,
            caps,
            created_ms,
            expires_ms,
        };
        // Defensive: drop any prior session that happens to share
        // the same hash (collision probability ~2^-256, but the
        // check is free and keeps the invariant tight).
        self.sessions.retain(|s| s.token_hash != token_hash);
        self.sessions.push(session.clone());
        Ok((token, session))
    }

    fn validate_token(&self, token: &SessionToken) -> CelResult<Session> {
        let needle = hash_token(token);
        let now = now_ms();
        let s = self
            .sessions
            .iter()
            .find(|s| s.token_hash == needle)
            .ok_or(CelError::CapabilityDenied("auth.session"))?;
        if s.is_expired_at(now) {
            return Err(CelError::CapabilityDenied("auth.session"));
        }
        Ok(s.clone())
    }

    fn revoke_token(&mut self, token: &SessionToken) -> CelResult<()> {
        let needle = hash_token(token);
        let before = self.sessions.len();
        self.sessions.retain(|s| s.token_hash != needle);
        if self.sessions.len() == before {
            // Idempotent on already-revoked / never-existed tokens
            // so logout never reveals which is which.
            return Ok(());
        }
        Ok(())
    }

    fn purge_expired_sessions(&mut self) -> usize {
        let now = now_ms();
        let before = self.sessions.len();
        self.sessions.retain(|s| !s.is_expired_at(now));
        before - self.sessions.len()
    }

    // -----------------------------------------------------------
    // W33 — bulk revocation, cap rotation, recursive delete
    // -----------------------------------------------------------

    /// Revoke every session belonging to `(tenant, user)`. The
    /// tenant must exist; the user need not (a remove_user racing
    /// with a session revocation should still succeed).
    fn revoke_user_sessions(
        &mut self,
        tenant: TenantId,
        user: UserId,
    ) -> CelResult<usize> {
        if !self.tenants.contains_key(&tenant.0) {
            return Err(CelError::Invalid("tenant id unknown"));
        }
        let before = self.sessions.len();
        self.sessions
            .retain(|s| !(s.tenant == tenant && s.user == user));
        Ok(before - self.sessions.len())
    }

    /// Revoke every session belonging to `tenant`.
    fn revoke_tenant_sessions(&mut self, tenant: TenantId) -> CelResult<usize> {
        if !self.tenants.contains_key(&tenant.0) {
            return Err(CelError::Invalid("tenant id unknown"));
        }
        let before = self.sessions.len();
        self.sessions.retain(|s| s.tenant != tenant);
        Ok(before - self.sessions.len())
    }

    /// Rotate `tenant`'s root caps to `new_caps`.
    ///
    /// Three-step transaction (the surrounding mutex keeps it
    /// atomic against concurrent callers):
    ///
    /// 1. Validate against parent (if any). Subtenants cannot
    ///    rotate up past their parent's ceiling.
    /// 2. Re-attenuate every user's caps in-place to
    ///    `u.caps & new_caps`. We intersect rather than `attenuate`
    ///    because the caller is explicitly narrowing a ceiling —
    ///    no escalation can happen.
    /// 3. Revoke every live session for this tenant. Prior
    ///    sessions carried caps the operator just dropped, so
    ///    leaving them live would silently re-grant the rescinded
    ///    authority.
    fn rotate_root_caps(
        &mut self,
        tenant: TenantId,
        new_caps: TenantCaps,
    ) -> CelResult<RotateReport> {
        // Step 1 — read parent caps without holding the &mut t
        // borrow, so step 2 can mutate freely.
        let (parent, old_caps, tenant_name) = {
            let t = self
                .tenants
                .get(&tenant.0)
                .ok_or(CelError::Invalid("tenant id unknown"))?;
            (t.parent, t.root_caps, t.name.clone())
        };
        if let Some(parent_id) = parent {
            let p = self
                .tenants
                .get(&parent_id.0)
                .ok_or(CelError::Internal("parent tenant disappeared"))?;
            if !p.root_caps.contains(new_caps) {
                return Err(CelError::CapabilityDenied(
                    "rotated caps exceed parent",
                ));
            }
        }
        // Step 2 — narrow users + flip root caps.
        let mut attenuated_users = 0usize;
        {
            let t = self
                .tenants
                .get_mut(&tenant.0)
                .ok_or(CelError::Internal("tenant disappeared mid-rotate"))?;
            t.root_caps = new_caps;
            for u in &mut t.users {
                let narrowed =
                    TenantCaps::from_bits_truncate(u.caps.bits() & new_caps.bits());
                if narrowed != u.caps {
                    u.caps = narrowed;
                    attenuated_users += 1;
                }
            }
        }
        // Step 3 — revoke sessions.
        let revoked = {
            let before = self.sessions.len();
            self.sessions.retain(|s| s.tenant != tenant);
            before - self.sessions.len()
        };
        Ok(RotateReport {
            tenant,
            tenant_name,
            old_caps,
            new_caps,
            attenuated_users,
            revoked_sessions: revoked,
        })
    }

    /// Recursively delete `tenant` and its subtree.
    ///
    /// Validation runs across the whole subtree before any
    /// mutation: every node must have zero usage. This avoids
    /// the "deleted some children then bailed on a parent in use"
    /// half-state.
    fn delete_tenant_recursive(
        &mut self,
        tenant: TenantId,
    ) -> CelResult<DeleteReport> {
        // Collect the subtree (post-order: children before
        // parents). Capped at 64 levels to catch cycles.
        let mut subtree: Vec<TenantId> = Vec::new();
        fn walk(
            state: &StoreState,
            root: TenantId,
            out: &mut Vec<TenantId>,
            depth: u32,
        ) -> CelResult<()> {
            if depth > 64 {
                return Err(CelError::Internal("tenant hierarchy too deep"));
            }
            // Children first.
            let kids: Vec<TenantId> = state
                .tenants
                .values()
                .filter(|t| t.parent == Some(root))
                .map(|t| t.id)
                .collect();
            for k in kids {
                walk(state, k, out, depth + 1)?;
            }
            out.push(root);
            Ok(())
        }
        if !self.tenants.contains_key(&tenant.0) {
            return Err(CelError::Invalid("tenant id unknown"));
        }
        walk(self, tenant, &mut subtree, 0)?;

        // Validate: no node may have non-zero usage. Surface the
        // first violator by name so operators can fix it.
        for tid in &subtree {
            let t = self
                .tenants
                .get(&tid.0)
                .ok_or(CelError::Internal("subtree node vanished"))?;
            if t.usage != QuotaUsage::default() {
                // Reuse the same static-str sentinel as
                // single-tenant delete so existing test patterns
                // keep matching; named context goes through the
                // tenant name in logs / audit.
                return Err(CelError::Invalid("tenant in use"));
            }
        }

        // Mutation pass — post-order, accumulating the report.
        let mut report = DeleteReport::default();
        for tid in subtree {
            // Revoke this tenant's sessions before removing it
            // (so the count is well-defined even on a
            // tenant-with-no-users-but-stale-sessions path).
            let before = self.sessions.len();
            self.sessions.retain(|s| s.tenant != tid);
            report.revoked_sessions += before - self.sessions.len();

            let t = self
                .tenants
                .remove(&tid.0)
                .ok_or(CelError::Internal("subtree node vanished"))?;
            report.dropped_users += t.users.len();
            self.by_name.remove(&t.name);
            report.deleted_tenants.push((tid, t.name));
        }
        Ok(report)
    }
}

/// In-memory [`TenantStore`]. Used by integration tests and the
/// `celtenancy` binary when no `--store` path is given.
#[derive(Debug, Default)]
pub struct MemTenantStore {
    inner: Mutex<StoreState>,
}

impl MemTenantStore {
    /// Construct an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut StoreState) -> CelResult<R>) -> CelResult<R> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CelError::Storage("tenant store mutex poisoned".to_string()))?;
        f(&mut guard)
    }
}

impl TenantStore for MemTenantStore {
    fn create(&self, spec: TenantSpec, root_caps: TenantCaps) -> CelResult<Tenant> {
        self.with_state(|s| s.create(spec, root_caps))
    }

    fn get(&self, id: TenantId) -> CelResult<Tenant> {
        self.with_state(|s| {
            s.tenants
                .get(&id.0)
                .cloned()
                .ok_or(CelError::Invalid("tenant id unknown"))
        })
    }

    fn get_by_name(&self, name: &str) -> CelResult<Tenant> {
        self.with_state(|s| {
            let id = s
                .by_name
                .get(name)
                .copied()
                .ok_or(CelError::Invalid("tenant name unknown"))?;
            s.tenants
                .get(&id)
                .cloned()
                .ok_or(CelError::Internal("tenant index out of sync"))
        })
    }

    fn list(&self) -> CelResult<Vec<Tenant>> {
        self.with_state(|s| {
            let mut out: Vec<Tenant> = s.tenants.values().cloned().collect();
            out.sort_by_key(|t| t.id);
            Ok(out)
        })
    }

    fn delete(&self, id: TenantId) -> CelResult<()> {
        self.with_state(|s| s.delete(id))
    }

    fn add_user(
        &self,
        tenant: TenantId,
        user_name: String,
        requested_caps: TenantCaps,
    ) -> CelResult<User> {
        self.with_state(|s| s.add_user(tenant, user_name, requested_caps))
    }

    fn remove_user(&self, tenant: TenantId, user: UserId) -> CelResult<()> {
        self.with_state(|s| s.remove_user(tenant, user))
    }

    fn charge(&self, tenant: TenantId, charge: QuotaCharge) -> CelResult<QuotaUsage> {
        self.with_state(|s| s.charge(tenant, charge))
    }

    fn release(&self, tenant: TenantId, charge: QuotaCharge) -> CelResult<QuotaUsage> {
        self.with_state(|s| s.release(tenant, charge))
    }

    fn set_password(
        &self,
        tenant: TenantId,
        user: UserId,
        plain: &str,
    ) -> CelResult<()> {
        self.with_state(|s| s.set_password(tenant, user, plain))
    }

    fn authenticate(
        &self,
        tenant_name: &str,
        user_name: &str,
        password: &str,
    ) -> CelResult<(TenantId, UserId, TenantCaps)> {
        self.with_state(|s| s.authenticate(tenant_name, user_name, password))
    }

    fn create_session(
        &self,
        tenant: TenantId,
        user: UserId,
        requested_caps: TenantCaps,
        ttl_secs: Option<u64>,
    ) -> CelResult<(SessionToken, Session)> {
        self.with_state(|s| s.create_session(tenant, user, requested_caps, ttl_secs))
    }

    fn validate_token(&self, token: &SessionToken) -> CelResult<Session> {
        self.with_state(|s| s.validate_token(token))
    }

    fn revoke_token(&self, token: &SessionToken) -> CelResult<()> {
        self.with_state(|s| s.revoke_token(token))
    }

    fn purge_expired_sessions(&self) -> CelResult<usize> {
        self.with_state(|s| Ok(s.purge_expired_sessions()))
    }

    fn revoke_user_sessions(
        &self,
        tenant: TenantId,
        user: UserId,
    ) -> CelResult<usize> {
        self.with_state(|s| s.revoke_user_sessions(tenant, user))
    }

    fn revoke_tenant_sessions(&self, tenant: TenantId) -> CelResult<usize> {
        self.with_state(|s| s.revoke_tenant_sessions(tenant))
    }

    fn rotate_root_caps(
        &self,
        tenant: TenantId,
        new_caps: TenantCaps,
    ) -> CelResult<RotateReport> {
        self.with_state(|s| s.rotate_root_caps(tenant, new_caps))
    }

    fn delete_tenant_recursive(
        &self,
        tenant: TenantId,
    ) -> CelResult<DeleteReport> {
        self.with_state(|s| s.delete_tenant_recursive(tenant))
    }
}

// ---------------------------------------------------------------------------
// File-backed implementation
// ---------------------------------------------------------------------------

/// JSON-on-disk [`TenantStore`]. Atomicity model: every mutation
/// serialises the whole state, writes it to `<path>.tmp` and renames
/// over `<path>`. Suitable for the operator-scale tenant counts we
/// expect at the Tenancy-Layer boundary (10\u00b3 tenants); the W28
/// Federated Tenancy spec will move this to gossip.
pub struct FileTenantStore {
    path: PathBuf,
    inner: Mutex<StoreState>,
}

impl FileTenantStore {
    /// Open or create the store at `path`. A missing file is
    /// initialised empty.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Storage`] when the file exists but cannot
    /// be read or parsed as JSON.
    pub fn open(path: impl AsRef<Path>) -> CelResult<Self> {
        let path = path.as_ref().to_path_buf();
        let state = if path.exists() {
            let bytes = fs::read(&path).map_err(|e| {
                CelError::Storage(format!("tenant store read {}: {e}", path.display()))
            })?;
            if bytes.is_empty() {
                StoreState::default()
            } else {
                serde_json::from_slice(&bytes).map_err(|e| {
                    CelError::Storage(format!(
                        "tenant store parse {}: {e}",
                        path.display()
                    ))
                })?
            }
        } else {
            StoreState::default()
        };
        Ok(Self {
            path,
            inner: Mutex::new(state),
        })
    }

    fn persist(&self, s: &StoreState) -> CelResult<()> {
        let tmp = self.path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(s)
            .map_err(|e| CelError::Storage(format!("tenant store encode: {e}")))?;
        // Best-effort directory creation; ignored if the parent is "" or already exists.
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| {
                    CelError::Storage(format!(
                        "tenant store mkdir {}: {e}",
                        parent.display()
                    ))
                })?;
            }
        }
        {
            let mut f = fs::File::create(&tmp).map_err(|e| {
                CelError::Storage(format!("tenant store tmp {}: {e}", tmp.display()))
            })?;
            f.write_all(&bytes).map_err(|e| {
                CelError::Storage(format!(
                    "tenant store write {}: {e}",
                    tmp.display()
                ))
            })?;
            f.sync_all().map_err(|e| {
                CelError::Storage(format!("tenant store fsync {}: {e}", tmp.display()))
            })?;
        }
        fs::rename(&tmp, &self.path).map_err(|e| {
            CelError::Storage(format!(
                "tenant store rename {}: {e}",
                self.path.display()
            ))
        })?;
        Ok(())
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut StoreState) -> CelResult<R>) -> CelResult<R> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CelError::Storage("tenant store mutex poisoned".to_string()))?;
        let out = f(&mut guard)?;
        self.persist(&guard)?;
        Ok(out)
    }
}

impl TenantStore for FileTenantStore {
    fn create(&self, spec: TenantSpec, root_caps: TenantCaps) -> CelResult<Tenant> {
        self.with_state(|s| s.create(spec, root_caps))
    }

    fn get(&self, id: TenantId) -> CelResult<Tenant> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| CelError::Storage("tenant store mutex poisoned".to_string()))?;
        guard
            .tenants
            .get(&id.0)
            .cloned()
            .ok_or(CelError::Invalid("tenant id unknown"))
    }

    fn get_by_name(&self, name: &str) -> CelResult<Tenant> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| CelError::Storage("tenant store mutex poisoned".to_string()))?;
        let id = guard
            .by_name
            .get(name)
            .copied()
            .ok_or(CelError::Invalid("tenant name unknown"))?;
        guard
            .tenants
            .get(&id)
            .cloned()
            .ok_or(CelError::Internal("tenant index out of sync"))
    }

    fn list(&self) -> CelResult<Vec<Tenant>> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| CelError::Storage("tenant store mutex poisoned".to_string()))?;
        let mut out: Vec<Tenant> = guard.tenants.values().cloned().collect();
        out.sort_by_key(|t| t.id);
        Ok(out)
    }

    fn delete(&self, id: TenantId) -> CelResult<()> {
        self.with_state(|s| s.delete(id))
    }

    fn add_user(
        &self,
        tenant: TenantId,
        user_name: String,
        requested_caps: TenantCaps,
    ) -> CelResult<User> {
        self.with_state(|s| s.add_user(tenant, user_name, requested_caps))
    }

    fn remove_user(&self, tenant: TenantId, user: UserId) -> CelResult<()> {
        self.with_state(|s| s.remove_user(tenant, user))
    }

    fn charge(&self, tenant: TenantId, charge: QuotaCharge) -> CelResult<QuotaUsage> {
        self.with_state(|s| s.charge(tenant, charge))
    }

    fn release(&self, tenant: TenantId, charge: QuotaCharge) -> CelResult<QuotaUsage> {
        self.with_state(|s| s.release(tenant, charge))
    }

    fn set_password(
        &self,
        tenant: TenantId,
        user: UserId,
        plain: &str,
    ) -> CelResult<()> {
        self.with_state(|s| s.set_password(tenant, user, plain))
    }

    fn authenticate(
        &self,
        tenant_name: &str,
        user_name: &str,
        password: &str,
    ) -> CelResult<(TenantId, UserId, TenantCaps)> {
        self.with_state(|s| s.authenticate(tenant_name, user_name, password))
    }

    fn create_session(
        &self,
        tenant: TenantId,
        user: UserId,
        requested_caps: TenantCaps,
        ttl_secs: Option<u64>,
    ) -> CelResult<(SessionToken, Session)> {
        self.with_state(|s| s.create_session(tenant, user, requested_caps, ttl_secs))
    }

    fn validate_token(&self, token: &SessionToken) -> CelResult<Session> {
        self.with_state(|s| s.validate_token(token))
    }

    fn revoke_token(&self, token: &SessionToken) -> CelResult<()> {
        self.with_state(|s| s.revoke_token(token))
    }

    fn purge_expired_sessions(&self) -> CelResult<usize> {
        self.with_state(|s| Ok(s.purge_expired_sessions()))
    }

    fn revoke_user_sessions(
        &self,
        tenant: TenantId,
        user: UserId,
    ) -> CelResult<usize> {
        self.with_state(|s| s.revoke_user_sessions(tenant, user))
    }

    fn revoke_tenant_sessions(&self, tenant: TenantId) -> CelResult<usize> {
        self.with_state(|s| s.revoke_tenant_sessions(tenant))
    }

    fn rotate_root_caps(
        &self,
        tenant: TenantId,
        new_caps: TenantCaps,
    ) -> CelResult<RotateReport> {
        self.with_state(|s| s.rotate_root_caps(tenant, new_caps))
    }

    fn delete_tenant_recursive(
        &self,
        tenant: TenantId,
    ) -> CelResult<DeleteReport> {
        self.with_state(|s| s.delete_tenant_recursive(tenant))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::TenantQuotas;

    fn quotas() -> TenantQuotas {
        TenantQuotas {
            max_vcpus: 8,
            max_memory_mib: 8 * 1024,
            max_storage_bytes: 100 * 1024 * 1024,
            max_network_mbps: 1_000,
            max_iops: 10_000,
        }
    }

    #[test]
    fn mem_store_create_list_delete() {
        let s = MemTenantStore::new();
        let t = s
            .create(
                TenantSpec::new("acme", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        assert_eq!(t.namespace, "/tenants/acme");
        let listed = s.list().unwrap();
        assert_eq!(listed.len(), 1);
        s.delete(t.id).unwrap();
        assert!(s.get(t.id).is_err());
    }

    #[test]
    fn mem_store_duplicate_name_errors() {
        let s = MemTenantStore::new();
        s.create(
            TenantSpec::new("acme", quotas()).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
        let err = s
            .create(
                TenantSpec::new("acme", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap_err();
        assert!(matches!(err, CelError::Storage(_)));
    }

    #[test]
    fn user_caps_attenuate() {
        let s = MemTenantStore::new();
        let t = s
            .create(
                TenantSpec::new("acme", quotas()).unwrap(),
                TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
            )
            .unwrap();
        // Subset OK.
        let u = s
            .add_user(t.id, "alice".to_string(), TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        assert_eq!(u.caps, TenantCaps::VM_LIFECYCLE_READ);
        // Escalation rejected.
        let err = s
            .add_user(t.id, "bob".to_string(), TenantCaps::VOLUME_WRITE)
            .unwrap_err();
        assert!(matches!(err, CelError::CapabilityDenied(_)));
    }

    #[test]
    fn charge_and_release_round_trip() {
        let s = MemTenantStore::new();
        let t = s
            .create(
                TenantSpec::new("acme", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let charge = QuotaCharge {
            vcpus: 2,
            memory_mib: 1024,
            ..Default::default()
        };
        let u1 = s.charge(t.id, charge).unwrap();
        assert_eq!(u1.vcpus, 2);
        // Cannot delete with usage.
        assert!(s.delete(t.id).is_err());
        let u2 = s.release(t.id, charge).unwrap();
        assert_eq!(u2, QuotaUsage::default());
        // Now delete works.
        s.delete(t.id).unwrap();
    }

    #[test]
    fn file_store_round_trip_through_disk() {
        let dir = tempdir();
        let path = dir.join("tenants.json");
        let s = FileTenantStore::open(&path).unwrap();
        let t = s
            .create(
                TenantSpec::new("acme", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        s.add_user(t.id, "alice".to_string(), TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        drop(s);

        // Reopen and verify durability.
        let s2 = FileTenantStore::open(&path).unwrap();
        let t2 = s2.get_by_name("acme").unwrap();
        assert_eq!(t2.users.len(), 1);
        assert_eq!(t2.users[0].name, "alice");
    }

    fn tempdir() -> PathBuf {
        // Best-effort scratch path inside the workspace target dir
        // so we don't depend on a tempfile crate. A per-call atomic
        // counter keeps parallel tests in the same process from
        // stomping on each other.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "celtenancy-test-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("create temp dir for test");
        base
    }

    // -----------------------------------------------------------
    // W31 — nested tenants
    // -----------------------------------------------------------

    fn small_quotas(vcpus: u32) -> TenantQuotas {
        TenantQuotas {
            max_vcpus: vcpus,
            max_memory_mib: u64::from(vcpus) * 1024,
            max_storage_bytes: u64::from(vcpus) * 1024 * 1024,
            max_network_mbps: vcpus * 100,
            max_iops: vcpus * 1000,
        }
    }

    #[test]
    fn subtenant_inherits_parent_field() {
        let s = MemTenantStore::new();
        let p = s
            .create(TenantSpec::new("p", small_quotas(8)).unwrap(), TenantCaps::ALL)
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", small_quotas(4)).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        assert_eq!(c.parent, Some(p.id));
        // Top-level tenant has no parent.
        assert_eq!(p.parent, None);
        let kids = s.children(p.id).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].id, c.id);
        let ancs = s.ancestors(c.id).unwrap();
        assert_eq!(ancs.len(), 1);
        assert_eq!(ancs[0].id, p.id);
    }

    #[test]
    fn subtenant_caps_must_be_subset_of_parent() {
        let s = MemTenantStore::new();
        let p = s
            .create(
                TenantSpec::new("p", small_quotas(8)).unwrap(),
                TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
            )
            .unwrap();
        // Escalation: parent has no VOLUME_WRITE.
        let err = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", small_quotas(2)).unwrap(),
                TenantCaps::VOLUME_WRITE,
            )
            .unwrap_err();
        assert!(matches!(err, CelError::CapabilityDenied(_)));
    }

    #[test]
    fn subtenant_quotas_cannot_exceed_parent() {
        let s = MemTenantStore::new();
        let p = s
            .create(TenantSpec::new("p", small_quotas(4)).unwrap(), TenantCaps::ALL)
            .unwrap();
        let err = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", small_quotas(8)).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap_err();
        assert!(matches!(err, CelError::Invalid("subtenant quotas exceed parent quotas")));
    }

    #[test]
    fn charge_propagates_to_ancestors() {
        let s = MemTenantStore::new();
        let p = s
            .create(TenantSpec::new("p", small_quotas(8)).unwrap(), TenantCaps::ALL)
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", small_quotas(4)).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let charge = QuotaCharge {
            vcpus: 2,
            ..Default::default()
        };
        let cu = s.charge(c.id, charge).unwrap();
        assert_eq!(cu.vcpus, 2);
        // Parent usage reflects the child's charge.
        let p_after = s.get(p.id).unwrap();
        assert_eq!(p_after.usage.vcpus, 2);
    }

    #[test]
    fn charge_fails_when_ancestor_exhausted() {
        let s = MemTenantStore::new();
        // Parent only has 4 vCPUs even though it owns a child
        // with a 4-vCPU child quota.
        let p = s
            .create(TenantSpec::new("p", small_quotas(4)).unwrap(), TenantCaps::ALL)
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", small_quotas(4)).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        // Burn 3 vCPUs against the parent directly.
        s.charge(
            p.id,
            QuotaCharge {
                vcpus: 3,
                ..Default::default()
            },
        )
        .unwrap();
        // Child tries to take 2 — fits child's own quota, but
        // would push parent to 5/4. Must be rejected, and no
        // partial state must land on the child.
        let err = s
            .charge(
                c.id,
                QuotaCharge {
                    vcpus: 2,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, CelError::Exhausted("quota.vcpus")));
        let c_after = s.get(c.id).unwrap();
        assert_eq!(c_after.usage.vcpus, 0);
        let p_after = s.get(p.id).unwrap();
        assert_eq!(p_after.usage.vcpus, 3);
    }

    #[test]
    fn release_propagates_to_ancestors() {
        let s = MemTenantStore::new();
        let p = s
            .create(TenantSpec::new("p", small_quotas(8)).unwrap(), TenantCaps::ALL)
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", small_quotas(4)).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let charge = QuotaCharge {
            vcpus: 2,
            ..Default::default()
        };
        s.charge(c.id, charge).unwrap();
        let cu = s.release(c.id, charge).unwrap();
        assert_eq!(cu, QuotaUsage::default());
        let p_after = s.get(p.id).unwrap();
        assert_eq!(p_after.usage, QuotaUsage::default());
    }

    #[test]
    fn cannot_delete_parent_with_live_subtenant() {
        let s = MemTenantStore::new();
        let p = s
            .create(TenantSpec::new("p", small_quotas(8)).unwrap(), TenantCaps::ALL)
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", small_quotas(2)).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let err = s.delete(p.id).unwrap_err();
        assert!(matches!(err, CelError::Invalid("tenant has subtenants")));
        // Deleting child first unblocks parent.
        s.delete(c.id).unwrap();
        s.delete(p.id).unwrap();
    }

    #[test]
    fn file_store_persists_parent_field_across_reopen() {
        let dir = tempdir();
        let path = dir.join("tenants.json");
        let s = FileTenantStore::open(&path).unwrap();
        let p = s
            .create(TenantSpec::new("p", small_quotas(8)).unwrap(), TenantCaps::ALL)
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", small_quotas(2)).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        s.charge(
            c.id,
            QuotaCharge {
                vcpus: 1,
                ..Default::default()
            },
        )
        .unwrap();
        drop(s);

        let s2 = FileTenantStore::open(&path).unwrap();
        let c2 = s2.get_by_name("c").unwrap();
        assert_eq!(c2.parent, Some(p.id));
        let p2 = s2.get_by_name("p").unwrap();
        assert_eq!(p2.usage.vcpus, 1);
        assert_eq!(s2.children(p2.id).unwrap().len(), 1);
    }

    // -----------------------------------------------------------
    // W32 — authentication & sessions
    // -----------------------------------------------------------

    /// Standard fixture: tenant "acme" with user "alice" having
    /// VM read+write caps.
    fn auth_fixture() -> (MemTenantStore, TenantId, UserId) {
        let s = MemTenantStore::new();
        let t = s
            .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
            .unwrap();
        let u = s
            .add_user(
                t.id,
                "alice".into(),
                TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
            )
            .unwrap();
        (s, t.id, u.id)
    }

    #[test]
    fn set_password_then_authenticate_succeeds() {
        let (s, tid, uid) = auth_fixture();
        s.set_password(tid, uid, "correct horse battery").unwrap();
        let (got_tid, got_uid, caps) =
            s.authenticate("acme", "alice", "correct horse battery").unwrap();
        assert_eq!(got_tid, tid);
        assert_eq!(got_uid, uid);
        assert!(caps.contains(TenantCaps::VM_LIFECYCLE_READ));
    }

    fn assert_uniform_credentials_denied(err: CelError) {
        match err {
            CelError::CapabilityDenied("auth.credentials") => {}
            other => panic!("expected auth.credentials denied, got {other:?}"),
        }
    }

    #[test]
    fn authenticate_wrong_password_yields_uniform_error() {
        let (s, tid, uid) = auth_fixture();
        s.set_password(tid, uid, "correct horse battery").unwrap();
        let err = s.authenticate("acme", "alice", "WRONG").unwrap_err();
        assert_uniform_credentials_denied(err);
    }

    #[test]
    fn authenticate_unknown_user_yields_uniform_error() {
        let (s, tid, uid) = auth_fixture();
        s.set_password(tid, uid, "pw").unwrap();
        let err = s.authenticate("acme", "ghost", "pw").unwrap_err();
        assert_uniform_credentials_denied(err);
    }

    #[test]
    fn authenticate_unknown_tenant_yields_uniform_error() {
        let (s, _tid, _uid) = auth_fixture();
        let err = s.authenticate("nope", "alice", "pw").unwrap_err();
        assert_uniform_credentials_denied(err);
    }

    #[test]
    fn authenticate_no_password_set_yields_uniform_error() {
        let (s, _tid, _uid) = auth_fixture();
        // alice has no password_hash at all.
        let err = s.authenticate("acme", "alice", "pw").unwrap_err();
        assert_uniform_credentials_denied(err);
    }

    #[test]
    fn create_session_attenuates_caps_through_user() {
        let (s, tid, uid) = auth_fixture();
        s.set_password(tid, uid, "pw").unwrap();
        // alice only has VM caps; ask for ALL — must come back
        // attenuated to her actual caps.
        let (token, session) = s
            .create_session(tid, uid, TenantCaps::ALL, Some(60))
            .unwrap();
        assert_eq!(session.tenant, tid);
        assert_eq!(session.user, uid);
        assert!(session.caps.contains(TenantCaps::VM_LIFECYCLE_READ));
        assert!(!session.caps.contains(TenantCaps::VOLUME_WRITE));
        // token is 64 hex chars.
        assert_eq!(token.as_str().len(), 64);
    }

    #[test]
    fn validate_token_round_trip() {
        let (s, tid, uid) = auth_fixture();
        s.set_password(tid, uid, "pw").unwrap();
        let (token, _) = s
            .create_session(tid, uid, TenantCaps::ALL, Some(60))
            .unwrap();
        let got = s.validate_token(&token).unwrap();
        assert_eq!(got.tenant, tid);
        assert_eq!(got.user, uid);
        assert_eq!(got.user_name, "alice");
    }

    #[test]
    fn validate_token_rejects_expired() {
        let (s, tid, uid) = auth_fixture();
        s.set_password(tid, uid, "pw").unwrap();
        // ttl = 0 ⇒ expires_ms == created_ms ⇒ is_expired_at(now)
        // is true on the very next call.
        let (token, _) = s
            .create_session(tid, uid, TenantCaps::ALL, Some(0))
            .unwrap();
        let err = s.validate_token(&token).unwrap_err();
        match err {
            CelError::CapabilityDenied("auth.session") => {}
            other => panic!("expected auth.session denied, got {other:?}"),
        }
    }

    #[test]
    fn revoke_token_is_idempotent() {
        let (s, tid, uid) = auth_fixture();
        s.set_password(tid, uid, "pw").unwrap();
        let (token, _) = s
            .create_session(tid, uid, TenantCaps::ALL, Some(60))
            .unwrap();
        s.revoke_token(&token).unwrap();
        // Second revoke returns Ok(()).
        s.revoke_token(&token).unwrap();
        // And the token is no longer usable.
        let err = s.validate_token(&token).unwrap_err();
        assert!(matches!(err, CelError::CapabilityDenied("auth.session")));
    }

    #[test]
    fn purge_expired_sessions_counts() {
        let (s, tid, uid) = auth_fixture();
        s.set_password(tid, uid, "pw").unwrap();
        let _ = s
            .create_session(tid, uid, TenantCaps::ALL, Some(0))
            .unwrap();
        let _ = s
            .create_session(tid, uid, TenantCaps::ALL, Some(60))
            .unwrap();
        let purged = s.purge_expired_sessions().unwrap();
        assert_eq!(purged, 1);
    }

    #[test]
    fn file_store_persists_sessions_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenants.json");
        let token_str = {
            let s = FileTenantStore::open(&path).unwrap();
            let t = s
                .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
                .unwrap();
            let u = s
                .add_user(t.id, "alice".into(), TenantCaps::VM_LIFECYCLE_READ)
                .unwrap();
            s.set_password(t.id, u.id, "pw").unwrap();
            let (token, _) = s
                .create_session(t.id, u.id, TenantCaps::ALL, Some(3600))
                .unwrap();
            token.as_str().to_string()
        };
        // Reopen the store and check the token still validates.
        let s2 = FileTenantStore::open(&path).unwrap();
        let token = SessionToken::from_hex(&token_str).unwrap();
        let session = s2.validate_token(&token).unwrap();
        assert_eq!(session.user_name, "alice");
    }

    #[test]
    fn password_hash_never_persisted_as_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenants.json");
        let s = FileTenantStore::open(&path).unwrap();
        let t = s
            .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
            .unwrap();
        let u = s
            .add_user(t.id, "alice".into(), TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        let secret = "supersecret-marker-XYZ";
        s.set_password(t.id, u.id, secret).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(secret),
            "plaintext password leaked into tenants.json"
        );
    }

    // -----------------------------------------------------------
    // W33 — bulk revocation, cap rotation, recursive delete
    // -----------------------------------------------------------

    /// Shared fixture: tenant "acme" with users "alice" + "bob",
    /// both with a password set. Returns store + ids.
    #[allow(clippy::type_complexity)]
    fn w33_fixture() -> (MemTenantStore, TenantId, UserId, UserId) {
        let s = MemTenantStore::new();
        let t = s
            .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
            .unwrap();
        let a = s
            .add_user(t.id, "alice".into(), TenantCaps::ALL)
            .unwrap();
        let b = s
            .add_user(t.id, "bob".into(), TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        s.set_password(t.id, a.id, "alice-pw").unwrap();
        s.set_password(t.id, b.id, "bob-pw").unwrap();
        (s, t.id, a.id, b.id)
    }

    #[test]
    fn revoke_user_sessions_drops_only_that_users_tokens() {
        let (s, tid, aid, bid) = w33_fixture();
        let (alice_token, _) = s.create_session(tid, aid, TenantCaps::ALL, None).unwrap();
        let (_, _) = s.create_session(tid, aid, TenantCaps::ALL, None).unwrap();
        let (bob_token, _) = s
            .create_session(tid, bid, TenantCaps::VM_LIFECYCLE_READ, None)
            .unwrap();
        let n = s.revoke_user_sessions(tid, aid).unwrap();
        assert_eq!(n, 2);
        // Alice's tokens dead.
        assert!(s.validate_token(&alice_token).is_err());
        // Bob's token still alive.
        assert!(s.validate_token(&bob_token).is_ok());
        // Idempotent.
        assert_eq!(s.revoke_user_sessions(tid, aid).unwrap(), 0);
    }

    #[test]
    fn revoke_tenant_sessions_drops_every_token() {
        let (s, tid, aid, bid) = w33_fixture();
        let (a_tok, _) = s.create_session(tid, aid, TenantCaps::ALL, None).unwrap();
        let (b_tok, _) = s
            .create_session(tid, bid, TenantCaps::VM_LIFECYCLE_READ, None)
            .unwrap();
        let n = s.revoke_tenant_sessions(tid).unwrap();
        assert_eq!(n, 2);
        assert!(s.validate_token(&a_tok).is_err());
        assert!(s.validate_token(&b_tok).is_err());
    }

    #[test]
    fn set_password_revokes_only_that_users_sessions() {
        let (s, tid, aid, bid) = w33_fixture();
        let (a_tok, _) = s.create_session(tid, aid, TenantCaps::ALL, None).unwrap();
        let (b_tok, _) = s
            .create_session(tid, bid, TenantCaps::VM_LIFECYCLE_READ, None)
            .unwrap();
        s.set_password(tid, aid, "alice-pw-v2").unwrap();
        // Alice's old session is gone.
        assert!(s.validate_token(&a_tok).is_err());
        // Bob untouched.
        assert!(s.validate_token(&b_tok).is_ok());
    }

    #[test]
    fn remove_user_revokes_their_sessions() {
        let (s, tid, aid, _bid) = w33_fixture();
        let (a_tok, _) = s.create_session(tid, aid, TenantCaps::ALL, None).unwrap();
        s.remove_user(tid, aid).unwrap();
        assert!(s.validate_token(&a_tok).is_err());
    }

    #[test]
    fn rotate_root_caps_narrows_users_and_kills_sessions() {
        let (s, tid, aid, bid) = w33_fixture();
        let (a_tok, _) = s.create_session(tid, aid, TenantCaps::ALL, None).unwrap();
        let (b_tok, _) = s
            .create_session(tid, bid, TenantCaps::VM_LIFECYCLE_READ, None)
            .unwrap();
        // Narrow tenant to read-only VM caps.
        let report = s
            .rotate_root_caps(tid, TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        assert_eq!(report.old_caps, TenantCaps::ALL);
        assert_eq!(report.new_caps, TenantCaps::VM_LIFECYCLE_READ);
        // Alice had ALL caps → narrowed. Bob had READ → unchanged.
        assert_eq!(report.attenuated_users, 1);
        assert_eq!(report.revoked_sessions, 2);
        // Every session for the tenant is dead.
        assert!(s.validate_token(&a_tok).is_err());
        assert!(s.validate_token(&b_tok).is_err());
        // User caps are narrowed in the persisted record.
        let alice = s
            .get(tid)
            .unwrap()
            .users
            .into_iter()
            .find(|u| u.id == aid)
            .unwrap();
        assert_eq!(alice.caps, TenantCaps::VM_LIFECYCLE_READ);
    }

    #[test]
    fn rotate_root_caps_refuses_to_exceed_parent() {
        let s = MemTenantStore::new();
        let p = s
            .create(
                TenantSpec::new("p", quotas()).unwrap(),
                TenantCaps::VM_LIFECYCLE_READ,
            )
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", quotas()).unwrap(),
                TenantCaps::VM_LIFECYCLE_READ,
            )
            .unwrap();
        // Try to give child more than parent has.
        let err = s
            .rotate_root_caps(c.id, TenantCaps::ALL)
            .unwrap_err();
        assert!(matches!(err, CelError::CapabilityDenied(_)));
        // Original caps untouched.
        assert_eq!(
            s.get(c.id).unwrap().root_caps,
            TenantCaps::VM_LIFECYCLE_READ
        );
    }

    #[test]
    fn delete_recursive_walks_subtree_post_order() {
        let s = MemTenantStore::new();
        let root = s
            .create(TenantSpec::new("root", quotas()).unwrap(), TenantCaps::ALL)
            .unwrap();
        let mid = s
            .create_subtenant(
                root.id,
                TenantSpec::new("mid", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let leaf = s
            .create_subtenant(
                mid.id,
                TenantSpec::new("leaf", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let u = s
            .add_user(leaf.id, "z".into(), TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        s.set_password(leaf.id, u.id, "pw").unwrap();
        let (_tok, _) = s
            .create_session(leaf.id, u.id, TenantCaps::VM_LIFECYCLE_READ, None)
            .unwrap();
        let report = s.delete_tenant_recursive(root.id).unwrap();
        // Post-order: leaf, mid, root.
        let names: Vec<&str> = report
            .deleted_tenants
            .iter()
            .map(|(_, n)| n.as_str())
            .collect();
        assert_eq!(names, vec!["leaf", "mid", "root"]);
        assert_eq!(report.revoked_sessions, 1);
        assert_eq!(report.dropped_users, 1);
        // Store is empty.
        assert_eq!(s.list().unwrap().len(), 0);
    }

    #[test]
    fn delete_recursive_refuses_if_any_node_has_usage() {
        let s = MemTenantStore::new();
        let p = s
            .create(TenantSpec::new("p", quotas()).unwrap(), TenantCaps::ALL)
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("c", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        // Burn 1 vcpu against the leaf — propagates to parent.
        s.charge(
            c.id,
            QuotaCharge {
                vcpus: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let err = s.delete_tenant_recursive(p.id).unwrap_err();
        assert!(matches!(err, CelError::Invalid("tenant in use")));
        // Both tenants still present.
        assert_eq!(s.list().unwrap().len(), 2);
    }

    #[test]
    fn file_store_persists_w33_state_across_reopen() {
        let dir = tempdir();
        let path = dir.join("tenants.json");
        let s = FileTenantStore::open(&path).unwrap();
        let t = s
            .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
            .unwrap();
        let u = s.add_user(t.id, "alice".into(), TenantCaps::ALL).unwrap();
        s.set_password(t.id, u.id, "pw").unwrap();
        let (_tok, _) = s.create_session(t.id, u.id, TenantCaps::ALL, None).unwrap();
        s.rotate_root_caps(t.id, TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        drop(s);

        let s2 = FileTenantStore::open(&path).unwrap();
        let t2 = s2.get_by_name("acme").unwrap();
        assert_eq!(t2.root_caps, TenantCaps::VM_LIFECYCLE_READ);
        assert_eq!(t2.users[0].caps, TenantCaps::VM_LIFECYCLE_READ);
        // Sessions revoked → purge sees nothing left.
        assert_eq!(s2.purge_expired_sessions().unwrap(), 0);
    }
}
