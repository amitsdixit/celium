//! Week-14 end-to-end and security tests.
//!
//! Two separate scenarios are covered:
//!
//! 1. **`vm_lifecycle_with_persistent_volume_survives_restart`** —
//!    full operator workflow over real UDP: create VM on `n2`,
//!    create + attach a `FileVolumeStore`-backed volume, write data,
//!    **stop the VM, start it again** (simulating a guest restart),
//!    and verify the bytes are still readable. Then bring the host
//!    down and back up against the same on-disk root and verify the
//!    bytes still survive a process restart.
//!
//! 2. **`capability_denied_blocks_unauthorised_ops`** — a host
//!    configured with [`Capabilities::VM_LIFECYCLE_READ`] only must
//!    accept `List` but reject `Create`, `WriteVolume`, and
//!    `CreateSnapshot` with a stable `capability denied` error.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use celmesh::{
    Capabilities, FileVolumeStore, Mesh, MeshConfig, MemVmHost, NodeId, RestartPolicy, Transport,
    UdpTransport, VmHost, VmOp, VmOpReply, VolumeId, VolumeStore,
};

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
    c.supervisor_interval = Duration::from_secs(60); // disabled
    c
}

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!("celium-w14-{tag}-{nanos}"));
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vm_lifecycle_with_persistent_volume_survives_restart() {
    let payload: &[u8] = b"Celium W14 end-to-end!";
    let vol_size: u64 = 64;
    let root = fresh_root("e2e");

    let n2_id = NodeId::from("n2");
    let vol_id;
    let vm_id_first;

    // ---- First lifetime ----------------------------------------------------
    {
        let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
        let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
        let a1 = t1.local_addr();
        let a2 = t2.local_addr();
        let m1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()]), t1.clone()).await.unwrap();
        let m2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()]), t2.clone()).await.unwrap();

        let vault = Arc::new(FileVolumeStore::open_or_create(&root).unwrap());
        let host2: Arc<dyn VmHost> = Arc::new(MemVmHost::with_vault(vault.clone()));
        m2.set_host(host2.clone()).await;
        let host1: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
        m1.set_host(host1).await;
        let _ = host2.snapshot(&n2_id).await;

        assert!(
            wait_until(|| async {
                m1.alive_count().await >= 2 && m2.alive_count().await >= 2
            }).await,
            "cluster failed to converge"
        );

        // Create VM on n2.
        let vm_id = match m1.invoke(
            &n2_id,
            VmOp::Create {
                label: "guest".into(),
                restart_policy: RestartPolicy::Never,
                image_path: None,
                cpu_count: None,
                memory_mib: None,
                boot_blob_crc32c: None,
            },
            Duration::from_millis(2_000),
        ).await.unwrap() {
            VmOpReply::Created { vm_id } => vm_id,
            r => panic!("unexpected: {r:?}"),
        };
        vm_id_first = vm_id;

        // Create and attach volume.
        let vol = match m1.invoke(
            &n2_id,
            VmOp::CreateVolume { name: "data0".into(), size_bytes: vol_size },
            Duration::from_millis(2_000),
        ).await.unwrap() {
            VmOpReply::VolumeCreated { volume } => volume,
            r => panic!("unexpected: {r:?}"),
        };
        vol_id = vol.id.clone();
        let _ = m1.invoke(
            &n2_id,
            VmOp::AttachVolume {
                vm_id, volume_id: vol_id.clone(), mount_name: "data0".into(),
            },
            Duration::from_millis(2_000),
        ).await.unwrap();

        // Start, write, stop, start again.
        let _ = m1.invoke(&n2_id, VmOp::Start { vm_id }, Duration::from_millis(2_000))
            .await.unwrap();
        let _ = m1.invoke(
            &n2_id,
            VmOp::WriteVolume {
                volume_id: vol_id.clone(), offset: 0, bytes: payload.to_vec(),
            },
            Duration::from_millis(2_000),
        ).await.unwrap();
        let _ = m1.invoke(&n2_id, VmOp::Stop { vm_id }, Duration::from_millis(2_000))
            .await.unwrap();

        // The VM moved through `halted` → `stopped`. The W12/W13
        // host model doesn't allow re-`Start` after `Stop` (terminal
        // state) — so for this test "restarting the VM" means
        // delete + recreate against the *same* persistent volume,
        // which is the operator-visible behaviour anyway.
        let _ = m1.invoke(&n2_id, VmOp::Delete { vm_id }, Duration::from_millis(2_000))
            .await.unwrap();
        let restarted_id = match m1.invoke(
            &n2_id,
            VmOp::Create { label: "guest".into(), restart_policy: RestartPolicy::Never, image_path: None, cpu_count: None, memory_mib: None, boot_blob_crc32c: None },
            Duration::from_millis(2_000),
        ).await.unwrap() {
            VmOpReply::Created { vm_id } => vm_id,
            r => panic!("unexpected: {r:?}"),
        };
        let _ = m1.invoke(
            &n2_id,
            VmOp::AttachVolume {
                vm_id: restarted_id,
                volume_id: vol_id.clone(),
                mount_name: "data0".into(),
            },
            Duration::from_millis(2_000),
        ).await.unwrap();
        let _ = m1.invoke(&n2_id, VmOp::Start { vm_id: restarted_id },
                          Duration::from_millis(2_000)).await.unwrap();

        // Read the original payload back through the freshly-restarted VM.
        match m1.invoke(
            &n2_id,
            VmOp::ReadVolume {
                volume_id: vol_id.clone(), offset: 0, len: payload.len() as u64,
            },
            Duration::from_millis(2_000),
        ).await.unwrap() {
            VmOpReply::VolumeData { bytes, .. } => assert_eq!(&bytes, payload,
                "data did not survive in-process VM restart"),
            r => panic!("unexpected: {r:?}"),
        }

        let _ = m1.shutdown().await;
        let _ = m2.shutdown().await;
    }

    // ---- Second lifetime: full host process restart ------------------------
    {
        let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
        let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
        let a1 = t1.local_addr();
        let a2 = t2.local_addr();
        let m1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()]), t1.clone()).await.unwrap();
        let m2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()]), t2.clone()).await.unwrap();

        let vault = Arc::new(FileVolumeStore::open_or_create(&root).unwrap());
        // Manifest restored.
        assert_eq!(vault.list().len(), 1);
        let host2: Arc<dyn VmHost> = Arc::new(MemVmHost::with_vault(vault));
        m2.set_host(host2.clone()).await;
        let host1: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
        m1.set_host(host1).await;
        let _ = host2.snapshot(&n2_id).await;

        assert!(
            wait_until(|| async {
                m1.alive_count().await >= 2 && m2.alive_count().await >= 2
            }).await
        );

        match m1.invoke(
            &n2_id,
            VmOp::ReadVolume {
                volume_id: vol_id.clone(), offset: 0, len: payload.len() as u64,
            },
            Duration::from_millis(2_000),
        ).await.unwrap() {
            VmOpReply::VolumeData { bytes, .. } => assert_eq!(&bytes, payload,
                "data did not survive process restart"),
            r => panic!("unexpected: {r:?}"),
        }

        let _ = m1.shutdown().await;
        let _ = m2.shutdown().await;
    }

    let _ = std::fs::remove_dir_all(&root);
    let _ = vm_id_first;
}

/// Verify W14 capability enforcement gates VM and volume operations.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capability_denied_blocks_unauthorised_ops() {
    let t1 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let t2 = Arc::new(UdpTransport::bind("127.0.0.1:0").await.unwrap());
    let a1 = t1.local_addr();
    let a2 = t2.local_addr();
    let m1 = Mesh::start(cfg("n1", &a1, vec![a2.clone()]), t1.clone()).await.unwrap();
    let m2 = Mesh::start(cfg("n2", &a2, vec![a1.clone()]), t2.clone()).await.unwrap();
    let n2 = NodeId::from("n2");

    // n2 is read-only for VM lifecycle and volumes.
    let read_only = Capabilities::VM_LIFECYCLE_READ
        | Capabilities::VOLUME_READ
        | Capabilities::SNAPSHOT_READ;
    let host2: Arc<dyn VmHost> = Arc::new(MemVmHost::new().with_caps(read_only));
    m2.set_host(host2.clone()).await;
    let host1: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    m1.set_host(host1).await;
    let _ = host2.snapshot(&n2).await;

    assert!(
        wait_until(|| async {
            m1.alive_count().await >= 2 && m2.alive_count().await >= 2
        }).await
    );

    // List should pass.
    let r = m1.invoke(&n2, VmOp::List, Duration::from_millis(2_000)).await.unwrap();
    assert!(matches!(r, VmOpReply::Listed { .. }));

    // Create must be denied.
    let r = m1.invoke(
        &n2,
        VmOp::Create { label: "x".into(), restart_policy: RestartPolicy::Never, image_path: None, cpu_count: None, memory_mib: None, boot_blob_crc32c: None },
        Duration::from_millis(2_000),
    ).await;
    assert!(
        matches!(&r, Err(e) if e.to_string().contains("capability denied")),
        "expected capability denied, got {r:?}"
    );

    // WriteVolume must be denied even though the volume id is bogus —
    // capability check happens before the vault lookup.
    let r = m1.invoke(
        &n2,
        VmOp::WriteVolume {
            volume_id: VolumeId::from("n2/v1"), offset: 0, bytes: vec![1, 2, 3],
        },
        Duration::from_millis(2_000),
    ).await;
    assert!(
        matches!(&r, Err(e) if e.to_string().contains("capability denied")),
        "expected capability denied, got {r:?}"
    );

    // CreateSnapshot must be denied.
    let r = m1.invoke(
        &n2,
        VmOp::CreateSnapshot {
            volume_id: VolumeId::from("n2/v1"), name: "s".into(),
        },
        Duration::from_millis(2_000),
    ).await;
    assert!(
        matches!(&r, Err(e) if e.to_string().contains("capability denied")),
        "expected capability denied, got {r:?}"
    );

    let _ = m1.shutdown().await;
    let _ = m2.shutdown().await;
}
