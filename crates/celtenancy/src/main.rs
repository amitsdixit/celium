//! `celtenancy` admin binary.
//!
//! Operates against a JSON [`FileTenantStore`] at `--store` (or the
//! in-memory store when `--store` is omitted, in which case mutations
//! are ephemeral and the binary is useful only for `version` / a
//! one-shot demo).
//!
//! See `docs/02_TENANCY_LAYER.md` for the operator playbook.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::PathBuf;

use celcommon::{CelError, CelResult};
use celtenancy::{
    FileTenantStore, MemTenantStore, QuotaCharge, TenantCaps, TenantQuotas, TenantSpec,
    TenantStore,
};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "celtenancy", version, about = "Celium Tenancy Layer admin CLI")]
struct Cli {
    /// Path to the tenant store JSON file. If omitted, an ephemeral
    /// in-memory store is used (only useful for `version`).
    #[arg(long, global = true)]
    store: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Print build info.
    Version,
    /// Tenant lifecycle (create / list / show / delete).
    Tenant {
        #[command(subcommand)]
        op: TenantCmd,
    },
    /// Per-tenant user management.
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

#[derive(Debug, Subcommand)]
enum TenantCmd {
    /// Create a new tenant.
    Create(TenantCreateArgs),
    /// List every tenant.
    List,
    /// Show a single tenant by name.
    Show {
        /// Tenant name.
        #[arg(long)]
        name: String,
    },
    /// Delete a tenant by name. Refuses if usage is non-zero.
    Delete {
        /// Tenant name.
        #[arg(long)]
        name: String,
    },
}

#[derive(Debug, Args)]
struct TenantCreateArgs {
    /// Tenant name (path-safe: `[A-Za-z0-9_-]+`, up to 64 bytes).
    #[arg(long)]
    name: String,
    /// Maximum vCPUs.
    #[arg(long, default_value_t = 16)]
    max_vcpus: u32,
    /// Maximum RAM (MiB).
    #[arg(long, default_value_t = 32 * 1024)]
    max_memory_mib: u64,
    /// Maximum persistent storage (bytes).
    #[arg(long, default_value_t = 100 * 1024 * 1024 * 1024)]
    max_storage_bytes: u64,
    /// Maximum network throughput (Mbps).
    #[arg(long, default_value_t = 10_000)]
    max_network_mbps: u32,
    /// Maximum IOPS.
    #[arg(long, default_value_t = 50_000)]
    max_iops: u32,
    /// Root capability set, comma-separated tags (`all`, `none`, or
    /// e.g. `vm.read,vm.write,vol.read`). Default `all`.
    #[arg(long, default_value = "all")]
    caps: String,
}

#[derive(Debug, Subcommand)]
enum UserCmd {
    /// Add a user to a tenant with an attenuated cap set.
    Add {
        /// Tenant name.
        #[arg(long)]
        tenant: String,
        /// User name.
        #[arg(long)]
        name: String,
        /// Capability tags, comma-separated (must be \u2286 tenant root caps).
        #[arg(long, default_value = "vm.read")]
        caps: String,
    },
    /// List users for a tenant.
    List {
        /// Tenant name.
        #[arg(long)]
        tenant: String,
    },
    /// Remove a user by name.
    Remove {
        /// Tenant name.
        #[arg(long)]
        tenant: String,
        /// User name.
        #[arg(long)]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum QuotaCmd {
    /// Show tenant quotas + current usage.
    Show {
        /// Tenant name.
        #[arg(long)]
        tenant: String,
    },
    /// Charge an allocation. Useful for operator-driven dry runs.
    Charge {
        /// Tenant name.
        #[arg(long)]
        tenant: String,
        /// vCPUs to allocate.
        #[arg(long, default_value_t = 0)]
        vcpus: u32,
        /// Memory MiB to allocate.
        #[arg(long, default_value_t = 0)]
        memory_mib: u64,
        /// Storage bytes to allocate.
        #[arg(long, default_value_t = 0)]
        storage_bytes: u64,
        /// Network Mbps to reserve.
        #[arg(long, default_value_t = 0)]
        network_mbps: u32,
        /// IOPS to reserve.
        #[arg(long, default_value_t = 0)]
        iops: u32,
    },
    /// Release a previously charged allocation.
    Release {
        /// Tenant name.
        #[arg(long)]
        tenant: String,
        /// vCPUs to release.
        #[arg(long, default_value_t = 0)]
        vcpus: u32,
        /// Memory MiB to release.
        #[arg(long, default_value_t = 0)]
        memory_mib: u64,
        /// Storage bytes to release.
        #[arg(long, default_value_t = 0)]
        storage_bytes: u64,
        /// Network Mbps to release.
        #[arg(long, default_value_t = 0)]
        network_mbps: u32,
        /// IOPS to release.
        #[arg(long, default_value_t = 0)]
        iops: u32,
    },
}

fn main() -> CelResult<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let cli = Cli::parse();

    if matches!(cli.cmd, Cmd::Version) {
        println!("celtenancy {} (W27)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let store: Box<dyn TenantStore> = match cli.store.as_deref() {
        Some(p) => Box::new(FileTenantStore::open(p)?),
        None => Box::new(MemTenantStore::new()),
    };

    match cli.cmd {
        Cmd::Version => Ok(()),
        Cmd::Tenant { op } => run_tenant(store.as_ref(), op),
        Cmd::User { op } => run_user(store.as_ref(), op),
        Cmd::Quota { op } => run_quota(store.as_ref(), op),
    }
}

fn run_tenant(store: &dyn TenantStore, op: TenantCmd) -> CelResult<()> {
    match op {
        TenantCmd::Create(a) => {
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
        TenantCmd::List => {
            let rows = store.list()?;
            println!("{:<14}  {:<24}  {}", "ID", "NAME", "NAMESPACE");
            for t in rows {
                println!("{:<14}  {:<24}  {}", t.id.to_string(), t.name, t.namespace);
            }
            Ok(())
        }
        TenantCmd::Show { name } => {
            let t = store.get_by_name(&name)?;
            println!("id        = {}", t.id);
            println!("name      = {}", t.name);
            println!("namespace = {}", t.namespace);
            println!("root_caps = {}", t.root_caps.to_tags());
            println!("quotas    = vcpus<= {}, memory_mib<= {}, storage<= {} B, net<= {} Mbps, iops<= {}",
                t.quotas.max_vcpus, t.quotas.max_memory_mib,
                t.quotas.max_storage_bytes, t.quotas.max_network_mbps, t.quotas.max_iops);
            println!("usage     = vcpus= {}, memory_mib= {}, storage= {} B, net= {} Mbps, iops= {}",
                t.usage.vcpus, t.usage.memory_mib,
                t.usage.storage_bytes, t.usage.network_mbps, t.usage.iops);
            println!("users     = {}", t.users.len());
            for u in &t.users {
                println!("  {}  name={}  caps={}", u.id, u.name, u.caps.to_tags());
            }
            Ok(())
        }
        TenantCmd::Delete { name } => {
            let t = store.get_by_name(&name)?;
            store.delete(t.id)?;
            println!("deleted {} ({})", t.name, t.id);
            Ok(())
        }
    }
}

fn run_user(store: &dyn TenantStore, op: UserCmd) -> CelResult<()> {
    match op {
        UserCmd::Add {
            tenant,
            name,
            caps,
        } => {
            let t = store.get_by_name(&tenant)?;
            let requested = TenantCaps::parse_tags(&caps)?;
            let u = store.add_user(t.id, name, requested)?;
            println!(
                "{}  name={}  caps={}  tenant={}",
                u.id,
                u.name,
                u.caps.to_tags(),
                t.name
            );
            Ok(())
        }
        UserCmd::List { tenant } => {
            let t = store.get_by_name(&tenant)?;
            println!("{:<10}  {:<24}  CAPS", "ID", "NAME");
            for u in t.users {
                println!("{:<10}  {:<24}  {}", u.id.to_string(), u.name, u.caps.to_tags());
            }
            Ok(())
        }
        UserCmd::Remove { tenant, name } => {
            let t = store.get_by_name(&tenant)?;
            let uid = t
                .users
                .iter()
                .find(|u| u.name == name)
                .map(|u| u.id)
                .ok_or(CelError::Invalid("user name unknown"))?;
            store.remove_user(t.id, uid)?;
            println!("removed {} from {}", name, t.name);
            Ok(())
        }
    }
}

fn run_quota(store: &dyn TenantStore, op: QuotaCmd) -> CelResult<()> {
    match op {
        QuotaCmd::Show { tenant } => {
            let t = store.get_by_name(&tenant)?;
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
        QuotaCmd::Charge {
            tenant,
            vcpus,
            memory_mib,
            storage_bytes,
            network_mbps,
            iops,
        } => {
            let t = store.get_by_name(&tenant)?;
            let u = store.charge(
                t.id,
                QuotaCharge {
                    vcpus,
                    memory_mib,
                    storage_bytes,
                    network_mbps,
                    iops,
                },
            )?;
            println!(
                "charged: vcpus= {}, memory_mib= {}, storage= {} B, net= {} Mbps, iops= {}",
                u.vcpus, u.memory_mib, u.storage_bytes, u.network_mbps, u.iops
            );
            Ok(())
        }
        QuotaCmd::Release {
            tenant,
            vcpus,
            memory_mib,
            storage_bytes,
            network_mbps,
            iops,
        } => {
            let t = store.get_by_name(&tenant)?;
            let u = store.release(
                t.id,
                QuotaCharge {
                    vcpus,
                    memory_mib,
                    storage_bytes,
                    network_mbps,
                    iops,
                },
            )?;
            println!(
                "released: vcpus= {}, memory_mib= {}, storage= {} B, net= {} Mbps, iops= {}",
                u.vcpus, u.memory_mib, u.storage_bytes, u.network_mbps, u.iops
            );
            Ok(())
        }
    }
}
