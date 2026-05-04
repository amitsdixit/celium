//! Federated VM namespace.
//!
//! Each node owns a small set of VMs. The federation layer keeps a
//! union of every node's owned-VM list so that a CLI on any node can
//! list and address VMs cluster-wide using paths of the form:
//!
//! ```text
//! /cluster/<node-id>/vms/<n>
//! ```
//!
//! Reconciliation rules:
//!
//! * Each row carries the owning node's `(epoch, hlc)`. LWW.
//! * When a node is marked `Left` or `Dead`, its rows are kept but
//!   tagged via [`RemoteVm::owner_alive`] = `false` so the operator
//!   sees the last-known state.
//! * Local rows (owned by this node) are authoritative — never
//!   overwritten by a peer.

use std::collections::BTreeMap;

use celvault::VolumeAttachment;
use serde::{Deserialize, Serialize};

use crate::membership::NodeId;

/// Restart behaviour the supervisor applies when the VM's owning
/// node is no longer Alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Do nothing — leave the row visible with `owner_alive=false`.
    /// This is the default; matches v0.1 single-node behaviour.
    #[default]
    Never,
    /// On owner failure the elected supervisor recreates an
    /// equivalent VM on its own node.
    Always,
}

/// One federated VM record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteVm {
    /// Node that owns the VM.
    pub owner: NodeId,
    /// Numeric id within the owning node.
    pub vm_id: u32,
    /// Free-form label.
    pub label: String,
    /// Stable lifecycle tag matching `celcli::vm::VmState::tag`.
    pub state: String,
    /// Optional last basic exit reason.
    pub last_exit: Option<u32>,
    /// Owner's `(epoch, hlc)` at the moment this row was generated.
    /// Only used for LWW; never compared across owners.
    pub epoch: u64,
    /// Owner-side hybrid clock value.
    pub hlc: u64,
    /// Set by the receiver — `false` means the owner is Suspect/Dead/Left.
    /// Not gossiped on the wire (it's a function of the local
    /// membership view), but populated by [`NamespaceFederation::list`].
    #[serde(default = "default_owner_alive")]
    pub owner_alive: bool,
    /// Restart policy. Defaults to `Never` for backwards-compat.
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    /// Week-12: volume attachments propagated alongside the VM row.
    /// Empty by default to stay wire-compatible with W11 senders.
    #[serde(default)]
    pub volumes: Vec<VolumeAttachment>,
}

fn default_owner_alive() -> bool { true }

impl RemoteVm {
    /// Render this row's federated path.
    #[must_use]
    pub fn path(&self) -> String {
        format!("/cluster/{}/vms/{}", self.owner, self.vm_id)
    }

    fn key(&self) -> (NodeId, u32) {
        (self.owner.clone(), self.vm_id)
    }
}

/// Federation table.
#[derive(Debug)]
pub struct NamespaceFederation {
    self_id: NodeId,
    rows:    BTreeMap<(NodeId, u32), RemoteVm>,
}

impl NamespaceFederation {
    /// New federation owned by `self_id`.
    #[must_use]
    pub fn new(self_id: NodeId) -> Self {
        Self { self_id, rows: BTreeMap::new() }
    }

    /// Replace this node's own rows wholesale. Called whenever the
    /// local controller's state changes — cheaper than diffing.
    pub fn set_local(&mut self, mut local: Vec<RemoteVm>) {
        // Drop any prior rows owned by us.
        self.rows.retain(|(owner, _), _| owner != &self.self_id);
        for row in local.drain(..) {
            // Defensively rewrite the owner so callers cannot inject
            // someone else's rows via this path.
            let mut row = row;
            row.owner = self.self_id.clone();
            row.owner_alive = true;
            self.rows.insert(row.key(), row);
        }
    }

    /// Merge a single incoming row using LWW. Returns `true` if the
    /// local table changed. Local rows are never overwritten.
    pub fn merge(&mut self, mut incoming: RemoteVm) -> bool {
        if incoming.owner == self.self_id { return false; }
        // Receivers always recompute owner_alive from membership.
        incoming.owner_alive = true;
        let key = incoming.key();
        match self.rows.get(&key) {
            Some(existing)
                if (existing.epoch, existing.hlc) >= (incoming.epoch, incoming.hlc) => false,
            _ => {
                self.rows.insert(key, incoming);
                true
            }
        }
    }

    /// Stamp `owner_alive` on every row using a closure that resolves
    /// the owner's current liveness from the membership table.
    pub fn refresh_alive(&mut self, mut alive_of: impl FnMut(&NodeId) -> bool) {
        for row in self.rows.values_mut() {
            row.owner_alive = if row.owner == self.self_id { true }
                              else                          { alive_of(&row.owner) };
        }
    }

    /// Snapshot of every known VM in the cluster.
    #[must_use]
    pub fn list(&self) -> Vec<RemoteVm> {
        self.rows.values().cloned().collect()
    }

    /// Local rows only — used when this node serialises its delta.
    #[must_use]
    pub fn local_rows(&self) -> Vec<RemoteVm> {
        self.rows.values().filter(|r| r.owner == self.self_id).cloned().collect()
    }

    /// Resolve a federated path `"/cluster/<node>/vms/<n>"` to a row.
    ///
    /// Returns `Ok(None)` for syntactically valid but unallocated
    /// paths, and `Err(())` for malformed paths so the caller can
    /// surface the right error variant.
    #[allow(clippy::result_unit_err)]
    pub fn resolve(&self, path: &str) -> Result<Option<&RemoteVm>, ()> {
        let suffix = path.strip_prefix("/cluster/").ok_or(())?;
        let (node, rest) = suffix.split_once('/').ok_or(())?;
        let rest = rest.strip_prefix("vms/").ok_or(())?;
        if rest.is_empty() || rest.contains('/') { return Err(()); }
        let n: u32 = rest.parse().map_err(|_| ())?;
        let key = (NodeId::from(node), n);
        Ok(self.rows.get(&key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(owner: &str, id: u32, hlc: u64) -> RemoteVm {
        RemoteVm {
            owner: NodeId::from(owner),
            vm_id: id,
            label: format!("{owner}-{id}"),
            state: "created".into(),
            last_exit: None,
            epoch: 1,
            hlc,
            owner_alive: true,
            restart_policy: RestartPolicy::Never,
            volumes: Vec::new(),
        }
    }

    #[test]
    fn local_replace_wipes_prior_local_rows() {
        let mut f = NamespaceFederation::new(NodeId::from("a"));
        f.set_local(vec![vm("a", 0, 1), vm("a", 1, 1)]);
        f.set_local(vec![vm("a", 0, 2)]);
        let list = f.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].vm_id, 0);
        assert_eq!(list[0].hlc, 2);
    }

    #[test]
    fn merge_lww_orders_by_epoch_then_hlc() {
        let mut f = NamespaceFederation::new(NodeId::from("a"));
        assert!(f.merge(vm("b", 0, 1)));
        assert!(!f.merge(vm("b", 0, 1))); // not strictly newer
        assert!(f.merge(vm("b", 0, 2)));
        let mut newer_epoch = vm("b", 0, 0);
        newer_epoch.epoch = 7;
        assert!(f.merge(newer_epoch));
    }

    #[test]
    fn local_rows_cannot_be_overwritten_by_peer() {
        let mut f = NamespaceFederation::new(NodeId::from("a"));
        f.set_local(vec![vm("a", 0, 5)]);
        let mut spoof = vm("a", 0, 99);
        spoof.label = "spoofed".into();
        assert!(!f.merge(spoof));
        assert_eq!(f.list()[0].label, "a-0");
    }

    #[test]
    fn resolve_path_round_trip() {
        let mut f = NamespaceFederation::new(NodeId::from("a"));
        f.set_local(vec![vm("a", 7, 1)]);
        let row = f.list()[0].clone();
        assert_eq!(row.path(), "/cluster/a/vms/7");
        assert!(f.resolve("/cluster/a/vms/7").unwrap().is_some());
        assert!(f.resolve("/cluster/a/vms/8").unwrap().is_none());
        assert!(f.resolve("/wrong").is_err());
        assert!(f.resolve("/cluster/a/vms/").is_err());
    }
}
