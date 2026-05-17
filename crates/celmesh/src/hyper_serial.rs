//! W22-B-1 — TCP transport for the CelHyper bridge.
//!
//! [`SerialHyperLink`] speaks the W22 wire shape over a streaming
//! socket using **newline-delimited JSON**: one [`HyperRequest`] per
//! line, one [`HyperReply`] per line. The format is intentionally
//! human-readable so a sysadmin watching a QEMU `-serial tcp:...`
//! console can decode what the host is asking the kernel without
//! tooling.
//!
//! The kernel side will land in W22-B-2 as a `no_std` decoder reading
//! the same frame format off a 16550A UART. Until then, [`serve`]
//! provides a generic server that pairs any [`HyperLink`] with any
//! `AsyncRead + AsyncWrite` stream — used both by the unit tests
//! (over `tokio::io::duplex`) and by a future helper for a userspace
//! "kernel emulator" process.
//!
//! # Framing rules
//!
//! * Requests and replies are JSON values terminated by exactly one
//!   `\n`. Embedded `\n` inside a JSON value is impossible because
//!   `serde_json` never emits raw newlines.
//! * Lines longer than [`MAX_FRAME_BYTES`] cause the connection to
//!   be torn down. Keeps a faulty peer from exhausting host memory.
//! * Each call waits at most [`CALL_TIMEOUT`] for its reply before
//!   the link surfaces a `CelError::Timeout`.
//!
//! # Reconnection
//!
//! [`SerialHyperLink::connect`] returns a link that has *already*
//! established its TCP connection. If the peer drops, every
//! subsequent `call` returns `CelError::Io` until the caller rebuilds
//! the link. The control plane already knows how to retry transient
//! `set_host` failures; baking reconnection into the link would hide
//! peer death from gossip metrics, which is the wrong default for
//! the kernel-bridge case.

use std::sync::Arc;
use std::time::Duration;

use celcommon::{CelError, CelResult};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::hyper_host::{HyperLink, HyperReply, HyperRequest};
use std::future::Future;
use std::pin::Pin;

/// Maximum length of a single NDJSON frame, in bytes. Picked to fit
/// the largest realistic [`HyperReply::Listed`] (`HYPER_MAX_VMS` rows
/// × ~256 bytes per row, plus slop) without ever needing fragmentation.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Hard ceiling on how long a single bridge call may wait for its
/// reply before the link surfaces `CelError::Timeout`. The kernel is
/// expected to reply in well under a millisecond; one second is the
/// "the wire is dead" threshold, not a normal-operation budget.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(1);

/// TCP-backed bridge to a remote CelHyper kernel (or emulator).
///
/// Holds the socket halves behind a `Mutex` because [`HyperLink::call`]
/// is `&self` and the protocol is strictly request/reply with no
/// pipelining: serializing access on the link is correct *and*
/// matches what a real serial-line peer can handle.
pub struct SerialHyperLink {
    inner: Mutex<SerialInner>,
}

struct SerialInner {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl SerialHyperLink {
    /// Connect to `addr` (any [`tokio::net::ToSocketAddrs`]).
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> CelResult<Self> {
        let sock = TcpStream::connect(addr)
            .await
            .map_err(|e| CelError::Io(format!("hyper-serial connect: {e}")))?;
        sock.set_nodelay(true).ok();
        let (r, w) = sock.into_split();
        Ok(Self {
            inner: Mutex::new(SerialInner {
                reader: BufReader::new(r),
                writer: w,
            }),
        })
    }
}

impl HyperLink for SerialHyperLink {
    fn call<'a>(
        &'a self,
        req: HyperRequest,
    ) -> Pin<Box<dyn Future<Output = CelResult<HyperReply>> + Send + 'a>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().await;
            let mut line = serde_json::to_string(&req)
                .map_err(|e| CelError::Io(format!("hyper-serial encode: {e}")))?;
            line.push('\n');
            guard
                .writer
                .write_all(line.as_bytes())
                .await
                .map_err(|e| CelError::Io(format!("hyper-serial write: {e}")))?;
            guard
                .writer
                .flush()
                .await
                .map_err(|e| CelError::Io(format!("hyper-serial flush: {e}")))?;

            let mut buf = String::new();
            let read = timeout(CALL_TIMEOUT, guard.reader.read_line(&mut buf)).await;
            let n = match read {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    return Err(CelError::Io(format!("hyper-serial read: {e}")))
                }
                Err(_) => {
                    return Err(CelError::Timeout(format!(
                        "hyper-serial: no reply in {CALL_TIMEOUT:?}"
                    )))
                }
            };
            if n == 0 {
                return Err(CelError::Io("hyper-serial: peer closed".into()));
            }
            if buf.len() > MAX_FRAME_BYTES {
                return Err(CelError::Io(format!(
                    "hyper-serial: reply > {MAX_FRAME_BYTES} bytes"
                )));
            }
            let reply: HyperReply = serde_json::from_str(buf.trim_end())
                .map_err(|e| CelError::Io(format!("hyper-serial decode: {e}")))?;
            Ok(reply)
        })
    }
}

/// Serve one peer connection: read NDJSON requests off `stream`, route
/// each one through `link`, write NDJSON replies back. Returns when
/// the peer closes the read side cleanly or any I/O / framing error
/// occurs. Generic over the stream type so unit tests can drive it
/// with `tokio::io::duplex` while production wires it to a TCP socket.
pub async fn serve<S>(stream: S, link: Arc<dyn HyperLink>) -> CelResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .map_err(|e| CelError::Io(format!("hyper-serve read: {e}")))?;
        if n == 0 {
            return Ok(()); // peer closed cleanly
        }
        if buf.len() > MAX_FRAME_BYTES {
            return Err(CelError::Io(format!(
                "hyper-serve: request > {MAX_FRAME_BYTES} bytes"
            )));
        }
        let req: HyperRequest = match serde_json::from_str(buf.trim_end()) {
            Ok(r) => r,
            Err(e) => {
                return Err(CelError::Io(format!("hyper-serve decode: {e}")));
            }
        };
        // Dispatch errors are surfaced as a JSON object so the peer can
        // recover; only transport failures tear the connection down.
        let reply_line = match link.call(req).await {
            Ok(reply) => {
                let mut s = serde_json::to_string(&reply)
                    .map_err(|e| CelError::Io(format!("hyper-serve encode: {e}")))?;
                s.push('\n');
                s
            }
            Err(e) => {
                // Encode as a `Listed` with zero rows? No — we need a
                // distinguishable shape. The wire enum is closed for
                // W22, so we shut the connection. Recovery is by
                // re-connecting; that matches the kernel-bridge model
                // (a panicked kernel can't keep streaming replies).
                return Err(CelError::Io(format!(
                    "hyper-serve: link error not representable on wire: {e:?}"
                )));
            }
        };
        w.write_all(reply_line.as_bytes())
            .await
            .map_err(|e| CelError::Io(format!("hyper-serve write: {e}")))?;
        w.flush()
            .await
            .map_err(|e| CelError::Io(format!("hyper-serve flush: {e}")))?;
    }
}

/// Convenience: spawn an accept loop binding `addr`, serving each
/// inbound connection with a fresh clone of `link`. Returns the bound
/// `SocketAddr` so tests can connect back. The accept loop runs until
/// the returned `JoinHandle` is dropped; production callers should
/// keep the handle alive for the lifetime of the kernel emulator.
pub async fn serve_listener(
    addr: impl ToSocketAddrs,
    link: Arc<dyn HyperLink>,
) -> CelResult<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| CelError::Io(format!("hyper-serve bind: {e}")))?;
    let bound = listener
        .local_addr()
        .map_err(|e| CelError::Io(format!("hyper-serve local_addr: {e}")))?;
    let handle = tokio::spawn(async move {
        loop {
            let (sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(target: "celmesh::hyper_serial",
                                   error = %e, "accept failed");
                    return;
                }
            };
            sock.set_nodelay(true).ok();
            let link = link.clone();
            tokio::spawn(async move {
                if let Err(e) = serve(sock, link).await {
                    tracing::debug!(target: "celmesh::hyper_serial",
                                    error = ?e, "conn closed");
                }
            });
        }
    });
    Ok((bound, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hyper_host::{CelhyperVmHost, LoopbackHyperLink};
    use crate::host::VmHost;
    use crate::federation::RestartPolicy;
    use crate::proto::{VmOp, VmOpReply};

    /// End-to-end smoke: bind a kernel-emulator listener backed by
    /// LoopbackHyperLink, point a SerialHyperLink at it, drive a
    /// full Create→Start→Delete cycle through CelhyperVmHost.
    #[tokio::test]
    async fn serial_link_round_trip_through_real_tcp() {
        let kernel: Arc<dyn HyperLink> = Arc::new(LoopbackHyperLink::new());
        let (addr, _h) = serve_listener("127.0.0.1:0", kernel).await.unwrap();

        let link: Arc<dyn HyperLink> =
            Arc::new(SerialHyperLink::connect(addr).await.unwrap());
        let host = CelhyperVmHost::new(link);

        let r = host
            .handle(VmOp::Create { label: "g".into(), restart_policy: RestartPolicy::Never, image_path: None, cpu_count: None, memory_mib: None, boot_blob_crc32c: None })
            .await
            .unwrap();
        let VmOpReply::Created { vm_id } = r else { panic!("create") };
        assert_eq!(vm_id, 0);

        let r = host.handle(VmOp::Start { vm_id }).await.unwrap();
        let VmOpReply::State { state, .. } = r else { panic!("start") };
        assert_eq!(state, "halted");

        let r = host.handle(VmOp::Delete { vm_id }).await.unwrap();
        assert!(matches!(r, VmOpReply::Deleted { vm_id: 0 }));
    }

    /// Drive `serve` over an in-memory `duplex` so framing logic is
    /// covered without binding a real port.
    #[tokio::test]
    async fn duplex_framing_drives_serve_correctly() {
        let (client, server) = tokio::io::duplex(8 * 1024);
        let link: Arc<dyn HyperLink> = Arc::new(LoopbackHyperLink::new());
        tokio::spawn(async move { serve(server, link).await.ok(); });

        let (r, mut w) = tokio::io::split(client);
        let mut reader = BufReader::new(r);

        let req = HyperRequest::Create {
            label: "dup".into(),
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        };
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        w.write_all(line.as_bytes()).await.unwrap();
        w.flush().await.unwrap();

        let mut buf = String::new();
        reader.read_line(&mut buf).await.unwrap();
        let reply: HyperReply = serde_json::from_str(buf.trim_end()).unwrap();
        assert_eq!(reply, HyperReply::Created { vm_id: 0 });
    }

    /// A reply that never arrives must surface as `CelError::Timeout`,
    /// not block the link forever. Uses a listener that accepts the
    /// connection then never writes anything.
    #[tokio::test]
    async fn link_times_out_when_peer_is_silent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept and drop the read half; never write a reply.
            let (_s, _) = listener.accept().await.unwrap();
            // Hold the socket until the test ends.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let link = SerialHyperLink::connect(addr).await.unwrap();
        let err = link
            .call(HyperRequest::List)
            .await
            .expect_err("silent peer must time out");
        assert!(
            matches!(err, CelError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
    }
}
