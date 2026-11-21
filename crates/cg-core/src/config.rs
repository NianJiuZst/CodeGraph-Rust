//! Project configuration: `codegraph.json` (upstream-compatible) and `cgr.json`
//! (same fields, wins field by field). A malformed file only produces a warning,
//! never an error (design doc 4.5).

use crate::{brand, hash, kinds::Lang};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct ProjectConfig {
    /// Extra extension -> language mappings, e.g. `".tpl" -> Php`.
    pub extensions: BTreeMap<String, Lang>,
    /// gitignore-style patterns dropped from the index even if committed.
    pub exclude: Vec<String>,
    /// gitignore-style patterns forced back in, overriding `.gitignore`.
    pub include: Vec<String>,
    /// Paths that stay indexed but rank below first-party code.
    pub deprioritize: Vec<String>,
    /// Also index embedded git repositories.
    pub include_ignored: bool,
    /// Project-relative PHP web root for `$_SERVER['DOCUMENT_ROOT']` includes (`""` = repo root).
    pub web_root: Option<String>,
    /// Problems found while reading (shown once by `status`, logged otherwise).
    pub warnings: Vec<String>,
}

impl ProjectConfig {
    pub fn load(root: &Path) -> ProjectConfig {
        let mut cfg = ProjectConfig::default();
        for name in [brand::UPSTREAM_CONFIG_FILE, brand::CONFIG_FILE] {
            let path = root.join(name);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => cfg.merge(&v, name),
                Err(e) => cfg.warnings.push(format!("{name}: not valid JSON ({e}); ignored")),
            }
        }
        cfg
    }

    fn merge(&mut self, v: &Value, file: &str) {
        let Some(obj) = v.as_object() else {
            self.warnings.push(format!("{file}: top level must be an object; ignored"));
            return;
        };
        let strings = |key: &str, warnings: &mut Vec<String>| -> Option<Vec<String>> {
            let arr = obj.get(key)?;
            let Some(items) = arr.as_array() else {
                warnings.push(format!("{file}: `{key}` must be an array of strings; ignored"));
                return None;
            };
            Some(items.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
        };
        if let Some(x) = strings("exclude", &mut self.warnings) {
            self.exclude = x;
        }
        if let Some(x) = strings("include", &mut self.warnings) {
            self.include = x;
        }
        if let Some(x) = strings("deprioritize", &mut self.warnings) {
            self.deprioritize = x;
        }
        if let Some(b) = obj.get("includeIgnored").and_then(Value::as_bool) {
            self.include_ignored = b;
        }
        if let Some(wr) = obj.get("webRoot") {
            match wr.as_str() {
                Some(raw) => {
                    let norm = raw.replace('\\', "/").trim().trim_start_matches("./").trim_matches('/').to_owned();
                    let norm = if norm == "." { String::new() } else { norm };
                    if !norm.split('/').any(|seg| seg == "..") {
                        self.web_root = Some(norm);
                    } else {
                        self.warnings.push(format!("{file}: `webRoot` must stay inside the project; ignored"));
                    }
                }
                None => self.warnings.push(format!("{file}: `webRoot` must be a string; ignored")),
            }
        }
        if let Some(m) = obj.get("extensions") {
            match m.as_object() {
                Some(m) => {
                    for (ext, lang) in m {
                        let l = lang.as_str().map(Lang::parse).unwrap_or(Lang::Unknown);
                        if l == Lang::Unknown {
                            self.warnings.push(format!("{file}: extension `{ext}` maps to unknown language {lang}; ignored"));
                        } else {
                            let ext = if ext.starts_with('.') { ext.clone() } else { format!(".{ext}") };
                            self.extensions.insert(ext.to_ascii_lowercase(), l);
                        }
                    }
                }
                None => self.warnings.push(format!("{file}: `extensions` must be an object; ignored")),
            }
        }
    }

    /// Stable digest of everything that changes which files are indexed or how they rank.
    pub fn digest(&self) -> u64 {
        let mut s = String::new();
        for (k, v) in &self.extensions {
            s.push_str(&format!("x:{k}={}\n", v.as_str()));
        }
        for (tag, list) in [("e", &self.exclude), ("i", &self.include), ("d", &self.deprioritize)] {
            for p in list {
                s.push_str(&format!("{tag}:{p}\n"));
            }
        }
        s.push_str(&format!("ii:{}", self.include_ignored));
        if let Some(wr) = &self.web_root {
            s.push_str(&format!("\nwr:{wr}"));
        }
        hash::hash64(s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgr_json_wins_and_bad_fields_only_warn() {
        let dir = std::env::temp_dir().join(format!("cg-core-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("codegraph.json"), r#"{"exclude":["a/"],"extensions":{".tpl":"php","bad":"klingon"},"deprioritize":"oops"}"#).unwrap();
        std::fs::write(dir.join("cgr.json"), r#"{"exclude":["b/"]}"#).unwrap();
        let cfg = ProjectConfig::load(&dir);
        assert_eq!(cfg.exclude, vec!["b/".to_string()]);
        assert_eq!(cfg.extensions.get(".tpl"), Some(&Lang::Php));
        assert_eq!(cfg.warnings.len(), 2, "{:?}", cfg.warnings);
        std::fs::remove_dir_all(&dir).ok();
    }
}
