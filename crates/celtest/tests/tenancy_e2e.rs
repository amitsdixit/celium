//! W27 — Tenancy Layer end-to-end integration tests.
//!
//! Each test exercises a different rung of the contract that the
//! Core Layer never learns about tenants: the Tenancy Layer mints a
//! capability set, projects it through
//! [`celtenancy::TenantCaps::to_mesh_capabilities`], and the
//! resulting `MemVmHost` enforces it via the existing W14 capability
//! check without any tenancy-aware code path.

use std::sync::Arc;

use celcommon::CelError;
use celmesh::{
    Capabilities, MemNetworkStore, MemVmHost, MemVolumeStore, RestartPolicy, VmHost, VmOp, VmOpReply,
};
use celtenancy::{
    FileTenantStore, MemTenantStore, QuotaCharge, TenantCaps, TenantNamespace, TenantQuotas,
    TenantSpec, TenantStore,
};

fn quotas() -> TenantQuotas {
    TenantQuotas {
        max_vcpus: 8,
        max_memory_mib: 8 * 1024,
        max_storage_bytes: 100 * 1024 * 1024,
        max_network_mbps: 1_000,
        max_iops: 10_000,
    }
}

fn tenant_host(core: Capabilities) -> MemVmHost {
    MemVmHost::with_stores(
        Arc::new(MemVolumeStore::new()),
        Arc::new(MemNetworkStore::new()),
    )
    .with_caps(core)
}

#[test]
fn tenant_create_lists_and_namespace_paths_are_well_formed() {
    let store = MemTenantStore::new();
    let t = store
        .create(
            TenantSpec::new("acme", quotas()).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();

    let ns = TenantNamespace::new(&t.name).unwrap();
    assert_eq!(t.namespace, "/tenants/acme");
    assert_eq!(ns.vms(), "/tenants/acme/vms");
    assert_eq!(ns.volumes(), "/tenants/acme/volumes");
    assert_eq!(ns.networks(), "/tenants/acme/networks");
    assert_eq!(ns.users(), "/tenants/acme/users");
    assert_eq!(ns.quotas(), "/tenants/acme/quotas");

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "acme");
}

#[tokio::test]
async fn tenant_root_capability_is_enforced_by_core_layer_host() {
    let store = MemTenantStore::new();
    let tenant = store
        .create(
            TenantSpec::new("blue", quotas()).unwrap(),
            TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
        )
        .unwrap();

    let core = tenant.root_caps.to_mesh_capabilities();
    assert!(core.contains(Capabilities::VM_LIFECYCLE_READ));
    assert!(core.contains(Capabilities::VM_LIFECYCLE_WRITE));
    assert!(!core.contains(Capabilities::VOLUME_WRITE));

    let host = tenant_host(core);

    let reply = host
        .handle(VmOp::Create {
            label: "web".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        })
        .await;
    assert!(
        matches!(reply, Ok(VmOpReply::Created { .. })),
        "expected Created, got {reply:?}",
    );

    let reply = host
        .handle(VmOp::CreateVolume {
            name: "data".into(),
            size_bytes: 1024,
        })
        .await;
    match reply {
        Err(msg) => assert!(
            msg.contains("capability denied"),
            "expected capability denial, got: {msg}",
        ),
        other => panic!("expected denial, got {other:?}"),
    }
}

#[tokio::test]
async fn user_capability_attenuation_is_enforced() {
    let store = MemTenantStore::new();
    let tenant = store
        .create(
            TenantSpec::new("green", quotas()).unwrap(),
            TenantCaps::VM_LIFECYCLE_READ | TenantCaps::VM_LIFECYCLE_WRITE,
        )
        .unwrap();

    let alice = store
        .add_user(tenant.id, "alice".to_string(), TenantCaps::VM_LIFECYCLE_READ)
        .unwrap();
    assert_eq!(alice.caps, TenantCaps::VM_LIFECYCLE_READ);

    let err = store
        .add_user(tenant.id, "bob".to_string(), TenantCaps::VOLUME_WRITE)
        .unwrap_err();
    assert!(matches!(err, CelError::CapabilityDenied(_)));

    let host = tenant_host(alice.caps.to_mesh_capabilities());

    let reply = host.handle(VmOp::List).await;
    assert!(matches!(reply, Ok(VmOpReply::Listed { .. })));

    let reply = host
        .handle(VmOp::Create {
            label: "x".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        })
        .await;
    match reply {
        Err(msg) => assert!(msg.contains("capability denied")),
        other => panic!("expected denial, got {other:?}"),
    }
}

#[test]
fn quota_charges_to_ceiling_then_rejects() {
    let store = MemTenantStore::new();
    let t = store
        .create(
            TenantSpec::new("quota", quotas()).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();

    store
        .charge(t.id, QuotaCharge { vcpus: 4, ..Default::default() })
        .unwrap();
    let u = store
        .charge(t.id, QuotaCharge { vcpus: 4, ..Default::default() })
        .unwrap();
    assert_eq!(u.vcpus, 8);

    let err = store
        .charge(t.id, QuotaCharge { vcpus: 1, ..Default::default() })
        .unwrap_err();
    assert!(matches!(err, CelError::Exhausted("quota.vcpus")));

    store
        .release(t.id, QuotaCharge { vcpus: 1, ..Default::default() })
        .unwrap();
    let u = store
        .charge(t.id, QuotaCharge { vcpus: 1, ..Default::default() })
        .unwrap();
    assert_eq!(u.vcpus, 8);
}

#[test]
fn file_tenant_store_persists_across_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "celtenancy-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tenants.json");

    {
        let s = FileTenantStore::open(&path).unwrap();
        let t = s
            .create(
                TenantSpec::new("persistent", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        s.add_user(t.id, "alice".to_string(), TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        s.charge(
            t.id,
            QuotaCharge { vcpus: 2, memory_mib: 256, ..Default::default() },
        )
        .unwrap();
    }

    let s = FileTenantStore::open(&path).unwrap();
    let t = s.get_by_name("persistent").unwrap();
    assert_eq!(t.name, "persistent");
    assert_eq!(t.users.len(), 1);
    assert_eq!(t.users[0].name, "alice");
    assert_eq!(t.usage.vcpus, 2);
    assert_eq!(t.usage.memory_mib, 256);

    let err = s.delete(t.id).unwrap_err();
    assert!(matches!(err, CelError::Invalid(_)));

    s.release(
        t.id,
        QuotaCharge { vcpus: 2, memory_mib: 256, ..Default::default() },
    )
    .unwrap();
    s.delete(t.id).unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn full_tenant_workflow_creates_a_vm() {
    let store = MemTenantStore::new();
    let tenant = store
        .create(
            TenantSpec::new("workflow", quotas()).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();

    let user = store
        .add_user(tenant.id, "ops".to_string(), TenantCaps::ALL)
        .unwrap();

    let charge = QuotaCharge {
        vcpus: 2,
        memory_mib: 1024,
        ..Default::default()
    };
    store.charge(tenant.id, charge).unwrap();

    let host = tenant_host(user.caps.to_mesh_capabilities());

    // The networking store mints ids relative to the owning node;
    // prime the host with a `snapshot(owner)` first so its owner is
    // known before the first `CreateNetwork`.
    let node = celmesh::NodeId::from("tenant-host");
    let _ = host.snapshot(&node).await;

    let reply = host
        .handle(VmOp::CreateNetwork {
            name: format!("{}-default", tenant.name),
            cidr: "10.100.0.0/24".to_string(),
        })
        .await;
    assert!(
        matches!(reply, Ok(VmOpReply::NetworkCreated { .. })),
        "expected NetworkCreated, got {reply:?}",
    );

    let reply = host
        .handle(VmOp::Create {
            label: "web".into(),
            restart_policy: RestartPolicy::Always,
            image_path: None,
            cpu_count: Some(2),
            memory_mib: Some(1024),
            boot_blob_crc32c: None,
        })
        .await;
    assert!(
        matches!(reply, Ok(VmOpReply::Created { .. })),
        "expected Created, got {reply:?}",
    );

    let t = store.get(tenant.id).unwrap();
    assert_eq!(t.usage.vcpus, 2);
    assert_eq!(t.usage.memory_mib, 1024);

    store.release(tenant.id, charge).unwrap();
    let t = store.get(tenant.id).unwrap();
    assert_eq!(t.usage.vcpus, 0);
}
