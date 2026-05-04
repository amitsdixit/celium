//! `Mesh` — the gossip + federation engine.
//!
//! A `Mesh` owns a `Membership`, a `NamespaceFederation`, and a
//! `Transport`. It runs two long-lived async tasks:
//!
//! * **`receiver`** decodes incoming frames, validates the cluster
//!   id, and merges them into the local view.
//! * **`gossiper`** periodically picks a peer at random, sends a
//!   `Sync` envelope, and runs the failure detector.
//!
//! Every public mutation goes through `Arc<Mutex<Inner>>`, so the
//! CLI (which runs in the main task) and the gossip loop see a
//! consistent view.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use celcommon::{CelError, CelResult};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};

use crate::federation::{NamespaceFederation, RemoteVm, RestartPolicy};
use crate::host::VmHost;
use crate::membership::{Membership, NodeId, NodeInfo, NodeStatus};
use crate::metrics::MeshMetrics;
use crate::proto::{
    DecodeError, Envelope, Payload, VmOp, VmOpReply, VmOpResult, MAGIC, PROTO_VERSION,
};
use crate::transport::Transport;

/// Configuration for a `Mesh` instance.
#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// Cluster name. Frames whose `cluster` differs are dropped.
    pub cluster: String,
    /// Stable id of this node.
    pub node_id: NodeId,
    /// Address peers should reach us at — propagated via gossip.
    pub advertise_addr: String,
    /// Restart counter. Bump on every node start.
    pub epoch: u64,
    /// Initial peer addresses to contact. May be empty for a
    /// single-node test.
    pub seeds: Vec<String>,
    /// How often the gossiper fires.
    pub gossip_interval: Duration,
    /// Timeout before a quiet peer is marked Suspect.
    pub timeout_suspect: Duration,
    /// Timeout before a Suspect is marked Dead.
    pub timeout_dead: Duration,
    /// How often the auto-supervisor task runs `run_supervisor_step`.
    /// `Duration::ZERO` disables the task entirely (Week-9 default
    /// for tests that drive the supervisor manually).
    pub supervisor_interval: Duration,
}

impl MeshConfig {
    /// Sensible defaults for tests and demos. Auto-supervisor is
    /// disabled by default so that older tests keep working — set
    /// `supervisor_interval` explicitly to enable it.
    #[must_use]
    pub fn defaults(node_id: impl Into<String>, addr: impl Into<String>) -> Self {
        Self {
            cluster:             "celium".into(),
            node_id:             NodeId(node_id.into()),
            advertise_addr:      addr.into(),
            epoch:               1,
            seeds:               Vec::new(),
            gossip_interval:     Duration::from_millis(50),
            timeout_suspect:     Duration::from_millis(500),
            timeout_dead:        Duration::from_millis(1500),
            supervisor_interval: Duration::ZERO,
        }
    }
}

/// Mutable state shared between the gossip loop and the public API.
struct Inner {
    config:     MeshConfig,
    membership: Membership,
    federation: NamespaceFederation,
    /// Monotonic counter used as our HLC.
    hlc:        u64,
    /// Peers we have ever heard about — kept separate from the
    /// membership view so we can ping `Suspect` rows too.
    known:      HashSet<String>,
}

impl Inner {
    fn tick_hlc(&mut self) -> u64 {
        self.hlc = self.hlc.saturating_add(1);
        self.hlc
    }

    fn self_row(&self) -> NodeInfo {
        // Always read the canonical row out of the membership table.
        // It is impossible for it to be missing — `Membership::new`
        // inserts it — but we still fall back gracefully.
        self.membership
            .get(self.membership.self_id())
            .cloned()
            .unwrap_or_else(|| NodeInfo {
                id: self.config.node_id.clone(),
                addr: self.config.advertise_addr.clone(),
                epoch: self.config.epoch,
                hlc: self.hlc,
                status: NodeStatus::Alive,
            })
    }
}

/// Live mesh handle. Cheap to clone; carries an `Arc<Mutex<Inner>>`
/// behind it.
#[derive(Clone)]
pub struct Mesh {
    inner:     Arc<Mutex<Inner>>,
    transport: Arc<dyn Transport>,
    tasks:     Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Pluggable VM host. `None` means "no local VM lifecycle yet";
    /// inbound `Request` frames are then NACKed with a clear error.
    host:      Arc<Mutex<Option<Arc<dyn VmHost>>>>,
    /// Outstanding `invoke` calls waiting for a `Response`. The key
    /// is the request id we minted; the value is the oneshot sender
    /// the caller is awaiting on.
    pending:   Arc<Mutex<HashMap<u64, oneshot::Sender<VmOpResult>>>>,
    /// Monotonic counter for outbound request ids.
    next_req:  Arc<AtomicU64>,
    /// W17: shared, lock-free counters covering gossip / RPC /
    /// failure-detector activity. Cloned together with the rest of
    /// the handle so every task sees the same values.
    metrics:   MeshMetrics,
}

/// One row returned by the supervisor when it recreates a VM that
/// was stranded on a Dead/Left owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartedVm {
    /// Owner that died.
    pub original_owner: NodeId,
    /// Original `vm_id` on the dead owner.
    pub original_vm_id: u32,
    /// New `vm_id` allocated locally.
    pub new_vm_id: u32,
    /// Free-form label preserved from the orphan row.
    pub label: String,
}

impl Mesh {
    /// Build a new mesh and launch its background tasks. The
    /// transport is consumed because gossip and the public API both
    /// share it through an `Arc`.
    pub async fn start(
        config: MeshConfig,
        transport: Arc<dyn Transport>,
    ) -> CelResult<Self> {
        let self_row = NodeInfo {
            id:     config.node_id.clone(),
            addr:   config.advertise_addr.clone(),
            epoch:  config.epoch,
            hlc:    0,
            status: NodeStatus::Alive,
        };
        let membership = Membership::new(
            config.cluster.clone(),
            self_row,
            config.timeout_suspect,
            config.timeout_dead,
        );
        let federation = NamespaceFederation::new(config.node_id.clone());
        let mut known = HashSet::new();
        for s in &config.seeds { known.insert(s.clone()); }

        let inner = Arc::new(Mutex::new(Inner {
            config,
            membership,
            federation,
            hlc: 0,
            known,
        }));
        let mesh = Mesh {
            inner,
            transport,
            tasks: Arc::new(Mutex::new(Vec::new())),
            host:    Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_req: Arc::new(AtomicU64::new(1)),
            metrics: MeshMetrics::new(),
        };

        let recv_task = tokio::spawn(receiver_loop(mesh.clone()));
        let goss_task = tokio::spawn(gossiper_loop(mesh.clone()));
        let mut tasks = mesh.tasks.lock().await;
        tasks.push(recv_task);
        tasks.push(goss_task);
        // Auto-supervisor: only spawn when explicitly enabled. The
        // task itself waits for a host to be registered before doing
        // anything so it is safe to start it before `set_host`.
        let sup_interval = mesh.inner.lock().await.config.supervisor_interval;
        if !sup_interval.is_zero() {
            let sup_task = tokio::spawn(supervisor_loop(mesh.clone(), sup_interval));
            tasks.push(sup_task);
        }
        drop(tasks);

        // Best-effort hello to every seed so discovery doesn't have
        // to wait for the first gossip tick.
        let seeds: Vec<String> = {
            let g = mesh.inner.lock().await;
            g.config.seeds.clone()
        };
        for s in seeds {
            let _ = mesh.send_hello(&s).await;
        }

        Ok(mesh)
    }

    /// Snapshot of every membership row.
    pub async fn members(&self) -> Vec<NodeInfo> {
        self.inner.lock().await.membership.snapshot()
    }

    /// Cluster-wide counter snapshot. W17 added this surface for
    /// `/metrics` endpoints, alerting, and deterministic tests; it
    /// is `O(1)` and lock-free on the read path.
    #[must_use]
    pub fn metrics(&self) -> crate::metrics::MeshMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Render every counter in the Prometheus text-exposition
    /// format. Useful for the future `/metrics` HTTP handler.
    #[must_use]
    pub fn metrics_prometheus(&self) -> String {
        self.metrics.render_prometheus()
    }

    /// Add `addr` as a runtime seed and best-effort send a `Hello`
    /// so the peer learns about us without waiting for a gossip
    /// tick. W17 added this so an operator can heal a partitioned
    /// cluster without bouncing nodes.
    ///
    /// # Errors
    /// Returns the underlying transport error if the immediate
    /// `Hello` cannot be encoded or shipped. The seed is recorded
    /// either way, so the next gossip tick will retry.
    pub async fn join(&self, addr: impl Into<String>) -> CelResult<()> {
        let addr = addr.into();
        {
            let mut g = self.inner.lock().await;
            g.known.insert(addr.clone());
        }
        self.metrics.inc_join_calls();
        // The hello round-trip is best-effort; failures bubble up
        // so the operator sees them, but the seed has already been
        // recorded for retry on the next gossiper tick.
        self.send_hello(&addr).await
    }

    /// Federated VM list — every node's view, with `owner_alive`
    /// computed from the current membership.
    pub async fn list_vms(&self) -> Vec<RemoteVm> {
        let mut g = self.inner.lock().await;
        // Refresh owner_alive against the live membership.
        let alive: std::collections::BTreeMap<NodeId, bool> = g
            .membership
            .snapshot()
            .into_iter()
            .map(|r| (r.id, r.status == NodeStatus::Alive))
            .collect();
        g.federation.refresh_alive(|id| alive.get(id).copied().unwrap_or(false));
        g.federation.list()
    }

    /// Replace this node's owned VM list. Bumps the local HLC and
    /// triggers an out-of-band gossip burst so peers learn quickly.
    pub async fn publish_local_vms(&self, mut rows: Vec<RemoteVm>) -> CelResult<()> {
        let (epoch, peers) = {
            let mut g = self.inner.lock().await;
            let hlc = g.tick_hlc();
            let epoch = g.config.epoch;
            let self_id = g.config.node_id.clone();
            for r in &mut rows {
                r.owner = self_id.clone();
                r.epoch = epoch;
                r.hlc = hlc;
                r.owner_alive = true;
            }
            g.federation.set_local(rows);
            g.membership.bump_self(hlc);
            let peers: Vec<String> = g
                .membership
                .snapshot()
                .into_iter()
                .filter(|r| r.id != *g.membership.self_id() && r.status != NodeStatus::Left)
                .map(|r| r.addr)
                .collect();
            (epoch, peers)
        };
        let _ = epoch;
        for p in peers {
            let _ = self.send_sync(&p).await;
        }
        Ok(())
    }

    /// Number of peers currently classified `Alive`. Includes self.
    pub async fn alive_count(&self) -> usize {
        self.inner.lock().await.membership.alive_count()
    }

    /// Send a voluntary departure to every known peer and abort the
    /// background tasks. Idempotent.
    pub async fn shutdown(&self) -> CelResult<()> {
        let (peers, env) = {
            let mut g = self.inner.lock().await;
            let hlc = g.tick_hlc();
            let env = Envelope {
                magic:   MAGIC.into(),
                version: PROTO_VERSION,
                from:    g.config.node_id.to_string(),
                cluster: g.config.cluster.clone(),
                hlc,
                payload: Payload::Goodbye,
            };
            let peers: Vec<String> = g
                .membership
                .snapshot()
                .into_iter()
                .filter(|r| r.id != *g.membership.self_id())
                .map(|r| r.addr)
                .collect();
            (peers, env)
        };
        let bytes = env.encode().map_err(|_| CelError::Internal("encode goodbye"))?;
        for p in peers {
            let _ = self.transport.send(&p, &bytes).await;
        }
        let mut tasks = self.tasks.lock().await;
        for t in tasks.drain(..) { t.abort(); }
        Ok(())
    }

    // -- Week-10: cross-node VM control + supervisor --------------------

    /// Register the local [`VmHost`]. Subsequent `Request` frames
    /// addressed at this node are dispatched to `host`. May be called
    /// at most once meaningfully — later calls overwrite the previous
    /// host and any in-flight ops on the old host return as normal.
    pub async fn set_host(&self, host: Arc<dyn VmHost>) {
        // Prime the host's owner id so subsequent volume ops can mint
        // node-scoped volume ids without requiring callers to first
        // run a snapshot.
        let self_id = self.self_id().await;
        let _ = host.snapshot(&self_id).await;
        *self.host.lock().await = Some(host);
    }

    /// Stable identifier of this node.
    pub async fn self_id(&self) -> NodeId {
        self.inner.lock().await.config.node_id.clone()
    }

    /// Apply `op` on `target`. If `target` is the local node we
    /// dispatch to the host directly; otherwise we ship a `Request`
    /// envelope and wait up to `wait` for the matching `Response`.
    ///
    /// After a successful `Create`/`Start`/`Stop`/`Delete` against
    /// the local host the federated rows are republished so peers
    /// learn quickly.
    pub async fn invoke(
        &self,
        target: &NodeId,
        op: VmOp,
        wait: Duration,
    ) -> CelResult<VmOpReply> {
        let self_id = { self.inner.lock().await.config.node_id.clone() };
        let _span = tracing::info_span!(
            target: "celmesh::invoke",
            "invoke",
            from   = %self_id,
            to     = %target,
            op     = crate::capabilities::Capabilities::op_tag(&op),
        )
        .entered();
        if &self_id == target {
            // Local fast-path. Skips serialisation but uses the same
            // host trait the wire path uses.
            let host = match self.host.lock().await.clone() {
                Some(h) => h,
                None    => return Err(CelError::Invalid("no VmHost registered")),
            };
            let r = host.handle(op).await
                .map_err(|s| CelError::Io(format!("host: {s}")))?;
            self.republish_after_op(&host, &self_id).await?;
            return Ok(r);
        }

        // Look up target's address from the membership table. A
        // request for an unknown node is a hard error; a Dead/Left
        // peer is rejected up-front so callers don't burn the full
        // timeout.
        let addr = {
            let g = self.inner.lock().await;
            match g.membership.get(target) {
                None => return Err(CelError::Invalid("unknown target node")),
                Some(r) if matches!(r.status, NodeStatus::Dead | NodeStatus::Left) =>
                    return Err(CelError::Invalid("target node not Alive")),
                Some(r) => r.addr.clone(),
            }
        };

        let req_id = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<VmOpResult>();
        self.pending.lock().await.insert(req_id, tx);

        let env = {
            let mut g = self.inner.lock().await;
            let hlc = g.tick_hlc();
            Envelope {
                magic:   MAGIC.into(),
                version: PROTO_VERSION,
                from:    g.config.node_id.to_string(),
                cluster: g.config.cluster.clone(),
                hlc,
                payload: Payload::Request {
                    req_id,
                    target: target.to_string(),
                    op,
                },
            }
        };
        let bytes = env.encode().map_err(|_| CelError::Internal("encode request"))?;
        if let Err(e) = self.transport.send(&addr, &bytes).await {
            self.pending.lock().await.remove(&req_id);
            return Err(e);
        }
        self.metrics.inc_gossip_sent();
        self.metrics.inc_rpc_out();

        match timeout(wait, rx).await {
            Ok(Ok(VmOpResult::Ok(r)))  => Ok(r),
            Ok(Ok(VmOpResult::Err(s))) => {
                self.metrics.inc_rpc_errors();
                Err(CelError::Io(format!("remote host: {s}")))
            }
            Ok(Err(_))                 => {
                // Sender dropped — typically because the response
                // arrived after we'd already taken the slot, which
                // shouldn't happen, but is harmless.
                self.metrics.inc_rpc_errors();
                Err(CelError::Internal("response channel dropped"))
            }
            Err(_) => {
                self.pending.lock().await.remove(&req_id);
                self.metrics.inc_rpc_timeouts();
                Err(CelError::Timeout("mesh rpc".into()))
            }
        }
    }

    /// VMs whose owners are no longer Alive **and** whose
    /// `restart_policy` is `Always`. Used by the supervisor.
    pub async fn orphaned_vms(&self) -> Vec<RemoteVm> {
        let rows = self.list_vms().await;
        rows.into_iter()
            .filter(|r| !r.owner_alive && r.restart_policy == RestartPolicy::Always)
            .collect()
    }

    /// Deterministic supervisor election: the lowest-id Alive node
    /// in the cluster wins. Returns `true` if **this** node is the
    /// supervisor.
    pub async fn is_supervisor(&self) -> bool {
        let g = self.inner.lock().await;
        let me = &g.config.node_id;
        let lowest = g
            .membership
            .snapshot()
            .into_iter()
            .filter(|r| r.status == NodeStatus::Alive)
            .map(|r| r.id)
            .min();
        matches!(lowest, Some(id) if &id == me)
    }

    /// Run one supervision pass.
    ///
    /// If we are the elected supervisor, every orphaned VM with
    /// `restart_policy=Always` is recreated locally via the
    /// registered host. Returns the list of recreations made.
    ///
    /// Idempotent within a single pass: once an orphan has a local
    /// counterpart with the same label and original-owner annotation,
    /// it is skipped on subsequent passes.
    pub async fn run_supervisor_step(&self) -> CelResult<Vec<RestartedVm>> {
        if !self.is_supervisor().await {
            return Ok(Vec::new());
        }
        let host = match self.host.lock().await.clone() {
            Some(h) => h,
            None    => return Err(CelError::Invalid("no VmHost registered")),
        };
        let self_id = self.self_id().await;
        let orphans = self.orphaned_vms().await;
        if orphans.is_empty() {
            return Ok(Vec::new());
        }

        // Collect existing labels we already own to dedupe.
        let existing: HashSet<String> = host
            .snapshot(&self_id)
            .await
            .into_iter()
            .map(|r| r.label)
            .collect();

        let mut out = Vec::new();
        for orphan in orphans {
            // Encode the original origin in the new label so a second
            // pass can recognise the prior recreation. Truncate to fit
            // the 32-char label cap shared by host + controller.
            let new_label = format!("{}@{}", orphan.label, orphan.owner);
            let new_label = if new_label.len() > 32 {
                new_label[..32].to_string()
            } else {
                new_label
            };
            if existing.contains(&new_label) {
                continue;
            }
            let reply = host.handle(VmOp::Create {
                label: new_label.clone(),
                restart_policy: RestartPolicy::Never,
            }).await.map_err(|s| CelError::Io(format!("supervisor create: {s}")))?;
            if let VmOpReply::Created { vm_id } = reply {
                // Week-12: preserve volume attachments across the
                // restart. We use the dedicated `attach_preserved`
                // path so volumes whose vault lives on a third node
                // (or even a still-dead one) keep their metadata —
                // the user can re-bind data later when that vault is
                // reachable. Failures are logged but do not abort
                // the rest of the recovery pass.
                if !orphan.volumes.is_empty() {
                    if let Err(e) = host.attach_preserved(vm_id, orphan.volumes.clone()).await {
                        tracing::warn!(
                            target: "celmesh::supervisor",
                            new_vm_id = vm_id,
                            "attach_preserved failed: {e}"
                        );
                    }
                }
                out.push(RestartedVm {
                    original_owner: orphan.owner.clone(),
                    original_vm_id: orphan.vm_id,
                    new_vm_id: vm_id,
                    label: new_label,
                });
            }
        }

        if !out.is_empty() {
            self.metrics.inc_supervisor_restarts(out.len() as u64);
            self.republish_after_op(&host, &self_id).await?;
        }
        Ok(out)
    }

    /// Pull the host's current snapshot and push it through
    /// `publish_local_vms`. Called after every successful local op.
    async fn republish_after_op(
        &self,
        host: &Arc<dyn VmHost>,
        self_id: &NodeId,
    ) -> CelResult<()> {
        let rows = host.snapshot(self_id).await;
        self.publish_local_vms(rows).await
    }

    // -- Week-11: federated path-based ops + observability ------------

    /// Apply `op` against the VM addressed by `path`, where `path`
    /// follows the federated grammar `"/cluster/<node>/vms/<n>"`.
    ///
    /// `op` may be supplied with any `vm_id` — it is rewritten to the
    /// numeric segment of `path`. For `Create`, the `vm_id` field is
    /// allocated by the target host so the input is ignored.
    pub async fn invoke_path(
        &self,
        path: &str,
        mut op: VmOp,
        wait: Duration,
    ) -> CelResult<VmOpReply> {
        let (owner, vm_id) = parse_cluster_path(path)?;
        match &mut op {
            VmOp::Start  { vm_id: v }
            | VmOp::Stop   { vm_id: v }
            | VmOp::Delete { vm_id: v }
            | VmOp::AttachVolume { vm_id: v, .. }
            | VmOp::DetachVolume { vm_id: v, .. } => { *v = vm_id; }
            VmOp::Create { .. }
            | VmOp::List
            | VmOp::CreateVolume { .. }
            | VmOp::DeleteVolume { .. }
            | VmOp::ListVolumes
            | VmOp::ReadVolume { .. }
            | VmOp::WriteVolume { .. }
            | VmOp::CreateSnapshot { .. }
            | VmOp::ListSnapshots { .. }
            | VmOp::DeleteSnapshot { .. }
            | VmOp::RestoreSnapshot { .. }
            | VmOp::CreateNetwork { .. }
            | VmOp::DeleteNetwork { .. }
            | VmOp::ListNetworks
            | VmOp::DetachNic { .. }
            | VmOp::ListNics
            | VmOp::CreateSecurityGroup { .. }
            | VmOp::DeleteSecurityGroup { .. }
            | VmOp::ListSecurityGroups
            | VmOp::CreateLoadBalancer { .. }
            | VmOp::DeleteLoadBalancer { .. }
            | VmOp::ListLoadBalancers => {}
            VmOp::AttachNic { vm_id: v, .. } => { *v = vm_id; }
        }
        self.invoke(&owner, op, wait).await
    }

    /// Aggregate cluster snapshot. Cheap enough for the CLI to call
    /// every second and useful for logs / dashboards.
    pub async fn cluster_status(&self) -> ClusterStatus {
        let g = self.inner.lock().await;
        let members = g.membership.snapshot();
        let alive   = members.iter().filter(|r| r.status == NodeStatus::Alive).count();
        let suspect = members.iter().filter(|r| r.status == NodeStatus::Suspect).count();
        let dead    = members.iter().filter(|r| matches!(r.status, NodeStatus::Dead | NodeStatus::Left)).count();
        let me = g.config.node_id.clone();
        // Federation snapshot. We deliberately copy here so the caller
        // does not have to hold the lock.
        let mut vms = g.federation.list();
        // Patch owner_alive consistently with `list_vms`.
        let alive_set: std::collections::BTreeMap<NodeId, bool> = members
            .iter()
            .map(|r| (r.id.clone(), r.status == NodeStatus::Alive))
            .collect();
        for v in &mut vms {
            v.owner_alive = if v.owner == me { true }
                            else { alive_set.get(&v.owner).copied().unwrap_or(false) };
        }
        let total_vms = vms.len();
        let orphaned  = vms.iter().filter(|r| !r.owner_alive).count();
        ClusterStatus {
            self_id: me,
            cluster: g.config.cluster.clone(),
            members,
            vms,
            alive,
            suspect,
            dead,
            total_vms,
            orphaned_vms: orphaned,
        }
    }

    // -- internals --------------------------------------------------------

    async fn send_hello(&self, peer: &str) -> CelResult<()> {
        let env = {
            let mut g = self.inner.lock().await;
            let hlc = g.tick_hlc();
            let mut self_row = g.self_row();
            self_row.hlc = hlc;
            Envelope {
                magic:   MAGIC.into(),
                version: PROTO_VERSION,
                from:    g.config.node_id.to_string(),
                cluster: g.config.cluster.clone(),
                hlc,
                payload: Payload::Hello { node: self_row },
            }
        };
        let bytes = env.encode().map_err(|_| CelError::Internal("encode hello"))?;
        let r = self.transport.send(peer, &bytes).await;
        if r.is_ok() { self.metrics.inc_gossip_sent(); }
        r
    }

    async fn send_sync(&self, peer: &str) -> CelResult<()> {
        let env = {
            let mut g = self.inner.lock().await;
            let hlc = g.tick_hlc();
            let mut self_row = g.self_row();
            self_row.hlc = hlc;
            // Always include our (just-updated) self row.
            let mut nodes = g.membership.snapshot();
            if let Some(slot) = nodes.iter_mut().find(|r| r.id == self_row.id) {
                *slot = self_row;
            }
            let vms = g.federation.local_rows();
            Envelope {
                magic:   MAGIC.into(),
                version: PROTO_VERSION,
                from:    g.config.node_id.to_string(),
                cluster: g.config.cluster.clone(),
                hlc,
                payload: Payload::Sync { nodes, vms },
            }
        };
        let bytes = env.encode().map_err(|_| CelError::Internal("encode sync"))?;
        let r = self.transport.send(peer, &bytes).await;
        if r.is_ok() { self.metrics.inc_gossip_sent(); }
        r
    }
}

// ---------------------------------------------------------------------------
// Background tasks.
// ---------------------------------------------------------------------------

async fn receiver_loop(mesh: Mesh) {
    loop {
        let (bytes, src) = match mesh.transport.recv().await {
            Ok(p)  => p,
            Err(e) => {
                tracing::debug!("celmesh recv: {e:?}");
                // brief back-off so a permanently-dead transport does
                // not spin the CPU.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let env = match Envelope::decode(&bytes) {
            Ok(e) => e,
            Err(DecodeError::TooLarge | DecodeError::BadMagic
                | DecodeError::Malformed | DecodeError::VersionMismatch) => {
                mesh.metrics.inc_decode_errors();
                tracing::trace!("celmesh: dropping unparseable frame from {src}");
                continue;
            }
        };
        mesh.metrics.inc_gossip_recv();
        handle_envelope(&mesh, env, &src).await;
    }
}

async fn handle_envelope(mesh: &Mesh, env: Envelope, src: &str) {
    // Cluster check + bookkeeping run under the inner lock; RPC
    // dispatch must drop it before awaiting the host so the host
    // call doesn't deadlock against `publish_local_vms`.
    let payload = {
        let mut g = mesh.inner.lock().await;
        if env.cluster != g.config.cluster {
            mesh.metrics.inc_foreign_cluster_drops();
            tracing::trace!("celmesh: dropping foreign-cluster frame from {src}");
            return;
        }
        g.known.insert(src.to_string());
        env.payload
    };

    match payload {
        Payload::Hello { node } => {
            mesh.inner.lock().await.membership.merge(node);
        }
        Payload::Sync { nodes, vms } => {
            let mut g = mesh.inner.lock().await;
            for n in nodes { g.membership.merge(n); }
            for v in vms   { g.federation.merge(v); }
        }
        Payload::Goodbye => {
            let id = NodeId::from(env.from.as_str());
            mesh.inner.lock().await.membership.mark_left(&id);
        }
        Payload::Request { req_id, target, op } => {
            let self_id = { mesh.inner.lock().await.config.node_id.clone() };
            // Ignore requests not addressed at us — there is no
            // forwarding in v0.1.
            if target != self_id.0 {
                return;
            }
            mesh.metrics.inc_rpc_in();
            let host = mesh.host.lock().await.clone();
            let result = match host {
                None => VmOpResult::Err("no VmHost registered".into()),
                Some(h) => match h.handle(op).await {
                    Ok(reply) => {
                        // Republish so peers see the new state quickly.
                        let _ = mesh.republish_after_op(&h, &self_id).await;
                        VmOpResult::Ok(reply)
                    }
                    Err(s)    => VmOpResult::Err(s),
                },
            };
            // Look up the requester's address from membership and
            // ship the response back. If the requester is unknown
            // we silently drop — caller will time out.
            let from_id = NodeId::from(env.from.as_str());
            let addr = {
                let g = mesh.inner.lock().await;
                g.membership.get(&from_id).map(|r| r.addr.clone())
            };
            if let Some(addr) = addr {
                let resp = {
                    let mut g = mesh.inner.lock().await;
                    let hlc = g.tick_hlc();
                    Envelope {
                        magic:   MAGIC.into(),
                        version: PROTO_VERSION,
                        from:    g.config.node_id.to_string(),
                        cluster: g.config.cluster.clone(),
                        hlc,
                        payload: Payload::Response { req_id, result },
                    }
                };
                if let Ok(bytes) = resp.encode() {
                    if mesh.transport.send(&addr, &bytes).await.is_ok() {
                        mesh.metrics.inc_gossip_sent();
                    }
                }
            }
        }
        Payload::Response { req_id, result } => {
            let waiter = mesh.pending.lock().await.remove(&req_id);
            if let Some(tx) = waiter {
                let _ = tx.send(result);
            } else {
                tracing::trace!("celmesh: stray response req_id={req_id} from {src}");
            }
        }
    }
}

async fn gossiper_loop(mesh: Mesh) {
    let interval_dur = mesh.inner.lock().await.config.gossip_interval;
    let mut tick = interval(interval_dur);
    // Skip the immediate fire so `start()` returns before any
    // outbound traffic; the explicit hello in `start` already covers
    // first-contact.
    tick.tick().await;
    loop {
        tick.tick().await;

        // Pick a peer + advance the failure detector.
        let target: Option<String> = {
            let mut g = mesh.inner.lock().await;
            let delta = g.membership.tick(Instant::now());
            mesh.metrics.inc_suspect_promotions(delta.suspect_promotions as u64);
            mesh.metrics.inc_dead_promotions(delta.dead_promotions as u64);
            let candidates: Vec<String> = g
                .membership
                .snapshot()
                .into_iter()
                .filter(|r| r.id != *g.membership.self_id())
                .filter(|r| !matches!(r.status, NodeStatus::Left))
                .map(|r| r.addr)
                .chain(g.known.iter().cloned())
                .collect();
            pick_one(&candidates, g.config.epoch.wrapping_add(g.hlc))
        };

        if let Some(peer) = target {
            if let Err(e) = mesh.send_sync(&peer).await {
                tracing::trace!("celmesh sync to {peer} failed: {e:?}");
            }
        }
    }
}

/// Pick one entry from `xs` deterministically based on `seed`. Avoids
/// pulling in `rand`; even distribution is unnecessary at this scale.
fn pick_one(xs: &[String], seed: u64) -> Option<String> {
    if xs.is_empty() { return None; }
    let i = (seed as usize) % xs.len();
    Some(xs[i].clone())
}

/// Aggregate cluster snapshot returned by [`Mesh::cluster_status`].
#[derive(Debug, Clone)]
pub struct ClusterStatus {
    /// This node's id.
    pub self_id: NodeId,
    /// Cluster name.
    pub cluster: String,
    /// All membership rows.
    pub members: Vec<NodeInfo>,
    /// All federated VM rows (owner_alive populated).
    pub vms: Vec<RemoteVm>,
    /// Number of `Alive` rows.
    pub alive: usize,
    /// Number of `Suspect` rows.
    pub suspect: usize,
    /// Number of `Dead`+`Left` rows.
    pub dead: usize,
    /// Number of federated VMs.
    pub total_vms: usize,
    /// Number of VMs whose owner is no longer Alive.
    pub orphaned_vms: usize,
}

/// Parse `"/cluster/<node>/vms/<n>"` → `(NodeId, u32)`.
///
/// Errors map to `CelError::Invalid` so callers don't need to know the
/// grammar.
fn parse_cluster_path(path: &str) -> CelResult<(NodeId, u32)> {
    let suffix = path
        .strip_prefix("/cluster/")
        .ok_or(CelError::Invalid("path: missing /cluster/ prefix"))?;
    let (node, rest) = suffix
        .split_once('/')
        .ok_or(CelError::Invalid("path: expected /cluster/<node>/vms/<n>"))?;
    if node.is_empty() {
        return Err(CelError::Invalid("path: empty node id"));
    }
    let rest = rest
        .strip_prefix("vms/")
        .ok_or(CelError::Invalid("path: expected /vms/<n> segment"))?;
    if rest.is_empty() || rest.contains('/') {
        return Err(CelError::Invalid("path: expected exactly one VM segment"));
    }
    let n: u32 = rest
        .parse()
        .map_err(|_| CelError::Invalid("path: VM id is not a u32"))?;
    Ok((NodeId::from(node), n))
}

/// Background task that drives `run_supervisor_step` at the cadence
/// configured via `MeshConfig::supervisor_interval`. Errors are
/// logged at trace-level — we never panic because a transient
/// supervisor hiccup must not bring down the gossip plane.
async fn supervisor_loop(mesh: Mesh, period: Duration) {
    let mut tick = interval(period);
    // Skip the immediate fire so callers can finish wiring (e.g.
    // calling `set_host`) before the first pass.
    tick.tick().await;
    loop {
        tick.tick().await;
        match mesh.run_supervisor_step().await {
            Ok(v) if v.is_empty() => {}
            Ok(v) => {
                for r in &v {
                    tracing::info!(
                        target: "celmesh::supervisor",
                        original_owner = %r.original_owner,
                        original_vm_id = r.original_vm_id,
                        new_vm_id = r.new_vm_id,
                        label = %r.label,
                        "restarted orphan vm"
                    );
                }
            }
            Err(e) => {
                tracing::trace!(target: "celmesh::supervisor", "step failed: {e:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MemTransportFactory;

    fn cfg(id: &str, addr: &str, seeds: Vec<String>) -> MeshConfig {
        let mut c = MeshConfig::defaults(id, addr);
        c.seeds = seeds;
        c.gossip_interval = Duration::from_millis(20);
        c.timeout_suspect = Duration::from_millis(200);
        c.timeout_dead    = Duration::from_millis(600);
        c
    }

    #[tokio::test]
    async fn two_nodes_form_a_cluster() {
        let f = MemTransportFactory::new();
        let ta = Arc::new(f.bind("mem://a").await.unwrap());
        let tb = Arc::new(f.bind("mem://b").await.unwrap());

        let a = Mesh::start(cfg("a", "mem://a", vec!["mem://b".into()]), ta).await.unwrap();
        let b = Mesh::start(cfg("b", "mem://b", vec!["mem://a".into()]), tb).await.unwrap();

        // Wait for convergence.
        for _ in 0..40 {
            if a.alive_count().await == 2 && b.alive_count().await == 2 { break; }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(a.alive_count().await, 2);
        assert_eq!(b.alive_count().await, 2);
    }

    #[tokio::test]
    async fn vm_published_on_a_is_visible_on_b() {
        let f = MemTransportFactory::new();
        let ta = Arc::new(f.bind("mem://a").await.unwrap());
        let tb = Arc::new(f.bind("mem://b").await.unwrap());

        let a = Mesh::start(cfg("a", "mem://a", vec!["mem://b".into()]), ta).await.unwrap();
        let b = Mesh::start(cfg("b", "mem://b", vec!["mem://a".into()]), tb).await.unwrap();

        // Let them meet.
        tokio::time::sleep(Duration::from_millis(80)).await;

        let row = RemoteVm {
            owner: NodeId::from("a"),
            vm_id: 0,
            label: "alpha".into(),
            state: "created".into(),
            last_exit: None,
            epoch: 1,
            hlc: 0,
            owner_alive: true,
            restart_policy: crate::federation::RestartPolicy::Never,
            volumes: Vec::new(),
        };
        a.publish_local_vms(vec![row]).await.unwrap();

        for _ in 0..40 {
            let v = b.list_vms().await;
            if v.iter().any(|r| r.label == "alpha") { return; }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("vm did not propagate from a to b");
    }

    #[test]
    fn pick_one_handles_empty() {
        assert!(pick_one(&[], 0).is_none());
        assert_eq!(pick_one(&["x".into()], 7).as_deref(), Some("x"));
    }

    #[test]
    fn parse_cluster_path_accepts_well_formed_paths() {
        let (n, v) = parse_cluster_path("/cluster/n1/vms/0").unwrap();
        assert_eq!(n.as_str(), "n1");
        assert_eq!(v, 0);
        let (n, v) = parse_cluster_path("/cluster/host-7/vms/123").unwrap();
        assert_eq!(n.as_str(), "host-7");
        assert_eq!(v, 123);
    }

    #[test]
    fn parse_cluster_path_rejects_malformed() {
        for bad in [
            "",
            "/vms/0",
            "/cluster/",
            "/cluster//vms/0",
            "/cluster/n1",
            "/cluster/n1/vms",
            "/cluster/n1/vms/",
            "/cluster/n1/vms/abc",
            "/cluster/n1/vms/0/extra",
        ] {
            assert!(parse_cluster_path(bad).is_err(), "{bad:?} must reject");
        }
    }
}
