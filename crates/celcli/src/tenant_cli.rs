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
use std::sync::Arc;

use celcommon::{CelError, CelResult};
use celtenancy::{
    audit::{AuditSink, FileAuditSink},
    exec::{self, ExecOptions},
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
    /// Execute a single VmOp through a tenant-scoped
    /// [`celtenancy::TenantVmHost`] (W29). The host is ephemeral —
    /// state does not survive the call — but quota charges land
    /// in the configured tenant store.
    Exec {
        #[command(subcommand)]
        op: ExecCmd,
    },
    /// Persistent audit log inspection (W30).
    Audit {
        #[command(subcommand)]
        op: AuditCmd,
    },
    /// Nested tenant management (W31).
    Subtenant {
        #[command(subcommand)]
        op: SubtenantCmd,
    },
    /// Print the tenant tree rooted at every top-level tenant (W31).
    Tree(StoreArgs),
}

/// `celctl tenant subtenant <op>` dispatch enum.
#[derive(Debug, Subcommand)]
pub enum SubtenantCmd {
    /// Create a subtenant under an existing parent.
    Create(SubtenantCreateArgs),
    /// List direct children of a parent tenant.
    List {
        #[command(flatten)]
        store: StoreArgs,
        /// Parent tenant name.
        #[arg(long)]
        parent: String,
    },
}

/// Arguments for `tenant subtenant create`.
#[derive(Debug, Args, Clone)]
pub struct SubtenantCreateArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Parent tenant name.
    #[arg(long)]
    pub parent: String,
    /// Subtenant name (must be globally unique across the store).
    #[arg(long)]
    pub name: String,
    /// Maximum vCPUs. Must be ≤ parent's `max_vcpus`.
    #[arg(long, default_value_t = 4)]
    pub max_vcpus: u32,
    /// Maximum RAM (MiB). Must be ≤ parent's `max_memory_mib`.
    #[arg(long, default_value_t = 4 * 1024)]
    pub max_memory_mib: u64,
    /// Maximum persistent storage (bytes). Must be ≤ parent's `max_storage_bytes`.
    #[arg(long, default_value_t = 10 * 1024 * 1024 * 1024)]
    pub max_storage_bytes: u64,
    /// Maximum network throughput (Mbps). Must be ≤ parent's `max_network_mbps`.
    #[arg(long, default_value_t = 1_000)]
    pub max_network_mbps: u32,
    /// Maximum IOPS. Must be ≤ parent's `max_iops`.
    #[arg(long, default_value_t = 5_000)]
    pub max_iops: u32,
    /// Capability tags (must be ⊆ parent root caps).
    /// Default `inherit` copies parent's root caps verbatim.
    #[arg(long, default_value = "inherit")]
    pub caps: String,
}

/// `celctl tenant exec <op>` dispatch enum.
#[derive(Debug, Subcommand)]
pub enum ExecCmd {
    /// Provision a VM slot through the tenant runtime.
    VmCreate(ExecVmCreateArgs),
    /// Provision a volume slot through the tenant runtime.
    VolumeCreate(ExecVolumeCreateArgs),
}

/// Shared `--tenant` / `--user` / `--release-after` arguments for
/// every `tenant exec` subcommand.
#[derive(Debug, Args, Clone)]
pub struct ExecCommonArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Tenant name.
    #[arg(long)]
    pub tenant: String,
    /// Optional user name; applies user-attenuated caps when set.
    #[arg(long)]
    pub user: Option<String>,
    /// If true, refund the charge after a successful Create.
    /// Turns the call into a charge-and-refund dry-run useful for
    /// "would this op succeed right now?" checks.
    #[arg(long, default_value_t = false)]
    pub release_after: bool,
    /// Emit machine-readable JSON instead of a human summary.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Optional path to a JSON-lines audit log. When set, every
    /// charge / release / denial / dispatch outcome is appended
    /// as a single line.
    #[arg(long)]
    pub audit_log: Option<PathBuf>,
}

/// Arguments for `tenant exec vm-create`.
#[derive(Debug, Args, Clone)]
pub struct ExecVmCreateArgs {
    #[command(flatten)]
    pub common: ExecCommonArgs,
    /// VM label.
    #[arg(long)]
    pub label: String,
    /// vCPUs requested.
    #[arg(long, default_value_t = 1)]
    pub cpus: u32,
    /// Memory (MiB) requested.
    #[arg(long, default_value_t = 512)]
    pub memory_mib: u64,
}

/// Arguments for `tenant exec volume-create`.
#[derive(Debug, Args, Clone)]
pub struct ExecVolumeCreateArgs {
    #[command(flatten)]
    pub common: ExecCommonArgs,
    /// Volume name.
    #[arg(long)]
    pub name: String,
    /// Size in bytes.
    #[arg(long)]
    pub size_bytes: u64,
}

/// `celctl tenant audit <op>` dispatch enum.
#[derive(Debug, Subcommand)]
pub enum AuditCmd {
    /// Show the last N events in the audit log (default 10).
    Tail(AuditTailArgs),
    /// Print a one-line summary of the audit log.
    Stats(AuditStatsArgs),
}

/// Arguments for `tenant audit tail`.
#[derive(Debug, Args, Clone)]
pub struct AuditTailArgs {
    /// Path to the audit log.
    #[arg(long)]
    pub audit_log: PathBuf,
    /// Number of trailing events to show.
    #[arg(long, short = 'n', default_value_t = 10)]
    pub lines: usize,
    /// Emit JSON instead of one human line per event.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Arguments for `tenant audit stats`.
#[derive(Debug, Args, Clone)]
pub struct AuditStatsArgs {
    /// Path to the audit log.
    #[arg(long)]
    pub audit_log: PathBuf,
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
        TenantCmd::Exec { op } => run_exec(op),
        TenantCmd::Audit { op } => run_audit(op),
        TenantCmd::Subtenant { op } => run_subtenant(op),
        TenantCmd::Tree(s) => run_tree(&s),
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

fn run_exec(op: ExecCmd) -> CelResult<()> {
    match op {
        ExecCmd::VmCreate(a) => dispatch_exec(
            &a.common,
            exec::vm_create_op(a.label, a.cpus, a.memory_mib),
        ),
        ExecCmd::VolumeCreate(a) => dispatch_exec(
            &a.common,
            exec::volume_create_op(a.name, a.size_bytes),
        ),
    }
}

fn dispatch_exec(common: &ExecCommonArgs, op: celmesh::VmOp) -> CelResult<()> {
    let store: Arc<dyn TenantStore> = Arc::new(open(&common.store)?);
    let sink: Option<Arc<dyn AuditSink>> = match &common.audit_log {
        Some(p) => Some(Arc::new(FileAuditSink::open(p.clone())?)),
        None => None,
    };
    let opts = ExecOptions {
        release_after_create: common.release_after,
        node: None,
        audit: sink,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CelError::Io(format!("tokio runtime: {e}")))?;
    let audit = rt.block_on(exec::exec(
        store,
        &common.tenant,
        common.user.as_deref(),
        op,
        opts,
    ))?;

    if common.json {
        let json = serde_json::to_string_pretty(&audit)
            .map_err(|e| CelError::Storage(format!("audit json: {e}")))?;
        println!("{json}");
    } else {
        let status = if audit.ok() { "ok" } else { "FAILED" };
        println!("exec {status}: tenant={} user={:?} op={}", audit.tenant, audit.user, audit.op);
        println!("  cap_tag       = {}", audit.op_capability_tag);
        println!("  caps_applied  = {}", audit.effective_caps);
        if let Some(c) = audit.planned_charge {
            println!(
                "  planned_charge= vcpus={} memory_mib={} storage_bytes={}",
                c.vcpus, c.memory_mib, c.storage_bytes
            );
        }
        if let Some(r) = audit.reply {
            println!("  reply         = {r}");
        }
        if let Some(e) = audit.error {
            println!("  error         = {e}");
        }
        println!(
            "  usage_before  = vcpus={} memory_mib={} storage_bytes={}",
            audit.usage_before.vcpus,
            audit.usage_before.memory_mib,
            audit.usage_before.storage_bytes
        );
        println!(
            "  usage_after   = vcpus={} memory_mib={} storage_bytes={}",
            audit.usage_after.vcpus,
            audit.usage_after.memory_mib,
            audit.usage_after.storage_bytes
        );
    }
    Ok(())
}

fn run_audit(op: AuditCmd) -> CelResult<()> {
    match op {
        AuditCmd::Tail(a) => {
            let sink = FileAuditSink::open(a.audit_log)?;
            let events = sink.tail(a.lines)?;
            if a.json {
                let json = serde_json::to_string_pretty(&events)
                    .map_err(|e| CelError::Storage(format!("audit json: {e}")))?;
                println!("{json}");
            } else {
                for ev in events {
                    println!(
                        "{} tenant={} user={:?} action={:?} cap={:?} success={} note={:?}",
                        ev.timestamp_millis,
                        ev.tenant,
                        ev.user,
                        ev.action,
                        ev.op_capability_tag,
                        ev.success,
                        ev.note,
                    );
                }
            }
            Ok(())
        }
        AuditCmd::Stats(a) => {
            let sink = FileAuditSink::open(a.audit_log)?;
            let events = sink.read_all()?;
            let total = events.len();
            let denied = events.iter().filter(|e| !e.success).count();
            let charges = events
                .iter()
                .filter(|e| matches!(e.action, celtenancy::AuditAction::Charge))
                .count();
            let releases = events
                .iter()
                .filter(|e| matches!(e.action, celtenancy::AuditAction::Release))
                .count();
            let execs = events
                .iter()
                .filter(|e| matches!(e.action, celtenancy::AuditAction::Exec))
                .count();
            println!(
                "audit {}: total={} charges={} releases={} execs={} denied={}",
                sink.path().display(),
                total,
                charges,
                releases,
                execs,
                denied,
            );
            Ok(())
        }
    }
}

fn run_subtenant(op: SubtenantCmd) -> CelResult<()> {
    match op {
        SubtenantCmd::Create(a) => {
            let store = open(&a.store)?;
            let parent = store.get_by_name(&a.parent)?;
            // `inherit` is a CLI sugar that copies the parent's
            // root caps verbatim. Any other value goes through the
            // standard tag parser, and the store enforces ⊆ parent.
            let caps = if a.caps.eq_ignore_ascii_case("inherit") {
                parent.root_caps
            } else {
                TenantCaps::parse_tags(&a.caps)?
            };
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
            let t = store.create_subtenant(parent.id, spec, caps)?;
            println!(
                "{}  name={}  parent={}  ns={}  caps={}",
                t.id,
                t.name,
                parent.name,
                t.namespace,
                t.root_caps.to_tags()
            );
            Ok(())
        }
        SubtenantCmd::List { store, parent } => {
            let s = open(&store)?;
            let p = s.get_by_name(&parent)?;
            let kids = s.children(p.id)?;
            println!("{:<14}  {:<24}  {}", "ID", "NAME", "NAMESPACE");
            for c in kids {
                println!("{:<14}  {:<24}  {}", c.id.to_string(), c.name, c.namespace);
            }
            Ok(())
        }
    }
}

fn run_tree(args: &StoreArgs) -> CelResult<()> {
    let s = open(args)?;
    let all = s.list()?;
    // Index children by parent id for O(N) tree print.
    let mut by_parent: std::collections::BTreeMap<Option<u64>, Vec<&celtenancy::Tenant>> =
        std::collections::BTreeMap::new();
    for t in &all {
        by_parent
            .entry(t.parent.map(celtenancy::TenantId::raw))
            .or_default()
            .push(t);
    }
    fn print_subtree(
        node: &celtenancy::Tenant,
        depth: usize,
        by_parent: &std::collections::BTreeMap<Option<u64>, Vec<&celtenancy::Tenant>>,
    ) {
        let pad = "  ".repeat(depth);
        println!(
            "{pad}{}  vcpus={}/{}  mem={}/{} MiB",
            node.name,
            node.usage.vcpus,
            node.quotas.max_vcpus,
            node.usage.memory_mib,
            node.quotas.max_memory_mib,
        );
        if let Some(kids) = by_parent.get(&Some(node.id.raw())) {
            for k in kids {
                print_subtree(k, depth + 1, by_parent);
            }
        }
    }
    if let Some(roots) = by_parent.get(&None) {
        for r in roots {
            print_subtree(r, 0, &by_parent);
        }
    }
    Ok(())
}
