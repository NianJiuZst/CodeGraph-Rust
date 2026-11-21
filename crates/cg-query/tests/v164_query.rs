//! End-to-end tests for v1.6.4 query capabilities and bug fixes:
//! - One-symbol focused `explore` mode (#3407) + fallback rules
//! - Same-package symbol dependencies & `--cross-tier` gating in `affected` (#3320)
//! - `files --format package` manifest resolution + `testsOnly` / `excludeTests` filters
//! - `implementations --depth`
//! - `callers` callback-registration (`fnRef`) labeling and stale-file warnings

use cg_query::explore::ExploreOptions;
use cg_query::tools::{self, ToolEnv};
use cg_query::Project;
use cg_resolve::{index_project, IndexOptions};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, text).unwrap();
}

fn index_opts() -> IndexOptions {
    IndexOptions { threads: Some(2), ..Default::default() }
}

#[test]
fn one_symbol_explore_returns_definition_and_sites_and_falls_back_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"demo"}"#);
    write(
        root,
        "src/format.ts",
        "export function helperTag(s: string): string {\n  return `[${s}]`;\n}\nexport function formatInvoice(id: string): string {\n  return helperTag(id);\n}\n",
    );
    write(
        root,
        "src/service.ts",
        "import { formatInvoice } from './format';\nexport class InvoiceService {\n  render(id: string): string {\n    return formatInvoice(id);\n  }\n}\n",
    );
    write(
        root,
        "tests/service.test.ts",
        "import { formatInvoice } from '../src/format';\nexport function testFormat(): string {\n  return formatInvoice('42');\n}\n",
    );
    index_project(root, &index_opts()).unwrap();
    let proj = Project::locate(root).unwrap();
    let g = &proj.graph;

    // Bare single symbol -> focused one-symbol answer with verbatim `<n>\t<line>` body, callers, tests, callees.
    let r = g.explore("formatInvoice", &ExploreOptions::default());
    assert!(r.found_named);
    assert_eq!(r.symbols, 1);
    assert!(r.text.contains("**`formatInvoice`** (function) — defined at `src/format.ts:4`"), "{}", r.text);
    assert!(r.text.contains("4\texport function formatInvoice"), "{}", r.text);
    assert!(r.text.contains("**Called by**"), "{}", r.text);
    assert!(r.text.contains("`src/service.ts:4` (`InvoiceService.render`): `return formatInvoice(id);`"), "{}", r.text);
    assert!(r.text.contains("**In tests**"), "{}", r.text);
    assert!(r.text.contains("tests/service.test.ts"), "{}", r.text);
    assert!(r.text.contains("**Calls** `helperTag`"), "{}", r.text);

    // Question phrasing with one symbol -> same focused mode.
    let q = g.explore("who calls formatInvoice?", &ExploreOptions::default());
    assert!(q.text.contains("**`formatInvoice`** (function)"), "{}", q.text);
    assert!(q.text.contains("**Called by**"), "{}", q.text);

    // Multi-symbol query -> falls back to general explore.
    let multi = g.explore("formatInvoice helperTag", &ExploreOptions::default());
    assert!(multi.text.contains("**Exploration:"), "{}", multi.text);

    // Explicit maxFiles -> falls back to general explore.
    let explicit_max = g.explore("formatInvoice", &ExploreOptions { max_files: Some(3), ..Default::default() });
    assert!(explicit_max.text.contains("**Exploration:"), "{}", explicit_max.text);

    // Stale definition file on disk -> falls back to general explore with stale warning.
    let p = root.join("src/format.ts");
    let orig = std::fs::read_to_string(&p).unwrap();
    std::fs::write(&p, format!("// edited\n{orig}")).unwrap();
    let future = SystemTime::now() + Duration::from_secs(5);
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(future).unwrap();
    let stale = g.explore("formatInvoice", &ExploreOptions::default());
    assert!(stale.text.contains("**Exploration:"), "{}", stale.text);
    assert!(stale.text.contains("edited since the last index sync"), "{}", stale.text);
}

#[test]
fn affected_includes_same_package_deps_and_gates_cross_tier_edges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Go same-package dependency without import statements.
    write(root, "go.mod", "module example.com/demo\n\ngo 1.22\n");
    write(root, "pkg/calc.go", "package pkg\n\nfunc Compute(x int) int {\n\treturn x * 2\n}\n");
    write(root, "pkg/service.go", "package pkg\n\nfunc RunService(x int) int {\n\treturn Compute(x)\n}\n");
    write(root, "pkg/service_test.go", "package pkg\n\nfunc TestRunService() int {\n\treturn RunService(3)\n}\n");

    // Cross-tier HTTP server route <-> client fetch <-> client unit test.
    write(root, "package.json", r#"{"name":"fullstack","dependencies":{"express":"^4.19.0"}}"#);
    write(
        root,
        "server/routes.ts",
        "import express from 'express';\nconst app = express();\nexport function listItems(req: any, res: any) { res.json([]); }\napp.get('/api/items', listItems);\n",
    );
    write(root, "client/api.ts", "export async function fetchItems() {\n  return fetch('/api/items');\n}\n");
    write(root, "client/api.test.ts", "import { fetchItems } from './api';\nexport async function testFetchItems() {\n  return fetchItems();\n}\n");

    index_project(root, &index_opts()).unwrap();
    let proj = Project::locate(root).unwrap();
    let g = &proj.graph;

    // Go same-package file dependents and transitive affected tests.
    let calc = g.view.file_by_path("pkg/calc.go").unwrap();
    let direct = g.file_dependents(calc, false);
    let direct_paths: Vec<&str> = direct.iter().map(|f| g.path(*f)).collect();
    assert!(direct_paths.contains(&"pkg/service.go"), "{direct_paths:?}");

    let fg_default = g.dependent_files_graph(false);
    let affected_go: Vec<&str> = g.affected_files(&fg_default, &[calc], 5).into_iter().filter(|f| g.is_test_file(*f)).map(|f| g.path(f)).collect();
    assert!(affected_go.contains(&"pkg/service_test.go"), "{affected_go:?}");

    // Cross-tier HTTP boundary: excluded by default, included with cross_tier=true.
    let routes = g.view.file_by_path("server/routes.ts").unwrap();
    let no_tier: Vec<&str> = g.affected_files(&fg_default, &[routes], 5).into_iter().filter(|f| g.is_test_file(*f)).map(|f| g.path(f)).collect();
    assert!(!no_tier.contains(&"client/api.test.ts"), "default affected must not cross HTTP tier: {no_tier:?}");

    let fg_cross = g.dependent_files_graph(true);
    let with_tier: Vec<&str> = g.affected_files(&fg_cross, &[routes], 5).into_iter().filter(|f| g.is_test_file(*f)).map(|f| g.path(f)).collect();
    assert!(with_tier.contains(&"client/api.test.ts"), "--cross-tier should reach client test: {with_tier:?}");
}

#[test]
fn files_package_format_and_test_filters_work_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "packages/core/package.json", r#"{"name":"@acme/core"}"#);
    write(root, "packages/core/src/a.ts", "export function a() { return 1; }\n");
    write(root, "packages/core/src/a.test.ts", "import { a } from './a';\nexport function testA() { return a(); }\n");
    write(root, "crates/worker/Cargo.toml", "[package]\nname = \"worker-bin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(root, "crates/worker/src/lib.rs", "pub fn work() -> i32 { 1 }\n");

    index_project(root, &index_opts()).unwrap();
    let proj = Project::locate(root).unwrap();
    let env = ToolEnv { graph: &proj.graph, history: None, status: proj.status_info() };

    let pkg = tools::call(&env, "files", &json!({"format": "package"}));
    assert!(!pkg.is_error);
    assert!(pkg.text.contains("**@acme/core (packages/core) (2)**"), "{}", pkg.text);
    assert!(pkg.text.contains("**worker-bin (crates/worker) (1)**"), "{}", pkg.text);

    let tests_only = tools::call(&env, "files", &json!({"format": "flat", "testsOnly": true}));
    assert!(tests_only.text.contains("packages/core/src/a.test.ts"), "{}", tests_only.text);
    assert!(!tests_only.text.contains("packages/core/src/a.ts"), "{}", tests_only.text);
    assert!(!tests_only.text.contains("crates/worker/src/lib.rs"), "{}", tests_only.text);

    let no_tests = tools::call(&env, "files", &json!({"format": "flat", "excludeTests": true}));
    assert!(no_tests.text.contains("packages/core/src/a.ts"), "{}", no_tests.text);
    assert!(no_tests.text.contains("crates/worker/src/lib.rs"), "{}", no_tests.text);
    assert!(!no_tests.text.contains("a.test.ts"), "{}", no_tests.text);
}

#[test]
fn implementations_respects_depth_limit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"hier"}"#);
    write(
        root,
        "src/hier.ts",
        "export interface Base { run(): void; }\nexport class Mid implements Base { run() {} }\nexport class Leaf extends Mid { run() {} }\n",
    );
    index_project(root, &index_opts()).unwrap();
    let proj = Project::locate(root).unwrap();
    let g = &proj.graph;
    let base = g.resolve_symbol("Base")[0];

    let d1: Vec<&str> = g.implementations(base, 1).into_iter().map(|n| g.name(n)).collect();
    assert_eq!(d1, vec!["Mid"]);

    let d2: Vec<&str> = g.implementations(base, 2).into_iter().map(|n| g.name(n)).collect();
    assert_eq!(d2, vec!["Mid", "Leaf"]);
}

#[test]
fn callers_shows_callback_registration_and_stale_warning() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"cb"}"#);
    write(root, "src/cb.ts", "export function onClick() { return 1; }\nexport function register(fn: () => number) { return fn(); }\n");
    write(root, "src/app.ts", "import { onClick, register } from './cb';\nexport function wire() {\n  register(onClick);\n}\n");
    index_project(root, &index_opts()).unwrap();
    let proj = Project::locate(root).unwrap();
    let env = ToolEnv { graph: &proj.graph, history: None, status: proj.status_info() };

    let out = tools::call(&env, "callers", &json!({"symbol": "onClick"}));
    assert!(!out.is_error);
    assert!(out.text.contains("wire"), "{}", out.text);
    assert!(out.text.contains("via callback registration"), "expected fnRef label: {}", out.text);
    assert!(!out.text.contains("edited since the last index sync"), "{}", out.text);

    // Edit caller file on disk without re-syncing -> callers & impact warn about staleness.
    let app = root.join("src/app.ts");
    let orig = std::fs::read_to_string(&app).unwrap();
    std::fs::write(&app, format!("// changed\n{orig}")).unwrap();
    let future = SystemTime::now() + Duration::from_secs(5);
    std::fs::OpenOptions::new().write(true).open(&app).unwrap().set_modified(future).unwrap();

    let stale_callers = tools::call(&env, "callers", &json!({"symbol": "onClick"}));
    assert!(stale_callers.text.contains("edited since the last index sync"), "{}", stale_callers.text);
    assert!(stale_callers.text.contains("src/app.ts"), "{}", stale_callers.text);

    let stale_impact = tools::call(&env, "impact", &json!({"symbol": "onClick"}));
    assert!(stale_impact.text.contains("edited since the last index sync"), "{}", stale_impact.text);
    assert!(stale_impact.text.contains("src/app.ts"), "{}", stale_impact.text);
}
