//! CelVault — Celium's persistent volume layer.
//!
//! ## Surface
//! W12 introduced opaque [`VolumeId`]s allocated by a per-node
//! [`VolumeStore`], typed [`VolumeMeta`], and per-VM
//! [`VolumeAttachment`] records that travel through `celmesh`'s
//! gossip and RPC layers.
//!
//! W13 adds:
//!
//! * **Snapshots.** A volume can be snapshotted at any time; the
//!   resulting [`SnapshotMeta`] is addressable cluster-wide via its
//!   [`SnapshotId`]. Snapshots can be listed, deleted, and restored
//!   onto their parent volume.
//! * **A disk-backed [`FileVolumeStore`].** Persists volume bodies
//!   and snapshots under a root directory so data survives process
//!   restart. Its on-disk layout is documented in the type's docs.
//! * **Random-access read/write are unchanged.** Both the in-memory
//!   and disk-backed stores implement the full [`VolumeStore`]
//!   trait, including the new snapshot methods.
//!
//! All public surface keeps the project-wide rule: every fallible
//! call returns `CelResult<T>`; no `unwrap`/`panic` on production
//! paths; `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]
#![warn(missing_docs, rust_2018_idioms)]
#![deny(rustdoc::broken_intra_doc_links)]

mod file_store;
pub mod network;

use std::collections::BTreeMap;
use std::sync::Mutex;

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

pub use file_store::FileVolumeStore;
pub use network::{
    Cidr4, Direction, L4Proto, LbAlgo, LbBackend, LoadBalancer, LoadBalancerId,
    MemNetworkStore, NetworkId, NetworkStore, Nic, NicId, SecurityGroup, SecurityGroupId,
    SecurityRule, VirtualNetwork,
};

/// Returns `Ok(())` once the vault subsystem has been initialised.
///
/// # Errors
/// Currently infallible; signature reserved for future storage/network work.
pub fn init() -> CelResult<()> {
    tracing::debug!("celvault::init");
    Ok(())
}

/// Globally unique volume identifier.
///
/// Generated as `"<owner-node-id>/v<counter>"` so the wire form is
/// human-readable and node-scoped without pulling in a UUID dep.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VolumeId(pub String);

impl VolumeId {
    /// Borrow the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for VolumeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for VolumeId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

/// Globally unique snapshot identifier.
///
/// Wire form: `"<volume-id>@s<counter>"`, e.g. `"n2/v3@s1"`. Counter
/// is per-volume so a snapshot id always resolves uniquely without a
/// separate lookup table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SnapshotId(pub String);

impl SnapshotId {
    /// Borrow the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }

    /// Split a snapshot id back into its `(volume, counter)`
    /// components. Returns `None` if the id is malformed.
    #[must_use]
    pub fn split(&self) -> Option<(VolumeId, u64)> {
        let (vol, tail) = self.0.split_once('@')?;
        let n = tail.strip_prefix('s')?.parse().ok()?;
        Some((VolumeId(vol.to_string()), n))
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SnapshotId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

/// User-visible metadata for a volume. The actual byte content lives
/// behind the [`VolumeStore`] interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMeta {
    /// Globally-unique id.
    pub id: VolumeId,
    /// Owning node. Volumes are pinned to a single host today.
    pub owner: String,
    /// Free-form label, ≤ `MAX_NAME` chars.
    pub name: String,
    /// Logical size in bytes. The store may allocate lazily.
    pub size_bytes: u64,
}

/// User-visible metadata for a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// Globally-unique id.
    pub id: SnapshotId,
    /// Volume this snapshot was taken from.
    pub volume: VolumeId,
    /// Free-form label provided at creation time, ≤ `MAX_NAME` chars.
    pub name: String,
    /// Logical size of the captured volume body. Always equal to the
    /// volume's `size_bytes` at the moment the snapshot was taken.
    pub size_bytes: u64,
}

/// Single volume → VM attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeAttachment {
    /// Volume to attach.
    pub volume_id: VolumeId,
    /// Mount-point name inside the guest. ≤ `MAX_MOUNT` chars.
    /// Free-form; the host does not interpret it today.
    pub mount_name: String,
}

/// Maximum permitted volume name size.
pub const MAX_NAME: usize  = 64;
/// Maximum permitted mount-point name size.
pub const MAX_MOUNT: usize = 32;
/// Hard cap on volume size to keep accidental allocations bounded.
pub const MAX_VOLUME_BYTES: u64 = 64 * 1024 * 1024;

/// Per-volume usage statistics. W17 added this surface so the
/// observability collector and the operator CLI can report real
/// storage state without scraping internal types. The struct is
/// intentionally cheap to compute — every field is derived from
/// already-tracked metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeStats {
    /// Volume the stats refer to.
    pub id: VolumeId,
    /// Logical size as declared at creation time.
    pub size_bytes: u64,
    /// Snapshots currently held against this volume.
    pub snapshot_count: u32,
    /// Sum of the captured bodies' declared sizes. Mirrors what the
    /// supervisor would have to copy if the volume were rebuilt
    /// purely from snapshots.
    pub total_snapshot_bytes: u64,
}

/// Result of a [`VolumeStore::integrity_check`] pass. The check is
/// a fast pointer-walk: every volume's body is opened and its
/// length is compared with `VolumeMeta::size_bytes`. Body bytes
/// themselves are not hashed — that is reserved for a future
/// `verify` API which will be policy-driven.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IntegrityReport {
    /// Number of volumes inspected.
    pub volumes_checked: u32,
    /// Number of snapshots inspected.
    pub snapshots_checked: u32,
    /// Per-resource error strings. Empty means clean.
    pub errors: Vec<String>,
}

impl IntegrityReport {
    /// `true` if the report contains no error rows.
    #[must_use]
    pub fn is_clean(&self) -> bool { self.errors.is_empty() }
}

/// Per-node volume store API.
///
/// Implementations must be `Send + Sync` and internally locked. The
/// trait methods are deliberately synchronous — they're cheap pointer
/// shuffles for `MemVolumeStore`, and a future async-disk
/// implementation can wrap `tokio::task::spawn_blocking` at the call
/// site rather than infecting the trait surface.
pub trait VolumeStore: Send + Sync {
    /// Create a new volume owned by `owner`.
    fn create(&self, owner: &str, name: &str, size_bytes: u64) -> CelResult<VolumeMeta>;
    /// Delete a volume. Idempotent: deleting a missing volume is `Ok`.
    /// Callers must ensure no VM is currently attached. Snapshots of
    /// the volume are deleted alongside it.
    fn delete(&self, id: &VolumeId) -> CelResult<()>;
    /// All known volumes.
    fn list(&self) -> Vec<VolumeMeta>;
    /// Lookup a single volume by id.
    fn get(&self, id: &VolumeId) -> Option<VolumeMeta>;
    /// Random-access read. Out-of-range reads error with
    /// `CelError::Invalid`.
    fn read(&self, id: &VolumeId, offset: u64, len: usize) -> CelResult<Vec<u8>>;
    /// Random-access write. Out-of-range writes error with
    /// `CelError::Invalid`.
    fn write(&self, id: &VolumeId, offset: u64, data: &[u8]) -> CelResult<()>;

    // -- W13 snapshot API ---------------------------------------------------

    /// Capture a point-in-time copy of `id` and return the new
    /// snapshot's metadata. Errors with `CelError::Invalid` if the
    /// volume is unknown.
    fn create_snapshot(&self, id: &VolumeId, name: &str) -> CelResult<SnapshotMeta>;
    /// All snapshots, optionally filtered to those of `volume`.
    /// Pass `None` to list every snapshot in the store.
    fn list_snapshots(&self, volume: Option<&VolumeId>) -> Vec<SnapshotMeta>;
    /// Delete a snapshot. Idempotent: deleting a missing snapshot is
    /// `Ok`. Volume bodies are unaffected.
    fn delete_snapshot(&self, id: &SnapshotId) -> CelResult<()>;
    /// Overwrite the volume body with the snapshot's captured bytes.
    /// Errors with `CelError::Invalid` if the snapshot or its parent
    /// volume cannot be found, or if the volume's size has changed
    /// since the snapshot was taken.
    fn restore_snapshot(&self, id: &SnapshotId) -> CelResult<()>;

    // -- W17 durability + observability ------------------------------------

    /// Flush every pending durable change to stable media. The
    /// in-memory store is a no-op; the disk-backed store fsyncs the
    /// manifest. Implementations must be safe to call after every
    /// mutating op or once at shutdown — both patterns are valid.
    ///
    /// # Errors
    /// Returns [`CelError::Storage`] if the underlying backing store
    /// reports a sync failure.
    fn flush(&self) -> CelResult<()> { Ok(()) }

    /// Per-volume usage snapshot. Default impl computes counts from
    /// `list_snapshots`; specialised stores may override for speed.
    ///
    /// # Errors
    /// Returns [`CelError::Invalid`] if `id` is unknown to the store.
    fn stats(&self, id: &VolumeId) -> CelResult<VolumeStats> {
        let meta = self.get(id).ok_or(CelError::Invalid("stats: unknown volume"))?;
        let snaps = self.list_snapshots(Some(id));
        let total = snaps.iter().map(|s| s.size_bytes).fold(0u64, u64::saturating_add);
        let count = u32::try_from(snaps.len()).unwrap_or(u32::MAX);
        Ok(VolumeStats {
            id: meta.id,
            size_bytes: meta.size_bytes,
            snapshot_count: count,
            total_snapshot_bytes: total,
        })
    }

    /// Walk every volume + snapshot and compare the recorded
    /// `size_bytes` with the body size we can actually read. The
    /// default impl uses [`VolumeStore::read`] / `list_snapshots`,
    /// which is enough for both reference impls today; specialised
    /// stores may override to short-circuit (e.g. compare on-disk
    /// length without reading every byte).
    ///
    /// # Errors
    /// Currently infallible — every issue surfaces as a row in the
    /// returned report.
    fn integrity_check(&self) -> CelResult<IntegrityReport> {
        let mut rep = IntegrityReport::default();
        for v in self.list() {
            rep.volumes_checked = rep.volumes_checked.saturating_add(1);
            // Read the trailing byte (if any) so a torn body shows up.
            if v.size_bytes > 0 {
                let off = v.size_bytes - 1;
                if let Err(e) = self.read(&v.id, off, 1) {
                    rep.errors.push(format!("volume {}: {e}", v.id));
                }
            }
        }
        for s in self.list_snapshots(None) {
            rep.snapshots_checked = rep.snapshots_checked.saturating_add(1);
            // Snapshot bytes are not exposed via `read` so we just
            // check size consistency against the parent volume.
            match self.get(&s.volume) {
                None => rep.errors.push(format!(
                    "snapshot {}: parent volume {} missing", s.id, s.volume
                )),
                Some(parent) if parent.size_bytes != s.size_bytes =>
                    rep.errors.push(format!(
                        "snapshot {}: size {} != parent {} ({})",
                        s.id, s.size_bytes, s.volume, parent.size_bytes
                    )),
                Some(_) => {}
            }
        }
        Ok(rep)
    }
}

/// Reference in-memory implementation of [`VolumeStore`].
pub struct MemVolumeStore {
    inner: Mutex<MemInner>,
}

#[derive(Default)]
struct MemInner {
    next: u64,
    rows: BTreeMap<VolumeId, MemVolume>,
    /// Per-volume monotonic snapshot counter.
    next_snap: BTreeMap<VolumeId, u64>,
    /// All snapshots, keyed by id.
    snaps: BTreeMap<SnapshotId, MemSnapshot>,
}

#[derive(Debug)]
struct MemVolume {
    meta: VolumeMeta,
    body: Vec<u8>,
}

#[derive(Debug)]
struct MemSnapshot {
    meta: SnapshotMeta,
    body: Vec<u8>,
}

impl Default for MemVolumeStore {
    fn default() -> Self { Self::new() }
}

impl MemVolumeStore {
    /// Construct an empty in-memory volume store.
    #[must_use]
    pub fn new() -> Self {
        Self { inner: Mutex::new(MemInner::default()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemInner> {
        // No `unsafe` here. A poisoned lock means a previous user
        // panicked while holding the guard — we recover the inner
        // value because none of our critical sections leave the
        // structure half-mutated.
        match self.inner.lock() {
            Ok(g)  => g,
            Err(p) => p.into_inner(),
        }
    }

    fn validate_name(name: &str) -> CelResult<()> {
        if name.is_empty() {
            return Err(CelError::Invalid("volume name: empty"));
        }
        if name.len() > MAX_NAME {
            return Err(CelError::Invalid("volume name: too long"));
        }
        Ok(())
    }
}

impl VolumeStore for MemVolumeStore {
    fn create(&self, owner: &str, name: &str, size_bytes: u64) -> CelResult<VolumeMeta> {
        Self::validate_name(name)?;
        if size_bytes == 0 {
            return Err(CelError::Invalid("volume size: must be > 0"));
        }
        if size_bytes > MAX_VOLUME_BYTES {
            return Err(CelError::Invalid("volume size: exceeds MAX_VOLUME_BYTES"));
        }
        let mut g = self.lock();
        g.next = g.next.saturating_add(1);
        let id = VolumeId(format!("{owner}/v{}", g.next));
        let meta = VolumeMeta {
            id: id.clone(),
            owner: owner.to_string(),
            name: name.to_string(),
            size_bytes,
        };
        // size_bytes <= MAX_VOLUME_BYTES (64 MiB), well within usize
        // on 32/64-bit hosts — cast is safe.
        let body = vec![0u8; size_bytes as usize];
        g.rows.insert(id.clone(), MemVolume { meta: meta.clone(), body });
        Ok(meta)
    }

    fn delete(&self, id: &VolumeId) -> CelResult<()> {
        let mut g = self.lock();
        g.rows.remove(id);
        g.next_snap.remove(id);
        // Drop snapshots whose parent volume is gone.
        let drop: Vec<_> = g.snaps
            .iter()
            .filter(|(_, s)| &s.meta.volume == id)
            .map(|(k, _)| k.clone())
            .collect();
        for k in drop { g.snaps.remove(&k); }
        Ok(())
    }

    fn list(&self) -> Vec<VolumeMeta> {
        self.lock().rows.values().map(|v| v.meta.clone()).collect()
    }

    fn get(&self, id: &VolumeId) -> Option<VolumeMeta> {
        self.lock().rows.get(id).map(|v| v.meta.clone())
    }

    fn read(&self, id: &VolumeId, offset: u64, len: usize) -> CelResult<Vec<u8>> {
        let g = self.lock();
        let v = g.rows.get(id).ok_or(CelError::Invalid("volume: unknown id"))?;
        let off = usize::try_from(offset)
            .map_err(|_| CelError::Invalid("volume: offset overflow"))?;
        let end = off
            .checked_add(len)
            .ok_or(CelError::Invalid("volume: read range overflow"))?;
        if end > v.body.len() {
            return Err(CelError::Invalid("volume: read past end"));
        }
        Ok(v.body[off..end].to_vec())
    }

    fn write(&self, id: &VolumeId, offset: u64, data: &[u8]) -> CelResult<()> {
        let mut g = self.lock();
        let v = g.rows.get_mut(id).ok_or(CelError::Invalid("volume: unknown id"))?;
        let off = usize::try_from(offset)
            .map_err(|_| CelError::Invalid("volume: offset overflow"))?;
        let end = off
            .checked_add(data.len())
            .ok_or(CelError::Invalid("volume: write range overflow"))?;
        if end > v.body.len() {
            return Err(CelError::Invalid("volume: write past end"));
        }
        v.body[off..end].copy_from_slice(data);
        Ok(())
    }

    fn create_snapshot(&self, id: &VolumeId, name: &str) -> CelResult<SnapshotMeta> {
        Self::validate_name(name)?;
        let mut g = self.lock();
        let body = {
            let v = g.rows.get(id).ok_or(CelError::Invalid("snapshot: unknown volume"))?;
            v.body.clone()
        };
        let n = g.next_snap.entry(id.clone()).or_insert(0);
        *n = n.saturating_add(1);
        let counter = *n;
        let size_bytes = body.len() as u64;
        let snap_id = SnapshotId(format!("{id}@s{counter}"));
        let meta = SnapshotMeta {
            id:    snap_id.clone(),
            volume: id.clone(),
            name:  name.to_string(),
            size_bytes,
        };
        g.snaps.insert(snap_id, MemSnapshot { meta: meta.clone(), body });
        Ok(meta)
    }

    fn list_snapshots(&self, volume: Option<&VolumeId>) -> Vec<SnapshotMeta> {
        let g = self.lock();
        g.snaps.values()
            .filter(|s| volume.is_none_or(|v| &s.meta.volume == v))
            .map(|s| s.meta.clone())
            .collect()
    }

    fn delete_snapshot(&self, id: &SnapshotId) -> CelResult<()> {
        self.lock().snaps.remove(id);
        Ok(())
    }

    fn restore_snapshot(&self, id: &SnapshotId) -> CelResult<()> {
        let mut g = self.lock();
        let body = {
            let s = g.snaps.get(id).ok_or(CelError::Invalid("restore: unknown snapshot"))?;
            s.body.clone()
        };
        let vol_id = id.split()
            .ok_or(CelError::Invalid("restore: malformed snapshot id"))?
            .0;
        let v = g.rows.get_mut(&vol_id)
            .ok_or(CelError::Invalid("restore: volume gone"))?;
        if v.body.len() != body.len() {
            return Err(CelError::Invalid("restore: volume size changed"));
        }
        v.body.copy_from_slice(&body);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_read_write_round_trip() {
        let s = MemVolumeStore::new();
        let m = s.create("n1", "data", 32).unwrap();
        assert_eq!(m.size_bytes, 32);
        assert_eq!(m.owner, "n1");
        assert_eq!(m.id.as_str(), "n1/v1");
        s.write(&m.id, 4, b"abcd").unwrap();
        let r = s.read(&m.id, 0, 8).unwrap();
        assert_eq!(&r, &[0, 0, 0, 0, b'a', b'b', b'c', b'd']);
    }

    #[test]
    fn reject_invalid_volumes() {
        let s = MemVolumeStore::new();
        assert!(s.create("n1", "x", 0).is_err());
        assert!(s.create("n1", "x", MAX_VOLUME_BYTES + 1).is_err());
        assert!(s.create("n1", "", 4).is_err());
    }

    #[test]
    fn out_of_range_io_is_explicit() {
        let s = MemVolumeStore::new();
        let m = s.create("n1", "data", 8).unwrap();
        assert!(s.read(&m.id, 4, 8).is_err());
        assert!(s.write(&m.id, 4, &[0; 8]).is_err());
    }

    #[test]
    fn ids_are_node_scoped_and_monotonic() {
        let s = MemVolumeStore::new();
        let a = s.create("n1", "a", 4).unwrap();
        let b = s.create("n1", "b", 4).unwrap();
        let c = s.create("n2", "c", 4).unwrap();
        assert_eq!(a.id.as_str(), "n1/v1");
        assert_eq!(b.id.as_str(), "n1/v2");
        assert_eq!(c.id.as_str(), "n2/v3");
    }

    #[test]
    fn snapshot_create_list_restore_delete() {
        let s = MemVolumeStore::new();
        let v = s.create("n1", "data", 8).unwrap();
        s.write(&v.id, 0, b"hello!!!").unwrap();

        let snap = s.create_snapshot(&v.id, "first").unwrap();
        assert_eq!(snap.id.as_str(), "n1/v1@s1");
        assert_eq!(snap.volume, v.id);

        // Mutate the volume after the snapshot.
        s.write(&v.id, 0, b"OVERWRIT").unwrap();
        assert_eq!(&s.read(&v.id, 0, 8).unwrap(), b"OVERWRIT");

        // Listing — both filtered and unfiltered.
        let all = s.list_snapshots(None);
        assert_eq!(all.len(), 1);
        let just_v = s.list_snapshots(Some(&v.id));
        assert_eq!(just_v.len(), 1);
        let just_other = s.list_snapshots(Some(&VolumeId::from("nope/v9")));
        assert!(just_other.is_empty());

        // Restore returns the captured bytes.
        s.restore_snapshot(&snap.id).unwrap();
        assert_eq!(&s.read(&v.id, 0, 8).unwrap(), b"hello!!!");

        // Delete is idempotent.
        s.delete_snapshot(&snap.id).unwrap();
        s.delete_snapshot(&snap.id).unwrap();
        assert!(s.list_snapshots(None).is_empty());
    }

    #[test]
    fn deleting_volume_drops_snapshots() {
        let s = MemVolumeStore::new();
        let v = s.create("n1", "data", 4).unwrap();
        let _ = s.create_snapshot(&v.id, "a").unwrap();
        let _ = s.create_snapshot(&v.id, "b").unwrap();
        assert_eq!(s.list_snapshots(None).len(), 2);
        s.delete(&v.id).unwrap();
        assert!(s.list_snapshots(None).is_empty());
    }

    #[test]
    fn restore_unknown_or_orphaned_is_invalid() {
        let s = MemVolumeStore::new();
        assert!(s.restore_snapshot(&SnapshotId::from("nope/v1@s1")).is_err());
        let v = s.create("n1", "data", 4).unwrap();
        let snap = s.create_snapshot(&v.id, "a").unwrap();
        s.delete(&v.id).unwrap();
        assert!(s.restore_snapshot(&snap.id).is_err());
    }
}
