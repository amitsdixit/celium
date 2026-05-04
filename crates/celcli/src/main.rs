//! `celctl` — Celium operator CLI.
//!
//! Week-9 surface: `version`, `probe`, the `vm` subcommand tree
//! (mirrors `celhyper::manager` against a host-side JSON state file),
//! and a new `cluster` subcommand tree backed by `celmesh`. The
//! kernel itself runs only on bare metal, so the host-side
//! `Controller` is a parallel data model; `cluster start` is the
//! entrypoint that brings up a real `Mesh` over UDP and federates
//! the controller's view across the cluster.
#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod bridge;
pub mod vm;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use celcommon::{CelError, CelResult};
use celmesh::{
    Mesh, MeshConfig, MemVmHost, NodeId, RestartPolicy, Transport, UdpTransport, VmHost, VmOp,
    VmOpReply,
};
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::vm::{Controller, VmId};

#[derive(Debug, Parser)]
#[command(name = "celctl", version, about = "Celium operator CLI")]
struct Cli {
    /// Path to the controller's state file. Defaults to
    /// `./build/celctl-state.json`.
    #[arg(long, global = true)]
    state_file: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Print build info and exit.
    Version,
    /// Probe local hypervisor presence (stub).
    Probe,
    /// Manage VMs in the local controller.
    Vm {
        #[command(subcommand)]
        op: VmCmd,
    },
    /// Manage cluster membership and federation.
    Cluster {
        #[command(subcommand)]
        op: ClusterCmd,
    },
}

#[derive(Debug, Subcommand)]
enum ClusterCmd {
    /// Start a CelMesh node, federate this node's VMs, and run for
    /// `--duration` seconds (or until Ctrl-C). Used by demos and the
    /// integration test.
    Start(StartArgs),
    /// Print members of an ad-hoc, single-shot cluster snapshot.
    Members(StartArgs),
    /// Print the federated VM list.
    Vms(StartArgs),
    /// Send a one-shot VM op to a remote node and print the reply.
    Invoke(InvokeArgs),
    /// Send a one-shot VM op addressed by federated path
    /// (`/cluster/<node>/vms/<n>`).
    InvokePath(InvokePathArgs),
    /// Run one supervisor pass: if this node is the elected
    /// supervisor, recreate every orphaned VM whose policy is
    /// `always`. Prints the list of recreations.
    Recover(StartArgs),
    /// Print a single cluster status snapshot — members, VMs,
    /// alive/suspect/dead counters, supervisor flag.
    Status(StartArgs),
}

#[derive(Debug, Args, Clone)]
struct StartArgs {
    /// Stable id of this node.
    #[arg(long)]
    node_id: String,
    /// UDP socket to bind, e.g. `127.0.0.1:7100`.
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,
    /// Address peers should reach us at — defaults to `--bind`.
    #[arg(long)]
    advertise: Option<String>,
    /// Cluster name. Frames from a different cluster are dropped.
    #[arg(long, default_value = "celium")]
    cluster: String,
    /// Comma-separated list of seed `host:port` addresses.
    #[arg(long, default_value = "")]
    seeds: String,
    /// Restart counter — bump on every node start.
    #[arg(long, default_value_t = 1)]
    epoch: u64,
    /// Seconds to keep the mesh up before exiting. `0` means run
    /// until Ctrl-C.
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// For `members` / `vms`: how many seconds to give the cluster
    /// to converge before printing.
    #[arg(long, default_value_t = 1)]
    settle: u64,
}

/// Operands for `cluster invoke`. Wraps `StartArgs` plus the target
/// node and the chosen op.
#[derive(Debug, Args, Clone)]
struct InvokeArgs {
    #[command(flatten)]
    common: StartArgs,
    /// Target node id whose host should run the op. May be the same
    /// as `--node-id`, in which case the local fast-path is taken.
    #[arg(long = "target")]
    target: String,
    /// Op to perform: `create | start | stop | delete | list |
    /// create-volume | delete-volume | list-volumes | attach-volume |
    /// detach-volume`.
    #[arg(long)]
    op: String,
    /// `--label` is required for `create` (and is the volume name
    /// for `create-volume`); ignored otherwise.
    #[arg(long, default_value = "")]
    label: String,
    /// Slot id on the target. Required for start/stop/delete and
    /// for `attach-volume`/`detach-volume`.
    #[arg(long, default_value_t = 0)]
    vm_id: u32,
    /// `--restart never|always`. Only meaningful with `create`.
    #[arg(long, default_value = "never")]
    restart: String,
    /// Volume id (e.g. `n1/v1`). Required for delete-volume,
    /// attach-volume, detach-volume.
    #[arg(long, default_value = "")]
    volume_id: String,
    /// Volume size in bytes. Required for `create-volume`.
    #[arg(long, default_value_t = 0)]
    volume_size: u64,
    /// Mount name for `attach-volume`. Free-form; ≤ 32 chars.
    #[arg(long, default_value = "")]
    mount_name: String,
    /// RPC timeout in milliseconds.
    #[arg(long, default_value_t = 5_000)]
    timeout_ms: u64,
}

/// Operands for `cluster invoke-path`.
#[derive(Debug, Args, Clone)]
struct InvokePathArgs {
    #[command(flatten)]
    common: StartArgs,
    /// Federated path: `/cluster/<node>/vms/<n>`.
    #[arg(long)]
    path: String,
    /// Op to perform: `start | stop | delete`.
    #[arg(long)]
    op: String,
    /// RPC timeout in milliseconds.
    #[arg(long, default_value_t = 5_000)]
    timeout_ms: u64,
}

#[derive(Debug, Subcommand)]
enum VmCmd {
    /// Allocate a new VM slot.
    Create(CreateArgs),
    /// List allocated VMs.
    List,
    /// Start a VM (model-only on the host).
    Start(IdArg),
    /// Stop a VM (idempotent on terminal states).
    Stop(IdArg),
    /// Print the state of a single VM.
    State(IdArg),
}

#[derive(Debug, Args)]
struct CreateArgs {
    /// Free-form label, ≤ 32 chars.
    #[arg(long, default_value = "")]
    label: String,
}

#[derive(Debug, Args)]
struct IdArg {
    /// Either a numeric id (`0`) or a path (`/vms/0`).
    target: String,
}

fn main() -> CelResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    let state_path = cli.state_file.unwrap_or_else(vm::default_state_path);

    match cli.cmd {
        Cmd::Version => {
            println!("celctl {}", env!("CARGO_PKG_VERSION"));
        }
        Cmd::Probe => {
            tracing::info!("probe: no hypervisor RPC implemented yet (week 1 stub)");
        }
        Cmd::Vm { op } => {
            let mut c = Controller::load(&state_path)?;
            run_vm_cmd(&mut c, op)?;
            c.save()?;
        }
        Cmd::Cluster { op } => {
            // Tokio runtime is constructed lazily so `version` /
            // `vm` paths stay synchronous and fast.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| CelError::Io(format!("tokio runtime: {e}")))?;
            rt.block_on(run_cluster_cmd(&state_path, op))?;
        }
    }
    Ok(())
}

fn run_vm_cmd(c: &mut Controller, op: VmCmd) -> CelResult<()> {
    match op {
        VmCmd::Create(a) => {
            let id = c.create_vm(a.label)?;
            println!("created {} ({})", id.0, Controller::path_for(id));
        }
        VmCmd::List => {
            let rows = c.list_vms();
            println!("{:>4}  {:<8}  {:<10}  {}", "id", "state", "last_exit", "label");
            for r in rows {
                let exit = r.last_exit.map_or_else(|| "-".to_string(), |x| x.to_string());
                println!("{:>4}  {:<8}  {:<10}  {}", r.id.0, r.state.tag(), exit, r.label);
            }
        }
        VmCmd::Start(a) => {
            let id = resolve(c, &a.target)?;
            let s = c.start_vm(id)?;
            println!("started {} -> {}", id.0, s.tag());
        }
        VmCmd::Stop(a) => {
            let id = resolve(c, &a.target)?;
            let s = c.stop_vm(id)?;
            println!("stopped {} -> {}", id.0, s.tag());
        }
        VmCmd::State(a) => {
            let id = resolve(c, &a.target)?;
            let s = c.vm_state(id)?;
            println!("{} {}", Controller::path_for(id), s.tag());
        }
    }
    Ok(())
}

/// Accept either `"7"` or `"/vms/7"`. Numeric form is for terse shell
/// use; the path form mirrors the kernel namespace.
fn resolve(c: &Controller, target: &str) -> CelResult<VmId> {
    if let Ok(n) = target.parse::<u32>() {
        c.resolve_path(&format!("/vms/{n}"))
    } else {
        c.resolve_path(target)
    }
}

// ---------------------------------------------------------------------------
// Cluster subcommand
// ---------------------------------------------------------------------------

async fn run_cluster_cmd(state_path: &std::path::Path, op: ClusterCmd) -> CelResult<()> {
    match op {
        ClusterCmd::Start(args)   => cluster_start(state_path, args).await,
        ClusterCmd::Members(args) => cluster_snapshot_members(state_path, args).await,
        ClusterCmd::Vms(args)     => cluster_snapshot_vms(state_path, args).await,
        ClusterCmd::Invoke(args)  => cluster_invoke(args).await,
        ClusterCmd::InvokePath(args) => cluster_invoke_path(args).await,
        ClusterCmd::Recover(args) => cluster_recover(args).await,
        ClusterCmd::Status(args)  => cluster_status(args).await,
    }
}

fn parse_seeds(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

async fn build_mesh(args: &StartArgs, owner: &mut String) -> CelResult<Mesh> {
    let transport = Arc::new(UdpTransport::bind(&args.bind).await?);
    let advertise = args.advertise.clone().unwrap_or_else(|| transport.local_addr());
    *owner = args.node_id.clone();
    let cfg = MeshConfig {
        cluster:         args.cluster.clone(),
        node_id:         NodeId(args.node_id.clone()),
        advertise_addr:  advertise,
        epoch:           args.epoch,
        seeds:           parse_seeds(&args.seeds),
        gossip_interval: Duration::from_millis(100),
        timeout_suspect: Duration::from_millis(750),
        timeout_dead:    Duration::from_millis(2_500),
        // Run a supervisor pass once a second on every node — only
        // the elected lowest-id Alive node will actually do work.
        supervisor_interval: Duration::from_secs(1),
    };
    Mesh::start(cfg, transport).await
}

async fn cluster_start(state_path: &std::path::Path, args: StartArgs) -> CelResult<()> {
    let controller = Controller::load(state_path)?;
    let mut owner_id = String::new();
    let mesh = build_mesh(&args, &mut owner_id).await?;
    let owner = NodeId(owner_id.clone());

    // Register an in-memory VmHost so peers' `cluster invoke` calls
    // land somewhere. The kernel-side IPC bridge will replace this in
    // a future week; for now the same in-process model the local
    // `vm` subcommand uses keeps semantics consistent.
    let host: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    mesh.set_host(host.clone()).await;

    // Seed the host from any pre-existing local Controller rows so
    // `cluster vms` immediately reflects what `vm list` sees.
    for r in controller.list_vms() {
        let _ = host.handle(VmOp::Create {
            label: r.label.clone(),
            restart_policy: RestartPolicy::Never,
        }).await;
    }
    let snapshot = host.snapshot(&owner).await;
    mesh.publish_local_vms(snapshot).await?;
    println!("celctl: node {} listening on {} (seeds={:?})",
             args.node_id, args.bind, parse_seeds(&args.seeds));

    let stop_after = if args.duration == 0 { None }
                     else                   { Some(Duration::from_secs(args.duration)) };
    if let Some(d) = stop_after {
        tokio::time::sleep(d).await;
    } else {
        // Run until Ctrl-C. `signal::ctrl_c()` returns once the
        // signal is delivered; ignoring its error is safe — there's
        // no recovery path other than exit.
        let _ = tokio::signal::ctrl_c().await;
    }
    let _ = mesh.shutdown().await;
    Ok(())
}

async fn cluster_snapshot_members(state_path: &std::path::Path, args: StartArgs) -> CelResult<()> {
    let _ = state_path; // unused: pure observation
    let mut owner_id = String::new();
    let mesh = build_mesh(&args, &mut owner_id).await?;
    tokio::time::sleep(Duration::from_secs(args.settle.max(1))).await;
    let members = mesh.members().await;
    println!("{:<20}  {:<24}  {:<8}  {:>6}  {:>6}",
             "node", "addr", "status", "epoch", "hlc");
    for r in members {
        println!("{:<20}  {:<24}  {:<8?}  {:>6}  {:>6}",
                 r.id.as_str(), r.addr, r.status, r.epoch, r.hlc);
    }
    let _ = mesh.shutdown().await;
    Ok(())
}

async fn cluster_snapshot_vms(state_path: &std::path::Path, args: StartArgs) -> CelResult<()> {
    let controller = Controller::load(state_path)?;
    let mut owner_id = String::new();
    let mesh = build_mesh(&args, &mut owner_id).await?;
    let owner = NodeId(owner_id);
    let snapshot = bridge::snapshot(&controller, &owner);
    mesh.publish_local_vms(snapshot).await?;
    tokio::time::sleep(Duration::from_secs(args.settle.max(1))).await;

    let vms = mesh.list_vms().await;
    println!("{:<28}  {:<8}  {:<10}  {:<6}  {}",
             "path", "state", "last_exit", "alive", "label");
    for v in vms {
        let exit = v.last_exit.map_or_else(|| "-".to_string(), |x| x.to_string());
        println!("{:<28}  {:<8}  {:<10}  {:<6}  {}",
                 v.path(), v.state, exit, v.owner_alive, v.label);
    }
    let _ = mesh.shutdown().await;
    Ok(())
}

// -- Week-10 --------------------------------------------------------------

async fn cluster_invoke(args: InvokeArgs) -> CelResult<()> {
    let mut owner_id = String::new();
    let mesh = build_mesh(&args.common, &mut owner_id).await?;

    // Local host so a Request that comes back at us has somewhere to
    // land. Harmless when --target is remote.
    let host: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    mesh.set_host(host).await;

    // Allow gossip to discover the target node.
    tokio::time::sleep(Duration::from_secs(args.common.settle.max(1))).await;

    let target = NodeId(args.target.clone());
    let restart = match args.restart.as_str() {
        "always" => RestartPolicy::Always,
        "never"  => RestartPolicy::Never,
        other    => return Err(CelError::Invalid(match other {
            _ => "restart: expected 'never' or 'always'",
        })),
    };
    let op = match args.op.as_str() {
        "create" => VmOp::Create { label: args.label.clone(), restart_policy: restart },
        "start"  => VmOp::Start  { vm_id: args.vm_id },
        "stop"   => VmOp::Stop   { vm_id: args.vm_id },
        "delete" => VmOp::Delete { vm_id: args.vm_id },
        "list"   => VmOp::List,
        "create-volume" => VmOp::CreateVolume {
            name: args.label.clone(),
            size_bytes: args.volume_size,
        },
        "delete-volume" => VmOp::DeleteVolume {
            volume_id: celmesh::VolumeId(args.volume_id.clone()),
        },
        "list-volumes" => VmOp::ListVolumes,
        "attach-volume" => VmOp::AttachVolume {
            vm_id: args.vm_id,
            volume_id: celmesh::VolumeId(args.volume_id.clone()),
            mount_name: args.mount_name.clone(),
        },
        "detach-volume" => VmOp::DetachVolume {
            vm_id: args.vm_id,
            volume_id: celmesh::VolumeId(args.volume_id.clone()),
        },
        "create-snapshot" => VmOp::CreateSnapshot {
            volume_id: celmesh::VolumeId(args.volume_id.clone()),
            name: args.label.clone(),
        },
        "list-snapshots" => VmOp::ListSnapshots {
            volume_id: if args.volume_id.is_empty() {
                None
            } else {
                Some(celmesh::VolumeId(args.volume_id.clone()))
            },
        },
        "delete-snapshot" => VmOp::DeleteSnapshot {
            snapshot_id: celmesh::SnapshotId(args.volume_id.clone()),
        },
        "restore-snapshot" => VmOp::RestoreSnapshot {
            snapshot_id: celmesh::SnapshotId(args.volume_id.clone()),
        },
        _ => return Err(CelError::Invalid(
            "op: create|start|stop|delete|list|create-volume|delete-volume|list-volumes|attach-volume|detach-volume|create-snapshot|list-snapshots|delete-snapshot|restore-snapshot",
        )),
    };

    let reply = mesh.invoke(&target, op, Duration::from_millis(args.timeout_ms)).await?;
    match reply {
        VmOpReply::Created { vm_id } =>
            println!("created vm {} on {} (path /cluster/{}/vms/{})",
                     vm_id, target, target, vm_id),
        VmOpReply::State   { vm_id, state } =>
            println!("vm {}/{} state={}", target, vm_id, state),
        VmOpReply::Deleted { vm_id } =>
            println!("deleted vm {}/{}", target, vm_id),
        VmOpReply::Listed  { rows } => {
            println!("{:<28}  {:<8}  {}", "path", "state", "label");
            for r in rows {
                println!("{:<28}  {:<8}  {}", r.path(), r.state, r.label);
            }
        }
        VmOpReply::VolumeCreated { volume } =>
            println!("created volume {} on {} (size={} name={})",
                     volume.id, volume.owner, volume.size_bytes, volume.name),
        VmOpReply::VolumeDeleted { volume_id } =>
            println!("deleted volume {volume_id}"),
        VmOpReply::VolumesListed { volumes } => {
            println!("{:<24}  {:<10}  {:<8}  {}", "id", "size", "owner", "name");
            for v in volumes {
                println!("{:<24}  {:<10}  {:<8}  {}", v.id, v.size_bytes, v.owner, v.name);
            }
        }
        VmOpReply::Attachments { vm_id, volumes } => {
            println!("vm {target}/{vm_id} attachments={}", volumes.len());
            for a in volumes {
                println!("  {} -> {}", a.mount_name, a.volume_id);
            }
        }
        VmOpReply::VolumeData { volume_id, bytes } =>
            println!("read {} bytes from {volume_id}", bytes.len()),
        VmOpReply::VolumeWritten { volume_id, bytes_written } =>
            println!("wrote {bytes_written} bytes to {volume_id}"),
        VmOpReply::SnapshotCreated { snapshot } =>
            println!("created snapshot {} ({} bytes) of {}",
                     snapshot.id, snapshot.size_bytes, snapshot.volume),
        VmOpReply::SnapshotsListed { snapshots } => {
            println!("{:<28}  {:<18}  {:<10}  {}", "id", "volume", "size", "name");
            for s in snapshots {
                println!("{:<28}  {:<18}  {:<10}  {}", s.id, s.volume, s.size_bytes, s.name);
            }
        }
        VmOpReply::SnapshotDeleted { snapshot_id } =>
            println!("deleted snapshot {snapshot_id}"),
        VmOpReply::SnapshotRestored { snapshot_id } =>
            println!("restored snapshot {snapshot_id}"),
    }
    let _ = mesh.shutdown().await;
    Ok(())
}

async fn cluster_recover(args: StartArgs) -> CelResult<()> {
    let mut owner_id = String::new();
    let mesh = build_mesh(&args, &mut owner_id).await?;
    let host: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    mesh.set_host(host).await;

    tokio::time::sleep(Duration::from_secs(args.settle.max(1))).await;

    let restarted = mesh.run_supervisor_step().await?;
    if restarted.is_empty() {
        println!("celctl: nothing to recover (supervisor={})", mesh.is_supervisor().await);
    } else {
        println!("celctl: recovered {} vm(s):", restarted.len());
        for r in restarted {
            println!("  {}/{} -> {}/{}  label={}",
                     r.original_owner, r.original_vm_id,
                     args.node_id, r.new_vm_id, r.label);
        }
    }
    let _ = mesh.shutdown().await;
    Ok(())
}

// -- Week-11 --------------------------------------------------------------

async fn cluster_invoke_path(args: InvokePathArgs) -> CelResult<()> {
    let mut owner_id = String::new();
    let mesh = build_mesh(&args.common, &mut owner_id).await?;
    let host: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    mesh.set_host(host).await;
    tokio::time::sleep(Duration::from_secs(args.common.settle.max(1))).await;

    // `invoke_path` rewrites the op's vm_id from the path, so any
    // value here is replaced — start with 0.
    let op = match args.op.as_str() {
        "start"  => VmOp::Start  { vm_id: 0 },
        "stop"   => VmOp::Stop   { vm_id: 0 },
        "delete" => VmOp::Delete { vm_id: 0 },
        _        => return Err(CelError::Invalid("op: start|stop|delete")),
    };
    let reply = mesh
        .invoke_path(&args.path, op, Duration::from_millis(args.timeout_ms))
        .await?;
    match reply {
        VmOpReply::State   { vm_id, state } =>
            println!("{} state={} (vm_id={})", args.path, state, vm_id),
        VmOpReply::Deleted { vm_id } =>
            println!("{} deleted (vm_id={})", args.path, vm_id),
        other => println!("{:?}", other),
    }
    let _ = mesh.shutdown().await;
    Ok(())
}

async fn cluster_status(args: StartArgs) -> CelResult<()> {
    let mut owner_id = String::new();
    let mesh = build_mesh(&args, &mut owner_id).await?;
    let host: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    mesh.set_host(host).await;
    tokio::time::sleep(Duration::from_secs(args.settle.max(1))).await;

    let s = mesh.cluster_status().await;
    let is_sup = mesh.is_supervisor().await;
    println!(
        "node={} cluster={} alive={} suspect={} dead={} vms={} orphans={} supervisor={}",
        s.self_id, s.cluster, s.alive, s.suspect, s.dead,
        s.total_vms, s.orphaned_vms, is_sup,
    );
    println!("-- members --");
    println!("{:<16}  {:<22}  {:<8}  epoch", "node", "addr", "status");
    for r in &s.members {
        println!("{:<16}  {:<22}  {:<8}  {}", r.id, r.addr, format!("{:?}", r.status), r.epoch);
    }
    println!("-- vms --");
    println!("{:<28}  {:<8}  {:<6}  {}", "path", "state", "alive", "label");
    for v in &s.vms {
        println!("{:<28}  {:<8}  {:<6}  {}", v.path(), v.state, v.owner_alive, v.label);
    }
    let _ = mesh.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::VmState;

    #[test]
    fn create_then_list_then_state() {
        let mut c = Controller::in_memory();
        run_vm_cmd(&mut c, VmCmd::Create(CreateArgs { label: "alpha".into() })).unwrap();
        run_vm_cmd(&mut c, VmCmd::Create(CreateArgs { label: "beta".into() })).unwrap();
        run_vm_cmd(&mut c, VmCmd::List).unwrap();
        run_vm_cmd(&mut c, VmCmd::State(IdArg { target: "/vms/1".into() })).unwrap();
    }

    #[test]
    fn start_via_numeric_target_works() {
        let mut c = Controller::in_memory();
        c.create_vm("").unwrap();
        run_vm_cmd(&mut c, VmCmd::Start(IdArg { target: "0".into() })).unwrap();
        assert_eq!(c.vm_state(VmId(0)).unwrap(), VmState::Halted);
    }

    #[test]
    fn start_via_path_target_works() {
        let mut c = Controller::in_memory();
        c.create_vm("").unwrap();
        run_vm_cmd(&mut c, VmCmd::Start(IdArg { target: "/vms/0".into() })).unwrap();
        assert_eq!(c.vm_state(VmId(0)).unwrap(), VmState::Halted);
    }

    #[test]
    fn stop_unallocated_id_errors() {
        let mut c = Controller::in_memory();
        let r = run_vm_cmd(&mut c, VmCmd::Stop(IdArg { target: "/vms/2".into() }));
        assert!(r.is_err());
    }

    #[test]
    fn create_label_too_long_is_rejected() {
        let mut c = Controller::in_memory();
        let big = "x".repeat(33);
        let r = run_vm_cmd(&mut c, VmCmd::Create(CreateArgs { label: big }));
        assert!(r.is_err());
    }
}
