//! End-to-end resolution tests for deep Rust generics/pin_project/traits,
//! Kotlin fluent builders/scope lambdas/DSL receivers/extensions, and Python
//! default params/@cached_property/decorator self-injection/Django proxies.

use cg_resolve::{build_seg, IndexOptions};
use cg_store::{dump, View};
use std::sync::Arc;

fn index_edges(files: &[(&str, &str)]) -> Vec<String> {
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
    dump::dump_edges(&view)
}

fn has_call(edges: &[String], from: &str, to: &str) {
    let exact = format!("{from} -calls-> {to}");
    assert!(edges.iter().any(|e| e == &exact), "missing `{exact}`\n{edges:#?}");
}

fn lacks_call(edges: &[String], from: &str, to: &str) {
    let exact = format!("{from} -calls-> {to}");
    assert!(!edges.iter().any(|e| e == &exact), "unexpected `{exact}`\n{edges:#?}");
}

#[test]
fn rust_pin_project_generic_deref_extension_traits_and_inline_mod() {
    let e = index_edges(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"),
        (
            "src/lib.rs",
            r#"pub mod ext;
pub mod mocks;
pub mod wrapper;

pub trait Source {
    fn poll_ready(&mut self);
}

pub struct Queue;
impl Queue {
    pub fn with_capacity(_n: usize) -> Self { Queue }
    pub fn insert_at(&mut self, _v: u32) {}
}

pub struct Spawn<T> {
    inner: T,
}
impl<T> std::ops::Deref for Spawn<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.inner }
}
impl<T> Spawn<T> {
    pub fn poll(&mut self) {}
}
pub fn spawn<T>(inner: T) -> Spawn<T> {
    Spawn { inner }
}

pub struct Guarded<T>(T);
impl<T: Source> Guarded<T> {
    pub fn drive(&mut self) {
        self.0.poll_ready();
    }
}

pub struct Request;
impl Request {
    pub fn status(&self) {}
}
pub struct Response;
impl Response {
    pub fn status(&self) {}
}
pub fn send(_req: Request) -> Spawn<Response> {
    Spawn { inner: Response }
}

pub fn top_helper() {}

mod alpha {
    pub fn run() {}
}
mod beta {
    pub fn run() {}
}

#[cfg(test)]
mod tests {
    use super::top_helper;
    use crate::ext::StreamExt;
    use crate::mocks::*;
    use crate::{send, spawn, Queue, Request};

    #[test]
    fn exercise_all() {
        top_helper();
        pool::run_one();
        let mut q = spawn(Queue::with_capacity(4));
        q.poll();
        q.insert_at(7);
        let mapped = crate::ext::make_stream().map(|x| x);
        mapped.is_terminated();
        // Unrelated type not implementing `Stream` must NOT match `StreamExt::map`.
        crate::ext::make_plain().map(|x| x);
        // Factory returning `Spawn<Response>` must deref to `Response`, NOT its `Request` arg.
        let sent = send(Request);
        sent.status();
    }

    #[test]
    fn sibling_inline_mods() {
        crate::alpha::run();
        crate::beta::run();
    }
}
"#,
        ),
        (
            "src/wrapper.rs",
            r#"use pin_project_lite::pin_project;

pub struct Inner;
impl Inner {
    pub fn tick(&mut self) {}
}

pin_project! {
    pub struct Framed {
        #[pin]
        inner: Inner,
    }
}

impl Framed {
    pub fn step(mut self: std::pin::Pin<&mut Self>) {
        let me = self.as_mut().project();
        me.inner.tick();
    }
}
"#,
        ),
        (
            "src/ext.rs",
            r#"pub trait Stream {}
pub struct Iter;
impl Stream for Iter {}
pub struct Plain;
pub struct Map;
impl Map {
    pub fn is_terminated(&self) -> bool { false }
}
pub trait StreamExt {
    fn map<F>(self, f: F) -> Map;
}
impl<T: Stream> StreamExt for T {}
pub fn make_stream() -> Iter { Iter }
pub fn make_plain() -> Plain { Plain }
"#,
        ),
        (
            "src/mocks.rs",
            r#"pub(super) mod pool {
    pub fn run_one() {}
}
"#,
        ),
    ]);

    has_call(&e, "src/lib.rs::Guarded::drive", "src/lib.rs::Source::poll_ready");
    has_call(&e, "src/lib.rs::exercise_all", "src/lib.rs::top_helper");
    has_call(&e, "src/lib.rs::exercise_all", "src/mocks.rs::run_one");
    has_call(&e, "src/lib.rs::exercise_all", "src/lib.rs::Spawn::poll");
    has_call(&e, "src/lib.rs::exercise_all", "src/lib.rs::Queue::insert_at");
    has_call(&e, "src/lib.rs::exercise_all", "src/ext.rs::StreamExt::map");
    has_call(&e, "src/lib.rs::exercise_all", "src/ext.rs::Map::is_terminated");
    has_call(&e, "src/lib.rs::exercise_all", "src/lib.rs::Response::status");
    lacks_call(&e, "src/lib.rs::exercise_all", "src/lib.rs::Request::status");
    has_call(&e, "src/wrapper.rs::Framed::step", "src/wrapper.rs::Inner::tick");
    // Only `make_stream().map()` hits `StreamExt::map` (count == 1, NOT 2 from `make_plain()`).
    let map_calls = e.iter().filter(|l| l.as_str() == "src/lib.rs::exercise_all -calls-> src/ext.rs::StreamExt::map").count();
    assert_eq!(map_calls, 1, "Plain must not resolve StreamExt::map\n{e:#?}");
}

#[test]
fn sibling_rust_inline_modules_resolve_distinct_targets() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("Cargo.toml", "[package]\nname = \"sib\"\nversion = \"0.1.0\"\n"),
        (
            "src/lib.rs",
            "mod alpha {\n    pub fn run() {}\n}\nmod beta {\n    pub fn run() {}\n}\npub fn call_both() {\n    alpha::run();\n    beta::run();\n}\n",
        ),
    ];
    for (rel, body) in files {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }
    let seg = build_seg(dir.path(), &IndexOptions { threads: Some(2), ..Default::default() }).unwrap().seg;
    let view = View::new(Arc::new(seg)).with_root(dir.path().to_path_buf());
    let f = view.files().find(|f| view.file(*f).path() == "src/lib.rs").unwrap();
    let mut target_lines: Vec<u32> = view
        .refs_of_file(f)
        .iter()
        .filter(|r| matches!(view.text_str(r.name), "alpha::run" | "beta::run"))
        .map(|r| {
            assert_eq!(r.status, cg_store::status::RESOLVED, "unresolved {}", view.text_str(r.name));
            view.node(r.target).start_line()
        })
        .collect();
    target_lines.sort();
    assert_eq!(target_lines, vec![2, 5], "alpha::run and beta::run must resolve to distinct functions");
}

#[test]
fn kotlin_fluent_builders_scope_lambdas_dsl_and_extensions() {
    let e = index_edges(&[
        (
            "src/demo/Http.kt",
            r#"package demo

interface Dns {
  fun lookup(host: String): String
  companion object {
    val SYSTEM: Dns = SystemDns()
  }
}

class SystemDns : Dns {
  override fun lookup(host: String): String = host
}

class ResponseBody {
  fun string(): String = "ok"
}

class Response(val body: ResponseBody) {
  fun close() {}
}

class Call {
  fun execute(): Response = Response(ResponseBody())
  fun clone(): Call = this
}

class Client {
  fun newCall(): Call = Call()
  fun newBuilder(): Builder = Builder()

  class Builder {
    internal var dns: Dns = Dns.SYSTEM
    internal var host: String = ""

    fun dns(dns: Dns) = apply { this.dns = dns }
    fun host(host: String) = apply { this.host = host }
    fun build(): Client = Client()
  }

  companion object {
    fun String.toUrl(): HttpUrl = HttpUrl()
  }
}

class HttpUrl {
  fun hostName(): String = "example.com"
}

class StageB {
  fun next(): StageC = StageC()
}

class StageC {
  fun next(): Unit {}
}

class StageA {
  fun fetch(): StageB = StageB()
}
"#,
        ),
        (
            "src/demo/internal/Ext.kt",
            r#"package demo.internal

import demo.Response

fun Response.closeQuietly() {
  close()
}
"#,
        ),
        (
            "src/demo/Dsl.kt",
            r#"package demo

class Tester {
  fun makeCall(): Call = Call()
}

fun runDsl(block: Tester.() -> Unit) {
  Tester().block()
}
"#,
        ),
        (
            "src/demo/App.kt",
            r#"package demo.app

import demo.Client
import demo.Client.Companion.toUrl
import demo.Dns
import demo.StageA
import demo.internal.closeQuietly
import demo.runDsl

class App {
  fun run(raw: String, a: StageA) {
    a.fetch().next().next()
    val host = Dns.SYSTEM.lookup("example.com")
    val url = raw.toUrl()
    url.hostName()

    val client =
      Client
        .Builder()
        .dns(Dns.SYSTEM)
        .host(host)
        .host("second")
        .build()

    client.newBuilder().apply {
      dns(Dns.SYSTEM)
      host("applied")
    }.build().newCall().execute().use { response ->
      response.body.string()
      response.closeQuietly()
    }

    runDsl {
      val c = makeCall()
      c.clone().execute().body.string()
    }
  }
}
"#,
        ),
    ]);

    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::Dns::lookup");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::String::toUrl");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::HttpUrl::hostName");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::Client::Builder::dns");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::Client::Builder::host");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::Client::Builder::build");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::Client::newCall");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::Call::execute");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::ResponseBody::string");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/internal/Ext.kt::Response::closeQuietly");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Dsl.kt::demo::Tester::makeCall");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::Call::clone");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::StageB::next");
    has_call(&e, "src/demo/App.kt::demo.app::App::run", "src/demo/Http.kt::demo::StageC::next");
}

#[test]
fn python_default_params_properties_decorator_injection_and_django_proxies() {
    let e = index_edges(&[
        ("requirements.txt", "django>=5.0\n"),
        ("django/__init__.py", ""),
        ("django/db/backends/__init__.py", ""),
        ("django/db/backends/base/__init__.py", ""),
        ("django/db/models/__init__.py", ""),
        (
            "django/db/backends/base/schema.py",
            r#"class BaseDatabaseSchemaEditor:
    def create_model(self, model):
        pass
"#,
        ),
        (
            "django/db/backends/utils.py",
            r#"class CursorWrapper:
    def execute(self, sql):
        pass
"#,
        ),
        (
            "django/db/backends/base/base.py",
            r#"class BaseDatabaseWrapper:
    SchemaEditorClass = None
    def schema_editor(self):
        return self.SchemaEditorClass(self)
    def cursor(self):
        pass
"#,
        ),
        (
            "django/db/models/query.py",
            r#"class QuerySet:
    def _clone(self):
        c = self.__class__()
        return c
    def filter(self, **kwargs):
        clone = self._clone()
        return clone
    def count(self):
        return 0
"#,
        ),
        (
            "django/db/models/base.py",
            r#"class Model:
    pass
"#,
        ),
        (
            "django/db/utils.py",
            r#"class ConnectionHandler:
    pass
class ConnectionProxy:
    pass
"#,
        ),
        (
            "django/db/__init__.py",
            r#"from django.db.utils import ConnectionHandler, ConnectionProxy
connections = ConnectionHandler()
connection = ConnectionProxy()
"#,
        ),
        (
            "app/registry.py",
            r#"class Apps:
    def get_model(self, label):
        pass

apps = Apps()
"#,
        ),
        (
            "app/engine.py",
            r#"from functools import cached_property

class Engine:
    def render_to_string(self, name):
        return name

class EngineMixin:
    @cached_property
    def engine(self):
        return self.backend()

class DjangoRenderer(EngineMixin):
    backend = Engine
    def render(self, name):
        return self.engine.render_to_string(name)
"#,
        ),
        (
            "app/utils.py",
            r#"from app.engine import Engine

def setup(templates):
    def decorator(func):
        def inner(self):
            self.engine = Engine()
            return func(self)
        return inner
    return decorator
"#,
        ),
        (
            "app/views.py",
            r#"from django.db import connection, connections
from django.db.models.base import Model
from app.registry import apps as global_apps
from app.utils import setup

class User(Model):
    pass

def run_migration(apps=global_apps):
    apps.get_model("User")
    with connection.schema_editor() as editor:
        editor.create_model(User)
    with connections["default"].cursor() as cur:
        cur.execute("SELECT 1")
    User.objects.filter(active=True).count()

class RenderTests:
    @setup({"t": "hi"})
    def test_render(self):
        self.engine.render_to_string("t")
"#,
        ),
    ]);

    has_call(&e, "app/views.py::run_migration", "app/registry.py::Apps::get_model");
    has_call(&e, "app/views.py::run_migration", "django/db/backends/base/base.py::BaseDatabaseWrapper::schema_editor");
    has_call(&e, "app/views.py::run_migration", "django/db/backends/base/schema.py::BaseDatabaseSchemaEditor::create_model");
    has_call(&e, "app/views.py::run_migration", "django/db/backends/base/base.py::BaseDatabaseWrapper::cursor");
    has_call(&e, "app/views.py::run_migration", "django/db/backends/utils.py::CursorWrapper::execute");
    has_call(&e, "app/views.py::run_migration", "django/db/models/query.py::QuerySet::filter");
    has_call(&e, "app/views.py::run_migration", "django/db/models/query.py::QuerySet::count");
    has_call(&e, "app/views.py::RenderTests::test_render", "app/engine.py::Engine::render_to_string");
    has_call(&e, "app/engine.py::DjangoRenderer::render", "app/engine.py::Engine::render_to_string");
}
