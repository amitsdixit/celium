//! Bridge between the host-side [`crate::vm::Controller`] and the
//! [`celmesh`] federated namespace. Owning a separate module keeps
//! the kernel-mirroring `vm.rs` free of mesh dependencies.

use celmesh::{NodeId, RemoteVm};

use crate::vm::{Controller, VmRecord};

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
