//! The rules ported for resolution gaps and dynamic dispatch must keep an incremental sync equal
//! to a fresh index when either side of a link changes.
use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::path::Path;
use std::time::{Duration, SystemTime};

const FILES: &[(&str, &str)] = &[
    ("go.mod", "module example.com/demo\n\ngo 1.22\n"),
    (
        "disc/disc.go",
        "package disc\n\nimport \"io\"\n\ntype ReadCloser interface {\n\tio.Reader\n\tClose() error\n}\n\ntype Options struct {\n\tRefreshF func() error\n}\n\ntype Discovery struct {\n\trefreshf func() error\n}\n\nfunc New(o Options) *Discovery {\n\treturn &Discovery{refreshf: o.RefreshF}\n}\n\nfunc (d *Discovery) Run() {\n\td.refreshf()\n}\n",
    ),
    ("ec2/ec2.go", "package ec2\n\nimport \"example.com/demo/disc\"\n\ntype EC2 struct{}\n\nfunc (e *EC2) refresh() error { return nil }\nfunc (e *EC2) Read(b []byte) (int, error) { return 0, nil }\nfunc (e *EC2) Close() error { return nil }\n\nfunc Wire() *disc.Discovery {\n\te := &EC2{}\n\treturn disc.New(disc.Options{RefreshF: e.refresh})\n}\n"),
    ("package.json", r#"{"name":"demo"}"#),
    ("src/impl.ts", "export function realImpl() { return 1; }\nexport function other() { return 2; }\nexport const alias = realImpl;\n"),
    ("src/use.ts", "import { alias } from './impl';\nexport function caller() {\n  alias();\n}\n"),
    ("src/proto.js", "function Params() {}\nParams.prototype.render = function render() { return 1; };\nfunction use() {\n  var p = new Params();\n  p.render();\n}\nmodule.exports = use;\n"),
    ("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    ("src/lib.rs", "pub mod wrap;\npub trait Service {\n    fn call(&self) -> u32;\n}\npub fn run<S: Service>(s: S) -> u32 {\n    s.call()\n}\n"),
    ("src/wrap.rs", "use std::ops::Deref;\npub struct Inner;\nimpl Inner {\n    pub fn work(&self) {}\n}\npub struct Other;\nimpl Other {\n    pub fn work(&self) {}\n}\npub struct Wrapper { inner: Inner }\nimpl Deref for Wrapper {\n    type Target = Inner;\n    fn deref(&self) -> &Inner { &self.inner }\n}\npub fn go(w: Wrapper) {\n    w.work();\n}\n"),
    ("app/__init__.py", ""),
    ("app/models.py", "class Worker:\n    def go(self):\n        pass\n\nclass Other:\n    def go(self):\n        pass\n"),
    ("tests/conftest.py", "import pytest\nfrom app.models import Worker\n\n@pytest.fixture\ndef worker():\n    return Worker()\n"),
    ("tests/test_worker.py", "def test_go(worker):\n    worker.go()\n"),
];

fn opts() -> IndexOptions {
    IndexOptions { threads: Some(2), ..Default::default() }
}
fn write(root: &Path, rel: &str, text: &str, step: u64) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
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
fn fresh(root: &Path) -> Vec<String> {
    let temp = tempfile::tempdir().unwrap();
    for (rel, _) in FILES {
        let text = std::fs::read_to_string(root.join(rel)).unwrap();
        write(temp.path(), rel, &text, 1);
    }
    index_project(temp.path(), &opts()).unwrap();
    graph(temp.path())
}

#[test]
fn gap_rule_edits_remain_equal_to_full_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for (i, (rel, contents)) in FILES.iter().enumerate() {
        write(root, rel, contents, i as u64);
    }
    index_project(root, &opts()).unwrap();
    let has = |needle: &str| edges(root).iter().any(|e| e == needle);
    assert!(has("disc/disc.go::Discovery::Run -calls-> ec2/ec2.go::EC2::refresh"), "{:#?}", edges(root));
    assert!(has("ec2/ec2.go::EC2 -implements-> disc/disc.go::ReadCloser"));
    assert!(has("src/use.ts::caller -calls-> src/impl.ts::realImpl"));
    assert!(has("src/proto.js::use -calls-> src/proto.js::Params::render"));
    assert!(has("src/lib.rs::run -calls-> src/lib.rs::Service::call"));
    assert!(has("src/wrap.rs::go -calls-> src/wrap.rs::Inner::work"));
    assert!(has("tests/test_worker.py::test_go -calls-> app/models.py::Worker::go"));
    let changes = [
        ("ec2/ec2.go", "RefreshF: e.refresh", "RefreshF: nil"),
        ("ec2/ec2.go", "RefreshF: nil", "RefreshF: e.refresh"),
        ("ec2/ec2.go", "func (e *EC2) Read(b []byte) (int, error) { return 0, nil }\n", ""),
        (
            "ec2/ec2.go",
            "func (e *EC2) Close() error { return nil }\n",
            "func (e *EC2) Close() error { return nil }\nfunc (e *EC2) Read(b []byte) (int, error) { return 0, nil }\n",
        ),
        ("src/impl.ts", "export const alias = realImpl;", "export const alias = other;"),
        ("src/proto.js", "Params.prototype.render = function render()", "Params.prototype.paint = function render()"),
        ("src/proto.js", "Params.prototype.paint = function render()", "Params.prototype.render = function render()"),
        ("src/lib.rs", "run<S: Service>", "run<S>"),
        ("src/wrap.rs", "type Target = Inner;", "type Target = Other;"),
        ("tests/conftest.py", "return Worker()", "return Other()"),
        ("app/models.py", "class Other:\n    def go(self):", "class Other:\n    def gone(self):"),
    ];
    for (i, (file, from, to)) in changes.iter().enumerate() {
        let current = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(current.contains(from), "step {i}: {file} lacks {from:?}");
        write(root, file, &current.replacen(from, to, 1), 100 + i as u64);
        let report = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
        assert!(report.full_rebuild.is_none(), "step {i} used a full rebuild: {:?}", report.full_rebuild);
        let (inc, full) = (graph(root), fresh(root));
        assert_eq!(inc, full, "incremental graph differs at step {i}: {file}");
        match i {
            0 => assert!(!has("disc/disc.go::Discovery::Run -calls-> ec2/ec2.go::EC2::refresh"), "stale func-field edge"),
            1 => assert!(has("disc/disc.go::Discovery::Run -calls-> ec2/ec2.go::EC2::refresh")),
            2 => assert!(!has("ec2/ec2.go::EC2 -implements-> disc/disc.go::ReadCloser"), "io.Reader asks for Read"),
            3 => assert!(has("ec2/ec2.go::EC2 -implements-> disc/disc.go::ReadCloser")),
            4 => {
                assert!(has("src/use.ts::caller -calls-> src/impl.ts::other"));
                assert!(!has("src/use.ts::caller -calls-> src/impl.ts::realImpl"));
            }
            5 => assert!(!has("src/proto.js::use -calls-> src/proto.js::Params::render")),
            6 => assert!(has("src/proto.js::use -calls-> src/proto.js::Params::render")),
            7 => assert!(!has("src/lib.rs::run -calls-> src/lib.rs::Service::call")),
            8 => {
                assert!(has("src/wrap.rs::go -calls-> src/wrap.rs::Other::work"));
                assert!(!has("src/wrap.rs::go -calls-> src/wrap.rs::Inner::work"));
            }
            9 => assert!(has("tests/test_worker.py::test_go -calls-> app/models.py::Other::go")),
            10 => assert!(!has("tests/test_worker.py::test_go -calls-> app/models.py::Other::go")),
            _ => {}
        }
    }
}
