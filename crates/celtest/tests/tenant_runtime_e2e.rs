//! W28 — Multi-tenant isolation via [`celtenancy::TenantVmHost`].
//!
//! These tests prove that two tenants sharing one Core-Layer host
//! infrastructure remain isolated: cap denials are per-tenant,
//! quota exhaustion in one tenant doesn't affect the other, and
//! resource accounting refunds correctly across success / failure
//! paths.

use std::sync::Arc;

use celcommon::CelError;
use celmesh::{
    Capabilities, MemNetworkStore, MemVmHost, MemVolumeStore, NodeId, RestartPolicy, VmHost,
    VmOp, VmOpReply,
};
use celtenancy::{
    MemTenantStore, QuotaCharge, TenantCaps, TenantId, TenantQuotas, TenantSpec, TenantStore,
    TenantVmHost,
};

fn quotas(max_vcpus: u32, max_memory_mib: u64, max_storage_bytes: u64) -> TenantQuotas {
    TenantQuotas {
        max_vcpus,
        max_memory_mib,
        max_storage_bytes,
        max_network_mbps: 1_000,
        max_iops: 10_000,
    }
}

/// Build a tenant-scoped host bound to `tid` whose Core-Layer caps
/// are exactly the tenant's projected root caps.
fn host_for(
    store: Arc<MemTenantStore>,
    tid: TenantId,
    caps: Capabilities,
) -> TenantVmHost {
    let inner = Arc::new(
        MemVmHost::with_stores(
            Arc::new(MemVolumeStore::new()),
            Arc::new(MemNetworkStore::new()),
        )
        .with_caps(caps),
    );
    TenantVmHost::new(tid, store, inner)
}

#[tokio::test]
async fn two_tenants_share_host_with_independent_quota_books() {
    let store = Arc::new(MemTenantStore::new());

    let a = store
        .create(
            TenantSpec::new("alpha", quotas(4, 4096, 16 * 1024)).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    let b = store
        .create(
            TenantSpec::new("bravo", quotas(2, 2048, 8 * 1024)).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();

    let ha = host_for(store.clone(), a.id, Capabilities::ALL);
    let hb = host_for(store.clone(), b.id, Capabilities::ALL);
    let node = NodeId::from("shared-node");
    let _ = ha.snapshot(&node).await;
    let _ = hb.snapshot(&node).await;

    // Tenant alpha consumes 2 vCPU.
    ha.handle(VmOp::Create {
        label: "a1".into(),
        restart_policy: RestartPolicy::Never,
        image_path: None,
        cpu_count: Some(2),
        memory_mib: Some(512),
        boot_blob_crc32c: None,
    })
    .await
    .unwrap();
    // Tenant bravo consumes 1 vCPU.
    hb.handle(VmOp::Create {
        label: "b1".into(),
        restart_policy: RestartPolicy::Never,
        image_path: None,
        cpu_count: Some(1),
        memory_mib: Some(256),
        boot_blob_crc32c: None,
    })
    .await
    .unwrap();

    let ta = store.get(a.id).unwrap();
    let tb = store.get(b.id).unwrap();
    assert_eq!(ta.usage.vcpus, 2);
    assert_eq!(ta.usage.memory_mib, 512);
    assert_eq!(tb.usage.vcpus, 1);
    assert_eq!(tb.usage.memory_mib, 256);
}

#[tokio::test]
async fn tenant_quota_exhaustion_does_not_spill_to_other_tenant() {
    let store = Arc::new(MemTenantStore::new());

    let a = store
        .create(
            TenantSpec::new("alpha", quotas(2, 1024, 4096)).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    let b = store
        .create(
            TenantSpec::new("bravo", quotas(4, 4096, 8192)).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();

    let ha = host_for(store.clone(), a.id, Capabilities::ALL);
    let hb = host_for(store.clone(), b.id, Capabilities::ALL);
    let node = NodeId::from("shared-node");
    let _ = ha.snapshot(&node).await;
    let _ = hb.snapshot(&node).await;

    // Exhaust alpha entirely.
    ha.handle(VmOp::Create {
        label: "a1".into(),
        restart_policy: RestartPolicy::Never,
        image_path: None,
        cpu_count: Some(2),
        memory_mib: Some(1024),
        boot_blob_crc32c: None,
    })
    .await
    .unwrap();

    // Another op against alpha trips quota.
    let err = ha
        .handle(VmOp::Create {
            label: "a2".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: Some(1),
            memory_mib: None,
            boot_blob_crc32c: None,
        })
        .await
        .unwrap_err();
    assert!(err.contains("quota"), "expected quota error, got {err}");

    // Bravo is unaffected.
    hb.handle(VmOp::Create {
        label: "b1".into(),
        restart_policy: RestartPolicy::Never,
        image_path: None,
        cpu_count: Some(3),
        memory_mib: Some(2048),
        boot_blob_crc32c: None,
    })
    .await
    .unwrap();

    let ta = store.get(a.id).unwrap();
    let tb = store.get(b.id).unwrap();
    assert_eq!(ta.usage.vcpus, 2); // exactly at ceiling
    assert_eq!(tb.usage.vcpus, 3);
}

#[tokio::test]
async fn per_user_attenuated_caps_isolate_within_a_tenant() {
    let store = Arc::new(MemTenantStore::new());

    let t = store
        .create(
            TenantSpec::new("acme", quotas(8, 8192, 64 * 1024)).unwrap(),
            TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
        )
        .unwrap();

    let alice = store
        .add_user(t.id, "alice".to_string(), TenantCaps::VM_LIFECYCLE_READ)
        .unwrap();
    let bob = store
        .add_user(
            t.id,
            "bob".to_string(),
            TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
        )
        .unwrap();

    // Two host views: each user gets their own projected caps but
    // the same TenantStore so quota is shared.
    let host_alice = host_for(store.clone(), t.id, alice.caps.to_mesh_capabilities());
    let host_bob = host_for(store.clone(), t.id, bob.caps.to_mesh_capabilities());
    let node = NodeId::from("shared-node");
    let _ = host_alice.snapshot(&node).await;
    let _ = host_bob.snapshot(&node).await;

    // Alice can list but not create.
    let reply = host_alice.handle(VmOp::List).await.unwrap();
    assert!(matches!(reply, VmOpReply::Listed { .. }));
    let err = host_alice
        .handle(VmOp::Create {
            label: "denied".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: Some(1),
            memory_mib: None,
            boot_blob_crc32c: None,
        })
        .await
        .unwrap_err();
    assert!(err.contains("capability denied"));

    // No quota leak from the denied path.
    assert_eq!(store.get(t.id).unwrap().usage.vcpus, 0);

    // Bob can create.
    host_bob
        .handle(VmOp::Create {
            label: "bob-vm".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: Some(1),
            memory_mib: Some(256),
            boot_blob_crc32c: None,
        })
        .await
        .unwrap();
    assert_eq!(store.get(t.id).unwrap().usage.vcpus, 1);
}

#[tokio::test]
async fn capability_denial_does_not_charge_quota() {
    // Bind a tenant whose ROOT caps allow vol.write but the
    // *projected* host caps only allow vm lifecycle, modelling a
    // user whose attenuated caps don't include vol.write.
    let store = Arc::new(MemTenantStore::new());
    let t = store
        .create(
            TenantSpec::new("acme", quotas(8, 8192, 64 * 1024)).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    let read_only = Capabilities::VM_LIFECYCLE_READ;
    let host = host_for(store.clone(), t.id, read_only);
    let node = NodeId::from("n");
    let _ = host.snapshot(&node).await;

    // Important: capability check happens INSIDE the inner host,
    // AFTER the wrapper has charged. We expect the wrapper to
    // refund on failure so the tenant's books stay zero.
    let err = host
        .handle(VmOp::CreateVolume {
            name: "ghost".into(),
            size_bytes: 4096,
        })
        .await
        .unwrap_err();
    assert!(err.contains("capability denied"));
    assert_eq!(store.get(t.id).unwrap().usage.storage_bytes, 0);
}

#[tokio::test]
async fn delete_refunds_resources_back_to_quota() {
    let store = Arc::new(MemTenantStore::new());
    let t = store
        .create(
            TenantSpec::new("acme", quotas(2, 1024, 8192)).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    let host = host_for(store.clone(), t.id, Capabilities::ALL);
    let node = NodeId::from("n");
    let _ = host.snapshot(&node).await;

    // Allocate full quota.
    let reply = host
        .handle(VmOp::Create {
            label: "big".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: Some(2),
            memory_mib: Some(1024),
            boot_blob_crc32c: None,
        })
        .await
        .unwrap();
    let vm_id = match reply {
        VmOpReply::Created { vm_id } => vm_id,
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(store.get(t.id).unwrap().usage.vcpus, 2);

    // Drive to terminal then delete.
    host.handle(VmOp::Start { vm_id }).await.unwrap();
    host.handle(VmOp::Delete { vm_id }).await.unwrap();
    assert_eq!(store.get(t.id).unwrap().usage.vcpus, 0);

    // Slot is free — can reallocate.
    host.handle(VmOp::Create {
        label: "again".into(),
        restart_policy: RestartPolicy::Never,
        image_path: None,
        cpu_count: Some(2),
        memory_mib: Some(1024),
        boot_blob_crc32c: None,
    })
    .await
    .unwrap();
    assert_eq!(store.get(t.id).unwrap().usage.vcpus, 2);

    // Sanity check that an unrelated CelError variant still exists
    // — the previous block intentionally exercises no error paths.
    let _ = CelError::Invalid("noop");

    // Sanity check via direct store: a zero-only QuotaCharge is a
    // no-op against the store too.
    store
        .charge(t.id, QuotaCharge::default())
        .unwrap();
}
