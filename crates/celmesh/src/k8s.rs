//! Kubernetes-as-a-Service personality (W15 groundwork).
//!
//! Celium is not Kubernetes — but a *personality* layered on top of
//! Celium can present a K8s-shaped surface to operators. W15 lays
//! the groundwork: a small orchestrator that takes a [`K8sClusterSpec`],
//! provisions a virtual network, a control-plane VM, and a set of
//! worker VMs on a single owner node via existing [`crate::Mesh`]
//! RPCs, and returns the resulting [`K8sCluster`] handle.
//!
//! This is intentionally *control-plane only*: no kubelet runs yet,
//! no etcd is started, no container is launched. The contract is
//! "after `K8sCluster::create` returns, you have N+1 Celium VMs all
//! attached to one private network with deterministic IPs, plus a
//! load balancer in front of the workers". The next personality
//! sprint will boot a real k3s/k0s image into each VM and join it
//! to the control plane.
//!
//! Capability surface
//! ------------------
//! Provisioning a cluster touches every networking + VM-write
//! capability: `VM_LIFECYCLE_WRITE`, `NETWORK_WRITE`, and
//! `LB_WRITE`. The owner node must grant all three on its
//! [`crate::host::MemVmHost`]; otherwise the orchestrator surfaces
//! the host's `capability denied` error verbatim.
//!
//! Determinism
//! -----------
//! Within a fresh network the linear NIC allocator hands out
//! addresses in order, so the control-plane always lands on the
//! `.1` host address and the `i`-th worker on `.(i+2)`. Tests rely
//! on this.

use std::time::Duration;

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

use crate::federation::RestartPolicy;
use crate::membership::NodeId;
use crate::mesh::Mesh;
use crate::proto::{
    Cidr4, LbAlgo, LbBackend, LoadBalancer, Nic, VirtualNetwork, VmOp, VmOpReply,
};

/// Caller-supplied recipe for a fresh K8s cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sClusterSpec {
    /// Operator-visible cluster name, used as the network and LB name.
    pub name: String,
    /// Number of worker VMs. Must be `>= 1` and `<= 16`.
    pub workers: u32,
    /// Private CIDR for the cluster network. Defaults to
    /// `"10.42.0.0/24"` when constructed via [`K8sClusterSpec::new`].
    pub cidr: String,
    /// Front-end port the cluster API server LB listens on.
    pub api_port: u16,
    /// Back-end port the API server runs on inside each control-plane VM.
    pub api_backend_port: u16,
    /// Distribution policy for the API server LB.
    pub algo: LbAlgo,
}

impl K8sClusterSpec {
    /// Sensible defaults: `10.42.0.0/24`, 6443→6443, round-robin.
    #[must_use]
    pub fn new(name: impl Into<String>, workers: u32) -> Self {
        Self {
            name: name.into(),
            workers,
            cidr: "10.42.0.0/24".into(),
            api_port: 6443,
            api_backend_port: 6443,
            algo: LbAlgo::RoundRobin,
        }
    }
}

/// Role of a single K8s node within a [`K8sCluster`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum K8sNodeRole {
    /// API server / scheduler / controller-manager.
    ControlPlane,
    /// kubelet worker.
    Worker,
}

/// One member of a provisioned K8s cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sNodeRecord {
    /// Whether this node hosts the control plane or a worker.
    pub role: K8sNodeRole,
    /// Underlying Celium VM slot id on the cluster's owner node.
    pub vm_id: u32,
    /// VM label as registered with the host.
    pub label: String,
    /// NIC allocated on the cluster network.
    pub nic: Nic,
}

/// Live handle to a provisioned K8s cluster. Owned by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sCluster {
    /// Spec used at creation time.
    pub spec: K8sClusterSpec,
    /// Node that owns the underlying Celium VMs and network.
    pub owner: NodeId,
    /// Cluster network on the owner node.
    pub network: VirtualNetwork,
    /// Front-end load balancer for the API server.
    pub lb: LoadBalancer,
    /// All control-plane + worker nodes, in declaration order.
    pub nodes: Vec<K8sNodeRecord>,
}

impl K8sCluster {
    /// Provision a new cluster on `owner` according to `spec`.
    ///
    /// Each step is an existing [`VmOp`] over the mesh; failures are
    /// surfaced verbatim. On error this routine does *not*
    /// automatically tear down partially-created resources — that's
    /// the caller's responsibility via [`Self::destroy`].
    ///
    /// # Errors
    /// Any underlying RPC error (capability denied, target unreachable,
    /// CIDR exhausted, etc.) is returned through [`CelError`].
    pub async fn create(
        mesh: &Mesh,
        owner: &NodeId,
        spec: K8sClusterSpec,
        rpc_timeout: Duration,
    ) -> CelResult<Self> {
        if spec.workers == 0 || spec.workers > 16 {
            return Err(CelError::Invalid("k8s: workers must be in 1..=16"));
        }
        let _block = Cidr4::parse(&spec.cidr).map_err(|_| CelError::Invalid("k8s: bad cidr"))?;

        // 1. Network.
        let network = match mesh.invoke(
            owner,
            VmOp::CreateNetwork { name: spec.name.clone(), cidr: spec.cidr.clone() },
            rpc_timeout,
        ).await? {
            VmOpReply::NetworkCreated { network } => network,
            _ => return Err(CelError::Internal("k8s: unexpected reply (CreateNetwork)")),
        };

        // 2. Control-plane VM + NIC.
        let cp_label = format!("k8s-cp-{}", spec.name);
        let cp_vm_id = match mesh.invoke(
            owner,
            VmOp::Create { label: cp_label.clone(), restart_policy: RestartPolicy::Always },
            rpc_timeout,
        ).await? {
            VmOpReply::Created { vm_id } => vm_id,
            _ => return Err(CelError::Internal("k8s: unexpected reply (Create cp)")),
        };
        let cp_nic = match mesh.invoke(
            owner,
            VmOp::AttachNic { network_id: network.id.clone(), vm_id: cp_vm_id, ip: None },
            rpc_timeout,
        ).await? {
            VmOpReply::NicAttached { nic } => nic,
            _ => return Err(CelError::Internal("k8s: unexpected reply (AttachNic cp)")),
        };
        let _ = mesh.invoke(owner, VmOp::Start { vm_id: cp_vm_id }, rpc_timeout).await?;

        let mut nodes = Vec::with_capacity(spec.workers as usize + 1);
        nodes.push(K8sNodeRecord {
            role: K8sNodeRole::ControlPlane,
            vm_id: cp_vm_id,
            label: cp_label,
            nic: cp_nic,
        });

        // 3. Worker VMs + NICs.
        let mut backends: Vec<LbBackend> = Vec::with_capacity(spec.workers as usize);
        for i in 0..spec.workers {
            let label = format!("k8s-worker-{}-{i}", spec.name);
            let vm_id = match mesh.invoke(
                owner,
                VmOp::Create { label: label.clone(), restart_policy: RestartPolicy::Always },
                rpc_timeout,
            ).await? {
                VmOpReply::Created { vm_id } => vm_id,
                _ => return Err(CelError::Internal("k8s: unexpected reply (Create worker)")),
            };
            let nic = match mesh.invoke(
                owner,
                VmOp::AttachNic { network_id: network.id.clone(), vm_id, ip: None },
                rpc_timeout,
            ).await? {
                VmOpReply::NicAttached { nic } => nic,
                _ => return Err(CelError::Internal("k8s: unexpected reply (AttachNic worker)")),
            };
            let _ = mesh.invoke(owner, VmOp::Start { vm_id }, rpc_timeout).await?;
            backends.push(LbBackend {
                vm_id, ip: nic.ip, port: spec.api_backend_port,
            });
            nodes.push(K8sNodeRecord {
                role: K8sNodeRole::Worker,
                vm_id, label, nic,
            });
        }

        // 4. Front-end LB. The VIP is the highest usable host
        // address in the network so it cannot collide with the
        // linearly-allocated NICs.
        let vip = network
            .cidr
            .nth_host(network.cidr.capacity())
            .ok_or(CelError::Invalid("k8s: no room for VIP in CIDR"))?;
        let lb = match mesh.invoke(
            owner,
            VmOp::CreateLoadBalancer {
                name: format!("{}-api", spec.name),
                network_id: network.id.clone(),
                vip: vip.to_string(),
                frontend_port: spec.api_port,
                algo: spec.algo,
                backends,
            },
            rpc_timeout,
        ).await? {
            VmOpReply::LoadBalancerCreated { lb } => lb,
            _ => return Err(CelError::Internal("k8s: unexpected reply (CreateLoadBalancer)")),
        };

        Ok(Self { spec, owner: owner.clone(), network, lb, nodes })
    }

    /// Tear down every resource provisioned by [`Self::create`]:
    /// LB, then each VM (Stop+Delete + NIC), then the network.
    ///
    /// Best-effort: every step is attempted even if a previous one
    /// failed. The first non-ok status is returned at the end.
    pub async fn destroy(&self, mesh: &Mesh, rpc_timeout: Duration) -> CelResult<()> {
        let mut first_err: Option<CelError> = None;

        if let Err(e) = mesh.invoke(
            &self.owner,
            VmOp::DeleteLoadBalancer { lb_id: self.lb.id.clone() },
            rpc_timeout,
        ).await {
            first_err.get_or_insert(e);
        }
        for n in &self.nodes {
            if let Err(e) = mesh.invoke(
                &self.owner,
                VmOp::DetachNic { nic_id: n.nic.id.clone() },
                rpc_timeout,
            ).await { first_err.get_or_insert(e); }
            let _ = mesh.invoke(
                &self.owner,
                VmOp::Stop { vm_id: n.vm_id },
                rpc_timeout,
            ).await;
            if let Err(e) = mesh.invoke(
                &self.owner,
                VmOp::Delete { vm_id: n.vm_id },
                rpc_timeout,
            ).await { first_err.get_or_insert(e); }
        }
        if let Err(e) = mesh.invoke(
            &self.owner,
            VmOp::DeleteNetwork { network_id: self.network.id.clone() },
            rpc_timeout,
        ).await { first_err.get_or_insert(e); }

        match first_err {
            None    => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// Convenience: return only the control-plane records.
    #[must_use]
    pub fn control_plane(&self) -> Vec<&K8sNodeRecord> {
        self.nodes.iter().filter(|n| n.role == K8sNodeRole::ControlPlane).collect()
    }

    /// Convenience: return only the worker records.
    #[must_use]
    pub fn workers(&self) -> Vec<&K8sNodeRecord> {
        self.nodes.iter().filter(|n| n.role == K8sNodeRole::Worker).collect()
    }
}
