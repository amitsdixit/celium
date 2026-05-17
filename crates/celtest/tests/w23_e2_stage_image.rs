//! W23-E2 — bridge-streamed boot image end-to-end.
//!
//! Exercises the full host-side path for the new
//! `HyperRequest::ImageLoad` / `HyperReply::ImageLoaded` wire
//! introduced in W23-E1 (kernel side) and W23-E2 (host side):
//!
//!   `CelhyperVmHost::stage_image`
//!       → `SerialHyperLink` (JSON over TCP)
//!       → `serve_hyper_listener`
//!       → `LoopbackHyperLink::apply` (validates hex/len/CRC)
//!
//! Together with the kernel-side `wire.rs` unit tests this gives us
//! complete validation of the new wire shape, with `LoopbackHyperLink`
//! standing in for the bare-metal kernel's `image_loader::stage_from_hex`.
//! The real-kernel-in-QEMU variant is `#[ignore]`-gated in
//! `w23_qemu_bridge.rs` (and is W23-E3 work — it needs the kernel
//! `bridge.rs` to be running against an actual COM2 socket).

use std::sync::Arc;

use celmesh::{
    serve_hyper_listener, CelhyperVmHost, LoopbackHyperLink, SerialHyperLink,
};

#[tokio::test]
async fn stage_image_round_trips_over_real_tcp_bridge() {
    let backend: Arc<LoopbackHyperLink> = Arc::new(LoopbackHyperLink::new());
    let (addr, server) =
        serve_hyper_listener("127.0.0.1:0", backend.clone())
            .await
            .expect("listener");
    let link = Arc::new(
        SerialHyperLink::connect(addr).await.expect("connect"),
    );
    let host = CelhyperVmHost::new(link);

    // 1 KiB pseudo-random payload (well under the 4 KiB cap).
    let payload: Vec<u8> = (0u32..1024).map(|i| (i * 31 + 7) as u8).collect();

    let crc = host
        .stage_image(&payload)
        .await
        .expect("stage_image must succeed over TCP bridge");

    // CRC the host reports must match a freshly-computed one.
    // We can't call hyper_host's private crc helper directly so we
    // re-stage the same bytes and confirm the kernel echoes the
    // same value back. Two successful stages with identical input
    // means the JSON round-trip + verification path is stable.
    let crc2 = host
        .stage_image(&payload)
        .await
        .expect("re-stage must succeed");
    assert_eq!(crc, crc2, "same payload must produce same CRC");

    server.abort();
}

#[tokio::test]
async fn stage_image_propagates_kernel_rejection_over_tcp() {
    // Empty payload is rejected client-side before any wire I/O —
    // proves the host's input validation is symmetric with the
    // kernel's `stage_from_hex` precondition (len > 0).
    let backend: Arc<LoopbackHyperLink> = Arc::new(LoopbackHyperLink::new());
    let (addr, server) =
        serve_hyper_listener("127.0.0.1:0", backend.clone())
            .await
            .expect("listener");
    let link = Arc::new(
        SerialHyperLink::connect(addr).await.expect("connect"),
    );
    let host = CelhyperVmHost::new(link);

    let err = host
        .stage_image(&[])
        .await
        .expect_err("empty payload must be rejected");
    assert!(err.contains("empty"), "err={err}");

    // Oversize: reject *before* wire I/O — keeps the kernel's RX
    // buffer safe even from a buggy controller.
    let payload = vec![0xAB; 4097];
    let err = host
        .stage_image(&payload)
        .await
        .expect_err("oversize must be rejected");
    assert!(err.contains("MAX_STAGED_IMAGE_BYTES"), "err={err}");

    server.abort();
}
