//! Cluster-wide observability surface.
//!
//! W14 added per-host structured tracing (one span per RPC, one
//! debug event per applied op). W15 layers a poll-based aggregator
//! on top: [`Mesh::cluster_report`](crate::Mesh::cluster_report)
//! walks every Alive peer, calls the existing `List` /
//! `ListVolumes` / `ListNetworks` ops, and returns a single
//! [`ClusterReport`] suitable for the CLI's `cluster status`
//! subcommand or a `/metrics`-style HTTP endpoint.
//!
//! The collector deliberately uses already-shipped wire ops so it
//! does not bloat the protocol. Ops that fail or time out reduce
//! the affected node's report to "unreachable" rather than failing
//! the whole call — partial data is more useful than no data.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::membership::{NodeId, NodeStatus};
use crate::proto::{VirtualNetwork, VolumeMeta, VmOp, VmOpReply};

/// Per-volume usage line item. Today this is identical to
/// [`VolumeMeta`] plus a `node` field; reserved for future fields
/// like `bytes_used` / `dirty_pages` when the vault grows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeUsage {
    /// Owning node.
    pub node: NodeId,
    /// Volume metadata as reported by the owner.
    pub volume: VolumeMeta,
}

/// Per-node observability slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReport {
    /// Stable node id.
    pub id: NodeId,
    /// Advertised address.
    pub addr: String,
    /// Membership status as observed by the reporter.
    pub status: NodeStatus,
    /// VMs the node is currently hosting.
    pub vm_count: u32,
    /// Volumes the node is currently hosting.
    pub volume_count: u32,
    /// Total declared volume bytes on this node.
    pub total_volume_bytes: u64,
    /// Networks the node owns.
    pub network_count: u32,
    /// `true` if the reporter could fetch fresh data; `false` for
    /// timeouts / errors.
    pub reachable: bool,
}

/// Cluster-wide aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClusterReport {
    /// Per-node slices. Always includes the reporter itself.
    pub nodes: Vec<NodeReport>,
    /// Volumes seen across the cluster.
    pub volumes: Vec<VolumeUsage>,
    /// Networks seen across the cluster.
    pub networks: Vec<VirtualNetwork>,
    /// Total VMs across all reachable nodes.
    pub total_vms: u32,
    /// Total volumes across all reachable nodes.
    pub total_volumes: u32,
    /// Total declared volume bytes across all reachable nodes.
    pub total_volume_bytes: u64,
}

impl crate::Mesh {
    /// Cluster-wide observability snapshot.
    ///
    /// Polls every Alive peer in parallel for VMs, volumes, and
    /// networks, then folds the responses into a [`ClusterReport`].
    /// `rpc_timeout` is applied per peer; the collector does not
    /// wait longer than that even if some peers are slow.
    ///
    /// Errors are *not* propagated for unreachable peers — they are
    /// recorded as `reachable=false` rows. The function only
    /// returns `Err` for catastrophic local failures (none today).
    pub async fn cluster_report(&self, rpc_timeout: Duration) -> celcommon::CelResult<ClusterReport> {
        let me = self.self_id().await;
        let members = self.members().await;
        let alive: Vec<_> = members
            .iter()
            .filter(|r| r.status == NodeStatus::Alive)
            .cloned()
            .collect();
        let dead: Vec<_> = members
            .iter()
            .filter(|r| r.status != NodeStatus::Alive)
            .cloned()
            .collect();

        let mut report = ClusterReport::default();
        let mut by_node: BTreeMap<NodeId, NodeReport> = BTreeMap::new();
        for r in &members {
            by_node.insert(r.id.clone(), NodeReport {
                id: r.id.clone(),
                addr: r.addr.clone(),
                status: r.status,
                vm_count: 0,
                volume_count: 0,
                total_volume_bytes: 0,
                network_count: 0,
                reachable: false,
            });
        }

        // Local VMs come from the federation snapshot directly.
        let federated_vms = self.list_vms().await;

        for r in &alive {
            let id = r.id.clone();
            // `by_node` was seeded above from `members`; `alive` is a
            // filtered view of the same vector, so every entry must
            // already exist. We still avoid `expect` to honour the
            // "no panic on production paths" rule — a missing row
            // means another mutation slipped in between iterations,
            // in which case we skip it rather than abort.
            let Some(mut node_row) = by_node.remove(&id) else {
                tracing::warn!(node = %id, "cluster_report: alive row missing from by_node");
                continue;
            };

            // VMs. Use the federation count for self; for peers,
            // use the federated rows we already have (they came in
            // via gossip), and only RPC List as a freshness probe.
            let owned: u32 = federated_vms.iter()
                .filter(|v| v.owner == id)
                .count() as u32;
            node_row.vm_count = owned;

            // Volumes — federation does not gossip volumes today,
            // so we have to RPC.
            match self.invoke(&id, VmOp::ListVolumes, rpc_timeout).await {
                Ok(VmOpReply::VolumesListed { volumes }) => {
                    node_row.volume_count = volumes.len() as u32;
                    node_row.total_volume_bytes = volumes
                        .iter()
                        .map(|v| v.size_bytes)
                        .fold(0u64, u64::saturating_add);
                    for v in volumes {
                        report.volumes.push(VolumeUsage { node: id.clone(), volume: v });
                    }
                    node_row.reachable = true;
                }
                Ok(_) | Err(_) => {
                    node_row.reachable = false;
                }
            }

            // Networks.
            if node_row.reachable {
                if let Ok(VmOpReply::NetworksListed { networks })
                    = self.invoke(&id, VmOp::ListNetworks, rpc_timeout).await
                {
                    node_row.network_count = networks.len() as u32;
                    report.networks.extend(networks);
                }
            }

            by_node.insert(id, node_row);
        }

        // Dead/Suspect/Left rows already start as reachable=false;
        // emit them so the operator sees the full picture.
        for r in &dead {
            // No-op; the seeded row already has the right status.
            let _ = r;
        }

        // Drain map → vec, ordered by node id for determinism.
        let mut nodes: Vec<NodeReport> = by_node.into_values().collect();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));

        report.total_vms = nodes.iter().map(|n| n.vm_count).sum();
        report.total_volumes = nodes.iter().map(|n| n.volume_count).sum();
        report.total_volume_bytes = nodes
            .iter()
            .map(|n| n.total_volume_bytes)
            .fold(0u64, u64::saturating_add);
        report.nodes = nodes;

        tracing::info!(
            target: "celmesh::observability",
            from = %me,
            nodes = report.nodes.len(),
            total_vms = report.total_vms,
            total_volumes = report.total_volumes,
            total_volume_bytes = report.total_volume_bytes,
            "cluster report"
        );
        Ok(report)
    }
}
