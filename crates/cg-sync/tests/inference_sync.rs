//! Incremental-sync equivalence tests (`incremental == full rebuild`) for the deepened
//! Rust, Kotlin, and Python inference paths:
//! - Rust inline `pub mod` under wildcard import, imported extension trait return type changes
//! - Kotlin expression-body `= apply` fluent builders & companion extension imports
//! - Python decorator-injected `self.engine`, `@cached_property`, and default-arg singletons.

use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::path::Path;
use std::time::{Duration, SystemTime};

fn opts() -> IndexOptions {
    IndexOptions { threads: Some(2), ..Default::default() }
}

fn write(root: &Path, rel: &str, text: &str, step: u64) {
    let path = root.join(rel);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&path, text).unwrap();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step * 10);
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(t).unwrap();
}

fn assert_same_as_rebuild(root: &Path, step: &str) {
    let synced = Store::locate(root).unwrap().open_snapshot().unwrap();
    let fresh = cg_resolve::build_seg(root, &opts()).unwrap().seg;
    let fresh_view = cg_store::View::new(std::sync::Arc::new(fresh)).with_root(root.to_path_buf());
    let a = dump::canonical(&synced.view);
    let b = dump::canonical(&fresh_view);
    assert_eq!(a, b, "incremental sync diverged from full rebuild at step: {step}");
}

#[test]
fn rust_kotlin_python_inference_sync_matches_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // ---- Rust: wildcard inline mod + extension trait ----
    write(root, "rust_pkg/Cargo.toml", "[package]\nname = \"rp\"\nversion = \"0.1.0\"\n", 1);
    write(
        root,
        "rust_pkg/src/ext.rs",
        "pub struct A;\nimpl A { pub fn go(&self) {} }\npub struct B;\nimpl B { pub fn go(&self) {} }\npub trait Base {}\npub struct S;\nimpl Base for S {}\npub trait Ext { fn map(self) -> A; }\nimpl<T: Base> Ext for T {}\npub fn make() -> S { S }\n",
        1,
    );
    write(root, "rust_pkg/src/mocks.rs", "pub mod pool { pub fn run_one() {} }\n", 1);
    write(
        root,
        "rust_pkg/src/lib.rs",
        "pub mod ext;\npub mod mocks;\nuse crate::ext::{make, Ext};\nuse crate::mocks::*;\npub fn caller() { pool::run_one(); make().map().go(); }\n",
        1,
    );

    // ---- Kotlin: expression-body `= apply` + companion extension ----
    write(
        root,
        "kt/Client.kt",
        "package demo\nclass Url { fun ok() {} }\nclass Url2 { fun ok() {} }\nclass Builder {\n  var host: String = \"\"\n  fun host(h: String) = apply { this.host = h }\n  fun build(): Url = Url()\n  companion object { fun String.toUrl(): Url = Url() }\n}\n",
        1,
    );
    write(
        root,
        "kt/Use.kt",
        "package demo.use\nimport demo.Builder\nimport demo.Builder.Companion.toUrl\nfun run(s: String) {\n  s.toUrl().ok()\n  Builder().host(\"a\").build().ok()\n}\n",
        1,
    );

    // ---- Python: decorator self-injection + default param singleton ----
    write(root, "py/eng.py", "class EngineA:\n    def render(self): pass\nclass EngineB:\n    def render(self): pass\n", 1);
    write(root, "py/reg.py", "class AppsA:\n    def get_model(self): pass\nclass AppsB:\n    def get_model(self): pass\napps = AppsA()\n", 1);
    write(
        root,
        "py/dec.py",
        "from py.eng import EngineA\ndef setup():\n    def wrap(fn):\n        def inner(self):\n            self.engine = EngineA()\n            return fn(self)\n        return inner\n    return wrap\n",
        1,
    );
    write(
        root,
        "py/user.py",
        "from py.dec import setup\nfrom py.reg import apps as global_apps\ndef run(apps=global_apps):\n    apps.get_model()\nclass T:\n    @setup()\n    def test_it(self):\n        self.engine.render()\n",
        1,
    );

    index_project(root, &opts()).unwrap();
    assert_same_as_rebuild(root, "initial");

    // Step 2: change Rust extension trait return type (`A` -> `B`) and remove `pool` inline mod
    write(
        root,
        "rust_pkg/src/ext.rs",
        "pub struct A;\nimpl A { pub fn go(&self) {} }\npub struct B;\nimpl B { pub fn go(&self) {} }\npub trait Base {}\npub struct S;\nimpl Base for S {}\npub trait Ext { fn map(self) -> B; }\nimpl<T: Base> Ext for T {}\npub fn make() -> S { S }\n",
        2,
    );
    write(root, "rust_pkg/src/mocks.rs", "pub mod other { pub fn run_one() {} }\n", 2);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "after rust trait & inline mod change");

    // Step 3: change Kotlin companion extension return type (`Url` -> `Url2`) and `build()` return type
    write(
        root,
        "kt/Client.kt",
        "package demo\nclass Url { fun ok() {} }\nclass Url2 { fun ok() {} }\nclass Builder {\n  var host: String = \"\"\n  fun host(h: String) = apply { this.host = h }\n  fun build(): Url2 = Url2()\n  companion object { fun String.toUrl(): Url2 = Url2() }\n}\n",
        3,
    );
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "after kotlin extension & builder change");

    // Step 4: change Python decorator-injected `self.engine` (`EngineA` -> `EngineB`) and `apps` (`AppsA` -> `AppsB`)
    write(
        root,
        "py/dec.py",
        "from py.eng import EngineB\ndef setup():\n    def wrap(fn):\n        def inner(self):\n            self.engine = EngineB()\n            return fn(self)\n        return inner\n    return wrap\n",
        4,
    );
    write(root, "py/reg.py", "class AppsA:\n    def get_model(self): pass\nclass AppsB:\n    def get_model(self): pass\napps = AppsB()\n", 4);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "after python decorator & singleton change");

    // Step 5 (Codex P1 #1): swap ONLY the import source while keeping `self.engine = Engine()` identical
    write(root, "py/eng_a.py", "class Engine:\n    def render(self): pass\n", 5);
    write(root, "py/eng_b.py", "class Engine:\n    def render(self): pass\n", 5);
    write(
        root,
        "py/dec.py",
        "from py.eng_a import Engine\ndef setup():\n    def wrap(fn):\n        def inner(self):\n            self.engine = Engine()\n            return fn(self)\n        return inner\n    return wrap\n",
        5,
    );
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "after decorator uses eng_a.Engine");

    write(
        root,
        "py/dec.py",
        "from py.eng_b import Engine\ndef setup():\n    def wrap(fn):\n        def inner(self):\n            self.engine = Engine()\n            return fn(self)\n        return inner\n    return wrap\n",
        6,
    );
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "after swapping only decorator import eng_a -> eng_b");

    // Step 6 (Codex P1 #2): sibling Kotlin extension files in same dir/package; editing A.kt must
    // not flip `import ext.parse` tie-break to B.kt due to delta NodeIdx ordering.
    write(root, "kt/ext/A.kt", "package ext\nfun String.parse(): Int = 1\n", 7);
    write(root, "kt/ext/B.kt", "package ext\nfun Int.parse(): Int = 2\n", 7);
    write(root, "kt/ext/UseExt.kt", "package useext\nimport ext.parse\nfun f(s: String) { s.parse() }\n", 7);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "after adding sibling Kotlin extension files");

    write(root, "kt/ext/A.kt", "package ext\n// edited comment\nfun String.parse(): Int = 1\n", 8);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "after editing A.kt with sibling B.kt extension");
}
