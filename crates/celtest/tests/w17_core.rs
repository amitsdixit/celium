//! Week-17 — Core Layer hardening tests.
//!
//! Five scenarios that exercise the W17 polish work end-to-end:
//!
//! 1. **`mesh_metrics_increment_under_real_traffic`** — drive a
//!    two-node UDP cluster through gossip + an RPC and verify the
//!    counter snapshot reflects what actually crossed the wire.
//! 2. **`mesh_join_heals_isolated_node`** — start two nodes with
//!    no seeds, then call [`celmesh::Mesh::join`] from one of them
//!    and confirm membership converges + the join counter ticks.
//! 3. **`rpc_timeout_returns_celerror_timeout`** — point a node at
//!    a target whose membership row is fresh but whose underlying
//!    UDP socket has been dropped; the call must surface as
//!    `CelError::Timeout` (W17 added the variant) rather than the
//!    historical `Io("rpc timed out")` shape.
//! 4. **`mem_volume_store_w17_surface`** — exercise `flush`,
//!    `stats`, and `integrity_check` on the in-memory store. The
//!    in-memory store's `flush` is a no-op but must still succeed,
//!    and stats / integrity_check must return accurate counts.
//! 5. **`file_volume_store_w17_surface`** — same shape against the
//!    on-disk store, plus a deliberate corruption check that
//!    `integrity_check` flags torn body files.

use std::sync::Arc;
use std::time::Duration;

use celcommon::CelError;
use celmesh::{
    Capabilities, FileVolumeStore, IntegrityReport, MemVolumeStore, Mesh, MeshConfig,
    MemVmHost, NodeId, RestartPolicy, Transport, UdpTransport, VmHost, VmOp, VmOpReply,
    VolumeStore,
};

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

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

fn tmp_root(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!("celium-w17-{label}-{nanos}"));
    p
}

// ---------------------------------------------------------------------------
// 1. Mesh metrics under real UDP traffic.
// ---------------------------------------------------------------------------

// `#[ignore]`: UDP test. Skipped under default `cargo test --workspace`
// to keep the parallel run stable on Windows; run with
// `cargo test -p celtest --test w17_core -- --include-ignored`.
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mesh_metrics_increment_under_real_traffic() {
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();
    let m1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()]), t1).await.unwrap();
    let m2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()]), t2).await.unwrap();

    let host1: Arc<dyn VmHost> = Arc::new(MemVmHost::new().with_caps(Capabilities::ALL));
    let host2: Arc<dyn VmHost> = Arc::new(MemVmHost::new().with_caps(Capabilities::ALL));
    m1.set_host(host1).await;
    m2.set_host(host2).await;

    assert!(
        wait_until(|| async {
            m1.alive_count().await >= 2 && m2.alive_count().await >= 2
        }).await,
        "two-node cluster failed to converge"
    );

    // Snapshot before the explicit RPC so we can assert it ticked
    // rpc_in / rpc_out exactly once each.
    let pre = m1.metrics();
    let n2 = NodeId::from("n2");
    let reply = m1.invoke(&n2, VmOp::Create {
        label: "alpha".into(),
        restart_policy: RestartPolicy::Never,
    }, Duration::from_secs(2)).await.expect("rpc must succeed");
    assert!(matches!(reply, VmOpReply::Created { .. }), "unexpected reply: {reply:?}");

    // Give the receiver time to update its counters.
    tokio::time::sleep(Duration::from_millis(120)).await;

    let post1 = m1.metrics();
    let post2 = m2.metrics();

    // n1 issued at least one outbound RPC.
    assert!(
        post1.rpc_out > pre.rpc_out,
        "rpc_out did not advance: pre={} post={}", pre.rpc_out, post1.rpc_out,
    );
    // n2 saw at least one inbound RPC.
    assert!(post2.rpc_in >= 1, "n2.rpc_in == 0: {post2:?}");
    // Both sides have shipped Hello + at least one Sync each.
    assert!(post1.gossip_sent >= 2, "n1.gossip_sent: {post1:?}");
    assert!(post2.gossip_sent >= 2, "n2.gossip_sent: {post2:?}");
    assert!(post1.gossip_recv >= 1, "n1.gossip_recv: {post1:?}");
    assert!(post2.gossip_recv >= 1, "n2.gossip_recv: {post2:?}");
    // No timeouts on the happy path.
    assert_eq!(post1.rpc_timeouts, 0);
    assert_eq!(post2.rpc_timeouts, 0);

    // Prometheus rendering at least mentions every counter family.
    let txt = m1.metrics_prometheus();
    for k in [
        "celmesh_gossip_sent_total",
        "celmesh_gossip_recv_total",
        "celmesh_rpc_in_total",
        "celmesh_rpc_out_total",
        "celmesh_rpc_timeouts_total",
        "celmesh_dead_promotions_total",
        "celmesh_join_calls_total",
    ] {
        assert!(txt.contains(k), "prometheus dump missing {k}:\n{txt}");
    }

    let _ = m1.shutdown().await;
    let _ = m2.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. Runtime join heals an isolated node.
// ---------------------------------------------------------------------------

#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mesh_join_heals_isolated_node() {
    // Start with no seeds — neither node knows the other yet.
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();
    let m1 = Mesh::start(cfg("n1", &a1, Vec::new()), t1).await.unwrap();
    let m2 = Mesh::start(cfg("n2", &a2, Vec::new()), t2).await.unwrap();

    // Brief wait so both gossipers have ticked at least once with
    // empty seed lists. They should still see only themselves.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(m1.alive_count().await, 1);
    assert_eq!(m2.alive_count().await, 1);

    // Heal at runtime.
    m1.join(a2.clone()).await.expect("join must succeed");

    assert!(
        wait_until(|| async {
            m1.alive_count().await >= 2 && m2.alive_count().await >= 2
        }).await,
        "cluster did not converge after runtime join"
    );

    let s = m1.metrics();
    assert!(s.join_calls >= 1, "join counter did not tick: {s:?}");

    let _ = m1.shutdown().await;
    let _ = m2.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3. RPC timeout surfaces as CelError::Timeout.
// ---------------------------------------------------------------------------

#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_timeout_returns_celerror_timeout() {
    // Use UDP so a `send` always succeeds at the kernel layer —
    // the datagram is dropped silently by the network stack once
    // the peer's receiver is gone, which is exactly the scenario
    // we want the timeout path to handle. We tear down n2's mesh
    // tasks (`shutdown`) after convergence; n1's membership still
    // holds n2 as Alive until the failure detector kicks in, so
    // the in-flight RPC sees the silent void.
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();
    // Generous suspect/dead timeouts so n2 stays Alive for the
    // duration of the test even after we abort its tasks.
    let mut c1 = MeshConfig::defaults("a", &a1);
    c1.seeds = vec![a2.clone()];
    c1.gossip_interval = Duration::from_millis(50);
    c1.timeout_suspect = Duration::from_secs(60);
    c1.timeout_dead    = Duration::from_secs(120);
    let mut c2 = MeshConfig::defaults("b", &a2);
    c2.seeds = vec![a1.clone()];
    c2.gossip_interval = Duration::from_millis(50);
    c2.timeout_suspect = Duration::from_secs(60);
    c2.timeout_dead    = Duration::from_secs(120);
    let m1 = Mesh::start(c1, t1).await.unwrap();
    let m2 = Mesh::start(c2, t2).await.unwrap();

    let host: Arc<dyn VmHost> = Arc::new(MemVmHost::new().with_caps(Capabilities::ALL));
    m1.set_host(host).await;

    assert!(wait_until(|| async {
        m1.alive_count().await >= 2 && m2.alive_count().await >= 2
    }).await, "cluster failed to converge");

    // Abort n2's gossip + receiver tasks. Goodbye is *not* sent so
    // n1 keeps n2 Alive in its view.
    m2.shutdown().await.expect("shutdown ok");

    let pre = m1.metrics();
    let res = m1.invoke(
        &NodeId::from("b"),
        VmOp::List,
        Duration::from_millis(150),
    ).await;
    let err = res.expect_err("rpc must time out when peer is silent");
    match err {
        CelError::Timeout(s) => assert!(s.contains("rpc"), "unexpected timeout msg: {s}"),
        other => panic!("expected CelError::Timeout, got {other:?}"),
    }
    let post = m1.metrics();
    assert!(
        post.rpc_timeouts > pre.rpc_timeouts,
        "rpc_timeouts did not advance: pre={} post={}",
        pre.rpc_timeouts, post.rpc_timeouts,
    );

    let _ = m1.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. MemVolumeStore W17 surface.
// ---------------------------------------------------------------------------

#[test]
fn mem_volume_store_w17_surface() {
    let s = MemVolumeStore::new();
    let v1 = s.create("n1", "data", 64).unwrap();
    let v2 = s.create("n1", "more", 32).unwrap();
    s.write(&v1.id, 0, b"hello").unwrap();
    let _ = s.create_snapshot(&v1.id, "snap-a").unwrap();
    let _ = s.create_snapshot(&v1.id, "snap-b").unwrap();

    // flush is a no-op for Mem but must succeed.
    s.flush().expect("mem flush must succeed");

    // stats reflects size + snapshot accounting.
    let st = s.stats(&v1.id).unwrap();
    assert_eq!(st.id, v1.id);
    assert_eq!(st.size_bytes, 64);
    assert_eq!(st.snapshot_count, 2);
    assert_eq!(st.total_snapshot_bytes, 128); // two 64-byte snapshots
    let st2 = s.stats(&v2.id).unwrap();
    assert_eq!(st2.snapshot_count, 0);
    assert_eq!(st2.total_snapshot_bytes, 0);

    // Integrity check on a healthy store is clean.
    let rep = s.integrity_check().unwrap();
    assert!(rep.is_clean(), "expected clean report, got {rep:?}");
    assert_eq!(rep.volumes_checked, 2);
    assert_eq!(rep.snapshots_checked, 2);

    // Unknown stats target errors with Invalid.
    assert!(matches!(
        s.stats(&celmesh::VolumeId::from("nope/v9")),
        Err(CelError::Invalid(_))
    ));
}

// ---------------------------------------------------------------------------
// 5. FileVolumeStore W17 surface — including a torn-body fault.
// ---------------------------------------------------------------------------

#[test]
fn file_volume_store_w17_surface() {
    let root = tmp_root("file-vault");
    let s = FileVolumeStore::open_or_create(&root).unwrap();
    let v = s.create("n1", "data", 16).unwrap();
    s.write(&v.id, 0, b"hello world!!!!!").unwrap();
    let _ = s.create_snapshot(&v.id, "ok").unwrap();

    // flush returns Ok and is durable: a follow-up reopen sees the
    // same data.
    s.flush().expect("file flush must succeed");
    drop(s);
    let s = FileVolumeStore::open_or_create(&root).unwrap();
    let bytes = s.read(&v.id, 0, 16).unwrap();
    assert_eq!(&bytes, b"hello world!!!!!");

    // stats works through the trait default; integrity_check uses
    // the disk-aware override.
    let st = s.stats(&v.id).unwrap();
    assert_eq!(st.size_bytes, 16);
    assert_eq!(st.snapshot_count, 1);

    let rep: IntegrityReport = s.integrity_check().unwrap();
    assert!(rep.is_clean(), "expected clean report, got {rep:?}");

    // Now deliberately tear the body file: shrink it to 8 bytes so
    // the on-disk length disagrees with the manifest. The override
    // must flag this without ever touching the bytes themselves.
    drop(s);
    let body_path = root.join("volumes").join("n1_v1.bin");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&body_path)
        .expect("open body for truncation");
    f.set_len(8).expect("truncate body");
    drop(f);
    let s = FileVolumeStore::open_or_create(&root).unwrap();
    let rep = s.integrity_check().unwrap();
    assert!(!rep.is_clean(), "torn body should flag integrity_check");
    assert!(
        rep.errors.iter().any(|e| e.contains("on-disk len")),
        "expected on-disk-len error, got {:?}", rep.errors,
    );

    let _ = std::fs::remove_dir_all(&root);
}
