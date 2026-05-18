//! W31 — Nested tenant end-to-end.
//!
//! Exercises subtenant lifecycle on a real on-disk
//! [`celtenancy::FileTenantStore`]:
//!
//! * Subset-cap + subset-quota validation at creation time.
//! * Charge / release propagation up the ancestor chain.
//! * Parent-deletion guard while a subtenant lives.
//! * On-disk durability of the `parent` field across process
//!   restarts (the property the tenancy CLI relies on).
//!
//! These complement the per-store unit tests in
//! `celtenancy::store::tests::subtenant_*` by driving the full
//! `TenantStore` trait surface through an actual JSON file.

#![forbid(unsafe_code)]

use std::sync::Arc;

use celcommon::CelError;
use celtenancy::{
    FileTenantStore, QuotaCharge, QuotaUsage, TenantCaps, TenantQuotas, TenantSpec,
    TenantStore,
};

fn parent_quotas() -> TenantQuotas {
    TenantQuotas {
        max_vcpus: 8,
        max_memory_mib: 16 * 1024,
        max_storage_bytes: 1024 * 1024 * 1024,
        max_network_mbps: 10_000,
        max_iops: 50_000,
    }
}

fn child_quotas() -> TenantQuotas {
    TenantQuotas {
        max_vcpus: 4,
        max_memory_mib: 8 * 1024,
        max_storage_bytes: 512 * 1024 * 1024,
        max_network_mbps: 1_000,
        max_iops: 10_000,
    }
}

#[test]
fn subtenant_lifecycle_charge_propagates_and_parent_deletion_blocked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store_path = tmp.path().join("tenants.json");
    let store: Arc<dyn TenantStore> =
        Arc::new(FileTenantStore::open(&store_path).unwrap());

    // 1. Create parent + subtenant.
    let parent = store
        .create(
            TenantSpec::new("acme", parent_quotas()).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    let child = store
        .create_subtenant(
            parent.id,
            TenantSpec::new("acme-eng", child_quotas()).unwrap(),
            TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
        )
        .unwrap();
    assert_eq!(child.parent, Some(parent.id));

    // 2. Charge against the subtenant; parent usage must mirror.
    let charge = QuotaCharge {
        vcpus: 2,
        memory_mib: 1024,
        ..Default::default()
    };
    let cu = store.charge(child.id, charge).unwrap();
    assert_eq!(cu.vcpus, 2);
    let p_mid = store.get(parent.id).unwrap();
    assert_eq!(p_mid.usage.vcpus, 2);
    assert_eq!(p_mid.usage.memory_mib, 1024);

    // 3. Parent deletion is refused while a subtenant lives.
    let err = store.delete(parent.id).unwrap_err();
    assert!(matches!(err, CelError::Invalid("tenant has subtenants")));

    // 4. Release the charge; both levels return to zero.
    let cu2 = store.release(child.id, charge).unwrap();
    assert_eq!(cu2, QuotaUsage::default());
    let p_after = store.get(parent.id).unwrap();
    assert_eq!(p_after.usage, QuotaUsage::default());

    // 5. Child then parent both delete cleanly.
    store.delete(child.id).unwrap();
    store.delete(parent.id).unwrap();
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn subtenant_cap_escalation_is_rejected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store_path = tmp.path().join("tenants.json");
    let store: Arc<dyn TenantStore> =
        Arc::new(FileTenantStore::open(&store_path).unwrap());

    // Parent only owns vm.read.
    let parent = store
        .create(
            TenantSpec::new("acme", parent_quotas()).unwrap(),
            TenantCaps::VM_LIFECYCLE_READ,
        )
        .unwrap();
    // Try to mint a child with vm.write — must be denied.
    let err = store
        .create_subtenant(
            parent.id,
            TenantSpec::new("acme-eng", child_quotas()).unwrap(),
            TenantCaps::VM_LIFECYCLE_WRITE,
        )
        .unwrap_err();
    assert!(matches!(err, CelError::CapabilityDenied(_)));
}

#[test]
fn subtenant_quota_dimension_overshoot_is_rejected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store_path = tmp.path().join("tenants.json");
    let store: Arc<dyn TenantStore> =
        Arc::new(FileTenantStore::open(&store_path).unwrap());
    let parent = store
        .create(
            TenantSpec::new("acme", parent_quotas()).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    // 16 vcpus > parent's 8.
    let too_big = TenantQuotas {
        max_vcpus: 16,
        ..parent_quotas()
    };
    let err = store
        .create_subtenant(
            parent.id,
            TenantSpec::new("acme-eng", too_big).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        CelError::Invalid("subtenant quotas exceed parent quotas")
    ));
}

#[test]
fn charge_atomic_when_parent_exhausted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store_path = tmp.path().join("tenants.json");
    let store: Arc<dyn TenantStore> =
        Arc::new(FileTenantStore::open(&store_path).unwrap());

    let parent = store
        .create(
            TenantSpec::new(
                "acme",
                TenantQuotas {
                    max_vcpus: 4,
                    ..parent_quotas()
                },
            )
            .unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    let child = store
        .create_subtenant(
            parent.id,
            TenantSpec::new(
                "acme-eng",
                TenantQuotas {
                    max_vcpus: 4,
                    ..child_quotas()
                },
            )
            .unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();

    // Burn 3 vCPUs at the parent (e.g. parent's own direct VMs).
    store
        .charge(
            parent.id,
            QuotaCharge {
                vcpus: 3,
                ..Default::default()
            },
        )
        .unwrap();

    // Child tries 2 — fits child's quota but blows parent's.
    let err = store
        .charge(
            child.id,
            QuotaCharge {
                vcpus: 2,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, CelError::Exhausted("quota.vcpus")));

    // Atomicity: the failed call must NOT have partially charged
    // the child either.
    let c_after = store.get(child.id).unwrap();
    assert_eq!(c_after.usage.vcpus, 0);
    let p_after = store.get(parent.id).unwrap();
    assert_eq!(p_after.usage.vcpus, 3);
}

#[test]
fn file_store_persists_hierarchy_across_process_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store_path = tmp.path().join("tenants.json");

    let parent_id;
    let child_id;
    // Process 1: create + charge.
    {
        let s: Arc<dyn TenantStore> =
            Arc::new(FileTenantStore::open(&store_path).unwrap());
        let p = s
            .create(
                TenantSpec::new("acme", parent_quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let c = s
            .create_subtenant(
                p.id,
                TenantSpec::new("acme-eng", child_quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        s.charge(
            c.id,
            QuotaCharge {
                vcpus: 1,
                memory_mib: 256,
                ..Default::default()
            },
        )
        .unwrap();
        parent_id = p.id;
        child_id = c.id;
    }

    // Process 2: reopen and verify hierarchy + propagated usage
    // are intact.
    {
        let s: Arc<dyn TenantStore> =
            Arc::new(FileTenantStore::open(&store_path).unwrap());
        let p = s.get(parent_id).unwrap();
        let c = s.get(child_id).unwrap();
        assert_eq!(c.parent, Some(parent_id));
        assert_eq!(p.usage.vcpus, 1);
        assert_eq!(p.usage.memory_mib, 256);
        assert_eq!(c.usage.vcpus, 1);
        let kids = s.children(parent_id).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].id, child_id);
        let ancs = s.ancestors(child_id).unwrap();
        assert_eq!(ancs.len(), 1);
        assert_eq!(ancs[0].id, parent_id);
    }
}
