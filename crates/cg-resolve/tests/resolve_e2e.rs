use cg_resolve::{build_seg, IndexOptions};
use cg_store::{dump, Seg, View};
use std::path::Path;
use std::sync::Arc;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, View) {
    let dir = tempfile::tempdir().unwrap();
    for (p, c) in files {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, c).unwrap();
    }
    let built = build_seg(dir.path(), &IndexOptions { threads: Some(2), ..Default::default() }).unwrap();
    let seg: Arc<Seg> = Arc::new(built.seg);
    let root = Path::new(dir.path()).to_path_buf();
    (dir, View::new(seg).with_root(root))
}

fn has(edges: &[String], needle: &str) -> bool {
    edges.iter().any(|e| e == needle)
}

#[test]
fn parallel_event_keys_and_resolution_are_byte_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..80 {
        let source = format!("export function handler{i}() {{}}\nexport function fire{i}() {{ bus.emit('event{i}'); }}\nbus.on('event{i}', handler{i});\n");
        std::fs::write(dir.path().join(format!("file{i}.ts")), source).unwrap();
    }
    let build = |threads| build_seg(dir.path(), &IndexOptions { threads: Some(threads), memory_mb: Some(2048), ..Default::default() }).unwrap();
    let sequential = build(1);
    let parallel = build(4);
    assert_eq!(sequential.seg.to_bytes(), parallel.seg.to_bytes(), "all partitions and metadata must be independent of scheduling");
    let view = View::new(Arc::new(parallel.seg));
    assert_eq!(dump::canonical(&view).iter().filter(|line| line.contains("event-emitter")).count(), 80);
}

#[test]
fn typescript_imports_classes_and_receivers() {
    let (_d, view) = project(&[
        ("src/service.ts", "export class Service { run(): void {} }\nexport function helper() {}\n"),
        ("src/index.ts", "export * from './service';\n"),
        (
            "src/widget.ts",
            r#"import { Service, helper } from './index';
import * as svc from './service';
export class Base { base() {} }
export class Widget extends Base {
  private svc: Service;
  constructor() { super(); this.svc = new Service(); }
  render(): void {
    this.svc.run();
    helper();
    svc.helper();
    this.base();
    const s = new Service();
    s.run();
    localFn();
    function localFn() {}
  }
}
"#,
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "src/widget.ts::Widget -extends-> src/widget.ts::Base"));
    assert!(has(&edges, "src/widget.ts::Widget::render -calls-> src/service.ts::Service::run"), "this.svc.run through the re-export barrel");
    assert!(has(&edges, "src/widget.ts::Widget::render -calls-> src/service.ts::helper"));
    assert!(has(&edges, "src/widget.ts::Widget::render -calls-> src/widget.ts::Base::base"), "inherited method");
    assert!(has(&edges, "src/widget.ts::Widget::render -instantiates-> src/service.ts::Service"));
    assert!(has(&edges, "src/widget.ts::Widget::render -calls-> src/widget.ts::Widget::render::localFn"));
    assert!(has(&edges, "src/widget.ts -imports-> src/index.ts"));
    assert!(has(&edges, "src/index.ts -imports-> src/service.ts"));
}

#[test]
fn typescript_satisfies_keeps_concrete_call_targets() {
    let (_d, view) = project(&[
        ("service.ts", "export interface Runner { run(): void }\nexport class Service { run() {} extra() {} }\nexport function makeService(): Service { return new Service(); }\n"),
        (
            "app.ts",
            r#"import { Runner, Service, makeService } from './service';
export function direct() {
    const service = new Service() satisfies Runner;
    service.run();
    service.extra();
}
export function factory() {
    const service = makeService() satisfies Runner;
    service.run();
    service.extra();
}
export function plainFactory() {
    const service = makeService();
    service.run();
    service.extra();
}
export class Holder {
    service = new Service() satisfies Runner;
    use() { this.service.run(); this.service.extra(); }
}
export function asserted() {
    const service = (new Service() as Runner) satisfies object;
    service.run();
}
"#,
        ),
    ]);
    let edges = dump::dump_edges(&view);
    for caller in ["app.ts::direct", "app.ts::Holder::use"] {
        for method in ["run", "extra"] {
            assert!(
                has(&edges, &format!("{caller} -calls-> service.ts::Service::{method}")),
                "{caller} must retain the initializer's concrete receiver: {edges:#?}"
            );
        }
        assert!(!has(&edges, &format!("{caller} -calls-> service.ts::Runner::run")), "satisfies must not replace the receiver with its constraint");
    }
    assert!(has(&edges, "app.ts::factory -calls-> service.ts::makeService"), "the factory call itself must be preserved");
    assert!(!has(&edges, "app.ts::factory -calls-> service.ts::Runner::run"), "satisfies must not replace an unknown receiver with its constraint");
    assert!(has(&edges, "app.ts::asserted -calls-> service.ts::Runner::run"), "an inner as assertion still supplies its explicit type");
}

#[test]
fn python_modules_and_self() {
    let (_d, view) = project(&[
        ("pkg/__init__.py", "from .core import Engine\n"),
        ("pkg/core.py", "class Engine:\n    def start(self):\n        self.warm()\n    def warm(self):\n        pass\n\ndef make():\n    return Engine()\n"),
        ("app.py", "from pkg import Engine, core\nimport pkg.core as pc\n\ndef main():\n    e = Engine()\n    e.start()\n    pc.make()\n    core.make()\n    print('x')\n"),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "app.py::main -calls-> pkg/core.py::Engine::start"), "typed local from constructor call through the package re-export");
    assert!(has(&edges, "pkg/core.py::Engine::start -calls-> pkg/core.py::Engine::warm"));
    assert!(has(&edges, "app.py::main -calls-> pkg/core.py::make"));
    assert!(has(&edges, "app.py::main -instantiates-> pkg/core.py::Engine"));
}

#[test]
fn go_packages_and_interfaces() {
    let (_d, view) = project(&[
        ("go.mod", "module example.com/proj\n"),
        ("pkg/util/util.go", "package util\n\nfunc Do(n int) int { return n }\n\nfunc helper() {}\n"),
        (
            "cmd/main.go",
            r#"package main

import "example.com/proj/pkg/util"

type Repo interface { Find(id int) string }
type Server struct { repo Repo }

func (s *Server) Run() {
	s.repo.Find(1)
	util.Do(3)
	helper := func() {}
	helper()
	other()
}

func other() {}
func main() { (&Server{}).Run() }
"#,
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "cmd/main.go::Server::Run -calls-> cmd/main.go::Repo::Find"));
    assert!(has(&edges, "cmd/main.go::Server::Run -calls-> pkg/util/util.go::Do"));
    assert!(has(&edges, "cmd/main.go::Server::Run -calls-> cmd/main.go::other"));
    assert!(!edges.iter().any(|e| e.contains("util.go::helper")), "the local closure must shadow the package function");
}

#[test]
fn java_packages_imports_and_members() {
    let (_d, view) = project(&[
        ("src/com/x/util/Helper.java", "package com.x.util;\npublic class Helper { public void assist() {} public static void s() {} }\n"),
        ("src/com/x/util/Statics.java", "package com.x.util;\npublic class Statics { public static void go() {} }\n"),
        (
            "src/com/x/svc/A.java",
            r#"package com.x.svc;
import com.x.util.Helper;
import static com.x.util.Statics.go;
public class A extends Base {
    private final Helper repo;
    public void run() {
        repo.assist();
        Helper h = new Helper();
        h.assist();
        go();
        this.other();
        Helper.s();
    }
    private void other() {}
}
class Base {}
"#,
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    let run = "src/com/x/svc/A.java::com.x.svc::A::run";
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/util/Helper.java::com.x.util::Helper::assist")));
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/util/Statics.java::com.x.util::Statics::go")), "static import");
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/svc/A.java::com.x.svc::A::other")));
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/util/Helper.java::com.x.util::Helper::s")));
    assert!(has(&edges, &format!("{run} -instantiates-> src/com/x/util/Helper.java::com.x.util::Helper")));
}

#[test]
fn rust_modules_use_and_impl() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/lib.rs", "pub mod util;\npub mod server;\n"),
        ("src/util.rs", "pub fn helper(n: u32) -> u32 { n }\npub struct Thing;\nimpl Thing { pub fn new() -> Thing { Thing } pub fn go(&self) {} }\n"),
        (
            "src/server.rs",
            r#"use crate::util::{helper, Thing as T};
pub trait Repo { fn find(&self, id: u32) -> u32; }
pub struct Server { repo: Box<dyn Repo> }
impl Server {
    pub fn run(&self) {
        helper(3);
        let t = T::new();
        t.go();
        Self::other();
    }
    fn other() {}
}
"#,
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "src/server.rs::Server::run -calls-> src/util.rs::helper"));
    assert!(has(&edges, "src/server.rs::Server::run -calls-> src/util.rs::Thing::new"));
    assert!(has(&edges, "src/server.rs::Server::run -calls-> src/util.rs::Thing::go"), "type of `t` comes from T::new()");
    assert!(has(&edges, "src/server.rs::Server::run -calls-> src/server.rs::Server::other"));
}

#[test]
fn kotlin_members_and_imports() {
    let (_d, view) = project(&[
        ("src/com/x/util/Helper.kt", "package com.x.util\nclass Helper { fun assist() {} }\n"),
        (
            "src/com/x/svc/A.kt",
            "package com.x.svc\nimport com.x.util.Helper\nclass A(private val repo: Helper) {\n    fun run() {\n        repo.assist()\n        val h = Helper()\n        h.assist()\n        other()\n    }\n    private fun other() {}\n}\n",
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    let run = "src/com/x/svc/A.kt::com.x.svc::A::run";
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/util/Helper.kt::com.x.util::Helper::assist")));
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/svc/A.kt::com.x.svc::A::other")));
}

#[test]
fn rust_pointer_roots_use_the_final_field_method() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        ("src/lib.rs", "pub mod worker;\npub mod owner;\n"),
        (
            "src/worker.rs",
            r#"pub struct Worker;
impl Worker {
    pub fn execute(&self) -> u8 { 7 }
    pub fn clone(&self) -> u8 { 99 }
}
pub struct Noise;
impl Noise { pub fn execute(&self) -> u8 { 0 } }
"#,
        ),
        (
            "src/owner.rs",
            r#"use crate::worker::Worker;
use std::rc::Rc;
use std::sync::Arc;
pub struct Owner {
    plain: Worker,
    child: Child,
}
pub struct Child { plain: Worker }
impl Child { pub fn clone(&self) -> u8 { 88 } }
impl Owner {
    pub fn plain(&self) -> u8 { self.plain.execute() }
    pub fn child(&self) -> u8 { self.child.plain.execute() }
    pub fn plain_clone(&self) -> u8 { self.plain.clone() }
    pub fn child_plain_clone(&self) -> u8 { self.child.plain.clone() }
}
pub fn local_plain(w: Child) -> u8 { w.plain.clone() }
pub fn local_arc(w: Arc<Child>) -> u8 { w.plain.clone() }
pub fn local_rc(w: Rc<Child>) -> u8 { w.plain.clone() }
pub fn local_box(w: Box<Child>) -> u8 { w.plain.clone() }
pub fn local_nested(w: Arc<Box<Child>>) -> u8 { w.plain.clone() }
pub fn direct_plain(w: Child) -> u8 { w.clone() }
pub fn direct_arc(w: Arc<Child>) { w.clone(); }
pub fn direct_rc(w: Rc<Child>) { w.clone(); }
"#,
        ),
    ]);
    let edges = dump::dump_edges(&view);
    for name in ["plain", "child"] {
        let calls: Vec<_> = edges.iter().filter(|e| e.starts_with(&format!("src/owner.rs::Owner::{name} -calls-> "))).map(String::as_str).collect();
        assert_eq!(calls, [format!("src/owner.rs::Owner::{name} -calls-> src/worker.rs::Worker::execute")], "{edges:#?}");
    }
    for name in ["plain_clone", "child_plain_clone"] {
        assert!(has(&edges, &format!("src/owner.rs::Owner::{name} -calls-> src/worker.rs::Worker::clone")), "{edges:#?}");
    }
    for name in ["local_plain", "local_arc", "local_rc", "local_box", "local_nested"] {
        let calls: Vec<_> = edges.iter().filter(|e| e.starts_with(&format!("src/owner.rs::{name} -calls-> "))).map(String::as_str).collect();
        assert_eq!(calls, [format!("src/owner.rs::{name} -calls-> src/worker.rs::Worker::clone")], "{edges:#?}");
    }
    assert!(has(&edges, "src/owner.rs::direct_plain -calls-> src/owner.rs::Child::clone"), "{edges:#?}");
    for name in ["direct_arc", "direct_rc"] {
        assert!(!edges.iter().any(|e| e.starts_with(&format!("src/owner.rs::{name} -calls-> "))), "{edges:#?}");
    }
}

#[test]
fn debug_ts_unresolved() {
    let (_d, view) = project(&[
        ("src/service.ts", "export class Service { run(): void {} }\nexport function helper() {}\n"),
        (
            "src/widget.ts",
            "import { Service } from './service';\nexport class Base { base() {} }\nexport class Widget extends Base {\n  render(): void {\n    this.base();\n    this.render();\n  }\n}\n",
        ),
    ]);
    for u in dump::dump_unresolved(&view) {
        eprintln!("UNRESOLVED {u}");
    }
    for e in dump::dump_edges(&view) {
        eprintln!("EDGE {e}");
    }
}

#[test]
fn debug_python_typing_and_package() {
    let (_d, view) = project(&[
        ("src/flask/__init__.py", "from .app import Flask as Flask\n"),
        ("src/flask/app.py", "import typing as t\nT = t.TypeVar('T')\nclass Flask:\n    def route(self): pass\n"),
        ("tests/test_a.py", "import flask\nimport typing as t\n\ndef test():\n    app = flask.Flask('x')\n    app.route()\n    x = t.cast(int, 1)\n"),
    ]);
    for u in dump::dump_unresolved(&view) {
        eprintln!("UNRESOLVED {u}");
    }
    for e in dump::dump_edges(&view) {
        eprintln!("EDGE {e}");
    }
    for f in view.files() {
        for i in view.imports_of(f) {
            eprintln!(
                "IMPORT {} local={} imported={} spec={} kind={} scope={} line={}",
                view.file(f).path(),
                view.text_str(i.local),
                view.text_str(i.imported),
                view.text_str(i.spec),
                i.kind,
                i.scope.0,
                i.line
            );
        }
    }
}

#[test]
fn express_flask_and_gin_routes_link_to_their_handlers() {
    let (_d, v) = project(&[
        ("package.json", r#"{"name":"a","dependencies":{"express":"4"}}"#),
        ("web/routes.js", "const app = require('express')();\nfunction list(req, res) {}\nconst ctrl = { show(req, res) {} };\napp.get('/users', list);\napp.get('/users/:id', ctrl.show);\n"),
        ("requirements.txt", "flask\n"),
        ("py/views.py", "from flask import Flask\napp = Flask(__name__)\n\n@app.route('/hello')\ndef hello():\n    return 'x'\n"),
        ("go.mod", "module example.com/m\n\nrequire github.com/gin-gonic/gin v1.9.0\n"),
        ("srv/main.go", "package main\n\nfunc ping() {}\n\nfunc setup(r *Engine) {\n\tr.GET(\"/ping\", ping)\n}\n"),
    ]);
    let edges = cg_store::dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e.contains(needle));
    assert!(has("web/routes.js::web/routes.js::GET:/users -references-> web/routes.js::list"), "{edges:#?}");
    assert!(has("GET:/users/:id -references-> web/routes.js::ctrl::show"), "{edges:#?}");
    assert!(has("py/views.py::py/views.py::GET:/hello -references-> py/views.py::hello"), "{edges:#?}");
    assert!(has("srv/main.go::srv/main.go::GET:/ping -references-> srv/main.go::ping"), "{edges:#?}");
}

fn synth_meta(v: &View, from: &str, to: &str) -> Option<String> {
    for f in v.files() {
        for n in v.nodes_in_file(f) {
            let n = cg_core::NodeIdx(n);
            if cg_store::dump::node_label(v, n) != from {
                continue;
            }
            for e in v.out_edges(n) {
                if cg_store::dump::node_label(v, e.dst) == to {
                    return v.edge_meta(e.eid).map(str::to_owned);
                }
            }
        }
    }
    None
}

#[test]
fn synthesised_edges_for_dynamic_dispatch() {
    let (_d, v) = project(&[
        ("ui/App.tsx", "import { Child } from './Child';\nexport function App() {\n  return <div><Child /></div>;\n}\n"),
        ("ui/Child.tsx", "export function Child() {\n  return <span />;\n}\n"),
        ("ui/Comp.jsx", "import React from 'react';\nexport class Comp extends React.Component {\n  handle() {\n    this.setState({ a: 1 });\n  }\n  render() {\n    return null;\n  }\n}\n"),
        ("bus/bus.ts", "import { EventEmitter } from 'events';\nconst bus = new EventEmitter();\nexport function onTick() {}\nbus.on('tick', onTick);\nexport function fire() {\n  bus.emit('tick');\n}\n"),
        ("j/Shape.java", "package j;\npublic interface Shape {\n    double area();\n}\n"),
        ("j/Circle.java", "package j;\npublic class Circle implements Shape {\n    public double area() { return 1; }\n}\n"),
        ("go.mod", "module example.com/m\n\ngo 1.21\n"),
        ("g/g.go", "package g\n\ntype Greeter interface {\n\tGreet() string\n\tName() string\n}\n\ntype En struct{}\n\nfunc (e *En) Greet() string { return \"hi\" }\nfunc (e *En) Name() string { return \"en\" }\n"),
    ]);
    let edges = cg_store::dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    assert!(has("ui/App.tsx::App -calls-> ui/Child.tsx::Child"), "jsx-render: {edges:#?}");
    assert!(synth_meta(&v, "ui/App.tsx::App", "ui/Child.tsx::Child").unwrap().contains("jsx-render"));
    // like the other synthesised edges, a render sits on the rendering component's first line
    let render_lines: Vec<_> = v
        .files()
        .flat_map(|f| v.nodes_in_file(f))
        .flat_map(|n| v.out_edges(cg_core::NodeIdx(n)))
        .filter(|e| e.kind == cg_core::EdgeKind::Calls && cg_store::dump::node_label(&v, e.dst) == "ui/Child.tsx::Child")
        .map(|e| e.line)
        .collect();
    assert_eq!(render_lines, [Some(2)]);
    assert!(has("ui/Comp.jsx::Comp::handle -calls-> ui/Comp.jsx::Comp::render"), "react-render: {edges:#?}");
    assert!(has("bus/bus.ts::fire -calls-> bus/bus.ts::onTick"), "event-emitter: {edges:#?}");
    assert!(synth_meta(&v, "bus/bus.ts::fire", "bus/bus.ts::onTick").unwrap().contains("\"event\":\"tick\""));
    assert!(has("j/Shape.java::j::Shape::area -calls-> j/Circle.java::j::Circle::area"), "interface-impl: {edges:#?}");
    assert!(has("g/g.go::En -implements-> g/g.go::Greeter"), "go-implements: {edges:#?}");
    assert!(has("g/g.go::Greeter::Greet -calls-> g/g.go::En::Greet"), "go interface-impl: {edges:#?}");
}

#[test]
fn names_on_continuation_lines_of_multi_line_imports() {
    let (_d, view) = project(&[
        ("pkg/core.py", "class Engine:\n    pass\n\ndef make():\n    pass\n"),
        ("app.py", "from pkg.core import (\n    Engine,\n    make,\n)\n"),
        ("lib.ts", "export function a() {}\nexport class B {}\n"),
        ("main.ts", "import {\n  a,\n  B,\n} from \"./lib\";\n"),
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/lib.rs", "pub mod util;\npub mod server;\n"),
        ("src/util.rs", "pub fn helper() {}\npub struct Thing;\n"),
        ("src/server.rs", "use crate::util::{\n    helper,\n    Thing,\n};\n"),
    ]);
    let edges = edges_of(&view);
    for e in [
        "app.py -imports-> pkg/core.py::Engine",
        "app.py -imports-> pkg/core.py::make",
        "main.ts -imports-> lib.ts::a",
        "main.ts -imports-> lib.ts::B",
        "src/server.rs -imports-> src/util.rs::helper",
        "src/server.rs -imports-> src/util.rs::Thing",
    ] {
        assert!(has(&edges, e), "{e}");
    }
}

#[test]
fn kotlin_call_takes_the_method_beside_a_same_named_property() {
    let (_d, view) = project(&[(
        "src/com/x/Client.kt",
        "package com.x\nclass Factory\nclass Client {\n    val socketFactory: Factory = Factory()\n    fun socketFactory(): Factory = socketFactory\n    var method: String = \"GET\"\n    fun method(m: String, body: String?): Client = this\n    fun put(body: String): Client = method(\"PUT\", body)\n}\nclass User {\n    fun run(c: Client) {\n        c.socketFactory()\n    }\n}\n",
    )]);
    let mut calls = Vec::new();
    for f in view.files() {
        for n in view.nodes_in_file(f) {
            let n = cg_core::NodeIdx(n);
            for e in view.out_edges(n).filter(|e| e.kind == cg_core::EdgeKind::Calls) {
                calls.push(format!("{} -> {}", view.node(n).name(), dump::node_key(&view, e.dst)));
            }
        }
    }
    assert!(calls.iter().any(|c| c.starts_with("run -> ") && c.contains("#method@com.x::Client::socketFactory")), "{calls:#?}");
    assert!(calls.iter().any(|c| c.starts_with("put -> ") && c.contains("#method@com.x::Client::method")), "{calls:#?}");
    assert!(!calls.iter().any(|c| c.contains("#field@") || c.contains("#property@")), "{calls:#?}");
}

#[test]
fn kotlin_extension_function_is_a_member_of_its_receiver_and_subtypes() {
    let (_d, view) = project(&[
        ("src/com/x/Lockable.kt", "package com.x\ninterface Lockable\ninline fun Lockable.awaitNanos(nanos: Long) {}\n"),
        (
            "src/com/x/TaskRunner.kt",
            "package com.x\nclass TaskRunner : Lockable {\n    fun direct(l: Lockable) {\n        l.awaitNanos(6L)\n    }\n    fun subtype(r: TaskRunner) {\n        r.awaitNanos(5L)\n    }\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    for from in ["direct", "subtype"] {
        assert!(
            has(&edges, &format!("src/com/x/TaskRunner.kt::com.x::TaskRunner::{from} -calls-> src/com/x/Lockable.kt::Lockable::awaitNanos")),
            "{from}: {edges:#?}"
        );
    }
}

#[test]
fn rust_use_through_crate_self_or_super_imports_no_module_file() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/lib.rs", "pub mod util;\npub mod net;\n"),
        ("src/util.rs", "pub struct Thing;\n"),
        ("src/net/mod.rs", "pub mod tcp;\npub struct Addr;\n"),
        ("src/net/tcp.rs", "use crate::util::Thing;\nuse super::Addr;\nuse self::inner::Deep;\nmod inner {\n    pub struct Deep;\n}\n"),
        ("tests/it.rs", "use demo::util::Thing;\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/net/tcp.rs -imports-> src/util.rs::Thing"), "{edges:#?}");
    assert!(has(&edges, "src/net/tcp.rs -imports-> src/net/mod.rs::Addr"), "{edges:#?}");
    // the bare `crate` / `super` / `self` root names no file the module depends on
    assert!(!edges.iter().any(|e| e.starts_with("src/net/tcp.rs -imports-> ") && !e.contains("::")), "{edges:#?}");
    // a path through the crate's name, from another crate of the package, still does
    assert!(has(&edges, "tests/it.rs -imports-> src/lib.rs"), "{edges:#?}");
}

#[test]
fn rust_path_member_receiver_is_the_type_the_path_names() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/lib.rs", "pub mod other;\npub mod user;\npub enum Kind {\n    A,\n}\n"),
        (
            "src/user.rs",
            "use crate::Kind;\npub fn local() {\n    let _k = Kind::A;\n}\npub fn dependency() {\n    let _k = mio::Kind::A;\n}\npub fn module() {\n    let _k = crate::other::Kind::A;\n}\n",
        ),
        ("src/other.rs", "pub enum Kind {\n    A,\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/user.rs::local -references-> src/lib.rs::Kind"), "{edges:#?}");
    assert!(!edges.iter().any(|e| e.starts_with("src/user.rs::dependency -references-> ")), "{edges:#?}");
    assert!(has(&edges, "src/user.rs::module -references-> src/other.rs::Kind"), "{edges:#?}");
}

#[test]
fn typescript_new_makes_the_value_beside_a_same_named_interface() {
    let (_d, view) = project(&[
        (
            "src/schemas.ts",
            "function ctor(name: string): any {\n  return null;\n}\nexport interface ZodUnknown {\n  kind: string;\n}\nexport const ZodUnknown: any = ctor(\"ZodUnknown\");\nexport function local(): ZodUnknown {\n  return new ZodUnknown();\n}\n",
        ),
        ("src/use.ts", "import { ZodUnknown } from \"./schemas\";\nexport function make(): ZodUnknown {\n  return new ZodUnknown();\n}\n"),
    ]);
    // the interface and the const share a label: tell them apart by kind
    let mut made = Vec::new();
    for f in view.files() {
        for n in view.nodes_in_file(f) {
            let n = cg_core::NodeIdx(n);
            for e in view.out_edges(n).filter(|e| e.kind == cg_core::EdgeKind::Instantiates) {
                made.push(format!("{} -> {}", view.node(n).name(), dump::node_key(&view, e.dst)));
            }
        }
    }
    for from in ["local", "make"] {
        assert!(made.iter().any(|m| m.starts_with(&format!("{from} -> ")) && m.contains("@ZodUnknown:")), "{made:#?}");
    }
    assert!(!made.iter().any(|m| m.contains("#interface@")), "{made:#?}");
}

#[test]
fn typescript_import_of_a_type_and_value_twin_names_its_first_declaration() {
    let (_d, view) = project(&[
        (
            "src/schemas.ts",
            "function ctor(): any {\n  return null;\n}\nexport interface TypeFirst {\n  kind: string;\n}\nexport const TypeFirst: any = ctor();\nexport const ValueFirst: any = ctor();\nexport type ValueFirst = { kind: string };\n",
        ),
        ("src/index.ts", "export { TypeFirst, ValueFirst } from \"./schemas\";\n"),
        ("src/use.ts", "import { TypeFirst, ValueFirst } from \"./schemas\";\nexport function make() {\n  return [new TypeFirst(), new ValueFirst()];\n}\n"),
    ]);
    let mut edges = Vec::new();
    for f in view.files() {
        for n in view.nodes_in_file(f) {
            let n = cg_core::NodeIdx(n);
            for e in view.out_edges(n).filter(|e| matches!(e.kind, cg_core::EdgeKind::Imports | cg_core::EdgeKind::Instantiates)) {
                edges.push(format!("{} -{:?}-> {}", dump::node_label(&view, n), e.kind, dump::node_key(&view, e.dst)));
            }
        }
    }
    let to = |from: &str, kind: &str, name: &str| {
        edges.iter().filter(|e| e.starts_with(&format!("{from} -{kind}-> ")) && e.contains(&format!("@{name}:"))).collect::<Vec<_>>()
    };
    // `import` and `export .. from` name the declaration that comes first, as upstream does
    for from in ["src/use.ts", "src/index.ts"] {
        let (t, v) = (to(from, "Imports", "TypeFirst"), to(from, "Imports", "ValueFirst"));
        assert!(t.len() == 1 && t[0].contains("#interface@"), "{edges:#?}");
        assert!(v.len() == 1 && v[0].contains("#constant@"), "{edges:#?}");
    }
    // `new` still makes the value
    for name in ["TypeFirst", "ValueFirst"] {
        let made = to("src/use.ts::make", "Instantiates", name);
        assert!(made.len() == 1 && made[0].contains("#constant@"), "{edges:#?}");
    }
}

#[test]
fn typescript_type_parameters_name_no_declaration() {
    let (_d, view) = project(&[
        ("src/output.ts", "export class Output {\n  run() {}\n}\nexport interface T {\n  kind: string;\n}\n"),
        (
            "src/schema.ts",
            "export class Schema<Output> {\n  parse(data: unknown): Output {\n    return data as Output;\n  }\n}\nexport function make<T>(value: T): T {\n  return value;\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    assert!(!edges.iter().any(|e| e.starts_with("src/schema.ts") && e.contains("-> src/output.ts")), "{edges:#?}");
}

/// `from -kind-> path:line#kind@qname` for every edge of the given kinds out of `from`.
fn keyed_edges(view: &View, kinds: &[cg_core::EdgeKind]) -> Vec<String> {
    let mut out = Vec::new();
    for f in view.files() {
        for n in view.nodes_in_file(f) {
            let n = cg_core::NodeIdx(n);
            for e in view.out_edges(n).filter(|e| kinds.contains(&e.kind)) {
                out.push(format!("{} -{:?}-> {}", dump::node_label(view, n), e.kind, dump::node_key(view, e.dst)));
            }
        }
    }
    out
}

#[test]
fn typescript_receiver_calls_name_the_first_overload() {
    let (_d, view) = project(&[(
        "src/doc.ts",
        "type ModeWriter = (doc: Doc) => void;\nexport class Doc {\n  write(fn: ModeWriter): void;\n  write(line: string): void;\n  write(arg: any) {}\n  f(s: string): void;\n  f(n: number, m?: number): void;\n  f(a: any, b?: any): void {}\n  run(n: number) {\n    this.f('x');\n    this.f(n, 2);\n  }\n}\nexport function emit(doc: Doc) {\n  doc.write(`a`);\n  doc.write('b');\n}\n",
    )]);
    let edges = keyed_edges(&view, &[cg_core::EdgeKind::Calls]);
    // through a typed receiver, the method's first declaration stands for its overloads, as upstream links it
    let writes: Vec<_> = edges.iter().filter(|e| e.starts_with("src/doc.ts::emit -Calls-> ")).collect();
    assert!(writes.len() == 2 && writes.iter().all(|w| w.contains("src/doc.ts:3:")), "{edges:#?}");
    // `this.f(..)` keeps the overload its arguments fit
    for line in [6, 7] {
        assert!(edges.iter().any(|e| e.starts_with("src/doc.ts::Doc::run -Calls-> ") && e.contains(&format!("src/doc.ts:{line}:"))), "{edges:#?}");
    }
}

#[test]
fn typescript_new_of_a_bare_interface_links_nothing() {
    let (_d, view) = project(&[
        ("src/types.ts", "export interface File {\n  name: string;\n}\nexport type Blob = { size: number };\n"),
        ("src/use.test.ts", "export function make() {\n  return [new File([], \"a.txt\"), new Blob()];\n}\n"),
    ]);
    let edges = keyed_edges(&view, &[cg_core::EdgeKind::Instantiates, cg_core::EdgeKind::Calls]);
    // the DOM's `File`, not the project's interface: an interface makes no value
    assert!(!edges.iter().any(|e| e.starts_with("src/use.test.ts::make ")), "{edges:#?}");
}

#[test]
fn commonjs_member_call_reaches_the_variable_a_module_exports() {
    let (_d, view) = project(&[
        (
            "lib/express.js",
            "var bodyParser = require('body-parser');\nvar Router = require('router');\nexports = module.exports = createApplication;\nfunction createApplication() {\n  return {};\n}\nexports.Router = Router;\nexports.json = bodyParser.json;\n",
        ),
        ("test/app.js", "var express = require('../lib/express');\nfunction make() {\n  var app = express();\n  express.json();\n  return express.Router();\n}\n"),
        // the package root forwards the whole module
        ("index.js", "module.exports = require('./lib/express');\n"),
        ("test/root.js", "var express = require('..');\nfunction viaRoot() {\n  return express.Router();\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "test/app.js::make -calls-> lib/express.js::createApplication"), "{edges:#?}");
    assert!(has(&edges, "test/app.js::make -calls-> lib/express.js::Router"), "{edges:#?}");
    assert!(has(&edges, "test/root.js::viaRoot -calls-> lib/express.js::Router"), "{edges:#?}");
    // `exports.json = bodyParser.json` is a dependency's function, not the `bodyParser` variable
    assert!(!has(&edges, "test/app.js::make -calls-> lib/express.js::bodyParser"), "{edges:#?}");
}

#[test]
fn javascript_window_global_members_answer_unbound_receivers() {
    let (_d, view) = project(&[
        (
            "static/filter.js",
            "'use strict';\n{\n  window.SelectFilter = {\n    init: function (id) {\n      SelectFilter.refresh(id);\n    },\n    refresh: function (id) {}\n  };\n}\n",
        ),
        ("static/inlines.js", "function update(row) {\n  SelectFilter.init(row);\n}\nfunction shadowed(SelectFilter) {\n  SelectFilter.init(1);\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "static/inlines.js::update -calls-> static/filter.js::window.SelectFilter::init"), "{edges:#?}");
    assert!(has(&edges, "static/filter.js::window.SelectFilter::init -calls-> static/filter.js::window.SelectFilter::refresh"), "{edges:#?}");
    // a parameter of that name is not the global
    assert!(!edges.iter().any(|e| e.starts_with("static/inlines.js::shadowed -calls-> ")), "{edges:#?}");
}

#[test]
fn javascript_default_import_names_the_module_file() {
    let (_d, view) = project(&[
        ("src/anon.ts", "export default function () {\n  return 1;\n}\n"),
        ("src/named.ts", "export default function named() {\n  return 2;\n}\n"),
        ("src/use.ts", "import anon from \"./anon\";\nimport named from \"./named\";\nexport function run() {\n  return anon() + named();\n}\n"),
    ]);
    let edges = edges_of(&view);
    // the statement and the bound name both link the file, as upstream does
    let user = view.files().find(|&f| view.file(f).path() == "src/use.ts").unwrap();
    let mut sites: Vec<_> = view
        .out_edges(view.file(user).rec.node_start)
        .filter(|e| e.kind == cg_core::EdgeKind::Imports)
        .map(|e| format!("{:?}:{:?} {}", e.line, e.col, dump::node_label(&view, e.dst)))
        .collect();
    sites.sort();
    assert_eq!(sites, ["Some(1):Some(0) src/anon.ts", "Some(1):Some(7) src/anon.ts", "Some(2):Some(0) src/named.ts", "Some(2):Some(7) src/named.ts"]);
    // calls still reach the function
    assert!(has(&edges, "src/use.ts::run -calls-> src/named.ts::named"), "{edges:#?}");
}

#[test]
fn rust_tuple_destructuring_types_each_element() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/lib.rs", "pub mod chan;\npub mod user;\n"),
        (
            "src/chan.rs",
            "pub struct Sender;\nimpl Sender {\n    pub fn send(&self) {}\n}\npub struct Receiver;\nimpl Receiver {\n    pub fn recv(&self) {}\n}\npub struct Other;\nimpl Other {\n    pub fn send(&self) {}\n    pub fn recv(&self) {}\n}\npub fn channel() -> (Sender, Receiver) {\n    (Sender, Receiver)\n}\npub fn try_channel() -> Result<(Sender, Receiver), ()> {\n    Ok((Sender, Receiver))\n}\n",
        ),
        (
            "src/user.rs",
            "use crate::chan;\npub fn plain() {\n    let (tx, rx) = chan::channel();\n    tx.send();\n    rx.recv();\n}\npub fn tried() -> Result<(), ()> {\n    let (_, mut rx) = chan::try_channel()?;\n    rx.recv();\n    Ok(())\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    for e in [
        "src/user.rs::plain -calls-> src/chan.rs::Sender::send",
        "src/user.rs::plain -calls-> src/chan.rs::Receiver::recv",
        "src/user.rs::tried -calls-> src/chan.rs::Receiver::recv",
    ] {
        assert!(has(&edges, e), "{e}: {edges:#?}");
    }
    assert!(!edges.iter().any(|e| e.contains("-calls-> src/chan.rs::Other::")), "{edges:#?}");
}

#[test]
fn rust_type_parameters_name_no_same_named_struct() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/lib.rs", "pub mod cell;\nmod spot;\n"),
        (
            "src/cell.rs",
            "pub struct Real;\npub struct Cell<T> {\n    value: T,\n}\nimpl<T> Cell<T> {\n    pub fn new(value: T) -> Option<Box<T>> {\n        None\n    }\n    pub fn map<U>(self, f: U, r: Real) -> U {\n        f\n    }\n}\npub fn free<T>(t: T) {}\n",
        ),
        // Tokio's only `T` is local to a test fn in another file (`bucket_computation_spot_check::T`)
        ("src/spot.rs", "#[cfg(test)]\nmod tests {\n    #[test]\n    fn spot_check() {\n        struct T;\n        struct U;\n    }\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/cell.rs::Cell::map -references-> src/cell.rs::Real"), "{edges:#?}");
    assert!(!edges.iter().any(|e| e.contains("-references->") && (e.ends_with("::T") || e.ends_with("::U"))), "{edges:#?}");
}

#[test]
fn typescript_reexport_lists_import_each_name() {
    let (_d, view) = project(&[
        ("core.ts", "export function _lt() {}\nexport function _gte() {}\nexport function _gt() {}\n"),
        ("checks.ts", "export {\n  _lt as lt,\n  _gte as gte,\n  _gte as minimum,\n} from \"./core\";\nexport { _gt } from \"./core\";\n"),
    ]);
    let edges = edges_of(&view);
    for e in ["checks.ts -imports-> core.ts", "checks.ts -imports-> core.ts::_lt", "checks.ts -imports-> core.ts::_gte", "checks.ts -imports-> core.ts::_gt"] {
        assert!(has(&edges, e), "{e}: {edges:#?}");
    }
}

#[test]
fn commonjs_require_member_binds_the_export() {
    let (_d, view) = project(&[
        ("lib/utils.js", "exports.compileETag = function(val) {\n  return val;\n};\nexports.methods = ['get'];\n"),
        ("lib/trust.js", "exports.compileTrust = function(val) {\n  return val;\n};\n"),
        (
            "lib/application.js",
            "var compileETag = require('./utils').compileETag;\nvar { compileTrust } = require('./trust');\nfunction setup(val) {\n  compileETag(val);\n  compileTrust(val);\n}\nmodule.exports = setup;\n",
        ),
    ]);
    let edges = edges_of(&view);
    // the destructured form already binds the export; the member form must do the same
    assert!(has(&edges, "lib/application.js::setup -calls-> lib/trust.js::compileTrust"), "{edges:#?}");
    assert!(has(&edges, "lib/application.js -imports-> lib/utils.js"), "{edges:#?}");
    assert!(has(&edges, "lib/application.js::setup -calls-> lib/utils.js::compileETag"), "{edges:#?}");
}

#[test]
fn javascript_calls_through_declared_variables_link_the_variable() {
    let (_d, view) = project(&[
        ("admin.js", "\"use strict\";\n{\n    const $ = django.jQuery;\n    function init() {\n        $(document);\n    }\n}\n"),
        ("express.js", "exports.Router = function Router() {};\n"),
        ("router.js", "var express = require('./express')\n  , Router = express.Router;\nfunction make() {\n  return new Router();\n}\n"),
        (
            "bench.ts",
            "function compileFn(x: number) {\n  return (v: number) => v === x;\n}\nconst compiledFn = compileFn(1);\nexport function run() {\n  return compiledFn(2);\n}\nexport function apply(compiledFn: (v: number) => boolean) {\n  return compiledFn(3);\n}\nexport function local() {\n  const g = compileFn(4);\n  return g(5);\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    for e in ["admin.js::init -calls-> admin.js::$", "router.js::make -instantiates-> router.js::Router", "bench.ts::run -calls-> bench.ts::compiledFn"] {
        assert!(has(&edges, e), "{e}: {edges:#?}");
    }
    // a parameter shadows the module's constant; a function-local variable has no node
    assert!(!edges.iter().any(|e| e.starts_with("bench.ts::apply -calls-> ") || e.starts_with("bench.ts::local -calls-> bench.ts::g")), "{edges:#?}");
}

#[test]
fn typescript_new_of_a_parameter_names_no_same_named_class() {
    let (_d, view) = project(&[
        ("util.ts", "export abstract class Class {}\nexport class Widget {}\n"),
        (
            "api.ts",
            "import { Widget } from \"./util\";\nexport function make(Class: new (d: object) => object) {\n  return new Class({});\n}\nexport function build() {\n  return new Widget();\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "api.ts::build -instantiates-> util.ts::Widget"), "{edges:#?}");
    // Zod's `_nonoptional(Class: util.SchemaClass<..>)`: the parameter holds the class
    assert!(!edges.iter().any(|e| e.starts_with("api.ts::make -instantiates-> ")), "{edges:#?}");
}

#[test]
fn typescript_namespace_type_goes_through_the_star_exports() {
    let (_d, view) = project(&[
        ("core/core.ts", "export type output<T> = T;\nexport interface $ZodType {\n  kind: string;\n}\n"),
        ("core/index.ts", "export * from \"./core.js\";\n"),
        ("helpers/errorUtil.ts", "export namespace errorUtil {\n  export type ErrMessage = string | { message?: string };\n}\n"),
        ("helpers/use.ts", "import { errorUtil } from \"./errorUtil.js\";\nexport function min(message?: errorUtil.ErrMessage): void {}\n"),
        ("classic/in-out.ts", "import type * as core from \"../core/index.js\";\nexport type output<T> = core.output<T>;\n"),
        (
            "classic/schemas.ts",
            "import * as core from \"../core/index.js\";\nexport function parse(x: unknown): core.output<number> {\n  return x as core.output<number>;\n}\nexport function kind(t: core.$ZodType): string {\n  return t.kind;\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    // Zod's `core.output<this>` beside classic's own `output` alias
    for e in [
        "classic/schemas.ts::parse -references-> core/core.ts::output",
        "classic/in-out.ts::output -references-> core/core.ts::output",
        // a TS `namespace`'s members are flattened into its file: the path falls back to the last segment
        "helpers/use.ts::min -references-> helpers/errorUtil.ts::ErrMessage",
    ] {
        assert!(has(&edges, e), "{e}: {edges:#?}");
    }
    assert!(!edges.iter().any(|e| e.ends_with("-references-> classic/in-out.ts::output")), "{edges:#?}");
}

#[test]
fn typescript_variable_and_field_annotations_and_enum_members_name_their_types() {
    let (_d, view) = project(&[
        ("core/core.ts", "export type output<T> = T;\n"),
        ("core/index.ts", "export * from \"./core.js\";\n"),
        ("classic/in-out.ts", "import type * as core from \"../core/index.js\";\nexport type output<T> = core.output<T>;\n"),
        (
            "classic/parse.ts",
            "import * as core from \"../core/index.js\";\nconst make = (): any => null;\nexport const parse: (x: unknown) => core.output<number> = make();\nexport class Box {\n  value?: core.output<string>;\n}\n",
        ),
        (
            "v3/types.ts",
            "export enum Kind {\n  ZodLiteral = \"ZodLiteral\",\n}\nexport class ZodLiteral {}\nexport interface ZodLiteralDef {\n  typeName: Kind.ZodLiteral;\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    // Zod's `export const decode: (.., _ctx?: core.ParseContext<..>) => ..`
    assert!(has(&edges, "classic/parse.ts::parse -references-> core/core.ts::output"), "{edges:#?}");
    let field = edges.iter().any(|e| e.starts_with("classic/parse.ts::Box") && e.ends_with("-references-> core/core.ts::output"));
    assert!(field, "{edges:#?}");
    assert!(!edges.iter().any(|e| e.ends_with("-references-> classic/in-out.ts::output")), "{edges:#?}");
    // v3's `typeName: ZodFirstPartyTypeKind.ZodLiteral` beside `class ZodLiteral`
    assert!(has(&edges, "v3/types.ts::ZodLiteralDef::typeName -references-> v3/types.ts::Kind::ZodLiteral"), "{edges:#?}");
    assert!(!edges.iter().any(|e| e.ends_with("-references-> v3/types.ts::ZodLiteral")), "{edges:#?}");
}

fn edges_of(view: &View) -> Vec<String> {
    let mut edges = dump::dump_edges(view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    edges
}

#[test]
fn php_parent_calls_namespaces_and_qualified_names() {
    let (_d, view) = project(&[
        ("src/Base.php", "<?php\nnamespace App;\nclass Base {\n  public function __construct() {}\n  public function go() {}\n}\n"),
        (
            "src/Child.php",
            "<?php\nnamespace App;\nclass Child extends Base {\n  public function __construct() { parent::__construct(); }\n  public function go() { parent::go(); $this->run(); }\n  public function run() {}\n}\n",
        ),
        ("src/Logger.php", "<?php\nnamespace App;\nclass Logger {\n  public static function make() {}\n}\n"),
        ("src/Http/Svc.php", "<?php\nnamespace App\\Http;\nclass Svc {\n  public function run() { \\Vendor\\Lib\\Logger::make(); }\n}\n"),
        ("src/Http/Other.php", "<?php\nnamespace App\\Http;\nuse App\\Logger;\nclass Other {\n  public function run() { \\App\\Logger::make(); Logger::make(); }\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/Child.php::App::Child -extends-> src/Base.php::App::Base"));
    assert!(has(&edges, "src/Child.php::App::Child::__construct -calls-> src/Base.php::App::Base::__construct"), "parent::__construct()");
    assert!(!has(&edges, "src/Child.php::App::Child::__construct -calls-> src/Child.php::App::Child::__construct"), "no self-loop constructor");
    assert!(has(&edges, "src/Child.php::App::Child::go -calls-> src/Base.php::App::Base::go"), "parent::go() from go()");
    assert!(!has(&edges, "src/Child.php::App::Child::go -calls-> src/Child.php::App::Child::go"), "bare go inside go is parent::go, not recursion");
    assert!(has(&edges, "src/Child.php::App::Child::go -calls-> src/Child.php::App::Child::run"), "$this->run()");
    assert!(!edges.iter().any(|e| e.starts_with("src/Http/Svc.php::App\\Http::Svc::run -calls->")), "a vendor class is not a project class: {edges:?}");
    assert!(
        has(&edges, "src/Http/Other.php::App\\Http::Other::run -calls-> src/Logger.php::App::Logger::make"),
        "\\App\\Logger::make() and the imported Logger"
    );
}

#[test]
fn csharp_namespaces_partial_classes_and_determinism() {
    let files: &[(&str, &str)] = &[
        ("src/Util1.cs", "namespace A {\n  public partial class Util {\n    public static void Go() {}\n  }\n}\n"),
        ("src/Util2.cs", "namespace A {\n  public partial class Util {\n    public static void Stop() { Go(); }\n  }\n}\n"),
        ("src/Helper.cs", "namespace A {\n  public class Helper {\n    public void Help() {}\n  }\n}\n"),
        (
            "src/Sub/User.cs",
            "namespace A.B {\n  public class User {\n    public void Run() {\n      Util.Go();\n      var h = new Helper();\n      h.Help();\n    }\n  }\n}\n",
        ),
    ];
    let (_d, view) = project(files);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/Util2.cs::A::Util::Stop -calls-> src/Util1.cs::A::Util::Go"), "member of the other partial declaration");
    assert!(has(&edges, "src/Sub/User.cs::A.B::User::Run -calls-> src/Util1.cs::A::Util::Go"), "type of the enclosing namespace without a using");
    assert!(has(&edges, "src/Sub/User.cs::A.B::User::Run -instantiates-> src/Helper.cs::A::Helper"));
    assert!(has(&edges, "src/Sub/User.cs::A.B::User::Run -calls-> src/Helper.cs::A::Helper::Help"));
    // the pick among cross-file candidates must not depend on scheduling
    let dir = tempfile::tempdir().unwrap();
    for (p, c) in files {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, c).unwrap();
    }
    let a = build_seg(dir.path(), &IndexOptions { threads: Some(1), ..Default::default() }).unwrap();
    let b = build_seg(dir.path(), &IndexOptions { threads: Some(4), ..Default::default() }).unwrap();
    assert_eq!(a.seg.to_bytes(), b.seg.to_bytes());
}

#[test]
fn csharp_names_reach_types_namespaces_and_declared_fields() {
    let (_d, view) = project(&[
        ("src/Unit.cs", "using System.Threading.Tasks;\nnamespace A {\n  public struct Unit {\n    public static Task<Unit> Task => null;\n  }\n}\n"),
        ("src/Details.cs", "using System;\nnamespace A {\n  public class Details {\n    public Type Type { get; }\n  }\n}\n"),
        ("src/Runner.cs", "namespace A.B {\n  public class Runner {\n    public static void Run() {}\n    public void Own() {}\n    public void Write() {}\n    public void Group() { Items(Own); }\n    void Items(object o) {}\n  }\n}\n"),
        (
            "src/User.cs",
            "using System;\nusing System.IO;\nusing System.Threading.Tasks;\nusing A.B;\nnamespace A.C {\n  public class User {\n    private readonly TextWriter _writer;\n    private readonly Runner _runner;\n    public Task Handle(Type t) {\n      _writer.Write();\n      _runner.Own();\n      return Task.Run(() => 1);\n    }\n  }\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    assert!(!edges.iter().any(|e| e.ends_with("-> src/Unit.cs::A::Unit::Task")), "the type Task is not the property Unit.Task: {edges:?}");
    assert!(!edges.iter().any(|e| e.ends_with("-> src/Details.cs::A::Details::Type")), "the type Type is not the property Details.Type: {edges:?}");
    assert!(
        !has(&edges, "src/User.cs::A.C::User::Handle -calls-> src/Runner.cs::A.B::Runner::Run"),
        "Task.Run is the dependency's, not a unique-name guess: {edges:?}"
    );
    assert!(
        !has(&edges, "src/User.cs::A.C::User::Handle -calls-> src/Runner.cs::A.B::Runner::Write"),
        "a field of a dependency's type is no project receiver: {edges:?}"
    );
    assert!(has(&edges, "src/User.cs::A.C::User::Handle -calls-> src/Runner.cs::A.B::Runner::Own"), "implied this on a declared field: {edges:?}");
    assert!(
        edges.iter().any(|e| e.starts_with("src/User.cs") && e.contains("-imports-> ") && e.ends_with("::A.B")),
        "a using names the namespace node: {edges:?}"
    );
    assert!(
        has(&edges, "src/Runner.cs::A.B::Runner::Group -references-> src/Runner.cs::A.B::Runner::Own"),
        "a method group of the enclosing type stays: {edges:?}"
    );
}

#[test]
fn swift_types_are_their_declarations_not_their_extensions() {
    let (_d, view) = project(&[
        ("Sources/App/Session.swift", "open class Session {\n  public init() {}\n  public func request() -> Request { Request() }\n}\n"),
        ("Sources/App/Request.swift", "public class Request {\n  public func validate() {}\n}\n"),
        ("Tests/AHelpers.swift", "extension Session {\n  func helper() {}\n}\nextension Bool {\n  var flag: Bool { self }\n}\nextension DispatchQueue {\n  func hop() {}\n}\n"),
        ("Sources/App/ZNote.swift", "extension Request {\n  func note() {}\n}\n"),
        (
            "Tests/SessionTests.swift",
            "final class Holder {\n  private let session = Session()\n  private let queue: DispatchQueue = .main\n  func make(_ on: Bool) -> Session {\n    let s = Session()\n    s.helper()\n    session.request()\n    queue.hop()\n    return s\n  }\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    assert!(
        has(&edges, "Tests/SessionTests.swift::Holder::make -instantiates-> Sources/App/Session.swift::Session")
            || has(&edges, "Tests/SessionTests.swift::Holder::make -calls-> Sources/App/Session.swift::Session"),
        "Session() is the class: {edges:?}"
    );
    assert!(!edges.iter().any(|e| e.contains("-> Tests/AHelpers.swift::Session") && !e.contains("::Session::")), "never the extension: {edges:?}");
    assert!(
        !edges.iter().any(|e| e.ends_with("-> Tests/AHelpers.swift::Bool") || e.ends_with("-> Tests/AHelpers.swift::DispatchQueue")),
        "an extended dependency type is not a project declaration: {edges:?}"
    );
    assert!(
        has(&edges, "Tests/SessionTests.swift::Holder::make -calls-> Tests/AHelpers.swift::Session::helper"),
        "members added by the extension stay reachable: {edges:?}"
    );
    assert!(has(&edges, "Tests/SessionTests.swift::Holder::make -calls-> Sources/App/Session.swift::Session::request"), "implied self on a field: {edges:?}");
}

#[test]
fn c_macros_are_not_calls_and_macro_namespaces_resolve() {
    let (_d, view) = project(&[
        ("include/fmt/format.h", "#define FMT_BEGIN_NAMESPACE namespace fmt {\n#define FMT_END_NAMESPACE }\n#define CHECK(x) do_check(x)\nFMT_BEGIN_NAMESPACE\ninline int format(int a) { return a; }\ninline int do_check(int a) { return a; }\nFMT_END_NAMESPACE\n"),
        ("src/app.cc", "#include \"fmt/format.h\"\nvoid run() {\n  CHECK(1);\n  fmt::format(1);\n  dep::missing(1);\n}\n"),
        ("src/other.cc", "int missing(int a) { return a; }\n"),
    ]);
    let edges = edges_of(&view);
    assert!(!edges.iter().any(|e| e.contains("-calls-> ") && e.ends_with("::CHECK")), "a macro invocation is not a call: {edges:?}");
    assert!(has(&edges, "src/app.cc::run -calls-> include/fmt/format.h::format"), "a namespace opened by a macro: {edges:?}");
    assert!(!has(&edges, "src/app.cc::run -calls-> src/other.cc::missing"), "a same-named function the file does not include: {edges:?}");
}

#[test]
fn ruby_production_code_does_not_bind_to_spec_helpers() {
    let (_d, view) = project(&[
        ("lib/app.rb", "class App\n  def run\n    use_it 1\n    File.only_here(1)\n  end\nend\n"),
        ("spec/app_spec.rb", "def use_it(x); end\ndef only_here(x); end\nclass Other\n  def only_here(x); end\nend\n"),
    ]);
    let edges = edges_of(&view);
    assert!(!edges.iter().any(|e| e.starts_with("lib/app.rb") && e.contains("-calls-> spec/")), "{edges:?}");
}

#[test]
fn php_this_method_return_types_type_locals() {
    let (_d, view) = project(&[
        ("src/Dispatcher.php", "<?php\nnamespace App;\nclass Dispatcher {\n  public function handle() {}\n}\n"),
        (
            "tests/TestBase.php",
            "<?php\nnamespace Tests;\nuse App\\Dispatcher;\nclass TestBase {\n  protected function make(): Dispatcher { return new Dispatcher(); }\n}\n",
        ),
        (
            "tests/UseTest.php",
            "<?php\nnamespace Tests;\nclass UseTest extends TestBase {\n  public function go() {\n    $d = $this->make();\n    $d->handle();\n  }\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "tests/UseTest.php::Tests::UseTest::go -calls-> src/Dispatcher.php::App::Dispatcher::handle"), "{edges:?}");
}

#[test]
fn cpp_namespace_order_and_out_of_line_members() {
    let (_d, view) = project(&[
        (
            "src/a.cpp",
            "namespace util { void log(int) {} }\nnamespace app {\n  namespace util { void log(int) {} }\n  struct S { void run(); };\n}\nvoid app::S::run() { util::log(1); ::util::log(2); }\n",
        ),
        ("src/foo.h", "struct Foo {\n  void bar();\n  void baz();\n};\n"),
        ("src/foo.cpp", "#include \"foo.h\"\nvoid Foo::bar() {}\nvoid Foo::baz() { bar(); }\n"),
        ("src/use.cpp", "#include \"foo.h\"\nvoid g() { Foo f; f.bar(); }\n"),
        ("src/other/foo2.h", "struct Foo {\n  void bar();\n};\n"),
        ("src/other/foo2.cpp", "#include \"foo2.h\"\nvoid Foo::bar() {}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/a.cpp::app::S::run -calls-> src/a.cpp::app::util::log"), "innermost namespace first");
    assert!(has(&edges, "src/a.cpp::app::S::run -calls-> src/a.cpp::util::log"), "::util::log is absolute");
    assert!(has(&edges, "src/foo.cpp -imports-> src/foo.h"));
    assert!(has(&edges, "src/use.cpp::g -calls-> src/foo.cpp::Foo::bar"), "out-of-line definition in the file that includes the header");
    assert!(!has(&edges, "src/use.cpp::g -calls-> src/other/foo2.cpp::Foo::bar"), "an unrelated Foo");
}

#[test]
fn cpp_declarations_definitions_and_unknown_namespaces() {
    let (_d, view) = project(&[
        ("include/lib/core.h", "namespace lib { namespace detail { void vprint(int); } }\n"),
        ("src/core.cc", "#include \"lib/core.h\"\nnamespace lib { namespace detail { void vprint(int) {} } }\n"),
        ("src/app.cc", "#include \"lib/core.h\"\nnamespace lib { void print() { detail::vprint(1); } }\n"),
        ("src/abort.cc", "struct Death { void Abort() {} };\nvoid g() { posix::Abort(); }\n"),
    ]);
    let edges = edges_of(&view);
    assert!(
        edges.iter().any(|e| e.starts_with("src/app.cc::lib::print -calls-> ") && e.ends_with("::lib::detail::vprint")),
        "a function declared in one file and defined in another resolves: {edges:?}"
    );
    assert!(!has(&edges, "src/abort.cc::g -calls-> src/abort.cc::Death::Abort"), "an unknown namespace is not a unique-name guess");
}

#[test]
fn cpp_global_namespace_and_standard_free_functions() {
    let (_d, view) = project(&[
        ("test/util.h", "inline int helper() { return 1; }\n"),
        ("test/mock.cc", "#include \"util.h\"\nnamespace test { int open(const char* p); }\nint test::open(const char* p) {\n  return ::open(p);\n}\nvoid use() {\n  ::helper();\n  auto s = ::std::string(\"x\");\n}\n"),
        ("test/ranges.cc", "template <int N> int get(int t) { return t; }\n"),
        ("test/other.cc", "struct File { int get() { return 0; } };\nvoid run() {\n  get();\n}\n"),
        ("test/old.c", "int max(int a, int b) { return a > b ? a : b; }\n"),
        ("test/use.c", "int pick(int a, int b) { return max(a, b); }\n"),
    ]);
    let edges = edges_of(&view);
    assert!(
        !edges.iter().any(|e| e.starts_with("test/mock.cc::test::open -calls-> ")),
        "::open is the global POSIX function, not the wrapper itself: {edges:?}"
    );
    assert!(has(&edges, "test/mock.cc::use -calls-> test/util.h::helper"), "::helper() declared in an included header: {edges:?}");
    assert!(!edges.iter().any(|e| e.starts_with("test/mock.cc::use -") && e.contains("string")), "::std::string is the standard library: {edges:?}");
    assert!(!has(&edges, "test/other.cc::run -calls-> test/ranges.cc::get"), "an unqualified get() in C++ is not another file's template: {edges:?}");
    assert!(has(&edges, "test/use.c::pick -calls-> test/old.c::max"), "C keeps project functions named like std ones: {edges:?}");
}

#[test]
fn ruby_requires_and_module_methods() {
    let (_d, view) = project(&[
        ("lib/mygem.rb", "require 'set'\nrequire 'logger'\nrequire_relative 'mygem/util'\nrequire 'mygem/set'\n"),
        ("lib/mygem/util.rb", "module Mygem\n  module Util\n    def self.helper\n    end\n  end\nend\n"),
        ("lib/mygem/set.rb", "module Mygem\n  class Set\n  end\nend\n"),
        ("lib/mygem/logger.rb", "module Mygem\n  class Logger\n  end\nend\n"),
        ("lib/app.rb", "require_relative 'mygem/util'\nclass App\n  def run\n    Mygem::Util.helper\n  end\nend\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "lib/mygem.rb -imports-> lib/mygem/util.rb"), "require_relative");
    assert!(has(&edges, "lib/mygem.rb -imports-> lib/mygem/set.rb"), "require 'mygem/set'");
    assert!(!has(&edges, "lib/mygem.rb -imports-> lib/mygem/logger.rb"), "require 'logger' is the standard library, not lib/mygem/logger.rb");
    assert!(has(&edges, "lib/app.rb::App::run -calls-> lib/mygem/util.rb::Mygem::Util::helper"));
}

#[test]
fn swift_standard_types_versus_project_types() {
    let (_d, view) = project(&[
        ("Sources/App/Task.swift", "struct Task {\n  let title: String\n}\n"),
        ("Sources/App/Ext.swift", "import Foundation\nextension Data {\n  func asString() -> String { \"\" }\n}\n"),
        ("Sources/App/Res.swift", "extension Result {\n  var isSuccess: Bool { false }\n}\n"),
        ("Sources/App/Monitor.swift", "struct PathMonitor {\n  enum Result {\n    case ok\n  }\n  func check() {\n    let r = Result.ok\n    _ = r\n  }\n}\nstruct Throwing {\n  struct Error: Swift.Error {}\n  func fail() throws {\n    throw Error()\n  }\n}\n"),
        ("Sources/App/Main.swift", "import Foundation\nfunc make() {\n  let t = Task(title: \"x\")\n  let d = Data()\n  d.asString()\n  let r = Result { try load() }\n  _ = (t, r)\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(
        edges.iter().any(|e| e.starts_with("Sources/App/Main.swift::make -") && e.ends_with("-> Sources/App/Task.swift::Task")),
        "a project struct named like a standard type is still the project's: {edges:?}"
    );
    assert!(
        !edges.iter().any(|e| e.starts_with("Sources/App/Main.swift::make -") && e.ends_with("-> Sources/App/Ext.swift::Data")),
        "Data() is Foundation's, not the extension"
    );
    assert!(
        !edges.iter().any(|e| e.starts_with("Sources/App/Main.swift::make -") && e.ends_with("-> Sources/App/Res.swift::Result")),
        "Result {{ .. }} is the standard library's init(catching:), not the extension, even with a nested enum Result elsewhere: {edges:?}"
    );
    assert!(
        has(&edges, "Sources/App/Monitor.swift::Throwing::fail -instantiates-> Sources/App/Monitor.swift::Throwing::Error"),
        "a nested type shadows the standard type inside its enclosing declaration: {edges:?}"
    );
    assert!(has(&edges, "Sources/App/Main.swift::make -calls-> Sources/App/Ext.swift::Data::asString"), "members added by the extension are reachable");
}

#[test]
fn mediatr_sends_reach_handlers_and_csharp_supertypes_their_implementations() {
    let (_d, view) = project(&[
        (
            "Api/Orders.cs",
            "class Orders {\n  readonly IMediator _mediator;\n  void Cancel() { var cmd = new CancelOrder(); _mediator.Send(cmd); }\n  void Other(ShipOrder ship) { _mediator.Publish(ship); }\n  void Mail() { _smtp.Send(new CancelOrder()); }\n}\n",
        ),
        (
            "App/Handlers.cs",
            "class CancelOrder {}\nclass ShipOrder {}\nclass CancelHandler : IRequestHandler<CancelOrder, bool> {\n  public bool Handle(CancelOrder c, int t) { return true; }\n}\nclass ShipHandler : IEventHandler<ShipOrder> {\n  public void Handle(ShipOrder s, int t) {}\n}\n",
        ),
        ("App/Shape.cs", "abstract class Shape { public abstract int Area(); }\nclass Square : Shape { public override int Area() { return 1; } }\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "Api/Orders.cs::Orders::Cancel -calls-> App/Handlers.cs::CancelHandler::Handle"), "{edges:?}");
    assert!(has(&edges, "Api/Orders.cs::Orders::Other -calls-> App/Handlers.cs::ShipHandler::Handle"), "{edges:?}");
    assert!(!has(&edges, "Api/Orders.cs::Orders::Mail -calls-> App/Handlers.cs::CancelHandler::Handle"), "{edges:?}");
    assert!(has(&edges, "App/Shape.cs::Shape::Area -calls-> App/Shape.cs::Square::Area"), "{edges:?}");
}

#[test]
fn cpp_macro_namespace_calls_pick_the_overload_their_arguments_fit() {
    let (_d, view) = project(&[
        ("a_style.hpp", "struct style {};\nint format(style s, const char* f) { return 1; }\n"),
        ("b_plain.hpp", "int format(const char* f, int a) { return 2; }\nint size(int n) { return n; }\n"),
        (
            "main.cc",
            "#include \"a_style.hpp\"\n#include \"b_plain.hpp\"\nvoid plain() { ns::format(\"x\", 1); }\nvoid styled(style st, int w) { ns::format(st, w); }\nvoid other() { dep::missing(1); }\n",
        ),
        ("ns.hpp", "#define NS_BEGIN namespace ns {\n"),
        // a module import makes a variable of the root's name, and includes nothing
        ("mod.cc", "import ns;\nvoid imported() { ns::size(3); }\n"),
        ("use.cc", "#include \"b_plain.hpp\"\nvoid measure() { ns::size(3); }\nvoid foreign() { dep::size(3); }\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "main.cc::plain -calls-> b_plain.hpp::format"), "{edges:?}");
    assert!(has(&edges, "main.cc::styled -calls-> a_style.hpp::format"), "{edges:?}");
    assert!(has(&edges, "use.cc::measure -calls-> b_plain.hpp::size"), "{edges:?}");
    assert!(has(&edges, "mod.cc::imported -calls-> b_plain.hpp::size"), "{edges:?}");
    assert!(!edges.iter().any(|e| e.starts_with("main.cc::other -calls->")), "{edges:?}");
    assert!(!edges.iter().any(|e| e.starts_with("use.cc::foreign -calls->")), "no macro opens `dep`: {edges:?}");
}

#[test]
fn cpp_bases_in_macro_namespaces_and_calls_through_field_chains_are_typed() {
    let (_d, view) = project(&[
        (
            "base.h",
            "#define FMT_BEGIN_NAMESPACE namespace fmt {\n#define FMT_END_NAMESPACE }\nFMT_BEGIN_NAMESPACE\nnamespace detail {\ntemplate <typename T> class buffer {\n public:\n  int size() const { return 0; }\n};\n}\nclass file {\n public:\n  void close() {}\n};\nstruct pipe {\n  file read_end;\n};\nFMT_END_NAMESPACE\n",
        ),
        (
            "t.cc",
            "#include \"base.h\"\ntemplate <typename T> struct mock_buffer : fmt::detail::buffer<T> {};\nclass other {\n public:\n  int size() const { return 1; }\n  void close() {}\n};\nvoid use() {\n  mock_buffer<int> buffer;\n  buffer.size();\n  fmt::pipe p;\n  p.read_end.close();\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "t.cc::mock_buffer -extends-> base.h::detail::buffer"), "{edges:?}");
    assert!(has(&edges, "t.cc::use -calls-> base.h::detail::buffer::size"), "{edges:?}");
    assert!(has(&edges, "t.cc::use -calls-> base.h::file::close"), "{edges:?}");
}

#[test]
fn js_this_calls_in_object_literal_methods_reach_the_one_function_of_their_file() {
    let (_d, view) = project(&[
        (
            "src/typeahead.js",
            "var EventEmitter = (function () {\n  function trigger(types) { return types; }\n  return { trigger: trigger };\n})();\nfunction Input() {}\nfunction dup() {}\nvar Other = { dup: function dup() {} };\n_.mixin(Input.prototype, EventEmitter, {\n  _onBlur: function onBlur() {\n    this.trigger('blurred');\n    this.dup();\n    this.elsewhere();\n  },\n});\n",
        ),
        ("src/other.js", "function elsewhere() {}\n"),
    ]);
    let edges = edges_of(&view);
    let from_blur = |to: &str| edges.iter().any(|e| e.contains("onBlur -calls->") && e.ends_with(to));
    assert!(from_blur("-calls-> src/typeahead.js::EventEmitter::trigger"), "{edges:?}");
    assert!(!edges.iter().any(|e| e.contains("onBlur -calls->") && e.contains("::dup")), "two `dup` in the file: {edges:?}");
    assert!(!from_blur("-calls-> src/other.js::elsewhere"), "another file's function: {edges:?}");
}

#[test]
fn ruby_bare_calls_are_method_calls_and_modules_reach_by_name() {
    let (_d, view) = project(&[(
        "lib/app.rb",
        "module Helpers\n  def redirect(uri)\n    uri(uri)\n    halt(302)\n  end\n  def uri(x)\n  end\nend\nclass Base\n  include Helpers\n  def halt(code)\n  end\n  def self.build(x)\n  end\n  def build(x)\n  end\n  def run\n    build(1)\n  end\nend\n",
    )]);
    let edges = edges_of(&view);
    assert!(has(&edges, "lib/app.rb::Helpers::redirect -calls-> lib/app.rb::Helpers::uri"), "a call, not the parameter: {edges:?}");
    assert!(has(&edges, "lib/app.rb::Helpers::redirect -calls-> lib/app.rb::Base::halt"), "a module's self is its includer: {edges:?}");
    let run_build: Vec<&String> = edges.iter().filter(|e| e.starts_with("lib/app.rb::Base::run -calls-> lib/app.rb::Base::build")).collect();
    assert_eq!(run_build.len(), 1, "{edges:?}");
}

#[test]
fn swift_chained_calls_reach_a_member_by_name() {
    let (_d, view) = project(&[
        ("Sources/Session.swift", "class Session {\n  func request() -> Int { return 0 }\n}\nclass DataRequest {\n  func validate() -> Int { return 0 }\n}\n"),
        ("Sources/Use.swift", "func use() {\n  let s = make()\n  s.request().validate()\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "Sources/Use.swift::use -calls-> Sources/Session.swift::Session::request"), "{edges:?}");
    assert!(has(&edges, "Sources/Use.swift::use -calls-> Sources/Session.swift::DataRequest::validate"), "{edges:?}");
}

#[test]
fn swift_extensions_read_their_type_fields_and_a_binding_is_not_its_own_callee() {
    let (_d, view) = project(&[(
        "Sources/Monitor.swift",
        "protocol Monitor {\n  func ping()\n}\nclass Delegate {\n  var observer: (any Monitor)?\n  func lookup(for id: Int) -> Int? { return id }\n}\nextension Delegate {\n  func go() {\n    observer?.ping()\n    guard let lookup = lookup(for: 1) else { return }\n    _ = lookup\n  }\n}\nclass Other {\n  func ping() {}\n}\nextension Monitor {\n  func ping() {}\n}\n",
    )]);
    let edges = edges_of(&view);
    assert!(has(&edges, "Sources/Monitor.swift::Delegate::go -calls-> Sources/Monitor.swift::Monitor::ping"), "the declaration's field: {edges:?}");
    assert!(has(&edges, "Sources/Monitor.swift::Delegate::go -calls-> Sources/Monitor.swift::Delegate::lookup"), "not the binding: {edges:?}");
}

#[test]
fn swift_nested_types_static_members_and_extension_overloads() {
    let (_d, view) = project(&[
        (
            "Sources/Monitor.swift",
            "final class PathMonitor {\n  enum Result {\n    case ok\n  }\n  func last() -> Result { return .ok }\n}\nfunc parse() -> Result<Int, Error> { return .success(1) }\n",
        ),
        (
            "Sources/Session.swift",
            "class Session {\n  func open(to url: String) {}\n}\nextension NotificationCenter {\n  func postRequest(named name: String) {}\n}\nfunc notify() {\n  NotificationCenter.default.postRequest(named: \"x\")\n}\n",
        ),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "Sources/Monitor.swift::PathMonitor::last -references-> Sources/Monitor.swift::PathMonitor::Result"), "{edges:?}");
    assert!(!has(&edges, "Sources/Monitor.swift::parse -references-> Sources/Monitor.swift::PathMonitor::Result"), "nested, not in scope: {edges:?}");
    assert!(has(&edges, "Sources/Session.swift::notify -calls-> Sources/Session.swift::NotificationCenter::postRequest"), "{edges:?}");
}

#[test]
fn cpp_receivers_constructors_and_overrides() {
    let (_d, view) = project(&[
        (
            "lib.hpp",
            "#define LIB_BEGIN namespace lib {\nLIB_BEGIN\nnamespace detail {\nstruct buf { int size() { return 0; } };\ninline auto make() -> buf { return buf(); }\nstruct bigint { bigint() {} bigint(long n) {} };\n}\nusing view = detail::buf;\nclass base { public: virtual int run() { return 0; } };\nclass impl : public base { public: int run() { return 1; } };\nclass holder { detail::buf b_; int len() { return b_.size(); } };\n}\n",
        ),
        (
            "other.hpp",
            "struct wrap { struct Item { int id() { return 1; } }; };\n",
        ),
        (
            "main.cc",
            "#include \"lib.hpp\"\n#include \"other.hpp\"\nusing lib::detail::bigint;\nusing lib::detail::make;\nvoid ctor() { bigint(42); }\nvoid stack() { bigint n(42); }\nvoid by_using() { make(); }\nvoid by_return() { auto b = lib::detail::make(); b.size(); }\nvoid by_alias(lib::view v) { v.size(); }\n",
        ),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "lib.hpp::detail::make -instantiates-> lib.hpp::detail::buf"), "no constructor to call: {edges:?}");
    assert!(edges.iter().any(|e| e.starts_with("main.cc::ctor -calls-> lib.hpp::detail::bigint::bigint")), "{edges:?}");
    assert!(edges.iter().any(|e| e.starts_with("main.cc::stack -calls-> lib.hpp::detail::bigint::bigint")), "{edges:?}");
    assert!(has(&edges, "main.cc::by_using -calls-> lib.hpp::detail::make"), "`using` names the declaration: {edges:?}");
    assert!(has(&edges, "main.cc::by_return -calls-> lib.hpp::detail::buf::size"), "the return type: {edges:?}");
    assert!(has(&edges, "main.cc::by_alias -calls-> lib.hpp::detail::buf::size"), "through the alias: {edges:?}");
    assert!(has(&edges, "lib.hpp::holder::len -calls-> lib.hpp::detail::buf::size"), "a field receiver: {edges:?}");
    assert!(has(&edges, "lib.hpp::base::run -calls-> lib.hpp::impl::run"), "a virtual call's hop: {edges:?}");
}

#[test]
fn javascript_local_class_shadows_a_same_named_outer_class() {
    let src = "class Factory {\n  run() { return \"outer\"; }\n}\n\nexport function top() {\n  const value = new Factory();\n  return value.run();\n}\n\nexport function nested() {\n  class Factory {\n    run() { return \"inner\"; }\n  }\n  const value = new Factory();\n  new Factory().run();\n  return value.run();\n}\n";
    for file in ["app.ts", "app.js"] {
        let (_d, view) = project(&[(file, src)]);
        let edges = edges_of(&view);
        for want in [
            "top -calls-> {f}::Factory::run",
            "top -instantiates-> {f}::Factory",
            "nested -calls-> {f}::nested::Factory::run",
            "nested -instantiates-> {f}::nested::Factory",
        ] {
            assert!(has(&edges, &format!("{file}::{}", want.replace("{f}", file))), "{want}: {edges:#?}");
        }
        assert!(!has(&edges, &format!("{file}::nested -calls-> {file}::Factory::run")), "{edges:#?}");
        assert!(!has(&edges, &format!("{file}::nested -instantiates-> {file}::Factory")), "{edges:#?}");
    }
}

#[test]
fn rust_field_calls_look_through_smart_pointers() {
    for wrap in ["Arc", "Rc", "Box", ""] {
        let field = if wrap.is_empty() { "Worker".to_owned() } else { format!("{wrap}<Worker>") };
        let src = format!(
            "use std::rc::Rc;\nuse std::sync::Arc;\npub struct Worker;\nimpl Worker {{ pub fn execute(&self) -> u8 {{ 7 }} pub fn clone(&self) -> u8 {{ 99 }} }}\npub struct Noise;\nimpl Noise {{ pub fn execute(&self) -> u8 {{ 0 }} }}\npub struct Owner {{ worker: {field} }}\nimpl Owner {{ pub fn run(&self) -> u8 {{ self.worker.execute() }} pub fn dup(&self) {{ self.worker.clone(); }} }}\n"
        );
        let (_d, view) = project(&[("Cargo.toml", "[package]\nname = \"demo\"\n"), ("src/main.rs", &src)]);
        let edges = edges_of(&view);
        assert!(has(&edges, "src/main.rs::Owner::run -calls-> src/main.rs::Worker::execute"), "{wrap}: {edges:#?}");
        // `Arc::clone` / `Rc::clone` clone the pointer, not the value
        let clones = has(&edges, "src/main.rs::Owner::dup -calls-> src/main.rs::Worker::clone");
        if wrap != "Box" {
            assert!(clones == wrap.is_empty(), "{wrap}: {edges:#?}");
        }
    }
}

#[test]
fn python_members_follow_the_c3_method_resolution_order() {
    let (_d, view) = project(&[
        ("swap.py", "class Left:\n    def run(self):\n        return 'left'\n\nclass Right:\n    def run(self):\n        return 'right'\n\nclass Derived(Right, Left):\n    def invoke(self):\n        return self.run()\n\ndef caller():\n    value = Derived()\n    return value.run()\n"),
        ("deep.py", "class Root:\n    def run(self):\n        return 'root'\n\nclass Right:\n    def run(self):\n        return 'right'\n\nclass Left(Root):\n    pass\n\nclass Derived(Left, Right):\n    def invoke(self):\n        return self.run()\n"),
        // C3, not depth-first: D -> B -> C -> A
        ("diamond.py", "class A:\n    def run(self):\n        return 'a'\n\nclass B(A):\n    pass\n\nclass C(A):\n    def run(self):\n        return 'c'\n\nclass D(B, C):\n    def invoke(self):\n        return self.run()\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "swap.py::Derived::invoke -calls-> swap.py::Right::run"), "{edges:#?}");
    assert!(has(&edges, "swap.py::caller -calls-> swap.py::Right::run"), "{edges:#?}");
    assert!(has(&edges, "deep.py::Derived::invoke -calls-> deep.py::Root::run"), "{edges:#?}");
    assert!(has(&edges, "diamond.py::D::invoke -calls-> diamond.py::C::run"), "{edges:#?}");
}

#[test]
fn rust_inherent_method_shadows_a_trait_method() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        (
            "src/main.rs",
            "mod a_trait;\nmod z_inherent;\nmod same;\n\npub trait Action { fn run(&self) -> u8; }\npub struct Worker;\n\npub fn direct(w: &Worker) -> u8 { w.run() }\npub fn associated(w: &Worker) -> u8 { Worker::run(w) }\n",
        ),
        ("src/a_trait.rs", "use crate::{Action, Worker};\nimpl Action for Worker { fn run(&self) -> u8 { 1 } }\n"),
        ("src/z_inherent.rs", "use crate::Worker;\nimpl Worker { pub fn run(&self) -> u8 { 2 } }\n"),
        // both impls beside the type, the trait's first
        ("src/same.rs", "use crate::Action;\npub struct Job;\nimpl Action for Job { fn run(&self) -> u8 { 1 } }\nimpl Job { pub fn run(&self) -> u8 { 2 } }\npub fn go(j: &Job) -> u8 { j.run() }\npub trait Speak { fn speak(&self); }\nimpl Speak for Job { fn speak(&self) {} }\npub fn talk(j: &Job) { j.speak() }\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/main.rs::direct -calls-> src/z_inherent.rs::Worker::run"), "{edges:#?}");
    assert!(has(&edges, "src/main.rs::associated -calls-> src/z_inherent.rs::Worker::run"), "{edges:#?}");
    assert!(!edges.iter().any(|e| e.ends_with("-calls-> src/a_trait.rs::Worker::run")), "{edges:#?}");
    let go: Vec<_> = keyed_edges(&view, &[cg_core::EdgeKind::Calls]).into_iter().filter(|e| e.starts_with("src/same.rs::go ")).collect();
    assert!(go.len() == 1 && go[0].contains("src/same.rs:4:"), "{go:#?}");
    // with no inherent method of the name, the trait's
    assert!(has(&edges, "src/same.rs::talk -calls-> src/same.rs::Job::speak"), "{edges:#?}");
}

/// `dump_edges` with the target's start line, which tells overloads apart.
fn edges_with_target_lines(view: &View) -> Vec<String> {
    let mut out = Vec::new();
    for f in view.files() {
        for n in view.nodes_in_file(f) {
            let n = cg_core::NodeIdx(n);
            for e in view.out_edges(n) {
                if e.kind != cg_core::EdgeKind::Contains {
                    out.push(format!("{} -{}-> {}:{}", dump::node_label(view, n), e.kind, dump::node_label(view, e.dst), view.node(e.dst).start_line()));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn java_overloads_fields_super_and_type_parameters() {
    let (_d, view) = project(&[
        (
            "src/p/Box.java",
            r#"package p;
public class Box {
  private int size;
  public int size() { return size; }
  public void put(Object a) {}
  public void put(Object a, Object b) {}
  public void log() {}
  public void log(String fmt, Object... args) {}
  public <T> T pick(T value) { return value; }
  public int run() { put(1, 2); log("a", 1, 2); return size(); }
}
"#,
        ),
        ("src/p/E.java", "package p;\npublic enum E { T, U }\n"),
        ("src/p/Base.java", "package p;\npublic class Base { public void close() {} }\n"),
        ("src/p/Child.java", "package p;\npublic class Child extends Base {\n  @Override public void close() { super.close(); }\n}\n"),
        ("src/p/Builder.java", "package p;\npublic interface Builder<R> { R build(); }\n"),
        (
            "src/p/Maker.java",
            "package p;\npublic class Maker implements Builder<Integer> {\n  public Integer build() { return 1; }\n  public static class Builder {}\n}\n",
        ),
    ]);
    let edges = edges_with_target_lines(&view);
    for e in &edges {
        eprintln!("{e}");
    }
    let run = "src/p/Box.java::p::Box::run";
    assert!(has(&edges, &format!("{run} -calls-> src/p/Box.java::p::Box::put:6")), "two arguments pick the two-parameter overload");
    assert!(!has(&edges, &format!("{run} -calls-> src/p/Box.java::p::Box::put:5")));
    assert!(has(&edges, &format!("{run} -calls-> src/p/Box.java::p::Box::log:8")), "varargs overload");
    assert!(has(&edges, &format!("{run} -calls-> src/p/Box.java::p::Box::size:4")), "a call names the method, not the field");
    assert!(!has(&edges, &format!("{run} -calls-> src/p/Box.java::p::Box::size:3")));
    let close = "src/p/Child.java::p::Child::close";
    assert!(has(&edges, &format!("{close} -calls-> src/p/Base.java::p::Base::close:2")), "super.close() is the parent's");
    assert!(!has(&edges, &format!("{close} -calls-> {close}:3")), "super.close() is not recursion");
    assert!(!edges.iter().any(|e| e.contains("-references-> src/p/E.java::p::E::T")), "a type parameter is not the enum constant T");
    assert!(
        has(&edges, "src/p/Maker.java::p::Maker -implements-> src/p/Builder.java::p::Builder:2"),
        "a supertype clause cannot name the class's own member type"
    );
}

#[test]
fn java_duplicate_source_roots_resolve_within_their_own_root() {
    let closer = "package q;\npublic final class Closer {\n  public static Closer create() { return new Closer(); }\n  public void close() {}\n}\n";
    let user = "package q;\nclass User {\n  void run() {\n    Closer closer = Closer.create();\n    closer.close();\n  }\n}\n";
    let (_d, view) = project(&[
        ("lib/src/q/Closer.java", closer),
        ("lib/src/q/User.java", user),
        ("android/lib/src/q/Closer.java", closer),
        ("android/lib/src/q/User.java", user),
        ("lib-tests/test/q/UserTest.java", "package q;\nclass UserTest {\n  void t() { Closer.create().close(); }\n}\n"),
    ]);
    let edges = dump::dump_edges(&view);
    for e in &edges {
        eprintln!("{e}");
    }
    for root in ["lib", "android/lib"] {
        let run = format!("{root}/src/q/User.java::q::User::run");
        assert!(has(&edges, &format!("{run} -calls-> {root}/src/q/Closer.java::q::Closer::create")), "{root}: same-package type in its own root");
        assert!(has(&edges, &format!("{run} -calls-> {root}/src/q/Closer.java::q::Closer::close")), "{root}: typed local");
    }
    assert!(
        has(&edges, "lib-tests/test/q/UserTest.java::q::UserTest::t -calls-> lib/src/q/Closer.java::q::Closer::create"),
        "the nearest root by directory distance"
    );
}

#[test]
fn rust_inline_test_modules_self_init_calls_and_guesses() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/lib.rs", "macro_rules! cfg_net { ($($i:item)*) => { $($i)* } }\npub mod net;\npub mod app;\n"),
        (
            "src/net.rs",
            r#"cfg_net! {
    pub struct Sock { fd: i32 }
}
impl Sock {
    pub fn bind() -> Sock { Sock { fd: 0 } }
    pub fn send(&self) { self.flush(); }
    fn flush(&self) {}
    pub fn sync_all(&self) {}
}
"#,
        ),
        (
            "src/app.rs",
            r#"use crate::net::Sock;
fn rt() -> u32 { 1 }
pub fn run() {
    let s = Sock::bind();
    let rt = rt();
}
pub struct File { std: u32 }
impl File {
    pub fn sync_all(&self) { let std = self.std; std.sync_all(); }
}
"#,
        ),
        ("tests/it.rs", "mod support {\n    pub mod helper;\n}\nuse support::helper::assist;\n#[test]\nfn t() { assist(); }\n"),
        ("tests/support/helper.rs", "pub fn assist() {}\n"),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "src/app.rs::run -calls-> src/app.rs::rt"), "`let rt = rt()` calls the function");
    assert!(!has(&edges, "src/app.rs::File::sync_all -calls-> src/app.rs::File::sync_all"), "a name-only guess is never the caller");
    assert!(has(&edges, "tests/it.rs::t -calls-> tests/support/helper.rs::assist"), "inline `mod support {{ pub mod helper; }}` in a test crate");
}
