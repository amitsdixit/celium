//! W23-A — drive a *live* CelHyper bridge running inside QEMU over
//! its COM2-to-TCP redirect.
//!
//! Gated `#[ignore]` because it requires an externally-provisioned
//! environment:
//!
//!   * `CELIUM_BRIDGE_TCP=host:port` — the address that
//!     `scripts/run-qemu.sh BRIDGE_TCP=...` is listening on.
//!
//! Boot the kernel in one terminal:
//!
//! ```bash
//! BRIDGE_TCP=127.0.0.1:5555 TIMEOUT=120 bash scripts/run-qemu.sh
//! ```
//!
//! Then in another:
//!
//! ```bash
//! CELIUM_BRIDGE_TCP=127.0.0.1:5555 \
//!   cargo test -p celtest --test w23_qemu_bridge -- --ignored --nocapture
//! ```
//!
//! Together with the in-process W22-C suite this proves the bridge
//! end-to-end: real kernel image, real COM2 UART, real TCP, real
//! `SerialHyperLink` from a host process.

use std::sync::Arc;
use std::time::Duration;

use celmesh::{CelhyperVmHost, NodeId, SerialHyperLink, VmHost, VmOp, VmOpReply};

fn bridge_addr() -> Option<String> {
    std::env::var("CELIUM_BRIDGE_TCP").ok()
}

#[tokio::test]
#[ignore = "requires a running QEMU+CelHyper with COM2 -> TCP (see file header)"]
async fn live_qemu_bridge_lists_bringup_vms() {
    let Some(addr) = bridge_addr() else {
        panic!("CELIUM_BRIDGE_TCP unset; see header for usage");
    };

    // The bridge starts only after `bring_up` completes, which on a
    // KVM-nested host takes well under a second. Retry the connect
    // a few times to absorb the small race against QEMU's startup.
    let link = {
        let mut last_err = None;
        let mut sock = None;
        for _ in 0..50 {
            match SerialHyperLink::connect(addr.as_str()).await {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        match sock {
            Some(s) => Arc::new(s),
            None => panic!("connect {addr} failed: {last_err:?}"),
        }
    };

    let host: Arc<dyn VmHost> = Arc::new(CelhyperVmHost::new(link));

    // Bring-up registers two guests ("vm-a", "vm-b") and runs both
    // to a HLT before main hands control to bridge::run(). The list
    // op must observe both (the kernel's logical-delete contract
    // keeps Halted entries visible).
    //
    // QEMU opens its `-serial tcp:...,server=on,wait=off` listener at
    // start-up — *before* the kernel reaches `bridge::run()`. So the
    // first few snapshots can legitimately return an empty list
    // (request bytes are buffered by QEMU until the kernel's UART
    // driver drains them) or time out. Retry for up to ~15 s, which
    // is comfortably above the observed bring-up wall-clock.
    let owner: NodeId = "qemu-host".into();
    let mut snap = host.snapshot(&owner).await;
    for _ in 0..150 {
        if snap.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        snap = host.snapshot(&owner).await;
    }
    assert!(
        snap.len() >= 2,
        "expected at least 2 bringup VMs in the live bridge snapshot after retry, got {snap:?}",
    );

    eprintln!("w23: live bridge snapshot rows = {}", snap.len());
    for vm in &snap {
        eprintln!("  vm_id={} state={} last_exit={:?}", vm.vm_id, vm.state, vm.last_exit);
    }

    // Also exercise an explicit op so we are not relying solely on
    // the snapshot path.
    let r = host
        .handle(VmOp::Create {
            label: "w23-live".into(),
            restart_policy: celmesh::RestartPolicy::Never,
        })
        .await
        .expect("Create through live bridge");
    let id = match r {
        VmOpReply::Created { vm_id } => vm_id,
        other => panic!("unexpected Create reply from live bridge: {other:?}"),
    };

    // Newly-created VM must be in the snapshot.
    let snap2 = host.snapshot(&owner).await;
    assert!(
        snap2.iter().any(|v| v.vm_id == id),
        "freshly-created vm {id} missing from snapshot {snap2:?}",
    );

    // Start the new VM end-to-end through the live bridge. Real
    // vmlaunch should drive it to HLT and back via the longjmp
    // resume path.
    let started = host
        .handle(VmOp::Start { vm_id: id })
        .await
        .expect("Start through live bridge");
    eprintln!("w23: start reply for vm {id} = {started:?}");

    let del = host
        .handle(VmOp::Delete { vm_id: id })
        .await
        .expect("Delete through live bridge");
    assert!(
        matches!(del, VmOpReply::Deleted { vm_id } if vm_id == id),
        "unexpected Delete reply: {del:?}",
    );
}
