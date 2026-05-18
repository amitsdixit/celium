//! W33 — final Tenancy Layer polish: cap rotation, recursive
//! tenant deletion, bulk session revocation, multi-user
//! integration.
//!
//! These tests drive the public [`TenantStore`] surface end-to-end
//! through a [`FileTenantStore`] so the on-disk persistence path
//! is exercised the same way `celctl tenant` does in production.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use celcommon::CelError;
use celtenancy::{
    DeleteReport, FileTenantStore, RotateReport, TenantCaps, TenantQuotas, TenantSpec,
    TenantStore,
};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tempdir() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "celium-w33-e2e-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

fn quotas() -> TenantQuotas {
    TenantQuotas {
        max_vcpus: 8,
        max_memory_mib: 8 * 1024,
        max_storage_bytes: 1_000_000_000,
        max_network_mbps: 1_000,
        max_iops: 10_000,
    }
}

/// `rotate_root_caps` survives a process restart: the persisted
/// JSON correctly carries the narrowed caps + cleared session
/// list.
#[test]
fn rotate_caps_persists_across_reopen() {
    let dir = tempdir();
    let path = dir.join("tenants.json");
    let report: RotateReport = {
        let s = FileTenantStore::open(&path).unwrap();
        let t = s
            .create(
                TenantSpec::new("acme", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let u = s
            .add_user(t.id, "alice".into(), TenantCaps::ALL)
            .unwrap();
        s.set_password(t.id, u.id, "pw").unwrap();
        let (_tok, _) = s
            .create_session(t.id, u.id, TenantCaps::ALL, Some(60))
            .unwrap();
        s.rotate_root_caps(t.id, TenantCaps::VM_LIFECYCLE_READ)
            .unwrap()
    };
    assert_eq!(report.attenuated_users, 1);
    assert_eq!(report.revoked_sessions, 1);

    // Reopen and verify durability.
    let s2 = FileTenantStore::open(&path).unwrap();
    let t2 = s2.get_by_name("acme").unwrap();
    assert_eq!(t2.root_caps, TenantCaps::VM_LIFECYCLE_READ);
    assert_eq!(t2.users[0].caps, TenantCaps::VM_LIFECYCLE_READ);
    // No live sessions after the rotation.
    assert_eq!(s2.purge_expired_sessions().unwrap(), 0);
}

/// Subtenant cannot rotate up past its parent's ceiling.
#[test]
fn rotate_caps_refuses_to_escape_parent_ceiling() {
    let dir = tempdir();
    let path = dir.join("tenants.json");
    let s = FileTenantStore::open(&path).unwrap();
    let parent = s
        .create(
            TenantSpec::new("p", quotas()).unwrap(),
            TenantCaps::VM_LIFECYCLE_READ,
        )
        .unwrap();
    let child = s
        .create_subtenant(
            parent.id,
            TenantSpec::new("c", quotas()).unwrap(),
            TenantCaps::VM_LIFECYCLE_READ,
        )
        .unwrap();
    let err = s
        .rotate_root_caps(child.id, TenantCaps::ALL)
        .unwrap_err();
    assert!(matches!(err, CelError::CapabilityDenied(_)));
    // Child caps unchanged.
    assert_eq!(
        s.get(child.id).unwrap().root_caps,
        TenantCaps::VM_LIFECYCLE_READ
    );
}

/// A password change kicks every session for that user, but leaves
/// the OTHER users' sessions alone. Persists across reopen.
#[test]
fn password_change_kicks_user_sessions_only() {
    let dir = tempdir();
    let path = dir.join("tenants.json");
    let s = FileTenantStore::open(&path).unwrap();
    let t = s
        .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
        .unwrap();
    let a = s.add_user(t.id, "alice".into(), TenantCaps::ALL).unwrap();
    let b = s.add_user(t.id, "bob".into(), TenantCaps::ALL).unwrap();
    s.set_password(t.id, a.id, "alice-pw").unwrap();
    s.set_password(t.id, b.id, "bob-pw").unwrap();
    let (a_tok, _) = s
        .create_session(t.id, a.id, TenantCaps::ALL, Some(60))
        .unwrap();
    let (b_tok, _) = s
        .create_session(t.id, b.id, TenantCaps::ALL, Some(60))
        .unwrap();
    // Rotate Alice's password — Bob's session must survive.
    s.set_password(t.id, a.id, "alice-pw-v2").unwrap();
    drop(s);

    let s2 = FileTenantStore::open(&path).unwrap();
    assert!(s2.validate_token(&a_tok).is_err());
    assert!(s2.validate_token(&b_tok).is_ok());
}

/// Recursive deletion walks the subtree post-order, revoking
/// every session and dropping every user along the way.
#[test]
fn delete_recursive_cleans_up_subtree() {
    let dir = tempdir();
    let path = dir.join("tenants.json");
    let s = FileTenantStore::open(&path).unwrap();
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
    // Sprinkle users + sessions across the subtree.
    for (tid, name) in [(root.id, "r"), (mid.id, "m"), (leaf.id, "l")] {
        let u = s.add_user(tid, name.into(), TenantCaps::ALL).unwrap();
        s.set_password(tid, u.id, "pw").unwrap();
        let (_tok, _) = s
            .create_session(tid, u.id, TenantCaps::ALL, Some(60))
            .unwrap();
    }
    let report: DeleteReport = s.delete_tenant_recursive(root.id).unwrap();
    assert_eq!(report.deleted_tenants.len(), 3);
    assert_eq!(report.revoked_sessions, 3);
    assert_eq!(report.dropped_users, 3);
    // Post-order: leaf, mid, root.
    assert_eq!(report.deleted_tenants[0].1, "leaf");
    assert_eq!(report.deleted_tenants[1].1, "mid");
    assert_eq!(report.deleted_tenants[2].1, "root");
    drop(s);

    // Reopen — store is empty.
    let s2 = FileTenantStore::open(&path).unwrap();
    assert!(s2.list().unwrap().is_empty());
}

/// Recursive deletion refuses if any node in the subtree has
/// non-zero usage. Atomicity check: no partial cleanup must
/// land on disk.
#[test]
fn delete_recursive_refuses_when_any_node_in_use() {
    let dir = tempdir();
    let path = dir.join("tenants.json");
    let s = FileTenantStore::open(&path).unwrap();
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
    // Charge against the child — propagates to parent.
    s.charge(
        c.id,
        celtenancy::QuotaCharge {
            vcpus: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let err = s.delete_tenant_recursive(p.id).unwrap_err();
    assert!(matches!(err, CelError::Invalid("tenant in use")));
    drop(s);

    // Reopen — both tenants still present.
    let s2 = FileTenantStore::open(&path).unwrap();
    assert_eq!(s2.list().unwrap().len(), 2);
}

/// `revoke_tenant_sessions` kicks every active user in the
/// tenant simultaneously — multi-user "kick everyone" scenario.
#[test]
fn bulk_revoke_kicks_every_user_in_tenant() {
    let dir = tempdir();
    let path = dir.join("tenants.json");
    let s = FileTenantStore::open(&path).unwrap();
    let t = s
        .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
        .unwrap();
    let mut tokens = Vec::new();
    for n in 0..5 {
        let u = s
            .add_user(t.id, format!("user-{n}"), TenantCaps::ALL)
            .unwrap();
        s.set_password(t.id, u.id, "pw").unwrap();
        let (tok, _) = s
            .create_session(t.id, u.id, TenantCaps::ALL, Some(60))
            .unwrap();
        tokens.push(tok);
    }
    let n = s.revoke_tenant_sessions(t.id).unwrap();
    assert_eq!(n, 5);
    for tok in &tokens {
        assert!(s.validate_token(tok).is_err());
    }
}
