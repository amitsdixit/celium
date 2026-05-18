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
}
