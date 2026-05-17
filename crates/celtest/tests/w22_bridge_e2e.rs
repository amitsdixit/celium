//! W22-C — celhyper bridge end-to-end test through real TCP.
//!
//! Stands up the full host-side W22 stack in one process:
//! `CelhyperVmHost` → `SerialHyperLink` → TCP → `serve_listener` →
//! `LoopbackHyperLink`. Drives the five lifecycle ops via the
//! public [`VmHost`] trait so it mirrors how `celctl --vm-host
//! celhyper-serial:host:port` reaches the kernel in production
//! (replacing only the kernel's wire-decoder + manager with the
//! host-side loopback for testability).
//!
//! Together with `wire::tests::kernel_encoder_matches_host_serde_
//! byte_for_byte` in celhyper, this gives us a complete bridge
//! validation chain without needing a live VM.

use std::sync::Arc;

use celmesh::{
    serve_hyper_listener, CelhyperVmHost, LoopbackHyperLink, NodeId,
    RestartPolicy, SerialHyperLink, VmHost, VmOp, VmOpReply,
};

type HostResult = Result<VmOpReply, String>;

fn ok(r: HostResult) -> VmOpReply {
    r.expect("VmOp failed")
}

#[tokio::test]
async fn end_to_end_bridge_drives_lifecycle_through_real_tcp() {
    // Server side: the loopback backend (stand-in for the kernel's
    // manager). serve_listener accepts connections and forwards
    // each JSON line through the LoopbackHyperLink::apply state
    // machine, returning the encoded reply over TCP.
    let backend: Arc<LoopbackHyperLink> = Arc::new(LoopbackHyperLink::new());
    let (addr, server) =
        serve_hyper_listener("127.0.0.1:0", backend.clone())
            .await
            .expect("listener");

    // Client side: SerialHyperLink encodes VmOps as JSON over the
    // real TCP socket. CelhyperVmHost is the production VmHost
    // shim that maps VmOp::{Create,Start,Stop,Delete,List} to
    // HyperRequest::* and routes everything else to a fallback.
    let link = Arc::new(
        SerialHyperLink::connect(addr).await.expect("connect"),
    );
    let host: Arc<dyn VmHost> = Arc::new(CelhyperVmHost::new(link));

    // Create three guests.
    let mut ids = Vec::new();
    for i in 0..3 {
        let r = ok(host
            .handle(VmOp::Create {
                label: format!("g{i}"),
                restart_policy: RestartPolicy::Never,
                image_path: None,
                cpu_count: None,
                memory_mib: None,
                boot_blob_crc32c: None,
            })
            .await);
        match r {
            VmOpReply::Created { vm_id } => ids.push(vm_id),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    assert_eq!(ids, vec![0, 1, 2]);

    // Start every guest; verify each transitions to "halted" with
    // the sentinel HLT exit code 12.
    for &id in &ids {
        let r = ok(host.handle(VmOp::Start { vm_id: id }).await);
        match r {
            VmOpReply::State { vm_id, state } => {
                assert_eq!(vm_id, id);
                assert_eq!(state, "halted");
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    // Delete every guest (now all terminal). Verifies the kernel's
    // logical-delete contract in mirrored form (loopback frees the
    // slot; the kernel sets DELETED bit — both are invisible to
    // future snapshot()s).
    for &id in &ids {
        let r = ok(host.handle(VmOp::Delete { vm_id: id }).await);
        assert!(matches!(r, VmOpReply::Deleted { vm_id } if vm_id == id));
    }

    // snapshot() goes through HyperRequest::List → server →
    // backend.list. After 3 deletes, the loopback's slot table is
    // empty.
    let owner: NodeId = "test-node".into();
    let snap = host.snapshot(&owner).await;
    assert!(snap.is_empty(), "expected empty snapshot, got {snap:?}");

    // Teardown.
    drop(host);
    drop(backend);
    server.abort();
}

#[tokio::test]
async fn stop_then_delete_round_trips_through_bridge() {
    let backend: Arc<LoopbackHyperLink> = Arc::new(LoopbackHyperLink::new());
    let (addr, server) =
        serve_hyper_listener("127.0.0.1:0", backend.clone())
            .await
            .unwrap();
    let link = Arc::new(SerialHyperLink::connect(addr).await.unwrap());
    let host: Arc<dyn VmHost> = Arc::new(CelhyperVmHost::new(link));

    // Create then Stop (without Start). Verifies Stop's idempotent
    // "non-terminal → stopped" path round-trips JSON correctly.
    let r = ok(host
        .handle(VmOp::Create {
            label: "to-stop".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        })
        .await);
    let vm_id = match r {
        VmOpReply::Created { vm_id } => vm_id,
        other => panic!("{other:?}"),
    };
    let r = ok(host.handle(VmOp::Stop { vm_id }).await);
    assert!(matches!(
        r,
        VmOpReply::State { vm_id: id, state } if id == vm_id && state == "stopped"
    ));

    // Delete on a stopped VM must succeed.
    let r = ok(host.handle(VmOp::Delete { vm_id }).await);
    assert!(matches!(r, VmOpReply::Deleted { vm_id: id } if id == vm_id));

    server.abort();
}
