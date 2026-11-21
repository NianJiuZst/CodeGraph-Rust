//! Incremental-sync equivalence tests for v1.6.4 multi-language resolution rules:
//! Python class-valued attribute propagation, Go concrete package var initializers,
//! Kotlin enclosing class fields + member extensions, and PHP auto-inferred web roots.

use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::path::Path;
use std::time::{Duration, SystemTime};

const FILES: &[(&str, &str)] = &[
    // Python class-valued attribute + dependent field
    (
        "py/client.py",
        "class Client:\n    def fetch(self):\n        return 1\n\nclass OtherClient:\n    def fetch(self):\n        return 2\n",
    ),
    (
        "py/base.py",
        "from py.client import Client, OtherClient\n\nclass BaseTest:\n    client_class = Client\n\n    def setUp(self):\n        self.client = self.client_class()\n\n    def run_check(self):\n        return self.client.fetch()\n",
    ),
    (
        "py/sub.py",
        "from py.base import BaseTest\n\nclass SubTest(BaseTest):\n    def sub_check(self):\n        return self.client.fetch()\n",
    ),
    // Go concrete package var initializer
    ("go.mod", "module example.com/v164\n\ngo 1.22\n"),
    (
        "gpkg/bind.go",
        "package gpkg\n\ntype Binding interface {\n\tBind() error\n}\n\ntype jsonBinding struct{}\nfunc (jsonBinding) Bind() error { return nil }\n\ntype xmlBinding struct{}\nfunc (xmlBinding) Bind() error { return nil }\n\nvar Active Binding = jsonBinding{}\n",
    ),
    ("gpkg/use.go", "package gpkg\n\nfunc DoBind() error {\n\treturn Active.Bind()\n}\n"),
    // Kotlin enclosing field + member extension
    (
        "kt/Store.kt",
        "package demo\n\nclass MemoryStore { fun save(v: String) {} }\nclass DiskStore { fun save(v: String) {} }\nclass Editor { fun put(v: String) {} }\n",
    ),
    (
        "kt/App.kt",
        "package demo\n\nclass App {\n    private val store = MemoryStore()\n    private fun Editor.writeAll(v: String) { put(v) }\n    fun run(e: Editor) {\n        store.save(\"a\")\n        e.writeAll(\"b\")\n    }\n}\n",
    ),
    // PHP inferred webRoot + relative include
    ("www/inc/a.php", "<?php\nfunction phpA() { return 1; }\n"),
    ("www/inc/b.php", "<?php\nfunction phpB() { return 2; }\n"),
    ("www/inc/c.php", "<?php\nfunction phpC() { return 3; }\n"),
    (
        "www/index.php",
        "<?php\nrequire_once $_SERVER['DOCUMENT_ROOT'] . '/inc/a.php';\nrequire_once $_SERVER['DOCUMENT_ROOT'] . '/inc/b.php';\nrequire_once $_SERVER['DOCUMENT_ROOT'] . '/inc/c.php';\nfunction entry() { return phpA() + phpB() + phpC(); }\n",
    ),
];

fn opts() -> IndexOptions {
    IndexOptions { threads: Some(2), ..Default::default() }
}

fn write(root: &Path, rel: &str, text: &str, step: u64) {
    let path = root.join(rel);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&path, text).unwrap();
    let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step);
    std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(stamp).unwrap();
}

fn graph(root: &Path) -> Vec<String> {
    let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
    dump::canonical(&snap.view)
}

fn edges(root: &Path) -> Vec<String> {
    let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
    dump::dump_edges(&snap.view)
}

fn fresh(root: &Path, rels: &[&str]) -> Vec<String> {
    let temp = tempfile::tempdir().unwrap();
    for rel in rels {
        let p = root.join(rel);
        if p.exists() {
            let text = std::fs::read_to_string(&p).unwrap();
            write(temp.path(), rel, &text, 1);
        }
    }
    index_project(temp.path(), &opts()).unwrap();
    graph(temp.path())
}

#[test]
fn v164_resolution_edits_match_full_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let rels: Vec<&str> = FILES.iter().map(|(r, _)| *r).collect();
    for (i, (rel, text)) in FILES.iter().enumerate() {
        write(root, rel, text, i as u64);
    }
    index_project(root, &opts()).unwrap();

    let has = |needle: &str| edges(root).iter().any(|e| e == needle);
    assert!(has("py/base.py::BaseTest::run_check -calls-> py/client.py::Client::fetch"), "{:#?}", edges(root));
    assert!(has("py/sub.py::SubTest::sub_check -calls-> py/client.py::Client::fetch"), "{:#?}", edges(root));
    assert!(has("gpkg/use.go::DoBind -calls-> gpkg/bind.go::jsonBinding::Bind"), "{:#?}", edges(root));
    assert!(has("kt/App.kt::demo::App::run -calls-> kt/Store.kt::demo::MemoryStore::save"), "{:#?}", edges(root));
    assert!(has("kt/App.kt::demo::App::run -calls-> kt/App.kt::Editor::writeAll"), "{:#?}", edges(root));
    assert!(has("www/index.php::entry -calls-> www/inc/a.php::phpA"), "{:#?}", edges(root));

    let edits = [
        // 0: Python swap client_class -> dependent self.client must retarget to OtherClient
        ("py/base.py", "client_class = Client", "client_class = OtherClient"),
        // 1: Go swap concrete initializer of package var -> DoBind must retarget to xmlBinding::Bind
        ("gpkg/bind.go", "var Active Binding = jsonBinding{}", "var Active Binding = xmlBinding{}"),
        // 2: Kotlin swap enclosing field initializer -> run must retarget to DiskStore::save
        ("kt/App.kt", "private val store = MemoryStore()", "private val store = DiskStore()"),
        // 3: PHP rename function inside included file under inferred webRoot
        ("www/inc/a.php", "function phpA()", "function phpARenamed()"),
    ];
    for (i, (file, from, to)) in edits.iter().enumerate() {
        let cur = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(cur.contains(from), "step {i}: missing {from:?} in {file}");
        write(root, file, &cur.replacen(from, to, 1), 100 + i as u64);
        let report = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
        assert!(report.full_rebuild.is_none(), "step {i} fell back to full rebuild: {:?}", report.full_rebuild);
        assert_eq!(graph(root), fresh(root, &rels), "incremental != full rebuild at step {i} ({file})");
        match i {
            0 => {
                assert!(has("py/base.py::BaseTest::run_check -calls-> py/client.py::OtherClient::fetch"));
                assert!(!has("py/base.py::BaseTest::run_check -calls-> py/client.py::Client::fetch"));
                assert!(has("py/sub.py::SubTest::sub_check -calls-> py/client.py::OtherClient::fetch"));
                assert!(!has("py/sub.py::SubTest::sub_check -calls-> py/client.py::Client::fetch"));
            }
            1 => {
                assert!(has("gpkg/use.go::DoBind -calls-> gpkg/bind.go::xmlBinding::Bind"));
                assert!(!has("gpkg/use.go::DoBind -calls-> gpkg/bind.go::jsonBinding::Bind"));
            }
            2 => {
                assert!(has("kt/App.kt::demo::App::run -calls-> kt/Store.kt::demo::DiskStore::save"));
                assert!(!has("kt/App.kt::demo::App::run -calls-> kt/Store.kt::demo::MemoryStore::save"));
            }
            3 => {
                assert!(!has("www/index.php::entry -calls-> www/inc/a.php::phpA"));
            }
            _ => {}
        }
    }
}

#[test]
fn codex_review_sync_edge_cases_match_full_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let initial: &[(&str, &str)] = &[
        // P1-1: TS barrel re-exporting an aliased local import
        ("package.json", r#"{"name":"barrel"}"#),
        ("ts/a.ts", "export function run() { return 1; }\n"),
        ("ts/b.ts", "export function run() { return 2; }\n"),
        ("ts/barrel.ts", "import { run as inner } from './a';\nexport { inner as execute };\n"),
        ("ts/use.ts", "import { execute } from './barrel';\nexport function go() { return execute(); }\n"),
        // P1-2: PHP function-body include whose target file does not exist yet
        ("php/loader.php", "<?php\nfunction loadCfg() {\n    return require __DIR__ . '/config.php';\n}\n"),
        // P1-3: Go package var whose declared interface changes while initializer text stays identical
        ("go.mod", "module example.com/codex\n\ngo 1.22\n"),
        (
            "gvar/types.go",
            "package gvar\n\ntype Reader interface { Close() error }\ntype Writer interface { Close() error }\nfunc openBackend() any { return nil }\nvar Backend Reader = openBackend()\n",
        ),
        ("gvar/use.go", "package gvar\n\nfunc Shut() error { return Backend.Close() }\n"),
        // P2-1: PHP case-insensitive method override added in child class
        ("php/base.php", "<?php\nclass BaseGreeter {\n    public function sayHello() { return 1; }\n}\n"),
        ("php/child.php", "<?php\nclass ChildGreeter extends BaseGreeter {\n}\n"),
        ("php/caller.php", "<?php\nfunction invoke(ChildGreeter $c) { return $c->SAYHELLO(); }\n"),
    ];
    let mut rels: Vec<&str> = initial.iter().map(|(r, _)| *r).collect();
    for (i, (rel, text)) in initial.iter().enumerate() {
        write(root, rel, text, i as u64);
    }
    index_project(root, &opts()).unwrap();
    let has = |needle: &str| edges(root).iter().any(|e| e == needle);
    assert!(has("ts/use.ts::go -calls-> ts/a.ts::run"), "{:#?}", edges(root));
    assert!(has("gvar/use.go::Shut -calls-> gvar/types.go::Reader::Close"), "{:#?}", edges(root));
    assert!(has("php/caller.php::invoke -calls-> php/base.php::BaseGreeter::sayHello"), "{:#?}", edges(root));

    // Step 0 (P1-1): switch TS barrel import from ./a to ./b
    write(root, "ts/barrel.ts", "import { run as inner } from './b';\nexport { inner as execute };\n", 100);
    let r = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert!(r.full_rebuild.is_none());
    assert_eq!(graph(root), fresh(root, &rels), "P1-1 TS barrel alias mismatch");
    assert!(has("ts/use.ts::go -calls-> ts/b.ts::run"));
    assert!(!has("ts/use.ts::go -calls-> ts/a.ts::run"));

    // Step 1 (P1-2): add previously missing php/config.php with no symbol definitions
    write(root, "php/config.php", "<?php\nreturn [];\n", 101);
    rels.push("php/config.php");
    let r = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert!(r.full_rebuild.is_none());
    assert_eq!(graph(root), fresh(root, &rels), "P1-2 PHP added include target mismatch");
    assert!(has("php/loader.php::loadCfg -imports-> php/config.php"));

    // Step 2 (P1-3): change Go package var declared interface Reader -> Writer
    let cur = std::fs::read_to_string(root.join("gvar/types.go")).unwrap();
    write(root, "gvar/types.go", &cur.replace("var Backend Reader = openBackend()", "var Backend Writer = openBackend()"), 102);
    let r = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert!(r.full_rebuild.is_none());
    assert_eq!(graph(root), fresh(root, &rels), "P1-3 Go package var declared type mismatch");
    assert!(has("gvar/use.go::Shut -calls-> gvar/types.go::Writer::Close"));
    assert!(!has("gvar/use.go::Shut -calls-> gvar/types.go::Reader::Close"));

    // Step 3 (P2-1): add lowercase/mixed-case override sayHello() in PHP child class
    write(root, "php/child.php", "<?php\nclass ChildGreeter extends BaseGreeter {\n    public function sayHello() { return 2; }\n}\n", 103);
    let r = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert!(r.full_rebuild.is_none());
    assert_eq!(graph(root), fresh(root, &rels), "P2-1 PHP case-insensitive override mismatch");
    assert!(has("php/caller.php::invoke -calls-> php/child.php::ChildGreeter::sayHello"));
    assert!(!has("php/caller.php::invoke -calls-> php/base.php::BaseGreeter::sayHello"));
}
