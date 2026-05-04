//! Week-9 multi-node integration test.
//!
//! Spins up three `celmesh::Mesh` nodes over the in-process
//! `MemTransport` and verifies that:
//!
//! 1. Membership converges — every node sees three `Alive` peers.
//! 2. A VM published on node `a` is visible from `b` and `c` via
//!    the federated namespace, addressable as
//!    `/cluster/a/vms/0`.
//! 3. After `b` shuts down, `a` and `c` mark `b` as non-Alive but
//!    still see `b`'s last-known VM rows (with `owner_alive = false`).
//!
//! These tests exercise the same public API the CLI uses (`Mesh`,
//! `RemoteVm`, `MeshConfig`) so a regression in either is caught.

use std::sync::Arc;
use std::time::Duration;

use celmesh::{Mesh, MeshConfig, MemTransportFactory, NodeId, RemoteVm};

/// Build a three-node test cluster wired through `MemTransport`.
async fn cluster3() -> (Mesh, Mesh, Mesh, MemTransportFactory) {
    let factory = MemTransportFactory::new();
    let ta = Arc::new(factory.bind("mem://a").await.unwrap());
    let tb = Arc::new(factory.bind("mem://b").await.unwrap());
    let tc = Arc::new(factory.bind("mem://c").await.unwrap());

    let mk = |id: &str, addr: &str, seeds: Vec<&str>| {
        let mut c = MeshConfig::defaults(id, addr);
        c.seeds           = seeds.into_iter().map(str::to_string).collect();
        c.gossip_interval = Duration::from_millis(20);
        c.timeout_suspect = Duration::from_millis(200);
        c.timeout_dead    = Duration::from_millis(500);
        c
    };

    let a = Mesh::start(mk("a", "mem://a", vec!["mem://b", "mem://c"]), ta).await.unwrap();
    let b = Mesh::start(mk("b", "mem://b", vec!["mem://a"]),            tb).await.unwrap();
    let c = Mesh::start(mk("c", "mem://c", vec!["mem://a"]),            tc).await.unwrap();

    (a, b, c, factory)
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

fn mk_vm(owner: &str, id: u32, label: &str) -> RemoteVm {
    RemoteVm {
        owner: NodeId::from(owner),
        vm_id: id,
        label: label.into(),
        state: "created".into(),
        last_exit: None,
        epoch: 1,
        hlc:   0,
        owner_alive: true,
        restart_policy: celmesh::RestartPolicy::Never,
        volumes: Vec::new(),
    }
}

#[tokio::test]
async fn three_nodes_converge_on_membership() {
    let (a, b, c, _f) = cluster3().await;
    let ok = wait_until(|| async {
        a.alive_count().await == 3
            && b.alive_count().await == 3
            && c.alive_count().await == 3
    }).await;
    assert!(ok, "membership did not converge to 3 alive nodes");

    let _ = a.shutdown().await;
    let _ = b.shutdown().await;
    let _ = c.shutdown().await;
}

#[tokio::test]
async fn vm_on_a_is_visible_on_b_and_c() {
    let (a, b, c, _f) = cluster3().await;
    // Wait for the cluster to form before publishing.
    let _ = wait_until(|| async {
        a.alive_count().await == 3
    }).await;

    a.publish_local_vms(vec![mk_vm("a", 0, "guest-zero")]).await.unwrap();

    let on_b = wait_until(|| async {
        b.list_vms().await.iter().any(|r| r.owner.as_str() == "a" && r.label == "guest-zero")
    }).await;
    let on_c = wait_until(|| async {
        c.list_vms().await.iter().any(|r| r.owner.as_str() == "a" && r.label == "guest-zero")
    }).await;
    assert!(on_b, "VM did not propagate to b");
    assert!(on_c, "VM did not propagate to c");

    // Path round-trip is the operator-facing contract.
    let row = b.list_vms().await
        .into_iter()
        .find(|r| r.owner.as_str() == "a" && r.vm_id == 0)
        .unwrap();
    assert_eq!(row.path(), "/cluster/a/vms/0");

    let _ = a.shutdown().await;
    let _ = b.shutdown().await;
    let _ = c.shutdown().await;
}

#[tokio::test]
async fn departed_owner_keeps_last_known_vms_with_owner_alive_false() {
    let (a, b, c, factory) = cluster3().await;
    let _ = wait_until(|| async { a.alive_count().await == 3 }).await;

    b.publish_local_vms(vec![mk_vm("b", 0, "from-b")]).await.unwrap();
    let propagated = wait_until(|| async {
        a.list_vms().await.iter().any(|r| r.owner.as_str() == "b")
            && c.list_vms().await.iter().any(|r| r.owner.as_str() == "b")
    }).await;
    assert!(propagated, "b's VM never propagated");

    // Hard-kill b — no goodbye, no shutdown.
    factory.drop_addr("mem://b").await;
    let _ = b.shutdown().await; // also abort the gossip loop

    // Wait for a + c to mark b non-Alive.
    let detected = wait_until(|| async {
        let av = a.list_vms().await.into_iter().find(|r| r.owner.as_str() == "b");
        let cv = c.list_vms().await.into_iter().find(|r| r.owner.as_str() == "b");
        matches!(&av, Some(r) if !r.owner_alive)
            && matches!(&cv, Some(r) if !r.owner_alive)
    }).await;
    assert!(detected, "failure detector did not flag departed owner");

    let _ = a.shutdown().await;
    let _ = c.shutdown().await;
}
