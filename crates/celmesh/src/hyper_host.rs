//! W22 — host-side bridge to CelHyper.
//!
//! # Why this module exists
//!
//! `celhyper` is a bare-metal `no_std` kernel built for the
//! `x86_64-unknown-none` target. It cannot link into `celcli` or
//! `celmesh` (which are `std` host crates), and the `vmlaunch`
//! instruction it ultimately issues requires CPL 0 — userspace can
//! never call it directly. Therefore "the control plane drives real
//! CelHyper" is necessarily a *bridge*: the host process speaks a
//! wire-stable IPC to a running CelHyper kernel (bare metal or under
//! QEMU), and the kernel dispatches each request to its
//! `manager::{create_vm, start_vm, stop_vm, delete_vm}` API.
//!
//! W22 ships three pieces of that bridge:
//!
//! 1. [`HyperLink`] — the transport-agnostic async trait every backend
//!    implements. The trait surface is intentionally a minimal slice of
//!    `VmOp` covering only VM lifecycle; volume / network / snapshot
//!    ops continue to flow through the in-process [`crate::MemVmHost`]
//!    until CelVault gets its own kernel-side bridge.
//! 2. [`LoopbackHyperLink`] — a pure-Rust state machine that mirrors
//!    the celhyper-kernel `manager` semantics (`MAX_VMS = 4`,
//!    `Created → Running → Halted`, exit code `12` from the canned
//!    `HELLO_BLOB`'s `hlt`). It lets the bridge run **end to end in
//!    CI** on Linux/Windows without QEMU, and it is the reference
//!    implementation that every future transport must match
//!    bit-for-bit.
//! 3. [`CelhyperVmHost`] — implements [`crate::host::VmHost`] by
//!    routing VM-lifecycle ops to a [`HyperLink`] and **delegating
//!    every other op** (volumes, networks, snapshots, security
//!    groups, load balancers) to a contained [`crate::MemVmHost`].
//!    This composition keeps the diff small and reversible while we
//!    burn in the wire shape.
//!
//! # Wire shape (frozen for W22)
//!
//! See [`wire`] for the request / reply enums. They are
//! `serde_json`-encodable today; W22-B will hand-encode the same
//! shape inside the kernel without pulling serde into `no_std`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use celcommon::{CelError, CelResult};

use crate::capabilities::Capabilities;
use crate::federation::{RemoteVm, RestartPolicy};
use crate::host::{HostFut, HostResult, MemVmHost, VmHost};
use crate::membership::NodeId;
use crate::proto::{VmOp, VmOpReply};

/// MAX VMs per CelHyper kernel instance. Must agree with the kernel
/// constant `celhyper::manager::MAX_VMS`. Bumping this value requires
/// changing both ends in lockstep.
pub const HYPER_MAX_VMS: usize = 4;

/// HLT exit code reported by the canned `HELLO_BLOB` guest. The
/// bare-metal kernel returns `12` (the value the test guest sets in
/// `eax` before its `hlt`). `MemVmHost` already uses this same value
/// for its in-memory model, so the wire shape stays uniform.
pub const HYPER_HLT_EXIT_CODE: u32 = 12;

/// Wire types. Public so future kernel-side decoders can pull in the
/// shape via `celcommon` re-export (W22-B will move them there once
/// the no_std encoder is in place).
pub mod wire {
    use serde::{Deserialize, Serialize};

    /// Bridge protocol version. Bump on any incompatible change.
    pub const HYPER_IPC_VERSION: u32 = 1;

    /// Magic prefix used by framed (line-based) transports so a
    /// human watching the serial console knows what they're reading.
    pub const HYPER_IPC_MAGIC: &str = "celhyper-ipc/1";

    /// One bridge call.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum HyperRequest {
        /// Allocate a VM slot with `label`. The kernel does not
        /// itself store the label; the host adapter mirrors it for
        /// gossip purposes. Returns [`HyperReply::Created`].
        Create {
            /// Free-form label, ≤ 32 chars. Validated by the host
            /// adapter before send so the kernel never has to.
            label: String,
        },
        /// `vmlaunch` slot `vm_id`. The kernel returns once the guest
        /// has exited (HLT in the canned-guest path). Returns
        /// [`HyperReply::State`] with `state = "halted"` and
        /// `last_exit = HYPER_HLT_EXIT_CODE`.
        Start {
            /// Slot id, 0 ≤ vm_id < `HYPER_MAX_VMS`.
            vm_id: u32,
        },
        /// Force a terminal slot to `Stopped`. Idempotent on already-
        /// terminal slots.
        Stop {
            /// Slot id.
            vm_id: u32,
        },
        /// Free the slot. Only valid on terminal VMs. Returns
        /// [`HyperReply::Deleted`].
        Delete {
            /// Slot id.
            vm_id: u32,
        },
        /// Snapshot every slot the kernel currently knows.
        List,
    }

    /// One bridge reply.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "reply", rename_all = "snake_case")]
    pub enum HyperReply {
        /// Returned for [`HyperRequest::Create`].
        Created {
            /// Newly-assigned slot id.
            vm_id: u32,
        },
        /// Returned for [`HyperRequest::Start`] and
        /// [`HyperRequest::Stop`].
        State {
            /// Slot id whose state changed.
            vm_id: u32,
            /// New state tag (`"halted"`, `"running"`, `"stopped"`,
            /// `"faulted"`).
            state: String,
            /// Guest exit code if the slot has terminated.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            last_exit: Option<u32>,
        },
        /// Returned for [`HyperRequest::Delete`].
        Deleted {
            /// Slot id that was freed.
            vm_id: u32,
        },
        /// Returned for [`HyperRequest::List`].
        Listed {
            /// Every slot the kernel currently holds.
            rows: Vec<HyperVmRow>,
        },
    }

    /// One row in [`HyperReply::Listed`].
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct HyperVmRow {
        /// Slot id.
        pub vm_id: u32,
        /// Free-form label (forwarded from `Create`).
        pub label: String,
        /// State tag.
        pub state: String,
        /// Last guest exit code, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub last_exit: Option<u32>,
    }
}

pub use wire::{HyperReply, HyperRequest, HyperVmRow};

/// Transport-agnostic CelHyper bridge.
///
/// Every backend (loopback, QEMU serial, real serial, vsock) ships a
/// single `call` method that turns one [`HyperRequest`] into one
/// [`HyperReply`]. The trait is async-shaped without `async-trait` so
/// it composes cleanly with the rest of `celmesh`.
pub trait HyperLink: Send + Sync {
    /// Issue one bridge call. Implementations must not panic and must
    /// surface every error as `Err(CelError)`.
    fn call<'a>(
        &'a self,
        req: HyperRequest,
    ) -> Pin<Box<dyn Future<Output = CelResult<HyperReply>> + Send + 'a>>;
}

// ---------------------------------------------------------------------------
// LoopbackHyperLink — in-process kernel sim.
// ---------------------------------------------------------------------------

/// In-process simulation of the celhyper kernel's `manager` state
/// machine. Carries `HYPER_MAX_VMS` slots; each slot has a label and
/// a state tag. State transitions:
///
/// * `Create`  : `_` → `Created`
/// * `Start`   : `Created` → `Halted` (last_exit = `HYPER_HLT_EXIT_CODE`)
/// * `Stop`    : `Created`/`Halted`/`Faulted` → `Stopped`
/// * `Delete`  : terminal → `_`
///
/// `Start` of an already-terminal slot returns an error so the
/// caller can distinguish "you already ran this VM" from a kernel
/// bug. This matches the kernel-side `manager::start_vm` behaviour.
pub struct LoopbackHyperLink {
    slots: Mutex<[Option<LoopSlot>; HYPER_MAX_VMS]>,
}

#[derive(Debug, Clone)]
struct LoopSlot {
    label: String,
    state: &'static str,
    last_exit: Option<u32>,
}

impl Default for LoopbackHyperLink {
    fn default() -> Self {
        Self { slots: Mutex::new(Default::default()) }
    }
}

impl LoopbackHyperLink {
    /// Build a fresh, empty kernel sim.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Apply one request synchronously. Exposed for tests that want
    /// to drive the sim without going through the async `HyperLink`.
    pub fn apply(&self, req: HyperRequest) -> CelResult<HyperReply> {
        let mut slots = lock_or_recover(&self.slots);
        match req {
            HyperRequest::Create { label } => {
                if label.len() > 32 {
                    return Err(CelError::Invalid("hyper: label > 32 chars"));
                }
                for (i, s) in slots.iter_mut().enumerate() {
                    if s.is_none() {
                        *s = Some(LoopSlot {
                            label,
                            state: "created",
                            last_exit: None,
                        });
                        // SAFETY-comment scope only: `i` is bounded by
                        // HYPER_MAX_VMS ≤ u32::MAX, so the cast is
                        // value-preserving.
                        return Ok(HyperReply::Created { vm_id: i as u32 });
                    }
                }
                Err(CelError::Invalid("hyper: vm registry full"))
            }
            HyperRequest::Start { vm_id } => {
                let s = slot_mut(&mut slots, vm_id)?;
                match s.state {
                    "halted" | "stopped" | "faulted" => {
                        Err(CelError::Invalid("hyper: vm already terminal"))
                    }
                    "running" => Err(CelError::Invalid("hyper: vm already running")),
                    _ => {
                        s.state = "halted";
                        s.last_exit = Some(HYPER_HLT_EXIT_CODE);
                        Ok(HyperReply::State {
                            vm_id,
                            state: s.state.to_string(),
                            last_exit: s.last_exit,
                        })
                    }
                }
            }
            HyperRequest::Stop { vm_id } => {
                let s = slot_mut(&mut slots, vm_id)?;
                if !matches!(s.state, "halted" | "stopped" | "faulted") {
                    s.state = "stopped";
                }
                Ok(HyperReply::State {
                    vm_id,
                    state: s.state.to_string(),
                    last_exit: s.last_exit,
                })
            }
            HyperRequest::Delete { vm_id } => {
                let i = vm_id as usize;
                if i >= HYPER_MAX_VMS {
                    return Err(CelError::Invalid("hyper: vm id out of range"));
                }
                let s = slots[i]
                    .as_ref()
                    .ok_or(CelError::Invalid("hyper: vm not allocated"))?;
                if !matches!(s.state, "halted" | "stopped" | "faulted") {
                    return Err(CelError::Invalid("hyper: vm not terminal"));
                }
                slots[i] = None;
                Ok(HyperReply::Deleted { vm_id })
            }
            HyperRequest::List => {
                let mut rows = Vec::with_capacity(HYPER_MAX_VMS);
                for (i, s) in slots.iter().enumerate() {
                    if let Some(s) = s {
                        rows.push(HyperVmRow {
                            vm_id: i as u32,
                            label: s.label.clone(),
                            state: s.state.to_string(),
                            last_exit: s.last_exit,
                        });
                    }
                }
                Ok(HyperReply::Listed { rows })
            }
        }
    }
}

impl HyperLink for LoopbackHyperLink {
    fn call<'a>(
        &'a self,
        req: HyperRequest,
    ) -> Pin<Box<dyn Future<Output = CelResult<HyperReply>> + Send + 'a>> {
        Box::pin(async move {
            // Round-trip through serde to make sure the wire shape
            // really is stable. Catches any accidental enum-variant
            // change at the loopback layer instead of in production.
            let encoded = serde_json::to_string(&req)
                .map_err(|e| CelError::Io(format!("hyper encode: {e}")))?;
            let decoded: HyperRequest = serde_json::from_str(&encoded)
                .map_err(|e| CelError::Io(format!("hyper decode: {e}")))?;
            let reply = self.apply(decoded)?;
            let encoded = serde_json::to_string(&reply)
                .map_err(|e| CelError::Io(format!("hyper reply encode: {e}")))?;
            let decoded: HyperReply = serde_json::from_str(&encoded)
                .map_err(|e| CelError::Io(format!("hyper reply decode: {e}")))?;
            Ok(decoded)
        })
    }
}

fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // No `unsafe`; same panic-recovery pattern as `MemVmHost::lock_slots`.
    match m.lock() {
        Ok(g)  => g,
        Err(p) => p.into_inner(),
    }
}

fn slot_mut(
    slots: &mut [Option<LoopSlot>; HYPER_MAX_VMS],
    vm_id: u32,
) -> CelResult<&mut LoopSlot> {
    let i = vm_id as usize;
    if i >= HYPER_MAX_VMS {
        return Err(CelError::Invalid("hyper: vm id out of range"));
    }
    slots[i]
        .as_mut()
        .ok_or(CelError::Invalid("hyper: vm not allocated"))
}

// ---------------------------------------------------------------------------
// CelhyperVmHost — the VmHost adapter.
// ---------------------------------------------------------------------------

/// `VmHost` implementation backed by a [`HyperLink`].
///
/// Routing rules:
///
/// * `Create`, `Start`, `Stop`, `Delete`, `List` → the kernel via
///   `link`. The label table for these slots is mirrored locally so
///   `snapshot` can emit `RemoteVm` rows with the correct `label`
///   field (the kernel itself does not store labels — keeping the
///   kernel's struct surface tiny is a deliberate W22 goal).
/// * Everything else (volumes, networks, snapshots, security groups,
///   load balancers) → the contained [`MemVmHost`] until CelVault's
///   own kernel-side bridge exists.
pub struct CelhyperVmHost {
    link: Arc<dyn HyperLink>,
    fallback: MemVmHost,
    /// Slot-id → label mirror. Authoritative on the host side.
    labels: Mutex<BTreeMap<u32, String>>,
    /// Slot-id → restart policy mirror. Used so `snapshot` can emit
    /// the correct policy in each `RemoteVm` row.
    policies: Mutex<BTreeMap<u32, RestartPolicy>>,
}

impl CelhyperVmHost {
    /// Build a host bound to `link`. The fallback is an empty
    /// [`MemVmHost`] with `Capabilities::ALL` granted.
    #[must_use]
    pub fn new(link: Arc<dyn HyperLink>) -> Self {
        Self {
            link,
            fallback: MemVmHost::new(),
            labels: Mutex::new(BTreeMap::new()),
            policies: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replace the capability set on the contained fallback (volumes,
    /// networks, …). Returns `self` so it composes with `new`.
    #[must_use]
    pub fn with_caps(mut self, caps: Capabilities) -> Self {
        self.fallback = MemVmHost::new().with_caps(caps);
        self
    }

    /// Borrow the underlying link. Useful for tests that want to
    /// poke the kernel directly without going through `VmHost`.
    #[must_use]
    pub fn link(&self) -> Arc<dyn HyperLink> { self.link.clone() }

    fn remember(&self, vm_id: u32, label: String, policy: RestartPolicy) {
        if let Ok(mut g) = self.labels.lock()   { g.insert(vm_id, label); }
        if let Ok(mut g) = self.policies.lock() { g.insert(vm_id, policy); }
    }

    fn forget(&self, vm_id: u32) {
        if let Ok(mut g) = self.labels.lock()   { g.remove(&vm_id); }
        if let Ok(mut g) = self.policies.lock() { g.remove(&vm_id); }
    }

    fn label_of(&self, vm_id: u32) -> String {
        self.labels.lock().ok()
            .and_then(|g| g.get(&vm_id).cloned())
            .unwrap_or_default()
    }

    fn policy_of(&self, vm_id: u32) -> RestartPolicy {
        self.policies.lock().ok()
            .and_then(|g| g.get(&vm_id).copied())
            .unwrap_or_default()
    }
}

impl VmHost for CelhyperVmHost {
    fn handle<'a>(&'a self, op: VmOp) -> HostFut<'a, HostResult> {
        Box::pin(async move {
            match op {
                VmOp::Create { label, restart_policy } => {
                    let label_for_mirror = label.clone();
                    let reply = self
                        .link
                        .call(HyperRequest::Create { label })
                        .await
                        .map_err(|e| format!("hyper: {e:?}"))?;
                    let HyperReply::Created { vm_id } = reply else {
                        return Err(format!("hyper: unexpected reply {reply:?}"));
                    };
                    self.remember(vm_id, label_for_mirror, restart_policy);
                    Ok(VmOpReply::Created { vm_id })
                }
                VmOp::Start { vm_id } => {
                    let reply = self
                        .link
                        .call(HyperRequest::Start { vm_id })
                        .await
                        .map_err(|e| format!("hyper: {e:?}"))?;
                    let HyperReply::State { vm_id, state, .. } = reply else {
                        return Err(format!("hyper: unexpected reply {reply:?}"));
                    };
                    Ok(VmOpReply::State { vm_id, state })
                }
                VmOp::Stop { vm_id } => {
                    let reply = self
                        .link
                        .call(HyperRequest::Stop { vm_id })
                        .await
                        .map_err(|e| format!("hyper: {e:?}"))?;
                    let HyperReply::State { vm_id, state, .. } = reply else {
                        return Err(format!("hyper: unexpected reply {reply:?}"));
                    };
                    Ok(VmOpReply::State { vm_id, state })
                }
                VmOp::Delete { vm_id } => {
                    let reply = self
                        .link
                        .call(HyperRequest::Delete { vm_id })
                        .await
                        .map_err(|e| format!("hyper: {e:?}"))?;
                    let HyperReply::Deleted { vm_id } = reply else {
                        return Err(format!("hyper: unexpected reply {reply:?}"));
                    };
                    self.forget(vm_id);
                    Ok(VmOpReply::Deleted { vm_id })
                }
                VmOp::List => {
                    // List replies are reconstructed in `snapshot`.
                    // The wire `Listed` carries `RemoteVm` rows which
                    // need the owner id — see snapshot() below.
                    Ok(VmOpReply::Listed { rows: Vec::new() })
                }
                // Everything else flows through MemVmHost so volume,
                // network, snapshot, security-group and LB ops keep
                // working unchanged.
                other => self.fallback.handle(other).await,
            }
        })
    }

    fn snapshot<'a>(&'a self, owner: &'a NodeId) -> HostFut<'a, Vec<RemoteVm>> {
        Box::pin(async move {
            let reply = match self.link.call(HyperRequest::List).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(target: "celmesh::hyper_host",
                                   error = ?e, "snapshot: list failed");
                    return Vec::new();
                }
            };
            let rows = match reply {
                HyperReply::Listed { rows } => rows,
                other => {
                    tracing::warn!(target: "celmesh::hyper_host",
                                   reply = ?other,
                                   "snapshot: unexpected reply");
                    return Vec::new();
                }
            };
            rows.into_iter()
                .map(|r| RemoteVm {
                    owner: owner.clone(),
                    vm_id: r.vm_id,
                    label: if r.label.is_empty() { self.label_of(r.vm_id) } else { r.label },
                    state: r.state,
                    last_exit: r.last_exit,
                    restart_policy: self.policy_of(r.vm_id),
                    owner_alive: true,
                    epoch: 0,
                    hlc: 0,
                    volumes: Vec::new(),
                    image_path: None,
                    cpu_count: None,
                    memory_mib: None,
                    boot_blob_crc32c: None,
                })
                .collect()
        })
    }

    fn attach_preserved<'a>(
        &'a self,
        vm_id: u32,
        attachments: Vec<celvault::VolumeAttachment>,
    ) -> HostFut<'a, Result<(), String>> {
        // Volumes still live in the fallback; mirror the call.
        self.fallback.attach_preserved(vm_id, attachments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> Arc<dyn HyperLink> {
        Arc::new(LoopbackHyperLink::new())
    }

    #[tokio::test]
    async fn loopback_round_trips_create_start_stop_delete() {
        let host = CelhyperVmHost::new(link());
        let owner = NodeId("n1".into());

        // create
        let r = host
            .handle(VmOp::Create { label: "guest-a".into(), restart_policy: RestartPolicy::Never })
            .await
            .unwrap();
        let VmOpReply::Created { vm_id } = r else { panic!("create reply") };
        assert_eq!(vm_id, 0);

        // snapshot reflects create
        let snap = host.snapshot(&owner).await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].label, "guest-a");
        assert_eq!(snap[0].state, "created");

        // start → halted with HLT exit
        let r = host.handle(VmOp::Start { vm_id }).await.unwrap();
        let VmOpReply::State { state, .. } = r else { panic!("start reply") };
        assert_eq!(state, "halted");

        // stop is idempotent on terminal but still moves to stopped
        let r = host.handle(VmOp::Stop { vm_id }).await.unwrap();
        let VmOpReply::State { state, .. } = r else { panic!("stop reply") };
        // halted is terminal so the loopback keeps it on halted.
        assert_eq!(state, "halted");

        // delete frees the slot
        let r = host.handle(VmOp::Delete { vm_id }).await.unwrap();
        assert!(matches!(r, VmOpReply::Deleted { vm_id: 0 }));
        let snap = host.snapshot(&owner).await;
        assert!(snap.is_empty(), "snapshot after delete: {snap:?}");
    }

    #[tokio::test]
    async fn loopback_rejects_oversized_label() {
        let host = CelhyperVmHost::new(link());
        let big = "x".repeat(33);
        let err = host
            .handle(VmOp::Create { label: big, restart_policy: RestartPolicy::Never })
            .await
            .expect_err("oversized label must be rejected");
        assert!(err.contains("32 chars"), "err={err}");
    }

    #[tokio::test]
    async fn loopback_runs_out_of_slots_after_max_vms() {
        let host = CelhyperVmHost::new(link());
        for i in 0..HYPER_MAX_VMS {
            let r = host
                .handle(VmOp::Create { label: format!("g{i}"), restart_policy: RestartPolicy::Never })
                .await
                .unwrap();
            assert!(matches!(r, VmOpReply::Created { .. }));
        }
        let err = host
            .handle(VmOp::Create { label: "overflow".into(), restart_policy: RestartPolicy::Never })
            .await
            .expect_err("registry must be full");
        assert!(err.contains("registry full"), "err={err}");
    }

    #[tokio::test]
    async fn loopback_start_on_terminal_is_an_error() {
        let host = CelhyperVmHost::new(link());
        let _ = host
            .handle(VmOp::Create { label: "g".into(), restart_policy: RestartPolicy::Never })
            .await
            .unwrap();
        let _ = host.handle(VmOp::Start { vm_id: 0 }).await.unwrap();
        let err = host
            .handle(VmOp::Start { vm_id: 0 })
            .await
            .expect_err("start-on-terminal must error");
        assert!(err.contains("terminal"), "err={err}");
    }

    #[tokio::test]
    async fn wire_round_trip_through_serde_is_lossless() {
        // The LoopbackHyperLink itself round-trips through serde on
        // every call; this test pins the wire shape explicitly so
        // the kernel-side decoder (W22-B) has a contract to match.
        let req = HyperRequest::Create { label: "rt".into() };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains(r#""op":"create""#), "encoded: {s}");
        let back: HyperRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back, req);

        let reply = HyperReply::State {
            vm_id: 0,
            state: "halted".into(),
            last_exit: Some(HYPER_HLT_EXIT_CODE),
        };
        let s = serde_json::to_string(&reply).unwrap();
        assert!(s.contains(r#""reply":"state""#), "encoded: {s}");
        let back: HyperReply = serde_json::from_str(&s).unwrap();
        assert_eq!(back, reply);
    }

    #[tokio::test]
    async fn fallback_handles_non_lifecycle_ops() {
        // CreateVolume is a fallback op — it must not be sent to the
        // hyper link and must succeed against the embedded MemVmHost.
        let host = CelhyperVmHost::new(link());
        let owner = NodeId("nA".into());
        // snapshot first so MemVmHost remembers the owner id.
        let _ = host.snapshot(&owner).await;
        // Need to seed the fallback's owner; we do it indirectly via
        // a fallback op (`MemVmHost::snapshot` is what remembers it
        // when called against the fallback). Easiest path: call the
        // fallback's snapshot via the public adapter.
        let _ = host.fallback.snapshot(&owner).await;
        let r = host
            .handle(VmOp::CreateVolume { name: "v1".into(), size_bytes: 64 * 1024 })
            .await
            .unwrap();
        assert!(matches!(r, VmOpReply::VolumeCreated { .. }));
    }
}
