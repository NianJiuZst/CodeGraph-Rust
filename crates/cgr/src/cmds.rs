//! Command implementations. Query-side commands go through the same `cg_query::tools`
//! handlers the MCP server uses, so the CLI and the agent see identical text.

use anyhow::{anyhow, Context, Result};
use cg_core::{brand, FileId, NodeIdx};
use cg_query::tools::{self, ToolEnv};
use cg_query::Project;
use cg_store::dump;
use serde_json::json;
use std::path::{Path, PathBuf};

fn start_dir(p: Option<PathBuf>) -> Result<PathBuf> {
    let p = p.unwrap_or_else(|| PathBuf::from("."));
    p.canonicalize().with_context(|| format!("cannot open {}", p.display()))
}

fn open(p: Option<PathBuf>) -> Result<Project> {
    let start = start_dir(p)?;
    Project::locate(&start).map_err(|e| anyhow!("{e} (looked above {}; run `{} init` first)", start.display(), brand::BIN_NAME))
}

fn call(p: Option<PathBuf>, tool: &str, args: serde_json::Value) -> Result<i32> {
    let proj = open(p)?;
    let env = ToolEnv { graph: &proj.graph, history: None, status: proj.status_info() };
    let out = tools::call(&env, tool, &args);
    println!("{}", out.text);
    Ok(if out.is_error { 1 } else { 0 })
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
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": 1,
                "root": proj.root().display().to_string(),
                "files": info.files, "nodes": info.nodes, "edges": info.edges,
                "generation": m.generation, "engineVersion": m.engine_version, "engineOutdated": info.engine_outdated,
                "bytes": info.bytes, "updatedAt": info.built_at, "deltaLayer": m.delta.is_some(),
                "verified": verify,
                "changes": cg_sync::measure_changes(proj.root(), &proj.snap.view).ok().map(|c| json!({"added": c.added, "modified": c.modified, "removed": c.removed})),
            }))?
        );
    } else {
        println!("files {}  nodes {}  edges {}  generation {}", m.n_files, m.n_nodes, m.n_edges, m.generation);
        for (name, n) in dump::ref_stats(&proj.snap.view) {
            println!("  refs {name:<12} {n}");
        }
        println!("  size {:.2} MB  updated {}", info.bytes as f64 / 1048576.0, info.built_at);
        if info.engine_outdated {
            println!("  note: index built by an older engine; run `{} index` to rebuild", brand::BIN_NAME);
        }
    }
    let changes = cg_sync::measure_changes(proj.root(), &proj.snap.view).ok();
    if !json {
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

pub fn query(base: Option<PathBuf>, search: &str, kind: Option<String>, limit: usize, json_out: bool) -> Result<i32> {
    if !json_out {
        let mut args = json!({"query": search, "limit": limit});
        if let Some(k) = kind {
            args["kind"] = json!(k);
        }
        return call(base, "search", args);
    }
    let proj = open(base)?;
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
    println!("{}", serde_json::to_string_pretty(&json!({"schema": 1, "results": arr}))?);
    Ok(0)
}

pub fn explore(base: Option<PathBuf>, query: &str, max_files: Option<usize>) -> Result<i32> {
    let mut args = json!({"query": query});
    if let Some(m) = max_files {
        args["maxFiles"] = json!(m);
    }
    call(base, "explore", args)
}

pub fn node(
    base: Option<PathBuf>,
    target: &str,
    code: bool,
    file: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
    symbols_only: bool,
) -> Result<i32> {
    let proj = open(base.clone())?;
    // A name that resolves to a symbol is a symbol; otherwise treat the argument as a file.
    let is_symbol = !proj.graph.resolve_symbol(target).is_empty();
    let mut args = json!({"includeCode": code});
    if is_symbol {
        args["symbol"] = json!(target);
        if let Some(f) = file {
            args["file"] = json!(f);
        }
    } else {
        args["file"] = json!(target);
        if let Some(o) = offset {
            args["offset"] = json!(o);
        }
        if let Some(l) = limit {
            args["limit"] = json!(l);
        }
        args["symbolsOnly"] = json!(symbols_only);
    }
    drop(proj);
    call(base, "node", args)
}

pub fn files(base: Option<PathBuf>, dir: Option<String>, pattern: Option<String>, format: &str, meta: bool, max_depth: Option<usize>) -> Result<i32> {
    let mut args = json!({"format": format, "includeMetadata": meta});
    if let Some(d) = dir {
        args["path"] = json!(d);
    }
    if let Some(p) = pattern {
        args["pattern"] = json!(p);
    }
    if let Some(m) = max_depth {
        args["maxDepth"] = json!(m);
    }
    call(base, "files", args)
}

pub fn callers(base: Option<PathBuf>, symbol: &str, file: Option<String>, limit: usize, callers: bool) -> Result<i32> {
    let mut args = json!({"symbol": symbol, "limit": limit});
    if let Some(f) = file {
        args["file"] = json!(f);
    }
    call(base, if callers { "callers" } else { "callees" }, args)
}

pub fn impact(base: Option<PathBuf>, symbol: &str, file: Option<String>, depth: u32) -> Result<i32> {
    let mut args = json!({"symbol": symbol, "depth": depth});
    if let Some(f) = file {
        args["file"] = json!(f);
    }
    call(base, "impact", args)
}

pub fn implementations(base: Option<PathBuf>, symbol: &str) -> Result<i32> {
    let proj = open(base)?;
    let g = &proj.graph;
    let nodes = g.resolve_symbol(symbol);
    if nodes.is_empty() {
        println!("Symbol \"{symbol}\" not found in the codebase");
        return Ok(0);
    }
    let mut total = 0;
    for n in nodes {
        let impls = g.implementations(n);
        println!("**Implementations of {} ({}:{}) — {} found**", g.qname(n), g.node_path(n), g.line(n), impls.len());
        for i in &impls {
            println!("- {} ({}) - {}:{}", g.name(*i), g.kind(*i), g.node_path(*i), g.line(*i));
        }
        total += impls.len();
    }
    let _ = total;
    Ok(0)
}

pub fn affected(base: Option<PathBuf>, mut files: Vec<String>, stdin: bool, depth: u32, filter: Option<String>, json_out: bool, quiet: bool) -> Result<i32> {
    use std::io::BufRead;
    if stdin {
        for l in std::io::stdin().lock().lines() {
            let l = l?;
            if !l.trim().is_empty() {
                files.push(l.trim().to_owned());
            }
        }
    }
    let proj = open(base)?;
    let g = &proj.graph;
    let mut changed: Vec<FileId> = Vec::new();
    for f in &files {
        let rel = match Path::new(f).strip_prefix(proj.root()) {
            Ok(r) => r.to_string_lossy().into_owned(),
            Err(_) => f.trim_start_matches("./").to_owned(),
        };
        if let Some(id) = g.view.file_by_path(&rel) {
            changed.push(id);
        }
    }
    let graph = g.file_graph();
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

pub fn cycles(path: Option<PathBuf>, json_out: bool) -> Result<i32> {
    let proj = open(path)?;
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
        println!("languages:      typescript, javascript, python, go, java, rust, kotlin, c, cpp, csharp, php, ruby, swift");
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

pub fn serve(base: Option<PathBuf>, http: Option<String>, token: Option<String>, no_watch: bool) -> Result<i32> {
    // stdout belongs to the protocol: all logging goes to stderr
    let filter = std::env::var(brand::env::LOG).unwrap_or_else(|_| "warn".into());
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(tracing_subscriber::EnvFilter::new(filter)).try_init();
    let dedup = brand::env::get("CGR_EXPLORE_DEDUP", Some("CODEGRAPH_EXPLORE_DEDUP")).is_some_and(|v| v == "1");
    let cfg_path = base.map(|p| p.canonicalize().unwrap_or(p));
    let cfg = cg_mcp::Config { path: cfg_path.clone(), tools: cg_mcp::defs::allowlist_from_env(), dedup };
    let server = cg_mcp::Server::new(cfg);
    server.set_rebuild_hook(std::sync::Arc::new(|root: &Path| {
        let _ = cg_resolve::index_project(root, &cg_resolve::IndexOptions { low_priority: true, ..Default::default() });
    }));
    server.set_status_hook(std::sync::Arc::new(|p: &Project| cg_sync::measure_changes(p.root(), &p.snap.view).ok().map(|c| c.describe())));
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
