//! Parallel file discovery (design doc 4.5): no `git` process on the hot path, ordering
//! by path bytes (the root of global determinism), size cap, supported languages only.

use crate::lang;
use crate::rules::IgnoreRules;
use cg_core::{config::ProjectConfig, Lang};
use ignore::{WalkBuilder, WalkState};
use rustc_hash::FxHashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Files over this size are not extracted (upstream `MAX_SOURCE_FILE_SIZE_BYTES`).
pub const MAX_SOURCE_FILE_SIZE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the project root, `/`-separated on every platform.
    pub rel: String,
    pub lang: Lang,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub max_file_size: u64,
    /// Also add committed-but-ignored files via `git ls-files` (full scans only).
    pub tracked_ignored: bool,
}

impl ScanOptions {
    pub fn new(root: impl Into<PathBuf>) -> ScanOptions {
        let root = root.into();
        let config = ProjectConfig::load(&root);
        ScanOptions { root, config, max_file_size: MAX_SOURCE_FILE_SIZE_BYTES, tracked_ignored: true }
    }
}

#[derive(Debug, Default)]
pub struct ScanResult {
    /// Sorted by path bytes.
    pub files: Vec<FileEntry>,
    /// Project configuration manifests discovered during the same walk (depth <= 8), sorted.
    pub config_files: Vec<String>,
    /// Over the size cap (not extracted).
    pub too_large: Vec<String>,
    /// Source files in languages without a walker in this build.
    pub unsupported: usize,
    pub warnings: Vec<String>,
}

pub fn is_config_filename(name: &str) -> bool {
    matches!(
        name,
        "package.json"
            | "jsconfig.json"
            | "go.mod"
            | "go.work"
            | "Cargo.toml"
            | "pyproject.toml"
            | "setup.cfg"
            | "setup.py"
            | "requirements.txt"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "settings.gradle"
            | "settings.gradle.kts"
            | "composer.json"
    ) || (name.starts_with("tsconfig") && name.ends_with(".json"))
        || (name.starts_with("requirements") && name.ends_with(".txt"))
}

const CONFIG_MAX_DEPTH: usize = 8;

fn within_config_depth(rel: &str, max_depth: usize) -> bool {
    // WalkBuilder max_depth(8): root-level file has depth 1 (0 slashes), depth 8 has 7 slashes.
    rel.bytes().filter(|&b| b == b'/').count() < max_depth
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

pub fn rel_string(root: &Path, p: &Path) -> Option<String> {
    let rel = p.strip_prefix(root).ok()?;
    let s = rel.to_str()?;
    #[cfg(windows)]
    {
        return Some(s.replace('\\', "/"));
    }
    #[cfg(not(windows))]
    {
        Some(s.to_owned())
    }
}

enum Verdict {
    Take(FileEntry),
    TooLarge,
    Unsupported,
    Skip,
}

fn judge(rel: &str, meta: &std::fs::Metadata, cfg: &ProjectConfig, max: u64) -> Verdict {
    if !meta.is_file() {
        return Verdict::Skip;
    }
    let l = lang::detect_from_path(rel, &cfg.extensions);
    if l == Lang::Unknown {
        return Verdict::Skip;
    }
    if !l.is_supported() {
        return Verdict::Unsupported;
    }
    if meta.len() > max {
        return Verdict::TooLarge;
    }
    Verdict::Take(FileEntry { rel: rel.to_owned(), lang: l, size: meta.len(), mtime_ns: mtime_ns(meta) })
}

/// Stat a single path the way the scan would (used by incremental sync and the watcher).
/// `None` = not an indexed file (ignored, unsupported, missing, too large).
pub fn stat_entry(rules: &IgnoreRules, cfg: &ProjectConfig, rel: &str) -> Option<FileEntry> {
    let abs = rules.root().join(rel);
    let meta = std::fs::symlink_metadata(&abs).ok()?;
    if meta.file_type().is_symlink() {
        return None;
    }
    if rules.is_ignored(rel, false) && !(rules.has_include() && rules.matches_include(rel, false) && !rules.builtin_ignored(rel, false)) {
        return None;
    }
    match judge(rel, &meta, cfg, MAX_SOURCE_FILE_SIZE_BYTES) {
        Verdict::Take(e) => Some(e),
        _ => None,
    }
}

#[derive(Default)]
struct Collected {
    files: Vec<FileEntry>,
    config_files: Vec<String>,
    too_large: Vec<String>,
    unsupported: usize,
}

struct Sink {
    local: Collected,
    out: Arc<Mutex<Vec<Collected>>>,
}

impl Drop for Sink {
    fn drop(&mut self) {
        let c = std::mem::take(&mut self.local);
        self.out.lock().unwrap().push(c);
    }
}

fn walk(root: &Path, rules: Arc<IgnoreRules>, cfg: &ProjectConfig, max: u64, only_included: bool) -> Vec<Collected> {
    let out: Arc<Mutex<Vec<Collected>>> = Arc::new(Mutex::new(Vec::new()));
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(false).follow_links(false);
    let walker = builder.build_parallel();
    let cfg = Arc::new(cfg.clone());
    let root_buf = root.to_path_buf();
    walker.run(|| {
        let mut sink = Sink { local: Collected::default(), out: out.clone() };
        let rules = rules.clone();
        let cfg = cfg.clone();
        let root = root_buf.clone();
        Box::new(move |res| {
            let Ok(entry) = res else { return WalkState::Continue };
            let Some(rel) = rel_string(&root, entry.path()) else { return WalkState::Continue };
            if rel.is_empty() {
                return WalkState::Continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                let skip = if only_included { rules.builtin_ignored(&rel, true) } else { rules.is_entry_ignored(&rel, true) };
                return if skip { WalkState::Skip } else { WalkState::Continue };
            }
            if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                return WalkState::Continue;
            }
            // Cheap path-only filter BEFORE ignore evaluation or stat(): skip files that are
            // neither supported/unsupported source files nor project configuration manifests.
            let basename = rel.rsplit('/').next().unwrap_or(&rel);
            let is_cfg = within_config_depth(&rel, CONFIG_MAX_DEPTH) && is_config_filename(basename);
            let l = lang::detect_from_path(&rel, &cfg.extensions);
            if l == Lang::Unknown && !is_cfg {
                return WalkState::Continue;
            }
            let keep =
                if only_included { rules.matches_include(&rel, false) && !rules.builtin_ignored(&rel, false) } else { !rules.is_entry_ignored(&rel, false) };
            if !keep {
                return WalkState::Continue;
            }
            if is_cfg && entry.file_type().map(|t| t.is_file()).unwrap_or(true) {
                sink.local.config_files.push(rel.clone());
            }
            if l == Lang::Unknown {
                return WalkState::Continue;
            }
            if !l.is_supported() {
                sink.local.unsupported += 1;
                return WalkState::Continue;
            }
            let Ok(meta) = entry.metadata() else { return WalkState::Continue };
            if !meta.is_file() {
                return WalkState::Continue;
            }
            if meta.len() > max {
                sink.local.too_large.push(rel);
            } else {
                sink.local.files.push(FileEntry { rel, lang: l, size: meta.len(), mtime_ns: mtime_ns(&meta) });
            }
            WalkState::Continue
        })
    });
    Arc::try_unwrap(out).map(|m| m.into_inner().unwrap()).unwrap_or_default()
}

/// `git ls-files -z -c -i --exclude-standard`: tracked files that match an ignore rule.
fn git_tracked_ignored(root: &Path) -> Vec<String> {
    if !root.join(".git").exists() {
        return Vec::new();
    }
    let child = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "-c", "-i", "--exclude-standard"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return Vec::new() };
    let mut stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(buf) => {
            let _ = child.wait();
            buf.split(|b| *b == 0).filter(|s| !s.is_empty()).filter_map(|s| std::str::from_utf8(s).ok().map(str::to_owned)).collect()
        }
        Err(_) => {
            let _ = child.kill();
            Vec::new()
        }
    }
}

/// Project configuration files that influence resolution (`package.json`, `tsconfig*.json`,
/// `go.mod`, `Cargo.toml`, `pyproject.toml`, ...), found with the same ignore rules as the scan.
/// Returns sorted repo-relative paths.
pub fn find_config_files(root: &Path, config: &ProjectConfig, max_depth: usize) -> Vec<String> {
    let rules = IgnoreRules::new(root, config);
    let mut out = Vec::new();
    let mut ignored_candidates = FxHashSet::default();
    let walker = WalkBuilder::new(root).standard_filters(false).follow_links(false).max_depth(Some(max_depth)).build();
    for e in walker.flatten() {
        let Some(rel) = rel_string(root, e.path()) else { continue };
        if rel.is_empty() {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir && !e.file_type().is_some_and(|t| t.is_symlink()) && is_config_filename(e.file_name().to_str().unwrap_or("")) {
            if !rules.is_ignored(&rel, false) {
                out.push(rel);
            } else if !rules.builtin_ignored(&rel, false) {
                ignored_candidates.insert(rel);
            }
        }
    }
    // A checked-in manifest can be covered by a broad .gitignore intended for
    // generated files (for example, `**/package.json`). Source scanning already
    // revives tracked-but-ignored files; apply the same policy to configuration.
    // Built-in exclusions and explicit cgr excludes still win. Avoid a Git
    // subprocess when there are no ignored candidate manifests.
    if !ignored_candidates.is_empty() {
        for rel in git_tracked_ignored(root) {
            if !ignored_candidates.contains(&rel) {
                continue;
            }
            if std::fs::symlink_metadata(root.join(&rel)).is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink()) {
                out.push(rel);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn scan(opts: &ScanOptions) -> ScanResult {
    // Overlap `git ls-files` with the parallel filesystem walk so the subprocess never sits on
    // the critical path when the repo has many directories.
    let git_handle = opts.tracked_ignored.then(|| {
        let root = opts.root.clone();
        std::thread::spawn(move || git_tracked_ignored(&root))
    });
    let rules = Arc::new(IgnoreRules::new(&opts.root, &opts.config));
    let mut parts = walk(&opts.root, rules.clone(), &opts.config, opts.max_file_size, false);
    if rules.has_include() {
        parts.extend(walk(&opts.root, rules.clone(), &opts.config, opts.max_file_size, true));
    }
    let mut res = ScanResult { warnings: opts.config.warnings.clone(), ..Default::default() };
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut seen_cfg: FxHashSet<String> = FxHashSet::default();
    for c in parts {
        res.unsupported += c.unsupported;
        res.too_large.extend(c.too_large);
        for f in c.files {
            if seen.insert(f.rel.clone()) {
                res.files.push(f);
            }
        }
        for cfg in c.config_files {
            if seen_cfg.insert(cfg.clone()) {
                res.config_files.push(cfg);
            }
        }
    }
    if let Some(handle) = git_handle {
        let tracked = handle.join().unwrap_or_default();
        for rel in tracked {
            if rules.builtin_ignored(&rel, false) {
                continue;
            }
            let basename = rel.rsplit('/').next().unwrap_or(&rel);
            let is_cfg = within_config_depth(&rel, CONFIG_MAX_DEPTH) && is_config_filename(basename) && !seen_cfg.contains(&rel);
            let need_src = !seen.contains(&rel) && lang::detect_from_path(&rel, &opts.config.extensions) != Lang::Unknown;
            if !is_cfg && !need_src {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(opts.root.join(&rel)) else { continue };
            if meta.file_type().is_symlink() || !meta.is_file() {
                continue;
            }
            if is_cfg && seen_cfg.insert(rel.clone()) {
                res.config_files.push(rel.clone());
            }
            if need_src {
                if let Verdict::Take(e) = judge(&rel, &meta, &opts.config, opts.max_file_size) {
                    seen.insert(rel);
                    res.files.push(e);
                }
            }
        }
    }
    res.files.sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    res.config_files.sort();
    res.too_large.sort();
    res
}

#[allow(dead_code)]
fn _unused(_: SystemTime) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let path = dir.path().join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, c).unwrap();
        }
        dir
    }

    #[test]
    fn scan_orders_filters_and_counts() {
        let d = tree(&[
            ("b/z.py", "x = 1\n"),
            ("a/m.ts", "export const a = 1;\n"),
            ("a/n.go", "package a\n"),
            ("node_modules/pkg/index.js", "x"),
            ("README.md", "# hi"),
            ("lib/native.c", "int x;"),
            ("lib/app.dart", "void main() {}"),
            (".gitignore", "ignored.ts\n"),
            ("ignored.ts", "export {}"),
            ("src/App.kt", "class App"),
        ]);
        let mut opts = ScanOptions::new(d.path());
        opts.tracked_ignored = false;
        let r = scan(&opts);
        let names: Vec<&str> = r.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(names, vec!["a/m.ts", "a/n.go", "b/z.py", "lib/native.c", "src/App.kt"]);
        assert_eq!(r.unsupported, 1, "the .dart file has no walker in this build");
    }

    #[test]
    fn size_cap_and_include() {
        let big = "x".repeat(2 * 1024 * 1024);
        let d = tree(&[("big.js", &big), ("Tools/t.ts", "export {}"), (".gitignore", "Tools/\n"), ("cgr.json", r#"{"include":["Tools/"]}"#)]);
        let mut opts = ScanOptions::new(d.path());
        opts.tracked_ignored = false;
        let r = scan(&opts);
        assert_eq!(r.too_large, vec!["big.js".to_string()]);
        let names: Vec<&str> = r.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(names, vec!["Tools/t.ts"]);
    }

    #[test]
    fn stat_entry_matches_scan() {
        let d = tree(&[("a/m.ts", "export {}"), ("node_modules/x.js", "1")]);
        let opts = ScanOptions::new(d.path());
        let rules = IgnoreRules::new(d.path(), &opts.config);
        assert!(stat_entry(&rules, &opts.config, "a/m.ts").is_some());
        assert!(stat_entry(&rules, &opts.config, "node_modules/x.js").is_none());
        assert!(stat_entry(&rules, &opts.config, "missing.ts").is_none());
    }
}
