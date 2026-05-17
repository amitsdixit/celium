//! W20 Phase C — End-to-end hardening of the image-metadata
//! gossip path.
//!
//! W18.4 added four optional fields to [`celmesh::RemoteVm`]
//! (`image_path`, `cpu_count`, `memory_mib`, `boot_blob_crc32c`) so
//! that operators can see, on any node, which disk image backs a
//! given VM and whether the staged boot blob still matches its
//! original digest. W19-A wired drift detection into the local
//! controller. W20-C asserts the cross-node story: those fields must
//! travel verbatim through the actual gossip plane and a fresh
//! digest (modelling a re-stage on the owner) must overtake the old
//! one on every peer.
//!
//! This is the comprehensive E2E test promised by the W20 plan. It
//! drives the same public surface the CLI does — `Mesh`, `RemoteVm`,
//! `MeshConfig`, `MemTransportFactory` — so any regression in the
//! wire shape, the federation merge, or HLC ordering is caught
//! here.

use std::sync::Arc;
use std::time::Duration;

use celmesh::{Mesh, MeshConfig, MemTransportFactory, NodeId, RemoteVm};

/// Build a three-node mesh wired through `MemTransport`. Mirrors
/// the helper in `multi_node.rs` so this test can run standalone.
async fn cluster3() -> (Mesh, Mesh, Mesh, MemTransportFactory) {
    let factory = MemTransportFactory::new();
    let ta = Arc::new(factory.bind("mem://w20-a").await.unwrap());
    let tb = Arc::new(factory.bind("mem://w20-b").await.unwrap());
    let tc = Arc::new(factory.bind("mem://w20-c").await.unwrap());

    let mk = |id: &str, addr: &str, seeds: Vec<&str>| {
        let mut c = MeshConfig::defaults(id, addr);
        c.seeds           = seeds.into_iter().map(str::to_string).collect();
        c.gossip_interval = Duration::from_millis(20);
        c.timeout_suspect = Duration::from_millis(200);
        c.timeout_dead    = Duration::from_millis(500);
        c
    };

    let a = Mesh::start(
        mk("a", "mem://w20-a", vec!["mem://w20-b", "mem://w20-c"]),
        ta,
    ).await.unwrap();
    let b = Mesh::start(mk("b", "mem://w20-b", vec!["mem://w20-a"]), tb).await.unwrap();
    let c = Mesh::start(mk("c", "mem://w20-c", vec!["mem://w20-a"]), tc).await.unwrap();
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

/// Construct a `RemoteVm` carrying every W18.4 field populated. The
/// `owner`/`epoch`/`hlc`/`owner_alive` triple is filled in by
/// `publish_local_vms` so we leave it at sensible defaults here.
fn vm_with_image(label: &str, image: &str, cpu: u32, mem: u64, crc: u32) -> RemoteVm {
    RemoteVm {
        owner:           NodeId::from(""),
        vm_id:           0,
        label:           label.into(),
        state:           "halted".into(),
        last_exit:       Some(12), // matches the controller's HLT convention
        epoch:           0,
        hlc:             0,
        owner_alive:     false,
        restart_policy:  celmesh::RestartPolicy::Never,
        volumes:         Vec::new(),
        image_path:      Some(image.into()),
        cpu_count:       Some(cpu),
        memory_mib:      Some(mem),
        boot_blob_crc32c: Some(crc),
    }
}

#[tokio::test]
async fn image_metadata_propagates_to_every_peer() {
    let (a, b, c, _f) = cluster3().await;

    // Cluster forms first; then we publish the row.
    assert!(wait_until(|| async { a.alive_count().await == 3 }).await);

    let row = vm_with_image(
        "drift-source",
        "/var/lib/celium/images/golden.raw",
        4,
        2048,
        0x1234_5678,
    );
    a.publish_local_vms(vec![row]).await.unwrap();

    // Both peers must observe the freshly-published row with every
    // image field intact. We poll because gossip is asynchronous.
    let saw_on_b = wait_until(|| async {
        b.list_vms()
            .await
            .iter()
            .any(|r| r.owner.as_str() == "a" && r.boot_blob_crc32c == Some(0x1234_5678))
    })
    .await;
    let saw_on_c = wait_until(|| async {
        c.list_vms()
            .await
            .iter()
            .any(|r| r.owner.as_str() == "a" && r.boot_blob_crc32c == Some(0x1234_5678))
    })
    .await;
    assert!(saw_on_b, "image metadata did not propagate to b");
    assert!(saw_on_c, "image metadata did not propagate to c");

    let on_b = b.list_vms().await
        .into_iter()
        .find(|r| r.owner.as_str() == "a")
        .expect("a/0 must be visible on b");
    assert_eq!(on_b.image_path.as_deref(), Some("/var/lib/celium/images/golden.raw"));
    assert_eq!(on_b.cpu_count,             Some(4));
    assert_eq!(on_b.memory_mib,            Some(2048));
    assert_eq!(on_b.boot_blob_crc32c,      Some(0x1234_5678));
    assert_eq!(on_b.path(),                "/cluster/a/vms/0");

    let on_c = c.list_vms().await
        .into_iter()
        .find(|r| r.owner.as_str() == "a")
        .expect("a/0 must be visible on c");
    assert_eq!(on_c.image_path.as_deref(), Some("/var/lib/celium/images/golden.raw"));
    assert_eq!(on_c.cpu_count,             Some(4));
    assert_eq!(on_c.memory_mib,            Some(2048));
    assert_eq!(on_c.boot_blob_crc32c,      Some(0x1234_5678));

    let _ = a.shutdown().await;
    let _ = b.shutdown().await;
    let _ = c.shutdown().await;
}

#[tokio::test]
async fn boot_blob_digest_update_overtakes_previous_value() {
    // Models the operator workflow:
    //   1. Owner stages a blob → publishes crc = X.
    //   2. Owner detects image content change, re-stages → crc = Y.
    //   3. Every peer must converge on Y (LWW on (epoch, hlc) is
    //      handled inside the federation merge; we exercise it).
    let (a, b, c, _f) = cluster3().await;
    assert!(wait_until(|| async { a.alive_count().await == 3 }).await);

    let v1 = vm_with_image("drift", "/img/v1.raw", 2, 1024, 0xAAAA_AAAA);
    a.publish_local_vms(vec![v1]).await.unwrap();
    assert!(
        wait_until(|| async {
            b.list_vms()
                .await
                .iter()
                .any(|r| r.owner.as_str() == "a" && r.boot_blob_crc32c == Some(0xAAAA_AAAA))
        })
        .await,
        "first digest did not propagate to b",
    );

    // Re-publish with a different digest. `publish_local_vms`
    // bumps the HLC so this is strictly newer than the v1 row.
    let v2 = vm_with_image("drift", "/img/v1.raw", 2, 1024, 0xBBBB_BBBB);
    a.publish_local_vms(vec![v2]).await.unwrap();

    let updated_b = wait_until(|| async {
        b.list_vms()
            .await
            .iter()
            .any(|r| r.owner.as_str() == "a" && r.boot_blob_crc32c == Some(0xBBBB_BBBB))
    })
    .await;
    let updated_c = wait_until(|| async {
        c.list_vms()
            .await
            .iter()
            .any(|r| r.owner.as_str() == "a" && r.boot_blob_crc32c == Some(0xBBBB_BBBB))
    })
    .await;
    assert!(updated_b, "updated digest did not overtake on b");
    assert!(updated_c, "updated digest did not overtake on c");

    // And critically, the *old* digest is gone — LWW, not duplicate.
    let rows_b: Vec<_> = b
        .list_vms()
        .await
        .into_iter()
        .filter(|r| r.owner.as_str() == "a")
        .collect();
    assert_eq!(rows_b.len(), 1, "duplicate rows on b: {rows_b:?}");
    assert_eq!(rows_b[0].boot_blob_crc32c, Some(0xBBBB_BBBB));

    let _ = a.shutdown().await;
    let _ = b.shutdown().await;
    let _ = c.shutdown().await;
}

#[tokio::test]
async fn owner_departure_preserves_image_fields_for_diagnosis() {
    // When the image-owner leaves the cluster, peers retain its
    // last-known row (with `owner_alive=false`). Operators rely on
    // that to diagnose "where did node a's golden image come from?"
    // *after* a has gone away. Regression guard for the W18.4 fields
    // surviving the post-departure pruning logic.
    let (a, b, c, _f) = cluster3().await;
    assert!(wait_until(|| async { a.alive_count().await == 3 }).await);

    let row = vm_with_image("ghost", "/img/post-mortem.raw", 8, 4096, 0xDEAD_BEEF);
    a.publish_local_vms(vec![row]).await.unwrap();
    assert!(
        wait_until(|| async {
            b.list_vms()
                .await
                .iter()
                .any(|r| r.owner.as_str() == "a" && r.boot_blob_crc32c == Some(0xDEAD_BEEF))
        })
        .await,
        "row did not reach b before shutdown",
    );

    // Take a out cleanly. b and c will downgrade it to Left/Dead.
    let _ = a.shutdown().await;
    let owner_gone = wait_until(|| async { b.alive_count().await < 3 }).await;
    assert!(owner_gone, "b did not notice a's departure");

    let view_b: Vec<_> = b
        .list_vms()
        .await
        .into_iter()
        .filter(|r| r.owner.as_str() == "a")
        .collect();
    assert_eq!(view_b.len(), 1, "post-departure row missing on b");
    assert!(!view_b[0].owner_alive, "row should be owner_alive=false");
    assert_eq!(view_b[0].image_path.as_deref(), Some("/img/post-mortem.raw"));
    assert_eq!(view_b[0].cpu_count,             Some(8));
    assert_eq!(view_b[0].memory_mib,            Some(4096));
    assert_eq!(view_b[0].boot_blob_crc32c,      Some(0xDEAD_BEEF));

    let _ = b.shutdown().await;
    let _ = c.shutdown().await;
}
