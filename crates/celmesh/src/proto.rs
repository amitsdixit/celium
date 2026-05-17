//! CelMesh wire protocol.
//!
//! Every gossip frame is a JSON `Envelope`. A frame may carry one of
//! three payloads:
//!
//! * `Hello`   — sent on first contact with a peer.
//! * `Sync`    — a full delta of this node's view: membership rows
//!   plus the VMs it owns. Receivers merge by
//!   last-writer-wins on `(epoch, hlc)`.
//! * `Goodbye` — voluntary departure.
//!
//! The format is intentionally human-readable for the v0.1 sprint;
//! the only requirement on the surrounding transport is that frames
//! arrive intact and atomically. UDP datagrams already give us that.

use serde::{Deserialize, Serialize};

use crate::federation::RemoteVm;
use crate::membership::NodeInfo;
pub use celvault::{
    Cidr4, Direction, L4Proto, LbAlgo, LbBackend, LoadBalancer, LoadBalancerId,
    NetworkId, Nic, NicId, SecurityGroup, SecurityGroupId, SecurityRule, SnapshotId,
    SnapshotMeta, VirtualNetwork, VolumeAttachment, VolumeId, VolumeMeta,
};
/// Protocol version. Bump on incompatible wire changes — receivers
/// drop frames whose `version` they do not recognise.
pub const PROTO_VERSION: u32 = 1;

/// Magic prefix written on every frame so junk on the wire is obvious.
pub const MAGIC: &str = "celmesh/1";

/// Maximum on-wire frame size, in bytes. Anything larger is dropped
/// without parsing. Keeps a hostile peer from forcing the receiver to
/// allocate unbounded memory.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Top-level wire envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Frame magic. See [`MAGIC`].
    pub magic: String,
    /// Protocol version. See [`PROTO_VERSION`].
    pub version: u32,
    /// Identifier of the node that emitted this frame.
    pub from: String,
    /// Cluster identifier — frames from a different cluster are
    /// dropped before they reach `Membership`.
    pub cluster: String,
    /// Hybrid logical clock value, monotonic per source node.
    pub hlc: u64,
    /// Concrete payload.
    pub payload: Payload,
}

/// Three-way message taxonomy. Anything else is rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    /// First-contact greeting.
    Hello {
        /// Sender's self-described row.
        node: NodeInfo,
    },
    /// Full delta sync.
    Sync {
        /// Every membership row the sender currently knows.
        nodes: Vec<NodeInfo>,
        /// Every VM the sender currently owns.
        vms: Vec<RemoteVm>,
    },
    /// Voluntary departure. Receivers mark `from` as `Left`.
    Goodbye,
    /// Week-10 cross-node VM operation request. The receiver checks
    /// whether `target` matches its own id and, if so, dispatches to
    /// its registered [`crate::host::VmHost`].
    Request {
        /// Caller-side correlation id. Echoed in the response so the
        /// caller can match replies to outstanding waiters.
        req_id: u64,
        /// Intended target node. Receivers ignore requests whose
        /// target id does not match their own.
        target: String,
        /// Operation to perform.
        op: VmOp,
    },
    /// Week-10 reply to a `Request`.
    Response {
        /// Mirrors the originating `req_id`.
        req_id: u64,
        /// Either an `Ok` reply or an error string.
        result: VmOpResult,
    },
}

/// VM operations that can travel between nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum VmOp {
    /// Allocate a VM with `label`. Optional `restart_policy`
    /// controls whether the supervisor will attempt to recreate the
    /// VM elsewhere on owner failure.
    Create {
        /// Free-form label, ≤ 32 chars.
        label: String,
        /// Default `Never`.
        #[serde(default)]
        restart_policy: crate::federation::RestartPolicy,
    },
    /// Move VM `vm_id` to `Halted` (single-step run).
    Start {
        /// Target VM slot id on the receiving node.
        vm_id: u32,
    },
    /// Move VM `vm_id` to `Stopped`.
    Stop  {
        /// Target VM slot id on the receiving node.
        vm_id: u32,
    },
    /// Free the slot. Only valid on terminal VMs.
    Delete{
        /// Target VM slot id on the receiving node.
        vm_id: u32,
    },
    /// Return the host's current local VM list.
    List,
    /// Week-12: Allocate a new persistent volume on the receiving
    /// node's [`celvault::VolumeStore`].
    CreateVolume {
        /// Free-form label, ≤ `celvault::MAX_NAME` chars.
        name: String,
        /// Logical size in bytes.
        size_bytes: u64,
    },
    /// Week-12: Delete a volume. The volume must not be attached.
    DeleteVolume {
        /// Volume to delete.
        volume_id: VolumeId,
    },
    /// Week-12: List volumes on the receiving node.
    ListVolumes,
    /// Week-12: Attach a volume to a VM. The volume must already
    /// exist on the receiving node's vault.
    AttachVolume {
        /// VM slot to attach the volume to.
        vm_id: u32,
        /// Volume to attach.
        volume_id: VolumeId,
        /// Mount-point name within the guest.
        mount_name: String,
    },
    /// Week-12: Detach a volume from a VM. Idempotent.
    DetachVolume {
        /// VM slot id.
        vm_id: u32,
        /// Volume to detach.
        volume_id: VolumeId,
    },
    /// Week-13: random-access read against a volume.
    ReadVolume {
        /// Volume to read from.
        volume_id: VolumeId,
        /// Byte offset.
        offset: u64,
        /// Number of bytes to read. Capped server-side.
        len: u64,
    },
    /// Week-13: random-access write to a volume.
    WriteVolume {
        /// Volume to write to.
        volume_id: VolumeId,
        /// Byte offset.
        offset: u64,
        /// Bytes to write. Capped server-side.
        bytes: Vec<u8>,
    },
    /// Week-13: take a snapshot of a volume.
    CreateSnapshot {
        /// Volume to snapshot.
        volume_id: VolumeId,
        /// Free-form snapshot label.
        name: String,
    },
    /// Week-13: list snapshots, optionally filtered to one volume.
    ListSnapshots {
        /// Volume filter; `None` lists every snapshot.
        #[serde(default)]
        volume_id: Option<VolumeId>,
    },
    /// Week-13: delete a snapshot.
    DeleteSnapshot {
        /// Snapshot to delete.
        snapshot_id: SnapshotId,
    },
    /// Week-13: restore a snapshot back onto its parent volume.
    RestoreSnapshot {
        /// Snapshot to restore.
        snapshot_id: SnapshotId,
    },

    // -- W15: networking --------------------------------------------------

    /// W15: create a virtual network.
    CreateNetwork {
        /// Free-form network name (≤ `celvault::network::MAX_NAME`).
        name: String,
        /// CIDR block in `"10.0.0.0/24"` form.
        cidr: String,
    },
    /// W15: delete a virtual network. Errors if NICs are attached.
    DeleteNetwork {
        /// Target network.
        network_id: NetworkId,
    },
    /// W15: list known networks on the receiving node.
    ListNetworks,
    /// W15: allocate a NIC for `vm_id` on `network_id`.
    AttachNic {
        /// Target network.
        network_id: NetworkId,
        /// VM slot id (must already exist on the receiving node).
        vm_id: u32,
        /// Optional explicit IP request; `None` for auto-allocate.
        #[serde(default)]
        ip: Option<String>,
    },
    /// W15: detach (and free) a NIC. Idempotent.
    DetachNic {
        /// NIC to free.
        nic_id: NicId,
    },
    /// W15: list every NIC the receiving node knows about.
    ListNics,

    // -- W15: security groups ---------------------------------------------

    /// W15: create a security group with `rules`.
    CreateSecurityGroup {
        /// Group name.
        name: String,
        /// Ordered rule list.
        rules: Vec<SecurityRule>,
    },
    /// W15: delete a security group. Idempotent.
    DeleteSecurityGroup {
        /// Group to delete.
        sg_id: SecurityGroupId,
    },
    /// W15: list every security group the receiving node knows about.
    ListSecurityGroups,

    // -- W15: load balancers ----------------------------------------------

    /// W15: create a load balancer.
    CreateLoadBalancer {
        /// Free-form name.
        name: String,
        /// Network the LB sits on.
        network_id: NetworkId,
        /// Front-end VIP (must be inside the network's CIDR).
        vip: String,
        /// Front-end port.
        frontend_port: u16,
        /// Distribution policy.
        #[serde(default)]
        algo: LbAlgo,
        /// Backend list.
        backends: Vec<LbBackend>,
    },
    /// W15: delete a load balancer. Idempotent.
    DeleteLoadBalancer {
        /// LB to delete.
        lb_id: LoadBalancerId,
    },
    /// W15: list every load balancer the receiving node knows about.
    ListLoadBalancers,
}

/// Reply to a [`VmOp`]. Tagged so the wire form is human-readable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum VmOpReply {
    /// Returned for `Create`. `vm_id` is the slot index.
    Created {
        /// Newly-allocated slot id.
        vm_id: u32,
    },
    /// Returned for `Start`/`Stop`. `state` matches `VmState::tag`.
    State   {
        /// Slot id whose state changed.
        vm_id: u32,
        /// New state tag (e.g. `"halted"`).
        state: String,
    },
    /// Returned for `Delete`.
    Deleted {
        /// Slot id that was freed.
        vm_id: u32,
    },
    /// Returned for `List`.
    Listed  {
        /// Every VM the host currently owns.
        rows: Vec<RemoteVm>,
    },
    /// Week-12: Returned for `CreateVolume`.
    VolumeCreated {
        /// Newly-created volume metadata.
        volume: VolumeMeta,
    },
    /// Week-12: Returned for `DeleteVolume`.
    VolumeDeleted {
        /// Volume that was removed.
        volume_id: VolumeId,
    },
    /// Week-12: Returned for `ListVolumes`.
    VolumesListed {
        /// Every volume on the receiving node.
        volumes: Vec<VolumeMeta>,
    },
    /// Week-12: Returned for `AttachVolume`/`DetachVolume`. The full
    /// post-op attachment list lets clients diff without an extra RPC.
    Attachments {
        /// VM slot id.
        vm_id: u32,
        /// Current attachments.
        volumes: Vec<VolumeAttachment>,
    },
    /// Week-13: returned for `ReadVolume`.
    VolumeData {
        /// Volume the bytes were read from.
        volume_id: VolumeId,
        /// Bytes read.
        bytes: Vec<u8>,
    },
    /// Week-13: returned for `WriteVolume`.
    VolumeWritten {
        /// Volume that was written to.
        volume_id: VolumeId,
        /// Number of bytes written.
        bytes_written: u64,
    },
    /// Week-13: returned for `CreateSnapshot`.
    SnapshotCreated {
        /// Newly-created snapshot.
        snapshot: SnapshotMeta,
    },
    /// Week-13: returned for `ListSnapshots`.
    SnapshotsListed {
        /// Snapshots matching the requested filter.
        snapshots: Vec<SnapshotMeta>,
    },
    /// Week-13: returned for `DeleteSnapshot`.
    SnapshotDeleted {
        /// Snapshot id that was removed.
        snapshot_id: SnapshotId,
    },
    /// Week-13: returned for `RestoreSnapshot`.
    SnapshotRestored {
        /// Snapshot id that was restored.
        snapshot_id: SnapshotId,
    },

    // -- W15 networking replies -------------------------------------------

    /// W15: returned for `CreateNetwork`.
    NetworkCreated {
        /// Newly-created network metadata.
        network: VirtualNetwork,
    },
    /// W15: returned for `DeleteNetwork`.
    NetworkDeleted {
        /// Network id that was removed.
        network_id: NetworkId,
    },
    /// W15: returned for `ListNetworks`.
    NetworksListed {
        /// Every network the receiving node knows.
        networks: Vec<VirtualNetwork>,
    },
    /// W15: returned for `AttachNic`.
    NicAttached {
        /// Newly-allocated NIC.
        nic: Nic,
    },
    /// W15: returned for `DetachNic`.
    NicDetached {
        /// NIC that was freed.
        nic_id: NicId,
    },
    /// W15: returned for `ListNics`.
    NicsListed {
        /// Every NIC the receiving node knows.
        nics: Vec<Nic>,
    },

    /// W15: returned for `CreateSecurityGroup`.
    SecurityGroupCreated {
        /// Newly-created group.
        sg: SecurityGroup,
    },
    /// W15: returned for `DeleteSecurityGroup`.
    SecurityGroupDeleted {
        /// Group id that was removed.
        sg_id: SecurityGroupId,
    },
    /// W15: returned for `ListSecurityGroups`.
    SecurityGroupsListed {
        /// Every group the receiving node knows.
        sgs: Vec<SecurityGroup>,
    },

    /// W15: returned for `CreateLoadBalancer`.
    LoadBalancerCreated {
        /// Newly-created LB.
        lb: LoadBalancer,
    },
    /// W15: returned for `DeleteLoadBalancer`.
    LoadBalancerDeleted {
        /// LB id that was removed.
        lb_id: LoadBalancerId,
    },
    /// W15: returned for `ListLoadBalancers`.
    LoadBalancersListed {
        /// Every LB the receiving node knows.
        lbs: Vec<LoadBalancer>,
    },
}

/// Wire-friendly `Result<VmOpReply, String>`. We avoid serialising a
/// raw `Result` so the JSON form is stable (`{ "result": { "ok":
/// {...} } }` or `{ "result": { "err": "msg" } }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmOpResult {
    /// Operation succeeded.
    Ok(VmOpReply),
    /// Operation failed; carries the host-side error string.
    Err(String),
}

impl Envelope {
    /// Encode `self` to a UTF-8 JSON byte vector. Errors only if a
    /// payload contains non-encodable data, which today is unreachable
    /// — every field in [`Payload`] is plain serde-derive — but the
    /// API surface is fallible to keep the door open.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decode `bytes` into an envelope. Rejects frames that are too
    /// large, do not match `MAGIC`, or carry a future
    /// `PROTO_VERSION`.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(DecodeError::TooLarge);
        }
        let env: Envelope = serde_json::from_slice(bytes)
            .map_err(|_| DecodeError::Malformed)?;
        if env.magic != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        if env.version != PROTO_VERSION {
            return Err(DecodeError::VersionMismatch);
        }
        Ok(env)
    }
}

/// Reason a frame was rejected. Stays in this module so the rest of
/// the crate can pattern-match it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Frame exceeded [`MAX_FRAME_BYTES`].
    TooLarge,
    /// JSON did not parse.
    Malformed,
    /// Magic prefix did not match.
    BadMagic,
    /// `version` did not match [`PROTO_VERSION`].
    VersionMismatch,
}
