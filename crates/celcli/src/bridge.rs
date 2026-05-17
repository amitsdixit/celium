//! Bridge between the host-side [`crate::vm::Controller`] and the
//! [`celmesh`] federated namespace. Owning a separate module keeps
//! the kernel-mirroring `vm.rs` free of mesh dependencies.

use std::sync::Arc;

use celcommon::{CelError, CelResult};
use celmesh::{CelhyperVmHost, NodeId, RemoteVm};

use crate::vm::{BootBlobSink, Controller, VmRecord};

/// Render a controller row as a federation row.
///
/// `epoch` and `hlc` are stamped by [`celmesh::Mesh::publish_local_vms`]
/// when the row is published, so we leave them as zero here.
#[must_use]
pub fn record_to_remote(owner: &NodeId, r: &VmRecord) -> RemoteVm {
    RemoteVm {
        owner: owner.clone(),
        vm_id: r.id.0,
        label: r.label.clone(),
        state: r.state.tag().to_string(),
        last_exit: r.last_exit,
        epoch: 0,
        hlc:   0,
        owner_alive: true,
        restart_policy: celmesh::RestartPolicy::Never,
        volumes: Vec::new(),
        // W18.4: propagate image-aware metadata across the cluster.
        image_path:       r.image_path.clone(),
        cpu_count:        r.cpu_count,
        memory_mib:       r.memory_mib,
        boot_blob_crc32c: r.boot_blob_crc32c,
    }
}

/// Snapshot every controller row as a vector ready to publish.
#[must_use]
pub fn snapshot(c: &Controller, owner: &NodeId) -> Vec<RemoteVm> {
    c.list_vms()
        .iter()
        .map(|r| record_to_remote(owner, r))
        .collect()
}

// ---------------------------------------------------------------------------
// W23-E3: HyperBootBlobSink \u2014 adapt CelhyperVmHost to BootBlobSink.
// ---------------------------------------------------------------------------

/// Synchronous [`BootBlobSink`] that ships boot blobs through a
/// [`CelhyperVmHost`]'s `stage_image` async method.
///
/// Owns a dedicated current-thread tokio runtime so it can be used
/// from `celctl vm start` (and any other sync context). **Do not**
/// construct one from within an existing tokio runtime \u2014
/// `block_on` will panic; in that case call `host.stage_image()`
/// directly.
pub struct HyperBootBlobSink {
    host: Arc<CelhyperVmHost>,
    rt:   tokio::runtime::Runtime,
}

impl HyperBootBlobSink {
    /// Wrap `host` in a sink. Builds a fresh single-thread runtime
    /// dedicated to the sink's blocking calls.
    pub fn new(host: Arc<CelhyperVmHost>) -> CelResult<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CelError::Io(format!("tokio runtime for HyperBootBlobSink: {e}")))?;
        Ok(Self { host, rt })
    }
}

impl BootBlobSink for HyperBootBlobSink {
    fn stage(&self, bytes: &[u8]) -> CelResult<u32> {
        // The host's `stage_image` returns `Result<u32, String>` so
        // we map the message into a `CelError` here. Operators see
        // the kernel's verbatim refusal in the error chain.
        self.rt
            .block_on(self.host.stage_image(bytes))
            .map_err(|e| CelError::Io(format!("hyper boot blob sink: {e}")))
    }
}
