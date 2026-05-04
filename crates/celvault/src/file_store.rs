//! Disk-backed [`VolumeStore`] used by W13 to provide real
//! persistence across process restarts.
//!
//! ## On-disk layout
//!
//! ```text
//! <root>/
//!   manifest.json          // VolumeMeta + SnapshotMeta + counters
//!   volumes/<safe-id>.bin  // raw bytes, size == VolumeMeta.size_bytes
//!   snapshots/<safe-id>.bin // raw bytes, size == SnapshotMeta.size_bytes
//! ```
//!
//! `safe-id` replaces the characters `/` and `@` with `_` so that
//! a volume id like `n2/v1` becomes `n2_v1.bin` and a snapshot id
//! like `n2/v1@s2` becomes `n2_v1_s2.bin`. Reverse lookups are not
//! needed because every operation flows through the manifest.
//!
//! Manifest writes are atomic: write `manifest.json.tmp`, fsync,
//! rename. Body writes use random-access `pwrite`-style updates via
//! `Seek + Write`; we tolerate torn body writes today on the
//! assumption that the guest will retry on a clean reopen. A
//! future revision can layer in journaling.
//!
//! No `unsafe`. All I/O errors are mapped onto `CelError::Io`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

use crate::{
    SnapshotId, SnapshotMeta, VolumeId, VolumeMeta, VolumeStore, MAX_NAME, MAX_VOLUME_BYTES,
};

/// Disk-backed [`VolumeStore`] rooted at a directory.
pub struct FileVolumeStore {
    root: PathBuf,
    inner: Mutex<FileInner>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    /// Per-owner counter so that `<owner>/v<n>` ids stay monotonic
    /// across reopens.
    next_per_owner: BTreeMap<String, u64>,
    /// Per-volume snapshot counter so `<vol>@s<n>` stays monotonic.
    next_snap: BTreeMap<VolumeId, u64>,
    volumes:   BTreeMap<VolumeId, VolumeMeta>,
    snapshots: BTreeMap<SnapshotId, SnapshotMeta>,
}

struct FileInner {
    manifest: Manifest,
}

fn map_io<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> CelError + '_ {
    move |e| CelError::Io(format!("file vault: {ctx}: {e}"))
}

fn safe_id(s: &str) -> String {
    s.chars().map(|c| match c {
        '/' | '@' | ':' | '\\' => '_',
        c => c,
    }).collect()
}

impl FileVolumeStore {
    /// Open or create a `FileVolumeStore` rooted at `root`. Missing
    /// directories are created. An existing manifest is reloaded
    /// verbatim, so volume ids and snapshot counters survive
    /// process restart.
    ///
    /// # Errors
    /// Returns `CelError::Io` if the directories cannot be created,
    /// or if an existing manifest exists but is unreadable / invalid
    /// JSON.
    pub fn open_or_create(root: impl AsRef<Path>) -> CelResult<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("volumes")).map_err(map_io("create volumes/"))?;
        fs::create_dir_all(root.join("snapshots")).map_err(map_io("create snapshots/"))?;
        let manifest = Self::load_manifest(&root)?;
        Ok(Self {
            root,
            inner: Mutex::new(FileInner { manifest }),
        })
    }

    fn manifest_path(root: &Path) -> PathBuf { root.join("manifest.json") }

    fn load_manifest(root: &Path) -> CelResult<Manifest> {
        let path = Self::manifest_path(root);
        if !path.exists() {
            return Ok(Manifest::default());
        }
        let bytes = fs::read(&path).map_err(map_io("read manifest"))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| CelError::Io(format!("file vault: parse manifest: {e}")))
    }

    fn save_manifest(&self, m: &Manifest) -> CelResult<()> {
        let path = Self::manifest_path(&self.root);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(m)
            .map_err(|e| CelError::Io(format!("file vault: encode manifest: {e}")))?;
        {
            let mut f = File::create(&tmp).map_err(map_io("create manifest tmp"))?;
            f.write_all(&bytes).map_err(map_io("write manifest tmp"))?;
            f.sync_all().map_err(map_io("fsync manifest tmp"))?;
        }
        fs::rename(&tmp, &path).map_err(map_io("rename manifest"))?;
        Ok(())
    }

    fn volume_path(&self, id: &VolumeId) -> PathBuf {
        self.root.join("volumes").join(format!("{}.bin", safe_id(id.as_str())))
    }

    fn snapshot_path(&self, id: &SnapshotId) -> PathBuf {
        self.root.join("snapshots").join(format!("{}.bin", safe_id(id.as_str())))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FileInner> {
        match self.inner.lock() {
            Ok(g)  => g,
            Err(p) => p.into_inner(),
        }
    }

    fn read_body(path: &Path) -> CelResult<Vec<u8>> {
        let mut f = File::open(path).map_err(map_io("open body"))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(map_io("read body"))?;
        Ok(buf)
    }

    fn write_body(path: &Path, bytes: &[u8]) -> CelResult<()> {
        let mut f = File::create(path).map_err(map_io("create body"))?;
        f.write_all(bytes).map_err(map_io("write body"))?;
        f.sync_all().map_err(map_io("fsync body"))?;
        Ok(())
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

impl VolumeStore for FileVolumeStore {
    fn create(&self, owner: &str, name: &str, size_bytes: u64) -> CelResult<VolumeMeta> {
        Self::validate_name(name)?;
        if size_bytes == 0 {
            return Err(CelError::Invalid("volume size: must be > 0"));
        }
        if size_bytes > MAX_VOLUME_BYTES {
            return Err(CelError::Invalid("volume size: exceeds MAX_VOLUME_BYTES"));
        }
        let mut g = self.lock();
        let n = g.manifest.next_per_owner.entry(owner.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        let counter = *n;
        let id = VolumeId(format!("{owner}/v{counter}"));
        let meta = VolumeMeta {
            id: id.clone(),
            owner: owner.to_string(),
            name: name.to_string(),
            size_bytes,
        };
        // Allocate a zero-filled body file of the requested size.
        let body = vec![0u8; size_bytes as usize];
        Self::write_body(&self.volume_path(&id), &body)?;
        g.manifest.volumes.insert(id, meta.clone());
        self.save_manifest(&g.manifest)?;
        Ok(meta)
    }

    fn delete(&self, id: &VolumeId) -> CelResult<()> {
        let mut g = self.lock();
        if g.manifest.volumes.remove(id).is_some() {
            let _ = fs::remove_file(self.volume_path(id));
        }
        g.manifest.next_snap.remove(id);
        // Cascade-delete snapshots.
        let to_drop: Vec<SnapshotId> = g.manifest.snapshots
            .iter()
            .filter(|(_, m)| &m.volume == id)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &to_drop {
            let _ = fs::remove_file(self.snapshot_path(k));
            g.manifest.snapshots.remove(k);
        }
        self.save_manifest(&g.manifest)?;
        Ok(())
    }

    fn list(&self) -> Vec<VolumeMeta> {
        self.lock().manifest.volumes.values().cloned().collect()
    }

    fn get(&self, id: &VolumeId) -> Option<VolumeMeta> {
        self.lock().manifest.volumes.get(id).cloned()
    }

    fn read(&self, id: &VolumeId, offset: u64, len: usize) -> CelResult<Vec<u8>> {
        let g = self.lock();
        let meta = g.manifest.volumes.get(id)
            .ok_or(CelError::Invalid("volume: unknown id"))?;
        let end = offset
            .checked_add(len as u64)
            .ok_or(CelError::Invalid("volume: read range overflow"))?;
        if end > meta.size_bytes {
            return Err(CelError::Invalid("volume: read past end"));
        }
        let path = self.volume_path(id);
        drop(g);
        let mut f = File::open(&path).map_err(map_io("open volume"))?;
        f.seek(SeekFrom::Start(offset)).map_err(map_io("seek volume"))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).map_err(map_io("read volume"))?;
        Ok(buf)
    }

    fn write(&self, id: &VolumeId, offset: u64, data: &[u8]) -> CelResult<()> {
        let g = self.lock();
        let meta = g.manifest.volumes.get(id)
            .ok_or(CelError::Invalid("volume: unknown id"))?;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(CelError::Invalid("volume: write range overflow"))?;
        if end > meta.size_bytes {
            return Err(CelError::Invalid("volume: write past end"));
        }
        let path = self.volume_path(id);
        drop(g);
        let mut f = OpenOptions::new()
            .read(true).write(true).open(&path)
            .map_err(map_io("open volume rw"))?;
        f.seek(SeekFrom::Start(offset)).map_err(map_io("seek volume"))?;
        f.write_all(data).map_err(map_io("write volume"))?;
        f.sync_data().map_err(map_io("fsync volume"))?;
        Ok(())
    }

    fn create_snapshot(&self, id: &VolumeId, name: &str) -> CelResult<SnapshotMeta> {
        Self::validate_name(name)?;
        let mut g = self.lock();
        let vol_meta = g.manifest.volumes.get(id)
            .ok_or(CelError::Invalid("snapshot: unknown volume"))?
            .clone();
        let body = Self::read_body(&self.volume_path(id))?;
        if body.len() as u64 != vol_meta.size_bytes {
            return Err(CelError::Io(
                "file vault: volume body size diverges from manifest".into(),
            ));
        }
        let n = g.manifest.next_snap.entry(id.clone()).or_insert(0);
        *n = n.saturating_add(1);
        let counter = *n;
        let snap_id = SnapshotId(format!("{id}@s{counter}"));
        let meta = SnapshotMeta {
            id: snap_id.clone(),
            volume: id.clone(),
            name: name.to_string(),
            size_bytes: vol_meta.size_bytes,
        };
        Self::write_body(&self.snapshot_path(&snap_id), &body)?;
        g.manifest.snapshots.insert(snap_id, meta.clone());
        self.save_manifest(&g.manifest)?;
        Ok(meta)
    }

    fn list_snapshots(&self, volume: Option<&VolumeId>) -> Vec<SnapshotMeta> {
        let g = self.lock();
        g.manifest.snapshots.values()
            .filter(|s| volume.map_or(true, |v| &s.volume == v))
            .cloned()
            .collect()
    }

    fn delete_snapshot(&self, id: &SnapshotId) -> CelResult<()> {
        let mut g = self.lock();
        if g.manifest.snapshots.remove(id).is_some() {
            let _ = fs::remove_file(self.snapshot_path(id));
        }
        self.save_manifest(&g.manifest)?;
        Ok(())
    }

    fn restore_snapshot(&self, id: &SnapshotId) -> CelResult<()> {
        let g = self.lock();
        let snap = g.manifest.snapshots.get(id)
            .ok_or(CelError::Invalid("restore: unknown snapshot"))?
            .clone();
        let vol = g.manifest.volumes.get(&snap.volume)
            .ok_or(CelError::Invalid("restore: volume gone"))?
            .clone();
        if vol.size_bytes != snap.size_bytes {
            return Err(CelError::Invalid("restore: volume size changed"));
        }
        drop(g);
        let bytes = Self::read_body(&self.snapshot_path(id))?;
        Self::write_body(&self.volume_path(&snap.volume), &bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tempfile` lives only behind dev-deps, so build a temp dir
    /// the simple way to keep this crate's surface unchanged.
    fn tmp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("celvault-test-{nanos}-{:?}", std::thread::current().id()));
        p
    }

    #[test]
    fn round_trip_through_disk() {
        let root = tmp_root();
        let s = FileVolumeStore::open_or_create(&root).unwrap();
        let v = s.create("n1", "data", 16).unwrap();
        s.write(&v.id, 0, b"hello world!!!!!").unwrap();
        assert_eq!(s.read(&v.id, 0, 5).unwrap(), b"hello");

        // Reopen — manifest must restore counters and ids.
        drop(s);
        let s2 = FileVolumeStore::open_or_create(&root).unwrap();
        let r = s2.read(&v.id, 0, 16).unwrap();
        assert_eq!(&r, b"hello world!!!!!");
        let v2 = s2.create("n1", "more", 4).unwrap();
        assert_eq!(v2.id.as_str(), "n1/v2");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_persists_across_reopen() {
        let root = tmp_root();
        let s = FileVolumeStore::open_or_create(&root).unwrap();
        let v = s.create("n2", "data", 8).unwrap();
        s.write(&v.id, 0, b"original").unwrap();
        let snap = s.create_snapshot(&v.id, "first").unwrap();
        s.write(&v.id, 0, b"changed!").unwrap();

        drop(s);
        let s2 = FileVolumeStore::open_or_create(&root).unwrap();
        // Snapshot survives reopen
        assert_eq!(s2.list_snapshots(Some(&v.id)).len(), 1);
        // Volume currently holds the post-snapshot bytes
        assert_eq!(&s2.read(&v.id, 0, 8).unwrap(), b"changed!");
        // Restore brings the original bytes back.
        s2.restore_snapshot(&snap.id).unwrap();
        assert_eq!(&s2.read(&v.id, 0, 8).unwrap(), b"original");

        let _ = std::fs::remove_dir_all(&root);
    }
}
