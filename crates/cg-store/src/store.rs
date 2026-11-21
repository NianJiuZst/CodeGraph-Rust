//! The `.cgr/` directory: manifest, locks, publishing and garbage collection (design doc 3.2,
//! 5.1, 5.7). Readers never block the writer and the writer never blocks readers: a snapshot is a
//! set of immutable files plus a shared lock that keeps them from being reclaimed.

use crate::seg::Seg;
use crate::view::View;
use cg_core::{brand, CgError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub format_version: u32,
    pub engine_version: u32,
    /// Bumped on every switch; readers compare it to notice a new snapshot.
    pub generation: u64,
    /// File name of the base segment.
    pub base: String,
    /// Generation embedded in the base's file name.
    pub base_gen: u64,
    /// File name of the current delta layer, if any.
    pub delta: Option<String>,
    pub delta_seq: u64,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub n_files: u32,
    pub n_nodes: u32,
    pub n_edges: u32,
    /// Number of syncs folded into the delta since the last merge.
    pub syncs_since_merge: u32,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// How hard writes are synced (`CGR_FSYNC`, design doc 16.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsyncMode {
    Default,
    Full,
    Off,
}

impl FsyncMode {
    pub fn from_env() -> FsyncMode {
        match std::env::var(brand::env::FSYNC).as_deref() {
            Ok("full") => FsyncMode::Full,
            Ok("off") => FsyncMode::Off,
            _ => FsyncMode::Default,
        }
    }
}

fn sync_file(f: &File, mode: FsyncMode, light: bool) -> std::io::Result<()> {
    match mode {
        FsyncMode::Off => Ok(()),
        FsyncMode::Default if light => {
            // macOS: plain fsync is enough for the small delta layers (sync_all would issue F_FULLFSYNC).
            #[cfg(target_os = "macos")]
            {
                use std::os::unix::io::AsRawFd;
                // SAFETY: valid fd owned by `f`.
                let r = unsafe { libc::fsync(f.as_raw_fd()) };
                return if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
            }
            #[allow(unreachable_code)]
            f.sync_all()
        }
        _ => f.sync_all(),
    }
}

fn sync_dir(dir: &Path, mode: FsyncMode, light: bool) {
    if mode == FsyncMode::Off {
        return;
    }
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = sync_file(&d, mode, light);
    }
    let _ = (dir, light);
}

pub struct Store {
    /// The `.cgr` directory.
    pub dir: PathBuf,
    /// The project root (parent of `.cgr`).
    pub root: PathBuf,
    pub fsync: FsyncMode,
}

/// A consistent view of the index plus the locks that keep its files alive.
pub struct Snapshot {
    pub view: View,
    pub manifest: Manifest,
    _locks: Vec<File>,
}

pub struct WriteGuard {
    _file: File,
}

pub struct WatchGuard {
    _file: File,
}

impl Store {
    pub fn at(root: &Path) -> Store {
        Store { dir: root.join(brand::DATA_DIR), root: root.to_path_buf(), fsync: FsyncMode::from_env() }
    }

    /// Nearest ancestor of `start` (inclusive) that has a `.cgr/manifest`.
    pub fn locate(start: &Path) -> Option<Store> {
        let mut cur = Some(start);
        while let Some(d) = cur {
            if d.join(brand::DATA_DIR).join("manifest").is_file() {
                return Some(Store::at(d));
            }
            cur = d.parent();
        }
        None
    }

    /// Create the data directory. A `.gitignore` containing `*` inside it keeps the index out of
    /// version control without touching the user's own ignore files (design doc 15.2 D3).
    pub fn create(&self) -> Result<()> {
        for sub in ["", "locks", "tmp", "logs"] {
            std::fs::create_dir_all(self.dir.join(sub))?;
        }
        let gi = self.dir.join(".gitignore");
        if !gi.exists() {
            std::fs::write(gi, "*\n")?;
        }
        Ok(())
    }

    fn lock_path(&self, name: &str) -> PathBuf {
        self.dir.join("locks").join(name)
    }

    fn open_lock(&self, name: &str) -> Result<File> {
        std::fs::create_dir_all(self.dir.join("locks"))?;
        Ok(OpenOptions::new().create(true).read(true).write(true).truncate(false).open(self.lock_path(name))?)
    }

    // ------------------------------------------------------------- manifest
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest")
    }

    pub fn read_manifest(&self) -> Result<Option<Manifest>> {
        match std::fs::read(self.manifest_path()) {
            Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| CgError::corrupt(format!("manifest: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn read_prev_manifest(&self) -> Option<Manifest> {
        serde_json::from_slice(&std::fs::read(self.dir.join("manifest.prev")).ok()?).ok()
    }

    /// Generation of the current manifest, 0 when there is no index.
    pub fn generation(&self) -> u64 {
        self.read_manifest().ok().flatten().map_or(0, |m| m.generation)
    }

    fn write_manifest(&self, m: &Manifest, light: bool) -> Result<()> {
        // manifest -> manifest.prev (tmp + rename), then the new one the same way.
        if let Ok(cur) = std::fs::read(self.manifest_path()) {
            let tmp = self.dir.join("manifest.prev.tmp");
            std::fs::write(&tmp, cur)?;
            std::fs::rename(&tmp, self.dir.join("manifest.prev"))?;
        }
        let tmp = self.dir.join("manifest.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(m).expect("manifest json"))?;
            sync_file(&f, self.fsync, light)?;
        }
        std::fs::rename(&tmp, self.manifest_path())?; // the commit point
        sync_dir(&self.dir, self.fsync, light);
        Ok(())
    }

    // ---------------------------------------------------------------- locks
    /// Exclusive writer lock; waits up to `timeout`.
    pub fn write_lock(&self, timeout: Duration) -> Result<WriteGuard> {
        let f = self.open_lock("write.lock")?;
        let deadline = Instant::now() + timeout;
        loop {
            if f.try_lock().is_ok() {
                return Ok(WriteGuard { _file: f });
            }
            if Instant::now() >= deadline {
                return Err(CgError::Lock("another cgr process is writing the index (write.lock)".into()));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Non-blocking: `Some` if this process now owns file watching for the project.
    pub fn try_watch_lock(&self) -> Result<Option<WatchGuard>> {
        let f = self.open_lock("watch.lock")?;
        Ok(f.try_lock().is_ok().then_some(WatchGuard { _file: f }))
    }

    // ------------------------------------------------------------ snapshots
    /// Map the current snapshot. Retries if the files vanish between reading the manifest and
    /// locking them (a concurrent switch + reclaim).
    pub fn open_snapshot(&self) -> Result<Snapshot> {
        let mut last = CgError::NoIndex(self.root.clone());
        for _ in 0..5 {
            let Some(m) = self.read_manifest()? else {
                return Err(CgError::NoIndex(self.root.clone()));
            };
            if m.format_version != brand::FORMAT_VERSION {
                return Err(CgError::Version(format!("index format {} (this build: {})", m.format_version, brand::FORMAT_VERSION)));
            }
            match self.open_manifest(&m) {
                Ok(s) => return Ok(s),
                Err(e) => {
                    last = e;
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        Err(last)
    }

    pub fn open_manifest(&self, m: &Manifest) -> Result<Snapshot> {
        let mut locks = Vec::new();
        let base_lock = self.open_lock(&format!("{}.lock", m.base))?;
        base_lock.lock_shared()?;
        locks.push(base_lock);
        let base = Arc::new(Seg::open(&self.dir.join(&m.base))?);
        let mut view = View::new(base).with_root(self.root.clone());
        if let Some(d) = &m.delta {
            let l = self.open_lock(&format!("{d}.lock"))?;
            l.lock_shared()?;
            locks.push(l);
            let seg = Arc::new(Seg::open(&self.dir.join(d))?);
            seg.verify()?; // deltas are small: always verify (design doc 9.4)
            view = view.with_delta(seg)?;
        }
        Ok(Snapshot { view, manifest: m.clone(), _locks: locks })
    }

    // ----------------------------------------------------------- publishing
    fn write_seg(&self, seg: &Seg, name: &str, light: bool) -> Result<u64> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        let file = File::create(&tmp)?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
        let n = seg.write_into(&mut w)?;
        w.flush()?;
        sync_file(w.get_ref(), self.fsync, light)?;
        drop(w);
        std::fs::rename(&tmp, self.dir.join(name))?;
        Ok(n)
    }

    /// Publish a new base segment (dropping any delta) and switch the manifest to it.
    pub fn publish_base(&self, seg: &Seg, _guard: &WriteGuard) -> Result<Manifest> {
        self.create()?;
        let prev = self.read_manifest().ok().flatten();
        let base_gen = prev.as_ref().map_or(1, |m| m.base_gen + 1);
        let name = format!("base-{base_gen:06}.seg");
        self.write_seg(seg, &name, false)?;
        let m = Manifest {
            format_version: brand::FORMAT_VERSION,
            engine_version: brand::ENGINE_VERSION,
            generation: prev.as_ref().map_or(1, |m| m.generation + 1),
            base: name,
            base_gen,
            delta: None,
            delta_seq: 0,
            created_ms: prev.as_ref().map_or_else(now_ms, |m| m.created_ms),
            updated_ms: now_ms(),
            n_files: seg.meta.n_files,
            n_nodes: seg.meta.n_nodes,
            n_edges: seg.meta.n_edges,
            syncs_since_merge: 0,
        };
        self.write_manifest(&m, false)?;
        self.gc();
        Ok(m)
    }

    /// Publish a (replacement) delta layer on top of the current base.
    pub fn publish_delta(&self, delta: &Seg, _guard: &WriteGuard) -> Result<Manifest> {
        let prev = self.read_manifest()?.ok_or_else(|| CgError::NoIndex(self.root.clone()))?;
        let seq = prev.delta_seq + 1;
        let name = format!("delta-{:06}-{seq:04}.seg", prev.base_gen);
        self.write_seg(delta, &name, true)?;
        let m = Manifest {
            generation: prev.generation + 1,
            delta: Some(name),
            delta_seq: seq,
            updated_ms: now_ms(),
            n_files: delta.meta.n_files,
            n_nodes: delta.meta.n_nodes,
            n_edges: delta.meta.n_edges,
            syncs_since_merge: prev.syncs_since_merge + 1,
            ..prev
        };
        self.write_manifest(&m, true)?;
        self.gc();
        Ok(m)
    }

    /// Drop the delta layer: the base alone is up to date again.
    pub fn clear_delta(&self, _guard: &WriteGuard) -> Result<Manifest> {
        let prev = self.read_manifest()?.ok_or_else(|| CgError::NoIndex(self.root.clone()))?;
        if prev.delta.is_none() {
            return Ok(prev);
        }
        let base_meta = self.open_manifest(&Manifest { delta: None, ..prev.clone() })?.view.base.meta.clone();
        let m = Manifest {
            generation: prev.generation + 1,
            delta: None,
            updated_ms: now_ms(),
            n_files: base_meta.n_files,
            n_nodes: base_meta.n_nodes,
            n_edges: base_meta.n_edges,
            syncs_since_merge: 0,
            ..prev
        };
        self.write_manifest(&m, true)?;
        self.gc();
        Ok(m)
    }

    // ------------------------------------------------------------------- gc
    /// Delete segment files no manifest (current or previous) references and no reader holds.
    pub fn gc(&self) {
        let mut keep = std::collections::HashSet::new();
        for m in [self.read_manifest().ok().flatten(), self.read_prev_manifest()].into_iter().flatten() {
            keep.insert(m.base.clone());
            if let Some(d) = m.delta {
                keep.insert(d);
            }
        }
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !(name.starts_with("base-") || name.starts_with("delta-")) || !name.ends_with(".seg") || keep.contains(&name) {
                continue;
            }
            // A reader that maps the file holds a shared lock on its lock file; only reclaim when
            // we can take it exclusively (also what makes this correct on Windows).
            let lock = self.lock_path(&format!("{name}.lock"));
            let free = match OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&lock) {
                Ok(f) => f.try_lock().is_ok(),
                Err(_) => false,
            };
            if free {
                let _ = std::fs::remove_file(e.path());
                let _ = std::fs::remove_file(lock);
            }
        }
    }

    /// Remove half-written files left by a crashed writer.
    pub fn cleanup_tmp(&self) {
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(self.dir.join("tmp")) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::Part;
    use crate::seg::{SegMeta, SegParts};

    fn tiny(n: u32) -> Seg {
        let p = SegParts { out_off: Part::owned(vec![0, n]), ..Default::default() };
        let meta = SegMeta { kind: "base".into(), format_version: brand::FORMAT_VERSION, n_nodes: n, ..Default::default() };
        Seg::from_parts(meta, p)
    }

    #[test]
    fn publish_switch_and_gc_respects_readers() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = Store::at(dir.path());
        st.fsync = FsyncMode::Off;
        st.create().unwrap();
        assert!(st.read_manifest().unwrap().is_none());
        assert!(matches!(st.open_snapshot(), Err(CgError::NoIndex(_))));
        let g = st.write_lock(Duration::from_secs(1)).unwrap();
        let m1 = st.publish_base(&tiny(1), &g).unwrap();
        let snap1 = st.open_snapshot().unwrap();
        assert_eq!(snap1.manifest.generation, m1.generation);
        let m2 = st.publish_base(&tiny(2), &g).unwrap();
        let m3 = st.publish_base(&tiny(3), &g).unwrap();
        assert!(m3.generation > m2.generation);
        // snap1 still maps base-000001; it must survive two switches
        assert!(dir.path().join(".cgr").join(&m1.base).exists());
        assert_eq!(snap1.view.base.meta.n_nodes, 1);
        drop(snap1);
        st.gc();
        assert!(!dir.path().join(".cgr").join(&m1.base).exists(), "reclaimed once the reader is gone");
        assert!(dir.path().join(".cgr").join(&m2.base).exists(), "manifest.prev keeps the previous version");
        assert_eq!(st.open_snapshot().unwrap().view.base.meta.n_nodes, 3);
        assert_eq!(std::fs::read_to_string(dir.path().join(".cgr/.gitignore")).unwrap(), "*\n");
    }

    #[test]
    fn locks_elect_one_watcher_and_one_writer() {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::at(dir.path());
        st.create().unwrap();
        let w1 = st.try_watch_lock().unwrap();
        assert!(w1.is_some());
        assert!(st.try_watch_lock().unwrap().is_none(), "second watcher must not get the lock");
        drop(w1);
        assert!(st.try_watch_lock().unwrap().is_some(), "released when the holder goes away");
        let g = st.write_lock(Duration::from_millis(50)).unwrap();
        assert!(st.write_lock(Duration::from_millis(100)).is_err());
        drop(g);
        assert!(st.write_lock(Duration::from_millis(100)).is_ok());
    }
}
