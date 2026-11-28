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

fn write_mode_and_telemetry_fixture(root: &Path) {
    write(
        root,
        "src/pricing.ts",
        concat!(
            "export function computeDiscount(subtotal: number, tier: number): number {\n",
            "  const clamped = Math.max(0, subtotal);\n",
            "  const rate = tier > 2 ? 0.15 : 0.05;\n",
            "  const reduction = Math.round(clamped * rate * 100) / 100;\n",
            "  const finalAmount = Math.max(0, clamped - reduction);\n",
            "  return finalAmount;\n",
            "}\n",
            "export class OrderService {\n",
            "  validateOrder(subtotal: number): boolean {\n",
            "    return subtotal >= 0;\n",
            "  }\n",
            "  processOrder(subtotal: number, tier: number): number {\n",
            "    if (!this.validateOrder(subtotal)) return 0;\n",
            "    return computeDiscount(subtotal, tier);\n",
            "  }\n",
            "}\n",
            "export function callerAlpha(v: number): number { return computeDiscount(v, 1); }\n",
            "export function callerBeta(v: number): number { return computeDiscount(v, 2); }\n",
            "export function callerGamma(v: number): number { return computeDiscount(v, 3); }\n",
            "export function callerDelta(v: number): number { return computeDiscount(v, 4); }\n",
        ),
    );
    let mut catalog = String::new();
    for i in 1..=80 {
        catalog.push_str(&format!("export function catalogItem_{i}(base: number): number {{\n  const adjusted = base + {i};\n  return adjusted * 2;\n}}\n"));
    }
    write(root, "src/catalog.ts", &catalog);
}

fn assert_no_mcp_leakage(label: &str, text: &str) {
    for forbidden in ["codegraph-rust_node", "codegraph-rust_explore", "noDedup: true", "Drop `symbolsOnly`", "Drop `compact`"] {
        assert!(!text.contains(forbidden), "{label} leaked MCP syntax `{forbidden}`:\n{text}");
    }
}

#[test]
fn cli_emits_cli_hints_without_mcp_leakage_and_records_one_symbol_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let telem = root.join(".cli_hints_telemetry.jsonl");
    write_mode_and_telemetry_fixture(root);
    run_ok(root, &["init", "--local-only"], &[]);

    let envs = [("CGR_SESSION", "sess-hints-cli"), ("CGR_TELEMETRY_LOG", telem.to_str().unwrap())];

    // 1. General multi-symbol explore --compact -> CLI syntax, one_symbol = false.
    let exp_compact_multi = run_ok(root, &["explore", "--compact", "computeDiscount processOrder"], &envs);
    assert!(exp_compact_multi.contains("Compact mode omits source bodies"), "{exp_compact_multi}");
    assert!(exp_compact_multi.contains("Drop `--compact`"), "{exp_compact_multi}");
    assert!(exp_compact_multi.contains("cgr node"), "{exp_compact_multi}");
    assert_no_mcp_leakage("cli explore --compact multi", &exp_compact_multi);

    // 2. Focused one-symbol explore --compact -> CLI syntax, one_symbol = true.
    let exp_compact_one = run_ok(root, &["explore", "--compact", "computeDiscount"], &envs);
    assert!(exp_compact_one.contains("Compact mode omits source bodies"), "{exp_compact_one}");
    assert!(exp_compact_one.contains("Drop `--compact`"), "{exp_compact_one}");
    assert!(exp_compact_one.contains("cgr node"), "{exp_compact_one}");
    assert_no_mcp_leakage("cli explore --compact one", &exp_compact_one);

    // 3. Focused one-symbol explore full -> caches computeDiscount body, one_symbol = true.
    let exp_one = run_ok(root, &["explore", "computeDiscount"], &envs);
    assert!(exp_one.contains("const finalAmount = Math.max(0, clamped - reduction);"), "{exp_one}");
    assert_no_mcp_leakage("cli explore one", &exp_one);

    // 4. Single-symbol explore with --max-files forces general explore -> one_symbol = false.
    let exp_max_files = run_ok(root, &["explore", "computeDiscount", "--max-files", "3", "--no-dedup"], &envs);
    assert!(exp_max_files.contains("**Exploration:"), "{exp_max_files}");
    assert_no_mcp_leakage("cli explore --max-files", &exp_max_files);

    // 5. Tight general explore truncation note -> CLI `cgr explore`, one_symbol = false.
    let exp_trunc = run_ok(root, &["explore", "computeDiscount processOrder catalogItem_1", "--max-chars", "1100", "--no-dedup"], &envs);
    assert!(exp_trunc.contains("cgr explore"), "truncation note must reference `cgr explore`: {exp_trunc}");
    assert_no_mcp_leakage("cli explore truncated", &exp_trunc);

    // 6. Container outline via `cgr node OrderService --code` -> CLI syntax, one_symbol = false.
    let node_container = run_ok(root, &["node", "OrderService", "--code"], &envs);
    assert!(node_container.contains("Structural outline only"), "{node_container}");
    assert!(node_container.contains("cgr node"), "{node_container}");
    assert_no_mcp_leakage("cli node container", &node_container);

    // 7. Symbol dedup + Trail header via `cgr node computeDiscount --code` -> CLI `--no-dedup` & `cgr node`.
    let node_dedup = run_ok(root, &["node", "computeDiscount", "--code"], &envs);
    assert!(node_dedup.contains("Already sent earlier in this conversation"), "{node_dedup}");
    assert!(node_dedup.contains("--no-dedup"), "dedup notice must use --no-dedup: {node_dedup}");
    assert!(node_dedup.contains("**Trail —") && node_dedup.contains("cgr node"), "trail header must use cgr node: {node_dedup}");
    assert_no_mcp_leakage("cli node dedup", &node_dedup);

    // 8. File `--symbols-only` footer -> CLI `--symbols-only` and `--offset`/`--limit`.
    let file_syms = run_ok(root, &["node", "src/catalog.ts", "--symbols-only"], &envs);
    assert!(file_syms.contains("--symbols-only"), "{file_syms}");
    assert!(file_syms.contains("--offset") && file_syms.contains("--limit"), "{file_syms}");
    assert_no_mcp_leakage("cli node --symbols-only", &file_syms);

    // 9. File pagination footer (first + dedup repeat) -> CLI `--offset`/`--limit`, `cgr node`, `--no-dedup`.
    let file_page1 = run_ok(root, &["node", "src/catalog.ts", "--limit", "25"], &envs);
    assert!(file_page1.contains("--offset") && file_page1.contains("--limit"), "{file_page1}");
    assert!(file_page1.contains("cgr node <symbol>"), "{file_page1}");
    assert_no_mcp_leakage("cli node file page1", &file_page1);

    let file_page2 = run_ok(root, &["node", "src/catalog.ts", "--limit", "25"], &envs);
    assert!(file_page2.contains("Already sent earlier in this conversation"), "{file_page2}");
    assert!(file_page2.contains("--no-dedup"), "{file_page2}");
    assert!(file_page2.contains("--offset") && file_page2.contains("--limit"), "{file_page2}");
    assert!(file_page2.contains("cgr node <symbol>"), "{file_page2}");
    assert_no_mcp_leakage("cli node file page2", &file_page2);

    // 10. `callers` widening (`--limit`) + dedup (`--no-dedup`, no `noDedup: true`).
    let callers1 = run_ok(root, &["callers", "computeDiscount", "--limit", "3"], &envs);
    assert!(callers1.contains("pass `--limit`"), "callers widen hint must use --limit: {callers1}");
    assert_no_mcp_leakage("cli callers", &callers1);
    let callers2 = run_ok(root, &["callers", "computeDiscount", "--limit", "3"], &envs);
    assert!(callers2.contains("Already sent earlier in this conversation"), "{callers2}");
    assert!(callers2.contains("--no-dedup"), "callers dedup must use --no-dedup: {callers2}");
    assert_no_mcp_leakage("cli callers dedup", &callers2);

    // 11. Verify JSONL telemetry `one_symbol` accuracy across all 12 CLI calls.
    let events = read_jsonl(&telem);
    assert_eq!(events.len(), 12, "expected 12 CLI telemetry events: {events:?}");
    assert!(events.iter().all(|e| e["mode"] == "cli" && e["one_symbol"].is_boolean()), "{events:?}");
    assert_eq!((events[0]["tool"].as_str(), events[0]["one_symbol"].as_bool()), (Some("explore"), Some(false)));
    assert_eq!((events[1]["tool"].as_str(), events[1]["one_symbol"].as_bool()), (Some("explore"), Some(true)));
    assert_eq!((events[2]["tool"].as_str(), events[2]["one_symbol"].as_bool()), (Some("explore"), Some(true)));
    assert_eq!((events[3]["tool"].as_str(), events[3]["one_symbol"].as_bool()), (Some("explore"), Some(false)));
    assert_eq!((events[4]["tool"].as_str(), events[4]["one_symbol"].as_bool()), (Some("explore"), Some(false)));
    for ev in &events[5..] {
        assert_ne!(ev["tool"], "explore", "expected non-explore tool: {ev:?}");
        assert_eq!(ev["one_symbol"], false, "non-explore tools must record one_symbol=false: {ev:?}");
    }
}

#[test]
fn mcp_preserves_mcp_hints_and_records_one_symbol_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let telem = root.join(".mcp_hints_telemetry.jsonl");
    write_mode_and_telemetry_fixture(root);
    run_ok(root, &["init", "--local-only"], &[]);

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
    let text_of = |v: &Value| v["result"]["content"][0]["text"].as_str().unwrap().to_owned();

    let init_r = rpc(1, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}));
    assert_eq!(init_r["result"]["serverInfo"]["name"], "codegraph-rust");

    // 1. General multi-symbol explore compact -> MCP syntax (`compact`, `codegraph-rust_node`), one_symbol = false.
    let exp_compact_multi =
        text_of(&rpc(2, "tools/call", json!({"name": "codegraph-rust_explore", "arguments": {"query": "computeDiscount processOrder", "compact": true}})));
    assert!(exp_compact_multi.contains("Drop `compact`"), "{exp_compact_multi}");
    assert!(exp_compact_multi.contains("codegraph-rust_node"), "{exp_compact_multi}");

    // 2. Focused one-symbol explore compact -> MCP syntax, one_symbol = true.
    let exp_compact_one = text_of(&rpc(3, "tools/call", json!({"name": "codegraph-rust_explore", "arguments": {"query": "computeDiscount", "compact": true}})));
    assert!(exp_compact_one.contains("Drop `compact`"), "{exp_compact_one}");

    // 3. Focused one-symbol explore full -> caches computeDiscount body, one_symbol = true.
    let exp_one = text_of(&rpc(4, "tools/call", json!({"name": "codegraph-rust_explore", "arguments": {"query": "computeDiscount"}})));
    assert!(exp_one.contains("const finalAmount = Math.max(0, clamped - reduction);"), "{exp_one}");

    // 4. Single-symbol explore with maxFiles -> falls back to general explore, one_symbol = false.
    let exp_max_files =
        text_of(&rpc(5, "tools/call", json!({"name": "codegraph-rust_explore", "arguments": {"query": "computeDiscount", "maxFiles": 3, "noDedup": true}})));
    assert!(exp_max_files.contains("**Exploration:"), "{exp_max_files}");

    // 5. Tight general explore truncation note -> MCP `codegraph-rust_explore`, one_symbol = false.
    let exp_trunc = text_of(&rpc(
        6,
        "tools/call",
        json!({"name": "codegraph-rust_explore", "arguments": {"query": "computeDiscount processOrder catalogItem_1", "maxChars": 1100, "noDedup": true}}),
    ));
    assert!(exp_trunc.contains("codegraph-rust_explore"), "MCP truncation note must keep codegraph-rust_explore: {exp_trunc}");

    // 6. Container outline via MCP node -> MCP `codegraph-rust_node`.
    let node_container = text_of(&rpc(7, "tools/call", json!({"name": "codegraph-rust_node", "arguments": {"symbol": "OrderService", "includeCode": true}})));
    assert!(node_container.contains("Structural outline only") && node_container.contains("codegraph-rust_node"), "{node_container}");

    // 7. Symbol dedup + Trail via MCP node -> MCP `noDedup: true` and `**Trail — codegraph-rust_node`.
    let node_dedup = text_of(&rpc(8, "tools/call", json!({"name": "codegraph-rust_node", "arguments": {"symbol": "computeDiscount", "includeCode": true}})));
    assert!(node_dedup.contains("Already sent earlier in this conversation"), "{node_dedup}");
    assert!(node_dedup.contains("noDedup: true"), "{node_dedup}");
    assert!(node_dedup.contains("**Trail — codegraph-rust_node"), "{node_dedup}");

    // 8. File `symbolsOnly` footer -> MCP `symbolsOnly` and `offset`/`limit`.
    let file_syms = text_of(&rpc(9, "tools/call", json!({"name": "codegraph-rust_node", "arguments": {"file": "src/catalog.ts", "symbolsOnly": true}})));
    assert!(file_syms.contains("Drop `symbolsOnly`"), "{file_syms}");
    assert!(file_syms.contains("`offset`/`limit`"), "{file_syms}");

    // 9. File pagination + dedup -> MCP `offset`/`limit`, `codegraph-rust_node <symbol>`, `noDedup: true`.
    let file_page1 = text_of(&rpc(10, "tools/call", json!({"name": "codegraph-rust_node", "arguments": {"file": "src/catalog.ts", "limit": 25}})));
    assert!(file_page1.contains("`offset`/`limit`") && file_page1.contains("codegraph-rust_node <symbol>"), "{file_page1}");
    let file_page2 = text_of(&rpc(11, "tools/call", json!({"name": "codegraph-rust_node", "arguments": {"file": "src/catalog.ts", "limit": 25}})));
    assert!(
        file_page2.contains("Already sent earlier") && file_page2.contains("noDedup: true") && file_page2.contains("codegraph-rust_node <symbol>"),
        "{file_page2}"
    );

    // 10. MCP callers widening (`limit`) + dedup (`noDedup: true`).
    let callers1 = text_of(&rpc(12, "tools/call", json!({"name": "codegraph-rust_callers", "arguments": {"symbol": "computeDiscount", "limit": 3}})));
    assert!(callers1.contains("pass `limit`"), "{callers1}");
    let callers2 = text_of(&rpc(13, "tools/call", json!({"name": "codegraph-rust_callers", "arguments": {"symbol": "computeDiscount", "limit": 3}})));
    assert!(callers2.contains("Already sent earlier") && callers2.contains("noDedup: true"), "{callers2}");

    drop(stdin);
    let status = child.wait().expect("wait cgr serve");
    assert!(status.success(), "cgr serve exited cleanly");

    // 11. Verify MCP JSONL telemetry `one_symbol` accuracy across all 12 tool calls.
    let events = read_jsonl(&telem);
    assert_eq!(events.len(), 12, "expected 12 MCP telemetry events: {events:?}");
    assert!(events.iter().all(|e| e["mode"] == "mcp" && e["one_symbol"].is_boolean()), "{events:?}");
    assert_eq!((events[0]["tool"].as_str(), events[0]["one_symbol"].as_bool()), (Some("explore"), Some(false)));
    assert_eq!((events[1]["tool"].as_str(), events[1]["one_symbol"].as_bool()), (Some("explore"), Some(true)));
    assert_eq!((events[2]["tool"].as_str(), events[2]["one_symbol"].as_bool()), (Some("explore"), Some(true)));
    assert_eq!((events[3]["tool"].as_str(), events[3]["one_symbol"].as_bool()), (Some("explore"), Some(false)));
    assert_eq!((events[4]["tool"].as_str(), events[4]["one_symbol"].as_bool()), (Some("explore"), Some(false)));
    for ev in &events[5..] {
        assert_ne!(ev["tool"], "explore", "expected non-explore tool: {ev:?}");
        assert_eq!(ev["one_symbol"], false, "non-explore tools must record one_symbol=false: {ev:?}");
    }
}
