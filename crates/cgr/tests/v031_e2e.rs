//! Top-level end-to-end tests for v0.3.1 driving the real `cgr` CLI and real `cgr serve --mcp`
//! stdio JSON-RPC server against real multi-file repositories:
//! 1. Cross-tool session deduplication across `explore`, `node` (symbol + file), and `callers`/`callees`,
//!    with edit+sync invalidation, `--no-dedup` bypass, and truncation safety.
//! 2. `tail_idx` dynamic-dispatch hints on `node`, low-caller `callers`/`impact`, and `implementations`.
//! 3. Opt-in structured JSONL telemetry (`CGR_TELEMETRY_LOG`) on both CLI and MCP paths.
//! 4. MCP parity: synchronous pre-query `auto_sync` picking up edits immediately with `--no-watch`,
//!    plus `maxChars`, `compact`, `noDedup`, and `implementations`.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

fn cgr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cgr"))
}

fn run_ok(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> String {
    let mut cmd = cgr();
    cmd.current_dir(dir).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn cgr");
    assert!(
        out.status.success(),
        "cgr {:?} failed (code {:?}):\nstdout:\n{}\nstderr:\n{}",
        args,
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    let raw = std::fs::read_to_string(path).expect("telemetry log exists");
    raw.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).expect("valid telemetry jsonl")).collect()
}

#[test]
fn cli_cross_tool_session_dedup_tail_idx_and_telemetry_work_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let telem = root.join(".tmp_telemetry.jsonl");
    let telem_str = telem.to_str().unwrap();

    write(
        root,
        "src/core.js",
        "export function dispatchHook(x) {\n  const normalized = Number(x) || 0;\n  const scaled = normalized * 3;\n  const offset = scaled + 1;\n  return offset;\n}\nexport function directOnly(y) {\n  return dispatchHook(y);\n}\nexport function tinyPing(v) { return v; }\n",
    );
    write(
        root,
        "src/dynamic.js",
        "export function invokeDynamic(plugin, val) {\n  return plugin.dispatchHook(val);\n}\nexport function silentDynamic(handler, x) {\n  return handler.onlyDynamic(x);\n}\n",
    );
    write(
        root,
        "src/target.js",
        "export function onlyDynamic(x) {\n  const clamped = Math.max(0, x);\n  const multiplied = clamped * 10;\n  const adjusted = multiplied + 4;\n  return adjusted;\n}\n",
    );

    run_ok(root, &["init", "--local-only"], &[]);

    let envs = [("CGR_SESSION", "sess-v031"), ("CGR_TELEMETRY_LOG", telem_str)];

    // 1. Explore sends dispatchHook body; follow-up `node dispatchHook --code` deduplicates the body
    //    while preserving its signature and Trail.
    let e1 = run_ok(root, &["explore", "dispatchHook"], &envs);
    assert!(e1.contains("const scaled = normalized * 3;"), "{e1}");

    let n1 = run_ok(root, &["node", "dispatchHook", "--code"], &envs);
    assert!(n1.contains("Already sent earlier in this conversation"), "node must reuse explore range cache: {n1}");
    assert!(n1.contains("saved "), "{n1}");
    assert!(!n1.contains("const scaled = normalized * 3;"), "node must not re-send body: {n1}");
    assert!(n1.contains("Called by"), "trail must remain visible: {n1}");

    // 1b. Tiny 1-line function must NOT be replaced by a longer dedup notice.
    let tiny1 = run_ok(root, &["node", "tinyPing", "--code"], &envs);
    let tiny2 = run_ok(root, &["node", "tinyPing", "--code"], &envs);
    assert!(tiny1.contains("return v;"), "{tiny1}");
    assert_eq!(tiny1, tiny2, "short body must never be inflated by dedup");
    assert!(!tiny2.contains("Already sent earlier"), "{tiny2}");

    // 2. File-mode `node` records lines and strictly shrinks on immediate repeat; `--no-dedup` bypasses.
    let f1 = run_ok(root, &["node", "src/target.js"], &envs);
    assert!(f1.contains("const multiplied = clamped * 10;"), "{f1}");
    let f2 = run_ok(root, &["node", "src/target.js"], &envs);
    assert!(f2.contains("Already sent earlier in this conversation"), "file-mode node must dedup: {f2}");
    assert!(f2.len() < f1.len(), "file-mode dedup must strictly shrink output ({} vs {})", f2.len(), f1.len());
    assert!(!f2.contains("const multiplied = clamped * 10;"), "{f2}");
    let f3 = run_ok(root, &["node", "src/target.js", "--no-dedup"], &envs);
    assert!(f3.contains("const multiplied = clamped * 10;"), "--no-dedup must force verbatim source: {f3}");

    // 3. `callers` on dispatchHook has 1 static caller (`directOnly`) PLUS a dynamic receiver site in
    //    src/dynamic.js:2 -> must surface the compact tail_idx dynamic-dispatch note, and shrink on repeat!
    let c1 = run_ok(root, &["callers", "dispatchHook"], &envs);
    assert!(c1.contains("directOnly"), "{c1}");
    assert!(c1.contains("Dynamic-dispatch note (`tail_idx`)"), "low-caller dynamic site must surface: {c1}");
    assert!(c1.contains("src/dynamic.js:2"), "{c1}");

    let c2 = run_ok(root, &["callers", "dispatchHook"], &envs);
    assert!(c2.contains("Already sent earlier in this conversation"), "callers must dedup at unchanged gen: {c2}");
    assert!(c2.len() < c1.len(), "callers dedup must strictly shrink output ({} vs {})", c2.len(), c1.len());

    // 4. `node` on `onlyDynamic` (0 static callers) must surface tail_idx hint + actionable rg tip!
    let n_dyn = run_ok(root, &["node", "onlyDynamic", "--compact"], &envs);
    assert!(n_dyn.contains("unresolved / ambiguous references"), "node must surface tail_idx on 0 callers: {n_dyn}");
    assert!(n_dyn.contains("src/dynamic.js:5"), "{n_dyn}");
    assert!(n_dyn.contains("rg -n \"onlyDynamic\""), "actionable rg tip expected: {n_dyn}");

    // 4b. Multi-target CLI `--max-chars` enforces a total cap across targets, and `query --json --max-chars` stays valid JSON.
    let multi = run_ok(root, &["node", "src/core.js", "src/target.js", "src/dynamic.js", "--no-dedup", "--max-chars", "650"], &envs);
    assert!(multi.contains("more target(s) omitted to fit `--max-chars 650`"), "multi-target cap expected: {multi}");
    let q_json = run_ok(root, &["query", "Dynamic", "--limit", "20", "--json", "--max-chars", "350"], &envs);
    let q_val: Value = serde_json::from_str(&q_json).expect("query --json --max-chars must stay valid JSON");
    assert_eq!(q_val["truncated"], true, "expected truncated flag on tight json cap: {q_json}");
    assert_eq!(q_val["results"].as_array().unwrap().len(), 1, "expected 1 result kept under tight cap: {q_json}");

    // 5. Editing src/core.js + `--sync` advances generation and invalidates BOTH range and callers dedup!
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(
        root,
        "src/core.js",
        "export function dispatchHook(x) {\n  const normalized = Number(x) || 0;\n  const scaled = normalized * 4;\n  const offset = scaled + 2;\n  return offset;\n}\nexport function directOnly(y) {\n  return dispatchHook(y);\n}\nexport function secondCaller(z) {\n  return dispatchHook(z);\n}\nexport function tinyPing(v) { return v; }\n",
    );
    let c3 = run_ok(root, &["--sync", "callers", "dispatchHook"], &envs);
    assert!(c3.contains("secondCaller"), "edit + --sync must invalidate callers cache: {c3}");
    assert!(!c3.contains("Already sent earlier"), "{c3}");

    let n_updated = run_ok(root, &["node", "dispatchHook", "--code"], &envs);
    assert!(n_updated.contains("const scaled = normalized * 4;"), "edit must invalidate node range cache: {n_updated}");

    // 6. Verify structured telemetry JSONL captured dedup hits, saved bytes, tail_idx triggers, and sync!
    let events = read_jsonl(&telem);
    assert!(events.len() >= 9, "expected telemetry events for every CLI query, got {}", events.len());
    assert!(events.iter().all(|e| e["mode"] == "cli"), "all CLI mode: {events:?}");
    assert!(
        events.iter().any(|e| e["tool"] == "node" && e["dedup_hits"].as_u64().unwrap() >= 1 && e["dedup_saved_bytes"].as_u64().unwrap() > 0),
        "node dedup telemetry missing: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["tool"] == "callers" && e["dedup_hits"].as_u64().unwrap() >= 1 && e["dedup_saved_bytes"].as_u64().unwrap() > 0),
        "callers dedup telemetry missing: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["tool"] == "callers" && e["tail_idx_triggered"] == true && e["tail_idx_dynamic"].as_u64().unwrap() >= 1),
        "low-caller tail_idx telemetry missing: {events:?}"
    );
    assert!(events.iter().any(|e| e["tool"] == "node" && e["tail_idx_triggered"] == true), "node tail_idx telemetry missing: {events:?}");
    assert!(events.iter().any(|e| e["tool"] == "callers" && e["sync_checked"] == true && e["synced"] == true), "sync telemetry missing: {events:?}");
}

#[test]
fn mcp_stdio_auto_sync_cross_tool_dedup_budget_and_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let telem = root.join(".mcp_telemetry.jsonl");

    write(
        root,
        "src/service.ts",
        "export interface Runner {\n  run(x: number): number;\n}\nexport class FastRunner implements Runner {\n  run(x: number): number {\n    return x + 1;\n  }\n}\nexport function helper(a: number): number {\n  const doubled = a * 2;\n  const bounded = Math.max(0, doubled);\n  const finalValue = bounded + 1;\n  return finalValue;\n}\nexport function entry(n: number): number {\n  return helper(n);\n}\n",
    );
    run_ok(root, &["init", "--local-only"], &[]);

    // Launch real `cgr serve --mcp --dedup --no-watch` (no watcher! Rely strictly on synchronous auto_sync).
    let mut child = cgr()
        .current_dir(root)
        .args(["serve", "--mcp", "--dedup", "--no-watch"])
        .env("CGR_MCP_TOOLS", "explore,node,callers,callees,impact,implementations,search,files,status")
        .env("CGR_TELEMETRY_LOG", telem.to_str().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cgr serve");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let mut rpc = |id: i64, method: &str, params: Value| -> Value {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{req}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read json-rpc response");
        serde_json::from_str(&line).expect("valid json-rpc response")
    };

    let init_r = rpc(1, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}));
    assert_eq!(init_r["result"]["serverInfo"]["name"], "codegraph-rust");

    // 1. Call `implementations` via MCP (verifying newly exposed parity tool + maxChars).
    let impl_r = rpc(2, "tools/call", json!({"name": "codegraph-rust_implementations", "arguments": {"symbol": "Runner", "maxChars": 2000}}));
    let impl_txt = impl_r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(impl_txt.contains("FastRunner"), "{impl_txt}");

    // 2. Call `explore` then `node(includeCode=true)` -> cross-tool dedup in MCP!
    let exp_r = rpc(3, "tools/call", json!({"name": "codegraph-rust_explore", "arguments": {"query": "helper", "maxChars": 4000}}));
    let exp_txt = exp_r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(exp_txt.contains("const doubled = a * 2;"), "{exp_txt}");

    let node_r = rpc(4, "tools/call", json!({"name": "codegraph-rust_node", "arguments": {"symbol": "helper", "includeCode": true}}));
    let node_txt = node_r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(node_txt.contains("Already sent earlier in this conversation"), "MCP node must reuse explore dedup cache: {node_txt}");
    assert!(!node_txt.contains("const doubled = a * 2;"), "{node_txt}");

    // 3. Edit src/service.ts on disk and IMMEDIATELY call `codegraph-rust_callers` with NO watcher running:
    //    synchronous pre-query auto_sync must pick up `addedCaller` on the very next call with zero stale banner!
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(
        root,
        "src/service.ts",
        "export interface Runner {\n  run(x: number): number;\n}\nexport class FastRunner implements Runner {\n  run(x: number): number {\n    return x + 1;\n  }\n}\nexport function helper(a: number): number {\n  const doubled = a * 2;\n  const bounded = Math.max(0, doubled);\n  const finalValue = bounded + 1;\n  return finalValue;\n}\nexport function entry(n: number): number {\n  return helper(n);\n}\nexport function addedCaller(m: number): number {\n  return helper(m + 1);\n}\n",
    );
    let callers_r = rpc(5, "tools/call", json!({"name": "codegraph-rust_callers", "arguments": {"symbol": "helper", "maxChars": 2000}}));
    let callers_txt = callers_r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(callers_txt.contains("addedCaller"), "MCP auto_sync must immediately reflect on-disk edit: {callers_txt}");
    assert!(!callers_txt.contains("edited since the last index sync"), "must not show stale banner after auto_sync: {callers_txt}");

    drop(stdin);
    let status = child.wait().expect("wait cgr serve");
    assert!(status.success(), "cgr serve exited cleanly");

    // 4. Verify MCP telemetry JSONL.
    let events = read_jsonl(&telem);
    assert_eq!(events.len(), 4, "4 MCP tool calls expected: {events:?}");
    assert!(events.iter().all(|e| e["mode"] == "mcp"), "{events:?}");
    assert!(
        events.iter().any(|e| e["tool"] == "node" && e["dedup_hits"].as_u64().unwrap() == 1 && e["dedup_saved_bytes"].as_u64().unwrap() > 0),
        "MCP node dedup telemetry expected: {events:?}"
    );
    assert!(events.iter().any(|e| e["tool"] == "callers" && e["sync_checked"] == true && e["synced"] == true), "MCP auto_sync telemetry expected: {events:?}");
}
