//! Top-level end-to-end tests driving the real `cgr` binary on real multi-file fixtures:
//! - Minified / Emscripten JS bundle skipping without false positives on normal JS
//! - CLI session-persistent deduplication across one-symbol and multi-symbol `explore` calls
//! - Compact / budgeted `--compact` and `--max-chars` controls without dedup poisoning
//! - Transparent `--sync` / `CGR_AUTO_SYNC=1` and multi-symbol queries in a single process
//! - Calibrated unresolved / ambiguous reference hints across base and delta segments + `status --json` refs

use serde_json::Value;
use std::path::Path;
use std::process::Command;

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

#[test]
fn minified_js_skipped_but_handwritten_indexed_and_status_exposes_refs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "src/app.js", "export function renderScene(scene) {\n  return scene.name;\n}\nexport function startApp(s) {\n  return renderScene(s);\n}\n");
    // Normal JS with a long base64 data URI, numeric lookup table, AND embedded minified CSS template literal (must NOT be skipped).
    let b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".repeat(120);
    let lut = "0.123456,".repeat(1000);
    let css = ".btn{color:red;padding:4px 8px;border:1px solid #ccc;}".repeat(120);
    write(
        root,
        "src/assets.js",
        &format!(
            "export const LOGO = \"data:image/png;base64,{b64}\";\nexport const LUT = [{lut}];\nexport const STYLES = `{css}`;\nexport function loadLogo() {{\n  return LOGO;\n}}\n"
        ),
    );
    // Emscripten-style compiled bundle (short header + dense single-line minified JS > 4 KB).
    let emscripten_body = "function a(b){return c(b);}var d=function(e){return a(e);};".repeat(300);
    write(root, "libs/draco_encoder.js", &format!("var DracoEncoderModule = function(M) {{\n  M = M || {{}};\n{emscripten_body}\n  return M;\n}};\n"));
    write(root, "libs/vendor.min.js", "function vendorHelper(x){return x+1;}\n");

    run_ok(root, &["init"], &[]);

    let status_raw = run_ok(root, &["status", "--json"], &[]);
    let status: Value = serde_json::from_str(&status_raw).expect("valid status json");
    assert_eq!(status["files"], 4, "all four files stay registered: {status}");
    assert!(status["refs"]["total"].as_u64().unwrap() >= 1, "refs breakdown exposed: {status}");
    assert!(status["refs"]["resolved"].as_u64().unwrap() >= 1, "resolved refs counted: {status}");
    assert!(status["refs"]["resolvedRate"].as_f64().unwrap() > 0.0, "resolvedRate computed: {status}");

    let callers = run_ok(root, &["callers", "renderScene"], &[]);
    assert!(callers.contains("startApp"), "hand-written JS edges intact: {callers}");

    let assets = run_ok(root, &["query", "loadLogo"], &[]);
    assert!(assets.contains("loadLogo"), "normal JS with base64 literal must still be extracted: {assets}");

    let draco = run_ok(root, &["node", "libs/draco_encoder.js", "--symbols-only"], &[]);
    assert!(draco.contains("0 symbols"), "minified Emscripten bundle must skip AST extraction: {draco}");

    let vendor = run_ok(root, &["node", "libs/vendor.min.js", "--symbols-only"], &[]);
    assert!(vendor.contains("0 symbols"), "named .min.js bundle must skip AST extraction: {vendor}");

    run_ok(root, &["debug", "diff"], &[]);
}

#[test]
fn cli_session_dedup_covers_one_symbol_and_general_explore_and_invalidates_on_edit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(
        root,
        "src/math.ts",
        "export function computeTotal(price: number, qty: number): number {\n  const subtotal = price * qty;\n  return applyTax(subtotal);\n}\nexport function applyTax(amount: number): number {\n  return amount * 1.1;\n}\n",
    );
    write(
        root,
        "src/main.ts",
        "import { computeTotal } from './math';\nexport function checkout(p: number, q: number): number {\n  return computeTotal(p, q);\n}\n",
    );

    run_ok(root, &["init"], &[]);

    // 1. First one-symbol explore in session s1 sends verbatim definition source.
    let first = run_ok(root, &["--session", "s1", "explore", "computeTotal"], &[]);
    assert!(first.contains("const subtotal = price * qty;"), "{first}");
    assert!(first.contains("checkout"), "{first}");

    // 2. Second identical one-symbol explore in session s1 deduplicates the body while keeping callers.
    let second = run_ok(root, &["--session", "s1", "explore", "computeTotal"], &[]);
    assert!(second.contains("Already sent earlier in this conversation"), "{second}");
    assert!(!second.contains("const subtotal = price * qty;"), "body should not be re-sent: {second}");
    assert!(second.contains("checkout"), "callers must remain visible: {second}");

    // 3. --no-dedup forces full verbatim source even inside session s1.
    let forced = run_ok(root, &["--session", "s1", "explore", "--no-dedup", "computeTotal"], &[]);
    assert!(forced.contains("const subtotal = price * qty;"), "{forced}");

    // 4. Separate session s2 is completely isolated and gets full source.
    let other_sess = run_ok(root, &["explore", "computeTotal"], &[("CGR_SESSION", "s2")]);
    assert!(other_sess.contains("const subtotal = price * qty;"), "{other_sess}");

    // 5. General multi-symbol explore in s1 deduplicates already-sent computeTotal while sending fresh checkout.
    let general = run_ok(root, &["--session", "s1", "explore", "computeTotal checkout"], &[]);
    assert!(general.contains("return computeTotal(p, q);"), "fresh file must be shown: {general}");
    assert!(general.contains("Already sent earlier in this conversation"), "cached range deduplicated: {general}");

    // 6. Editing src/math.ts + --sync updates fingerprint and serves the updated source verbatim.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(
        root,
        "src/math.ts",
        "export function computeTotal(price: number, qty: number): number {\n  const subtotal = price * qty + 5;\n  return applyTax(subtotal);\n}\nexport function applyTax(amount: number): number {\n  return amount * 1.1;\n}\n",
    );
    let updated = run_ok(root, &["--sync", "--session", "s1", "explore", "computeTotal"], &[]);
    assert!(updated.contains("const subtotal = price * qty + 5;"), "edit must invalidate dedup cache: {updated}");
    assert!(!updated.contains("Already sent earlier"), "{updated}");
}

#[test]
fn compact_and_max_chars_controls_never_poison_session_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mut big = String::new();
    for i in 1..=120 {
        big.push_str(&format!("export function helper_{i}(x: number): number {{ return x + {i}; }}\n"));
    }
    write(root, "src/big.ts", &big);
    write(
        root,
        "src/entry.ts",
        "import { helper_1, helper_2 } from './big';\nexport function runAll(n: number): number {\n  return helper_1(n) + helper_2(n);\n}\n",
    );
    run_ok(root, &["init"], &[]);

    // Compact one-symbol explore omits code fence and must NOT poison session s-compact.
    let c1 = run_ok(root, &["--session", "s-compact", "explore", "--compact", "runAll"], &[]);
    assert!(c1.contains("Compact mode omits source bodies"), "{c1}");
    assert!(!c1.contains("```ts"), "compact one-symbol must omit source fence: {c1}");

    // Follow-up full explore in the SAME session must still return full verbatim body.
    let full = run_ok(root, &["--session", "s-compact", "explore", "runAll"], &[]);
    assert!(full.contains("return helper_1(n) + helper_2(n);"), "compact call must not poison dedup: {full}");

    // Compact multi-symbol explore returns outlines without code fences.
    let c2 = run_ok(root, &["explore", "--compact", "runAll helper_1"], &[]);
    assert!(c2.contains("Symbols (compact outline)"), "{c2}");
    assert!(!c2.contains("```ts"), "{c2}");
    assert!(c2.contains("runAll") && c2.contains("helper_1"), "{c2}");

    // --max-chars strictly bounds explore output and node file view (including oversized single-line files).
    let tight = run_ok(root, &["explore", "--max-chars", "1500", "runAll helper_1 helper_2"], &[]);
    assert!(tight.len() <= 1500, "tight explore must stay <= max-chars cap (got {})", tight.len());

    let tight_file = run_ok(root, &["node", "src/big.ts", "--max-chars", "1000"], &[]);
    assert!(tight_file.contains("of 120"), "tight file view should paginate early: {tight_file}");

    write(root, "src/oneline.js", &format!("export const x = \"{}\";\n", "a".repeat(8000)));
    run_ok(root, &["sync"], &[]);
    let clipped = run_ok(root, &["node", "src/oneline.js", "--max-chars", "1000"], &[]);
    assert!(clipped.len() <= 1150, "oversized single line must be clipped near budget (got {})", clipped.len());
    assert!(clipped.contains("partial line"), "clipped line must be marked partial: {clipped}");
}

#[test]
fn transparent_sync_and_multi_symbol_queries_work_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "src/ops.ts", "export function alpha(x: number): number { return x + 1; }\nexport function beta(x: number): number { return x * 2; }\n");
    write(root, "src/use.ts", "import { alpha } from './ops';\nexport function runA(n: number) { return alpha(n); }\n");
    run_ok(root, &["init"], &[]);

    // Edit without running standalone `cgr sync`, then query both symbols in one `--sync` command.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(
        root,
        "src/use.ts",
        "import { alpha, beta } from './ops';\nexport function runA(n: number) { return alpha(n); }\nexport function runB(n: number) { return beta(n); }\n",
    );
    let callers = run_ok(root, &["--sync", "callers", "alpha", "beta"], &[]);
    assert!(callers.contains("runA") && callers.contains("runB"), "multi-symbol --sync callers: {callers}");
    assert!(callers.contains("\n\n---\n\n"), "multi-symbol separator expected: {callers}");

    // Edit again and test CGR_AUTO_SYNC=1 + --no-sync override.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(
        root,
        "src/use.ts",
        "import { alpha, beta } from './ops';\nexport function runA(n: number) { return alpha(n); }\nexport function runB(n: number) { return beta(n); }\nexport function runC(n: number) { return beta(n + 1); }\n",
    );
    let no_sync = run_ok(root, &["--no-sync", "callers", "beta"], &[("CGR_AUTO_SYNC", "1")]);
    assert!(no_sync.contains("edited since the last index sync"), "--no-sync must override CGR_AUTO_SYNC: {no_sync}");
    assert!(!no_sync.contains("runC"), "index should not have synced yet: {no_sync}");

    let auto = run_ok(root, &["impact", "alpha", "beta"], &[("CGR_AUTO_SYNC", "1")]);
    assert!(auto.contains("runC"), "CGR_AUTO_SYNC=1 should pick up runC: {auto}");

    let nodes = run_ok(root, &["node", "alpha", "beta", "--code"], &[]);
    assert!(nodes.contains("return x + 1;") && nodes.contains("return x * 2;"), "multi-target node: {nodes}");
}

#[test]
fn unresolved_dynamic_dispatch_hints_work_across_base_and_delta() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "src/plugin.js", "export function handleCustomEvent(x) {\n  return x + 1;\n}\n");
    write(root, "src/dispatch.js", "export function dispatch(ctx) {\n  return ctx.handleCustomEvent(42);\n}\n");
    run_ok(root, &["init"], &[]);

    let callers = run_ok(root, &["callers", "handleCustomEvent"], &[]);
    assert!(callers.contains("No callers found"), "{callers}");
    assert!(callers.contains("unresolved / ambiguous references"), "callers must surface unresolved hint: {callers}");
    assert!(callers.contains("src/dispatch.js:2"), "{callers}");

    let callees = run_ok(root, &["callees", "dispatch"], &[]);
    assert!(callees.contains("No callees found"), "{callees}");
    assert!(callees.contains("unresolved / ambiguous references"), "callees must surface outgoing unresolved hint: {callees}");
    assert!(callees.contains("ctx.handleCustomEvent"), "{callees}");

    let impact = run_ok(root, &["impact", "handleCustomEvent"], &[]);
    assert!(impact.contains("unresolved / ambiguous references"), "impact must surface unresolved hint: {impact}");
    assert!(impact.contains("src/dispatch.js:2"), "{impact}");

    let explore = run_ok(root, &["explore", "handleCustomEvent"], &[]);
    assert!(explore.contains("unresolved / ambiguous references"), "one-symbol explore must surface unresolved hint: {explore}");
    assert!(explore.contains("src/dispatch.js:2"), "{explore}");

    let explore_dispatch = run_ok(root, &["explore", "dispatch callees"], &[]);
    assert!(explore_dispatch.contains("unresolved / ambiguous references"), "one-symbol explore must surface outgoing unresolved callees: {explore_dispatch}");
    assert!(explore_dispatch.contains("ctx.handleCustomEvent"), "{explore_dispatch}");

    // Shift call line and sync into a delta layer: hint must reflect delta line 4, NOT stale base line 2.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(root, "src/dispatch.js", "// line 1\n// line 2\nexport function dispatch(ctx) {\n  return ctx.handleCustomEvent(42);\n}\n");
    run_ok(root, &["sync"], &[]);

    let callers_delta = run_ok(root, &["callers", "handleCustomEvent"], &[]);
    assert!(callers_delta.contains("src/dispatch.js:4"), "delta line should appear: {callers_delta}");
    assert!(!callers_delta.contains("src/dispatch.js:2"), "replaced base line must not leak: {callers_delta}");
}
