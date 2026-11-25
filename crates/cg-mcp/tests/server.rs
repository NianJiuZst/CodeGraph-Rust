//! MCP server behaviour that agents rely on, driven through the transport-independent core.

use cg_mcp::{Config, Reply, Server};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Collect(Mutex<Vec<Value>>);

impl Reply for Collect {
    fn send(&self, msg: Value) {
        self.0.lock().unwrap().push(msg);
    }
}

impl Collect {
    fn by_id(&self, id: i64) -> Option<Value> {
        self.0.lock().unwrap().iter().find(|m| m["id"] == json!(id)).cloned()
    }
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let w = |rel: &str, text: &str| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    w("src/greet.ts", "export function greet(name: string): string {\n  return format(name);\n}\nexport function format(name: string): string {\n  return `hello ${name}`;\n}\n");
    w("src/main.ts", "import { greet } from './greet';\nexport function main(): string {\n  return greet('world');\n}\n");
    cg_resolve::index_project(dir.path(), &Default::default()).unwrap();
    dir
}

fn server_for(root: &Path, tools: Option<Vec<String>>) -> (Arc<Server>, Arc<Collect>) {
    let server = Server::new(Config { path: Some(root.to_path_buf()), tools, dedup: false, auto_sync: false });
    let out = Arc::new(Collect::default());
    server.set_outbound(out.clone());
    (server, out)
}

fn rpc(server: &Arc<Server>, out: &Arc<Collect>, id: i64, method: &str, params: Value) {
    server.handle(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}), out.clone());
}

fn call(server: &Arc<Server>, out: &Arc<Collect>, id: i64, tool: &str, args: Value) {
    rpc(server, out, id, "tools/call", json!({"name": tool, "arguments": args}));
}

fn text_of(reply: &Value) -> String {
    reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned()
}

#[test]
fn initialize_announces_the_server_and_its_instructions() {
    let dir = project();
    let (server, out) = server_for(dir.path(), None);
    rpc(&server, &out, 1, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}));
    let r = out.by_id(1).expect("initialize answered synchronously");
    assert_eq!(r["result"]["serverInfo"]["name"], "codegraph-rust");
    assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    let instructions = r["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("codegraph-rust_explore"));
    // an unknown protocol version is answered with one we support
    let (server2, out2) = server_for(dir.path(), None);
    rpc(&server2, &out2, 1, "initialize", json!({"protocolVersion": "1999-01-01", "capabilities": {}}));
    assert_eq!(out2.by_id(1).unwrap()["result"]["protocolVersion"], "2025-06-18");
}

#[test]
fn default_surface_is_explore_only_and_the_allowlist_opens_more() {
    let dir = project();
    let (server, out) = server_for(dir.path(), None);
    rpc(&server, &out, 2, "tools/list", json!({}));
    let tools = out.by_id(2).unwrap()["result"]["tools"].clone();
    let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["codegraph-rust_explore"]);
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(tools[0]["_meta"]["anthropic/alwaysLoad"], true);

    let (server, out) = server_for(dir.path(), Some(vec!["explore".into(), "callers".into(), "node".into()]));
    rpc(&server, &out, 2, "tools/list", json!({}));
    let tools = out.by_id(2).unwrap()["result"]["tools"].clone();
    let mut names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["codegraph-rust_callers", "codegraph-rust_explore", "codegraph-rust_node"]);
}

#[test]
fn explore_returns_source_and_is_not_an_error() {
    let dir = project();
    let (server, out) = server_for(dir.path(), None);
    call(&server, &out, 3, "codegraph-rust_explore", json!({"query": "greet format"}));
    server.drain(Duration::from_secs(20));
    let r = out.by_id(3).expect("explore answered");
    assert!(r["result"]["isError"].is_null(), "{r}");
    let text = text_of(&r);
    assert!(text.contains("greet") && text.contains("format"), "{text}");
    assert!(text.contains("src/greet.ts"), "{text}");
}

/// Regression: when the client closes stdin right after its last request, that request still
/// gets its answer (the stdio loop drains in-flight calls before it exits).
#[test]
fn drain_waits_for_accepted_calls() {
    let dir = project();
    let (server, out) = server_for(dir.path(), None);
    for id in 10..16 {
        call(&server, &out, id, "codegraph-rust_explore", json!({"query": "main greet"}));
    }
    server.drain(Duration::from_secs(20));
    for id in 10..16 {
        assert!(out.by_id(id).is_some(), "call {id} was dropped");
    }
}

#[test]
fn problems_are_success_shaped_so_agents_can_read_them() {
    let dir = project();
    let (server, out) = server_for(dir.path(), None);
    call(&server, &out, 4, "codegraph-rust_nonexistent", json!({}));
    call(&server, &out, 5, "codegraph-rust_explore", json!({}));
    server.drain(Duration::from_secs(20));
    for id in [4, 5] {
        let r = out.by_id(id).unwrap();
        assert!(r.get("error").is_none(), "tool problems must not be JSON-RPC errors: {r}");
        assert!(!text_of(&r).is_empty());
    }
    // a project without an index says so in plain text instead of failing
    let empty = tempfile::tempdir().unwrap();
    let (server, out) = server_for(empty.path(), None);
    call(&server, &out, 6, "codegraph-rust_explore", json!({"query": "anything"}));
    server.drain(Duration::from_secs(20));
    let r = out.by_id(6).unwrap();
    assert!(r.get("error").is_none());
    assert!(text_of(&r).contains("cgr init"), "{}", text_of(&r));
}

#[test]
fn unknown_methods_get_a_json_rpc_error() {
    let dir = project();
    let (server, out) = server_for(dir.path(), None);
    rpc(&server, &out, 7, "no/such/method", json!({}));
    assert_eq!(out.by_id(7).unwrap()["error"]["code"], -32601);
}

#[test]
fn project_path_selects_another_index() {
    let first = project();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(second.path().join("other.py"), "def only_in_second():\n    return 1\n").unwrap();
    cg_resolve::index_project(second.path(), &Default::default()).unwrap();
    let (server, out) = server_for(first.path(), None);
    call(&server, &out, 8, "codegraph-rust_explore", json!({"query": "only_in_second", "projectPath": second.path().to_str().unwrap()}));
    call(&server, &out, 9, "codegraph-rust_explore", json!({"query": "only_in_second"}));
    server.drain(Duration::from_secs(20));
    assert!(text_of(&out.by_id(8).unwrap()).contains("only_in_second"));
    assert!(!text_of(&out.by_id(9).unwrap()).contains("def only_in_second"), "the default project must not see the other index");
}

#[test]
fn project_lru_evicts_the_corresponding_watcher_resource() {
    let dirs: Vec<_> = (0..9).map(|_| project()).collect();
    let (server, _) = server_for(dirs[0].path(), None);
    let watched = Arc::new(Mutex::new(std::collections::BTreeSet::new()));
    let opened = watched.clone();
    server.set_open_hook(Arc::new(move |root| {
        opened.lock().unwrap().insert(root.to_path_buf());
    }));
    let closed = watched.clone();
    server.set_close_hook(Arc::new(move |root| {
        closed.lock().unwrap().remove(root);
    }));
    for dir in &dirs {
        server.preopen(dir.path());
        assert!(watched.lock().unwrap().len() <= 8);
    }
    assert_eq!(watched.lock().unwrap().len(), 8);
    assert!(!watched.lock().unwrap().contains(dirs[0].path()));
    server.shutdown();
}

#[test]
fn explore_compact_max_chars_and_session_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let w = |rel: &str, text: &str| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    w(
        "src/greet.ts",
        "export function greet(name: string): string {\n  const trimmed = name.trim();\n  const normalized = trimmed.length > 0 ? trimmed : 'world';\n  return format(normalized);\n}\nexport function format(name: string): string {\n  return `hello ${name}`;\n}\nexport function tiny(x: number): number { return x; }\n",
    );
    w(
        "src/main.ts",
        "import { greet, tiny } from './greet';\nexport function runAlpha(): string { return greet('alpha'); }\nexport function runBeta(): string { return greet('beta'); }\nexport function runGamma(): string { return greet('gamma'); }\nexport function runTiny(): number { return tiny(1); }\n",
    );
    cg_resolve::index_project(dir.path(), &Default::default()).unwrap();

    let server = Server::new(Config {
        path: Some(dir.path().to_path_buf()),
        tools: Some(vec!["explore".into(), "node".into(), "callers".into(), "implementations".into()]),
        dedup: true,
        auto_sync: false,
    });
    let out = Arc::new(Collect::default());
    server.set_outbound(out.clone());

    rpc(&server, &out, 1, "tools/list", json!({}));
    let tools = out.by_id(1).unwrap()["result"]["tools"].clone();
    let explore_schema = tools.as_array().unwrap().iter().find(|t| t["name"] == "codegraph-rust_explore").unwrap();
    assert!(explore_schema["inputSchema"]["properties"]["compact"].is_object());
    assert!(explore_schema["inputSchema"]["properties"]["maxChars"].is_object());
    assert!(explore_schema["inputSchema"]["properties"]["noDedup"].is_object());
    assert!(explore_schema["inputSchema"]["properties"]["sync"].is_object());
    let node_schema = tools.as_array().unwrap().iter().find(|t| t["name"] == "codegraph-rust_node").unwrap();
    assert!(node_schema["inputSchema"]["properties"]["maxChars"].is_object());
    assert!(node_schema["inputSchema"]["properties"]["compact"].is_object());
    assert!(node_schema["inputSchema"]["properties"]["noDedup"].is_object());

    // Compact mode omits source fences and must not poison dedup.
    call(&server, &out, 2, "codegraph-rust_explore", json!({"query": "greet", "compact": true}));
    server.drain(Duration::from_secs(20));
    let t2 = text_of(&out.by_id(2).unwrap());
    assert!(t2.contains("Compact mode omits source bodies"), "{t2}");
    assert!(!t2.contains("```ts"), "{t2}");

    // First full call returns verbatim source; second deduplicates it and is strictly shorter.
    call(&server, &out, 3, "codegraph-rust_explore", json!({"query": "greet"}));
    server.drain(Duration::from_secs(20));
    let t3 = text_of(&out.by_id(3).unwrap());
    assert!(t3.contains("return format(normalized);"), "{t3}");

    call(&server, &out, 4, "codegraph-rust_explore", json!({"query": "greet"}));
    server.drain(Duration::from_secs(20));
    let t4 = text_of(&out.by_id(4).unwrap());
    assert!(t4.contains("Already sent earlier in this conversation"), "{t4}");
    assert!(t4.contains("saved "), "{t4}");
    assert!(!t4.contains("return format(normalized);"), "{t4}");
    assert!(t4.len() < t3.len(), "dedup must strictly shrink output ({} vs {})", t4.len(), t3.len());

    // Cross-tool dedup: `node(symbol=greet, includeCode=true)` reuses explore's range cache.
    call(&server, &out, 5, "codegraph-rust_node", json!({"symbol": "greet", "includeCode": true}));
    server.drain(Duration::from_secs(20));
    let t5 = text_of(&out.by_id(5).unwrap());
    assert!(t5.contains("Already sent earlier in this conversation"), "{t5}");
    assert!(!t5.contains("return format(normalized);"), "{t5}");
    assert!(t5.contains("Called by"), "trail must stay intact: {t5}");

    // `noDedup: true` forces verbatim source.
    call(&server, &out, 6, "codegraph-rust_node", json!({"symbol": "greet", "includeCode": true, "noDedup": true}));
    server.drain(Duration::from_secs(20));
    let t6 = text_of(&out.by_id(6).unwrap());
    assert!(t6.contains("return format(normalized);"), "{t6}");

    // `callers` dedup shrinks multi-caller lists, but skips tiny single-caller results (no inflation).
    call(&server, &out, 7, "codegraph-rust_callers", json!({"symbol": "greet"}));
    server.drain(Duration::from_secs(20));
    call(&server, &out, 8, "codegraph-rust_callers", json!({"symbol": "greet"}));
    server.drain(Duration::from_secs(20));
    let t7 = text_of(&out.by_id(7).unwrap());
    let t8 = text_of(&out.by_id(8).unwrap());
    assert!(t7.contains("runAlpha"), "{t7}");
    assert!(t8.contains("Already sent earlier in this conversation"), "{t8}");
    assert!(t8.len() < t7.len(), "callers dedup must strictly shrink output ({} vs {})", t8.len(), t7.len());

    call(&server, &out, 9, "codegraph-rust_callers", json!({"symbol": "tiny"}));
    server.drain(Duration::from_secs(20));
    call(&server, &out, 10, "codegraph-rust_callers", json!({"symbol": "tiny"}));
    server.drain(Duration::from_secs(20));
    let t9 = text_of(&out.by_id(9).unwrap());
    let t10 = text_of(&out.by_id(10).unwrap());
    assert_eq!(t9, t10, "short callers output must not be inflated by a longer dedup notice");
    assert!(!t10.contains("Already sent earlier"), "{t10}");
    server.shutdown();
}
