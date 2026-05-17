//! Week-10 multi-node guest integration test.
//!
//! Differs from `multi_node.rs` (Week 9) in that it exercises the
//! cross-node *control plane* that landed this week:
//!
//! * `Mesh::set_host` registers an in-process `VmHost` on every node.
//! * `Mesh::invoke` ships a `Request` to a remote node, which
//!   dispatches to that node's host and returns the reply.
//! * `Mesh::run_supervisor_step` recreates VMs whose owners died and
//!   whose `restart_policy` is `Always`.
//!
//! The test runs three nodes wired through the in-memory transport.
//! No network involvement, so it is deterministic and fast — but it
//! exercises the entire encode/decode/dispatch path end-to-end.

use std::sync::Arc;
use std::time::Duration;

use celmesh::{
    Mesh, MeshConfig, MemTransportFactory, MemVmHost, NodeId, RestartPolicy, VmOp,
    VmOpReply,
};

/// Build three nodes wired together. Each node carries a freshly
/// constructed [`MemVmHost`] so cross-node `invoke` calls have a
/// place to land.
async fn cluster3() -> (Mesh, Mesh, Mesh, MemTransportFactory,
                       Arc<MemVmHost>, Arc<MemVmHost>, Arc<MemVmHost>) {
    let factory = MemTransportFactory::new();
    let ta = Arc::new(factory.bind("mem://n1").await.unwrap());
    let tb = Arc::new(factory.bind("mem://n2").await.unwrap());
    let tc = Arc::new(factory.bind("mem://n3").await.unwrap());

    let mk = |id: &str, addr: &str, seeds: Vec<&str>| {
        let mut c = MeshConfig::defaults(id, addr);
        c.seeds           = seeds.into_iter().map(str::to_string).collect();
        c.gossip_interval = Duration::from_millis(20);
        c.timeout_suspect = Duration::from_millis(200);
        c.timeout_dead    = Duration::from_millis(500);
        c
    };

    let n1 = Mesh::start(mk("n1", "mem://n1", vec!["mem://n2", "mem://n3"]), ta).await.unwrap();
    let n2 = Mesh::start(mk("n2", "mem://n2", vec!["mem://n1"]),             tb).await.unwrap();
    let n3 = Mesh::start(mk("n3", "mem://n3", vec!["mem://n1"]),             tc).await.unwrap();

    let h1 = Arc::new(MemVmHost::new());
    let h2 = Arc::new(MemVmHost::new());
    let h3 = Arc::new(MemVmHost::new());
    n1.set_host(h1.clone()).await;
    n2.set_host(h2.clone()).await;
    n3.set_host(h3.clone()).await;

    (n1, n2, n3, factory, h1, h2, h3)
}

async fn wait_until<F, Fut>(mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if probe().await { return true; }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// 1. Create a VM on `n1` *from `n2`* via `invoke`.
/// 2. Verify the federated row is visible on every node.
/// 3. Start the VM remotely from `n3`. Verify state propagates back.
#[tokio::test]
async fn create_then_start_vm_across_nodes() {
    let (n1, n2, n3, _f, _h1, _h2, _h3) = cluster3().await;
    assert!(wait_until(|| async {
        n1.alive_count().await == 3
            && n2.alive_count().await == 3
            && n3.alive_count().await == 3
    }).await);

    // n2 → n1: create a VM.
    let create = n2.invoke(
        &NodeId::from("n1"),
        VmOp::Create {
            label: "guest-zero".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        },
        Duration::from_millis(2_000),
    ).await.expect("invoke create");
    let vm_id = match create {
        VmOpReply::Created { vm_id } => vm_id,
        other => panic!("unexpected reply: {other:?}"),
    };

    // VM should appear on every node's federated list.
    let visible_everywhere = wait_until(|| async {
        let on_n1 = n1.list_vms().await.iter().any(|r| r.owner.as_str() == "n1" && r.vm_id == vm_id);
        let on_n2 = n2.list_vms().await.iter().any(|r| r.owner.as_str() == "n1" && r.vm_id == vm_id);
        let on_n3 = n3.list_vms().await.iter().any(|r| r.owner.as_str() == "n1" && r.vm_id == vm_id);
        on_n1 && on_n2 && on_n3
    }).await;
    assert!(visible_everywhere, "VM did not propagate to all 3 nodes");

    // n3 → n1: start the VM remotely.
    let started = n3.invoke(
        &NodeId::from("n1"),
        VmOp::Start { vm_id },
        Duration::from_millis(2_000),
    ).await.expect("invoke start");
    assert!(matches!(started, VmOpReply::State { state, .. } if state == "halted"));

    // The new state must propagate back to n3's federated view.
    assert!(wait_until(|| async {
        n3.list_vms().await.iter().any(|r|
            r.owner.as_str() == "n1" && r.vm_id == vm_id && r.state == "halted")
    }).await, "halted state did not propagate back to n3");

    let _ = n1.shutdown().await;
    let _ = n2.shutdown().await;
    let _ = n3.shutdown().await;
}

/// 1. Create a VM on `n3` with `restart_policy = Always`.
/// 2. Kill `n3` (drop transport).
/// 3. Verify the supervisor (lowest-id Alive node, expected `n1`)
///    recreates an equivalent VM locally.
/// 4. The new row carries the original label suffixed with `@<owner>`.
#[tokio::test]
async fn supervisor_restarts_orphan_vm_with_always_policy() {
    let (n1, n2, n3, factory, _h1, _h2, _h3) = cluster3().await;
    // All three nodes must see the full set before any cross-node
    // RPC, otherwise the receiver may not know how to address the
    // requester yet.
    assert!(wait_until(|| async {
        n1.alive_count().await == 3
            && n2.alive_count().await == 3
            && n3.alive_count().await == 3
    }).await);

    // Publish a VM on n3.
    let _ = n2.invoke(
        &NodeId::from("n3"),
        VmOp::Create {
            label: "critical".into(),
            restart_policy: RestartPolicy::Always,
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        },
        Duration::from_millis(2_000),
    ).await.expect("invoke create");

    // Wait for the row to reach n1.
    assert!(wait_until(|| async {
        n1.list_vms().await.iter().any(|r| r.owner.as_str() == "n3" && r.label == "critical")
    }).await, "VM did not reach n1");

    // Kill n3.
    factory.drop_addr("mem://n3").await;
    let _ = n3.shutdown().await;

    // n1 must observe n3 as not-Alive.
    assert!(wait_until(|| async {
        n1.list_vms().await.iter().any(|r|
            r.owner.as_str() == "n3" && r.label == "critical" && !r.owner_alive)
    }).await, "n1 did not detect n3 failure");

    // n1 should be the supervisor (lowest-id alive).
    assert!(n1.is_supervisor().await, "n1 expected to be supervisor");
    assert!(!n2.is_supervisor().await);

    // Run one supervisor pass on n1.
    let restarted = n1.run_supervisor_step().await.expect("supervisor step");
    assert_eq!(restarted.len(), 1, "expected exactly one recreation");
    assert_eq!(restarted[0].original_owner, NodeId::from("n3"));
    assert!(restarted[0].label.starts_with("critical@"));

    // The new row must propagate to n2.
    assert!(wait_until(|| async {
        n2.list_vms().await.iter().any(|r|
            r.owner.as_str() == "n1" && r.label.starts_with("critical@"))
    }).await, "recreation did not propagate to n2");

    // A second pass must be a no-op (idempotent within a step).
    let again = n1.run_supervisor_step().await.expect("second supervisor step");
    assert!(again.is_empty(), "second pass should be a no-op");

    let _ = n1.shutdown().await;
    let _ = n2.shutdown().await;
}

/// Sanity: an `invoke` against an unknown node id surfaces a clean
/// error, not a panic.
#[tokio::test]
async fn invoke_unknown_target_is_clean_error() {
    let (n1, _n2, _n3, _f, _h1, _h2, _h3) = cluster3().await;
    assert!(wait_until(|| async { n1.alive_count().await == 3 }).await);

    let r = n1.invoke(
        &NodeId::from("ghost"),
        VmOp::List,
        Duration::from_millis(500),
    ).await;
    assert!(r.is_err(), "ghost target must error");
}
