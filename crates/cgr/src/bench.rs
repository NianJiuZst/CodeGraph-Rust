//! Development benchmarks (design doc 13.4): `bench index`, `bench sync`, `bench explore`.
//!
//! These commands are meant to be wrapped by `scripts/bench-local.sh`, which enforces the memory
//! guard on the machine. Nothing here modifies the project under test: `bench sync` works on a
//! copy of the tree unless `--in-place` is given.

use anyhow::{anyhow, bail, Context, Result};
use cg_core::{brand, NodeKind};
use cg_query::tools::{self, ToolEnv};
use cg_query::Project;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- statistics

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

#[derive(Clone, Debug, Default)]
struct Dist {
    n: usize,
    min: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
    mean: f64,
}

fn dist(mut v: Vec<f64>) -> Dist {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        return Dist::default();
    }
    Dist {
        n: v.len(),
        min: v[0],
        p50: percentile(&v, 50.0),
        p95: percentile(&v, 95.0),
        p99: percentile(&v, 99.0),
        max: v[v.len() - 1],
        mean: v.iter().sum::<f64>() / v.len() as f64,
    }
}

impl Dist {
    fn json(&self) -> Value {
        json!({"n": self.n, "min": self.min, "p50": self.p50, "p95": self.p95, "p99": self.p99, "max": self.max, "mean": self.mean})
    }
    fn line(&self, unit: &str) -> String {
        format!(
            "n={} min {:.1}{u} p50 {:.1}{u} p95 {:.1}{u} p99 {:.1}{u} max {:.1}{u} mean {:.1}{u}",
            self.n,
            self.min,
            self.p50,
            self.p95,
            self.p99,
            self.max,
            self.mean,
            u = unit
        )
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// A tiny deterministic generator (xorshift64*), so `--seed` reproduces a run exactly.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

fn project_root(path: Option<PathBuf>) -> Result<PathBuf> {
    let p = path.unwrap_or_else(|| PathBuf::from("."));
    cg_core::path::canonicalize(&p).with_context(|| format!("cannot open {}", p.display()))
}

// ---------------------------------------------------------------- queries

/// Symbol-ish names to build queries from: functions, methods, classes and the like.
fn sample_names(proj: &Project, rng: &mut Rng, want: usize) -> Vec<String> {
    let view = &proj.snap.view;
    let mut all: Vec<String> = Vec::new();
    for f in view.files() {
        for n in view.nodes_in_file(f) {
            let node = view.node(cg_core::NodeIdx(n));
            if matches!(node.kind(), NodeKind::Function | NodeKind::Method | NodeKind::Class | NodeKind::Struct | NodeKind::Interface | NodeKind::Trait)
                && node.name().len() >= 4
            {
                all.push(node.name().to_owned());
            }
        }
    }
    all.sort();
    all.dedup();
    let mut out = Vec::new();
    if all.is_empty() {
        return out;
    }
    for _ in 0..want {
        out.push(all[rng.below(all.len())].clone());
    }
    out
}

fn make_queries(proj: &Project, file: Option<&Path>, n: usize, seed: u64) -> Result<Vec<String>> {
    if let Some(f) = file {
        let text = std::fs::read_to_string(f).with_context(|| format!("cannot read {}", f.display()))?;
        let qs: Vec<String> = text.lines().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
        if qs.is_empty() {
            bail!("{} has no queries", f.display());
        }
        return Ok(qs);
    }
    let mut rng = Rng::new(seed);
    let names = sample_names(proj, &mut rng, n * 3);
    if names.len() < 3 {
        bail!("the index has too few symbols to build queries from");
    }
    let mut qs = Vec::with_capacity(n);
    for i in 0..n {
        let a = &names[i * 3];
        let b = &names[i * 3 + 1];
        let c = &names[i * 3 + 2];
        qs.push(match i % 4 {
            0 => a.clone(),
            1 => format!("{a} {b}"),
            2 => format!("how does {a} work with {b}"),
            _ => format!("{a} {b} {c}"),
        });
    }
    Ok(qs)
}

fn run_explore(proj: &Project, q: &str) -> (f64, usize) {
    let env = ToolEnv { graph: &proj.graph, history: None, status: proj.status_info() };
    let t = Instant::now();
    let out = tools::call(&env, "explore", &json!({"query": q}));
    (ms(t.elapsed()), out.text.len())
}

// ---------------------------------------------------------------- bench explore

pub fn explore(path: Option<PathBuf>, queries: Option<PathBuf>, count: usize, runs: usize, seed: u64, json_out: bool) -> Result<i32> {
    let root = project_root(path)?;
    let proj = Project::locate(&root).map_err(|e| anyhow!("{e} (run `{} init` first)", brand::BIN_NAME))?;
    let qs = make_queries(&proj, queries.as_deref(), count, seed)?;
    // one untimed pass so page faults on the mapped file do not count against the first queries
    for q in qs.iter().take(8) {
        run_explore(&proj, q);
    }
    let mut times = Vec::with_capacity(qs.len() * runs);
    let mut sizes = Vec::with_capacity(qs.len());
    let mut slowest: Vec<(f64, String)> = Vec::new();
    for r in 0..runs.max(1) {
        for q in &qs {
            let (t, len) = run_explore(&proj, q);
            times.push(t);
            if r == 0 {
                sizes.push(len as f64);
            }
            slowest.push((t, q.clone()));
        }
    }
    slowest.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    slowest.truncate(3);
    let d = dist(times);
    let s = dist(sizes);
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "bench": "explore", "root": root.display().to_string(), "queries": qs.len(), "runs": runs,
                "latencyMs": d.json(), "outputChars": s.json(),
                "slowest": slowest.iter().map(|(t, q)| json!({"ms": t, "query": q})).collect::<Vec<_>>(),
            }))?
        );
    } else {
        println!("explore on {} ({} queries x {} runs)", root.display(), qs.len(), runs);
        println!("  latency   {}", d.line(" ms"));
        println!("  output    {}", s.line(" chars"));
        for (t, q) in &slowest {
            println!("  slowest   {t:.1} ms  {q}");
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- bench index

/// Run one full index in a child process so its peak RSS belongs to that run alone.
fn child_index(root: &Path, threads: Option<usize>) -> Result<Value> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--path").arg(root).args(["index", "--quiet", "--report-json"]);
    if let Some(t) = threads {
        cmd.env(brand::env::THREADS, t.to_string());
    }
    let out = cmd.output().context("cannot start the indexing child process")?;
    if !out.status.success() {
        bail!("indexing failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    serde_json::from_slice(&out.stdout).context("the child process printed no report")
}

pub fn index(path: Option<PathBuf>, runs: usize, threads: Option<usize>, with_readers: usize, queries: Option<PathBuf>, json_out: bool) -> Result<i32> {
    let root = project_root(path)?;
    let runs = runs.max(1);
    let mut reports: Vec<Value> = Vec::new();

    // Readers need an existing snapshot to query while the next build runs.
    let stop = Arc::new(AtomicBool::new(false));
    let reader_times: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    let mut first = true;
    for _ in 0..runs {
        let r = child_index(&root, threads)?;
        reports.push(r);
        if first && with_readers > 0 {
            first = false;
            let proj = Project::locate(&root).map_err(|e| anyhow!("{e}"))?;
            let qs = Arc::new(make_queries(&proj, queries.as_deref(), 64, 1)?);
            drop(proj);
            for i in 0..with_readers {
                let (stop, times, qs, root) = (stop.clone(), reader_times.clone(), qs.clone(), root.clone());
                handles.push(std::thread::spawn(move || {
                    let mut k = i;
                    while !stop.load(Ordering::Relaxed) {
                        // a fresh snapshot per query, like an MCP session picking up a new generation
                        if let Ok(p) = Project::locate(&root) {
                            let (t, _) = run_explore(&p, &qs[k % qs.len()]);
                            times.lock().unwrap().push(t);
                        }
                        k += 1;
                    }
                }));
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    let f = |key: &str| -> Vec<f64> { reports.iter().filter_map(|r| r[key].as_f64()).collect() };
    let total = dist(f("totalMs"));
    let rss = dist(f("peakRssMb"));
    let mut phases: Vec<(String, f64)> = Vec::new();
    if let Some(obj) = reports[0]["phasesMs"].as_object() {
        for k in obj.keys() {
            let v: Vec<f64> = reports.iter().filter_map(|r| r["phasesMs"][k].as_f64()).collect();
            phases.push((k.clone(), dist(v).p50));
        }
    }
    let readers = dist(reader_times.lock().unwrap().clone());
    let r0 = &reports[0];
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "bench": "index", "root": root.display().to_string(), "runs": runs,
                "files": r0["files"], "nodes": r0["nodes"], "edges": r0["edges"], "refs": r0["refs"], "resolved": r0["resolved"],
                "sourceMb": r0["sourceMb"], "segmentMb": r0["segmentMb"], "threads": r0["threads"],
                "totalMs": total.json(), "peakRssMb": rss.json(),
                "phasesMsMedian": phases.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
                "readers": if with_readers > 0 { json!({"count": with_readers, "queryMs": readers.json()}) } else { Value::Null },
            }))?
        );
    } else {
        println!(
            "index {}: {} files, {} nodes, {} edges, {}/{} references resolved, source {} MB -> segment {} MB, {} threads",
            root.display(),
            r0["files"],
            r0["nodes"],
            r0["edges"],
            r0["resolved"],
            r0["refs"],
            r0["sourceMb"],
            r0["segmentMb"],
            r0["threads"]
        );
        println!("  total     {}", total.line(" ms"));
        println!("  peak RSS  {}", rss.line(" MB"));
        for (k, v) in &phases {
            println!("  {k:<14} {v:>9.1} ms (median)");
        }
        if with_readers > 0 {
            println!("  readers   {} threads, explore while indexing: {}", with_readers, readers.line(" ms"));
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- bench sync

fn copy_tree(src: &Path, dst: &Path) -> Result<u64> {
    let mut bytes = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let ty = entry.file_type()?;
        let to = dst.join(&name);
        if ty.is_dir() {
            if matches!(name.to_string_lossy().as_ref(), ".git" | ".cgr" | "node_modules" | "target") {
                continue;
            }
            std::fs::create_dir_all(&to)?;
            bytes += copy_tree(&entry.path(), &to)?;
        } else if ty.is_file() {
            bytes += std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(bytes)
}

fn comment_prefix(ext: &str) -> Option<&'static str> {
    match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "go" | "java" | "rs" | "kt" | "kts" => Some("//"),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "cs" | "php" | "swift" => Some("//"),
        "py" | "rb" | "rake" => Some("#"),
        _ => None,
    }
}

fn add_function(ext: &str, n: u64) -> Option<String> {
    Some(match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => format!("\nexport function cgrBench{n}() {{\n  return {n};\n}}\n"),
        "py" => format!("\n\ndef cgr_bench_{n}():\n    return {n}\n"),
        "go" => format!("\nfunc CgrBench{n}() int {{ return {n} }}\n"),
        "rs" => format!("\npub fn cgr_bench_{n}() -> u64 {{ {n} }}\n"),
        "kt" | "kts" => format!("\nfun cgrBench{n}(): Int = {n}\n"),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hxx" => format!("\nstatic int cgr_bench_{n}(void) {{ return {n}; }}\n"),
        "php" => format!("\nfunction cgr_bench_{n}() {{ return {n}; }}\n"),
        "rb" | "rake" => format!("\ndef cgr_bench_{n}\n  {n}\nend\n"),
        "swift" => format!("\nfunc cgrBench{n}() -> Int {{ {n} }}\n"),
        _ => return None,
    })
}

fn rename_regex(ext: &str) -> Option<regex::Regex> {
    let pat = match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => r"\bfunction\s+([A-Za-z_$][A-Za-z0-9_$]{3,})",
        "py" => r"\bdef\s+([A-Za-z_][A-Za-z0-9_]{3,})",
        "go" => r"\bfunc\s+([A-Za-z_][A-Za-z0-9_]{3,})\s*\(",
        "rs" => r"\bfn\s+([A-Za-z_][A-Za-z0-9_]{3,})",
        "kt" | "kts" => r"\bfun\s+([A-Za-z_][A-Za-z0-9_]{3,})",
        "php" => r"\bfunction\s+([A-Za-z_][A-Za-z0-9_]{3,})",
        "rb" | "rake" => r"\bdef\s+(?:self\.)?([A-Za-z_][A-Za-z0-9_]{3,})",
        "swift" => r"\bfunc\s+([A-Za-z_][A-Za-z0-9_]{3,})",
        // definitions `name(params) {` (keywords such as `while (..) {` are filtered out)
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "cs" => r"\b([A-Za-z_][A-Za-z0-9_]{3,})\s*\([^;{}()]*\)\s*(?:const\s*)?\{",
        _ => return None,
    };
    regex::Regex::new(pat).ok()
}

/// Names the C-family / C# rename rule may capture that are not declarations.
fn rename_excluded(name: &str) -> bool {
    matches!(
        name,
        "while"
            | "switch"
            | "return"
            | "sizeof"
            | "catch"
            | "defined"
            | "alignof"
            | "decltype"
            | "typeid"
            | "noexcept"
            | "static_assert"
            | "foreach"
            | "using"
            | "lock"
            | "fixed"
            | "checked"
            | "unchecked"
            | "else"
            | "operator"
            | "throw"
            | "delete"
            | "typeof"
            | "nameof"
            | "default"
            | "await"
            | "unsafe"
            | "when"
            | "with"
    )
}

fn import_line(ext: &str, n: u64) -> Option<String> {
    Some(match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => format!("import {{ cgrBench{n} }} from './cgr_bench_{n}';\n"),
        "py" => format!("import cgr_bench_{n}\n"),
        "rs" => "use std::fmt::Debug as _;\n".to_owned(),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "#include <stdint.h>\n".to_owned(),
        "cs" => "using System.Text;\n".to_owned(),
        "rb" | "rake" => "require 'set'\n".to_owned(),
        "swift" => "import Foundation\n".to_owned(),
        _ => return None,
    })
}

#[derive(Default)]
struct EditCounts {
    body: usize,
    add_fn: usize,
    rename: usize,
    add_import: usize,
    add_file: usize,
}

/// Apply one random edit to a source file under `root`. Returns a description of the edit.
fn random_edit(root: &Path, files: &[(String, String)], rng: &mut Rng, n: u64, counts: &mut EditCounts) -> Result<String> {
    let (rel, ext) = &files[rng.below(files.len())];
    let path = root.join(rel);
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let kind = rng.below(100);
    let edited: String;
    let what: String;
    if kind < 50 {
        // function body edit: a new comment line somewhere inside the file shifts everything below
        let prefix = comment_prefix(ext).ok_or_else(|| anyhow!("no comment syntax for .{ext}"))?;
        let mut lines: Vec<&str> = text.lines().collect();
        let at = rng.below(lines.len().max(1));
        let note = format!("{prefix} cgr bench edit {n}");
        lines.insert(at.min(lines.len()), &note);
        edited = lines.join("\n") + "\n";
        what = format!("comment line {} in {rel}", at + 1);
        counts.body += 1;
    } else if kind < 70 {
        match add_function(ext, n) {
            Some(f) => {
                edited = format!("{text}{f}");
                what = format!("new function at the end of {rel}");
                counts.add_fn += 1;
            }
            None => return random_edit(root, files, rng, n, counts),
        }
    } else if kind < 85 {
        let re = rename_regex(ext).ok_or_else(|| anyhow!("no rename rule for .{ext}"))?;
        let names: Vec<String> = re.captures_iter(&text).map(|c| c[1].to_owned()).filter(|n| !rename_excluded(n)).collect();
        if names.is_empty() {
            return random_edit(root, files, rng, n, counts);
        }
        let old = &names[rng.below(names.len())];
        let word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(old)))?;
        edited = word.replace_all(&text, format!("{old}_r{n}").as_str()).into_owned();
        what = format!("rename {old} to {old}_r{n} in {rel}");
        counts.rename += 1;
    } else if kind < 95 {
        match import_line(ext, n) {
            Some(l) => {
                edited = format!("{l}{text}");
                what = format!("new import at the top of {rel}");
                counts.add_import += 1;
            }
            None => return random_edit(root, files, rng, n, counts),
        }
    } else {
        // a brand-new file next to an existing one
        let Some(body) = add_function(ext, n) else { return random_edit(root, files, rng, n, counts) };
        let dir = path.parent().unwrap_or(root);
        let name = format!("cgr_bench_{n}.{ext}");
        std::fs::write(dir.join(&name), body.trim_start())?;
        counts.add_file += 1;
        return Ok(format!("new file {name} next to {rel}"));
    }
    std::fs::write(&path, edited)?;
    Ok(what)
}

pub fn sync(path: Option<PathBuf>, edits: usize, seed: u64, in_place: bool, verify: bool, verbose: bool, json_out: bool) -> Result<i32> {
    let src = project_root(path)?;
    let mut keep: Option<tempfile_guard::Dir> = None;
    let root = if in_place {
        src.clone()
    } else {
        let dir = tempfile_guard::Dir::new("cgr-bench-sync")?;
        let t = Instant::now();
        let bytes = copy_tree(&src, dir.path())?;
        eprintln!("copied {:.1} MB to {} in {:.1}s", bytes as f64 / 1048576.0, dir.path().display(), t.elapsed().as_secs_f64());
        let root = dir.path().to_path_buf();
        keep = Some(dir);
        root
    };
    let t = Instant::now();
    let report = cg_resolve::index_project(&root, &Default::default())?;
    let index_ms = ms(t.elapsed());

    let scan = cg_scan::scan(&cg_scan::ScanOptions::new(&root));
    let files: Vec<(String, String)> = scan
        .files
        .iter()
        .filter_map(|f| {
            let ext = f.rel.rsplit('.').next()?.to_ascii_lowercase();
            (comment_prefix(&ext).is_some() && f.size > 0 && !f.rel.contains("cgr_bench_")).then(|| (f.rel.clone(), ext))
        })
        .collect();
    if files.is_empty() {
        bail!("no editable source files in {}", root.display());
    }

    let mut rng = Rng::new(seed);
    let mut counts = EditCounts::default();
    let mut times = Vec::with_capacity(edits);
    let mut rebuilds: Vec<String> = Vec::new();
    let mut slow: Vec<(f64, String)> = Vec::new();
    let mut merges = 0usize;
    let mut phase_sum: std::collections::BTreeMap<&'static str, f64> = Default::default();
    for n in 0..edits as u64 {
        let what = random_edit(&root, &files, &mut rng, n + 1, &mut counts)?;
        let t = Instant::now();
        let rep = cg_sync::sync(&root, &Default::default())?;
        let took = ms(t.elapsed());
        times.push(took);
        slow.push((took, format!("{what}: {}", rep.summary())));
        if verbose {
            eprintln!("edit {:>4}: {:>8.1} ms  {what}\n            {}", n + 1, took, rep.summary());
        }
        if let Some(why) = &rep.full_rebuild {
            if why.contains("merge") || why.contains("delta layer") {
                merges += 1;
            }
            rebuilds.push(format!("{why} <- {what}"));
        } else {
            for (k, d) in &rep.timings {
                *phase_sum.entry(k).or_default() += ms(*d);
            }
        }
    }
    let d = dist(times);
    slow.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    slow.truncate(5);
    let mut verdict: Option<bool> = None;
    if verify {
        verdict = Some(crate::cmds::debug_diff_inner(&root, 10, json_out)?);
    }
    let delta_syncs = edits - rebuilds.len();
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "bench": "sync", "root": src.display().to_string(), "seed": seed, "edits": edits,
                "files": report.files, "initialIndexMs": index_ms,
                "latencyMs": d.json(), "deltaSyncs": delta_syncs, "fullRebuilds": rebuilds.len(), "merges": merges,
                "rebuildReasons": rebuilds,
                "slowest": slow.iter().map(|(t, w)| json!({"ms": t, "edit": w})).collect::<Vec<_>>(),
                "avgPhaseMs": phase_sum.iter().map(|(k, v)| (k.to_string(), json!(v / delta_syncs.max(1) as f64))).collect::<serde_json::Map<_, _>>(),
                "edited": {"body": counts.body, "addFunction": counts.add_fn, "rename": counts.rename, "addImport": counts.add_import, "addFile": counts.add_file},
                "incrementalEqualsFull": verdict,
            }))?
        );
    } else {
        println!("sync on {} ({} files, initial index {:.0} ms): {} edits, seed {}", src.display(), report.files, index_ms, edits, seed);
        println!("  latency   {}", d.line(" ms"));
        println!("  {} delta syncs, {} full rebuilds ({} merges)", delta_syncs, rebuilds.len(), merges);
        for (k, v) in &phase_sum {
            println!("  {k:<14} {:>8.2} ms avg", v / delta_syncs.max(1) as f64);
        }
        for why in rebuilds.iter().filter(|w| !w.contains("(merge)")).take(8) {
            println!("  rebuild: {why}");
        }
        for (t, w) in &slow {
            println!("  slowest: {t:.1} ms  {w}");
        }
        println!(
            "  edits: {} body, {} add-function, {} rename, {} add-import, {} add-file",
            counts.body, counts.add_fn, counts.rename, counts.add_import, counts.add_file
        );
        if let Some(ok) = verdict {
            println!("  incremental == full rebuild: {}", if ok { "yes" } else { "NO" });
        }
    }
    drop(keep);
    Ok(if verdict == Some(false) { 1 } else { 0 })
}

/// Minimal scratch directory (the `tempfile` crate is a dev-dependency only).
mod tempfile_guard {
    use anyhow::Result;
    use std::path::{Path, PathBuf};

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new(prefix: &str) -> Result<Dir> {
            let base = std::env::temp_dir();
            let pid = std::process::id();
            for i in 0..1000u32 {
                let p = base.join(format!("{prefix}-{pid}-{i}"));
                if std::fs::create_dir(&p).is_ok() {
                    return Ok(Dir(p));
                }
            }
            anyhow::bail!("cannot create a scratch directory in {}", base.display())
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

// ---------------------------------------------------------------- bench compare

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Ms,
    Mb,
}

/// `YYYY-MM-DD_HHMMSS` in UTC, so summaries sort by time and never overwrite each other.
fn unix_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    // civil date from days since 1970-01-01
    let z = secs.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let rem = secs.rem_euclid(86400);
    format!("{:04}-{:02}-{:02}_{:02}{:02}{:02}", if m <= 2 { y + 1 } else { y }, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Collect the headline numbers of every `<repo>.<kind>.json` in `dir` (and the process-tree
/// peaks that `scripts/bench-local.sh` measured, `<repo>.<kind>.guard.json`).
fn collect_metrics(dir: &Path) -> Result<std::collections::BTreeMap<String, (f64, Unit)>> {
    let mut out = std::collections::BTreeMap::new();
    let mut names: Vec<PathBuf> =
        std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    names.sort();
    for p in names {
        let Some(file) = p.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(stem) = file.strip_suffix(".json") else { continue };
        let v: Value = match std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            Some(v) => v,
            None => continue,
        };
        if let Some(name) = stem.strip_suffix(".guard") {
            if let Some(mb) = v["peakRssMb"].as_f64() {
                out.insert(format!("{name}.process-tree-peak-rss-mb"), (mb, Unit::Mb));
            }
            continue;
        }
        let Some((repo, kind)) = stem.rsplit_once('.') else { continue };
        let mut put = |k: &str, x: &Value, unit: Unit| {
            if let Some(f) = x.as_f64() {
                out.insert(format!("{repo}.{kind}.{k}"), (f, unit));
            }
        };
        match kind {
            "index" => {
                put("total-ms-p50", &v["totalMs"]["p50"], Unit::Ms);
                put("peak-rss-mb-p50", &v["peakRssMb"]["p50"], Unit::Mb);
            }
            "sync" => {
                put("latency-ms-p50", &v["latencyMs"]["p50"], Unit::Ms);
                put("latency-ms-p95", &v["latencyMs"]["p95"], Unit::Ms);
            }
            "explore" => {
                put("latency-ms-p50", &v["latencyMs"]["p50"], Unit::Ms);
                put("latency-ms-p95", &v["latencyMs"]["p95"], Unit::Ms);
                put("latency-ms-p99", &v["latencyMs"]["p99"], Unit::Ms);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Summarise a results directory, compare it with the newest earlier summary of the same tier in
/// `history` and fail when any timing is more than `threshold` percent slower (design doc 13.4).
pub fn compare(results: PathBuf, history: PathBuf, tier: String, threshold: f64, save: bool, force_save: bool) -> Result<i32> {
    let cur = collect_metrics(&results)?;
    if cur.is_empty() {
        bail!("no benchmark results found in {}", results.display());
    }
    std::fs::create_dir_all(&history)?;
    let suffix = format!("-{tier}.json");
    let mut earlier: Vec<PathBuf> = std::fs::read_dir(&history)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)))
        .collect();
    earlier.sort();
    let base_file = earlier.last().cloned();
    let base: std::collections::BTreeMap<String, f64> = match &base_file {
        Some(f) => {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(f)?)?;
            v["metrics"].as_object().map(|m| m.iter().filter_map(|(k, x)| x.as_f64().map(|f| (k.clone(), f))).collect()).unwrap_or_default()
        }
        None => Default::default(),
    };
    let mut regressions = 0;
    match &base_file {
        Some(f) => println!("comparing with {} (threshold {threshold}%)", f.display()),
        None => println!("no earlier summary for tier `{tier}`: nothing to compare with"),
    }
    for (k, (x, unit)) in &cur {
        let Some(b) = base.get(k) else {
            println!("  new      {k:<48} {x:>10.1}");
            continue;
        };
        let ratio = if *b > 0.0 { x / b } else { 1.0 };
        // timings under a couple of milliseconds are noise
        let slower = *unit == Unit::Ms && ratio > 1.0 + threshold / 100.0 && x - b > 2.0;
        let bigger = *unit == Unit::Mb && ratio > 1.0 + threshold / 100.0 && x - b > 8.0;
        let tag = if slower {
            regressions += 1;
            "SLOWER"
        } else if bigger {
            "bigger"
        } else {
            "ok"
        };
        println!("  {tag:<8} {k:<48} {b:>10.1} -> {x:>10.1}  ({:+.1}%)", (ratio - 1.0) * 100.0);
    }
    // A regressing run does not silently become the new baseline; `--force-save` re-baselines on purpose.
    if save && (regressions == 0 || base_file.is_none() || force_save) {
        let stamp = unix_stamp();
        let path = history.join(format!("{stamp}{suffix}"));
        let metrics: serde_json::Map<String, Value> = cur.iter().map(|(k, (x, _))| (k.clone(), json!(x))).collect();
        std::fs::write(&path, serde_json::to_string_pretty(&json!({"schema": 1, "tier": tier, "date": stamp, "metrics": metrics}))? + "\n")?;
        println!("saved {}", path.display());
    } else if save {
        println!("not saved as the new baseline because of the regressions above (use --force-save to re-baseline)");
    }
    if regressions > 0 {
        println!("{regressions} metric(s) are more than {threshold}% slower than the baseline");
        return Ok(1);
    }
    Ok(0)
}
