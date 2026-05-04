//! Membership table.
//!
//! Each node maintains a fixed-size set of `NodeInfo` rows. The local
//! row is created at construction; remote rows arrive via gossip and
//! are merged using last-writer-wins on the `(epoch, hlc)` pair —
//! `epoch` is monotonic per source node so a restart strictly wins
//! over any pre-restart state.
//!
//! Failure detection is intentionally simple for v0.1: a node that
//! has not been heard from for more than `timeout_suspect` is moved
//! to `Suspect`; after `timeout_dead` it is moved to `Dead`. The
//! Phi-accrual style detector in S-02 of the V Substrate plan is the
//! reference design we will adopt later.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Stable per-node identifier. `String` is convenient and lets the
/// CLI dump it; the wire format pins it as UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// Borrow the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

impl core::fmt::Display for NodeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Health state of a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeStatus {
    /// Heard from recently.
    Alive,
    /// Missed recent gossip but still tentatively a member.
    Suspect,
    /// Declared dead by failure detector.
    Dead,
    /// Voluntarily departed (`Goodbye`).
    Left,
}

/// One row of the membership table — fully gossipable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Stable identifier.
    pub id: NodeId,
    /// Address peers use to reach this node. Free-form so both UDP
    /// `host:port` and the in-memory `mem://...` URIs work.
    pub addr: String,
    /// Restart counter — monotonic per node.
    pub epoch: u64,
    /// HLC value at the moment this row was last updated by its
    /// owning node. Combined with `epoch` for LWW merge.
    pub hlc: u64,
    /// Current health.
    pub status: NodeStatus,
}

impl NodeInfo {
    /// Compare two rows for "newer than". Returns `true` if `self`
    /// strictly supersedes `other`.
    #[must_use]
    pub fn supersedes(&self, other: &Self) -> bool {
        (self.epoch, self.hlc) > (other.epoch, other.hlc)
    }
}

/// Local clock state. Tracks last-seen timestamps so the failure
/// detector can promote rows between states without polluting the
/// gossipable `NodeInfo`.
#[derive(Debug, Clone)]
struct LocalState {
    last_seen: Instant,
}

/// Membership table. Cheaply cloneable across an `Arc<Mutex<_>>`.
#[derive(Debug)]
pub struct Membership {
    self_id:        NodeId,
    cluster:        String,
    rows:           BTreeMap<NodeId, NodeInfo>,
    locals:         BTreeMap<NodeId, LocalState>,
    timeout_suspect: Duration,
    timeout_dead:    Duration,
}

impl Membership {
    /// Create a fresh table with `self_info` already inserted as the
    /// local row.
    #[must_use]
    pub fn new(
        cluster: impl Into<String>,
        self_info: NodeInfo,
        timeout_suspect: Duration,
        timeout_dead: Duration,
    ) -> Self {
        let self_id = self_info.id.clone();
        let mut rows = BTreeMap::new();
        let mut locals = BTreeMap::new();
        rows.insert(self_id.clone(), self_info);
        locals.insert(self_id.clone(), LocalState { last_seen: Instant::now() });
        Self {
            self_id,
            cluster: cluster.into(),
            rows,
            locals,
            timeout_suspect,
            timeout_dead,
        }
    }

    /// Identifier of this node.
    #[must_use]
    pub fn self_id(&self) -> &NodeId { &self.self_id }

    /// Cluster name.
    #[must_use]
    pub fn cluster(&self) -> &str { &self.cluster }

    /// Snapshot of every known row, in stable id order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<NodeInfo> {
        self.rows.values().cloned().collect()
    }

    /// Number of rows currently classified as `Alive`.
    #[must_use]
    pub fn alive_count(&self) -> usize {
        self.rows.values().filter(|r| r.status == NodeStatus::Alive).count()
    }

    /// Lookup a single row.
    #[must_use]
    pub fn get(&self, id: &NodeId) -> Option<&NodeInfo> {
        self.rows.get(id)
    }

    /// Bump our own row — used after we observe an external event
    /// that we want every peer to learn about.
    pub fn bump_self(&mut self, hlc: u64) {
        if let Some(row) = self.rows.get_mut(&self.self_id) {
            row.hlc = row.hlc.max(hlc);
            row.status = NodeStatus::Alive;
        }
        if let Some(l) = self.locals.get_mut(&self.self_id) {
            l.last_seen = Instant::now();
        }
    }

    /// Merge an incoming row using LWW semantics. Returns `true` if
    /// the local table changed.
    pub fn merge(&mut self, incoming: NodeInfo) -> bool {
        // Never let a peer overwrite our own row.
        if incoming.id == self.self_id { return false; }

        let touch = match self.rows.get(&incoming.id) {
            Some(existing) => incoming.supersedes(existing),
            None           => true,
        };
        if touch {
            self.locals.insert(
                incoming.id.clone(),
                LocalState { last_seen: Instant::now() },
            );
            self.rows.insert(incoming.id.clone(), incoming);
        } else if self.rows.contains_key(&incoming.id) {
            // Same epoch/hlc — we still saw a heartbeat.
            if let Some(l) = self.locals.get_mut(&incoming.id) {
                l.last_seen = Instant::now();
            }
        }
        touch
    }

    /// Mark `id` as `Left`. Idempotent.
    pub fn mark_left(&mut self, id: &NodeId) {
        if id == &self.self_id { return; }
        if let Some(row) = self.rows.get_mut(id) {
            row.status = NodeStatus::Left;
        }
    }

    /// Run the failure detector. Returns counts of state changes
    /// observed during this tick. W17 widened the return type from a
    /// bare `usize` so the mesh metrics layer can record `Alive →
    /// Suspect` and `* → Dead` transitions in O(1) per tick without
    /// having to snapshot the whole table.
    pub fn tick(&mut self, now: Instant) -> TickDelta {
        let mut delta = TickDelta::default();
        for (id, row) in self.rows.iter_mut() {
            if id == &self.self_id { continue; }
            if matches!(row.status, NodeStatus::Left | NodeStatus::Dead) {
                continue;
            }
            let last = self.locals.get(id).map(|l| l.last_seen).unwrap_or(now);
            let elapsed = now.saturating_duration_since(last);
            let next = if elapsed >= self.timeout_dead {
                NodeStatus::Dead
            } else if elapsed >= self.timeout_suspect {
                NodeStatus::Suspect
            } else {
                NodeStatus::Alive
            };
            if next != row.status {
                delta.state_changes += 1;
                if row.status == NodeStatus::Alive && next == NodeStatus::Suspect {
                    delta.suspect_promotions += 1;
                }
                if next == NodeStatus::Dead {
                    delta.dead_promotions += 1;
                }
                row.status = next;
            }
        }
        delta
    }
}

/// Output of a single failure-detector tick. Aggregated by the mesh
/// metrics layer; tests assert against `state_changes`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickDelta {
    /// Total number of rows whose status changed this tick.
    pub state_changes: usize,
    /// Subset of `state_changes`: Alive → Suspect transitions.
    pub suspect_promotions: usize,
    /// Subset of `state_changes`: any → Dead transitions.
    pub dead_promotions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, epoch: u64, hlc: u64, status: NodeStatus) -> NodeInfo {
        NodeInfo {
            id: NodeId::from(id),
            addr: format!("mem://{id}"),
            epoch,
            hlc,
            status,
        }
    }

    fn mk() -> Membership {
        Membership::new(
            "test",
            row("self", 1, 0, NodeStatus::Alive),
            Duration::from_millis(50),
            Duration::from_millis(100),
        )
    }

    #[test]
    fn merge_inserts_then_supersedes() {
        let mut m = mk();
        assert!(m.merge(row("peer", 1, 1, NodeStatus::Alive)));
        assert!(!m.merge(row("peer", 1, 1, NodeStatus::Alive))); // same hlc
        assert!(m.merge(row("peer", 1, 5, NodeStatus::Alive)));
        assert!(m.merge(row("peer", 2, 0, NodeStatus::Alive))); // new epoch wins
    }

    #[test]
    fn never_overwrites_self() {
        let mut m = mk();
        let injected = row("self", 999, 999, NodeStatus::Dead);
        assert!(!m.merge(injected));
        assert_eq!(m.get(&NodeId::from("self")).unwrap().status, NodeStatus::Alive);
    }

    #[test]
    fn tick_promotes_suspect_then_dead() {
        let mut m = mk();
        m.merge(row("peer", 1, 1, NodeStatus::Alive));

        // Simulate elapsed time by rewriting last_seen.
        let id = NodeId::from("peer");
        m.locals.get_mut(&id).unwrap().last_seen =
            Instant::now() - Duration::from_millis(60);
        m.tick(Instant::now());
        assert_eq!(m.get(&id).unwrap().status, NodeStatus::Suspect);

        m.locals.get_mut(&id).unwrap().last_seen =
            Instant::now() - Duration::from_millis(150);
        m.tick(Instant::now());
        assert_eq!(m.get(&id).unwrap().status, NodeStatus::Dead);
    }

    #[test]
    fn mark_left_is_idempotent() {
        let mut m = mk();
        m.merge(row("peer", 1, 1, NodeStatus::Alive));
        let id = NodeId::from("peer");
        m.mark_left(&id);
        m.mark_left(&id);
        assert_eq!(m.get(&id).unwrap().status, NodeStatus::Left);
    }

    #[test]
    fn tick_delta_counts_promotions() {
        let mut m = mk();
        m.merge(row("peer", 1, 1, NodeStatus::Alive));
        let id = NodeId::from("peer");
        // Push the row past the suspect threshold.
        m.locals.get_mut(&id).unwrap().last_seen =
            Instant::now() - Duration::from_millis(60);
        let d = m.tick(Instant::now());
        assert_eq!(d.state_changes, 1);
        assert_eq!(d.suspect_promotions, 1);
        assert_eq!(d.dead_promotions, 0);

        // Past the dead threshold.
        m.locals.get_mut(&id).unwrap().last_seen =
            Instant::now() - Duration::from_millis(150);
        let d = m.tick(Instant::now());
        assert_eq!(d.state_changes, 1);
        assert_eq!(d.suspect_promotions, 0);
        assert_eq!(d.dead_promotions, 1);

        // No further changes — already Dead.
        let d = m.tick(Instant::now());
        assert_eq!(d, TickDelta::default());
    }
}
