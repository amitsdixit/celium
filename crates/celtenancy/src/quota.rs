//! Tenant quotas and the charge/release accounting helpers.
//!
//! Quotas are checked **before** a resource is provisioned and
//! released when it is destroyed. The store-level methods
//! ([`crate::TenantStore::charge`] / [`crate::TenantStore::release`])
//! call into these pure helpers so the bookkeeping logic is unit
//! testable in isolation.

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

/// Per-tenant quota ceilings. Zero means "no resources of this kind
/// permitted"; use [`TenantQuotas::unlimited`] when an operator
/// explicitly wants no ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TenantQuotas {
    /// Maximum total vCPUs across all VMs in this tenant.
    pub max_vcpus: u32,
    /// Maximum total RAM across all VMs (MiB).
    pub max_memory_mib: u64,
    /// Maximum total persistent storage (bytes).
    pub max_storage_bytes: u64,
    /// Maximum total network throughput (Mbps).
    pub max_network_mbps: u32,
    /// Maximum total provisioned IOPS.
    pub max_iops: u32,
}

impl TenantQuotas {
    /// All ceilings at `u32::MAX` / `u64::MAX`. Use sparingly \u2014
    /// the whole point of quotas is to NOT be unlimited.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_vcpus: u32::MAX,
            max_memory_mib: u64::MAX,
            max_storage_bytes: u64::MAX,
            max_network_mbps: u32::MAX,
            max_iops: u32::MAX,
        }
    }
}

/// Running per-tenant usage counters. The store maintains one of
/// these per tenant; tenancy-aware higher layers update it via
/// [`crate::TenantStore::charge`] and [`crate::TenantStore::release`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QuotaUsage {
    /// Currently allocated vCPUs.
    pub vcpus: u32,
    /// Currently allocated RAM (MiB).
    pub memory_mib: u64,
    /// Currently allocated storage (bytes).
    pub storage_bytes: u64,
    /// Currently reserved network throughput (Mbps).
    pub network_mbps: u32,
    /// Currently reserved IOPS.
    pub iops: u32,
}

/// A single allocation request to charge against a tenant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaCharge {
    /// vCPUs requested.
    pub vcpus: u32,
    /// RAM requested (MiB).
    pub memory_mib: u64,
    /// Storage requested (bytes).
    pub storage_bytes: u64,
    /// Network throughput requested (Mbps).
    pub network_mbps: u32,
    /// IOPS requested.
    pub iops: u32,
}

/// Apply `charge` to `usage`, returning the new usage if it stays
/// within `quotas`.
///
/// # Errors
///
/// Returns [`CelError::Exhausted`] with one of the stable tags
/// `quota.vcpus`, `quota.memory`, `quota.storage`, `quota.network`,
/// `quota.iops` when the resulting usage would exceed the ceiling.
pub fn charge_quota(
    usage: QuotaUsage,
    quotas: TenantQuotas,
    charge: QuotaCharge,
) -> CelResult<QuotaUsage> {
    let new_vcpus = usage.vcpus.saturating_add(charge.vcpus);
    let new_memory = usage.memory_mib.saturating_add(charge.memory_mib);
    let new_storage = usage.storage_bytes.saturating_add(charge.storage_bytes);
    let new_network = usage.network_mbps.saturating_add(charge.network_mbps);
    let new_iops = usage.iops.saturating_add(charge.iops);

    if new_vcpus > quotas.max_vcpus {
        return Err(CelError::Exhausted("quota.vcpus"));
    }
    if new_memory > quotas.max_memory_mib {
        return Err(CelError::Exhausted("quota.memory"));
    }
    if new_storage > quotas.max_storage_bytes {
        return Err(CelError::Exhausted("quota.storage"));
    }
    if new_network > quotas.max_network_mbps {
        return Err(CelError::Exhausted("quota.network"));
    }
    if new_iops > quotas.max_iops {
        return Err(CelError::Exhausted("quota.iops"));
    }

    Ok(QuotaUsage {
        vcpus: new_vcpus,
        memory_mib: new_memory,
        storage_bytes: new_storage,
        network_mbps: new_network,
        iops: new_iops,
    })
}

/// Release a previously charged allocation. Saturating subtraction
/// is used so a double-release never panics; the caller is
/// responsible for matching charge/release pairs.
#[must_use]
pub fn release_quota(usage: QuotaUsage, charge: QuotaCharge) -> QuotaUsage {
    QuotaUsage {
        vcpus: usage.vcpus.saturating_sub(charge.vcpus),
        memory_mib: usage.memory_mib.saturating_sub(charge.memory_mib),
        storage_bytes: usage.storage_bytes.saturating_sub(charge.storage_bytes),
        network_mbps: usage.network_mbps.saturating_sub(charge.network_mbps),
        iops: usage.iops.saturating_sub(charge.iops),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ten_quotas() -> TenantQuotas {
        TenantQuotas {
            max_vcpus: 10,
            max_memory_mib: 10_000,
            max_storage_bytes: 10 * 1024 * 1024 * 1024,
            max_network_mbps: 1_000,
            max_iops: 10_000,
        }
    }

    #[test]
    fn small_charge_fits() {
        let u = charge_quota(
            QuotaUsage::default(),
            ten_quotas(),
            QuotaCharge {
                vcpus: 2,
                memory_mib: 1024,
                storage_bytes: 1024 * 1024,
                network_mbps: 100,
                iops: 500,
            },
        )
        .unwrap();
        assert_eq!(u.vcpus, 2);
        assert_eq!(u.memory_mib, 1024);
    }

    #[test]
    fn vcpu_exhaustion_surface() {
        let err = charge_quota(
            QuotaUsage {
                vcpus: 9,
                ..Default::default()
            },
            ten_quotas(),
            QuotaCharge {
                vcpus: 2,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, CelError::Exhausted("quota.vcpus")));
    }

    #[test]
    fn release_is_saturating() {
        let u = release_quota(
            QuotaUsage {
                vcpus: 1,
                ..Default::default()
            },
            QuotaCharge {
                vcpus: 5,
                ..Default::default()
            },
        );
        assert_eq!(u.vcpus, 0);
    }

    #[test]
    fn iops_exhaustion_surface() {
        let err = charge_quota(
            QuotaUsage {
                iops: 9_999,
                ..Default::default()
            },
            ten_quotas(),
            QuotaCharge {
                iops: 2,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, CelError::Exhausted("quota.iops")));
    }
}
