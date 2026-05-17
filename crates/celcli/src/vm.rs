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
//! W18.3 adds an optional **boot-blob staging** side-channel: when a
//! controller is bound to a stage root (see
//! [`Controller::with_stage_root`]) and a VM was created with an
//! `image_path`, [`Controller::start_vm`] reads the first page out of
//! that image via [`crate::boot::stage_boot_blob`] and records the
//! resulting digest on the `VmRecord`. The staged file is what the
//! supervisor (or, eventually, an RPC client of `celhyper`) hands off
//! to the hypervisor.
//!
//! No `unwrap`/`panic` on production paths.

use std::path::{Path, PathBuf};

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

use crate::boot;

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
    /// W18: optional path to a backing disk image (raw / qcow2 /
    /// vmdk / vhdx). Stored as-is so the supervisor can re-inspect
    /// it on restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
    /// W18: requested vCPU count. `None` means "use default
    /// (1 vCPU)".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,
    /// W18: requested guest RAM in MiB. `None` means "use default
    /// (256 MiB)".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    /// W18.3: number of bytes staged out of `image_path` at the most
    /// recent successful `start_vm`. `None` if the VM was never
    /// started with a stage-root-bound controller, or if it was
    /// created without an `image_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_blob_len: Option<u64>,
    /// W18.3: CRC-32C of the staged bytes. Pairs with
    /// [`Self::boot_blob_len`] to detect a silently swapped backing
    /// image across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_blob_crc32c: Option<u32>,
}

/// Resources requested for a new VM. Pass to [`Controller::create_vm_with`].
#[derive(Debug, Clone, Default)]
pub struct VmSpec {
    /// Free-form metadata; same constraints as [`Controller::create_vm`].
    pub label: String,
    /// Optional path to a backing disk image.
    pub image_path: Option<String>,
    /// Optional vCPU count.
    pub cpu_count: Option<u32>,
    /// Optional guest RAM in MiB.
    pub memory_mib: Option<u64>,
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

/// W20: aggregate counters surfaced by [`Controller::stats`]. All
/// fields are plain `usize` so the struct stays `Copy` and trivially
/// serialisable for future JSON/metrics surfaces.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerStats {
    /// Total slot capacity (== [`MAX_VMS`]).
    pub slots_total: usize,
    /// Slots currently free.
    pub slots_free: usize,
    /// Slots currently allocated to a VM (== `slots_total - slots_free`).
    pub allocated: usize,
    /// Allocated VMs in `Created`.
    pub created: usize,
    /// Allocated VMs in `Running`.
    pub running: usize,
    /// Allocated VMs in `Halted`.
    pub halted: usize,
    /// Allocated VMs in `Stopped`.
    pub stopped: usize,
    /// Allocated VMs in `Faulted`.
    pub faulted: usize,
    /// Allocated VMs that carry a recorded `boot_blob_crc32c`.
    /// Operator-visible signal that the W18.3 staging side-channel
    /// has executed at least once for that slot.
    pub with_boot_blob: usize,
}

/// In-memory controller. Construct via [`Controller::load`] or
/// [`Controller::in_memory`]; persist via [`Controller::save`].
pub struct Controller {
    state: ControllerState,
    path:  Option<PathBuf>,
    /// W18.3: directory under which `start_vm` stages boot blobs.
    /// `None` disables staging entirely (the VM still transitions
    /// through `Running → Halted` exactly as before).
    stage_root: Option<PathBuf>,
}

impl Controller {
    /// Construct a fresh, non-persistent controller. Used by tests.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            state: ControllerState::default(),
            path: None,
            stage_root: None,
        }
    }

    /// Bind a stage root for boot-blob staging. Builder-style; pairs
    /// with [`Self::load`] in the CLI hot path.
    #[must_use]
    pub fn with_stage_root(mut self, p: impl Into<PathBuf>) -> Self {
        self.stage_root = Some(p.into());
        self
    }

    /// Return the currently bound stage root, if any.
    #[must_use]
    pub fn stage_root(&self) -> Option<&Path> {
        self.stage_root.as_deref()
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
        Ok(Self { state, path: Some(path), stage_root: None })
    }

    /// Persist `self` if a path is bound. No-op for in-memory mode.
    ///
    /// W19 Phase B: crash-safe. Writes to `<path>.tmp`, fsyncs the
    /// file, then atomically renames over `<path>`. A crash between
    /// `create_dir_all` and the final `rename` either leaves the
    /// previous good state file untouched or no state file at all —
    /// never a half-written `path`. `fs::rename` is atomic over an
    /// existing destination on every supported platform (POSIX
    /// `rename(2)`; Windows `MoveFileEx(MOVEFILE_REPLACE_EXISTING)`).
    pub fn save(&self) -> CelResult<()> {
        let Some(p) = &self.path else { return Ok(()); };
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CelError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        let raw = serde_json::to_vec_pretty(&self.state)
            .map_err(|_| CelError::Internal("controller state: serialise failed"))?;

        // Use a distinctive suffix that won't collide with operator
        // edits. Per-pid avoids parallel-test interference when two
        // controllers point at the same state file in the same dir.
        let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| CelError::Io(format!("create {}: {e}", tmp.display())))?;
            f.write_all(&raw)
                .map_err(|e| CelError::Io(format!("write {}: {e}", tmp.display())))?;
            f.sync_all()
                .map_err(|e| CelError::Io(format!("fsync {}: {e}", tmp.display())))?;
        } // close before rename — required on Windows.
        std::fs::rename(&tmp, p).map_err(|e| {
            // Clean up the tmp file on failure so we don't leak.
            let _ = std::fs::remove_file(&tmp);
            CelError::Io(format!("rename {} -> {}: {e}", tmp.display(), p.display()))
        })?;
        Ok(())
    }

    // -- ops --------------------------------------------------------------

    /// Allocate a new VM slot. `label` is free-form metadata.
    pub fn create_vm(&mut self, label: impl Into<String>) -> CelResult<VmId> {
        self.create_vm_with(VmSpec { label: label.into(), ..Default::default() })
    }

    /// Allocate a new VM slot with a full [`VmSpec`]. `image_path`,
    /// `cpu_count` and `memory_mib` are recorded verbatim so the
    /// supervisor can re-apply them on restart; `celhyper` consumption
    /// of these fields is gated on the W18.3 milestone and not yet
    /// wired here.
    pub fn create_vm_with(&mut self, spec: VmSpec) -> CelResult<VmId> {
        let VmSpec { label, image_path, cpu_count, memory_mib } = spec;
        if label.len() > 32 {
            return Err(CelError::Invalid("label > 32 chars"));
        }
        if let Some(c) = cpu_count {
            if c == 0 || c > 64 {
                return Err(CelError::Invalid("cpu_count must be in 1..=64"));
            }
        }
        if let Some(m) = memory_mib {
            if m == 0 || m > 1024 * 1024 {
                return Err(CelError::Invalid("memory_mib must be in 1..=1048576"));
            }
        }
        for (i, slot) in self.state.slots.iter_mut().enumerate() {
            if slot.is_none() {
                let id = VmId(i as u32);
                *slot = Some(VmRecord {
                    id,
                    state: VmState::Created,
                    last_exit: None,
                    label,
                    image_path,
                    cpu_count,
                    memory_mib,
                    boot_blob_len: None,
                    boot_blob_crc32c: None,
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

    /// W20: lightweight aggregate counters over the slot table.
    /// Operator-visible via `celctl vm stats`. Cheap: a single pass
    /// over `MAX_VMS` slots, no clones.
    #[must_use]
    pub fn stats(&self) -> ControllerStats {
        let mut s = ControllerStats { slots_total: MAX_VMS, ..ControllerStats::default() };
        for slot in self.state.slots.iter() {
            match slot {
                None => s.slots_free += 1,
                Some(r) => {
                    s.allocated += 1;
                    match r.state {
                        VmState::Created => s.created += 1,
                        VmState::Running => s.running += 1,
                        VmState::Halted  => s.halted  += 1,
                        VmState::Stopped => s.stopped += 1,
                        VmState::Faulted => s.faulted += 1,
                    }
                    if r.boot_blob_crc32c.is_some() { s.with_boot_blob += 1; }
                }
            }
        }
        s
    }

    /// Mark the VM `Running`. Models the kernel's `start_vm` from the
    /// dev box: a VM that was `Created` immediately moves through
    /// `Running` to a terminal state — `Halted` by default. The exit
    /// reason is set to `12` (HLT) to match the kernel's convention.
    ///
    /// W18.3: if the VM was created with an `image_path` *and* this
    /// controller is bound to a stage root (see
    /// [`Self::with_stage_root`]), the first page of that image is
    /// staged via [`crate::boot::stage_boot_blob`] before the state
    /// transition. Any staging error aborts the start; the VM stays
    /// in `Created` so the operator can retry after fixing the image.
    ///
    /// W19: if the record already carries a `boot_blob_crc32c` from
    /// a prior start (e.g. across `reset_vm` + `start_vm` or across
    /// a controller restart), the *newly* staged digest is compared
    /// against it. A mismatch aborts the start with
    /// [`CelError::Invalid`] — the backing image content has
    /// changed under us and the operator must explicitly acknowledge
    /// (via `delete_vm` + `create_vm_with`).
    pub fn start_vm(&mut self, id: VmId) -> CelResult<VmState> {
        // Pre-compute staging inputs *without* holding a mutable
        // borrow into `self.state` so we can also borrow `self.stage_root`.
        let (image_path, stage_root, vm_id_raw, prior_crc) = {
            let r = self.lookup(id)?;
            if r.state.is_terminal() {
                return Err(CelError::Invalid("vm already terminal"));
            }
            if matches!(r.state, VmState::Running) {
                return Err(CelError::Invalid("vm already running"));
            }
            (
                r.image_path.clone(),
                self.stage_root.clone(),
                r.id.0,
                r.boot_blob_crc32c,
            )
        };

        let digest = match (image_path, stage_root) {
            (Some(img), Some(root)) => {
                let path = Path::new(&img).to_path_buf();
                match boot::stage_boot_blob(&path, &root, vm_id_raw) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        tracing::warn!(
                            vm_id = vm_id_raw,
                            image = %path.display(),
                            error = %e,
                            "vm start aborted: boot blob staging failed",
                        );
                        return Err(e);
                    }
                }
            }
            _ => None,
        };

        // W19: drift detection. If a prior digest exists and the
        // freshly staged one differs, refuse to start.
        if let (Some(d), Some(prev)) = (digest.as_ref(), prior_crc) {
            if d.crc32c != prev {
                tracing::error!(
                    vm_id = vm_id_raw,
                    expected = format!("{:08x}", prev),
                    actual   = format!("{:08x}", d.crc32c),
                    "vm start aborted: backing image content drifted since last start",
                );
                return Err(CelError::Invalid(
                    "boot blob: image content changed since last start",
                ));
            }
        }

        let r = self.lookup_mut(id)?;
        if let Some(d) = digest {
            r.boot_blob_len = Some(d.blob_len);
            r.boot_blob_crc32c = Some(d.crc32c);
        }
        r.state = VmState::Halted;
        r.last_exit = Some(12);
        Ok(r.state)
    }

    /// W19: move a terminal VM (`Halted` / `Stopped` / `Faulted`)
    /// back to `Created` without freeing the slot.
    ///
    /// Preserves every operator-supplied field (`label`, `image_path`,
    /// `cpu_count`, `memory_mib`) **and** any recorded
    /// `boot_blob_*` digest, so a subsequent `start_vm` will run
    /// drift detection against the prior digest.
    ///
    /// Errors:
    /// - [`CelError::Invalid`] if the VM is not currently terminal.
    pub fn reset_vm(&mut self, id: VmId) -> CelResult<VmState> {
        let r = self.lookup_mut(id)?;
        if !r.state.is_terminal() {
            return Err(CelError::Invalid("vm not terminal; cannot reset"));
        }
        r.state = VmState::Created;
        r.last_exit = None;
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

/// Default stage root for boot-blob staging. Sibling of
/// [`default_state_path`] so the CLI lays out one self-contained
/// `build/` tree.
#[must_use]
pub fn default_stage_root() -> PathBuf {
    Path::new("build").join("stage")
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

    // -- W18.3: boot-blob staging --------------------------------------

    /// Helper: write a 4 KiB raw image filled with `byte` under `dir`.
    fn synth_raw(dir: &Path, name: &str, byte: u8) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![byte; 4096]).unwrap();
        p
    }

    #[test]
    fn start_with_image_stages_boot_blob_and_records_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw(tmp.path(), "disk.img", 0x55);
        let stage = tmp.path().join("stage");

        let mut c = Controller::in_memory().with_stage_root(&stage);
        let id = c.create_vm_with(VmSpec {
            label: "img".into(),
            image_path: Some(img.display().to_string()),
            ..Default::default()
        }).unwrap();

        c.start_vm(id).unwrap();
        let r = &c.list_vms()[0];
        assert_eq!(r.state, VmState::Halted);
        assert_eq!(r.boot_blob_len, Some(4096));
        assert_eq!(r.boot_blob_crc32c, Some(celimage::crc32c(&[0x55u8; 4096])));
        let blob = stage.join(format!("vm-{}", id.0)).join("boot.blob");
        assert_eq!(std::fs::read(&blob).unwrap().len(), 4096);
    }

    #[test]
    fn start_with_image_but_no_stage_root_skips_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw(tmp.path(), "disk.img", 0xAA);

        let mut c = Controller::in_memory();
        let id = c.create_vm_with(VmSpec {
            label: "no-stage".into(),
            image_path: Some(img.display().to_string()),
            ..Default::default()
        }).unwrap();

        c.start_vm(id).unwrap();
        let r = &c.list_vms()[0];
        assert_eq!(r.state, VmState::Halted);
        assert!(r.boot_blob_len.is_none());
        assert!(r.boot_blob_crc32c.is_none());
    }

    #[test]
    fn start_with_missing_image_aborts_and_keeps_created_state() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist.img");
        let stage = tmp.path().join("stage");

        let mut c = Controller::in_memory().with_stage_root(&stage);
        let id = c.create_vm_with(VmSpec {
            label: "bad".into(),
            image_path: Some(missing.display().to_string()),
            ..Default::default()
        }).unwrap();

        let err = c.start_vm(id).unwrap_err();
        assert!(
            matches!(err, CelError::Io(_) | CelError::Invalid(_)),
            "unexpected variant: {err:?}",
        );
        // Staging failure must leave the VM retryable, not terminal.
        assert_eq!(c.vm_state(id).unwrap(), VmState::Created);
    }

    #[test]
    fn start_without_image_path_still_works_with_stage_root() {
        // Stage-root-bound controller must remain backward-compatible
        // for VMs that have no image_path.
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage");
        let mut c = Controller::in_memory().with_stage_root(&stage);
        let id = c.create_vm("plain").unwrap();
        c.start_vm(id).unwrap();
        let r = &c.list_vms()[0];
        assert_eq!(r.state, VmState::Halted);
        assert!(r.boot_blob_len.is_none());
        // Stage directory must not have been pre-created for VMs that
        // didn't actually stage anything.
        assert!(!stage.join(format!("vm-{}", id.0)).exists());
    }

    // --- W19: reset + drift detection ---------------------------------------

    #[test]
    fn reset_rejects_non_terminal_vm() {
        let mut c = Controller::in_memory();
        let id = c.create_vm("fresh").unwrap();
        // Created is not terminal.
        let err = c.reset_vm(id).unwrap_err();
        assert!(matches!(err, CelError::Invalid(_)), "got: {err:?}");
    }

    #[test]
    fn reset_returns_terminal_vm_to_created_and_preserves_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw(tmp.path(), "disk.img", 0x7E);
        let stage = tmp.path().join("stage");

        let mut c = Controller::in_memory().with_stage_root(&stage);
        let id = c.create_vm_with(VmSpec {
            label: "r".into(),
            image_path: Some(img.display().to_string()),
            ..Default::default()
        }).unwrap();
        c.start_vm(id).unwrap();
        let crc_before = c.list_vms()[0].boot_blob_crc32c;
        let img_before = c.list_vms()[0].image_path.clone();
        assert!(crc_before.is_some());

        assert_eq!(c.reset_vm(id).unwrap(), VmState::Created);
        let r = &c.list_vms()[0];
        assert_eq!(r.state, VmState::Created);
        assert!(r.last_exit.is_none());
        // Operator-supplied + digest fields must be preserved.
        assert_eq!(r.image_path, img_before);
        assert_eq!(r.boot_blob_crc32c, crc_before);
        assert_eq!(r.boot_blob_len, Some(4096));
    }

    #[test]
    fn start_after_reset_detects_image_content_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw(tmp.path(), "disk.img", 0xC3);
        let stage = tmp.path().join("stage");

        let mut c = Controller::in_memory().with_stage_root(&stage);
        let id = c.create_vm_with(VmSpec {
            label: "drift".into(),
            image_path: Some(img.display().to_string()),
            ..Default::default()
        }).unwrap();

        c.start_vm(id).unwrap();
        let original_crc = c.list_vms()[0].boot_blob_crc32c.unwrap();
        c.reset_vm(id).unwrap();

        // Mutate the backing image content under us.
        std::fs::write(&img, vec![0xAAu8; 4096]).unwrap();

        let err = c.start_vm(id).unwrap_err();
        assert!(
            matches!(err, CelError::Invalid(m) if m.contains("image content changed")),
        );
        // The drifted start must NOT have mutated state or digest.
        let r = &c.list_vms()[0];
        assert_eq!(r.state, VmState::Created);
        assert_eq!(r.boot_blob_crc32c, Some(original_crc));
    }

    #[test]
    fn start_after_reset_with_unchanged_image_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw(tmp.path(), "disk.img", 0x42);
        let stage = tmp.path().join("stage");

        let mut c = Controller::in_memory().with_stage_root(&stage);
        let id = c.create_vm_with(VmSpec {
            label: "ok".into(),
            image_path: Some(img.display().to_string()),
            ..Default::default()
        }).unwrap();

        c.start_vm(id).unwrap();
        c.reset_vm(id).unwrap();
        // Same bytes → same digest → start must succeed.
        c.start_vm(id).unwrap();
        assert_eq!(c.list_vms()[0].state, VmState::Halted);
    }

    // --- W19 Phase B: crash-safe persistence + cross-restart drift ----------

    #[test]
    fn save_is_atomic_and_does_not_leak_tmp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("celctl-state.json");

        let mut c = Controller::load(&state_path).unwrap();
        let id = c.create_vm("atomic").unwrap();
        c.save().unwrap();

        // Exactly one file in the directory — the final state file.
        // No `*.tmp.<pid>` sidecar must survive a successful save.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["celctl-state.json"], "got: {entries:?}");

        // Reloading must see the same VM.
        let c2 = Controller::load(&state_path).unwrap();
        assert_eq!(c2.vm_count(), 1);
        assert_eq!(c2.list_vms()[0].id, id);
    }

    #[test]
    fn save_overwrite_replaces_prior_state_atomically() {
        // Two sequential saves must result in the second save's
        // content being fully visible; no torn writes, no leftover
        // tmp file. Exercises the rename-over-existing path.
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("s.json");

        let mut c = Controller::load(&state_path).unwrap();
        c.create_vm("first").unwrap();
        c.save().unwrap();

        c.create_vm("second").unwrap();
        c.save().unwrap();

        let c2 = Controller::load(&state_path).unwrap();
        let rows = c2.list_vms();
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["first", "second"]);

        // Still no stale tmp file.
        let stale: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(stale.is_empty(), "stale tmp files: {stale:?}");
    }

    #[test]
    fn drift_detection_survives_save_then_load_roundtrip() {
        // The whole point of persisting `boot_blob_crc32c` is that
        // a fresh controller process can still catch a swapped
        // backing image. Save the state mid-life, drop the
        // controller, reload, mutate the image, retry start →
        // must refuse.
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw(tmp.path(), "disk.img", 0x5A);
        let stage = tmp.path().join("stage");
        let state_path = tmp.path().join("state.json");

        let id = {
            let mut c = Controller::load(&state_path)
                .unwrap()
                .with_stage_root(&stage);
            let id = c.create_vm_with(VmSpec {
                label: "persisted".into(),
                image_path: Some(img.display().to_string()),
                ..Default::default()
            }).unwrap();
            c.start_vm(id).unwrap();
            c.reset_vm(id).unwrap();
            c.save().unwrap();
            id
        };

        // Mutate image content while no controller is alive.
        std::fs::write(&img, vec![0xE7u8; 4096]).unwrap();

        // Fresh controller, fresh process logic — must still refuse.
        let mut c2 = Controller::load(&state_path)
            .unwrap()
            .with_stage_root(&stage);
        let err = c2.start_vm(id).unwrap_err();
        assert!(
            matches!(err, CelError::Invalid(m) if m.contains("image content changed")),
            "got: {err:?}",
        );
        assert_eq!(c2.vm_state(id).unwrap(), VmState::Created);
    }

    // --- W20: aggregate stats -----------------------------------------------

    #[test]
    fn stats_tracks_slot_occupancy_and_states() {
        let mut c = Controller::in_memory();
        let s0 = c.stats();
        assert_eq!(s0.slots_total, MAX_VMS);
        assert_eq!(s0.slots_free, MAX_VMS);
        assert_eq!(s0.allocated, 0);

        let a = c.create_vm("a").unwrap();
        let _b = c.create_vm("b").unwrap();
        c.start_vm(a).unwrap(); // Created -> Halted (no image, no stage_root)

        let s = c.stats();
        assert_eq!(s.allocated, 2);
        assert_eq!(s.slots_free, MAX_VMS - 2);
        assert_eq!(s.halted, 1);
        assert_eq!(s.created, 1);
        assert_eq!(s.running, 0);
        assert_eq!(s.with_boot_blob, 0);
    }

    #[test]
    fn stats_counts_boot_blob_only_for_staged_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let img = synth_raw(tmp.path(), "disk.img", 0x77);
        let stage = tmp.path().join("stage");

        let mut c = Controller::in_memory().with_stage_root(&stage);
        let with_img = c.create_vm_with(VmSpec {
            label: "img".into(),
            image_path: Some(img.display().to_string()),
            ..Default::default()
        }).unwrap();
        let _plain = c.create_vm("plain").unwrap();
        c.start_vm(with_img).unwrap();

        let s = c.stats();
        assert_eq!(s.allocated, 2);
        assert_eq!(s.with_boot_blob, 1);
    }
}
