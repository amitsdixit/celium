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
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
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

/// W23-B regression: prove the new wire fields (`image_path`,
/// `cpu_count`, `memory_mib`, `boot_blob_crc32c`) round-trip
/// through the *live* kernel — i.e. the bridge's request decoder
/// reads them, the EXTRAS side-table stores them, and the next
/// snapshot encoder emits them so the host sees identical values
/// echoed back.
#[tokio::test]
#[ignore = "requires a running QEMU+CelHyper with COM2 -> TCP (see file header)"]
async fn live_qemu_bridge_round_trips_w23b_metadata() {
    let Some(addr) = bridge_addr() else {
        panic!("CELIUM_BRIDGE_TCP unset; see header for usage");
    };

    // Same connect-with-retry pattern as the sibling test.
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
    let owner: NodeId = "qemu-host".into();

    // Settle: wait until the bring-up rows are visible (same as
    // the sibling test). Avoids racing the kernel's startup.
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
        "bring-up rows missing before W23-B metadata test: {snap:?}",
    );

    // Create a VM with every W23-B field populated.
    let want_image = "/golden/w23b-payload.raw".to_string();
    let want_cpu: u32 = 4;
    let want_mem: u64 = 512;
    let want_crc: u32 = 0xDEAD_BEEF;

    let r = host
        .handle(VmOp::Create {
            label: "w23b-meta".into(),
            restart_policy: celmesh::RestartPolicy::Never,
            image_path: Some(want_image.clone()),
            cpu_count: Some(want_cpu),
            memory_mib: Some(want_mem),
            boot_blob_crc32c: Some(want_crc),
        })
        .await
        .expect("Create with metadata through live bridge");
    let id = match r {
        VmOpReply::Created { vm_id } => vm_id,
        other => panic!("unexpected Create reply: {other:?}"),
    };
    eprintln!("w23b: created vm {id} with metadata; sampling snapshot...");

    // The kernel encoder must echo every populated field back.
    let snap2 = host.snapshot(&owner).await;
    let row = snap2
        .iter()
        .find(|v| v.vm_id == id)
        .unwrap_or_else(|| panic!("fresh vm {id} missing from snapshot {snap2:?}"));

    assert_eq!(row.image_path.as_deref(), Some(want_image.as_str()),
        "kernel did not echo image_path: {row:?}");
    assert_eq!(row.cpu_count, Some(want_cpu),
        "kernel did not echo cpu_count: {row:?}");
    assert_eq!(row.memory_mib, Some(want_mem),
        "kernel did not echo memory_mib: {row:?}");
    assert_eq!(row.boot_blob_crc32c, Some(want_crc),
        "kernel did not echo boot_blob_crc32c: {row:?}");

    // Drive the VM to a terminal state before Delete — the kernel
    // rejects Delete on a freshly-Created (non-terminal) slot with
    // `Invalid("manager: vm not terminal")`. Start runs the canned
    // bring-up template and HLTs, leaving the slot Halted.
    let started = host
        .handle(VmOp::Start { vm_id: id })
        .await
        .expect("Start through live bridge");
    eprintln!("w23b-meta: start reply for vm {id} = {started:?}");

    // Cleanup.
    let del = host
        .handle(VmOp::Delete { vm_id: id })
        .await
        .expect("Delete through live bridge");
    assert!(
        matches!(del, VmOpReply::Deleted { vm_id } if vm_id == id),
        "unexpected Delete reply: {del:?}",
    );
}

/// W23-E3: end-to-end `stage_image` → `Create(boot_blob_crc32c)` →
/// `Start` against a real kernel. Proves the new `HyperRequest::
/// ImageLoad` wire is plumbed through `celhyper::image_loader::
/// stage_from_hex` and that `BootImage::from_staged_or_embedded`
/// picks the staged blob when the CRC matches.
#[tokio::test]
#[ignore = "requires a running QEMU+CelHyper with COM2 -> TCP (see file header)"]
async fn live_qemu_bridge_stage_image_create_start() {
    let Some(addr) = bridge_addr() else {
        eprintln!("skip: CELIUM_BRIDGE_TCP unset");
        return;
    };

    // Same retry pattern as the sibling tests.
    let link = {
        let mut last_err = None;
        let mut sock = None;
        for _ in 0..50 {
            match SerialHyperLink::connect(addr.as_str()).await {
                Ok(s) => { sock = Some(s); break; }
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

    let host = CelhyperVmHost::new(link).with_strict(true);

    // Small deterministic payload — well under the 4 KiB kernel cap.
    let payload: Vec<u8> = (0..512u32)
        .map(|i| (i.wrapping_mul(73) ^ 0xa5) as u8)
        .collect();

    let crc = host
        .stage_image(&payload)
        .await
        .expect("stage_image must succeed against live kernel");
    eprintln!("w23-e3: staged {} bytes, crc=0x{:08x}", payload.len(), crc);

    // Create binds the staged image via boot_blob_crc32c.
    let reply = host
        .handle(VmOp::Create {
            label: "w23-e3-stage".into(),
            restart_policy: celmesh::RestartPolicy::Never,
            image_path: None,
            cpu_count:  Some(1),
            memory_mib: Some(4),
            boot_blob_crc32c: Some(crc),
        })
        .await
        .expect("Create");
    let vm_id = match reply {
        VmOpReply::Created { vm_id } => vm_id,
        other => panic!("unexpected Create reply: {other:?}"),
    };

    // Start — kernel should pick the staged image and run it to HLT.
    let started = host
        .handle(VmOp::Start { vm_id })
        .await
        .expect("Start");
    eprintln!("w23-e3: start reply for vm {vm_id} = {started:?}");
    match started {
        VmOpReply::State { vm_id: vid, state } => {
            assert_eq!(vid, vm_id);
            assert_eq!(state, "halted", "unexpected post-start state: {state}");
        }
        other => panic!("unexpected start reply: {other:?}"),
    }

    let del = host
        .handle(VmOp::Delete { vm_id })
        .await
        .expect("Delete");
    assert!(
        matches!(del, VmOpReply::Deleted { vm_id: vid } if vid == vm_id),
        "unexpected Delete reply: {del:?}",
    );
}
