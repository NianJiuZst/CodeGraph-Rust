//! End-to-end resolution tests covering multi-language precision & recall upgrades:
//! Kotlin fields / fluent builders / member extensions, Rust crate-self aliases /
//! wildcard submodules / inline submodules / Pin<P>, Python class-attr factories /
//! `with ... as`, Go package vars / callable vars, C# DI service getters / method
//! group filtering, PHP case-insensitive methods / `__DIR__` / `DOCUMENT_ROOT`,
//! and declaration-only C++ headers.

use cg_resolve::{build_seg, IndexOptions};
use cg_store::{dump, View};
use std::sync::Arc;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, View) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let p = dir.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, body).unwrap();
    }
    let seg = build_seg(dir.path(), &IndexOptions { threads: Some(2), ..Default::default() }).unwrap().seg;
    let view = View::new(Arc::new(seg)).with_root(dir.path().to_path_buf());
    (dir, view)
}

fn index_edges(files: &[(&str, &str)]) -> Vec<String> {
    let (_dir, view) = project(files);
    dump::dump_edges(&view)
}

fn has(edges: &[String], needle: &str) {
    assert!(edges.iter().any(|e| e == needle), "missing {needle}\n{edges:#?}");
}

fn lacks(edges: &[String], needle: &str) {
    assert!(!edges.iter().any(|e| e == needle), "unexpected {needle}\n{edges:#?}");
}

#[test]
fn kotlin_field_calls_builder_chains_and_member_extensions() {
    let e = index_edges(&[
        (
            "src/Client.kt",
            r#"package demo

interface Call {
    fun execute(): Response
}

class Response(val body: ResponseBody)

class ResponseBody {
    fun string(): String = ""
}

class OkHttpClient {
    fun newCall(url: String): Call = TODO()
    fun newBuilder(): Builder = Builder()
    class Builder {
        fun timeout(ms: Int): Builder = this
        fun build(): OkHttpClient = OkHttpClient()
    }
}
"#,
        ),
        (
            "src/ClientTest.kt",
            r#"package demo

class ClientTest {
    private lateinit var client: OkHttpClient

    fun run() {
        val call = client.newCall("https://example.com")
        val resp = call.execute()
        val body = resp.body
        body.string()

        val custom = client.newBuilder().timeout(500).build()
        custom.newCall("https://other.com")

        val built = OkHttpClient.Builder().timeout(100).build()
        built.newCall("https://third.com")

        call.assertDone()
    }

    private fun Call.assertDone() {}
}
"#,
        ),
    ]);
    assert_eq!(
        e.iter().filter(|x| x.as_str() == "src/ClientTest.kt::demo::ClientTest::run -calls-> src/Client.kt::demo::OkHttpClient::newCall").count(),
        3,
        "{e:#?}"
    );
    has(&e, "src/ClientTest.kt::demo::ClientTest::run -calls-> src/Client.kt::demo::Call::execute");
    has(&e, "src/ClientTest.kt::demo::ClientTest::run -calls-> src/Client.kt::demo::ResponseBody::string");
    has(&e, "src/ClientTest.kt::demo::ClientTest::run -calls-> src/Client.kt::demo::OkHttpClient::newBuilder");
    has(&e, "src/ClientTest.kt::demo::ClientTest::run -calls-> src/Client.kt::demo::OkHttpClient::Builder::build");
    has(&e, "src/ClientTest.kt::demo::ClientTest::run -calls-> src/ClientTest.kt::Call::assertDone");
}

#[test]
fn rust_crate_self_alias_wildcard_submod_inline_mod_and_pin() {
    let e = index_edges(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"helper\", \"app\"]\n"),
        ("helper/Cargo.toml", "[package]\nname = \"my-helper\"\nversion = \"0.1.0\"\n"),
        ("helper/src/lib.rs", "pub mod task;\npub fn boot() {}\n"),
        ("helper/src/task.rs", "pub struct Spawn;\nimpl Spawn { pub fn is_woken(&self) -> bool { true } }\npub fn spawn() -> Spawn { Spawn }\n"),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n"),
        ("app/src/lib.rs", "pub struct Inner;\nimpl Inner { pub fn ping(&self) {} }\n"),
        ("app/tests/support/mpsc.rs", "pub struct Sender;\nimpl Sender { pub fn send(&self) {} }\npub fn channel() -> Sender { Sender }\n"),
        (
            "app/tests/it.rs",
            r#"use std::pin::Pin;
use my_helper::{self as mh, *};
use app::Inner;

mod support {
    pub(crate) mod mpsc;
}
use support::mpsc;

fn check(pinned: Pin<Box<Inner>>) {
    mh::boot();
    let s = task::spawn();
    s.is_woken();
    let tx = mpsc::channel();
    tx.send();
    pinned.ping();
}
"#,
        ),
    ]);
    has(&e, "app/tests/it.rs::check -calls-> helper/src/lib.rs::boot");
    has(&e, "app/tests/it.rs::check -calls-> helper/src/task.rs::spawn");
    has(&e, "app/tests/it.rs::check -calls-> helper/src/task.rs::Spawn::is_woken");
    has(&e, "app/tests/it.rs::check -calls-> app/tests/support/mpsc.rs::channel");
    has(&e, "app/tests/it.rs::check -calls-> app/tests/support/mpsc.rs::Sender::send");
    has(&e, "app/tests/it.rs::check -calls-> app/src/lib.rs::Inner::ping");
}

#[test]
fn python_class_attr_factory_and_with_as_binding() {
    let e = index_edges(&[
        (
            "app/client.py",
            "class Client:\n    def get(self, url):\n        return url\n\nclass Editor:\n    def create_model(self):\n        pass\n\ndef open_editor() -> Editor:\n    return Editor()\n",
        ),
        (
            "app/base.py",
            "from app.client import Client\n\nclass BaseTest:\n    client_class = Client\n    @classmethod\n    def _pre_setup(cls):\n        cls.client = cls.client_class()\n",
        ),
        (
            "app/test_it.py",
            "from app.base import BaseTest\nfrom app.client import open_editor\n\nclass MyTest(BaseTest):\n    def test_ok(self):\n        self.client.get('/home')\n        with open_editor() as ed:\n            ed.create_model()\n",
        ),
    ]);
    has(&e, "app/test_it.py::MyTest::test_ok -calls-> app/client.py::Client::get");
    has(&e, "app/test_it.py::MyTest::test_ok -calls-> app/client.py::Editor::create_model");
}

#[test]
fn go_package_var_concrete_binding_and_callable_var() {
    let e = index_edges(&[
        ("go.mod", "module example.com/demo\ngo 1.22\n"),
        (
            "binding/binding.go",
            "package binding\ntype Binding interface { Bind() }\nvar JSON Binding = jsonBinding{}\ntype jsonBinding struct{}\nfunc (jsonBinding) Bind() {}\n",
        ),
        ("binding/binding_test.go", "package binding\nfunc TestJSON() { JSON.Bind() }\n"),
        (
            "app/main.go",
            "package main\nimport \"example.com/demo/binding\"\nvar engine = func() int { return 1 }\nfunc Run() { _ = engine(); binding.JSON.Bind() }\n",
        ),
    ]);
    has(&e, "binding/binding_test.go::TestJSON -calls-> binding/binding.go::jsonBinding::Bind");
    has(&e, "app/main.go::Run -calls-> app/main.go::engine");
    has(&e, "app/main.go::Run -calls-> binding/binding.go::jsonBinding::Bind");
}

#[test]
fn csharp_di_service_getter_and_method_group_filtering() {
    let e = index_edges(&[(
        "src/MediatR.cs",
        r#"namespace Demo;
public interface ISender { void Send(object req); }
public interface IMediator : ISender { }
public class Config { public string LicenseKey { get; set; } = ""; }
public class Runner {
    void Inspect(object x) {}
    public void Go(IServiceProvider sp, Config cfg) {
        var mediator = sp.GetRequiredService<IMediator>();
        mediator.Send(cfg);
        Inspect(cfg.LicenseKey);
    }
}
"#,
    )]);
    has(&e, "src/MediatR.cs::Demo::Runner::Go -calls-> src/MediatR.cs::Demo::ISender::Send");
    lacks(&e, "src/MediatR.cs::Demo::Runner::Go -references-> src/MediatR.cs::Demo::Config::LicenseKey");
}

#[test]
fn php_case_insensitive_methods_dir_and_document_root_includes() {
    let e = index_edges(&[
        ("codegraph.json", r#"{"webRoot": "public"}"#),
        ("public/inc/header.php", "<?php\nfunction renderHeader() {}\n"),
        ("public/lib/util.php", "<?php\nclass Greeter { public function sayHello() {} }\n"),
        (
            "public/pages/home.php",
            r#"<?php
require __DIR__ . '/../lib/util.php';
function boot() {
    require $_SERVER['DOCUMENT_ROOT'] . '/inc/header.php';
    $g = new Greeter();
    $g->SAYHELLO();
}
"#,
        ),
    ]);
    has(&e, "public/pages/home.php -imports-> public/lib/util.php");
    has(&e, "public/pages/home.php::boot -imports-> public/inc/header.php");
    has(&e, "public/pages/home.php::boot -calls-> public/lib/util.php::Greeter::sayHello");
}

#[test]
fn php_auto_inferred_document_root_without_config() {
    let e = index_edges(&[
        ("www/inc/a.php", "<?php\nfunction a() {}\n"),
        ("www/inc/b.php", "<?php\nfunction b() {}\n"),
        ("www/inc/c.php", "<?php\nfunction c() {}\n"),
        (
            "www/pages/index.php",
            "<?php\nrequire $_SERVER['DOCUMENT_ROOT'] . '/inc/a.php';\nrequire $_SERVER['DOCUMENT_ROOT'] . '/inc/b.php';\nrequire $_SERVER['DOCUMENT_ROOT'] . '/inc/c.php';\n",
        ),
    ]);
    has(&e, "www/pages/index.php -imports-> www/inc/a.php");
    has(&e, "www/pages/index.php -imports-> www/inc/b.php");
    has(&e, "www/pages/index.php -imports-> www/inc/c.php");
}

#[test]
fn cpp_declaration_only_header_detected_as_cpp() {
    let (_dir, view) = project(&[
        ("api.h", "#pragma once\nextern CComPtr<IDiaDataSource> g_source;\nbool load_symbol(const std::string &name, int depth = 0);\n"),
        ("api.cc", "#include \"api.h\"\nbool load_symbol(const std::string &name, int depth) { return depth == 0; }\n"),
        ("use.cc", "#include \"api.h\"\nvoid run(const std::string &s) { load_symbol(s); }\n"),
    ]);
    let f = view.file_by_path("api.h").expect("api.h indexed");
    assert_eq!(view.file(f).lang(), cg_core::Lang::Cpp);
    assert_eq!(view.file(f).flags() & cg_extract::fileflags::PARSE_ERROR, 0, "C++ header should parse cleanly");
    let edges = dump::dump_edges(&view);
    has(&edges, "use.cc::run -calls-> api.cc::load_symbol");
}
