//! Week-11 multi-node integration test over the **real UDP transport**.
//!
//! This is the first cross-node test that exercises the actual
//! `tokio::net::UdpSocket` plane, gossip-driven membership, federated
//! VM ops via path addressing (`/cluster/<node>/vms/<n>`) and the
//! auto-supervisor (`MeshConfig::supervisor_interval`). It mirrors
//! `multi_node_guest.rs` but never touches the in-process MemTransport.

use std::sync::Arc;
use std::time::Duration;

use celmesh::{
    Mesh, MeshConfig, MemVmHost, NodeId, RestartPolicy, Transport, UdpTransport,
    VmHost, VmOp, VmOpReply,
};

async fn wait_until<F, Fut>(mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..400 {
        if probe().await { return true; }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

fn cfg(id: &str, addr: &str, seeds: Vec<String>, sup: Duration) -> MeshConfig {
    let mut c = MeshConfig::defaults(id, addr);
    c.seeds               = seeds;
    c.gossip_interval     = Duration::from_millis(50);
    c.timeout_suspect     = Duration::from_millis(250);
    c.timeout_dead        = Duration::from_millis(750);
    c.supervisor_interval = sup;
    c
}

/// Bring up three real-UDP nodes and run a guest VM lifecycle
/// (`create → start → stop`) end-to-end, addressing the VM by its
/// federated path on the third hop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_three_nodes_guest_lifecycle_via_path() {
    // Bind ephemeral UDP sockets and learn the actual bound addrs.
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t3 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();
    let a3 = t3.local_addr();

    // 1s supervisor cadence so a missed Alive becomes a restart fast.
    let sup = Duration::from_millis(200);
    let n1 = Mesh::start(cfg("n1", &a1, vec![a2.clone(), a3.clone()], sup), t1).await.unwrap();
    let n2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()],            sup), t2).await.unwrap();
    let n3 = Mesh::start(cfg("n3", &a3, vec![a1.clone()],            sup), t3).await.unwrap();

    let h1: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    let h2: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    let h3: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    n1.set_host(h1).await;
    n2.set_host(h2).await;
    n3.set_host(h3).await;

    // Wait for all three nodes to see a 3-Alive cluster before we
    // dispatch any RPC. Without this guard `invoke` can race gossip.
    assert!(
        wait_until(|| async {
            n1.alive_count().await == 3
                && n2.alive_count().await == 3
                && n3.alive_count().await == 3
        }).await,
        "membership did not converge over UDP"
    );

    // n2 → n1: Create.
    let created = n2.invoke(
        &NodeId::from("n1"),
        VmOp::Create { label: "udp-guest".into(), restart_policy: RestartPolicy::Never },
        Duration::from_millis(3_000),
    ).await.expect("create");
    let vm_id = match created {
        VmOpReply::Created { vm_id } => vm_id,
        other => panic!("unexpected create reply: {other:?}"),
    };

    // n3 → n1: Start, addressed by federated path.
    let path = format!("/cluster/n1/vms/{vm_id}");
    let started = n3.invoke_path(
        &path,
        VmOp::Start { vm_id: 0 },           // vm_id is overwritten from path
        Duration::from_millis(3_000),
    ).await.expect("start via path");
    match started {
        VmOpReply::State { vm_id: v, ref state } if v == vm_id && state == "halted" => {}
        other => panic!("unexpected start reply: {other:?}"),
    }

    // n3 → n1: Stop, also via path. Stop is idempotent on terminal
    // states, so a VM that has already exited (HLT) stays "halted".
    let stopped = n3.invoke_path(
        &path,
        VmOp::Stop { vm_id: 0 },
        Duration::from_millis(3_000),
    ).await.expect("stop via path");
    match stopped {
        VmOpReply::State { vm_id: v, ref state }
            if v == vm_id && matches!(state.as_str(), "halted" | "stopped") => {}
        other => panic!("unexpected stop reply: {other:?}"),
    }

    let _ = n1.shutdown().await;
    let _ = n2.shutdown().await;
    let _ = n3.shutdown().await;
}

/// Verify the auto-supervisor: spawn three nodes with
/// `restart_policy=Always` on n2, kill n2, and expect the lowest-id
/// Alive node (n1) to recreate the VM locally without any explicit
/// `run_supervisor_step` call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_auto_supervisor_restarts_orphan() {
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t3 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();
    let a3 = t3.local_addr();

    let sup = Duration::from_millis(200);
    let n1 = Mesh::start(cfg("n1", &a1, vec![a2.clone(), a3.clone()], sup), t1).await.unwrap();
    let n2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()],            sup), t2).await.unwrap();
    let n3 = Mesh::start(cfg("n3", &a3, vec![a1.clone()],            sup), t3).await.unwrap();

    n1.set_host(Arc::new(MemVmHost::new())).await;
    n2.set_host(Arc::new(MemVmHost::new())).await;
    n3.set_host(Arc::new(MemVmHost::new())).await;

    assert!(
        wait_until(|| async {
            n1.alive_count().await == 3
                && n2.alive_count().await == 3
                && n3.alive_count().await == 3
        }).await,
        "membership did not converge"
    );

    // Create a VM on n2 with restart_policy=Always.
    let created = n3.invoke(
        &NodeId::from("n2"),
        VmOp::Create { label: "phoenix".into(), restart_policy: RestartPolicy::Always },
        Duration::from_millis(3_000),
    ).await.expect("create on n2");
    let vm_id = match created {
        VmOpReply::Created { vm_id } => vm_id,
        other => panic!("unexpected: {other:?}"),
    };

    // Tear n2 down — its rows are about to become orphans.
    let _ = n2.shutdown().await;

    // Wait for the auto-supervisor on n1 to recreate the VM. We
    // probe via federated rows on n3 to keep the assertion at the
    // cluster level rather than reaching into n1 directly.
    let label_match = "phoenix@n2".to_string();
    assert!(
        wait_until(|| {
            let n3 = n3.clone();
            let label_match = label_match.clone();
            async move {
                let rows = n3.list_vms().await;
                rows.iter().any(|r| r.owner == NodeId::from("n1") && r.label == label_match)
            }
        }).await,
        "auto-supervisor did not recreate orphan vm; current vms: {:?}",
        n3.list_vms().await
    );

    // Sanity: original orphan row reports owner_alive=false.
    let rows = n3.list_vms().await;
    assert!(
        rows.iter().any(|r| r.owner == NodeId::from("n2") && r.vm_id == vm_id && !r.owner_alive),
        "orphan row should still be visible with owner_alive=false: {:?}", rows
    );

    let _ = n1.shutdown().await;
    let _ = n3.shutdown().await;
}

/// Cluster status snapshot exposes the right counters and includes
/// every alive member.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_cluster_status_reports_full_topology() {
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();

    let n1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()], Duration::ZERO), t1).await.unwrap();
    let n2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()], Duration::ZERO), t2).await.unwrap();
    n1.set_host(Arc::new(MemVmHost::new())).await;
    n2.set_host(Arc::new(MemVmHost::new())).await;

    assert!(
        wait_until(|| async { n1.alive_count().await == 2 && n2.alive_count().await == 2 }).await
    );

    let s = n1.cluster_status().await;
    assert_eq!(s.alive, 2, "alive count: {s:?}");
    assert_eq!(s.cluster, "celium");
    assert_eq!(s.self_id, NodeId::from("n1"));
    assert!(s.members.iter().any(|m| m.id == NodeId::from("n2")));

    let _ = n1.shutdown().await;
    let _ = n2.shutdown().await;
}
