//! Week-13 end-to-end test: persistent volume survives VM restart
//! and a `FileVolumeStore` backing directory survives mesh restart.
//!
//! Scenario:
//!
//! 1. Start two real-UDP nodes, `n1` (driver) and `n2` (host with a
//!    disk-backed [`celmesh::FileVolumeStore`]).
//! 2. From `n1`, allocate a VM on `n2`, create a volume, attach it,
//!    start the VM, then write a fixed payload to the volume via
//!    [`celmesh::VmOp::WriteVolume`] (the "guest" writing real
//!    persistent data).
//! 3. Take a snapshot. Then deliberately corrupt the live volume.
//!    Verify a `RestoreSnapshot` brings the original payload back.
//! 4. Tear down `n2`'s mesh (and drop its host + vault handle), then
//!    bring it back up against the *same on-disk root*. Without any
//!    other coordination, the recreated `MemVmHost` must see the
//!    pre-existing volume in the manifest, and a fresh `ReadVolume`
//!    must return the original bytes — i.e. the data really survived
//!    a process-restart.
//!
//! All ops travel over the federated mesh exactly the way a real
//! operator would issue them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use celmesh::{
    FileVolumeStore, Mesh, MeshConfig, MemVmHost, NodeId, RestartPolicy, Transport, UdpTransport,
    VmHost, VmOp, VmOpReply, VolumeId, VolumeStore,
};

/// Poll `probe` for up to ~10s. Returns `true` once it succeeds.
async fn wait_until<F, Fut>(mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..400 {
        if probe().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

fn cfg(id: &str, addr: &str, seeds: Vec<String>) -> MeshConfig {
    let mut c = MeshConfig::defaults(id, addr);
    c.seeds = seeds;
    c.gossip_interval = Duration::from_millis(50);
    c.timeout_suspect = Duration::from_millis(250);
    c.timeout_dead = Duration::from_millis(750);
    c.supervisor_interval = Duration::from_secs(60); // disabled for this test
    c
}

/// Allocate a fresh per-test directory under the OS temp dir.
fn fresh_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!("celium-w13-{tag}-{nanos}"));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persistent_volume_survives_restart_and_snapshot_round_trip() {
    let payload: &[u8] = b"Hello, Celium W13!";
    let vol_size: u64 = 64;

    let root = fresh_root("e2e");
    // First lifetime ----------------------------------------------------------
    let n1_id;
    let n2_id;
    let vol_id: VolumeId;
    let snap_id;

    {
        let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
        let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
        let a1 = t1.local_addr();
        let a2 = t2.local_addr();

        let m1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()]), t1.clone()).await.unwrap();
        let m2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()]), t2.clone()).await.unwrap();
        n1_id = NodeId::from("n1");
        n2_id = NodeId::from("n2");

        // n2 hosts a FileVolumeStore rooted at a temp dir.
        let vault = Arc::new(FileVolumeStore::open_or_create(&root).unwrap());
        let host2: Arc<dyn VmHost> = Arc::new(MemVmHost::with_vault(vault));
        m2.set_host(host2.clone()).await;

        // n1 also installs a host so replies have somewhere to land.
        let host1: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
        m1.set_host(host1).await;

        // Seed n2's owner-id by snapshotting once (vault id minting
        // depends on knowing the owner string).
        let _ = host2.snapshot(&n2_id).await;

        // Wait for both nodes to see each other.
        assert!(
            wait_until(|| async { m1.alive_count().await >= 2 && m2.alive_count().await >= 2 })
                .await,
            "cluster failed to converge"
        );

        // ---- 1. Create VM on n2 from n1 ------------------------------------
        let vm_id = match m1
            .invoke(
                &n2_id,
                VmOp::Create {
                    label: "guest".into(),
                    restart_policy: RestartPolicy::Never,
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::Created { vm_id } => vm_id,
            r => panic!("unexpected: {r:?}"),
        };

        // ---- 2. Create volume on n2 ----------------------------------------
        let vol_meta = match m1
            .invoke(
                &n2_id,
                VmOp::CreateVolume {
                    name: "data0".into(),
                    size_bytes: vol_size,
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::VolumeCreated { volume } => volume,
            r => panic!("unexpected: {r:?}"),
        };
        vol_id = vol_meta.id.clone();
        assert_eq!(vol_meta.owner, "n2");

        // ---- 3. Attach + start VM ------------------------------------------
        let _ = m1
            .invoke(
                &n2_id,
                VmOp::AttachVolume {
                    vm_id,
                    volume_id: vol_id.clone(),
                    mount_name: "data0".into(),
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap();
        let _ = m1
            .invoke(&n2_id, VmOp::Start { vm_id }, Duration::from_millis(2_000))
            .await
            .unwrap();

        // ---- 4. Guest-side write -------------------------------------------
        match m1
            .invoke(
                &n2_id,
                VmOp::WriteVolume {
                    volume_id: vol_id.clone(),
                    offset: 0,
                    bytes: payload.to_vec(),
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::VolumeWritten { bytes_written, .. } => {
                assert_eq!(bytes_written, payload.len() as u64);
            }
            r => panic!("unexpected: {r:?}"),
        }

        // Read back through the mesh.
        match m1
            .invoke(
                &n2_id,
                VmOp::ReadVolume {
                    volume_id: vol_id.clone(),
                    offset: 0,
                    len: payload.len() as u64,
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::VolumeData { bytes, .. } => assert_eq!(&bytes, payload),
            r => panic!("unexpected: {r:?}"),
        }

        // ---- 5. Snapshot, then mutate, then restore ------------------------
        let snap_meta = match m1
            .invoke(
                &n2_id,
                VmOp::CreateSnapshot {
                    volume_id: vol_id.clone(),
                    name: "pre-corrupt".into(),
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::SnapshotCreated { snapshot } => snapshot,
            r => panic!("unexpected: {r:?}"),
        };
        snap_id = snap_meta.id.clone();
        assert_eq!(snap_meta.volume, vol_id);

        // Corrupt the live volume.
        let _ = m1
            .invoke(
                &n2_id,
                VmOp::WriteVolume {
                    volume_id: vol_id.clone(),
                    offset: 0,
                    bytes: vec![0xFFu8; payload.len()],
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap();

        // Restore from snapshot.
        let _ = m1
            .invoke(
                &n2_id,
                VmOp::RestoreSnapshot {
                    snapshot_id: snap_id.clone(),
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap();

        match m1
            .invoke(
                &n2_id,
                VmOp::ReadVolume {
                    volume_id: vol_id.clone(),
                    offset: 0,
                    len: payload.len() as u64,
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::VolumeData { bytes, .. } => {
                assert_eq!(&bytes, payload, "snapshot restore did not return original bytes");
            }
            r => panic!("unexpected: {r:?}"),
        }

        // ---- 6. Stop the VM, then tear n2 down completely. -----------------
        let _ = m1
            .invoke(&n2_id, VmOp::Stop { vm_id }, Duration::from_millis(2_000))
            .await
            .unwrap();

        let _ = m1.shutdown().await;
        let _ = m2.shutdown().await;
    }

    // Second lifetime — *new* mesh, *new* MemVmHost, but the same
    // on-disk vault root. The persistent payload must still be there,
    // and the snapshot taken in the previous lifetime must still be
    // listable / restorable.
    {
        let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
        let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
        let a1 = t1.local_addr();
        let a2 = t2.local_addr();

        let m1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()]), t1.clone()).await.unwrap();
        let m2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()]), t2.clone()).await.unwrap();

        let vault = Arc::new(FileVolumeStore::open_or_create(&root).unwrap());
        // Confirm the manifest came back populated.
        assert_eq!(vault.list().len(), 1, "expected the W12 volume to persist");
        assert_eq!(vault.list_snapshots(None).len(), 1, "expected snapshot to persist");

        let host2: Arc<dyn VmHost> = Arc::new(MemVmHost::with_vault(vault));
        m2.set_host(host2.clone()).await;
        let host1: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
        m1.set_host(host1).await;
        // Re-seed owner so any future create-volume keeps minting `n2/...` ids.
        let _ = host2.snapshot(&n2_id).await;

        assert!(
            wait_until(|| async { m1.alive_count().await >= 2 && m2.alive_count().await >= 2 })
                .await,
            "cluster failed to converge after restart"
        );

        // Read straight from the persisted volume.
        match m1
            .invoke(
                &n2_id,
                VmOp::ReadVolume {
                    volume_id: vol_id.clone(),
                    offset: 0,
                    len: payload.len() as u64,
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::VolumeData { bytes, .. } => {
                assert_eq!(&bytes, payload, "data did not survive process restart");
            }
            r => panic!("unexpected: {r:?}"),
        }

        // Snapshot is still listable post-restart.
        match m1
            .invoke(
                &n2_id,
                VmOp::ListSnapshots {
                    volume_id: Some(vol_id.clone()),
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::SnapshotsListed { snapshots } => {
                assert_eq!(snapshots.len(), 1);
                assert_eq!(snapshots[0].id, snap_id);
            }
            r => panic!("unexpected: {r:?}"),
        }

        // And restore-after-mutation still works.
        let _ = m1
            .invoke(
                &n2_id,
                VmOp::WriteVolume {
                    volume_id: vol_id.clone(),
                    offset: 0,
                    bytes: vec![0u8; payload.len()],
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap();
        let _ = m1
            .invoke(
                &n2_id,
                VmOp::RestoreSnapshot {
                    snapshot_id: snap_id.clone(),
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap();
        match m1
            .invoke(
                &n2_id,
                VmOp::ReadVolume {
                    volume_id: vol_id.clone(),
                    offset: 0,
                    len: payload.len() as u64,
                },
                Duration::from_millis(2_000),
            )
            .await
            .unwrap()
        {
            VmOpReply::VolumeData { bytes, .. } => assert_eq!(&bytes, payload),
            r => panic!("unexpected: {r:?}"),
        }

        let _ = m1.shutdown().await;
        let _ = m2.shutdown().await;
    }

    let _ = std::fs::remove_dir_all(&root);
    // Reference the unused id to silence dead-code lints.
    let _ = n1_id;
}

/// Reject reads/writes that exceed the per-call cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn volume_io_chunk_size_is_capped() {
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();

    let m1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()]), t1.clone()).await.unwrap();
    let m2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()]), t2.clone()).await.unwrap();
    let n2 = NodeId::from("n2");

    let host2: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    m2.set_host(host2.clone()).await;
    let _ = host2.snapshot(&n2).await;

    assert!(
        wait_until(|| async { m1.alive_count().await >= 2 && m2.alive_count().await >= 2 }).await
    );

    let vol = match m1
        .invoke(
            &n2,
            VmOp::CreateVolume {
                name: "huge".into(),
                size_bytes: 64 * 1024,
            },
            Duration::from_millis(2_000),
        )
        .await
        .unwrap()
    {
        VmOpReply::VolumeCreated { volume } => volume,
        r => panic!("unexpected: {r:?}"),
    };

    // Asking for 33 KiB exceeds the 32 KiB per-op cap.
    let r = m1
        .invoke(
            &n2,
            VmOp::ReadVolume {
                volume_id: vol.id.clone(),
                offset: 0,
                len: 33 * 1024,
            },
            Duration::from_millis(2_000),
        )
        .await;
    assert!(r.is_err(), "expected oversized read to be rejected");

    let _ = m1.shutdown().await;
    let _ = m2.shutdown().await;
}
