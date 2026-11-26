//! File watching (design doc 9.1). One process per project holds `watch.lock` and runs the
//! watcher; the others poll the lock and take over within a few seconds if the holder dies.
//! Events are filtered by the indexing rules, debounced adaptively (300 ms of quiet for up to two
//! files, 2 s for more), then synced by path. A burst of more than 500 files, a config change
//! or a lost event triggers a full scan instead.

use crate::sync::{sync, SyncOptions};
use cg_core::brand;
use cg_core::config::ProjectConfig;
use cg_scan::IgnoreRules;
use cg_store::Store;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const QUIET_SMALL: Duration = Duration::from_millis(300);
const QUIET_LARGE: Duration = Duration::from_millis(2000);
const ELECTION_POLL: Duration = Duration::from_secs(2);
const FALLBACK_SCAN: Duration = Duration::from_secs(60);

pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WatchHandle {
    /// Stop all projects against one shared deadline, so eight watched projects do not
    /// multiply shutdown latency. In-progress atomic writes may finish or be abandoned.
    pub fn stop_all(mut handles: Vec<Self>, wait: Duration) {
        for h in &handles {
            h.stop.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now() + wait;
        loop {
            for h in &mut handles {
                if h.thread.as_ref().is_some_and(|t| t.is_finished()) {
                    if let Some(t) = h.thread.take() {
                        let _ = t.join();
                    }
                }
            }
            if handles.iter().all(|h| h.thread.is_none()) || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Ask the watcher to stop and wait (up to ~1 s) for it.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_millis(1500);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub(crate) fn is_config_name(name: &str) -> bool {
    matches!(
        name,
        ".gitignore"
            | "cgr.json"
            | "codegraph.json"
            | "package.json"
            | "go.mod"
            | "Cargo.toml"
            | "pyproject.toml"
            | "setup.py"
            | "setup.cfg"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "jsconfig.json"
    ) || name.starts_with("tsconfig")
        || (name.starts_with("requirements") && name.ends_with(".txt"))
}

pub(crate) fn is_git_inventory_path(rel: &Path) -> bool {
    rel == Path::new(".git/index") || rel == Path::new(".git/info/exclude")
}

struct Filter {
    root: PathBuf,
    rules: IgnoreRules,
    indexed: Option<cg_store::Snapshot>,
}

impl Filter {
    fn new(root: &Path) -> Filter {
        let config = ProjectConfig::load(root);
        Filter { root: root.to_path_buf(), rules: IgnoreRules::new(root, &config), indexed: Store::at(root).open_snapshot().ok() }
    }

    /// Does an event at `p` possibly change what the index says?
    fn relevant(&self, p: &Path) -> bool {
        let Ok(rel) = p.strip_prefix(&self.root) else { return false };
        if is_git_inventory_path(rel) {
            return true;
        }
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || rel == brand::DATA_DIR || rel.starts_with(&format!("{}/", brand::DATA_DIR)) || rel == ".git" || rel.starts_with(".git/") {
            return false;
        }
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        if is_config_name(name) {
            return true;
        }
        let is_dir = p.is_dir();
        if !self.rules.is_ignored(&rel, is_dir) {
            return true;
        }
        if self.rules.builtin_ignored(&rel, is_dir) {
            return false;
        }
        // The full scan revives tracked-but-ignored sources. Use its published
        // inventory here, including deleted files and their removed directories.
        let Some(indexed) = &self.indexed else { return false };
        if indexed.view.file_by_path(&rel).is_some() {
            return true;
        }
        let prefix = format!("{rel}/");
        !p.is_file() && indexed.view.files().any(|f| indexed.view.file(f).path().starts_with(&prefix))
    }
}

/// Start a watcher thread for the project at `root`. Returns immediately.
pub fn spawn_watcher(root: PathBuf, opts: SyncOptions) -> WatchHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let thread = std::thread::Builder::new().name("cgr-watch".into()).spawn(move || watch_loop(root, opts, s2)).ok();
    WatchHandle { stop, thread }
}

fn sleep_checking(stop: &AtomicBool, total: Duration) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::SeqCst) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn watch_loop(root: PathBuf, opts: SyncOptions, stop: Arc<AtomicBool>) {
    let store = Store::at(&root);
    while !stop.load(Ordering::SeqCst) {
        let guard = match store.try_watch_lock() {
            Ok(Some(g)) => g,
            _ => {
                // another process watches; take over when it goes away
                sleep_checking(&stop, ELECTION_POLL);
                continue;
            }
        };
        tracing::debug!("watching {}", root.display());
        run_watch(&root, &opts, &stop);
        drop(guard);
    }
}

fn run_sync(root: &Path, opts: &SyncOptions) {
    match sync(root, opts) {
        Ok(r) if !r.up_to_date => tracing::info!("{}", r.summary()),
        Ok(_) => {}
        Err(e) => tracing::warn!("sync failed: {e}"),
    }
}

fn run_watch(root: &Path, opts: &SyncOptions, stop: &AtomicBool) {
    let (tx, rx) = mpsc::sync_channel::<notify::Result<Event>>(1024);
    let overflow = Arc::new(AtomicBool::new(false));
    let over = overflow.clone();
    let watcher = notify::recommended_watcher(move |res| {
        if tx.try_send(res).is_err() {
            over.store(true, Ordering::Relaxed);
        }
    });
    let mut watcher = match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!("file watching unavailable ({e}); falling back to periodic scans");
            None
        }
    };
    let mut degraded = watcher.is_none();
    if let Some(w) = watcher.as_mut() {
        if let Err(e) = w.watch(root, RecursiveMode::Recursive) {
            tracing::warn!("cannot watch {} ({e}); falling back to periodic scans", root.display());
            degraded = true;
        }
    }
    // Register first, then catch up: edits made while the initial scan runs must
    // remain queued rather than falling into a scan/watch registration gap.
    run_sync(root, opts);
    let mut filter = Filter::new(root);
    let mut dirty: HashSet<PathBuf> = HashSet::new();
    let mut full_scan = false;
    let mut last_event = Instant::now();
    let mut burst_started = None;
    let mut last_maintenance = Instant::now();
    let mut last_scan = Instant::now();
    let store = Store::at(root);
    while !stop.load(Ordering::SeqCst) {
        if overflow.swap(false, Ordering::Relaxed) {
            full_scan = true;
            burst_started.get_or_insert_with(Instant::now);
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(ev)) => {
                if ev.need_rescan() {
                    full_scan = true;
                    burst_started.get_or_insert_with(Instant::now);
                }
                if !matches!(ev.kind, EventKind::Access(_)) {
                    for p in ev.paths {
                        if p.strip_prefix(root).is_ok_and(is_git_inventory_path) {
                            full_scan = true;
                        }
                        if p.file_name().and_then(|n| n.to_str()).is_some_and(is_config_name) {
                            filter = Filter::new(root);
                        }
                        if filter.relevant(&p) {
                            burst_started.get_or_insert_with(Instant::now);
                            if dirty.len() < 500 {
                                dirty.insert(p);
                            } else {
                                full_scan = true;
                            }
                            last_event = Instant::now();
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("watcher error: {e}");
                degraded = true;
                full_scan = true;
                burst_started.get_or_insert_with(Instant::now);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !dirty.is_empty() || full_scan {
            let quiet = if dirty.len() <= 2 { QUIET_SMALL } else { QUIET_LARGE };
            if last_event.elapsed() >= quiet || burst_started.is_some_and(|t| t.elapsed() >= Duration::from_secs(5)) {
                let mut local = opts.clone();
                local.paths = if full_scan { None } else { Some(dirty.drain().collect()) };
                dirty.clear();
                full_scan = false;
                burst_started = None;
                run_sync(root, &local);
                filter = Filter::new(root);
                last_scan = Instant::now();
            }
        } else if degraded && last_scan.elapsed() >= FALLBACK_SCAN.min(Duration::from_secs(5)) {
            run_sync(root, opts);
            filter = Filter::new(root);
            last_scan = Instant::now();
        } else if last_scan.elapsed() >= FALLBACK_SCAN * 5 {
            // slow safety net: events can be dropped (inotify queue overflow, network drives)
            run_sync(root, opts);
            filter = Filter::new(root);
            last_scan = Instant::now();
        }
        if dirty.is_empty() && !full_scan && last_event.elapsed() >= Duration::from_secs(30) && last_maintenance.elapsed() >= Duration::from_secs(30) {
            if let Ok(report) = crate::compact_if_due(root) {
                if report.compacted {
                    tracing::info!("compacted idle snapshot");
                    filter = Filter::new(root);
                }
            }
            last_maintenance = Instant::now();
        }
        // the index may have been removed (`cgr uninit`): stop watching
        if store.read_manifest().ok().flatten().is_none() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cg_resolve::{index_project, IndexOptions};
    use std::process::Command;

    #[test]
    fn filter_keeps_indexed_ignored_paths_and_their_removed_parents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("ignored")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/dep")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(root.join("cgr.json"), r#"{"exclude":["ignored/excluded.ts"]}"#).unwrap();
        for rel in ["ignored/tracked.ts", "ignored/excluded.ts", "ignored/untracked.ts", "node_modules/dep/index.ts"] {
            std::fs::write(root.join(rel), "export function run() {}\n").unwrap();
        }
        for args in [vec!["init", "--quiet"], vec!["add", "-f", "ignored/tracked.ts", "ignored/excluded.ts", "node_modules/dep/index.ts"]] {
            let result = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        }
        index_project(root, &IndexOptions { threads: Some(2), ..Default::default() }).unwrap();
        let filter = Filter::new(root);
        assert!(filter.relevant(&root.join("ignored/tracked.ts")));
        for rel in ["ignored/untracked.ts", "ignored/excluded.ts", "node_modules/dep/index.ts", ".git/objects/ab/object", ".cgr/manifest"] {
            assert!(!filter.relevant(&root.join(rel)), "irrelevant: {rel}");
        }
        assert!(filter.relevant(&root.join(".git/index")), "Git inventory changes need a full scan");
        assert!(filter.relevant(&root.join(".git/info/exclude")), "Git ignore changes need a full scan");
        std::fs::remove_dir_all(root.join("ignored")).unwrap();
        assert!(filter.relevant(&root.join("ignored/tracked.ts")), "deleted tracked source");
        assert!(filter.relevant(&root.join("ignored")), "deleted ignored parent");
    }
}
