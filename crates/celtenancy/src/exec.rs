//! W29 — Single-shot tenant-scoped VmOp execution.
//!
//! [`exec`] builds an ephemeral [`celmesh::MemVmHost`] whose
//! capabilities are projected from the tenant's (optionally
//! user-attenuated) [`TenantCaps`], wraps it in a
//! [`TenantVmHost`], dispatches one [`VmOp`], and returns a
//! [`ExecAudit`] describing the trip end-to-end.
//!
//! The host itself is **ephemeral** — VMs and volumes created by
//! `exec` do not survive the call. The tenant store's quota book,
//! however, is the *real* [`TenantStore`] passed in, so successful
//! `Create` / `CreateVolume` ops leave a persistent quota
//! reservation behind. This makes `exec` useful as both a
//! diagnostic surface ("can this tenant create a 2-vCPU VM?") and
//! as an admin "quota provisioning" tool: if the caller wants the
//! reservation freed at the end of the call they can pass
//! [`ExecOptions::release_after_create`] to flip the trip into an
//! atomic charge-and-refund dry-run.

use std::sync::Arc;

use celcommon::{CelError, CelResult};
use celmesh::{
    Capabilities, MemNetworkStore, MemVmHost, MemVolumeStore, NodeId, VmHost, VmOp, VmOpReply,
};
use serde::{Deserialize, Serialize};

use crate::{
    QuotaCharge, QuotaUsage, Tenant, TenantStore, TenantVmHost,
};

/// Options controlling [`exec`]'s side effects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecOptions {
    /// When `true` and the dispatched op is a successful
    /// `Create` / `CreateVolume`, immediately release the charge
    /// from the tenant store, leaving usage where it started.
    /// Useful for "can this op succeed right now?" dry-runs.
    pub release_after_create: bool,
    /// Node label used to prime the ephemeral host. Defaults to
    /// `"tenant-exec"` when `None`.
    pub node: Option<String>,
}

/// Structured record of a single [`exec`] call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecAudit {
    /// Tenant name as resolved.
    pub tenant: String,
    /// User name, if one was supplied.
    pub user: Option<String>,
    /// One-line summary of the op (variant name).
    pub op: String,
    /// Stable capability tag the Core Layer demands for this op,
    /// from [`celmesh::Capabilities::op_tag`].
    pub op_capability_tag: &'static str,
    /// Tenant cap tags actually projected into the Core Layer host.
    pub effective_caps: String,
    /// Quota the tenant store was charged before dispatch
    /// (`None` for read-only ops).
    pub planned_charge: Option<QuotaCharge>,
    /// `true` if the inner host accepted the op.
    pub dispatch_succeeded: bool,
    /// Error string from the inner host (or charge step) on failure.
    pub error: Option<String>,
    /// Brief reply summary on success — see [`reply_summary`].
    pub reply: Option<String>,
    /// Tenant usage as observed before [`exec`] mutated anything.
    pub usage_before: QuotaUsage,
    /// Tenant usage as observed after [`exec`] returned.
    pub usage_after: QuotaUsage,
}

impl ExecAudit {
    /// `true` if the operation completed without error.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.dispatch_succeeded && self.error.is_none()
    }
}

/// Build a `VmOp::Create` with conservative defaults.
#[must_use]
pub fn vm_create_op(
    label: impl Into<String>,
    cpu_count: u32,
    memory_mib: u64,
) -> VmOp {
    VmOp::Create {
        label: label.into(),
        restart_policy: celmesh::RestartPolicy::Never,
        image_path: None,
        cpu_count: Some(cpu_count),
        memory_mib: Some(memory_mib),
        boot_blob_crc32c: None,
    }
}

/// Build a `VmOp::CreateVolume`.
#[must_use]
pub fn volume_create_op(name: impl Into<String>, size_bytes: u64) -> VmOp {
    VmOp::CreateVolume {
        name: name.into(),
        size_bytes,
    }
}

fn reply_summary(r: &VmOpReply) -> String {
    match r {
        VmOpReply::Created { vm_id } => format!("Created vm_id={vm_id}"),
        VmOpReply::Deleted { vm_id } => format!("Deleted vm_id={vm_id}"),
        VmOpReply::State { vm_id, state } => format!("State vm_id={vm_id} state={state:?}"),
        VmOpReply::Listed { rows } => format!("Listed rows={}", rows.len()),
        VmOpReply::VolumeCreated { volume } => {
            format!("VolumeCreated id={} size={}", volume.id, volume.size_bytes)
        }
        VmOpReply::VolumeDeleted { volume_id } => format!("VolumeDeleted id={volume_id}"),
        VmOpReply::VolumesListed { volumes } => format!("VolumesListed n={}", volumes.len()),
        VmOpReply::NetworkCreated { network } => {
            format!("NetworkCreated id={}", network.id)
        }
        VmOpReply::NetworksListed { networks } => format!("NetworksListed n={}", networks.len()),
        other => format!("{other:?}"),
    }
}

fn op_summary(op: &VmOp) -> &'static str {
    match op {
        VmOp::Create { .. } => "Create",
        VmOp::Start { .. } => "Start",
        VmOp::Stop { .. } => "Stop",
        VmOp::Delete { .. } => "Delete",
        VmOp::List => "List",
        VmOp::CreateVolume { .. } => "CreateVolume",
        VmOp::DeleteVolume { .. } => "DeleteVolume",
        VmOp::ListVolumes => "ListVolumes",
        VmOp::CreateNetwork { .. } => "CreateNetwork",
        VmOp::ListNetworks => "ListNetworks",
        _ => "Other",
    }
}

/// Reverse of the matching `Create*` op's charge — used to undo a
/// `release_after_create` round-trip.
fn refund_for(op: &VmOp) -> Option<QuotaCharge> {
    match op {
        VmOp::Create { cpu_count, memory_mib, .. } => Some(QuotaCharge {
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

/// Resolve the effective [`Capabilities`] for the (tenant, user)
/// pair. With no user, the tenant root caps are used. With a user,
/// the user's already-attenuated caps are used directly (they were
/// validated against the root caps at user-add time).
fn effective_caps_for(tenant: &Tenant, user: Option<&str>) -> CelResult<(Capabilities, String)> {
    let caps = match user {
        None => tenant.root_caps,
        Some(name) => tenant
            .users
            .iter()
            .find(|u| u.name == name)
            .map(|u| u.caps)
            .ok_or(CelError::Invalid("tenancy: user unknown"))?,
    };
    let tags = caps.to_tags();
    Ok((caps.to_mesh_capabilities(), tags))
}

/// Single-shot tenant-scoped VmOp execution. See module docs.
///
/// # Errors
///
/// * [`CelError::Invalid`] when tenant or user lookup fails.
/// * Other [`CelError`] variants surface from the underlying
///   [`TenantStore`].
pub async fn exec(
    store: Arc<dyn TenantStore>,
    tenant_name: &str,
    user_name: Option<&str>,
    op: VmOp,
    opts: ExecOptions,
) -> CelResult<ExecAudit> {
    let tenant = store.get_by_name(tenant_name)?;
    let tenant_id = tenant.id;
    let (caps, cap_tags) = effective_caps_for(&tenant, user_name)?;

    let op_tag = Capabilities::op_tag(&op);
    let op_kind = op_summary(&op);
    let usage_before = store.get(tenant_id)?.usage;

    let inner = Arc::new(
        MemVmHost::with_stores(
            Arc::new(MemVolumeStore::new()),
            Arc::new(MemNetworkStore::new()),
        )
        .with_caps(caps),
    );
    let host = TenantVmHost::new(tenant_id, store.clone(), inner);
    let node_label = opts.node.clone().unwrap_or_else(|| "tenant-exec".to_string());
    let node = NodeId::from(node_label.as_str());
    // MemVmHost mints ids relative to the owning node and refuses
    // CreateNetwork until snapshot() has named the owner.
    let _ = host.snapshot(&node).await;

    // TenantVmHost's `handle` plans the charge internally; we just
    // need to know what it WOULD have been for the audit. This is
    // a pure-function planning step against `op` — it does not
    // touch the store.
    let planned_charge = refund_for(&op);

    let dispatch_result = host.handle(op).await;

    let (dispatch_succeeded, reply, error) = match &dispatch_result {
        Ok(r) => (true, Some(reply_summary(r)), None),
        Err(e) => (false, None, Some(e.clone())),
    };

    // Optional dry-run refund — only relevant on a successful
    // Create*. If the inner host refunded already (failure path),
    // there's nothing to do.
    if opts.release_after_create && dispatch_succeeded {
        if let Some(c) = planned_charge {
            if c != QuotaCharge::default() {
                // Best-effort: surface the error in the audit but
                // don't bubble — the user's primary op already
                // succeeded.
                if let Err(e) = store.release(tenant_id, c) {
                    tracing::warn!("tenancy.exec: release after dry-run failed: {e:?}");
                }
            }
        }
    }

    let usage_after = store.get(tenant_id)?.usage;

    Ok(ExecAudit {
        tenant: tenant.name,
        user: user_name.map(str::to_owned),
        op: op_kind.to_string(),
        op_capability_tag: op_tag,
        effective_caps: cap_tags,
        planned_charge,
        dispatch_succeeded,
        error,
        reply,
        usage_before,
        usage_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemTenantStore, TenantCaps, TenantQuotas, TenantSpec};

    fn quotas() -> TenantQuotas {
        TenantQuotas {
            max_vcpus: 4,
            max_memory_mib: 4096,
            max_storage_bytes: 16 * 1024,
            max_network_mbps: 1_000,
            max_iops: 10_000,
        }
    }

    fn fixture(caps: TenantCaps) -> (Arc<dyn TenantStore>, String) {
        let s = Arc::new(MemTenantStore::new());
        let t = s
            .create(TenantSpec::new("acme", quotas()).unwrap(), caps)
            .unwrap();
        (s as Arc<dyn TenantStore>, t.name)
    }

    #[tokio::test]
    async fn vm_create_charges_and_audit_reflects_success() {
        let (store, name) = fixture(TenantCaps::ALL);
        let audit = exec(
            store.clone(),
            &name,
            None,
            vm_create_op("web", 2, 1024),
            ExecOptions::default(),
        )
        .await
        .unwrap();
        assert!(audit.ok(), "audit not ok: {audit:?}");
        assert_eq!(audit.op, "Create");
        assert_eq!(audit.usage_before.vcpus, 0);
        assert_eq!(audit.usage_after.vcpus, 2);
        assert_eq!(audit.usage_after.memory_mib, 1024);
        assert_eq!(audit.op_capability_tag, "vm.create");
        let pc = audit.planned_charge.unwrap();
        assert_eq!(pc.vcpus, 2);
        assert_eq!(pc.memory_mib, 1024);
    }

    #[tokio::test]
    async fn release_after_create_leaves_usage_unchanged() {
        let (store, name) = fixture(TenantCaps::ALL);
        let opts = ExecOptions {
            release_after_create: true,
            ..ExecOptions::default()
        };
        let audit = exec(
            store.clone(),
            &name,
            None,
            vm_create_op("web", 2, 1024),
            opts,
        )
        .await
        .unwrap();
        assert!(audit.ok());
        assert_eq!(audit.usage_after, audit.usage_before);
    }

    #[tokio::test]
    async fn quota_exhaustion_short_circuits_and_reports_error() {
        let (store, name) = fixture(TenantCaps::ALL);
        // 5 vCPU exceeds 4-vCPU ceiling.
        let audit = exec(
            store.clone(),
            &name,
            None,
            vm_create_op("big", 5, 1024),
            ExecOptions::default(),
        )
        .await
        .unwrap();
        assert!(!audit.ok());
        let err = audit.error.unwrap();
        assert!(err.contains("quota"), "expected quota error, got {err}");
        assert_eq!(audit.usage_after.vcpus, 0);
    }

    #[tokio::test]
    async fn capability_denied_when_user_caps_too_narrow() {
        let s = Arc::new(MemTenantStore::new());
        let t = s
            .create(TenantSpec::new("acme", quotas()).unwrap(), TenantCaps::ALL)
            .unwrap();
        // User only has read; Create needs vm.write.
        let _ = s
            .add_user(t.id, "alice".into(), TenantCaps::VM_LIFECYCLE_READ)
            .unwrap();
        let audit = exec(
            s.clone() as Arc<dyn TenantStore>,
            "acme",
            Some("alice"),
            vm_create_op("web", 1, 256),
            ExecOptions::default(),
        )
        .await
        .unwrap();
        assert!(!audit.ok());
        assert!(audit.error.as_deref().unwrap().contains("capability denied"));
        // Refund must have undone the wrapper's pre-charge.
        assert_eq!(audit.usage_after.vcpus, 0);
    }

    #[tokio::test]
    async fn unknown_user_returns_invalid() {
        let (store, name) = fixture(TenantCaps::ALL);
        let err = exec(
            store,
            &name,
            Some("ghost"),
            vm_create_op("web", 1, 256),
            ExecOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CelError::Invalid(_)));
    }

    #[tokio::test]
    async fn volume_create_charges_storage() {
        let (store, name) = fixture(TenantCaps::ALL);
        let audit = exec(
            store.clone(),
            &name,
            None,
            volume_create_op("data", 4096),
            ExecOptions::default(),
        )
        .await
        .unwrap();
        assert!(audit.ok());
        assert_eq!(audit.usage_after.storage_bytes, 4096);
        assert_eq!(audit.op_capability_tag, "vol.create");
    }
}
