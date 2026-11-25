//! Command implementations. Query-side commands go through the same `cg_query::tools`
//! handlers the MCP server uses, so the CLI and the agent see identical text.

use anyhow::{anyhow, Context, Result};
use cg_core::{brand, FileId, NodeIdx};
use cg_query::explore::History;
use cg_query::tools::{self, ToolEnv};
use cg_query::Project;
use cg_store::dump;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    Off,
    Auto,
    Explicit,
}

pub struct QueryCtx {
    pub base: Option<PathBuf>,
    pub sync: SyncMode,
    pub session: Option<String>,
}

pub struct NodeArgs {
    pub code: bool,
    pub file: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    pub symbols_only: bool,
    pub compact: bool,
    pub max_chars: Option<usize>,
    pub no_dedup: bool,
}

fn start_dir(p: Option<PathBuf>) -> Result<PathBuf> {
    let p = p.unwrap_or_else(|| PathBuf::from("."));
    p.canonicalize().with_context(|| format!("cannot open {}", p.display()))
}

fn open(p: Option<PathBuf>) -> Result<Project> {
    let start = start_dir(p)?;
    Project::locate(&start).map_err(|e| anyhow!("{e} (looked above {}; run `{} init` first)", start.display(), brand::BIN_NAME))
}

#[derive(Default)]
struct QuerySyncStat {
    checked: bool,
    synced: bool,
    sync_ms: f64,
}

fn open_for_query(p: Option<PathBuf>, sync: SyncMode) -> Result<(Project, QuerySyncStat)> {
    let start = start_dir(p)?;
    let store = cg_store::Store::locate(&start)
        .ok_or_else(|| anyhow!("no {} index found above {} (run `{} init` first)", brand::DATA_DIR, start.display(), brand::BIN_NAME))?;
    let mut stat = QuerySyncStat::default();
    if sync != SyncMode::Off {
        stat.checked = true;
        let timeout = match sync {
            SyncMode::Explicit => Duration::from_secs(5),
            SyncMode::Auto => Duration::from_millis(150),
            SyncMode::Off => unreachable!(),
        };
        let opts = cg_sync::SyncOptions { lock_timeout: Some(timeout), ..Default::default() };
        let t_sync = Instant::now();
        match cg_sync::sync(&store.root, &opts) {
            Ok(rep) => {
                stat.synced = !rep.up_to_date;
                stat.sync_ms = t_sync.elapsed().as_secs_f64() * 1e3;
            }
            Err(e) => {
                stat.sync_ms = t_sync.elapsed().as_secs_f64() * 1e3;
                if sync == SyncMode::Explicit {
                    eprintln!("warning: sync skipped ({e}); answering from existing snapshot");
                }
            }
        }
    }
    let proj = Project::open(store).map_err(|e| anyhow!("{e}"))?;
    Ok((proj, stat))
}

fn load_session(ctx: &QueryCtx, root: &Path, disabled: bool) -> (Option<Arc<Mutex<History>>>, Option<PathBuf>) {
    if disabled {
        return (None, None);
    }
    match ctx.session.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(sid) => {
            let path = session_path(root, sid);
            (Some(Arc::new(Mutex::new(History::load(&path)))), Some(path))
        }
        None => (None, None),
    }
}

fn save_session(history: Option<Arc<Mutex<History>>>, sess_file: Option<PathBuf>) {
    if let (Some(h), Some(path)) = (history, sess_file) {
        let guard = h.lock().unwrap_or_else(|e| e.into_inner());
        if guard.save_merged(&path).is_ok() {
            if let Some(dir) = path.parent() {
                prune_sessions(dir);
            }
        }
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn session_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(brand::DATA_DIR).join("sessions").join(format!("{:016x}.json", fnv1a_64(session_id.as_bytes())))
}

fn prune_sessions(dir: &Path) {
    const MAX_SESSIONS: usize = 32;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .filter_map(|p| std::fs::metadata(&p).ok().and_then(|m| m.modified().ok()).map(|t| (t, p)))
        .collect();
    if files.len() <= MAX_SESSIONS {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    for (_, p) in files.iter().take(files.len() - MAX_SESSIONS) {
        let _ = std::fs::remove_file(p);
    }
}

fn call_proj(proj: &Project, tool: &str, args: serde_json::Value, history: Option<Arc<Mutex<History>>>, sync_stat: &QuerySyncStat, t0: Instant) -> i32 {
    let dedup_enabled = history.is_some() && !args.get("noDedup").and_then(|v| v.as_bool()).unwrap_or(false);
    let status = proj.status_info();
    let gen = status.generation;
    let env = ToolEnv { graph: &proj.graph, history, status };
    let out = tools::call(&env, tool, &args);
    println!("{}", out.text);
    cg_query::telemetry::record(cg_query::telemetry::CallStats {
        mode: "cli",
        tool,
        project: proj.root(),
        duration_ms: t0.elapsed().as_secs_f64() * 1e3,
        sync_checked: sync_stat.checked,
        synced: sync_stat.synced,
        sync_ms: sync_stat.sync_ms,
        generation: gen,
        output_bytes: out.text.len(),
        compact: args.get("compact").and_then(|v| v.as_bool()).unwrap_or(false) || args.get("symbolsOnly").and_then(|v| v.as_bool()).unwrap_or(false),
        max_chars: args.get("maxChars").and_then(|v| v.as_u64()).map(|n| n as usize),
        dedup_enabled,
        meta: out.meta,
        is_error: out.is_error,
    });
    if out.is_error {
        1
    } else {
        0
    }
}

fn call(ctx: &QueryCtx, tool: &str, args: serde_json::Value) -> Result<i32> {
    let t0 = Instant::now();
    let (proj, sync_stat) = open_for_query(ctx.base.clone(), ctx.sync)?;
    Ok(call_proj(&proj, tool, args, None, &sync_stat, t0))
}

pub fn init(path: Option<PathBuf>, local_only: bool) -> Result<i32> {
    let root = start_dir(path)?;
    let report = cg_resolve::index_project(&root, &Default::default())?;
    if local_only {
        let exclude = root.join(".git/info/exclude");
        if exclude.parent().is_some_and(|d| d.is_dir()) {
            let cur = std::fs::read_to_string(&exclude).unwrap_or_default();
            if !cur.lines().any(|l| l.trim() == format!("{}/", brand::DATA_DIR)) {
                let sep = if cur.ends_with('\n') || cur.is_empty() { "" } else { "\n" };
                std::fs::write(&exclude, format!("{cur}{sep}{}/\n", brand::DATA_DIR))?;
            }
        }
    }
    println!("indexed {} files, {} nodes, {} edges in {:.2}s", report.files, report.nodes, report.edges, report.total().as_secs_f64());
    Ok(0)
}

pub fn index(path: Option<PathBuf>, _force: bool, quiet: bool, timings: bool, report_json: bool) -> Result<i32> {
    let root = start_dir(path)?;
    let report = cg_resolve::index_project(&root, &Default::default())?;
    if report_json {
        let phases: serde_json::Map<String, serde_json::Value> = report.timings.iter().map(|(k, d)| (k.to_string(), json!(d.as_secs_f64() * 1e3))).collect();
        println!(
            "{}",
            json!({
                "files": report.files, "nodes": report.nodes, "edges": report.edges, "refs": report.refs, "resolved": report.resolved_refs,
                "totalMs": report.total().as_secs_f64() * 1e3, "peakRssMb": report.peak_rss_mb, "threads": report.threads,
                "sourceMb": report.source_bytes / (1 << 20), "segmentMb": report.segment_bytes / (1 << 20), "phasesMs": phases,
                "spilledKernelBytes": report.spilled_kernel_bytes, "budgetMb": report.budget_mb,
            })
        );
        return Ok(0);
    }
    if !quiet {
        println!(
            "indexed {} files ({} unsupported, {} too large), {} nodes, {} edges, {}/{} references resolved",
            report.files, report.unsupported_files, report.too_large_files, report.nodes, report.edges, report.resolved_refs, report.refs
        );
    }
    if timings || std::env::var(brand::env::TIMINGS).is_ok() {
        print!("{}", report.render_timings());
    }
    Ok(0)
}

pub fn sync(path: Option<PathBuf>, quiet: bool) -> Result<i32> {
    let start = start_dir(path)?;
    let store = cg_store::Store::locate(&start)
        .ok_or_else(|| anyhow!("no {} index found from {} (run `{} init` first)", brand::DATA_DIR, start.display(), brand::BIN_NAME))?;
    let report = cg_sync::sync(&store.root, &Default::default())?;
    if !quiet {
        println!("{}", report.summary());
    }
    Ok(0)
}

pub fn status(path: Option<PathBuf>, json: bool, strict: bool, verify: bool) -> Result<i32> {
    let proj = open(path)?;
    if verify {
        proj.snap.view.base.verify()?;
        if let Some(d) = &proj.snap.view.delta {
            d.seg.verify()?;
        }
    }
    let m = &proj.snap.manifest;
    let info = proj.status_info();
    let ref_stats = dump::ref_stats(&proj.snap.view);
    let changes = cg_sync::measure_changes(proj.root(), &proj.snap.view).ok();
    if json {
        let rc = |k: &str| ref_stats.iter().find(|(name, _)| *name == k).map_or(0, |(_, n)| *n);
        let total: usize = ref_stats.iter().map(|(_, n)| *n).sum();
        let resolved = rc("Resolved");
        let resolved_rate = if total == 0 { 0.0 } else { ((resolved as f64 / total as f64) * 10_000.0).round() / 10_000.0 };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": 1,
                "root": proj.root().display().to_string(),
                "files": info.files, "nodes": info.nodes, "edges": info.edges,
                "generation": m.generation, "engineVersion": m.engine_version, "engineOutdated": info.engine_outdated,
                "bytes": info.bytes, "updatedAt": info.built_at, "deltaLayer": m.delta.is_some(),
                "verified": verify,
                "refs": {
                    "total": total,
                    "resolved": resolved,
                    "noCandidate": rc("NoCandidate"),
                    "ambiguous": rc("Ambiguous"),
                    "external": rc("External"),
                    "localBinding": rc("LocalBinding"),
                    "builtin": rc("Builtin"),
                    "pending": rc("Pending"),
                    "resolvedRate": resolved_rate,
                },
                "changes": changes.as_ref().map(|c| json!({"added": c.added, "modified": c.modified, "removed": c.removed})),
            }))?
        );
    } else {
        println!("files {}  nodes {}  edges {}  generation {}", m.n_files, m.n_nodes, m.n_edges, m.generation);
        for (name, n) in &ref_stats {
            println!("  refs {name:<12} {n}");
        }
        println!("  size {:.2} MB  updated {}", info.bytes as f64 / 1048576.0, info.built_at);
        if info.engine_outdated {
            println!("  note: index built by an older engine; run `{} index` to rebuild", brand::BIN_NAME);
        }
        match &changes {
            Some(c) => println!("  changes since last sync: {}", c.describe()),
            None => println!("  changes since last sync: unknown"),
        }
    }
    if strict && changes.is_some_and(|c| c.total() > 0) {
        return Ok(1);
    }
    Ok(0)
}

pub fn query(ctx: &QueryCtx, search: &str, kind: Option<String>, limit: usize, max_chars: Option<usize>, json_out: bool) -> Result<i32> {
    if !json_out {
        let mut args = json!({"query": search, "limit": limit});
        if let Some(k) = kind {
            args["kind"] = json!(k);
        }
        if let Some(mc) = max_chars {
            args["maxChars"] = json!(mc);
        }
        return call(ctx, "search", args);
    }
    let t0 = Instant::now();
    let (proj, sync_stat) = open_for_query(ctx.base.clone(), ctx.sync)?;
    let g = &proj.graph;
    let kinds: Vec<cg_core::NodeKind> =
        kind.as_deref().and_then(|k| if k == "type" { Some(cg_core::NodeKind::TypeAlias) } else { cg_core::NodeKind::parse(k) }).into_iter().collect();
    let hits = g.search(search, &cg_query::SearchOptions { limit, kinds, ..Default::default() });
    let arr: Vec<_> = hits
        .iter()
        .map(|h| {
            json!({
                "name": g.name(h.node), "kind": g.kind(h.node).as_str(), "qualifiedName": g.qname(h.node),
                "file": g.node_path(h.node), "line": g.line(h.node),
                "signature": g.view.node(h.node).signature(), "score": (h.score * 100.0).round() / 100.0,
            })
        })
        .collect();
    let out = if let Some(cap) = max_chars {
        let mut kept: Vec<serde_json::Value> = Vec::new();
        let mut truncated = false;
        for item in arr {
            kept.push(item);
            let candidate = serde_json::to_string_pretty(&json!({"schema": 1, "truncated": true, "results": kept}))?;
            if candidate.len() > cap && kept.len() > 1 {
                kept.pop();
                truncated = true;
                break;
            }
        }
        if truncated {
            serde_json::to_string_pretty(&json!({"schema": 1, "truncated": true, "results": kept}))?
        } else {
            serde_json::to_string_pretty(&json!({"schema": 1, "results": kept}))?
        }
    } else {
        serde_json::to_string_pretty(&json!({"schema": 1, "results": arr}))?
    };
    println!("{out}");
    cg_query::telemetry::record(cg_query::telemetry::CallStats {
        mode: "cli",
        tool: "search",
        project: proj.root(),
        duration_ms: t0.elapsed().as_secs_f64() * 1e3,
        sync_checked: sync_stat.checked,
        synced: sync_stat.synced,
        sync_ms: sync_stat.sync_ms,
        generation: proj.snap.manifest.generation,
        output_bytes: out.len(),
        compact: false,
        max_chars,
        dedup_enabled: false,
        meta: Default::default(),
        is_error: false,
    });
    Ok(0)
}

pub fn explore(ctx: &QueryCtx, query: &str, max_files: Option<usize>, max_chars: Option<usize>, compact: bool, no_dedup: bool) -> Result<i32> {
    let t0 = Instant::now();
    let (proj, sync_stat) = open_for_query(ctx.base.clone(), ctx.sync)?;
    let mut args = json!({"query": query, "compact": compact, "noDedup": no_dedup});
    if let Some(m) = max_files {
        args["maxFiles"] = json!(m);
    }
    if let Some(c) = max_chars {
        args["maxChars"] = json!(c);
    }
    let (history, sess_file) = load_session(ctx, proj.root(), no_dedup || compact);
    let code = call_proj(&proj, "explore", args, history.clone(), &sync_stat, t0);
    save_session(history, sess_file);
    Ok(code)
}

pub fn node(ctx: &QueryCtx, targets: &[String], opts: NodeArgs) -> Result<i32> {
    let t0 = Instant::now();
    let (proj, sync_stat) = open_for_query(ctx.base.clone(), ctx.sync)?;
    let (history, sess_file) = load_session(ctx, proj.root(), opts.no_dedup);
    let status = proj.status_info();
    let gen = status.generation;
    let env = ToolEnv { graph: &proj.graph, history: history.clone(), status };
    let mut parts: Vec<String> = Vec::with_capacity(targets.len());
    let mut any_err = false;
    let mut meta = cg_query::tools::ToolMeta::default();
    let mut used = 0usize;
    for (idx, target) in targets.iter().enumerate() {
        let sep = if idx > 0 { 7 } else { 0 };
        let mut per_cap = opts.max_chars;
        if let Some(total_cap) = opts.max_chars {
            if idx > 0 {
                let remaining = total_cap.saturating_sub(used + sep);
                if remaining < 560 {
                    parts.push(format!("> … +{} more target(s) omitted to fit `--max-chars {total_cap}`.", targets.len() - idx));
                    break;
                }
                per_cap = Some(remaining);
            }
        }
        // A name that resolves to a symbol is a symbol; otherwise treat the argument as a file.
        let is_symbol = !proj.graph.resolve_symbol(target).is_empty();
        let mut args = json!({"includeCode": opts.code, "compact": opts.compact, "noDedup": opts.no_dedup});
        if let Some(mc) = per_cap {
            args["maxChars"] = json!(mc);
        }
        if is_symbol {
            args["symbol"] = json!(target);
            if let Some(f) = &opts.file {
                args["file"] = json!(f);
            }
        } else {
            args["file"] = json!(target);
            if let Some(o) = opts.offset {
                args["offset"] = json!(o);
            }
            if let Some(l) = opts.limit {
                args["limit"] = json!(l);
            }
            args["symbolsOnly"] = json!(opts.symbols_only);
        }
        let out = tools::call(&env, "node", &args);
        any_err |= out.is_error;
        meta.merge(&out.meta);
        used += sep + out.text.len();
        parts.push(out.text);
    }
    let joined = parts.join("\n\n---\n\n");
    println!("{joined}");
    save_session(history, sess_file);
    cg_query::telemetry::record(cg_query::telemetry::CallStats {
        mode: "cli",
        tool: "node",
        project: proj.root(),
        duration_ms: t0.elapsed().as_secs_f64() * 1e3,
        sync_checked: sync_stat.checked,
        synced: sync_stat.synced,
        sync_ms: sync_stat.sync_ms,
        generation: gen,
        output_bytes: joined.len(),
        compact: opts.compact || opts.symbols_only,
        max_chars: opts.max_chars,
        dedup_enabled: ctx.session.as_deref().is_some_and(|s| !s.trim().is_empty()) && !opts.no_dedup,
        meta,
        is_error: any_err,
    });
    Ok(if any_err { 1 } else { 0 })
}

#[allow(clippy::too_many_arguments)]
pub fn files(
    ctx: &QueryCtx,
    dir: Option<String>,
    pattern: Option<String>,
    format: &str,
    meta: bool,
    tests_only: bool,
    exclude_tests: bool,
    max_depth: Option<usize>,
    max_chars: Option<usize>,
) -> Result<i32> {
    let mut args = json!({"format": format, "includeMetadata": meta, "testsOnly": tests_only, "excludeTests": exclude_tests});
    if let Some(d) = dir {
        args["path"] = json!(d);
    }
    if let Some(p) = pattern {
        args["pattern"] = json!(p);
    }
    if let Some(m) = max_depth {
        args["maxDepth"] = json!(m);
    }
    if let Some(mc) = max_chars {
        args["maxChars"] = json!(mc);
    }
    call(ctx, "files", args)
}

pub fn callers(ctx: &QueryCtx, symbols: &[String], file: Option<String>, limit: usize, max_chars: Option<usize>, no_dedup: bool, callers: bool) -> Result<i32> {
    let t0 = Instant::now();
    let (proj, sync_stat) = open_for_query(ctx.base.clone(), ctx.sync)?;
    let (history, sess_file) = load_session(ctx, proj.root(), no_dedup);
    let status = proj.status_info();
    let gen = status.generation;
    let env = ToolEnv { graph: &proj.graph, history: history.clone(), status };
    let tool = if callers { "callers" } else { "callees" };
    let mut parts: Vec<String> = Vec::with_capacity(symbols.len());
    let mut any_err = false;
    let mut meta = cg_query::tools::ToolMeta::default();
    let mut used = 0usize;
    for (idx, symbol) in symbols.iter().enumerate() {
        let sep = if idx > 0 { 7 } else { 0 };
        let mut per_cap = max_chars;
        if let Some(total_cap) = max_chars {
            if idx > 0 {
                let remaining = total_cap.saturating_sub(used + sep);
                if remaining < 560 {
                    parts.push(format!("> … +{} more symbol(s) omitted to fit `--max-chars {total_cap}`.", symbols.len() - idx));
                    break;
                }
                per_cap = Some(remaining);
            }
        }
        let mut args = json!({"symbol": symbol, "limit": limit, "noDedup": no_dedup});
        if let Some(f) = &file {
            args["file"] = json!(f);
        }
        if let Some(mc) = per_cap {
            args["maxChars"] = json!(mc);
        }
        let out = tools::call(&env, tool, &args);
        any_err |= out.is_error;
        meta.merge(&out.meta);
        used += sep + out.text.len();
        parts.push(out.text);
    }
    let joined = parts.join("\n\n---\n\n");
    println!("{joined}");
    save_session(history, sess_file);
    cg_query::telemetry::record(cg_query::telemetry::CallStats {
        mode: "cli",
        tool,
        project: proj.root(),
        duration_ms: t0.elapsed().as_secs_f64() * 1e3,
        sync_checked: sync_stat.checked,
        synced: sync_stat.synced,
        sync_ms: sync_stat.sync_ms,
        generation: gen,
        output_bytes: joined.len(),
        compact: false,
        max_chars,
        dedup_enabled: ctx.session.as_deref().is_some_and(|s| !s.trim().is_empty()) && !no_dedup,
        meta,
        is_error: any_err,
    });
    Ok(if any_err { 1 } else { 0 })
}

pub fn impact(ctx: &QueryCtx, symbols: &[String], file: Option<String>, depth: u32, max_chars: Option<usize>) -> Result<i32> {
    let t0 = Instant::now();
    let (proj, sync_stat) = open_for_query(ctx.base.clone(), ctx.sync)?;
    let status = proj.status_info();
    let gen = status.generation;
    let env = ToolEnv { graph: &proj.graph, history: None, status };
    let mut parts: Vec<String> = Vec::with_capacity(symbols.len());
    let mut any_err = false;
    let mut meta = cg_query::tools::ToolMeta::default();
    let mut used = 0usize;
    for (idx, symbol) in symbols.iter().enumerate() {
        let sep = if idx > 0 { 7 } else { 0 };
        let mut per_cap = max_chars;
        if let Some(total_cap) = max_chars {
            if idx > 0 {
                let remaining = total_cap.saturating_sub(used + sep);
                if remaining < 560 {
                    parts.push(format!("> … +{} more symbol(s) omitted to fit `--max-chars {total_cap}`.", symbols.len() - idx));
                    break;
                }
                per_cap = Some(remaining);
            }
        }
        let mut args = json!({"symbol": symbol, "depth": depth});
        if let Some(f) = &file {
            args["file"] = json!(f);
        }
        if let Some(mc) = per_cap {
            args["maxChars"] = json!(mc);
        }
        let out = tools::call(&env, "impact", &args);
        any_err |= out.is_error;
        meta.merge(&out.meta);
        used += sep + out.text.len();
        parts.push(out.text);
    }
    let joined = parts.join("\n\n---\n\n");
    println!("{joined}");
    cg_query::telemetry::record(cg_query::telemetry::CallStats {
        mode: "cli",
        tool: "impact",
        project: proj.root(),
        duration_ms: t0.elapsed().as_secs_f64() * 1e3,
        sync_checked: sync_stat.checked,
        synced: sync_stat.synced,
        sync_ms: sync_stat.sync_ms,
        generation: gen,
        output_bytes: joined.len(),
        compact: false,
        max_chars,
        dedup_enabled: false,
        meta,
        is_error: any_err,
    });
    Ok(if any_err { 1 } else { 0 })
}

pub fn implementations(ctx: &QueryCtx, symbols: &[String], file: Option<String>, depth: u32, max_chars: Option<usize>) -> Result<i32> {
    let t0 = Instant::now();
    let (proj, sync_stat) = open_for_query(ctx.base.clone(), ctx.sync)?;
    let status = proj.status_info();
    let gen = status.generation;
    let env = ToolEnv { graph: &proj.graph, history: None, status };
    let mut parts: Vec<String> = Vec::with_capacity(symbols.len());
    let mut any_err = false;
    let mut meta = cg_query::tools::ToolMeta::default();
    let mut used = 0usize;
    for (idx, symbol) in symbols.iter().enumerate() {
        let sep = if idx > 0 { 7 } else { 0 };
        let mut per_cap = max_chars;
        if let Some(total_cap) = max_chars {
            if idx > 0 {
                let remaining = total_cap.saturating_sub(used + sep);
                if remaining < 560 {
                    parts.push(format!("> … +{} more symbol(s) omitted to fit `--max-chars {total_cap}`.", symbols.len() - idx));
                    break;
                }
                per_cap = Some(remaining);
            }
        }
        let mut args = json!({"symbol": symbol, "depth": depth});
        if let Some(f) = &file {
            args["file"] = json!(f);
        }
        if let Some(mc) = per_cap {
            args["maxChars"] = json!(mc);
        }
        let out = tools::call(&env, "implementations", &args);
        any_err |= out.is_error;
        meta.merge(&out.meta);
        used += sep + out.text.len();
        parts.push(out.text);
    }
    let joined = parts.join("\n\n---\n\n");
    println!("{joined}");
    cg_query::telemetry::record(cg_query::telemetry::CallStats {
        mode: "cli",
        tool: "implementations",
        project: proj.root(),
        duration_ms: t0.elapsed().as_secs_f64() * 1e3,
        sync_checked: sync_stat.checked,
        synced: sync_stat.synced,
        sync_ms: sync_stat.sync_ms,
        generation: gen,
        output_bytes: joined.len(),
        compact: false,
        max_chars,
        dedup_enabled: false,
        meta,
        is_error: any_err,
    });
    Ok(if any_err { 1 } else { 0 })
}

#[allow(clippy::too_many_arguments)]
pub fn affected(
    ctx: &QueryCtx,
    mut files: Vec<String>,
    stdin: bool,
    depth: u32,
    filter: Option<String>,
    cross_tier: bool,
    json_out: bool,
    quiet: bool,
) -> Result<i32> {
    use std::io::BufRead;
    if stdin {
        for l in std::io::stdin().lock().lines() {
            let l = l?;
            if !l.trim().is_empty() {
                files.push(l.trim().to_owned());
            }
        }
    }
    let (proj, _) = open_for_query(ctx.base.clone(), ctx.sync)?;
    let g = &proj.graph;
    let mut changed: Vec<FileId> = Vec::new();
    for f in &files {
        let rel = match Path::new(f).strip_prefix(proj.root()) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => f.replace('\\', "/").trim_start_matches("./").to_owned(),
        };
        if let Some(id) = g.view.file_by_path(&rel) {
            changed.push(id);
        }
    }
    let graph = g.dependent_files_graph(cross_tier);
    let all = g.affected_files(&graph, &changed, depth);
    let matcher = filter.as_deref().and_then(|p| globset::Glob::new(p).ok()).map(|x| x.compile_matcher());
    let out: Vec<&str> = all
        .iter()
        .filter(|f| match &matcher {
            Some(m) => m.is_match(g.path(**f)),
            None => g.is_test_file(**f),
        })
        .map(|f| g.path(*f))
        .collect();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({"schema": 1, "changed": files, "affected": out}))?);
    } else if quiet {
        for p in &out {
            println!("{p}");
        }
    } else {
        println!("{} affected file(s) from {} changed:", out.len(), changed.len());
        for p in &out {
            println!("  {p}");
        }
    }
    Ok(0)
}

pub fn cycles(path: Option<PathBuf>, sync: SyncMode, json_out: bool) -> Result<i32> {
    let (proj, _) = open_for_query(path, sync)?;
    let g = &proj.graph;
    let fg = g.file_graph();
    let cycles = g.import_cycles(&fg);
    if json_out {
        let arr: Vec<Vec<&str>> = cycles.iter().map(|c| c.iter().map(|f| g.path(*f)).collect()).collect();
        println!("{}", serde_json::to_string_pretty(&json!({"schema": 1, "cycles": arr}))?);
    } else if cycles.is_empty() {
        println!("No import cycles found.");
    } else {
        println!("{} import cycle(s):", cycles.len());
        for (i, c) in cycles.iter().enumerate() {
            println!("{}. {}", i + 1, c.iter().map(|f| g.path(*f)).collect::<Vec<_>>().join(" → "));
        }
    }
    Ok(if cycles.is_empty() { 0 } else { 1 })
}

#[cfg(feature = "sqlite")]
pub fn export_sqlite(base: Option<PathBuf>, out: PathBuf, force: bool) -> Result<i32> {
    let proj = open(base)?;
    let t0 = std::time::Instant::now();
    let st = crate::export::export_sqlite(&proj, &out, force)?;
    println!(
        "exported {} files, {} nodes, {} edges, {} references and {} imports to {} ({:.1} MB, {:.2}s)",
        st.files,
        st.nodes,
        st.edges,
        st.refs,
        st.imports,
        out.display(),
        st.bytes as f64 / 1048576.0,
        t0.elapsed().as_secs_f64()
    );
    Ok(0)
}

#[cfg(not(feature = "sqlite"))]
pub fn export_sqlite(_base: Option<PathBuf>, _out: PathBuf, _force: bool) -> Result<i32> {
    Err(anyhow!("this build of {} was compiled without the `sqlite` feature", brand::BIN_NAME))
}

pub fn version(verbose: bool) -> Result<i32> {
    println!("{} {}", brand::BIN_NAME, env!("CARGO_PKG_VERSION"));
    if verbose {
        println!("project:        {}", brand::PROJECT_NAME);
        println!("target:         {}-{}", std::env::consts::ARCH, std::env::consts::OS);
        println!("engine version: {}", brand::ENGINE_VERSION);
        println!("format version: {}", brand::FORMAT_VERSION);
        println!("upstream kernel commit: {}", brand::UPSTREAM_KERNEL_COMMIT.trim());
        println!("languages:      typescript, javascript, vue, svelte, python, go, java, rust, kotlin, c, cpp, csharp, php, ruby, swift");
    }
    Ok(0)
}

pub fn debug_dump(base: Option<PathBuf>) -> Result<i32> {
    let proj = open(base)?;
    for l in dump::dump_edges(&proj.snap.view) {
        println!("{l}");
    }
    Ok(0)
}

pub fn debug_unresolved(base: Option<PathBuf>) -> Result<i32> {
    let proj = open(base)?;
    for l in dump::dump_unresolved(&proj.snap.view) {
        println!("{l}");
    }
    Ok(0)
}

/// Compare the stored index of the project at `root` with a fresh full rebuild of the same
/// tree. Returns true when they are identical. The report goes to stderr when `to_stderr`
/// (callers whose stdout is machine-readable), else to stdout.
pub fn debug_diff_inner(root: &Path, limit: usize, to_stderr: bool) -> Result<bool> {
    use std::collections::BTreeSet;
    let say = |line: String| {
        if to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    let proj = Project::locate(root).map_err(|e| anyhow!("{e}"))?;
    let stored: Vec<String> = dump::canonical(&proj.snap.view);
    let built = cg_resolve::index::build_seg(proj.root(), &Default::default())?;
    let fresh_view = cg_store::View::new(std::sync::Arc::new(built.seg)).with_root(proj.root().to_path_buf());
    let fresh: Vec<String> = dump::canonical(&fresh_view);
    if stored == fresh {
        say(format!("identical: {} canonical lines", stored.len()));
        return Ok(true);
    }
    let (a, b): (BTreeSet<&String>, BTreeSet<&String>) = (stored.iter().collect(), fresh.iter().collect());
    say(format!("DIFFERENT: {} stored lines vs {} rebuilt lines", stored.len(), fresh.len()));
    say("-- only in the stored index:".to_owned());
    for l in a.difference(&b).take(limit) {
        say(format!("  {l}"));
    }
    say("-- only in the rebuild:".to_owned());
    for l in b.difference(&a).take(limit) {
        say(format!("  {l}"));
    }
    Ok(false)
}

pub fn debug_diff(path: Option<PathBuf>, limit: usize) -> Result<i32> {
    let start = start_dir(path)?;
    Ok(if debug_diff_inner(&start, limit, false)? { 0 } else { 1 })
}

#[allow(dead_code)]
fn unused(_: NodeIdx) {}

fn env_truthy(name: &str, fallback: Option<&str>) -> bool {
    brand::env::get(name, fallback).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn env_falsy(name: &str) -> bool {
    brand::env::get(name, None).is_some_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
}

pub fn serve(base: Option<PathBuf>, http: Option<String>, token: Option<String>, no_watch: bool, dedup_flag: bool, no_auto_sync: bool) -> Result<i32> {
    // stdout belongs to the protocol: all logging goes to stderr
    let filter = std::env::var(brand::env::LOG).unwrap_or_else(|_| "warn".into());
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(tracing_subscriber::EnvFilter::new(filter)).try_init();
    let mut dedup = dedup_flag || env_truthy(brand::env::DEDUP, None) || env_truthy(brand::env::EXPLORE_DEDUP, Some(brand::env::EXPLORE_DEDUP_UPSTREAM));
    if http.is_some() && dedup {
        eprintln!("warning: session deduplication (--dedup) is disabled over --http because multiple clients share one server instance");
        dedup = false;
    }
    let auto_sync = !no_auto_sync && !env_falsy(brand::env::AUTO_SYNC);
    let cfg_path = base.map(|p| p.canonicalize().unwrap_or(p));
    let cfg = cg_mcp::Config { path: cfg_path.clone(), tools: cg_mcp::defs::allowlist_from_env(), dedup, auto_sync };
    let server = cg_mcp::Server::new(cfg);
    server.set_rebuild_hook(std::sync::Arc::new(|root: &Path| {
        let _ = cg_resolve::index_project(root, &cg_resolve::IndexOptions { low_priority: true, ..Default::default() });
    }));
    server.set_status_hook(std::sync::Arc::new(|p: &Project| cg_sync::measure_changes(p.root(), &p.snap.view).ok().map(|c| c.describe())));
    server.set_sync_hook(std::sync::Arc::new(|root: &Path, explicit: bool| {
        let t = Instant::now();
        let timeout = if explicit { Duration::from_secs(5) } else { Duration::from_millis(250) };
        let opts = cg_sync::SyncOptions { lock_timeout: Some(timeout), ..Default::default() };
        match cg_sync::sync(root, &opts) {
            Ok(rep) => cg_mcp::SyncHookResult { synced: !rep.up_to_date, sync_ms: t.elapsed().as_secs_f64() * 1e3, skipped: false },
            Err(e) => {
                if explicit {
                    tracing::warn!("explicit MCP sync skipped for {}: {e}", root.display());
                }
                cg_mcp::SyncHookResult { synced: false, sync_ms: t.elapsed().as_secs_f64() * 1e3, skipped: true }
            }
        }
    }));
    let watchers: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<PathBuf, cg_sync::WatchHandle>>> = Default::default();
    if !no_watch {
        let w = watchers.clone();
        server.set_open_hook(std::sync::Arc::new(move |root: &Path| {
            let mut map = w.lock().unwrap_or_else(|e| e.into_inner());
            if !map.contains_key(root) {
                let bg = cg_sync::SyncOptions { index: cg_resolve::IndexOptions { low_priority: true, ..Default::default() }, ..Default::default() };
                map.insert(root.to_path_buf(), cg_sync::spawn_watcher(root.to_path_buf(), bg));
            }
        }));
        let w = watchers.clone();
        server.set_close_hook(std::sync::Arc::new(move |root: &Path| {
            // Dropping requests a stop without holding the project-cache lock or delaying
            // the next query while an index write finishes.
            w.lock().unwrap_or_else(|e| e.into_inner()).remove(root);
        }));
        let w = watchers.clone();
        server.on_shutdown(Box::new(move || {
            let handles: Vec<_> = w.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, h)| h).collect();
            cg_sync::WatchHandle::stop_all(handles, std::time::Duration::from_millis(600));
        }));
        // watch the default project from the start
        let start = std::env::var_os("CGR_WATCH_ROOT").map(PathBuf::from).or_else(|| cfg_path.clone()).or_else(|| std::env::current_dir().ok());
        if let Some(start) = start {
            server.preopen(&start);
        }
    }
    match http {
        Some(addr) => {
            let token = token.or_else(|| std::env::var("CGR_HTTP_TOKEN").ok()).filter(|t| !t.is_empty());
            eprintln!("{} MCP over HTTP on http://{addr}/mcp", brand::PROJECT_NAME);
            cg_mcp::serve_http(server, cg_mcp::HttpConfig { addr, token, endpoint: "/mcp".into() }, None)?;
        }
        None => cg_mcp::serve_stdio(server)?,
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    base: Option<PathBuf>,
    agents: Vec<String>,
    local: bool,
    hook: bool,
    permissions: bool,
    dry_run: bool,
    bin: Option<PathBuf>,
    installing: bool,
) -> Result<i32> {
    use cg_install::{targets, Action, Env, Request};
    let env = Env::from_process();
    let project = match base {
        Some(p) => p.canonicalize().unwrap_or(p),
        None => env.cwd.clone(),
    };
    let bin = match bin {
        Some(b) => b,
        None => std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .ok_or_else(|| anyhow!("cannot determine the path of this executable; pass --bin"))?,
    };
    let req = Request { bin, local, project, hook, permissions, dry_run };
    let all = targets();
    let chosen: Vec<_> = if agents.is_empty() {
        all.iter().filter(|t| t.detect(&env)).collect()
    } else {
        let mut v = Vec::new();
        for a in &agents {
            match all.iter().find(|t| t.id() == a.as_str()) {
                Some(t) => v.push(t),
                None => return Err(anyhow!("unknown agent `{a}` (known: {})", all.iter().map(|t| t.id()).collect::<Vec<_>>().join(", "))),
            }
        }
        v
    };
    if chosen.is_empty() {
        println!("No supported agent found on this machine. Name one with --agent ({}).", all.iter().map(|t| t.id()).collect::<Vec<_>>().join(", "));
        return Ok(1);
    }
    for t in chosen {
        let changes = if installing { t.install(&env, &req) } else { t.uninstall(&env, &req) };
        match changes {
            Ok(changes) => {
                for c in changes {
                    let verb = match c.action {
                        Action::Created => "created",
                        Action::Updated => "updated",
                        Action::Unchanged => "unchanged",
                        Action::Removed => "removed",
                    };
                    println!("{}: {verb} {} ({}){}", t.display(), c.path.display(), c.what, if dry_run { "  [dry run]" } else { "" });
                }
            }
            Err(e) => {
                eprintln!("{}: {e}", t.display());
                return Ok(1);
            }
        }
    }
    if installing && !dry_run {
        println!("Restart the agent to pick up the change. Index a project with `{} init`.", brand::BIN_NAME);
    }
    Ok(0)
}

pub fn uninit(path: Option<PathBuf>, yes: bool) -> Result<i32> {
    use std::io::{BufRead, IsTerminal, Write};
    let start = start_dir(path)?;
    let store = cg_store::Store::locate(&start).ok_or_else(|| anyhow!("no {} index found from {}", brand::DATA_DIR, start.display()))?;
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err(anyhow!("refusing to delete {} without --yes (stdin is not a terminal)", store.dir.display()));
        }
        print!("Delete {} ? [y/N] ", store.dir.display());
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(1);
        }
    }
    let _guard = store.write_lock(std::time::Duration::from_secs(5)).ok();
    std::fs::remove_dir_all(&store.dir).with_context(|| format!("cannot remove {}", store.dir.display()))?;
    let exclude = store.root.join(".git/info/exclude");
    if let Ok(cur) = std::fs::read_to_string(&exclude) {
        let line = format!("{}/", brand::DATA_DIR);
        if cur.lines().any(|l| l.trim() == line) {
            let kept: Vec<&str> = cur.lines().filter(|l| l.trim() != line).collect();
            let mut out = kept.join("\n");
            if cur.ends_with('\n') {
                out.push('\n');
            }
            std::fs::write(&exclude, out)?;
        }
    }
    println!("Removed {}.", store.dir.display());
    Ok(0)
}
