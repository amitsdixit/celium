//! Mesh-wide observability counters.
//!
//! W17 introduces a tiny in-process metrics surface so the gossip
//! plane, the RPC plane, and the failure detector all expose hard
//! numbers an operator can grep, alert on, and chart. The design
//! is intentionally Prometheus-compatible: every value is a
//! monotonic `u64` so a Prometheus exporter can publish them as
//! `counter` metrics without any post-processing.
//!
//! The implementation is deliberately small:
//!
//! * Every counter is a [`std::sync::atomic::AtomicU64`] with
//!   `Relaxed` ordering. None of these values participate in
//!   cross-thread synchronisation; we only need monotonicity.
//! * `MeshMetrics` is `Clone` because the [`crate::Mesh`] handle
//!   is `Clone` and we want each clone to share counters via
//!   [`std::sync::Arc`].
//! * `MeshMetricsSnapshot` is the public read shape — a plain
//!   `Copy` struct of `u64`s. Callers consume snapshots; they
//!   never see the atomics directly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Shared counter set. Cheap to clone — the inner `Arc` is reused.
#[derive(Clone, Default, Debug)]
pub struct MeshMetrics {
    inner: Arc<MetricsInner>,
}

#[derive(Default, Debug)]
struct MetricsInner {
    gossip_sent:           AtomicU64,
    gossip_recv:           AtomicU64,
    decode_errors:         AtomicU64,
    foreign_cluster_drops: AtomicU64,
    suspect_promotions:    AtomicU64,
    dead_promotions:       AtomicU64,
    rpc_in:                AtomicU64,
    rpc_out:               AtomicU64,
    rpc_timeouts:          AtomicU64,
    rpc_errors:            AtomicU64,
    join_calls:            AtomicU64,
    supervisor_restarts:   AtomicU64,
}

/// Read-only snapshot of every counter at one instant.
///
/// The snapshot is intentionally `Copy + Serialize` so it can be
/// returned over the public API, dumped to JSON, or compared in
/// tests with simple value equality.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshMetricsSnapshot {
    /// Outbound gossip frames (Hello + Sync + Goodbye + Request +
    /// Response). Bumped before the bytes hit the transport.
    pub gossip_sent: u64,
    /// Inbound frames that decoded into a valid `Envelope`.
    pub gossip_recv: u64,
    /// Frames that failed to decode (bad magic, version mismatch,
    /// > `MAX_FRAME_BYTES`, malformed JSON).
    pub decode_errors: u64,
    /// Frames whose `cluster` did not match ours and were dropped.
    pub foreign_cluster_drops: u64,
    /// Number of times a peer transitioned from Alive → Suspect.
    pub suspect_promotions: u64,
    /// Number of times a peer transitioned to Dead.
    pub dead_promotions: u64,
    /// Inbound `Request` frames dispatched to the local host.
    pub rpc_in: u64,
    /// Outbound `Request` frames issued by `invoke`.
    pub rpc_out: u64,
    /// Outbound RPCs that exhausted their deadline.
    pub rpc_timeouts: u64,
    /// Outbound RPCs returning a non-timeout error from the peer.
    pub rpc_errors: u64,
    /// Number of times `join` was called at runtime.
    pub join_calls: u64,
    /// VMs restarted by the supervisor during recovery passes.
    pub supervisor_restarts: u64,
}

impl MeshMetrics {
    /// Build a fresh, all-zero counter set.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Capture every counter into a `Copy` snapshot.
    #[must_use]
    pub fn snapshot(&self) -> MeshMetricsSnapshot {
        let i = &self.inner;
        MeshMetricsSnapshot {
            gossip_sent:            i.gossip_sent.load(Ordering::Relaxed),
            gossip_recv:            i.gossip_recv.load(Ordering::Relaxed),
            decode_errors:          i.decode_errors.load(Ordering::Relaxed),
            foreign_cluster_drops:  i.foreign_cluster_drops.load(Ordering::Relaxed),
            suspect_promotions:     i.suspect_promotions.load(Ordering::Relaxed),
            dead_promotions:        i.dead_promotions.load(Ordering::Relaxed),
            rpc_in:                 i.rpc_in.load(Ordering::Relaxed),
            rpc_out:                i.rpc_out.load(Ordering::Relaxed),
            rpc_timeouts:           i.rpc_timeouts.load(Ordering::Relaxed),
            rpc_errors:             i.rpc_errors.load(Ordering::Relaxed),
            join_calls:             i.join_calls.load(Ordering::Relaxed),
            supervisor_restarts:    i.supervisor_restarts.load(Ordering::Relaxed),
        }
    }

    /// Render a Prometheus-style text exposition of every counter.
    /// Intended for the future `/metrics` endpoint and useful in
    /// tests as a stable string snapshot.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let s = self.snapshot();
        let mut out = String::new();
        for (k, v) in [
            ("celmesh_gossip_sent_total",            s.gossip_sent),
            ("celmesh_gossip_recv_total",            s.gossip_recv),
            ("celmesh_decode_errors_total",          s.decode_errors),
            ("celmesh_foreign_cluster_drops_total",  s.foreign_cluster_drops),
            ("celmesh_suspect_promotions_total",     s.suspect_promotions),
            ("celmesh_dead_promotions_total",        s.dead_promotions),
            ("celmesh_rpc_in_total",                 s.rpc_in),
            ("celmesh_rpc_out_total",                s.rpc_out),
            ("celmesh_rpc_timeouts_total",           s.rpc_timeouts),
            ("celmesh_rpc_errors_total",             s.rpc_errors),
            ("celmesh_join_calls_total",             s.join_calls),
            ("celmesh_supervisor_restarts_total",    s.supervisor_restarts),
        ] {
            out.push_str(&format!("# TYPE {k} counter\n{k} {v}\n"));
        }
        out
    }

    // -- mutation helpers (crate-private) ---------------------------------

    pub(crate) fn inc_gossip_sent(&self)            { self.inner.gossip_sent.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_gossip_recv(&self)            { self.inner.gossip_recv.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_decode_errors(&self)          { self.inner.decode_errors.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_foreign_cluster_drops(&self)  { self.inner.foreign_cluster_drops.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_suspect_promotions(&self, n: u64) {
        if n > 0 { self.inner.suspect_promotions.fetch_add(n, Ordering::Relaxed); }
    }
    pub(crate) fn inc_dead_promotions(&self, n: u64) {
        if n > 0 { self.inner.dead_promotions.fetch_add(n, Ordering::Relaxed); }
    }
    pub(crate) fn inc_rpc_in(&self)                 { self.inner.rpc_in.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_rpc_out(&self)                { self.inner.rpc_out.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_rpc_timeouts(&self)           { self.inner.rpc_timeouts.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_rpc_errors(&self)             { self.inner.rpc_errors.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_join_calls(&self)             { self.inner.join_calls.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn inc_supervisor_restarts(&self, n: u64) {
        if n > 0 { self.inner.supervisor_restarts.fetch_add(n, Ordering::Relaxed); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_zero_by_default() {
        let m = MeshMetrics::new();
        assert_eq!(m.snapshot(), MeshMetricsSnapshot::default());
    }

    #[test]
    fn counters_only_increase() {
        let m = MeshMetrics::new();
        m.inc_gossip_sent();
        m.inc_gossip_sent();
        m.inc_gossip_recv();
        m.inc_dead_promotions(3);
        let s = m.snapshot();
        assert_eq!(s.gossip_sent, 2);
        assert_eq!(s.gossip_recv, 1);
        assert_eq!(s.dead_promotions, 3);
    }

    #[test]
    fn prometheus_render_lists_every_metric() {
        let m = MeshMetrics::new();
        m.inc_rpc_out();
        let txt = m.render_prometheus();
        assert!(txt.contains("celmesh_rpc_out_total 1"));
        assert!(txt.contains("celmesh_dead_promotions_total 0"));
        assert!(txt.contains("# TYPE celmesh_gossip_sent_total counter"));
    }
}
