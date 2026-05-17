//! `celctl tenant` subcommand tree (W27).
//!
//! Thin wrapper over [`celtenancy`]: opens the tenant store file
//! (default `./build/celctl-tenants.json`, override with
//! `--store`) and dispatches to the same operations the standalone
//! `celtenancy` binary exposes.
//!
//! The user-facing UX is intentionally identical to `celtenancy` so
//! operators only need to learn one surface; `celctl tenant` exists
//! purely for ergonomics inside a workflow that already has a
//! `celctl` open.

use std::path::PathBuf;

use celcommon::{CelError, CelResult};
use celtenancy::{
    FileTenantStore, QuotaCharge, TenantCaps, TenantQuotas, TenantSpec, TenantStore,
};
use clap::{Args, Subcommand};

/// `celctl tenant <op>` dispatch enum.
#[derive(Debug, Subcommand)]
pub enum TenantCmd {
    /// Create a new tenant.
    Create(TenantCreateArgs),
    /// List every tenant.
    List(StoreArgs),
    /// Show a single tenant by name.
    Show {
        #[command(flatten)]
        store: StoreArgs,
        /// Tenant name.
        #[arg(long)]
        name: String,
    },
    /// Delete a tenant by name.
    Delete {
        #[command(flatten)]
        store: StoreArgs,
        /// Tenant name.
        #[arg(long)]
        name: String,
    },
    /// User management for a tenant.
    User {
        #[command(subcommand)]
        op: UserCmd,
    },
    /// Quota inspection / charge / release.
    Quota {
        #[command(subcommand)]
        op: QuotaCmd,
    },
}

/// Operations on users inside a tenant.
#[derive(Debug, Subcommand)]
pub enum UserCmd {
    /// Add a user with an attenuated cap set.
    Add(UserAddArgs),
    /// List users for a tenant.
    List {
        #[command(flatten)]
        store: StoreArgs,
        /// Tenant name.
        #[arg(long)]
        tenant: String,
    },
    /// Remove a user by name.
    Remove(UserRemoveArgs),
}

/// Operations on quotas.
#[derive(Debug, Subcommand)]
pub enum QuotaCmd {
    /// Show tenant quotas + current usage.
    Show {
        #[command(flatten)]
        store: StoreArgs,
        /// Tenant name.
        #[arg(long)]
        tenant: String,
    },
    /// Charge an allocation against a tenant.
    Charge(QuotaChangeArgs),
    /// Release an allocation.
    Release(QuotaChangeArgs),
}

/// Shared `--store` flag.
#[derive(Debug, Args, Clone)]
pub struct StoreArgs {
    /// Path to the tenant store JSON file. Defaults to
    /// `./build/celctl-tenants.json`.
    #[arg(long, default_value = "./build/celctl-tenants.json")]
    pub store: PathBuf,
}

/// Arguments for `tenant create`.
#[derive(Debug, Args, Clone)]
pub struct TenantCreateArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Tenant name.
    #[arg(long)]
    pub name: String,
    /// Maximum vCPUs.
    #[arg(long, default_value_t = 16)]
    pub max_vcpus: u32,
    /// Maximum RAM (MiB).
    #[arg(long, default_value_t = 32 * 1024)]
    pub max_memory_mib: u64,
    /// Maximum persistent storage (bytes).
    #[arg(long, default_value_t = 100 * 1024 * 1024 * 1024)]
    pub max_storage_bytes: u64,
    /// Maximum network throughput (Mbps).
    #[arg(long, default_value_t = 10_000)]
    pub max_network_mbps: u32,
    /// Maximum IOPS.
    #[arg(long, default_value_t = 50_000)]
    pub max_iops: u32,
    /// Root capability tags (comma-separated). Default `all`.
    #[arg(long, default_value = "all")]
    pub caps: String,
}

/// Arguments for `tenant user add`.
#[derive(Debug, Args, Clone)]
pub struct UserAddArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Tenant name.
    #[arg(long)]
    pub tenant: String,
    /// User name.
    #[arg(long)]
    pub name: String,
    /// Capability tags (must be \u2286 tenant root caps).
    #[arg(long, default_value = "vm.read")]
    pub caps: String,
}

/// Arguments for `tenant user remove`.
#[derive(Debug, Args, Clone)]
pub struct UserRemoveArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Tenant name.
    #[arg(long)]
    pub tenant: String,
    /// User name.
    #[arg(long)]
    pub name: String,
}

/// Arguments for `tenant quota charge` / `release`.
#[derive(Debug, Args, Clone)]
pub struct QuotaChangeArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Tenant name.
    #[arg(long)]
    pub tenant: String,
    /// vCPUs.
    #[arg(long, default_value_t = 0)]
    pub vcpus: u32,
    /// Memory MiB.
    #[arg(long, default_value_t = 0)]
    pub memory_mib: u64,
    /// Storage bytes.
    #[arg(long, default_value_t = 0)]
    pub storage_bytes: u64,
    /// Network Mbps.
    #[arg(long, default_value_t = 0)]
    pub network_mbps: u32,
    /// IOPS.
    #[arg(long, default_value_t = 0)]
    pub iops: u32,
}

fn open(args: &StoreArgs) -> CelResult<FileTenantStore> {
    FileTenantStore::open(&args.store)
}

/// Dispatch a parsed `celctl tenant ...` subcommand.
///
/// # Errors
///
/// Surfaces any [`CelError`] from the underlying [`FileTenantStore`].
pub fn run(cmd: TenantCmd) -> CelResult<()> {
    match cmd {
        TenantCmd::Create(a) => {
            let store = open(&a.store)?;
            let caps = TenantCaps::parse_tags(&a.caps)?;
            let spec = TenantSpec::new(
                a.name,
                TenantQuotas {
                    max_vcpus: a.max_vcpus,
                    max_memory_mib: a.max_memory_mib,
                    max_storage_bytes: a.max_storage_bytes,
                    max_network_mbps: a.max_network_mbps,
                    max_iops: a.max_iops,
                },
            )?;
            let t = store.create(spec, caps)?;
            println!(
                "{}  name={}  ns={}  caps={}",
                t.id,
                t.name,
                t.namespace,
                t.root_caps.to_tags()
            );
            Ok(())
        }
        TenantCmd::List(s) => {
            let store = open(&s)?;
            let rows = store.list()?;
            println!("{:<14}  {:<24}  {}", "ID", "NAME", "NAMESPACE");
            for t in rows {
                println!("{:<14}  {:<24}  {}", t.id.to_string(), t.name, t.namespace);
            }
            Ok(())
        }
        TenantCmd::Show { store, name } => {
            let s = open(&store)?;
            let t = s.get_by_name(&name)?;
            println!("id        = {}", t.id);
            println!("name      = {}", t.name);
            println!("namespace = {}", t.namespace);
            println!("root_caps = {}", t.root_caps.to_tags());
            println!(
                "quotas    = vcpus<= {}, memory_mib<= {}, storage<= {} B, net<= {} Mbps, iops<= {}",
                t.quotas.max_vcpus,
                t.quotas.max_memory_mib,
                t.quotas.max_storage_bytes,
                t.quotas.max_network_mbps,
                t.quotas.max_iops
            );
            println!(
                "usage     = vcpus= {}, memory_mib= {}, storage= {} B, net= {} Mbps, iops= {}",
                t.usage.vcpus,
                t.usage.memory_mib,
                t.usage.storage_bytes,
                t.usage.network_mbps,
                t.usage.iops
            );
            println!("users     = {}", t.users.len());
            for u in &t.users {
                println!("  {}  name={}  caps={}", u.id, u.name, u.caps.to_tags());
            }
            Ok(())
        }
        TenantCmd::Delete { store, name } => {
            let s = open(&store)?;
            let t = s.get_by_name(&name)?;
            s.delete(t.id)?;
            println!("deleted {} ({})", t.name, t.id);
            Ok(())
        }
        TenantCmd::User { op } => run_user(op),
        TenantCmd::Quota { op } => run_quota(op),
    }
}

fn run_user(op: UserCmd) -> CelResult<()> {
    match op {
        UserCmd::Add(a) => {
            let s = open(&a.store)?;
            let t = s.get_by_name(&a.tenant)?;
            let caps = TenantCaps::parse_tags(&a.caps)?;
            let u = s.add_user(t.id, a.name, caps)?;
            println!(
                "{}  name={}  caps={}  tenant={}",
                u.id,
                u.name,
                u.caps.to_tags(),
                t.name
            );
            Ok(())
        }
        UserCmd::List { store, tenant } => {
            let s = open(&store)?;
            let t = s.get_by_name(&tenant)?;
            println!("{:<10}  {:<24}  CAPS", "ID", "NAME");
            for u in t.users {
                println!("{:<10}  {:<24}  {}", u.id.to_string(), u.name, u.caps.to_tags());
            }
            Ok(())
        }
        UserCmd::Remove(a) => {
            let s = open(&a.store)?;
            let t = s.get_by_name(&a.tenant)?;
            let uid = t
                .users
                .iter()
                .find(|u| u.name == a.name)
                .map(|u| u.id)
                .ok_or(CelError::Invalid("user name unknown"))?;
            s.remove_user(t.id, uid)?;
            println!("removed {} from {}", a.name, t.name);
            Ok(())
        }
    }
}

fn run_quota(op: QuotaCmd) -> CelResult<()> {
    match op {
        QuotaCmd::Show { store, tenant } => {
            let s = open(&store)?;
            let t = s.get_by_name(&tenant)?;
            println!("tenant    = {}", t.name);
            println!(
                "quotas    = vcpus<= {}, memory_mib<= {}, storage<= {} B, net<= {} Mbps, iops<= {}",
                t.quotas.max_vcpus,
                t.quotas.max_memory_mib,
                t.quotas.max_storage_bytes,
                t.quotas.max_network_mbps,
                t.quotas.max_iops
            );
            println!(
                "usage     = vcpus= {}, memory_mib= {}, storage= {} B, net= {} Mbps, iops= {}",
                t.usage.vcpus,
                t.usage.memory_mib,
                t.usage.storage_bytes,
                t.usage.network_mbps,
                t.usage.iops
            );
            Ok(())
        }
        QuotaCmd::Charge(a) => apply_charge(a, true),
        QuotaCmd::Release(a) => apply_charge(a, false),
    }
}

fn apply_charge(a: QuotaChangeArgs, charge: bool) -> CelResult<()> {
    let s = open(&a.store)?;
    let t = s.get_by_name(&a.tenant)?;
    let c = QuotaCharge {
        vcpus: a.vcpus,
        memory_mib: a.memory_mib,
        storage_bytes: a.storage_bytes,
        network_mbps: a.network_mbps,
        iops: a.iops,
    };
    let u = if charge {
        s.charge(t.id, c)?
    } else {
        s.release(t.id, c)?
    };
    let label = if charge { "charged" } else { "released" };
    println!(
        "{label}: vcpus= {}, memory_mib= {}, storage= {} B, net= {} Mbps, iops= {}",
        u.vcpus, u.memory_mib, u.storage_bytes, u.network_mbps, u.iops
    );
    Ok(())
}
