//! CelVault — Celium's persistent volume layer.
//!
//! Week-12 surface: opaque [`VolumeId`]s allocated by a per-node
//! [`VolumeStore`], plus typed [`VolumeMeta`] and per-VM
//! [`VolumeAttachment`] records that travel through `celmesh`'s
//! gossip and RPC layers.
//!
//! The default implementation, [`MemVolumeStore`], holds volume
//! contents in RAM. It is sufficient for the in-tree integration
//! tests and for the single-node demo. A disk-backed store is the
//! next sprint's work.
#![forbid(unsafe_code)]
#![warn(missing_docs, rust_2018_idioms)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

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

/// Per-node volume store API.
///
/// Implementations must be `Send + Sync` and internally locked. The
/// trait methods are deliberately synchronous — they're cheap pointer
/// shuffles for `MemVolumeStore`, and a future disk-backed
/// implementation can wrap `tokio::task::spawn_blocking` at the call
/// site rather than infecting the trait surface.
pub trait VolumeStore: Send + Sync {
    /// Create a new volume owned by `owner`.
    fn create(&self, owner: &str, name: &str, size_bytes: u64) -> CelResult<VolumeMeta>;
    /// Delete a volume. Idempotent: deleting a missing volume is `Ok`.
    /// Callers must ensure no VM is currently attached.
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
}

/// Reference in-memory implementation of [`VolumeStore`].
pub struct MemVolumeStore {
    inner: Mutex<MemInner>,
}

#[derive(Default)]
struct MemInner {
    next: u64,
    rows: BTreeMap<VolumeId, MemVolume>,
}

#[derive(Debug)]
struct MemVolume {
    meta: VolumeMeta,
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
}
