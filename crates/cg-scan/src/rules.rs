//! Ignore rules shared by the full scan and the file watcher, so both agree on which
//! files are in the index (design doc 4.5, 9.1).
//!
//! Precedence, lowest first: built-in defaults < global excludes < `.git/info/exclude` <
//! `.gitignore` files from the root down < config `include` (revives user-ignored paths,
//! never built-in skips) < config `exclude` (always wins). A `!pattern` in a user ignore file
//! revives a built-in skip (`!vendor/`), as documented upstream.

use cg_core::config::ProjectConfig;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Directory names skipped everywhere (upstream `DEFAULT_IGNORE_DIRS`).
pub const DEFAULT_IGNORE_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    "jspm_packages",
    "web_modules",
    ".yarn",
    ".pnpm-store",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".vite",
    ".parcel-cache",
    ".angular",
    ".docusaurus",
    "storybook-static",
    ".vinxi",
    ".nitro",
    "out-tsc",
    ".vercel",
    ".netlify",
    ".wrangler",
    "dist",
    "build",
    "out",
    ".output",
    "coverage",
    ".nyc_output",
    "__pycache__",
    "__pypackages__",
    ".venv",
    "venv",
    ".pixi",
    ".pdm-build",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".hypothesis",
    ".ipynb_checkpoints",
    ".eggs",
    "target",
    ".gradle",
    "obj",
    "vendor",
    ".build",
    "Pods",
    "Carthage",
    "DerivedData",
    ".swiftpm",
    ".dart_tool",
    ".pub-cache",
    ".cxx",
    ".externalNativeBuild",
    "vcpkg_installed",
    ".bloop",
    ".metals",
    "lua_modules",
    ".luarocks",
    "__history",
    "__recovery",
    ".cache",
];

/// Android resource types whose directories hold XML, not code (upstream `ANDROID_RES_TYPES`).
pub const ANDROID_RES_TYPES: &[&str] =
    &["anim", "animator", "color", "drawable", "font", "layout", "menu", "mipmap", "navigation", "transition", "values", "xml"];

/// `build` is a legal JVM package segment under a conventional source root (#1642).
pub const JVM_BUILD_PACKAGE_DIRS: &[&str] = &["**/src/*/java/**/build/", "**/src/*/kotlin/**/build/", "**/src/*/scala/**/build/"];

fn default_patterns() -> Vec<String> {
    let mut v: Vec<String> = DEFAULT_IGNORE_DIRS.iter().map(|d| format!("{d}/")).collect();
    v.push("*.egg-info/".into());
    v.push("cmake-build-*/".into());
    v.push("bazel-*/".into());
    v.extend(ANDROID_RES_TYPES.iter().map(|t| format!("**/res/{t}*/")));
    v.extend(JVM_BUILD_PACKAGE_DIRS.iter().map(|d| format!("!{d}")));
    // Walk a `build` folder below `src/` too; only hand-written sources are kept (see
    // `build_dir_keeps_file`).
    v.push("!**/src/**/build/".into());
    v
}

/// Source files no build step writes into a `build` folder under `src/` (upstream
/// `HAND_WRITTEN_SOURCE`): TypeScript, JSX, Vue/Svelte/Astro, Go, C#, Rust.
pub fn is_hand_written_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".d.ts") || lower.ends_with(".d.mts") || lower.ends_with(".d.cts") || lower.ends_with(".d.ets") {
        return false;
    }
    [".ts", ".mts", ".cts", ".tsx", ".ets", ".jsx", ".vue", ".svelte", ".astro", ".go", ".cs", ".rs"].iter().any(|e| lower.ends_with(e))
}

pub struct IgnoreRules {
    root: PathBuf,
    defaults: Gitignore,
    exclude: Gitignore,
    has_exclude: bool,
    include: Gitignore,
    has_include: bool,
    jvm_build: Gitignore,
    /// Global excludes file + `.git/info/exclude`.
    base_user: Vec<Gitignore>,
    /// Per-directory `.gitignore`, loaded on first use (keyed by repo-relative dir path, `""` for root).
    nested: RwLock<FxHashMap<String, Option<Arc<Gitignore>>>>,
}

fn build(root: &Path, patterns: &[String]) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    for p in patterns {
        let _ = b.add_line(None, p);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

fn expand_tilde(s: &str) -> Option<PathBuf> {
    if let Some(rest) = s.strip_prefix("~/") {
        Some(PathBuf::from(std::env::var_os("HOME")?).join(rest))
    } else {
        Some(PathBuf::from(s))
    }
}

/// Fast-path reader for `[core] excludesFile` in a gitconfig file without spawning `git`.
/// Returns `None` if the file contains `[include` directives (caller falls back to `git config`).
fn parse_gitconfig_excludes(text: &str) -> Option<Option<String>> {
    let mut in_core = false;
    let mut found: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("[include") {
                return None;
            }
            in_core = lower.starts_with("[core]") || lower.starts_with("[core ");
            continue;
        }
        if in_core {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case("excludesfile") {
                    let mut val = v.trim();
                    if let Some(stripped) = val.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                        val = stripped;
                    } else {
                        if let Some(i) = val.find(['#', ';']) {
                            val = val[..i].trim();
                        }
                    }
                    if !val.is_empty() {
                        found = Some(val.to_owned());
                    }
                }
            }
        }
    }
    Some(found)
}

fn global_excludes_file() -> Option<PathBuf> {
    let xdg_cfg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let candidates: Vec<PathBuf> = if let Some(p) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        vec![PathBuf::from(p)]
    } else {
        let mut v = Vec::new();
        if let Some(xdg) = &xdg_cfg {
            v.push(xdg.join("git").join("config"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            v.push(PathBuf::from(home).join(".gitconfig"));
        }
        v
    };
    let mut need_git = false;
    let mut found: Option<String> = None;
    for cfg_path in &candidates {
        if let Ok(text) = std::fs::read_to_string(cfg_path) {
            match parse_gitconfig_excludes(&text) {
                Some(Some(val)) => found = Some(val),
                Some(None) => {}
                None => {
                    need_git = true;
                    break;
                }
            }
        }
    }
    if need_git {
        if let Some(out) = std::process::Command::new("git").args(["config", "--global", "core.excludesFile"]).output().ok().filter(|o| o.status.success()) {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return expand_tilde(&s);
            }
        }
    } else if let Some(s) = found {
        return expand_tilde(&s);
    }
    Some(xdg_cfg?.join("git").join("ignore"))
}

impl IgnoreRules {
    pub fn new(root: &Path, cfg: &ProjectConfig) -> IgnoreRules {
        let mut base_user = Vec::new();
        if let Some(g) = global_excludes_file().filter(|p| p.is_file()) {
            let mut b = GitignoreBuilder::new(root);
            let _ = b.add(g);
            if let Ok(gi) = b.build() {
                base_user.push(gi);
            }
        }
        let info = root.join(".git").join("info").join("exclude");
        if info.is_file() {
            let mut b = GitignoreBuilder::new(root);
            let _ = b.add(&info);
            if let Ok(gi) = b.build() {
                base_user.push(gi);
            }
        }
        IgnoreRules {
            root: root.to_path_buf(),
            defaults: build(root, &default_patterns()),
            exclude: build(root, &cfg.exclude),
            has_exclude: !cfg.exclude.is_empty(),
            include: build(root, &cfg.include),
            has_include: !cfg.include.is_empty(),
            jvm_build: build(root, &JVM_BUILD_PACKAGE_DIRS.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            base_user,
            nested: RwLock::new(FxHashMap::default()),
        }
    }

    fn dir_gitignore(&self, rel_dir: &str) -> Option<Arc<Gitignore>> {
        if let Ok(map) = self.nested.read() {
            if let Some(v) = map.get(rel_dir) {
                return v.clone();
            }
        }
        let dir = if rel_dir.is_empty() { self.root.clone() } else { self.root.join(rel_dir) };
        let file = dir.join(".gitignore");
        let loaded = if file.is_file() {
            let mut b = GitignoreBuilder::new(&dir);
            let _ = b.add(&file);
            b.build().ok().filter(|g| !g.is_empty()).map(Arc::new)
        } else {
            None
        };
        let mut map = self.nested.write().unwrap();
        map.entry(rel_dir.to_owned()).or_insert_with(|| loaded.clone()).clone()
    }

    /// State of `rel` (one path, not its parents) in the user layers: Some(true) = ignored,
    /// Some(false) = whitelisted by a `!` rule, None = no rule.
    fn user_state(&self, rel: &str, abs: &Path, is_dir: bool) -> Option<bool> {
        let mut state = None;
        for g in &self.base_user {
            match g.matched(abs, is_dir) {
                Match::Ignore(_) => state = Some(true),
                Match::Whitelist(_) => state = Some(false),
                Match::None => {}
            }
        }
        // .gitignore files from the root down to the path's directory; deeper wins.
        if let Some(g) = self.dir_gitignore("") {
            match g.matched(abs, is_dir) {
                Match::Ignore(_) => state = Some(true),
                Match::Whitelist(_) => state = Some(false),
                Match::None => {}
            }
        }
        let mut idx = 0;
        let bytes = rel.as_bytes();
        while let Some(off) = memchr::memchr(b'/', &bytes[idx..]) {
            let slash = idx + off;
            if slash > 0 {
                if let Some(g) = self.dir_gitignore(&rel[..slash]) {
                    match g.matched(abs, is_dir) {
                        Match::Ignore(_) => state = Some(true),
                        Match::Whitelist(_) => state = Some(false),
                        Match::None => {}
                    }
                }
            }
            idx = slash + 1;
        }
        state
    }

    fn default_state(&self, abs: &Path, is_dir: bool) -> Option<bool> {
        match self.defaults.matched(abs, is_dir) {
            Match::Ignore(_) => Some(true),
            Match::Whitelist(_) => Some(false),
            Match::None => None,
        }
    }

    /// Whether this single path is excluded by the rules (parents are *not* consulted).
    fn path_ignored(&self, rel: &str, is_dir: bool) -> bool {
        let abs = self.root.join(rel);
        if self.has_exclude && self.exclude.matched(&abs, is_dir).is_ignore() {
            return true;
        }
        let user = self.user_state(rel, &abs, is_dir);
        match user {
            Some(true) => !(self.has_include && self.include.matched(&abs, is_dir).is_ignore()),
            Some(false) => false, // explicitly revived, also revives built-in skips
            None => self.default_state(&abs, is_dir) == Some(true),
        }
    }

    /// Fast entry check for tree walkers that have already verified every ancestor directory is
    /// not ignored (so parents are not re-evaluated).
    pub fn is_entry_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if rel == ".git" || rel == cg_core::brand::DATA_DIR {
            return true;
        }
        if self.path_ignored(rel, is_dir) {
            return true;
        }
        if !is_dir && self.under_src_build(rel) && !self.in_jvm_build_package(rel) {
            return !is_hand_written_source(rel);
        }
        false
    }

    /// Is the repo-relative path (forward slashes) kept out of the index? A path inside an
    /// ignored directory is ignored. `is_dir` describes the last component.
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if rel == ".git" || rel.starts_with(".git/") || rel == cg_core::brand::DATA_DIR || rel.starts_with(".cgr/") {
            return true;
        }
        let mut idx = 0;
        let bytes = rel.as_bytes();
        while let Some(off) = memchr::memchr(b'/', &bytes[idx..]) {
            let slash = idx + off;
            if slash > 0 && self.path_ignored(&rel[..slash], true) {
                return true;
            }
            idx = slash + 1;
        }
        if !rel.is_empty() && !rel.ends_with('/') && self.path_ignored(rel, is_dir) {
            return true;
        }
        // A `build` directory below `src/` is walked, but only hand-written sources count.
        if !is_dir && self.under_src_build(rel) && !self.in_jvm_build_package(rel) {
            return !is_hand_written_source(rel);
        }
        false
    }

    fn under_src_build(&self, rel: &str) -> bool {
        if !rel.contains("/build/") && !rel.starts_with("src/build/") {
            return false;
        }
        let mut saw_src = false;
        let mut it = rel.split('/').peekable();
        while let Some(c) = it.next() {
            if it.peek().is_none() {
                break; // filename, not a directory component
            }
            if !saw_src {
                if c == "src" {
                    saw_src = true;
                }
            } else if c == "build" {
                return true;
            }
        }
        false
    }

    fn in_jvm_build_package(&self, rel: &str) -> bool {
        // Match the directory part of the path against the JVM build-package globs.
        let dir = match rel.rfind('/') {
            Some(i) => &rel[..i],
            None => return false,
        };
        let mut prefix = String::new();
        for c in dir.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(c);
            if c == "build" && self.jvm_build.matched(self.root.join(&prefix), true).is_ignore() {
                return true;
            }
        }
        false
    }

    /// Does `rel` match a config `include` pattern (used by the second, ignore-overriding walk)?
    pub fn matches_include(&self, rel: &str, is_dir: bool) -> bool {
        self.has_include && self.include.matched_path_or_any_parents(self.root.join(rel), is_dir).is_ignore()
    }

    pub fn has_include(&self) -> bool {
        self.has_include
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Does `rel` match config `exclude`?
    pub fn matches_exclude(&self, rel: &str, is_dir: bool) -> bool {
        self.has_exclude && self.exclude.matched_path_or_any_parents(self.root.join(rel), is_dir).is_ignore()
    }

    /// Built-in-only check used for `include`d and tracked-but-ignored files: only the
    /// default skips and config `exclude` apply.
    pub fn builtin_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if rel == ".git" || rel.starts_with(".git/") || rel == cg_core::brand::DATA_DIR || rel.starts_with(".cgr/") {
            return true;
        }
        if self.matches_exclude(rel, is_dir) {
            return true;
        }
        let mut idx = 0;
        let bytes = rel.as_bytes();
        while let Some(off) = memchr::memchr(b'/', &bytes[idx..]) {
            let slash = idx + off;
            if slash > 0 && self.default_state(&self.root.join(&rel[..slash]), true) == Some(true) {
                return true;
            }
            idx = slash + 1;
        }
        if !rel.is_empty() && !rel.ends_with('/') && self.default_state(&self.root.join(rel), is_dir) == Some(true) {
            return true;
        }
        if !is_dir && self.under_src_build(rel) && !self.in_jvm_build_package(rel) {
            return !is_hand_written_source(rel);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn rules(files: &[(&str, &str)], cfg: ProjectConfig) -> (tempfile::TempDir, IgnoreRules) {
        let dir = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let path = dir.path().join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, c).unwrap();
        }
        let r = IgnoreRules::new(dir.path(), &cfg);
        (dir, r)
    }

    #[test]
    fn defaults_and_gitignore() {
        let (_d, r) = rules(&[(".gitignore", "secret/\n*.log\n")], ProjectConfig::default());
        assert!(r.is_ignored("node_modules/a/b.js", false));
        assert!(r.is_ignored("a/vendor/x.go", false));
        assert!(r.is_ignored("secret/key.ts", false));
        assert!(r.is_ignored("a/b.log", false));
        assert!(!r.is_ignored("src/main.rs", false));
        assert!(!r.is_ignored(".github/workflows/ci.ts", false));
        assert!(r.is_ignored(".git/config", false));
    }

    #[test]
    fn negation_revives_builtin_skip() {
        let (_d, r) = rules(&[(".gitignore", "!vendor/\n")], ProjectConfig::default());
        assert!(!r.is_ignored("vendor/x.go", false));
        assert!(r.is_ignored("node_modules/x.js", false));
    }

    #[test]
    fn config_exclude_and_include() {
        let cfg = ProjectConfig { exclude: vec!["static/".into()], include: vec!["Tools/".into()], ..Default::default() };
        let (_d, r) = rules(&[(".gitignore", "Tools/\n")], cfg);
        assert!(r.is_ignored("static/a.js", false));
        assert!(!r.is_ignored("Tools/a.ts", false), "include revives a gitignored path");
        assert!(r.matches_include("Tools/a.ts", false));
    }

    #[test]
    fn build_dir_under_src() {
        let (_d, r) = rules(&[], ProjectConfig::default());
        assert!(r.is_ignored("build/out.js", false));
        assert!(!r.is_ignored("app/src/build/Gen.ts", false));
        assert!(r.is_ignored("app/src/build/gen.py", false), "python is copied into build/");
        assert!(!r.is_ignored("app/src/main/java/com/x/build/Util.java", false), "jvm package named build");
        assert!(r.is_ignored("app/src/build/types.d.ts", false));
        assert!(r.is_ignored("app/res/layout/main.xml", false));
    }
}
