//! In-memory VM controller mirroring `celhyper::manager`.
//!
//! The kernel-side manager runs only inside the bare-metal hypervisor.
//! On the host we maintain a parallel data model so that operators
//! can drive `celctl vm create / list / start / stop / state` against
//! a JSON state file before any RPC channel exists. When the Week-9+
//! IPC layer lands, this controller becomes a thin transport client;
//! the public surface here was chosen so that swap is mechanical.
//!
//! Path grammar matches the kernel: `/vms/<n>` where `<n>` is a
//! decimal `u32`.
//!
//! No `unwrap`/`panic` on production paths.

use std::path::{Path, PathBuf};

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

/// Maximum concurrent VMs — must match `celhyper::manager::MAX_VMS`.
pub const MAX_VMS: usize = 4;

/// Logical VM identifier; matches the kernel's `VmId(u32)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmId(pub u32);

/// Lifecycle state — must match `celhyper::vm::VmState` numerically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    Created,
    Running,
    Halted,
    Stopped,
    Faulted,
}

impl VmState {
    /// `true` once the VM cannot legally re-enter `Running`.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Halted | Self::Stopped | Self::Faulted)
    }

    /// Short stable label used by the CLI's text output and tests.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Halted  => "halted",
            Self::Stopped => "stopped",
            Self::Faulted => "faulted",
        }
    }
}

/// One row of the controller's table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRecord {
    /// Identifier (slot index).
    pub id: VmId,
    /// Current lifecycle state.
    pub state: VmState,
    /// Last basic exit reason recorded after `start`, if any.
    pub last_exit: Option<u32>,
    /// Tag attached at `create`. Free-form, ≤ 32 chars; pure metadata.
    pub label: String,
}

/// Persistent on-disk state. Versioned so older builds refuse mismatched
/// files instead of silently misinterpreting them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerState {
    /// Schema version. Bump on incompatible changes.
    pub version: u32,
    /// One slot per `MAX_VMS`; `None` means the slot is free.
    pub slots: [Option<VmRecord>; MAX_VMS],
}

impl Default for ControllerState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            slots: Default::default(),
        }
    }
}

/// Current state schema version.
pub const STATE_VERSION: u32 = 1;

/// In-memory controller. Construct via [`Controller::load`] or
/// [`Controller::in_memory`]; persist via [`Controller::save`].
pub struct Controller {
    state: ControllerState,
    path:  Option<PathBuf>,
}

impl Controller {
    /// Construct a fresh, non-persistent controller. Used by tests.
    #[must_use]
    pub fn in_memory() -> Self {
        Self { state: ControllerState::default(), path: None }
    }

    /// Load (or initialise) a controller backed by `path`. Missing
    /// files start empty; corrupt files surface as
    /// [`CelError::Invalid`].
    pub fn load(path: impl Into<PathBuf>) -> CelResult<Self> {
        let path = path.into();
        let state = if path.exists() {
            let raw = std::fs::read(&path)
                .map_err(|e| CelError::Io(format!("read {}: {e}", path.display())))?;
            let s: ControllerState = serde_json::from_slice(&raw)
                .map_err(|_| CelError::Invalid("controller state: malformed json"))?;
            if s.version != STATE_VERSION {
                return Err(CelError::Invalid("controller state: version mismatch"));
            }
            s
        } else {
            ControllerState::default()
        };
        Ok(Self { state, path: Some(path) })
    }

    /// Persist `self` if a path is bound. No-op for in-memory mode.
    pub fn save(&self) -> CelResult<()> {
        let Some(p) = &self.path else { return Ok(()); };
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CelError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        let raw = serde_json::to_vec_pretty(&self.state)
            .map_err(|_| CelError::Internal("controller state: serialise failed"))?;
        std::fs::write(p, raw)
            .map_err(|e| CelError::Io(format!("write {}: {e}", p.display())))?;
        Ok(())
    }

    // -- ops --------------------------------------------------------------

    /// Allocate a new VM slot. `label` is free-form metadata.
    pub fn create_vm(&mut self, label: impl Into<String>) -> CelResult<VmId> {
        let label = label.into();
        if label.len() > 32 {
            return Err(CelError::Invalid("label > 32 chars"));
        }
        for (i, slot) in self.state.slots.iter_mut().enumerate() {
            if slot.is_none() {
                let id = VmId(i as u32);
                *slot = Some(VmRecord {
                    id,
                    state: VmState::Created,
                    last_exit: None,
                    label,
                });
                return Ok(id);
            }
        }
        Err(CelError::Exhausted("vm registry full"))
    }

    /// Snapshot every allocated VM in id order.
    #[must_use]
    pub fn list_vms(&self) -> Vec<VmRecord> {
        self.state.slots.iter().flatten().cloned().collect()
    }

    /// Mark the VM `Running`. Models the kernel's `start_vm` from the
    /// dev box: a VM that was `Created` immediately moves through
    /// `Running` to a terminal state — `Halted` by default. The exit
    /// reason is set to `12` (HLT) to match the kernel's convention.
    pub fn start_vm(&mut self, id: VmId) -> CelResult<VmState> {
        let r = self.lookup_mut(id)?;
        if r.state.is_terminal() {
            return Err(CelError::Invalid("vm already terminal"));
        }
        if matches!(r.state, VmState::Running) {
            return Err(CelError::Invalid("vm already running"));
        }
        r.state = VmState::Halted;
        r.last_exit = Some(12);
        Ok(r.state)
    }

    /// Stop the VM. Idempotent on terminal states.
    pub fn stop_vm(&mut self, id: VmId) -> CelResult<VmState> {
        let r = self.lookup_mut(id)?;
        if !r.state.is_terminal() {
            r.state = VmState::Stopped;
        }
        Ok(r.state)
    }

    /// Inspect `id`.
    pub fn vm_state(&self, id: VmId) -> CelResult<VmState> {
        Ok(self.lookup(id)?.state)
    }

    /// Free a slot entirely. Allowed only on terminal VMs so live
    /// guests can't be silently leaked.
    pub fn delete_vm(&mut self, id: VmId) -> CelResult<()> {
        let i = self.slot_index(id)?;
        let slot = &mut self.state.slots[i];
        match slot {
            Some(r) if r.state.is_terminal() => *slot = None,
            Some(_) => return Err(CelError::Invalid("vm not terminal; stop first")),
            None    => return Err(CelError::Invalid("vm not allocated")),
        }
        Ok(())
    }

    /// Number of allocated slots.
    #[must_use]
    pub fn vm_count(&self) -> usize {
        self.state.slots.iter().flatten().count()
    }

    // -- path resolution --------------------------------------------------

    /// Resolve `/vms/<n>` to a [`VmId`]. Mirrors
    /// `celhyper::manager::resolve_path` exactly.
    pub fn resolve_path(&self, path: &str) -> CelResult<VmId> {
        let stripped = path.strip_prefix("/vms")
            .ok_or(CelError::Invalid("path: missing /vms root"))?;
        let suffix = stripped.strip_prefix('/')
            .ok_or(CelError::Invalid("path: expected /vms/<n>"))?;
        if suffix.is_empty() || suffix.contains('/') {
            return Err(CelError::Invalid("path: expected exactly one segment"));
        }
        let idx: u32 = suffix.parse()
            .map_err(|_| CelError::Invalid("path: VM id is not a u32"))?;
        let id = VmId(idx);
        let _ = self.lookup(id)?;
        Ok(id)
    }

    /// Render `id` as `/vms/<n>`.
    #[must_use]
    pub fn path_for(id: VmId) -> String {
        format!("/vms/{}", id.0)
    }

    // -- helpers ----------------------------------------------------------

    fn slot_index(&self, id: VmId) -> CelResult<usize> {
        let i = id.0 as usize;
        if i >= MAX_VMS {
            return Err(CelError::Invalid("VmId out of range"));
        }
        Ok(i)
    }

    fn lookup(&self, id: VmId) -> CelResult<&VmRecord> {
        let i = self.slot_index(id)?;
        self.state.slots[i].as_ref()
            .ok_or(CelError::Invalid("vm not allocated"))
    }

    fn lookup_mut(&mut self, id: VmId) -> CelResult<&mut VmRecord> {
        let i = self.slot_index(id)?;
        self.state.slots[i].as_mut()
            .ok_or(CelError::Invalid("vm not allocated"))
    }
}

/// Default state-file path inside the workspace's `build/` directory.
#[must_use]
pub fn default_state_path() -> PathBuf {
    Path::new("build").join("celctl-state.json")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_lists_and_resolves_path() {
        let mut c = Controller::in_memory();
        let a = c.create_vm("alpha").unwrap();
        let b = c.create_vm("beta").unwrap();
        assert_ne!(a, b);
        assert_eq!(c.vm_count(), 2);

        let list = c.list_vms();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].label, "alpha");
        assert_eq!(list[1].label, "beta");

        assert_eq!(c.resolve_path("/vms/0").unwrap(), a);
        assert_eq!(c.resolve_path("/vms/1").unwrap(), b);
        assert_eq!(Controller::path_for(a), "/vms/0");
    }

    #[test]
    fn registry_overflows_cleanly() {
        let mut c = Controller::in_memory();
        for _ in 0..MAX_VMS { c.create_vm("").unwrap(); }
        assert!(matches!(c.create_vm(""), Err(CelError::Exhausted(_))));
    }

    #[test]
    fn start_then_stop_lifecycle() {
        let mut c = Controller::in_memory();
        let id = c.create_vm("hello").unwrap();
        assert_eq!(c.vm_state(id).unwrap(), VmState::Created);
        c.start_vm(id).unwrap();
        assert_eq!(c.vm_state(id).unwrap(), VmState::Halted);
        // stop is idempotent on terminal states; state remains Halted.
        c.stop_vm(id).unwrap();
        assert_eq!(c.vm_state(id).unwrap(), VmState::Halted);
    }

    #[test]
    fn stop_before_start_yields_stopped() {
        let mut c = Controller::in_memory();
        let id = c.create_vm("").unwrap();
        c.stop_vm(id).unwrap();
        assert_eq!(c.vm_state(id).unwrap(), VmState::Stopped);
    }

    #[test]
    fn delete_requires_terminal_state() {
        let mut c = Controller::in_memory();
        let id = c.create_vm("").unwrap();
        assert!(matches!(c.delete_vm(id), Err(CelError::Invalid(_))));
        c.stop_vm(id).unwrap();
        c.delete_vm(id).unwrap();
        assert_eq!(c.vm_count(), 0);
        assert!(matches!(c.delete_vm(id), Err(CelError::Invalid(_))));
    }

    #[test]
    fn resolve_path_rejects_malformed_input() {
        let c = Controller::in_memory();
        for bad in ["", "/", "/vms", "/vms/", "/vm/0", "/vms/abc", "/vms/0/", "/vms/0/extra"] {
            assert!(c.resolve_path(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn resolve_path_denies_unallocated_id() {
        let mut c = Controller::in_memory();
        let _ = c.create_vm("").unwrap();
        // /vms/3 is syntactically valid but unallocated.
        assert!(c.resolve_path("/vms/3").is_err());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("celctl-{}", std::process::id()));
        let path = dir.join("state.json");
        let _ = std::fs::remove_dir_all(&dir);

        {
            let mut c = Controller::load(&path).unwrap();
            c.create_vm("alpha").unwrap();
            c.create_vm("beta").unwrap();
            let id = c.resolve_path("/vms/1").unwrap();
            c.start_vm(id).unwrap();
            c.save().unwrap();
        }

        let c2 = Controller::load(&path).unwrap();
        assert_eq!(c2.vm_count(), 2);
        assert_eq!(c2.list_vms()[1].state, VmState::Halted);
        assert_eq!(c2.list_vms()[1].label, "beta");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
