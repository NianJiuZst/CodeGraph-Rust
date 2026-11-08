//! Project-level configuration that changes how names resolve (design doc 6.1): TypeScript
//! path mappings, workspace packages, Go modules, Cargo crates, plus the dependency lists
//! framework detection reads. Parsed once per index run; the file hashes go into the segment
//! so incremental sync notices when one of them changes.

use cg_core::{config::ProjectConfig, hash};
use cg_scan::find_config_files;
use rustc_hash::FxHashSet;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct TsConfig {
    /// Directory of the tsconfig, repo-relative ("" = root).
    pub dir: String,
    /// `baseUrl` resolved to a repo-relative directory.
    pub base_url: Option<String>,
    /// (pattern, targets): `"@/*" -> ["src/*"]`, targets repo-relative.
    pub paths: Vec<(String, Vec<String>)>,
}

#[derive(Clone, Debug, Default)]
pub struct PackageJson {
    pub dir: String,
    pub name: Option<String>,
    /// Entry files declared by `exports["."]`, `main`, `module`, `types`, repo-relative.
    pub entries: Vec<String>,
    pub deps: FxHashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct GoMod {
    pub dir: String,
    pub module: String,
}

#[derive(Clone, Debug, Default)]
pub struct CargoCrate {
    pub dir: String,
    /// Package name with `-` replaced by `_`.
    pub name: String,
    /// Crate root files, repo-relative (`src/lib.rs`, `src/main.rs`, `[lib] path`).
    pub roots: Vec<String>,
    pub deps: FxHashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub tsconfigs: Vec<TsConfig>,
    pub packages: Vec<PackageJson>,
    pub go_mods: Vec<GoMod>,
    /// `require` lines of every go.mod (module paths).
    pub go_mod_deps: Vec<String>,
    pub crates: Vec<CargoCrate>,
    pub py_deps: FxHashSet<String>,
    pub jvm_deps_text: String,
    pub config_hashes: BTreeMap<String, u64>,
    pub warnings: Vec<String>,
}

fn join_rel(dir: &str, rel: &str) -> String {
    normalize(&if dir.is_empty() { rel.to_owned() } else { format!("{dir}/{rel}") })
}

/// Collapse `.` / `..` / `//` in a repo-relative path.
pub fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// How close two project paths are: minus the number of directory steps between their
/// folders. Copies of one declaration in several source roots (`guava/src/...`,
/// `android/guava/src/...`) are told apart by it; `guava-tests/test` is nearer the first.
pub fn path_affinity(a: &str, b: &str) -> i64 {
    let (a, b) = (parent_dir(a), parent_dir(b));
    let depth = |p: &str| if p.is_empty() { 0 } else { p.split('/').count() };
    let common = a.split('/').zip(b.split('/')).take_while(|(x, y)| x == y && !x.is_empty()).count();
    -((depth(a) - common + depth(b) - common) as i64)
}

pub fn parent_dir(p: &str) -> &str {
    p.rfind('/').map(|i| &p[..i]).unwrap_or("")
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    jsonc_parser::parse_to_serde_value(&text, &Default::default()).ok().flatten()
}

impl ProjectInfo {
    pub fn load(root: &Path, config: &ProjectConfig) -> ProjectInfo {
        let files = find_config_files(root, config, 8);
        Self::load_known(root, &files)
    }

    /// A watcher already knows the configuration file set. Ordinary source edits need no
    /// directory walk to rediscover it; configuration and directory events use `load`.
    pub fn load_known(root: &Path, files: &[String]) -> ProjectInfo {
        let mut info = ProjectInfo { root: root.to_path_buf(), ..Default::default() };
        for rel in files {
            let abs = root.join(rel);
            let Ok(bytes) = std::fs::read(&abs) else { continue };
            info.config_hashes.insert(rel.clone(), hash::hash64(&bytes));
            let dir = parent_dir(rel).to_owned();
            let name = rel.rsplit('/').next().unwrap_or(rel);
            let text = String::from_utf8_lossy(&bytes);
            match name {
                "package.json" => {
                    if let Some(v) = read_json(&abs) {
                        info.packages.push(parse_package(&dir, &v));
                    }
                }
                "go.mod" => {
                    for l in text.lines() {
                        let t = l.trim();
                        let t = t.strip_prefix("require ").map(str::trim).unwrap_or(t);
                        if !t.starts_with("module ") && !t.starts_with("//") && t.contains('/') {
                            info.go_mod_deps.push(t.split_whitespace().next().unwrap_or("").to_owned());
                        }
                    }
                    if let Some(m) = text.lines().find_map(|l| l.trim().strip_prefix("module ")) {
                        info.go_mods.push(GoMod { dir, module: m.trim().trim_matches('"').to_owned() });
                    }
                }
                "Cargo.toml" => {
                    if let Some(c) = parse_cargo(&dir, &text) {
                        info.crates.push(c);
                    }
                }
                "pyproject.toml" | "setup.cfg" | "setup.py" => collect_py_deps(&text, &mut info.py_deps),
                n if n.starts_with("requirements") && n.ends_with(".txt") => collect_py_deps(&text, &mut info.py_deps),
                "pom.xml" | "build.gradle" | "build.gradle.kts" => {
                    info.jvm_deps_text.push_str(&text);
                    info.jvm_deps_text.push('\n');
                }
                n if n.starts_with("tsconfig") || n == "jsconfig.json" => {
                    if let Some(tc) = parse_tsconfig(root, rel, 0) {
                        info.tsconfigs.push(tc);
                    }
                }
                _ => {}
            }
        }
        // deepest directory first so lookups pick the most specific config
        info.tsconfigs.sort_by(|a, b| b.dir.len().cmp(&a.dir.len()).then(a.dir.cmp(&b.dir)));
        info.go_mods.sort_by(|a, b| b.dir.len().cmp(&a.dir.len()).then(a.dir.cmp(&b.dir)));
        info.crates.sort_by(|a, b| b.dir.len().cmp(&a.dir.len()).then(a.dir.cmp(&b.dir)));
        info
    }

    /// The tsconfig governing a file (deepest ancestor directory), if any.
    pub fn tsconfig_for(&self, rel: &str) -> Option<&TsConfig> {
        let dir = parent_dir(rel);
        self.tsconfigs.iter().find(|t| t.dir.is_empty() || dir == t.dir || dir.starts_with(&format!("{}/", t.dir)))
    }

    pub fn go_mod_for(&self, rel: &str) -> Option<&GoMod> {
        let dir = parent_dir(rel);
        self.go_mods.iter().find(|g| g.dir.is_empty() || dir == g.dir || dir.starts_with(&format!("{}/", g.dir)))
    }

    pub fn crate_for(&self, rel: &str) -> Option<&CargoCrate> {
        self.crates.iter().find(|c| c.dir.is_empty() || rel.starts_with(&format!("{}/", c.dir)))
    }

    pub fn has_dep(&self, name: &str) -> bool {
        self.packages.iter().any(|p| p.deps.contains(name))
            || self.py_deps.contains(&name.to_ascii_lowercase())
            || self.crates.iter().any(|c| c.deps.contains(name))
    }

    /// Frameworks detected from manifests (design doc 6.7), sorted.
    pub fn detect_frameworks(&self) -> Vec<String> {
        let mut v: Vec<&str> = Vec::new();
        let mut add = |cond: bool, name: &'static str| {
            if cond {
                v.push(name);
            }
        };
        let npm = |name: &str| self.packages.iter().any(|p| p.name.as_deref() == Some(name) || p.deps.contains(name));
        add(npm("express"), "express");
        add(npm("react") || npm("react-dom"), "react");
        add(npm("next"), "next");
        add(self.packages.iter().any(|p| p.deps.iter().any(|d| d.starts_with("@nestjs/"))), "nestjs");
        add(npm("react-router") || npm("react-router-dom") || npm("react-router-native"), "react-router");
        add(
            ["@tanstack/react-router", "@tanstack/solid-router", "@tanstack/router", "@tanstack/react-start", "@tanstack/start"].iter().any(|d| npm(d)),
            "tanstack-router",
        );
        add(self.py_deps.contains("fastapi"), "fastapi");
        add(self.py_deps.contains("flask"), "flask");
        add(self.py_deps.contains("django"), "django");
        add(self.go_mod_deps.iter().any(|d| d == "github.com/gin-gonic/gin") || self.go_mods.iter().any(|m| m.module == "github.com/gin-gonic/gin"), "gin");
        add(self.jvm_deps_text.contains("spring-boot") || self.jvm_deps_text.contains("springframework"), "spring");
        add(self.crates.iter().any(|c| c.name == "axum" || c.deps.contains("axum")), "axum");
        v.sort();
        v.dedup();
        v.into_iter().map(str::to_owned).collect()
    }
}

fn parse_package(dir: &str, v: &Value) -> PackageJson {
    let mut p = PackageJson { dir: dir.to_owned(), name: v.get("name").and_then(Value::as_str).map(str::to_owned), ..Default::default() };
    for key in ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"] {
        if let Some(m) = v.get(key).and_then(Value::as_object) {
            p.deps.extend(m.keys().cloned());
        }
    }
    let mut push = |s: &str| p.entries.push(join_rel(dir, s));
    fn first_string(v: &Value) -> Option<&str> {
        match v {
            Value::String(s) => Some(s),
            Value::Object(m) => ["types", "import", "module", "default", "require", "node"].iter().find_map(|k| m.get(*k).and_then(first_string)),
            _ => None,
        }
    }
    if let Some(e) = v.get("exports") {
        let root = e.get(".").unwrap_or(e);
        if let Some(s) = first_string(root) {
            push(s);
        }
    }
    for k in ["types", "typings", "module", "main"] {
        if let Some(s) = v.get(k).and_then(Value::as_str) {
            push(s);
        }
    }
    p
}

fn parse_cargo(dir: &str, text: &str) -> Option<CargoCrate> {
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    let name = doc.get("package")?.get("name")?.as_str()?.replace('-', "_");
    let mut c = CargoCrate { dir: dir.to_owned(), name, ..Default::default() };
    let lib = doc.get("lib").and_then(|l| l.get("path")).and_then(|p| p.as_str()).unwrap_or("src/lib.rs");
    c.roots.push(join_rel(dir, lib));
    c.roots.push(join_rel(dir, "src/main.rs"));
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(t) = doc.get(key).and_then(|t| t.as_table_like()) {
            c.deps.extend(t.iter().map(|(k, _)| k.replace('-', "_")));
        }
    }
    Some(c)
}

fn collect_py_deps(text: &str, out: &mut FxHashSet<String>) {
    // names at the start of requirement lines or quoted in dependency arrays
    for line in text.lines() {
        let t = line.trim().trim_matches(|c| c == '"' || c == '\'' || c == ',').trim();
        let name: String = t
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
            .collect::<String>()
            .to_ascii_lowercase()
            .replace('_', "-");
        if name.len() > 1 && !name.starts_with('-') && !name.starts_with('.') {
            out.insert(name.clone());
            out.insert(name.replace('-', "_"));
        }
    }
    // inline `dependencies = ["a>=1", "b"]`
    for m in text.split(['"', '\'']) {
        let name: String = m.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect::<String>().to_ascii_lowercase();
        if name.len() > 1 && m.len() < 80 {
            out.insert(name);
        }
    }
}

fn parse_tsconfig(root: &Path, rel: &str, depth: u32) -> Option<TsConfig> {
    if depth > 6 {
        return None;
    }
    let v = read_json(&root.join(rel))?;
    let dir = parent_dir(rel).to_owned();
    let mut out = TsConfig { dir: dir.clone(), ..Default::default() };
    // inherit from `extends` first so this file's options override
    let ext: Vec<String> = match v.get("extends") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
        _ => vec![],
    };
    for e in ext {
        if !(e.starts_with('.') || e.starts_with('/')) {
            continue; // package presets carry no path mappings we can see
        }
        let mut target = join_rel(&dir, &e);
        if !target.ends_with(".json") {
            target.push_str(".json");
        }
        if let Some(base) = parse_tsconfig(root, &target, depth + 1) {
            // relative targets of an inherited config were already made repo-relative
            out.base_url = base.base_url;
            out.paths = base.paths;
        }
    }
    let co = v.get("compilerOptions");
    let base_url = co.and_then(|c| c.get("baseUrl")).and_then(Value::as_str).map(|b| join_rel(&dir, b));
    if let Some(b) = &base_url {
        out.base_url = Some(b.clone());
    }
    if let Some(paths) = co.and_then(|c| c.get("paths")).and_then(Value::as_object) {
        let anchor = base_url.clone().or(out.base_url.clone()).unwrap_or_else(|| dir.clone());
        out.paths = paths
            .iter()
            .map(|(pat, targets)| {
                let ts = targets.as_array().map(|a| a.iter().filter_map(|t| t.as_str()).map(|t| join_rel(&anchor, t)).collect()).unwrap_or_default();
                (pat.clone(), ts)
            })
            .collect();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize("a/./b/../c//d"), "a/c/d");
        assert_eq!(normalize("../x"), "x");
        assert_eq!(parent_dir("a/b/c.ts"), "a/b");
        assert_eq!(parent_dir("c.ts"), "");
    }

    #[test]
    fn loads_configs_and_detects_frameworks() {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, c: &str| {
            let path = d.path().join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, c).unwrap();
        };
        w("package.json", r#"{"name":"app","main":"./dist/index.js","dependencies":{"express":"^4","react":"18"}}"#);
        w(
            "tsconfig.base.json",
            r#"{ // comment
          "compilerOptions": {"baseUrl": ".", "paths": {"@lib/*": ["packages/lib/src/*"]}}}"#,
        );
        w("web/tsconfig.json", r#"{"extends":"../tsconfig.base.json","compilerOptions":{"paths":{"@/*":["src/*"]}}}"#);
        w("go.mod", "module example.com/proj\n\ngo 1.22\n");
        w("crates/a/Cargo.toml", "[package]\nname = \"my-crate\"\n[dependencies]\naxum = \"0.7\"\n");
        w("requirements.txt", "Flask==3.0\nrequests>=2\n");
        let info = ProjectInfo::load(d.path(), &ProjectConfig::default());
        let web = info.tsconfig_for("web/src/a.ts").unwrap();
        // baseUrl "." is inherited from the base config, so `paths` resolve from the repo root
        assert_eq!(web.paths, vec![("@/*".to_string(), vec!["src/*".to_string()])]);
        let root = info.tsconfig_for("src/a.ts").unwrap();
        assert_eq!(root.paths[0].1, vec!["packages/lib/src/*".to_string()]);
        assert_eq!(info.go_mods[0].module, "example.com/proj");
        assert_eq!(info.crates[0].name, "my_crate");
        assert_eq!(info.packages[0].entries, vec!["dist/index.js".to_string()]);
        assert_eq!(info.detect_frameworks(), vec!["axum", "express", "flask", "react"]);
        assert!(info.config_hashes.contains_key("go.mod"));
    }
}
