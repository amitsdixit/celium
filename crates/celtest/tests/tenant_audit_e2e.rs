//! W30 — Tenant audit sink end-to-end.
//!
//! Drives the full `celtenancy::exec::exec` path through a
//! `FileTenantStore` *and* a `FileAuditSink` on disk, restarts the
//! process, and verifies the audit history persists across
//! reopens — the property a real operator depends on when grepping
//! audit logs after a crash.

use std::sync::Arc;

use celtenancy::audit::{AuditAction, AuditSink, FileAuditSink};
use celtenancy::exec::{self, ExecOptions};
use celtenancy::{
    FileTenantStore, TenantCaps, TenantQuotas, TenantSpec, TenantStore,
};

fn quotas() -> TenantQuotas {
    TenantQuotas {
        max_vcpus: 8,
        max_memory_mib: 16 * 1024,
        max_storage_bytes: 1024 * 1024,
        max_network_mbps: 10_000,
        max_iops: 50_000,
    }
}

#[tokio::test]
async fn audit_log_records_charge_and_exec_across_process_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store_path = tmp.path().join("tenants.json");
    let audit_path = tmp.path().join("audit.jsonl");

    // Bootstrap tenant in process 1.
    {
        let s: Arc<dyn TenantStore> =
            Arc::new(FileTenantStore::open(&store_path).unwrap());
        s.create(
            TenantSpec::new("acme", quotas()).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    }

    // Process 2: dispatch a VM create with audit on.
    {
        let s: Arc<dyn TenantStore> =
            Arc::new(FileTenantStore::open(&store_path).unwrap());
        let sink: Arc<dyn AuditSink> =
            Arc::new(FileAuditSink::open(&audit_path).unwrap());
        let opts = ExecOptions {
            release_after_create: false,
            node: None,
            audit: Some(sink),
        };
        let audit = exec::exec(
            s,
            "acme",
            None,
            exec::vm_create_op("web", 2, 1024),
            opts,
        )
        .await
        .unwrap();
        assert!(audit.ok());
    }

    // Process 3: dispatch a quota-exhausting create and verify the
    // Deny event lands alongside the previous Charge.
    {
        let s: Arc<dyn TenantStore> =
            Arc::new(FileTenantStore::open(&store_path).unwrap());
        let sink: Arc<dyn AuditSink> =
            Arc::new(FileAuditSink::open(&audit_path).unwrap());
        let opts = ExecOptions {
            release_after_create: false,
            node: None,
            audit: Some(sink),
        };
        let audit = exec::exec(
            s,
            "acme",
            None,
            // 16 vCPUs blows past the 8-vCPU ceiling.
            exec::vm_create_op("big", 16, 1024),
            opts,
        )
        .await
        .unwrap();
        assert!(!audit.ok());
    }

    // Process 4: reopen for reading and assert the full timeline.
    {
        let sink = FileAuditSink::open(&audit_path).unwrap();
        let events = sink.read_all().unwrap();
        assert!(
            events.len() >= 4,
            "expected >=4 events, got {}",
            events.len()
        );
        // Successful trip first: Charge + Exec(success).
        assert_eq!(events[0].action, AuditAction::Charge);
        assert_eq!(events[0].op_capability_tag.as_deref(), Some("vm.create"));
        assert!(events[0].success);
        assert_eq!(events[1].action, AuditAction::Exec);
        assert!(events[1].success);
        // Quota-exhausted trip: Deny + Exec(failed).
        assert_eq!(events[2].action, AuditAction::Deny);
        assert!(!events[2].success);
        assert!(events[2].error.as_deref().unwrap().contains("quota"));
        assert_eq!(events[3].action, AuditAction::Exec);
        assert!(!events[3].success);
        // Tail correctness.
        let tail2 = sink.tail(2).unwrap();
        assert_eq!(tail2.len(), 2);
        assert_eq!(tail2[1].action, AuditAction::Exec);
        assert!(!tail2[1].success);
    }
}

#[tokio::test]
async fn audit_log_release_after_create_records_dry_run_release() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store_path = tmp.path().join("tenants.json");
    let audit_path = tmp.path().join("audit.jsonl");

    {
        let s: Arc<dyn TenantStore> =
            Arc::new(FileTenantStore::open(&store_path).unwrap());
        s.create(
            TenantSpec::new("acme", quotas()).unwrap(),
            TenantCaps::ALL,
        )
        .unwrap();
    }

    {
        let s: Arc<dyn TenantStore> =
            Arc::new(FileTenantStore::open(&store_path).unwrap());
        let sink: Arc<dyn AuditSink> =
            Arc::new(FileAuditSink::open(&audit_path).unwrap());
        let opts = ExecOptions {
            release_after_create: true,
            node: None,
            audit: Some(sink),
        };
        let audit = exec::exec(
            s.clone(),
            "acme",
            None,
            exec::volume_create_op("scratch", 8192),
            opts,
        )
        .await
        .unwrap();
        assert!(audit.ok());
        assert_eq!(audit.usage_after, audit.usage_before);
    }

    let sink = FileAuditSink::open(&audit_path).unwrap();
    let events = sink.read_all().unwrap();
    assert_eq!(events.len(), 3, "events: {events:?}");
    assert_eq!(events[0].action, AuditAction::Charge);
    assert_eq!(events[0].op_capability_tag.as_deref(), Some("vol.create"));
    assert_eq!(events[1].action, AuditAction::Release);
    assert_eq!(events[1].note.as_deref(), Some("dry-run"));
    assert_eq!(events[2].action, AuditAction::Exec);
    assert!(events[2]
        .note
        .as_deref()
        .unwrap()
        .contains("released=true"));
}
