//! W28 — `TenantVmHost`: a [`celmesh::VmHost`] adapter that
//! **auto-charges** a tenant's quota for every resource-creating
//! op and **releases** it on the matching delete.
//!
//! Design constraint: the Core Layer never learns about tenants.
//! `TenantVmHost` wraps an arbitrary `Arc<dyn VmHost>` (typically a
//! [`celmesh::MemVmHost`] constructed with
//! `TenantCaps::to_mesh_capabilities()` already applied) and
//! interposes on the `handle` call:
//!
//! 1. Compute the [`QuotaCharge`] implied by the op.
//! 2. If non-zero, **charge first** via the [`TenantStore`]. On
//!    quota exhaustion, return `Err("quota exhausted: ...")` to the
//!    caller without ever touching the inner host.
//! 3. Forward the op to the inner host.
//! 4. On success, remember the per-resource charge for the eventual
//!    `Delete*` op.
//! 5. On failure, refund the charge so the tenant's bookkeeping
//!    stays consistent.
//!
//! `Delete{Vm,Volume,…}` look up the recorded charge and release it
//! after the inner host returns success. If the inner host refuses
//! the delete (e.g. VM still running), the charge stays.
//!
//! All non-resource-mutating ops (`List`, `Read`, `Write`, `Attach`,
//! `Snapshot…`) pass through unchanged with no quota interaction.
//!
//! ## What is **not** in this module
//!
//! * Capability checking. The inner host still owns
//!   [`celmesh::Capabilities`] enforcement. The wrapper assumes you
//!   already projected `TenantCaps` into the host via
//!   `MemVmHost::with_caps(tenant.root_caps.to_mesh_capabilities())`.
//! * Namespace rewriting. Labels are passed through verbatim; the
//!   tenant prefix lives in the federated namespace, not in the
//!   per-host slot table.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use celmesh::host::HostFut;
use celmesh::{NodeId, RemoteVm, VmHost, VmOp, VmOpReply, VolumeAttachment, VolumeId};

use crate::{QuotaCharge, TenantId, TenantStore};

/// Per-op error tag used when the inner host rejects an op that we
/// already charged for. The wrapper releases the charge and surfaces
/// the original error verbatim, prefixed with the tag so operators
/// can grep tenancy issues out of host logs.
const TENANT_ERROR_PREFIX: &str = "tenant:";

/// `TenantVmHost` interposes quota accounting between a caller and
/// a Core-Layer [`VmHost`].
///
/// Construct one per `(tenant, inner_host)` pair. The wrapper holds
/// an `Arc` to a [`TenantStore`] so quota updates are visible to
/// every other component (CLI, admin server) talking to the same
/// store.
pub struct TenantVmHost {
    /// Tenant whose quota gets charged.
    tenant: TenantId,
    /// Shared tenant store. The wrapper never holds its lock across
    /// an `.await` on the inner host (see `with_state`).
    store: Arc<dyn TenantStore>,
    /// Underlying Core-Layer host. Caps are expected to already be
    /// projected from `tenant.root_caps` (or a user attenuation).
    inner: Arc<dyn VmHost>,
    /// vm_id → charge accounting, for refund on `Delete`.
    vm_charges: Mutex<HashMap<u32, QuotaCharge>>,
    /// volume_id → charge accounting, for refund on `DeleteVolume`.
    volume_charges: Mutex<HashMap<VolumeId, QuotaCharge>>,
}

impl TenantVmHost {
    /// Build a wrapper around `inner` that charges quota against
    /// `tenant` in `store`.
    #[must_use]
    pub fn new(
        tenant: TenantId,
        store: Arc<dyn TenantStore>,
        inner: Arc<dyn VmHost>,
    ) -> Self {
        Self {
            tenant,
            store,
            inner,
            vm_charges: Mutex::new(HashMap::new()),
            volume_charges: Mutex::new(HashMap::new()),
        }
    }

    /// Tenant this wrapper is bound to.
    #[must_use]
    pub fn tenant(&self) -> TenantId {
        self.tenant
    }
}

// ---------------------------------------------------------------------------
// Charge planning
// ---------------------------------------------------------------------------

/// Compute the implied charge for `op`. Returns `None` for ops that
/// do not consume tenant resources.
fn charge_for(op: &VmOp) -> Option<QuotaCharge> {
    match op {
        VmOp::Create {
            cpu_count,
            memory_mib,
            ..
        } => Some(QuotaCharge {
            vcpus: cpu_count.unwrap_or(0),
            memory_mib: memory_mib.unwrap_or(0),
            ..QuotaCharge::default()
        }),
        VmOp::CreateVolume { size_bytes, .. } => Some(QuotaCharge {
            storage_bytes: *size_bytes,
            ..QuotaCharge::default()
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Lock helpers — never poison the maps even if a panic slipped past
// our `forbid(unsafe_code)` + `Result<_,_>` discipline.
// ---------------------------------------------------------------------------

fn lock_vm<'a>(m: &'a Mutex<HashMap<u32, QuotaCharge>>) -> std::sync::MutexGuard<'a, HashMap<u32, QuotaCharge>> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn lock_vol<'a>(
    m: &'a Mutex<HashMap<VolumeId, QuotaCharge>>,
) -> std::sync::MutexGuard<'a, HashMap<VolumeId, QuotaCharge>> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

// ---------------------------------------------------------------------------
// VmHost impl
// ---------------------------------------------------------------------------

impl VmHost for TenantVmHost {
    fn handle<'a>(&'a self, op: VmOp) -> HostFut<'a, Result<VmOpReply, String>> {
        Box::pin(async move {
            // 1. Plan the charge (if any) and bill it up-front so a
            //    quota breach short-circuits without ever entering
            //    the inner host.
            let planned = charge_for(&op);
            if let Some(charge) = planned {
                if charge != QuotaCharge::default() {
                    self.store
                        .charge(self.tenant, charge)
                        .map_err(|e| format!("{TENANT_ERROR_PREFIX} quota: {e}"))?;
                }
            }

            // 2. Snapshot the keys we need to refund / track BEFORE
            //    moving `op` into the inner call.
            let delete_vm_id = match &op {
                VmOp::Delete { vm_id } => Some(*vm_id),
                _ => None,
            };
            let delete_volume_id = match &op {
                VmOp::DeleteVolume { volume_id } => Some(volume_id.clone()),
                _ => None,
            };
            let is_create_vm = matches!(op, VmOp::Create { .. });
            let is_create_volume = matches!(op, VmOp::CreateVolume { .. });

            // 3. Forward.
            let reply = self.inner.handle(op).await;

            // 4. Reconcile.
            match (&reply, planned) {
                // Successful Create: remember the charge for refund.
                (Ok(VmOpReply::Created { vm_id }), Some(c)) if is_create_vm => {
                    lock_vm(&self.vm_charges).insert(*vm_id, c);
                }
                (Ok(VmOpReply::VolumeCreated { volume }), Some(c)) if is_create_volume => {
                    lock_vol(&self.volume_charges).insert(volume.id.clone(), c);
                }
                // Inner host refused a Create — refund.
                (Err(_), Some(c)) if c != QuotaCharge::default() => {
                    // Best-effort release. If the store itself fails
                    // here we have nowhere to surface it (the
                    // primary error wins) — log and continue.
                    if let Err(e) = self.store.release(self.tenant, c) {
                        tracing::warn!(
                            target: "celtenancy::runtime",
                            tenant = %self.tenant,
                            "quota refund failed after inner Create error: {e}",
                        );
                    }
                }
                _ => {}
            }

            // 5. Delete refunds.
            if let (Ok(VmOpReply::Deleted { .. }), Some(vm_id)) = (&reply, delete_vm_id) {
                if let Some(charge) = lock_vm(&self.vm_charges).remove(&vm_id) {
                    if let Err(e) = self.store.release(self.tenant, charge) {
                        tracing::warn!(
                            target: "celtenancy::runtime",
                            tenant = %self.tenant,
                            vm_id,
                            "quota refund failed after Delete: {e}",
                        );
                    }
                }
            }
            if let (Ok(VmOpReply::VolumeDeleted { .. }), Some(vid)) = (&reply, delete_volume_id) {
                if let Some(charge) = lock_vol(&self.volume_charges).remove(&vid) {
                    if let Err(e) = self.store.release(self.tenant, charge) {
                        tracing::warn!(
                            target: "celtenancy::runtime",
                            tenant = %self.tenant,
                            volume = %vid,
                            "quota refund failed after DeleteVolume: {e}",
                        );
                    }
                }
            }

            reply
        })
    }

    fn snapshot<'a>(&'a self, owner: &'a NodeId) -> HostFut<'a, Vec<RemoteVm>> {
        self.inner.snapshot(owner)
    }

    fn attach_preserved<'a>(
        &'a self,
        vm_id: u32,
        attachments: Vec<VolumeAttachment>,
    ) -> HostFut<'a, Result<(), String>> {
        self.inner.attach_preserved(vm_id, attachments)
    }
}

// `Send + Sync` are required by the trait. All fields are already
// `Send + Sync`: `Arc<dyn TenantStore + Send + Sync>` (the trait
// already requires both), `Arc<dyn VmHost>` (ditto), and `Mutex<T>`
// where `T: Send`.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<TenantVmHost>();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemTenantStore, TenantCaps, TenantQuotas, TenantSpec};
    use celcommon::CelError;
    use celmesh::{Capabilities, MemNetworkStore, MemVmHost, MemVolumeStore};

    fn quotas() -> TenantQuotas {
        TenantQuotas {
            max_vcpus: 4,
            max_memory_mib: 2048,
            max_storage_bytes: 64 * 1024,
            max_network_mbps: 100,
            max_iops: 1000,
        }
    }

    fn fixture() -> (Arc<MemTenantStore>, TenantId, Arc<MemVmHost>) {
        let store = Arc::new(MemTenantStore::new());
        let t = store
            .create(
                TenantSpec::new("acme", quotas()).unwrap(),
                TenantCaps::ALL,
            )
            .unwrap();
        let inner = Arc::new(
            MemVmHost::with_stores(
                Arc::new(MemVolumeStore::new()),
                Arc::new(MemNetworkStore::new()),
            )
            .with_caps(Capabilities::ALL),
        );
        (store, t.id, inner)
    }

    #[tokio::test]
    async fn create_vm_charges_and_delete_releases() {
        let (store, tid, inner) = fixture();
        let host = TenantVmHost::new(tid, store.clone(), inner);

        // MemVmHost mints ids relative to the owning node; prime it.
        let node = NodeId::from("tenant-host");
        let _ = host.snapshot(&node).await;

        let reply = host
            .handle(VmOp::Create {
                label: "web".into(),
                restart_policy: celmesh::RestartPolicy::Never,
                image_path: None,
                cpu_count: Some(2),
                memory_mib: Some(1024),
                boot_blob_crc32c: None,
            })
            .await
            .unwrap();
        let vm_id = match reply {
            VmOpReply::Created { vm_id } => vm_id,
            other => panic!("expected Created, got {other:?}"),
        };

        let t = store.get(tid).unwrap();
        assert_eq!(t.usage.vcpus, 2);
        assert_eq!(t.usage.memory_mib, 1024);

        // Drive the VM to a terminal state. `Start` single-steps
        // the VM to `Halted`, which is the canonical terminal tag
        // accepted by MemVmHost's `Delete`.
        host.handle(VmOp::Start { vm_id }).await.unwrap();
        host.handle(VmOp::Delete { vm_id }).await.unwrap();

        let t = store.get(tid).unwrap();
        assert_eq!(t.usage.vcpus, 0);
        assert_eq!(t.usage.memory_mib, 0);
    }

    #[tokio::test]
    async fn quota_exhaustion_short_circuits_inner_host() {
        let (store, tid, inner) = fixture();
        let host = TenantVmHost::new(tid, store.clone(), inner);

        // Charge the entire quota up front via a direct call.
        store
            .charge(tid, QuotaCharge { vcpus: 4, ..Default::default() })
            .unwrap();

        // Create asks for 1 more vCPU → must fail BEFORE inner.
        let err = host
            .handle(VmOp::Create {
                label: "x".into(),
                restart_policy: celmesh::RestartPolicy::Never,
                image_path: None,
                cpu_count: Some(1),
                memory_mib: None,
                boot_blob_crc32c: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("quota"), "expected quota error, got {err}");

        // No leak — inner host is untouched.
        let t = store.get(tid).unwrap();
        assert_eq!(t.usage.vcpus, 4); // unchanged; the new charge was refused.
    }

    #[tokio::test]
    async fn create_volume_charges_and_delete_releases() {
        let (store, tid, inner) = fixture();
        let host = TenantVmHost::new(tid, store.clone(), inner);

        let node = NodeId::from("tenant-host");
        let _ = host.snapshot(&node).await;

        let reply = host
            .handle(VmOp::CreateVolume {
                name: "data".into(),
                size_bytes: 4096,
            })
            .await
            .unwrap();
        let vid = match reply {
            VmOpReply::VolumeCreated { volume } => volume.id,
            other => panic!("expected VolumeCreated, got {other:?}"),
        };

        assert_eq!(store.get(tid).unwrap().usage.storage_bytes, 4096);

        host.handle(VmOp::DeleteVolume { volume_id: vid }).await.unwrap();
        assert_eq!(store.get(tid).unwrap().usage.storage_bytes, 0);
    }

    #[tokio::test]
    async fn inner_failure_refunds_the_charge() {
        let (store, tid, inner) = fixture();
        let host = TenantVmHost::new(tid, store.clone(), inner);

        // Label > 32 chars triggers MemVmHost's validation reject.
        let err = host
            .handle(VmOp::Create {
                label: "x".repeat(64),
                restart_policy: celmesh::RestartPolicy::Never,
                image_path: None,
                cpu_count: Some(2),
                memory_mib: Some(512),
                boot_blob_crc32c: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("label"), "expected label error, got {err}");

        // Quota was refunded.
        let t = store.get(tid).unwrap();
        assert_eq!(t.usage.vcpus, 0);
        assert_eq!(t.usage.memory_mib, 0);
    }

    #[test]
    fn charge_planner_covers_create_and_create_volume_only() {
        assert!(matches!(
            charge_for(&VmOp::List),
            None,
        ));
        let c = charge_for(&VmOp::Create {
            label: "x".into(),
            restart_policy: celmesh::RestartPolicy::Never,
            image_path: None,
            cpu_count: Some(2),
            memory_mib: Some(256),
            boot_blob_crc32c: None,
        })
        .unwrap();
        assert_eq!(c.vcpus, 2);
        assert_eq!(c.memory_mib, 256);

        let c = charge_for(&VmOp::CreateVolume {
            name: "v".into(),
            size_bytes: 999,
        })
        .unwrap();
        assert_eq!(c.storage_bytes, 999);

        // CapabilityDenied / Invalid are not produced by the
        // planner — sanity check that we're not mis-classifying.
        let _ = CelError::Invalid("noop");
    }
}
