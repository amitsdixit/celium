//! Pluggable transport layer for CelMesh gossip.
//!
//! The `Mesh` engine talks to a `Transport`. Two implementations:
//!
//! * [`MemTransport`] — in-process pub/sub via `tokio::sync::mpsc`.
//!   Used by integration tests so they do not consume host UDP ports
//!   and run identically on Windows, Linux, and CI.
//! * [`UdpTransport`] — real `tokio::net::UdpSocket` carrier; the
//!   address scheme is the standard `host:port` string.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use celcommon::{CelError, CelResult};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};

use crate::proto::MAX_FRAME_BYTES;

/// Buffer size for the per-transport receive channel. Bounded so a
/// stuck consumer eventually back-pressures the producer instead of
/// growing memory without bound.
const RX_BUFFER: usize = 256;

/// Pluggable gossip transport. Implementations must be cheap to
/// share across tasks (`Send + Sync + 'static`).
///
/// We hand-roll boxed futures rather than pulling in `async-trait`
/// to keep the workspace dep tree small. The two implementations in
/// this module are the only ones the engine needs today.
pub trait Transport: Send + Sync + 'static {
    /// Address peers can use to reach this transport.
    fn local_addr(&self) -> String;

    /// Receive the next frame plus the source address. Cancellation
    /// safe.
    fn recv<'a>(
        &'a self,
    ) -> core::pin::Pin<Box<dyn core::future::Future<Output = CelResult<(Vec<u8>, String)>> + Send + 'a>>;

    /// Send `bytes` to `peer`.
    fn send<'a>(
        &'a self,
        peer: &'a str,
        bytes: &'a [u8],
    ) -> core::pin::Pin<Box<dyn core::future::Future<Output = CelResult<()>> + Send + 'a>>;
}

// ---------------------------------------------------------------------------
// In-memory transport — used by tests.
// ---------------------------------------------------------------------------

/// Factory that hands out connected `MemTransport` instances sharing
/// the same routing table.
#[derive(Clone, Default)]
pub struct MemTransportFactory {
    /// Map of address -> sender. Cloneable; the inner Mutex protects
    /// the routing table during connect/disconnect.
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<(Vec<u8>, String)>>>>,
}

impl MemTransportFactory {
    /// New, empty factory.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Build a transport bound to `addr`. Replaces any previous
    /// binding to the same address.
    pub async fn bind(&self, addr: impl Into<String>) -> CelResult<MemTransport> {
        let addr = addr.into();
        let (tx, rx) = mpsc::channel(RX_BUFFER);
        self.routes.lock().await.insert(addr.clone(), tx);
        Ok(MemTransport {
            addr,
            rx: Mutex::new(rx),
            routes: self.routes.clone(),
        })
    }

    /// Remove `addr` from the routing table — simulates a node
    /// crashing without a clean `Goodbye`.
    pub async fn drop_addr(&self, addr: &str) {
        self.routes.lock().await.remove(addr);
    }
}

/// In-process transport.
pub struct MemTransport {
    addr:   String,
    rx:     Mutex<mpsc::Receiver<(Vec<u8>, String)>>,
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<(Vec<u8>, String)>>>>,
}

impl Transport for MemTransport {
    fn local_addr(&self) -> String { self.addr.clone() }

    fn recv<'a>(
        &'a self,
    ) -> core::pin::Pin<Box<dyn core::future::Future<Output = CelResult<(Vec<u8>, String)>> + Send + 'a>> {
        Box::pin(async move {
            let mut rx = self.rx.lock().await;
            rx.recv().await
                .ok_or(CelError::Internal("mem transport closed"))
        })
    }

    fn send<'a>(
        &'a self,
        peer: &'a str,
        bytes: &'a [u8],
    ) -> core::pin::Pin<Box<dyn core::future::Future<Output = CelResult<()>> + Send + 'a>> {
        let local = self.addr.clone();
        let payload = bytes.to_vec();
        Box::pin(async move {
            let tx = {
                let routes = self.routes.lock().await;
                routes.get(peer).cloned()
            };
            match tx {
                Some(tx) => tx.send((payload, local)).await
                    .map_err(|_| CelError::Io(format!("mem peer {peer} closed"))),
                None => Err(CelError::Io(format!("mem peer {peer} unknown"))),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// UDP transport — real wire.
// ---------------------------------------------------------------------------

/// Real-network UDP transport.
pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Bind a UDP socket on `addr`. `addr` accepts the standard Tokio
    /// formats; pass `"0.0.0.0:0"` to let the kernel pick a port.
    pub async fn bind(addr: impl tokio::net::ToSocketAddrs) -> CelResult<Self> {
        let socket = UdpSocket::bind(addr).await
            .map_err(|e| CelError::Io(format!("udp bind: {e}")))?;
        Ok(Self { socket })
    }
}

impl Transport for UdpTransport {
    fn local_addr(&self) -> String {
        self.socket
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "udp://?".to_string())
    }

    fn recv<'a>(
        &'a self,
    ) -> core::pin::Pin<Box<dyn core::future::Future<Output = CelResult<(Vec<u8>, String)>> + Send + 'a>> {
        Box::pin(async move {
            let mut buf = vec![0u8; MAX_FRAME_BYTES];
            let (n, peer) = self.socket.recv_from(&mut buf).await
                .map_err(|e| CelError::Io(format!("udp recv: {e}")))?;
            buf.truncate(n);
            Ok((buf, peer.to_string()))
        })
    }

    fn send<'a>(
        &'a self,
        peer: &'a str,
        bytes: &'a [u8],
    ) -> core::pin::Pin<Box<dyn core::future::Future<Output = CelResult<()>> + Send + 'a>> {
        Box::pin(async move {
            let addr: SocketAddr = peer.parse()
                .map_err(|_| CelError::Invalid("udp peer addr"))?;
            self.socket.send_to(bytes, addr).await
                .map_err(|e| CelError::Io(format!("udp send: {e}")))?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mem_transport_round_trip() {
        let f = MemTransportFactory::new();
        let a = f.bind("mem://a").await.unwrap();
        let b = f.bind("mem://b").await.unwrap();
        a.send("mem://b", b"hello").await.unwrap();
        let (msg, src) = b.recv().await.unwrap();
        assert_eq!(msg, b"hello");
        assert_eq!(src, "mem://a");
    }

    #[tokio::test]
    async fn mem_transport_unknown_peer_errors() {
        let f = MemTransportFactory::new();
        let a = f.bind("mem://a").await.unwrap();
        assert!(a.send("mem://nope", b"x").await.is_err());
    }
}
