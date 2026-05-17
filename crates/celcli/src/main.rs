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
// CLI table headers intentionally mirror the format-string shape of
// the data-row `println!` that follows. Allowing `print_literal`
// keeps header + row formatting symmetric and visually aligned.
#![allow(clippy::print_literal)]
// Clap-derived top-level `Cmd` enum is created exactly once per CLI
// invocation; boxing variants purely to balance their size buys
// nothing and obscures the command surface.
#![allow(clippy::large_enum_variant)]

pub mod boot;
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

use crate::vm::{Controller, VmId, VmSpec};

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
    /// W18: inspect a disk image (raw / qcow2). VMDK and VHDX are
    /// recognised but their backends land in Phase 2 (W18.2).
    Image {
        #[command(subcommand)]
        op: ImageCmd,
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
    /// W20: print the Prometheus-format mesh metrics exposition
    /// after letting gossip settle. Future revisions will expose this
    /// as a long-lived `/metrics` HTTP endpoint; the one-shot form
    /// here lets operators capture a snapshot from any node without
    /// running a daemon.
    Metrics(StartArgs),
    /// W14: polished VM subcommand tree (create/list/start/stop/
    /// delete/attach-volume/detach-volume) targeting the cluster.
    Vm {
        #[command(subcommand)]
        op: ClusterVmCmd,
    },
    /// W14: polished volume subcommand tree (create/list/delete/
    /// read/write/snapshot/snapshots/restore) targeting the cluster.
    Vol {
        #[command(subcommand)]
        op: ClusterVolCmd,
    },
}

#[derive(Debug, Subcommand)]
enum ClusterVmCmd {
    /// Allocate a VM on the target node.
    Create(VmCreateArgs),
    /// List federated VMs (cluster-wide view).
    List(ClusterRpcArgs),
    /// Start a VM on the target node.
    Start(VmTargetArgs),
    /// Stop a VM on the target node.
    Stop(VmTargetArgs),
    /// Delete a stopped/halted VM on the target node.
    Delete(VmTargetArgs),
    /// Attach a volume to a VM.
    AttachVolume(VmAttachArgs),
    /// Detach a volume from a VM.
    DetachVolume(VmDetachArgs),
}

#[derive(Debug, Subcommand)]
enum ClusterVolCmd {
    /// Create a volume on the target node.
    Create(VolCreateArgs),
    /// List volumes on the target node.
    List(VolListArgs),
    /// Delete a volume on the target node.
    Delete(VolIdArgs),
    /// Read bytes from a volume.
    Read(VolReadArgs),
    /// Write bytes to a volume.
    Write(VolWriteArgs),
    /// Snapshot a volume.
    Snapshot(VolSnapshotArgs),
    /// List snapshots, optionally filtered to one volume.
    Snapshots(VolSnapshotListArgs),
    /// Restore a snapshot onto its parent volume.
    Restore(VolSnapshotIdArgs),
    /// Delete a snapshot.
    DeleteSnapshot(VolSnapshotIdArgs),
}

#[derive(Debug, Args, Clone)]
struct ClusterRpcArgs {
    #[command(flatten)]
    common: StartArgs,
    /// Target node id.
    #[arg(long)]
    target: String,
    /// RPC timeout in milliseconds.
    #[arg(long, default_value_t = 5_000)]
    timeout_ms: u64,
}

#[derive(Debug, Args, Clone)]
struct VmCreateArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    /// Free-form label, ≤ 32 chars.
    #[arg(long, default_value = "")]
    label: String,
    /// Restart policy: `never` or `always`.
    #[arg(long, default_value = "never")]
    restart: String,
}

#[derive(Debug, Args, Clone)]
struct VmTargetArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    /// Slot id on the target.
    #[arg(long)]
    vm_id: u32,
}

#[derive(Debug, Args, Clone)]
struct VmAttachArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    /// Slot id on the target.
    #[arg(long)]
    vm_id: u32,
    /// Volume id (e.g. `n2/v1`).
    #[arg(long)]
    volume_id: String,
    /// Mount-point name inside the guest.
    #[arg(long)]
    mount_name: String,
}

#[derive(Debug, Args, Clone)]
struct VmDetachArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    #[arg(long)]
    vm_id: u32,
    #[arg(long)]
    volume_id: String,
}

#[derive(Debug, Args, Clone)]
struct VolCreateArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    /// Volume name, ≤ 64 chars.
    #[arg(long)]
    name: String,
    /// Logical size in bytes.
    #[arg(long)]
    size: u64,
}

#[derive(Debug, Args, Clone)]
struct VolListArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
}

#[derive(Debug, Args, Clone)]
struct VolIdArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    #[arg(long)]
    volume_id: String,
}

#[derive(Debug, Args, Clone)]
struct VolReadArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    #[arg(long)]
    volume_id: String,
    #[arg(long, default_value_t = 0)]
    offset: u64,
    #[arg(long)]
    len: u64,
}

#[derive(Debug, Args, Clone)]
struct VolWriteArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    #[arg(long)]
    volume_id: String,
    #[arg(long, default_value_t = 0)]
    offset: u64,
    /// UTF-8 string payload. For binary data use `--bytes-hex`.
    #[arg(long, default_value = "")]
    text: String,
    /// Hex-encoded byte payload, takes precedence over `--text`.
    #[arg(long, default_value = "")]
    bytes_hex: String,
}

#[derive(Debug, Args, Clone)]
struct VolSnapshotArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    #[arg(long)]
    volume_id: String,
    /// Snapshot label.
    #[arg(long)]
    name: String,
}

#[derive(Debug, Args, Clone)]
struct VolSnapshotListArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    /// Optional filter — list only snapshots of this volume.
    #[arg(long, default_value = "")]
    volume_id: String,
}

#[derive(Debug, Args, Clone)]
struct VolSnapshotIdArgs {
    #[command(flatten)]
    rpc: ClusterRpcArgs,
    #[arg(long)]
    snapshot_id: String,
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
    /// W21: bind a lightweight admin HTTP server on this address
    /// and expose `GET /metrics`, `GET /healthz`, `GET /readyz`.
    /// Empty string disables the server (default). Use e.g.
    /// `127.0.0.1:9100` for local scraping or `0.0.0.0:9100` to
    /// expose to the rack. The server is a single zero-overhead
    /// tokio task off the gossip hot path.
    #[arg(long, default_value = "")]
    admin_addr: String,
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
    /// W19: move a terminal VM back to `Created` without freeing the
    /// slot. Preserves the recorded boot-blob digest so a subsequent
    /// `start` runs image-content drift detection against it.
    Reset(IdArg),
    /// W20: print aggregate controller counters (slot occupancy,
    /// per-state totals, boot-blob coverage). Operator-friendly
    /// observability that does not require running a mesh.
    Stats,
}

#[derive(Debug, Args)]
struct CreateArgs {
    /// Free-form label, ≤ 32 chars.
    #[arg(long, default_value = "")]
    label: String,
    /// W18: path to a backing disk image. The file is validated
    /// (format-detected and, for qcow2, header-parsed) up front so
    /// bad images fail at create time instead of at start time. The
    /// path is then stored verbatim on the VM record. `celhyper`
    /// consumption of this field is gated on milestone W18.3.
    #[arg(long)]
    image: Option<PathBuf>,
    /// W18: vCPU count to request, 1..=64.
    #[arg(long)]
    cpu: Option<u32>,
    /// W18: guest RAM. Accepts plain bytes or `K`/`M`/`G`/`T` suffixes
    /// (case-insensitive); SI and binary suffixes are treated
    /// identically (both `4G` and `4Gi` mean 4 GiB).
    #[arg(long)]
    memory: Option<String>,
}

#[derive(Debug, Args)]
struct IdArg {
    /// Either a numeric id (`0`) or a path (`/vms/0`).
    target: String,
}

#[derive(Debug, Subcommand)]
enum ImageCmd {
    /// Inspect a disk image and print its format / size / cluster info.
    Inspect(ImageInspectArgs),
    /// W19: compute a content-stable CRC-32C over the entire virtual
    /// disk. Useful for attesting that two image files (potentially
    /// in different on-disk formats) hold the same logical data.
    Checksum(ImageInspectArgs),
}

#[derive(Debug, Args)]
struct ImageInspectArgs {
    /// Path to the image file.
    path: PathBuf,
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
            let mut c = Controller::load(&state_path)?
                .with_stage_root(crate::vm::default_stage_root());
            run_vm_cmd(&mut c, op)?;
            c.save()?;
        }
        Cmd::Image { op } => {
            run_image_cmd(op)?;
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
            // Validate the image up front so bad files fail loudly at
            // create time, not later at start time.
            if let Some(p) = &a.image {
                let info = celimage::inspect(p)?;
                tracing::info!(
                    image = %p.display(),
                    format = info.format.tag(),
                    virtual_size = info.virtual_size,
                    "vm create: image validated",
                );
            }
            let memory_mib = a.memory.as_deref()
                .map(parse_size_to_mib)
                .transpose()?;
            let spec = VmSpec {
                label: a.label,
                image_path: a.image.as_ref().map(|p| p.display().to_string()),
                cpu_count: a.cpu,
                memory_mib,
            };
            let id = c.create_vm_with(spec)?;
            println!("created {} ({})", id.0, Controller::path_for(id));
        }
        VmCmd::List => {
            let rows = c.list_vms();
            println!(
                "{:>4}  {:<8}  {:<10}  {:<4}  {:<8}  {:<24}  {}",
                "id", "state", "last_exit", "cpu", "mem_mib", "image", "label",
            );
            for r in rows {
                let exit = r.last_exit.map_or_else(|| "-".to_string(), |x| x.to_string());
                let cpu  = r.cpu_count.map_or_else(|| "-".to_string(), |x| x.to_string());
                let mem  = r.memory_mib.map_or_else(|| "-".to_string(), |x| x.to_string());
                let img  = r.image_path.clone().unwrap_or_else(|| "-".to_string());
                let img_trunc = if img.len() > 24 { format!("…{}", &img[img.len() - 23..]) } else { img };
                println!(
                    "{:>4}  {:<8}  {:<10}  {:<4}  {:<8}  {:<24}  {}",
                    r.id.0, r.state.tag(), exit, cpu, mem, img_trunc, r.label,
                );
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
        VmCmd::Reset(a) => {
            let id = resolve(c, &a.target)?;
            let s = c.reset_vm(id)?;
            println!("reset {} -> {}", id.0, s.tag());
        }
        VmCmd::Stats => {
            let s = c.stats();
            println!("slots          {}/{} used ({} free)",
                     s.allocated, s.slots_total, s.slots_free);
            println!("created        {}", s.created);
            println!("running        {}", s.running);
            println!("halted         {}", s.halted);
            println!("stopped        {}", s.stopped);
            println!("faulted        {}", s.faulted);
            println!("with_boot_blob {}", s.with_boot_blob);
        }
    }
    Ok(())
}

fn run_image_cmd(op: ImageCmd) -> CelResult<()> {
    match op {
        ImageCmd::Inspect(a) => {
            let info = celimage::inspect(&a.path)?;
            let cluster = info.cluster_size
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into());
            println!("path           {}", a.path.display());
            println!("format         {}", info.format.tag());
            println!("backend        {}", info.backend);
            println!("virtual_size   {} ({})", info.virtual_size, human_size(info.virtual_size));
            println!("cluster_size   {}", cluster);
        }
        ImageCmd::Checksum(a) => {
            let img = celimage::open(&a.path)?;
            let crc = celimage::full_image_crc32c(img.as_ref())?;
            println!("path           {}", a.path.display());
            println!("format         {}", img.info().format.tag());
            println!("virtual_size   {} ({})", img.virtual_size(), human_size(img.virtual_size()));
            println!("crc32c         {:08x}", crc);
        }
    }
    Ok(())
}

/// Parse a memory size like `4G`, `512Mi`, `2048M`, `1073741824` to MiB.
/// SI (`K`, `M`, `G`, `T`) and binary (`Ki`, `Mi`, `Gi`, `Ti`) suffixes
/// are treated identically — both `4G` and `4Gi` mean 4 GiB. Plain
/// integers are interpreted as raw bytes.
fn parse_size_to_mib(s: &str) -> CelResult<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(CelError::Invalid("memory: empty"));
    }
    // Split numeric prefix from suffix.
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num_str, suffix) = s.split_at(split);
    let n: u64 = num_str.parse()
        .map_err(|_| CelError::Invalid("memory: not a non-negative integer"))?;
    let suffix = suffix.trim_end_matches('B').trim_end_matches('b').to_ascii_uppercase();
    let bytes: u64 = match suffix.as_str() {
        "" => n,
        "K" | "KI" => n.checked_mul(1024)
            .ok_or(CelError::Invalid("memory: overflow"))?,
        "M" | "MI" => n.checked_mul(1024 * 1024)
            .ok_or(CelError::Invalid("memory: overflow"))?,
        "G" | "GI" => n.checked_mul(1024 * 1024 * 1024)
            .ok_or(CelError::Invalid("memory: overflow"))?,
        "T" | "TI" => n.checked_mul(1024u64.pow(4))
            .ok_or(CelError::Invalid("memory: overflow"))?,
        _ => return Err(CelError::Invalid("memory: unknown suffix")),
    };
    if bytes == 0 {
        return Err(CelError::Invalid("memory: zero"));
    }
    if bytes % (1024 * 1024) != 0 {
        return Err(CelError::Invalid("memory: not a multiple of 1 MiB"));
    }
    Ok(bytes / (1024 * 1024))
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1024u64.pow(4), "TiB"),
        (1024u64.pow(3), "GiB"),
        (1024u64.pow(2), "MiB"),
        (1024u64,        "KiB"),
    ];
    for (div, label) in UNITS {
        if bytes >= *div {
            let q = bytes / div;
            let r = bytes % div;
            if r == 0 {
                return format!("{q} {label}");
            }
            return format!("{:.2} {label}", bytes as f64 / *div as f64);
        }
    }
    format!("{bytes} B")
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
        ClusterCmd::Metrics(args) => cluster_metrics(args).await,
        ClusterCmd::Vm  { op }    => cluster_vm_cmd(op).await,
        ClusterCmd::Vol { op }    => cluster_vol_cmd(op).await,
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

    // W21: optional admin HTTP server. Empty string disables.
    let admin = if args.admin_addr.is_empty() {
        None
    } else {
        let srv = celmesh::AdminServer::bind(mesh.clone(), &args.admin_addr).await?;
        println!("celctl: admin server on http://{}/  (/metrics /healthz /readyz)",
                 srv.addr);
        Some(srv)
    };

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
    if let Some(srv) = admin { srv.shutdown(); }
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
        _ => return Err(CelError::Invalid(
            "restart: expected 'never' or 'always'",
        )),
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
        // W15 networking replies — pretty-printed by the dedicated
        // `cluster network/secgroup/lb` subcommands; this generic
        // path just acknowledges them.
        VmOpReply::NetworkCreated { network }      => println!("created network {} ({})", network.id, network.cidr),
        VmOpReply::NetworkDeleted { network_id }   => println!("deleted network {network_id}"),
        VmOpReply::NetworksListed { networks }     => {
            for n in networks { println!("{}  {}  {}", n.id, n.name, n.cidr); }
        }
        VmOpReply::NicAttached { nic }             => println!("nic {} ip={} vm={}", nic.id, nic.ip, nic.vm_id),
        VmOpReply::NicDetached { nic_id }          => println!("detached nic {nic_id}"),
        VmOpReply::NicsListed { nics }             => {
            for n in nics { println!("{}  vm={}  ip={}", n.id, n.vm_id, n.ip); }
        }
        VmOpReply::SecurityGroupCreated { sg }     => println!("created sg {} ({} rules)", sg.id, sg.rules.len()),
        VmOpReply::SecurityGroupDeleted { sg_id }  => println!("deleted sg {sg_id}"),
        VmOpReply::SecurityGroupsListed { sgs }    => {
            for s in sgs { println!("{}  {}  rules={}", s.id, s.name, s.rules.len()); }
        }
        VmOpReply::LoadBalancerCreated { lb }      => println!("created lb {} vip={}:{}", lb.id, lb.vip, lb.frontend_port),
        VmOpReply::LoadBalancerDeleted { lb_id }   => println!("deleted lb {lb_id}"),
        VmOpReply::LoadBalancersListed { lbs }     => {
            for l in lbs { println!("{}  vip={}:{}  backends={}", l.id, l.vip, l.frontend_port, l.backends.len()); }
        }
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

/// W20: print a Prometheus-format snapshot of mesh metrics for this
/// node after letting gossip settle for `--settle` seconds. No host
/// is installed; counters reflect transport + gossip activity only,
/// which is the right surface for "is this node talking to the
/// cluster" diagnostics.
async fn cluster_metrics(args: StartArgs) -> CelResult<()> {
    let mut owner_id = String::new();
    let mesh = build_mesh(&args, &mut owner_id).await?;
    tokio::time::sleep(Duration::from_secs(args.settle.max(1))).await;
    print!("{}", mesh.metrics_prometheus());
    let _ = mesh.shutdown().await;
    Ok(())
}

// -- Week-14 --------------------------------------------------------------

/// Build a transient mesh, install a local `MemVmHost`, let gossip
/// settle, and run `body` against the connected mesh. Tears the mesh
/// down on exit.
async fn with_transient_mesh<F, Fut, T>(rpc: ClusterRpcArgs, body: F) -> CelResult<T>
where
    F: FnOnce(Mesh) -> Fut,
    Fut: std::future::Future<Output = CelResult<T>>,
{
    let mut owner_id = String::new();
    let mesh = build_mesh(&rpc.common, &mut owner_id).await?;
    let host: Arc<dyn VmHost> = Arc::new(MemVmHost::new());
    mesh.set_host(host).await;
    tokio::time::sleep(Duration::from_secs(rpc.common.settle.max(1))).await;
    let r = body(mesh.clone()).await;
    let _ = mesh.shutdown().await;
    r
}

fn parse_restart(s: &str) -> CelResult<RestartPolicy> {
    match s {
        "always" => Ok(RestartPolicy::Always),
        "never"  => Ok(RestartPolicy::Never),
        _        => Err(CelError::Invalid("restart: expected 'never' or 'always'")),
    }
}

fn decode_hex(s: &str) -> CelResult<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(CelError::Invalid("bytes-hex: odd length"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nib(bytes[i])?;
        let lo = hex_nib(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nib(b: u8) -> CelResult<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(CelError::Invalid("bytes-hex: non-hex char")),
    }
}

fn render_bytes(bytes: &[u8]) -> String {
    // Print as quoted text if printable ASCII, else hex.
    if bytes.iter().all(|b| (0x20..0x7f).contains(b) || *b == b'\n' || *b == b'\t') {
        match std::str::from_utf8(bytes) {
            Ok(s)  => format!("{s:?}"),
            Err(_) => bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        }
    } else {
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    }
}

async fn cluster_vm_cmd(op: ClusterVmCmd) -> CelResult<()> {
    match op {
        ClusterVmCmd::Create(args) => {
            let restart = parse_restart(&args.restart)?;
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::Create { label: args.label.clone(), restart_policy: restart },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::Created { vm_id } = reply {
                    println!("created vm {target}/{vm_id} (path /cluster/{target}/vms/{vm_id})");
                }
                Ok(())
            }).await
        }
        ClusterVmCmd::List(rpc) => {
            with_transient_mesh(rpc.clone(), |mesh| async move {
                let target = NodeId(rpc.target.clone());
                let reply = mesh.invoke(
                    &target, VmOp::List, Duration::from_millis(rpc.timeout_ms),
                ).await?;
                if let VmOpReply::Listed { rows } = reply {
                    println!("{:<28}  {:<8}  {}", "path", "state", "label");
                    for r in rows {
                        println!("{:<28}  {:<8}  {}", r.path(), r.state, r.label);
                    }
                }
                Ok(())
            }).await
        }
        ClusterVmCmd::Start(args)  => simple_vm_op(&args, |id| VmOp::Start  { vm_id: id }).await,
        ClusterVmCmd::Stop(args)   => simple_vm_op(&args, |id| VmOp::Stop   { vm_id: id }).await,
        ClusterVmCmd::Delete(args) => simple_vm_op(&args, |id| VmOp::Delete { vm_id: id }).await,
        ClusterVmCmd::AttachVolume(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::AttachVolume {
                        vm_id: args.vm_id,
                        volume_id: celmesh::VolumeId(args.volume_id.clone()),
                        mount_name: args.mount_name.clone(),
                    },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::Attachments { vm_id, volumes } = reply {
                    println!("vm {target}/{vm_id} now has {} attachment(s)", volumes.len());
                    for a in volumes {
                        println!("  {} -> {}", a.mount_name, a.volume_id);
                    }
                }
                Ok(())
            }).await
        }
        ClusterVmCmd::DetachVolume(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::DetachVolume {
                        vm_id: args.vm_id,
                        volume_id: celmesh::VolumeId(args.volume_id.clone()),
                    },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::Attachments { vm_id, volumes } = reply {
                    println!("vm {target}/{vm_id} now has {} attachment(s)", volumes.len());
                }
                Ok(())
            }).await
        }
    }
}

async fn simple_vm_op<F>(args: &VmTargetArgs, mk: F) -> CelResult<()>
where
    F: FnOnce(u32) -> VmOp,
{
    let op = mk(args.vm_id);
    let target = NodeId(args.rpc.target.clone());
    let timeout_ms = args.rpc.timeout_ms;
    with_transient_mesh(args.rpc.clone(), |mesh| async move {
        let reply = mesh.invoke(&target, op, Duration::from_millis(timeout_ms)).await?;
        match reply {
            VmOpReply::State { vm_id, state }  => println!("vm {target}/{vm_id} state={state}"),
            VmOpReply::Deleted { vm_id }       => println!("deleted vm {target}/{vm_id}"),
            other                              => println!("{other:?}"),
        }
        Ok(())
    }).await
}

async fn cluster_vol_cmd(op: ClusterVolCmd) -> CelResult<()> {
    match op {
        ClusterVolCmd::Create(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::CreateVolume { name: args.name.clone(), size_bytes: args.size },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::VolumeCreated { volume } = reply {
                    println!("created volume {} (size={} owner={} name={})",
                             volume.id, volume.size_bytes, volume.owner, volume.name);
                }
                Ok(())
            }).await
        }
        ClusterVolCmd::List(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target, VmOp::ListVolumes, Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::VolumesListed { volumes } = reply {
                    println!("{:<24}  {:>10}  {:<8}  {}", "id", "size", "owner", "name");
                    for v in volumes {
                        println!("{:<24}  {:>10}  {:<8}  {}", v.id, v.size_bytes, v.owner, v.name);
                    }
                }
                Ok(())
            }).await
        }
        ClusterVolCmd::Delete(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::DeleteVolume { volume_id: celmesh::VolumeId(args.volume_id.clone()) },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::VolumeDeleted { volume_id } = reply {
                    println!("deleted {volume_id}");
                }
                Ok(())
            }).await
        }
        ClusterVolCmd::Read(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::ReadVolume {
                        volume_id: celmesh::VolumeId(args.volume_id.clone()),
                        offset: args.offset, len: args.len,
                    },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::VolumeData { volume_id, bytes } = reply {
                    println!("{volume_id} @ {} ({} bytes): {}",
                             args.offset, bytes.len(), render_bytes(&bytes));
                }
                Ok(())
            }).await
        }
        ClusterVolCmd::Write(args) => {
            let bytes = if !args.bytes_hex.is_empty() {
                decode_hex(&args.bytes_hex)?
            } else {
                args.text.as_bytes().to_vec()
            };
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::WriteVolume {
                        volume_id: celmesh::VolumeId(args.volume_id.clone()),
                        offset: args.offset, bytes,
                    },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::VolumeWritten { volume_id, bytes_written } = reply {
                    println!("wrote {bytes_written} bytes to {volume_id} @ {}", args.offset);
                }
                Ok(())
            }).await
        }
        ClusterVolCmd::Snapshot(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::CreateSnapshot {
                        volume_id: celmesh::VolumeId(args.volume_id.clone()),
                        name: args.name.clone(),
                    },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::SnapshotCreated { snapshot } = reply {
                    println!("created snapshot {} of {} ({} bytes)",
                             snapshot.id, snapshot.volume, snapshot.size_bytes);
                }
                Ok(())
            }).await
        }
        ClusterVolCmd::Snapshots(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let filter = if args.volume_id.is_empty() {
                    None
                } else {
                    Some(celmesh::VolumeId(args.volume_id.clone()))
                };
                let reply = mesh.invoke(
                    &target,
                    VmOp::ListSnapshots { volume_id: filter },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::SnapshotsListed { snapshots } = reply {
                    println!("{:<28}  {:<18}  {:>10}  {}", "id", "volume", "size", "name");
                    for s in snapshots {
                        println!("{:<28}  {:<18}  {:>10}  {}",
                                 s.id, s.volume, s.size_bytes, s.name);
                    }
                }
                Ok(())
            }).await
        }
        ClusterVolCmd::Restore(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::RestoreSnapshot {
                        snapshot_id: celmesh::SnapshotId(args.snapshot_id.clone()),
                    },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::SnapshotRestored { snapshot_id } = reply {
                    println!("restored {snapshot_id}");
                }
                Ok(())
            }).await
        }
        ClusterVolCmd::DeleteSnapshot(args) => {
            with_transient_mesh(args.rpc.clone(), |mesh| async move {
                let target = NodeId(args.rpc.target.clone());
                let reply = mesh.invoke(
                    &target,
                    VmOp::DeleteSnapshot {
                        snapshot_id: celmesh::SnapshotId(args.snapshot_id.clone()),
                    },
                    Duration::from_millis(args.rpc.timeout_ms),
                ).await?;
                if let VmOpReply::SnapshotDeleted { snapshot_id } = reply {
                    println!("deleted {snapshot_id}");
                }
                Ok(())
            }).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::VmState;

    #[test]
    fn create_then_list_then_state() {
        let mut c = Controller::in_memory();
        run_vm_cmd(&mut c, VmCmd::Create(CreateArgs { label: "alpha".into(), image: None, cpu: None, memory: None })).unwrap();
        run_vm_cmd(&mut c, VmCmd::Create(CreateArgs { label: "beta".into(), image: None, cpu: None, memory: None })).unwrap();
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
        let r = run_vm_cmd(&mut c, VmCmd::Create(CreateArgs { label: big, image: None, cpu: None, memory: None }));
        assert!(r.is_err());
    }

    #[test]
    fn parse_size_to_mib_accepts_common_forms() {
        assert_eq!(parse_size_to_mib("1M").unwrap(), 1);
        assert_eq!(parse_size_to_mib("1Mi").unwrap(), 1);
        assert_eq!(parse_size_to_mib("4G").unwrap(), 4 * 1024);
        assert_eq!(parse_size_to_mib("4Gi").unwrap(), 4 * 1024);
        assert_eq!(parse_size_to_mib("1048576").unwrap(), 1); // raw bytes
    }

    #[test]
    fn parse_size_to_mib_rejects_non_mib_aligned() {
        assert!(parse_size_to_mib("1023K").is_err());
        assert!(parse_size_to_mib("0").is_err());
        assert!(parse_size_to_mib("12X").is_err());
        assert!(parse_size_to_mib("").is_err());
    }

    #[test]
    fn create_with_image_validates_and_records() {
        use std::io::Write;
        let mut t = tempfile::NamedTempFile::new().unwrap();
        t.write_all(&[0u8; 4096]).unwrap();
        t.flush().unwrap();
        let mut c = Controller::in_memory();
        let args = CreateArgs {
            label: "img-vm".into(),
            image: Some(t.path().to_path_buf()),
            cpu: Some(2),
            memory: Some("128M".into()),
        };
        run_vm_cmd(&mut c, VmCmd::Create(args)).unwrap();
        let row = &c.list_vms()[0];
        assert_eq!(row.cpu_count, Some(2));
        assert_eq!(row.memory_mib, Some(128));
        assert!(row.image_path.is_some());
    }

    #[test]
    fn create_with_bad_image_path_fails() {
        let mut c = Controller::in_memory();
        let args = CreateArgs {
            label: "bad".into(),
            image: Some(PathBuf::from("/definitely/does/not/exist.qcow2")),
            cpu: None,
            memory: None,
        };
        assert!(run_vm_cmd(&mut c, VmCmd::Create(args)).is_err());
    }
}

