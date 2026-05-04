//! CelMesh — gossip + membership + federated namespace fabric.
//!
//! Week-9 introduces basic clustering: every Celium host runs a
//! `Mesh` instance which gossips a small membership view over UDP,
//! discovers peers from a static seed list, and exposes a federated
//! namespace so that VMs created on any node are visible from every
//! node in the cluster.
//!
//! Design notes
//! ============
//! * **No third-party gossip dependency.** The on-wire format is a
//!   versioned, length-prefixed JSON envelope so it is auditable by a
//!   human with `tcpdump`. Production Celium will swap this for a
//!   binary frame; the public API is shaped to make that mechanical.
//! * **Two transports.** `MemTransport` is in-process (used by the
//!   integration test); `UdpTransport` is the real wire. Both
//!   implement the `Transport` trait, so the rest of the engine
//!   (`Mesh`, `Membership`, `NamespaceFederation`) is transport-free.
//! * **Tokio everywhere.** All async paths use Tokio per the global
//!   conventions. No blocking IO inside async fns.
//! * **Strict rules per `00_GLOBAL_CONVENTIONS.md`.** Every fallible
//!   API returns `CelResult<T>`; no `unwrap` / `panic` on production
//!   paths; this crate has `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]
#![warn(missing_docs, rust_2018_idioms)]

pub mod federation;
pub mod host;
pub mod membership;
pub mod mesh;
pub mod proto;
pub mod transport;

pub use federation::{NamespaceFederation, RemoteVm, RestartPolicy};
pub use host::{MemVmHost, VmHost};
pub use membership::{Membership, NodeId, NodeInfo, NodeStatus};
pub use mesh::{ClusterStatus, Mesh, MeshConfig, RestartedVm};
pub use proto::{VmOp, VmOpReply};
pub use transport::{MemTransport, MemTransportFactory, Transport, UdpTransport};

// Re-export celvault's volume surface so downstream crates only need
// to depend on celmesh.
pub use celvault::{
    FileVolumeStore, MemVolumeStore, SnapshotId, SnapshotMeta,
    VolumeAttachment, VolumeId, VolumeMeta, VolumeStore,
};

use celcommon::CelResult;

/// Initialise the mesh subsystem (process-global tracing hooks, etc.).
///
/// Currently a no-op apart from a debug log; kept so the legacy
/// `celmesh::init()` call site in `celctl` keeps working until the
/// CLI is wired through to a live `Mesh` handle.
///
/// # Errors
/// Currently infallible.
pub fn init() -> CelResult<()> {
    tracing::debug!("celmesh::init");
    Ok(())
}
