//! Host-side hook for cross-node VM operations.
//!
//! A `VmHost` is whatever runs the actual single-node VM lifecycle —
//! today that's the in-memory `celcli::vm::Controller`, tomorrow the
//! real CelHyper manager.
//!
//! `Mesh::set_host` registers an implementation; whenever the gossip
//! receiver decodes a `Payload::Request` whose `target` matches our
//! own id, the mesh dispatches the operation to the host and ships
//! the reply back over the same transport.
//!
//! The trait is `async`-shaped without pulling in `async-trait` —
//! every method returns a pinned boxed future, mirroring the style
//! used by [`crate::transport::Transport`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use celvault::{
    Cidr4, MemNetworkStore, MemVolumeStore, NetworkStore, VolumeAttachment, VolumeStore,
};

use crate::capabilities::Capabilities;
use crate::federation::{RemoteVm, RestartPolicy};
use crate::membership::NodeId;
use crate::proto::{VmOp, VmOpReply};

/// Boxed future result alias used by the host trait.
pub type HostFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Operation outcome surfaced to callers. Errors are stringly-typed
/// because they cross a node boundary; callers map them back into
/// `CelError::Io` or `CelError::Invalid` as appropriate.
pub type HostResult = Result<VmOpReply, String>;

/// Trait implemented by anything that can run a VM lifecycle on a
/// node. The mesh calls `handle` to apply an op and `snapshot` after
/// every successful op so it can republish the federated rows.
pub trait VmHost: Send + Sync {
    /// Apply `op`. Implementations must not panic.
    fn handle<'a>(&'a self, op: VmOp) -> HostFut<'a, HostResult>;
    /// Return the host's current local-VM list.
    fn snapshot<'a>(&'a self, owner: &'a NodeId) -> HostFut<'a, Vec<RemoteVm>>;
    /// Week-12: install a list of preserved volume attachments on
    /// `vm_id` without consulting the local vault. Used by the
    /// supervisor when reviving an orphan VM whose volumes may live
    /// on a third (still-Alive) node. The default returns an error
    /// so legacy `VmHost` impls remain compile-compatible.
    fn attach_preserved<'a>(
        &'a self,
        _vm_id: u32,
        _attachments: Vec<VolumeAttachment>,
    ) -> HostFut<'a, Result<(), String>> {
        Box::pin(async move { Err("attach_preserved: unsupported".to_string()) })
    }
}

// ---------------------------------------------------------------------------
// Reference in-memory implementation. Used by tests and by the CLI's
// `cluster start` command when no kernel-side IPC is wired yet.
// ---------------------------------------------------------------------------

/// Maximum slots — must agree with `celcli::vm::MAX_VMS`.
const MAX_SLOTS: usize = 4;

/// Per-call cap on `ReadVolume` / `WriteVolume` payloads. Chosen so a
/// single op fits inside the protocol's 64 KiB frame budget after
/// JSON + base64 framing overhead.
const MAX_VOLUME_IO_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone)]
struct Slot {
    label: String,
    state: &'static str,
    last_exit: Option<u32>,
    restart_policy: RestartPolicy,
    /// Week-12: volume attachments. Preserved across supervisor
    /// restarts so a recreated VM keeps its persistent volumes.
    volumes: Vec<VolumeAttachment>,
    /// W22-v2: optional image / config metadata carried verbatim
    /// from `VmOp::Create`. Surfaced through `snapshot` so federated
    /// peers see the same view a real kernel bridge would expose.
    image_path: Option<String>,
    cpu_count: Option<u32>,
    memory_mib: Option<u64>,
    boot_blob_crc32c: Option<u32>,
}

/// Reference in-memory `VmHost`.
///
/// Models the same state transitions as `celcli::vm::Controller` —
/// `Created` → `start` → `Halted` (exit 12) → `stop` (idempotent).
/// Carries an embedded [`VolumeStore`] so volume CRUD and
/// attach/detach ops can be served without round-tripping through a
/// separate vault handle.
pub struct MemVmHost {
    slots: Mutex<[Option<Slot>; MAX_SLOTS]>,
    vault: Arc<dyn VolumeStore>,
    /// W15: per-node networking control-plane store. Holds virtual
    /// networks, NICs, security groups, and load balancers.
    nets:  Arc<dyn NetworkStore>,
    /// Owner node id used when minting volume ids. Set the first
    /// time `snapshot` runs so `MemVolumeStore` ids match the
    /// convention `<node>/v<n>`.
    owner: Mutex<Option<String>>,
    /// W14: capabilities granted to peers invoking this host.
    /// Default is [`Capabilities::ALL`] for back-compat.
    caps: Capabilities,
}

impl Default for MemVmHost {
    fn default() -> Self { Self::new() }
}

impl MemVmHost {
    /// Construct an empty host with a fresh in-memory volume store.
    #[must_use]
    pub fn new() -> Self {
        Self::with_vault(Arc::new(MemVolumeStore::new()))
    }

    /// Construct an empty host using an explicit volume store. Useful
    /// for tests that pre-seed volumes or share a store between hosts.
    #[must_use]
    pub fn with_vault(vault: Arc<dyn VolumeStore>) -> Self {
        Self {
            slots: Mutex::new(Default::default()),
            vault,
            nets:  Arc::new(MemNetworkStore::new()),
            owner: Mutex::new(None),
            caps:  Capabilities::ALL,
        }
    }

    /// W15: construct a host with explicit volume **and** network
    /// stores. Tests and the K8s personality use this to seed both
    /// stores up-front.
    #[must_use]
    pub fn with_stores(
        vault: Arc<dyn VolumeStore>,
        nets:  Arc<dyn NetworkStore>,
    ) -> Self {
        Self {
            slots: Mutex::new(Default::default()),
            vault,
            nets,
            owner: Mutex::new(None),
            caps:  Capabilities::ALL,
        }
    }

    /// W14: replace the capability set granted to peers. Returns
    /// `self` so it composes with [`Self::new`] / [`Self::with_vault`].
    #[must_use]
    pub fn with_caps(mut self, caps: Capabilities) -> Self {
        self.caps = caps;
        self
    }

    /// Borrow the capability set. Useful for tests / status RPCs.
    #[must_use]
    pub fn caps(&self) -> Capabilities { self.caps }

    /// Borrow the volume store. Useful for tests.
    #[must_use]
    pub fn vault(&self) -> Arc<dyn VolumeStore> { self.vault.clone() }

    /// W15: borrow the networking store. Useful for tests and the
    /// K8s personality which inspects allocated NICs locally.
    #[must_use]
    pub fn nets(&self) -> Arc<dyn NetworkStore> { self.nets.clone() }

    fn lock_slots(&self) -> std::sync::MutexGuard<'_, [Option<Slot>; MAX_SLOTS]> {
        // SAFETY-comment scope only: this is `std::sync::Mutex`. A
        // poisoned guard means another thread panicked while holding
        // the lock — no `unsafe` is involved. We surface the panic by
        // taking the inner value, since no production path can poison
        // it (no panicking code runs under the guard).
        match self.slots.lock() {
            Ok(g)   => g,
            Err(p)  => p.into_inner(),
        }
    }

    fn remember_owner(&self, owner: &NodeId) {
        // No `unsafe`. Same panic-recovery pattern as `lock_slots`.
        let mut g = match self.owner.lock() {
            Ok(g)  => g,
            Err(p) => p.into_inner(),
        };
        if g.is_none() {
            *g = Some(owner.to_string());
        }
    }

    fn current_owner(&self) -> Option<String> {
        match self.owner.lock() {
            Ok(g)  => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    fn apply(&self, op: VmOp) -> HostResult {
        let tag = Capabilities::op_tag(&op);
        let needed = Capabilities::required(&op);
        if !self.caps.contains(needed) {
            tracing::warn!(target: "celmesh::host", op = tag, "capability denied");
            return Err(format!("capability denied: {tag}"));
        }
        tracing::debug!(target: "celmesh::host", op = tag, "apply");
        match op {
            // -- VM lifecycle ---------------------------------------------
            VmOp::Create { label, restart_policy, image_path, cpu_count, memory_mib, boot_blob_crc32c } => {
                let mut slots = self.lock_slots();
                if label.len() > 32 {
                    return Err("label > 32 chars".into());
                }
                for (i, s) in slots.iter_mut().enumerate() {
                    if s.is_none() {
                        *s = Some(Slot {
                            label,
                            state: "created",
                            last_exit: None,
                            restart_policy,
                            volumes: Vec::new(),
                            image_path,
                            cpu_count,
                            memory_mib,
                            boot_blob_crc32c,
                        });
                        return Ok(VmOpReply::Created { vm_id: i as u32 });
                    }
                }
                Err("vm registry full".into())
            }
            VmOp::Start { vm_id } => {
                let mut slots = self.lock_slots();
                let s = Self::slot_mut(&mut slots, vm_id)?;
                if matches!(s.state, "halted" | "stopped" | "faulted") {
                    return Err("vm already terminal".into());
                }
                if s.state == "running" {
                    return Err("vm already running".into());
                }
                s.state = "halted";
                s.last_exit = Some(12);
                Ok(VmOpReply::State { vm_id, state: s.state.into() })
            }
            VmOp::Stop { vm_id } => {
                let mut slots = self.lock_slots();
                let s = Self::slot_mut(&mut slots, vm_id)?;
                if !matches!(s.state, "halted" | "stopped" | "faulted") {
                    s.state = "stopped";
                }
                Ok(VmOpReply::State { vm_id, state: s.state.into() })
            }
            VmOp::Delete { vm_id } => {
                let mut slots = self.lock_slots();
                let i = vm_id as usize;
                if i >= MAX_SLOTS {
                    return Err("vm id out of range".into());
                }
                let s = slots[i].as_ref().ok_or_else(|| "vm not allocated".to_string())?;
                if !matches!(s.state, "halted" | "stopped" | "faulted") {
                    return Err("vm not terminal; stop first".into());
                }
                slots[i] = None;
                Ok(VmOpReply::Deleted { vm_id })
            }
            VmOp::List => Ok(VmOpReply::Listed { rows: Vec::new() }),
            // -- Week-12 volume ops ---------------------------------------
            VmOp::CreateVolume { name, size_bytes } => {
                let owner = self.current_owner()
                    .ok_or_else(|| "vault: owner not yet known; snapshot first".to_string())?;
                let meta = self.vault
                    .create(&owner, &name, size_bytes)
                    .map_err(|e| format!("vault create: {e:?}"))?;
                Ok(VmOpReply::VolumeCreated { volume: meta })
            }
            VmOp::DeleteVolume { volume_id } => {
                // Reject if still attached to any local VM.
                let slots = self.lock_slots();
                for slot in slots.iter().flatten() {
                    if slot.volumes.iter().any(|a| a.volume_id == volume_id) {
                        return Err("vault delete: volume still attached".into());
                    }
                }
                drop(slots);
                self.vault
                    .delete(&volume_id)
                    .map_err(|e| format!("vault delete: {e:?}"))?;
                Ok(VmOpReply::VolumeDeleted { volume_id })
            }
            VmOp::ListVolumes => {
                Ok(VmOpReply::VolumesListed { volumes: self.vault.list() })
            }
            VmOp::AttachVolume { vm_id, volume_id, mount_name } => {
                if mount_name.len() > celvault::MAX_MOUNT {
                    return Err("attach: mount_name > MAX_MOUNT".into());
                }
                if self.vault.get(&volume_id).is_none() {
                    return Err("attach: unknown volume".into());
                }
                let mut slots = self.lock_slots();
                let s = Self::slot_mut(&mut slots, vm_id)?;
                if s.volumes.iter().any(|a| a.volume_id == volume_id) {
                    return Err("attach: volume already attached".into());
                }
                s.volumes.push(VolumeAttachment { volume_id, mount_name });
                Ok(VmOpReply::Attachments { vm_id, volumes: s.volumes.clone() })
            }
            VmOp::DetachVolume { vm_id, volume_id } => {
                let mut slots = self.lock_slots();
                let s = Self::slot_mut(&mut slots, vm_id)?;
                s.volumes.retain(|a| a.volume_id != volume_id);
                Ok(VmOpReply::Attachments { vm_id, volumes: s.volumes.clone() })
            }
            // -- Week-13 volume IO + snapshots ----------------------------
            VmOp::ReadVolume { volume_id, offset, len } => {
                if len > MAX_VOLUME_IO_BYTES as u64 {
                    return Err("volume io: chunk too large".into());
                }
                let bytes = self.vault
                    .read(&volume_id, offset, len as usize)
                    .map_err(|e| format!("vault read: {e:?}"))?;
                Ok(VmOpReply::VolumeData { volume_id, bytes })
            }
            VmOp::WriteVolume { volume_id, offset, bytes } => {
                if bytes.len() > MAX_VOLUME_IO_BYTES {
                    return Err("volume io: chunk too large".into());
                }
                let written = bytes.len() as u64;
                self.vault
                    .write(&volume_id, offset, &bytes)
                    .map_err(|e| format!("vault write: {e:?}"))?;
                Ok(VmOpReply::VolumeWritten { volume_id, bytes_written: written })
            }
            VmOp::CreateSnapshot { volume_id, name } => {
                let snap = self.vault
                    .create_snapshot(&volume_id, &name)
                    .map_err(|e| format!("vault snapshot: {e:?}"))?;
                Ok(VmOpReply::SnapshotCreated { snapshot: snap })
            }
            VmOp::ListSnapshots { volume_id } => {
                let snaps = self.vault.list_snapshots(volume_id.as_ref());
                Ok(VmOpReply::SnapshotsListed { snapshots: snaps })
            }
            VmOp::DeleteSnapshot { snapshot_id } => {
                self.vault
                    .delete_snapshot(&snapshot_id)
                    .map_err(|e| format!("vault snapshot delete: {e:?}"))?;
                Ok(VmOpReply::SnapshotDeleted { snapshot_id })
            }
            VmOp::RestoreSnapshot { snapshot_id } => {
                self.vault
                    .restore_snapshot(&snapshot_id)
                    .map_err(|e| format!("vault snapshot restore: {e:?}"))?;
                Ok(VmOpReply::SnapshotRestored { snapshot_id })
            }
            // -- W15 networking ops ---------------------------------------
            VmOp::CreateNetwork { name, cidr } => {
                let owner = self.current_owner()
                    .ok_or_else(|| "net: owner not yet known; snapshot first".to_string())?;
                let block = Cidr4::parse(&cidr)
                    .map_err(|e| format!("net create: {e:?}"))?;
                let net = self.nets
                    .create_network(&owner, &name, block)
                    .map_err(|e| format!("net create: {e:?}"))?;
                Ok(VmOpReply::NetworkCreated { network: net })
            }
            VmOp::DeleteNetwork { network_id } => {
                self.nets
                    .delete_network(&network_id)
                    .map_err(|e| format!("net delete: {e:?}"))?;
                Ok(VmOpReply::NetworkDeleted { network_id })
            }
            VmOp::ListNetworks => {
                Ok(VmOpReply::NetworksListed { networks: self.nets.list_networks() })
            }
            VmOp::AttachNic { network_id, vm_id, ip } => {
                // VM must already exist on this host.
                {
                    let mut slots = self.lock_slots();
                    let _ = Self::slot_mut(&mut slots, vm_id)?;
                }
                let parsed = match ip {
                    Some(s) => Some(s.parse().map_err(|_| "net.nic.attach: bad ip")?),
                    None    => None,
                };
                let nic = self.nets
                    .attach_nic(&network_id, vm_id, parsed)
                    .map_err(|e| format!("net.nic.attach: {e:?}"))?;
                Ok(VmOpReply::NicAttached { nic })
            }
            VmOp::DetachNic { nic_id } => {
                self.nets
                    .detach_nic(&nic_id)
                    .map_err(|e| format!("net.nic.detach: {e:?}"))?;
                Ok(VmOpReply::NicDetached { nic_id })
            }
            VmOp::ListNics => {
                Ok(VmOpReply::NicsListed { nics: self.nets.list_nics() })
            }
            // -- W15 security groups --------------------------------------
            VmOp::CreateSecurityGroup { name, rules } => {
                let owner = self.current_owner()
                    .ok_or_else(|| "sg: owner not yet known; snapshot first".to_string())?;
                let sg = self.nets
                    .create_security_group(&owner, &name, rules)
                    .map_err(|e| format!("sg create: {e:?}"))?;
                Ok(VmOpReply::SecurityGroupCreated { sg })
            }
            VmOp::DeleteSecurityGroup { sg_id } => {
                self.nets
                    .delete_security_group(&sg_id)
                    .map_err(|e| format!("sg delete: {e:?}"))?;
                Ok(VmOpReply::SecurityGroupDeleted { sg_id })
            }
            VmOp::ListSecurityGroups => {
                Ok(VmOpReply::SecurityGroupsListed { sgs: self.nets.list_security_groups() })
            }
            // -- W15 load balancers ---------------------------------------
            VmOp::CreateLoadBalancer { name, network_id, vip, frontend_port, algo, backends } => {
                let owner = self.current_owner()
                    .ok_or_else(|| "lb: owner not yet known; snapshot first".to_string())?;
                let parsed_vip = vip.parse().map_err(|_| "lb create: bad vip")?;
                let lb = self.nets
                    .create_load_balancer(
                        &owner, &name, &network_id,
                        parsed_vip, frontend_port, algo, backends,
                    )
                    .map_err(|e| format!("lb create: {e:?}"))?;
                Ok(VmOpReply::LoadBalancerCreated { lb })
            }
            VmOp::DeleteLoadBalancer { lb_id } => {
                self.nets
                    .delete_load_balancer(&lb_id)
                    .map_err(|e| format!("lb delete: {e:?}"))?;
                Ok(VmOpReply::LoadBalancerDeleted { lb_id })
            }
            VmOp::ListLoadBalancers => {
                Ok(VmOpReply::LoadBalancersListed { lbs: self.nets.list_load_balancers() })
            }
        }
    }

    fn slot_mut(
        slots: &mut [Option<Slot>; MAX_SLOTS],
        vm_id: u32,
    ) -> Result<&mut Slot, String> {
        let i = vm_id as usize;
        if i >= MAX_SLOTS {
            return Err("vm id out of range".into());
        }
        slots[i].as_mut().ok_or_else(|| "vm not allocated".to_string())
    }

    fn current_rows(&self, owner: &NodeId) -> Vec<RemoteVm> {
        self.remember_owner(owner);
        let slots = self.lock_slots();
        slots.iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| {
            RemoteVm {
                owner: owner.clone(),
                vm_id: i as u32,
                label: s.label.clone(),
                state: s.state.into(),
                last_exit: s.last_exit,
                epoch: 0,
                hlc:   0,
                owner_alive: true,
                restart_policy: s.restart_policy,
                volumes: s.volumes.clone(),
                image_path: s.image_path.clone(),
                cpu_count: s.cpu_count,
                memory_mib: s.memory_mib,
                boot_blob_crc32c: s.boot_blob_crc32c,
            }
        })).collect()
    }
}

impl VmHost for MemVmHost {
    fn handle<'a>(&'a self, op: VmOp) -> HostFut<'a, HostResult> {
        Box::pin(async move { self.apply(op) })
    }

    fn snapshot<'a>(&'a self, owner: &'a NodeId) -> HostFut<'a, Vec<RemoteVm>> {
        Box::pin(async move { self.current_rows(owner) })
    }

    fn attach_preserved<'a>(
        &'a self,
        vm_id: u32,
        attachments: Vec<VolumeAttachment>,
    ) -> HostFut<'a, Result<(), String>> {
        Box::pin(async move {
            let mut slots = self.lock_slots();
            let s = Self::slot_mut(&mut slots, vm_id)?;
            s.volumes = attachments;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block<F: Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(f)
    }

    #[test]
    fn create_then_start_then_stop_cycle() {
        let h = MemVmHost::new();
        let id = match block(h.handle(VmOp::Create {
            label: "alpha".into(),
            restart_policy: RestartPolicy::Always,
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        })).unwrap() {
            VmOpReply::Created { vm_id } => vm_id,
            r => panic!("unexpected: {r:?}"),
        };
        let rep = block(h.handle(VmOp::Start { vm_id: id })).unwrap();
        assert!(matches!(rep, VmOpReply::State { state, .. } if state == "halted"));
        let rows = block(h.snapshot(&NodeId::from("n1")));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "alpha");
        assert_eq!(rows[0].restart_policy, RestartPolicy::Always);
    }

    #[test]
    fn registry_full_is_explicit() {
        let h = MemVmHost::new();
        for _ in 0..MAX_SLOTS {
            block(h.handle(VmOp::Create {
                label: "x".into(),
                restart_policy: RestartPolicy::Never,
                image_path: None,
                cpu_count: None,
                memory_mib: None,
                boot_blob_crc32c: None,
            })).unwrap();
        }
        let r = block(h.handle(VmOp::Create {
            label: "y".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        }));
        assert!(matches!(r, Err(s) if s.contains("full")));
    }

    #[test]
    fn create_attach_detach_volume_round_trip() {
        let h = MemVmHost::new();
        // Snapshot first so the host learns its owner id (required
        // for vault id minting).
        let _ = block(h.snapshot(&NodeId::from("n1")));
        let vm = match block(h.handle(VmOp::Create {
            label: "with-vol".into(),
            restart_policy: RestartPolicy::Never,
            image_path: None,
            cpu_count: None,
            memory_mib: None,
            boot_blob_crc32c: None,
        })).unwrap() {
            VmOpReply::Created { vm_id } => vm_id,
            r => panic!("unexpected: {r:?}"),
        };
        let vol = match block(h.handle(VmOp::CreateVolume {
            name: "data".into(),
            size_bytes: 16,
        })).unwrap() {
            VmOpReply::VolumeCreated { volume } => volume,
            r => panic!("unexpected: {r:?}"),
        };
        assert_eq!(vol.owner, "n1");
        let att = match block(h.handle(VmOp::AttachVolume {
            vm_id: vm,
            volume_id: vol.id.clone(),
            mount_name: "data0".into(),
        })).unwrap() {
            VmOpReply::Attachments { volumes, .. } => volumes,
            r => panic!("unexpected: {r:?}"),
        };
        assert_eq!(att.len(), 1);
        // Snapshot now must include the attachment so federation
        // propagates it to peers.
        let rows = block(h.snapshot(&NodeId::from("n1")));
        assert_eq!(rows[0].volumes.len(), 1);
        assert_eq!(rows[0].volumes[0].mount_name, "data0");

        // Cannot delete an attached volume.
        let r = block(h.handle(VmOp::DeleteVolume { volume_id: vol.id.clone() }));
        assert!(matches!(r, Err(s) if s.contains("attached")));

        // Detach and confirm we can delete.
        let _ = block(h.handle(VmOp::DetachVolume {
            vm_id: vm,
            volume_id: vol.id.clone(),
        })).unwrap();
        let _ = block(h.handle(VmOp::DeleteVolume { volume_id: vol.id })).unwrap();
    }
}
