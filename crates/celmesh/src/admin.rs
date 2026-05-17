//! Lightweight admin HTTP server for `/metrics` and `/healthz`.
//!
//! Week-21 Phase B introduces an *opt-in*, zero-overhead observability
//! surface that operators can scrape with Prometheus or a plain `curl`.
//! The server is deliberately tiny:
//!
//! * **Single dependency-free task.** No `hyper`, no `axum` — only
//!   `tokio::net::TcpListener` and a hand-rolled HTTP/1.0 reply.
//!   Build time and binary size stay small, audit surface stays narrow.
//! * **Off the hot path.** The listener task is spawned once and only
//!   wakes on inbound TCP connections. The gossip plane, the RPC
//!   plane and the failure detector never touch this code; they only
//!   bump the [`crate::metrics::MeshMetrics`] atomics, which are read
//!   lazily when a scrape arrives.
//! * **Bounded work per request.** Each connection reads at most
//!   4 KiB of request header, parses the first line,
//!   writes a fixed-shape reply, and closes. There is no keep-alive,
//!   no chunked transfer, no body parsing — a Prometheus scrape is a
//!   `GET /metrics HTTP/1.1\r\n...` and that is all we handle.
//! * **No allocations on the gossip hot path.** Strings are only
//!   built inside the handler task, when a scrape is actually in
//!   flight. With no scraper attached the cost is exactly the cost
//!   of one idle `accept()`.
//!
//! Endpoints
//! ---------
//! * `GET /metrics` → Prometheus text-exposition of every counter
//!   from [`crate::Mesh::metrics_prometheus`].
//! * `GET /healthz` → `200 OK` always (process is up + tokio
//!   runtime is responding). Body is the one-line
//!   [`crate::Mesh::summary`]; operators can grep it for
//!   `degraded=true`.
//! * `GET /readyz`  → `200 OK` iff this node sees at least one
//!   other alive peer (`degraded=false`); otherwise `503`.
//! * `GET /`        → tiny index that points at the three above.
//! * Anything else  → `404 Not Found`.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use celcommon::{CelError, CelResult};

use crate::Mesh;

/// Cap on the request-header bytes we will read before giving up.
/// A real Prometheus scrape header is well under 1 KiB; we accept up
/// to 4 KiB so unusual user-agents still work, and refuse the rest
/// without allocating.
const MAX_REQUEST_BYTES: usize = 4 * 1024;

/// Handle to a running admin server. Dropping the handle does **not**
/// stop the server — call [`AdminServer::shutdown`] explicitly. This
/// matches the rest of the `celmesh` API surface ([`Mesh`] also
/// requires an explicit `shutdown`).
#[must_use = "AdminServer must be stored or explicitly shut down"]
pub struct AdminServer {
    /// The address the listener is actually bound to. Useful when the
    /// caller passed `:0` and needs to know which ephemeral port the
    /// kernel chose.
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl AdminServer {
    /// Bind a TCP listener on `addr` and spawn the accept loop.
    ///
    /// `addr` is any string accepted by Tokio's resolver, e.g.
    /// `127.0.0.1:0`, `0.0.0.0:9100`, `[::1]:9100`. Use `:0` in
    /// tests so the kernel picks an unused port.
    ///
    /// # Errors
    /// Returns `CelError::Io` if the bind fails (port in use,
    /// permission denied, unresolvable host).
    pub async fn bind(mesh: Mesh, addr: &str) -> CelResult<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| CelError::Io(format!("admin bind {addr}: {e}")))?;
        let bound = listener
            .local_addr()
            .map_err(|e| CelError::Io(format!("admin local_addr: {e}")))?;

        let mesh = Arc::new(mesh);
        let task = tokio::spawn(accept_loop(listener, mesh));

        tracing::info!(target: "celmesh::admin",
                       addr = %bound, "admin server listening");
        Ok(Self { addr: bound, task })
    }

    /// Abort the accept loop. Idempotent — calling twice is safe.
    /// In-flight per-connection handlers continue until they
    /// finish naturally (they all have bounded work).
    pub fn shutdown(self) {
        self.task.abort();
    }
}

async fn accept_loop(listener: TcpListener, mesh: Arc<Mesh>) {
    loop {
        match listener.accept().await {
            Ok((sock, peer)) => {
                let mesh = mesh.clone();
                // Per-connection task. Bounded work, no shared state
                // beyond the cloned `Arc<Mesh>` — runs entirely off
                // the gossip hot path.
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(sock, &mesh).await {
                        tracing::debug!(target: "celmesh::admin",
                                        %peer, error = %e,
                                        "admin connection ended with error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(target: "celmesh::admin", error = %e,
                               "admin accept failed; retrying");
                // `accept()` errors are typically transient
                // (EMFILE, ECONNABORTED). Yield once to avoid a
                // busy-spin storm if the condition persists.
                tokio::task::yield_now().await;
            }
        }
    }
}

async fn handle_conn(mut sock: TcpStream, mesh: &Mesh) -> CelResult<()> {
    let path = read_request_path(&mut sock).await?;
    let (status, ctype, body) = route(mesh, &path).await;
    write_response(&mut sock, status, ctype, &body).await
}

/// Read up to [`MAX_REQUEST_BYTES`] of request header and return the
/// request path. We never look at headers or body — Prometheus does
/// not need them and ignoring them keeps the parser auditable.
async fn read_request_path(sock: &mut TcpStream) -> CelResult<String> {
    let mut buf = [0u8; MAX_REQUEST_BYTES];
    let mut filled = 0;
    loop {
        if filled == buf.len() {
            return Err(CelError::Invalid("admin: request header too large"));
        }
        let n = sock
            .read(&mut buf[filled..])
            .await
            .map_err(|e| CelError::Io(format!("admin read: {e}")))?;
        if n == 0 {
            return Err(CelError::Invalid("admin: client closed before request"));
        }
        filled += n;
        // The end of the request-line is the first CRLF (or LF).
        if let Some(end) = find_line_end(&buf[..filled]) {
            let line = std::str::from_utf8(&buf[..end])
                .map_err(|_| CelError::Invalid("admin: non-UTF-8 request line"))?;
            // Shape: `METHOD SP TARGET SP HTTP/x.y`.
            let mut it = line.split(' ');
            let method = it.next().unwrap_or("");
            let target = it.next().unwrap_or("");
            if method != "GET" && method != "HEAD" {
                return Err(CelError::Invalid("admin: only GET/HEAD supported"));
            }
            // Strip query string — we don't use it.
            let path = target.split('?').next().unwrap_or("/");
            return Ok(path.to_string());
        }
    }
}

fn find_line_end(b: &[u8]) -> Option<usize> {
    for i in 0..b.len() {
        if b[i] == b'\n' {
            return Some(if i > 0 && b[i - 1] == b'\r' { i - 1 } else { i });
        }
    }
    None
}

/// Map a request path to (status, content-type, body).
async fn route(mesh: &Mesh, path: &str) -> (u16, &'static str, String) {
    match path {
        "/metrics" => (200, "text/plain; version=0.0.4", mesh.metrics_prometheus()),
        "/healthz" => {
            // /healthz is "process alive + mesh handle responsive".
            // It always returns 200 unless the summary call itself
            // fails (which it can't — `summary` is infallible).
            let body = mesh.summary().await;
            (200, "text/plain; charset=utf-8", body + "\n")
        }
        "/readyz" => {
            // /readyz is the *cluster* health gate: 200 only if we
            // are not degraded (i.e. we see at least one other
            // alive peer). 503 otherwise so a load balancer can
            // depool the node.
            let summary = mesh.summary().await;
            let ready = summary.contains("degraded=false");
            let code = if ready { 200 } else { 503 };
            (code, "text/plain; charset=utf-8", summary + "\n")
        }
        "/" => (
            200,
            "text/plain; charset=utf-8",
            "celmesh admin\n  GET /metrics\n  GET /healthz\n  GET /readyz\n".to_string(),
        ),
        _ => (404, "text/plain; charset=utf-8", "not found\n".to_string()),
    }
}

async fn write_response(
    sock: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &str,
) -> CelResult<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Status",
    };
    // HTTP/1.0 + Connection: close — no keep-alive, no chunked.
    let head = format!(
        "HTTP/1.0 {status} {reason}\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    sock.write_all(head.as_bytes())
        .await
        .map_err(|e| CelError::Io(format!("admin write head: {e}")))?;
    sock.write_all(body.as_bytes())
        .await
        .map_err(|e| CelError::Io(format!("admin write body: {e}")))?;
    sock.shutdown()
        .await
        .map_err(|e| CelError::Io(format!("admin shutdown: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MemTransportFactory;
    use crate::{Mesh, MeshConfig, NodeId};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    async fn spin_single_node_mesh(cluster: &str, node: &str) -> Mesh {
        let factory = MemTransportFactory::new();
        let cfg = MeshConfig {
            cluster: cluster.to_string(),
            node_id: NodeId(node.to_string()),
            advertise_addr: format!("mem://{node}"),
            epoch: 1,
            seeds: vec![],
            gossip_interval: Duration::from_millis(50),
            timeout_suspect: Duration::from_millis(500),
            timeout_dead: Duration::from_millis(1_500),
            supervisor_interval: Duration::from_secs(0),
        };
        let tr = factory.bind(&cfg.advertise_addr).await.unwrap();
        Mesh::start(cfg, Arc::new(tr)).await.unwrap()
    }

    /// Issue a one-shot HTTP/1.0 GET and return (status_line, body).
    async fn http_get(addr: SocketAddr, path: &str) -> (String, String) {
        let mut sock = TcpStream::connect(addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.0\r\nHost: x\r\n\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let s = String::from_utf8(buf).unwrap();
        let mut parts = s.splitn(2, "\r\n\r\n");
        let head = parts.next().unwrap_or("").to_string();
        let body = parts.next().unwrap_or("").to_string();
        let status = head.lines().next().unwrap_or("").to_string();
        (status, body)
    }

    #[tokio::test]
    async fn metrics_endpoint_renders_prometheus_text() {
        let mesh = spin_single_node_mesh("c1", "n1").await;
        let admin = AdminServer::bind(mesh.clone(), "127.0.0.1:0").await.unwrap();
        let (status, body) = http_get(admin.addr, "/metrics").await;
        assert!(status.starts_with("HTTP/1.0 200"), "status={status}");
        assert!(body.contains("# TYPE celmesh_gossip_sent_total counter"),
                "body={body}");
        assert!(body.contains("celmesh_dead_promotions_total 0"),
                "body={body}");
        admin.shutdown();
        let _ = mesh.shutdown().await;
    }

    #[tokio::test]
    async fn healthz_always_returns_200_with_summary() {
        let mesh = spin_single_node_mesh("c1", "n1").await;
        let admin = AdminServer::bind(mesh.clone(), "127.0.0.1:0").await.unwrap();
        let (status, body) = http_get(admin.addr, "/healthz").await;
        assert!(status.starts_with("HTTP/1.0 200"));
        assert!(body.contains("node=n1"), "body={body}");
        assert!(body.contains("cluster=c1"), "body={body}");
        admin.shutdown();
        let _ = mesh.shutdown().await;
    }

    #[tokio::test]
    async fn readyz_is_503_when_single_node_and_degraded() {
        let mesh = spin_single_node_mesh("c1", "n1").await;
        let admin = AdminServer::bind(mesh.clone(), "127.0.0.1:0").await.unwrap();
        let (status, body) = http_get(admin.addr, "/readyz").await;
        assert!(status.starts_with("HTTP/1.0 503"), "status={status}");
        assert!(body.contains("degraded=true"), "body={body}");
        admin.shutdown();
        let _ = mesh.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let mesh = spin_single_node_mesh("c1", "n1").await;
        let admin = AdminServer::bind(mesh.clone(), "127.0.0.1:0").await.unwrap();
        let (status, _body) = http_get(admin.addr, "/no-such-thing").await;
        assert!(status.starts_with("HTTP/1.0 404"), "status={status}");
        admin.shutdown();
        let _ = mesh.shutdown().await;
    }

    #[tokio::test]
    async fn non_get_method_rejected_without_panic() {
        let mesh = spin_single_node_mesh("c1", "n1").await;
        let admin = AdminServer::bind(mesh.clone(), "127.0.0.1:0").await.unwrap();
        // The handler closes the connection on a non-GET method;
        // the important contract is "no panic, no hang". Read with
        // a short timeout so a misbehaving server is detectable.
        let mut sock = TcpStream::connect(admin.addr).await.unwrap();
        sock.write_all(b"POST /metrics HTTP/1.0\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2),
                                     sock.read_to_end(&mut buf)).await;
        // Either empty (handler closed) or an error line — both are
        // acceptable. The mesh must still be healthy.
        let (status, _) = http_get(admin.addr, "/healthz").await;
        assert!(status.starts_with("HTTP/1.0 200"), "post-misuse status={status}");
        admin.shutdown();
        let _ = mesh.shutdown().await;
    }
}
