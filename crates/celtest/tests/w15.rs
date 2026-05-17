//! Week-15 end-to-end tests.
//!
//! Three scenarios over real UDP between two nodes:
//!
//! 1. **`networking_round_trip`** — create a network on `n2`, attach
//!    NICs to two VMs, verify auto-allocated IPs land inside the
//!    CIDR and are unique, build a security group + a load balancer,
//!    then list each surface back from `n1`.
//! 2. **`k8s_personality_provisions_cluster`** — drive
//!    [`K8sCluster::create`] from `n1` against `n2`. Confirm the
//!    expected number of VMs, NICs, and a single LB exist on `n2`,
//!    and that a follow-up `cluster_report` from `n1` accounts for
//!    them.
//! 3. **`networking_capability_denial`** — restrict `n2` to read-
//!    only network caps and verify mutating ops fail with the
//!    stable `capability denied` error string.

use std::sync::Arc;
use std::time::Duration;

use celmesh::{
    Capabilities, Direction, K8sCluster, K8sClusterSpec, L4Proto, LbAlgo, LbBackend,
    Mesh, MeshConfig, MemVmHost, NetworkId, NodeId, SecurityRule, Transport, UdpTransport,
    VmHost, VmOp, VmOpReply,
};
use celmesh::Cidr4;

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

fn cfg(id: &str, addr: &str, seeds: Vec<String>) -> MeshConfig {
    let mut c = MeshConfig::defaults(id, addr);
    c.seeds = seeds;
    c.gossip_interval = Duration::from_millis(50);
    c.timeout_suspect = Duration::from_millis(250);
    c.timeout_dead    = Duration::from_millis(750);
    c
}

async fn pair(caps_for_n2: Capabilities) -> (Mesh, Mesh, NodeId) {
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();
    let m1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()]), t1.clone()).await.unwrap();
    let m2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()]), t2.clone()).await.unwrap();
    let host2: Arc<dyn VmHost> = Arc::new(MemVmHost::new().with_caps(caps_for_n2));
    m2.set_host(host2.clone()).await;
    let host1: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    m1.set_host(host1).await;
    let n2 = NodeId::from("n2");
    let _ = host2.snapshot(&n2).await;

    assert!(wait_until(|| async {
        m1.alive_count().await >= 2 && m2.alive_count().await >= 2
    }).await, "cluster failed to converge");
    (m1, m2, n2)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn networking_round_trip() {
    let (m1, m2, n2) = pair(Capabilities::ALL).await;

    // Two VMs.
    let vm0 = match m1.invoke(&n2, VmOp::Create {
        label: "a".into(),
        restart_policy: celmesh::RestartPolicy::Never,
        image_path: None,
        cpu_count: None,
        memory_mib: None,
        boot_blob_crc32c: None,
    }, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::Created { vm_id } => vm_id,
        r => panic!("unexpected: {r:?}"),
    };
    let vm1 = match m1.invoke(&n2, VmOp::Create {
        label: "b".into(),
        restart_policy: celmesh::RestartPolicy::Never,
        image_path: None,
        cpu_count: None,
        memory_mib: None,
        boot_blob_crc32c: None,
    }, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::Created { vm_id } => vm_id,
        r => panic!("unexpected: {r:?}"),
    };

    // Network on n2.
    let net = match m1.invoke(&n2, VmOp::CreateNetwork {
        name: "default".into(), cidr: "10.42.0.0/24".into(),
    }, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::NetworkCreated { network } => network,
        r => panic!("unexpected: {r:?}"),
    };
    assert_eq!(net.cidr, Cidr4::parse("10.42.0.0/24").unwrap());

    // Two NICs.
    let nic0 = match m1.invoke(&n2, VmOp::AttachNic {
        network_id: net.id.clone(), vm_id: vm0, ip: None,
    }, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::NicAttached { nic } => nic,
        r => panic!("unexpected: {r:?}"),
    };
    let nic1 = match m1.invoke(&n2, VmOp::AttachNic {
        network_id: net.id.clone(), vm_id: vm1, ip: None,
    }, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::NicAttached { nic } => nic,
        r => panic!("unexpected: {r:?}"),
    };
    assert!(net.cidr.contains(nic0.ip));
    assert!(net.cidr.contains(nic1.ip));
    assert_ne!(nic0.ip, nic1.ip);

    // Security group with a single ingress allow.
    let rule = SecurityRule {
        direction: Direction::Ingress,
        proto: L4Proto::Tcp,
        port_min: 80, port_max: 80,
        cidr: Cidr4::parse("0.0.0.0/1").unwrap(),
        allow: true,
    };
    let sg = match m1.invoke(&n2, VmOp::CreateSecurityGroup {
        name: "web".into(), rules: vec![rule],
    }, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::SecurityGroupCreated { sg } => sg,
        r => panic!("unexpected: {r:?}"),
    };
    assert_eq!(sg.rules.len(), 1);

    // Load balancer pointing to both NIC IPs.
    let lb = match m1.invoke(&n2, VmOp::CreateLoadBalancer {
        name: "frontends".into(),
        network_id: net.id.clone(),
        vip: "10.42.0.254".into(),
        frontend_port: 80,
        algo: LbAlgo::RoundRobin,
        backends: vec![
            LbBackend { vm_id: vm0, ip: nic0.ip, port: 8080 },
            LbBackend { vm_id: vm1, ip: nic1.ip, port: 8080 },
        ],
    }, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::LoadBalancerCreated { lb } => lb,
        r => panic!("unexpected: {r:?}"),
    };
    assert_eq!(lb.backends.len(), 2);

    // Listings round-trip.
    let nets = match m1.invoke(&n2, VmOp::ListNetworks, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::NetworksListed { networks } => networks,
        r => panic!("unexpected: {r:?}"),
    };
    assert_eq!(nets.len(), 1);
    let nics = match m1.invoke(&n2, VmOp::ListNics, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::NicsListed { nics } => nics,
        r => panic!("unexpected: {r:?}"),
    };
    assert_eq!(nics.len(), 2);
    let lbs = match m1.invoke(&n2, VmOp::ListLoadBalancers, Duration::from_millis(2_000)).await.unwrap() {
        VmOpReply::LoadBalancersListed { lbs } => lbs,
        r => panic!("unexpected: {r:?}"),
    };
    assert_eq!(lbs.len(), 1);

    // Cluster report observed from n1 should include n2's data.
    let report = m1.cluster_report(Duration::from_millis(2_000)).await.unwrap();
    let n2_row = report.nodes.iter().find(|r| r.id == n2).expect("n2 row");
    assert!(n2_row.reachable);
    assert_eq!(n2_row.vm_count, 2);
    assert_eq!(n2_row.network_count, 1);

    // Network deletion blocked while NICs exist; detach + retry.
    let r = m1.invoke(&n2, VmOp::DeleteNetwork {
        network_id: net.id.clone(),
    }, Duration::from_millis(2_000)).await;
    assert!(matches!(&r, Err(e) if e.to_string().contains("attached")),
        "expected attached error, got {r:?}");

    let _ = m1.invoke(&n2, VmOp::DetachNic { nic_id: nic0.id }, Duration::from_millis(2_000)).await.unwrap();
    let _ = m1.invoke(&n2, VmOp::DetachNic { nic_id: nic1.id }, Duration::from_millis(2_000)).await.unwrap();
    let _ = m1.invoke(&n2, VmOp::DeleteLoadBalancer { lb_id: lb.id }, Duration::from_millis(2_000)).await.unwrap();
    let _ = m1.invoke(&n2, VmOp::DeleteNetwork { network_id: net.id }, Duration::from_millis(2_000)).await.unwrap();

    let _ = m1.shutdown().await;
    let _ = m2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k8s_personality_provisions_cluster() {
    let (m1, m2, n2) = pair(Capabilities::ALL).await;

    let spec = K8sClusterSpec::new("dev", 3);
    let cluster = K8sCluster::create(&m1, &n2, spec, Duration::from_millis(2_000))
        .await
        .expect("create cluster");
    assert_eq!(cluster.nodes.len(), 4);
    assert_eq!(cluster.workers().len(), 3);
    assert_eq!(cluster.control_plane().len(), 1);
    assert_eq!(cluster.lb.backends.len(), 3);
    // CIDR must contain every NIC and the VIP.
    for n in &cluster.nodes {
        assert!(cluster.network.cidr.contains(n.nic.ip));
    }
    assert!(cluster.network.cidr.contains(cluster.lb.vip));

    // Observability picks it all up.
    let report = m1.cluster_report(Duration::from_millis(2_000)).await.unwrap();
    let n2_row = report.nodes.iter().find(|r| r.id == n2).expect("n2 row");
    assert!(n2_row.reachable);
    assert_eq!(n2_row.vm_count, 4);
    assert_eq!(n2_row.network_count, 1);

    // Tear down via the convenience helper.
    cluster.destroy(&m1, Duration::from_millis(2_000)).await.unwrap();
    let report = m1.cluster_report(Duration::from_millis(2_000)).await.unwrap();
    let n2_row = report.nodes.iter().find(|r| r.id == n2).expect("n2 row");
    assert_eq!(n2_row.network_count, 0);

    let _ = m1.shutdown().await;
    let _ = m2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn networking_capability_denial() {
    // n2 grants only network *read* and base VM lifecycle reads.
    let read_only = Capabilities::VM_LIFECYCLE_READ
        | Capabilities::NETWORK_READ
        | Capabilities::SECGROUP_READ
        | Capabilities::LB_READ;
    let (m1, m2, n2) = pair(read_only).await;

    // List ops succeed.
    assert!(matches!(
        m1.invoke(&n2, VmOp::ListNetworks, Duration::from_millis(2_000)).await.unwrap(),
        VmOpReply::NetworksListed { .. }
    ));

    // Mutations are denied with the stable error string.
    let denied_ops: Vec<VmOp> = vec![
        VmOp::CreateNetwork { name: "x".into(), cidr: "10.0.0.0/24".into() },
        VmOp::DeleteNetwork { network_id: NetworkId::from("n2/n1") },
        VmOp::CreateSecurityGroup { name: "x".into(), rules: vec![] },
        VmOp::CreateLoadBalancer {
            name: "x".into(),
            network_id: NetworkId::from("n2/n1"),
            vip: "10.0.0.10".into(),
            frontend_port: 80,
            algo: LbAlgo::RoundRobin,
            backends: vec![],
        },
    ];
    for op in denied_ops {
        let r = m1.invoke(&n2, op, Duration::from_millis(2_000)).await;
        assert!(
            matches!(&r, Err(e) if e.to_string().contains("capability denied")),
            "expected capability denied, got {r:?}"
        );
    }

    let _ = m1.shutdown().await;
    let _ = m2.shutdown().await;
}
