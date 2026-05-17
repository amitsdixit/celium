//! W29 — End-to-end smoke for the `tenant exec` dispatcher driving a
//! [`celtenancy::FileTenantStore`] (the same persistence path
//! `celctl tenant exec` uses on disk).

use std::sync::Arc;

use celtenancy::{
    exec::{self, ExecOptions},
    FileTenantStore, TenantCaps, TenantQuotas, TenantSpec, TenantStore,
};
use tempfile::TempDir;

fn quotas(vcpus: u32, mem: u64, storage: u64) -> TenantQuotas {
    TenantQuotas {
        max_vcpus: vcpus,
        max_memory_mib: mem,
        max_storage_bytes: storage,
        max_network_mbps: 1_000,
        max_iops: 10_000,
    }
}

#[tokio::test]
async fn exec_through_file_store_persists_charges_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tenants.json");

    // Bootstrap a tenant + user in one store handle, then drop it.
    {
        let s = FileTenantStore::open(&path).unwrap();
        let t = s
            .create(
                TenantSpec::new("acme", quotas(8, 8192, 16_384)).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let _ = s
            .add_user(t.id, "alice".into(), TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
    }

    // Reopen + exec via the dispatcher. The successful Create
    // must persist its 2-vCPU / 1024-MiB reservation to disk.
    {
        let s: Arc<dyn TenantStore> = Arc::new(FileTenantStore::open(&path).unwrap());
        let audit = exec::exec(
            s.clone(),
            "acme",
            None,
            exec::vm_create_op("web", 2, 1024),
            ExecOptions::default(),
        )
        .await
        .unwrap();
        assert!(audit.ok(), "audit failed: {audit:?}");
        assert_eq!(audit.usage_after.vcpus, 2);
        assert_eq!(audit.usage_after.memory_mib, 1024);
    }

    // Reopen yet again and confirm persistence survived the drop.
    {
        let s = FileTenantStore::open(&path).unwrap();
        let t = s.get_by_name("acme").unwrap();
        assert_eq!(t.usage.vcpus, 2);
        assert_eq!(t.usage.memory_mib, 1024);

        // Alice has only read; dispatcher must surface
        // capability-denied AND leave usage exactly where it was.
        let s_arc: Arc<dyn TenantStore> = Arc::new(s);
        let audit = exec::exec(
            s_arc.clone(),
            "acme",
            Some("alice"),
            exec::vm_create_op("denied", 1, 256),
            ExecOptions::default(),
        )
        .await
        .unwrap();
        assert!(!audit.ok());
        assert!(audit.error.as_deref().unwrap().contains("capability denied"));
        // Refund must have undone the wrapper's pre-charge.
        let t2 = s_arc.get_by_name("acme").unwrap();
        assert_eq!(t2.usage.vcpus, 2);
        assert_eq!(t2.usage.memory_mib, 1024);
    }
}

#[tokio::test]
async fn release_after_create_round_trip_leaves_disk_state_at_baseline() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tenants.json");

    let s_init = FileTenantStore::open(&path).unwrap();
    let _ = s_init
        .create(
            TenantSpec::new("acme", quotas(4, 4096, 8192)).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    drop(s_init);

    let s: Arc<dyn TenantStore> = Arc::new(FileTenantStore::open(&path).unwrap());
    let opts = ExecOptions {
        release_after_create: true,
        node: None,
    };
    let audit = exec::exec(
        s.clone(),
        "acme",
        None,
        exec::volume_create_op("scratch", 4096),
        opts,
    )
    .await
    .unwrap();
    assert!(audit.ok());
    assert_eq!(audit.usage_after.storage_bytes, 0);

    // Re-open and confirm disk state matches.
    let s2 = FileTenantStore::open(&path).unwrap();
    let t = s2.get_by_name("acme").unwrap();
    assert_eq!(t.usage.storage_bytes, 0);
}
