//! MCP server core (design doc 10): JSON-RPC dispatch, project location and snapshot caching,
//! a blocking pool for tool calls, per-call timeout, success-shaped errors.

use crate::defs;
use cg_core::brand;
use cg_core::cancel::{Cancellation, Cancelled};
use cg_query::explore::History;
use cg_query::strings;
use cg_query::tools::{self, StatusInfo, ToolEnv, ToolOutput};
use cg_query::Project;
use cg_store::Store;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const POOL_SIZE: usize = 4;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROJECTS: usize = 8;

/// Where responses and server-initiated requests go (stdout, or one HTTP response).
pub trait Reply: Send + Sync {
    fn send(&self, msg: Value);
}

#[derive(Clone, Default)]
pub struct Config {
    /// `--path`: default project directory when the client sends no roots.
    pub path: Option<PathBuf>,
    /// Short tool names enabled in addition to / instead of the default surface.
    pub tools: Option<Vec<String>>,
    pub dedup: bool,
    /// Run the registered pre-query incremental sync hook before answering queries so edits
    /// are immediately reflected without waiting for background watcher debounce.
    pub auto_sync: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncHookResult {
    pub synced: bool,
    pub sync_ms: f64,
    pub skipped: bool,
}

type StatusHook = Arc<dyn Fn(&Project) -> Option<String> + Send + Sync>;
type RebuildHook = Arc<dyn Fn(&Path) + Send + Sync>;
type OpenHook = Arc<dyn Fn(&Path) + Send + Sync>;
type SyncHook = Arc<dyn Fn(&Path, bool) -> SyncHookResult + Send + Sync>;

struct Entry {
    project: Arc<Project>,
    history: Arc<Mutex<History>>,
    noticed: Arc<AtomicBool>,
    last_used: u64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<PathBuf, Entry>,
    tick: u64,
}

#[derive(Default)]
struct RootsState {
    /// A `roots/list` request is outstanding.
    pending: bool,
    roots: Vec<PathBuf>,
}

type Job = Box<dyn FnOnce() + Send>;

/// A project with its session history and its "stale" flag, as handed to a tool call.
type ProjectHandles = (Arc<Project>, Arc<Mutex<History>>, Arc<AtomicBool>);

pub struct Server {
    cfg: Config,
    cache: Mutex<Cache>,
    project_lifecycle: Mutex<()>,
    roots: Mutex<RootsState>,
    roots_cv: Condvar,
    client_roots: AtomicBool,
    protocol: Mutex<String>,
    pool_tx: Mutex<Option<mpsc::SyncSender<Job>>>,
    requests: Mutex<HashMap<String, Cancellation>>,
    outbound: OnceLock<Arc<dyn Reply>>,
    shutdown_flag: AtomicBool,
    shutdown_hooks: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    status_hook: Mutex<Option<StatusHook>>,
    rebuild_hook: Mutex<Option<RebuildHook>>,
    rebuilding: Arc<Mutex<Option<PathBuf>>>,
    open_hook: Mutex<Option<OpenHook>>,
    close_hook: Mutex<Option<OpenHook>>,
    sync_hook: Mutex<Option<SyncHook>>,
    next_req: AtomicU64,
    /// Tool calls that were accepted and have not been answered yet (see [`Server::drain`]).
    inflight: (Mutex<usize>, Condvar),
    pub started: Instant,
}

/// Counts one accepted tool call until its answer has been written.
struct InFlight(Arc<Server>);

impl InFlight {
    fn enter(server: &Arc<Server>) -> InFlight {
        *server.inflight.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        InFlight(Arc::clone(server))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut n = self.0.inflight.0.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        self.0.inflight.1.notify_all();
    }
}

fn tool_result(o: ToolOutput) -> Value {
    let mut v = json!({"content": [{"type": "text", "text": o.text}]});
    if o.is_error {
        v["isError"] = json!(true);
    }
    v
}

/// `file:///a/b%20c` -> `/a/b c`
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8(out).ok()?;
    // Windows drive paths come as /C:/...
    let s = if s.len() > 2 && s.as_bytes()[0] == b'/' && s.as_bytes()[2] == b':' { s[1..].to_owned() } else { s };
    Some(PathBuf::from(s))
}

impl Server {
    pub fn new(cfg: Config) -> Arc<Server> {
        let (tx, rx) = mpsc::sync_channel::<Job>(POOL_SIZE * 4);
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..POOL_SIZE {
            let rx = Arc::clone(&rx);
            let _ = std::thread::Builder::new().name(format!("cgr-mcp-{i}")).stack_size(8 << 20).spawn(move || loop {
                let job = { rx.lock().unwrap_or_else(|e| e.into_inner()).recv() };
                match job {
                    Ok(j) => j(),
                    Err(_) => break,
                }
            });
        }
        Arc::new(Server {
            cfg,
            cache: Mutex::new(Cache::default()),
            project_lifecycle: Mutex::new(()),
            roots: Mutex::new(RootsState::default()),
            roots_cv: Condvar::new(),
            client_roots: AtomicBool::new(false),
            protocol: Mutex::new(SUPPORTED_PROTOCOLS[0].to_owned()),
            pool_tx: Mutex::new(Some(tx)),
            requests: Mutex::new(HashMap::new()),
            inflight: (Mutex::new(0), Condvar::new()),
            outbound: OnceLock::new(),
            shutdown_flag: AtomicBool::new(false),
            shutdown_hooks: Mutex::new(Vec::new()),
            status_hook: Mutex::new(None),
            rebuild_hook: Mutex::new(None),
            rebuilding: Arc::new(Mutex::new(None)),
            open_hook: Mutex::new(None),
            close_hook: Mutex::new(None),
            sync_hook: Mutex::new(None),
            next_req: AtomicU64::new(1),
            started: Instant::now(),
        })
    }

    pub fn set_outbound(&self, r: Arc<dyn Reply>) {
        let _ = self.outbound.set(r);
    }

    pub fn set_status_hook(&self, h: StatusHook) {
        *self.status_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }

    pub fn set_rebuild_hook(&self, h: RebuildHook) {
        *self.rebuild_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }

    /// Called with the project root the first time a project is opened (starts its watcher).
    pub fn set_open_hook(&self, h: OpenHook) {
        *self.open_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }

    /// Release per-project resources when its cached snapshot is evicted.
    pub fn set_close_hook(&self, h: OpenHook) {
        *self.close_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }

    /// Optional fast incremental sync hook run before queries when `auto_sync` or `sync: true` is active.
    pub fn set_sync_hook(&self, h: SyncHook) {
        *self.sync_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }

    /// Open the project at `start` ahead of the first tool call (so it is watched from the start).
    pub fn preopen(&self, start: &Path) {
        let _ = self.project_for(Some(&start.to_string_lossy()));
    }

    pub fn on_shutdown(&self, f: Box<dyn FnOnce() + Send>) {
        self.shutdown_hooks.lock().unwrap_or_else(|e| e.into_inner()).push(f);
    }

    /// Wait until every accepted tool call has been answered, or `max` has passed. Used when the
    /// client closes stdin: its last requests still get their answers.
    pub fn drain(&self, max: Duration) {
        let deadline = Instant::now() + max;
        let mut n = self.inflight.0.lock().unwrap_or_else(|e| e.into_inner());
        while *n > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            n = self.inflight.1.wait_timeout(n, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown_flag.load(Ordering::SeqCst)
    }

    /// Stop accepting work and run shutdown hooks (stop the watcher, release locks).
    pub fn shutdown(&self) {
        if self.shutdown_flag.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.pool_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
        for token in self.requests.lock().unwrap_or_else(|e| e.into_inner()).values() {
            token.cancel();
        }
        let hooks: Vec<_> = self.shutdown_hooks.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        for h in hooks {
            h();
        }
    }

    fn submit(&self, job: Job) -> bool {
        match self.pool_tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(tx) => tx.try_send(job).is_ok(),
            None => false,
        }
    }

    // ------------------------------------------------------------------ roots
    fn default_root(&self) -> PathBuf {
        // Wait briefly for an outstanding roots/list answer so the first call sees the roots.
        {
            let mut r = self.roots.lock().unwrap_or_else(|e| e.into_inner());
            let deadline = Instant::now() + Duration::from_millis(1500);
            while r.pending {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let (g, _) = self.roots_cv.wait_timeout(r, left).unwrap_or_else(|e| e.into_inner());
                r = g;
            }
            if let Some(first) = r.roots.first() {
                return first.clone();
            }
        }
        self.cfg.path.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("."))
    }

    fn request_roots(&self) {
        if !self.client_roots.load(Ordering::SeqCst) {
            return;
        }
        let Some(out) = self.outbound.get() else { return };
        let id = format!("cgr-roots-{}", self.next_req.fetch_add(1, Ordering::SeqCst));
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).pending = true;
        out.send(json!({"jsonrpc": "2.0", "id": id, "method": "roots/list"}));
    }

    fn on_roots_response(&self, msg: &Value) {
        let mut r = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        r.pending = false;
        if let Some(list) = msg.pointer("/result/roots").and_then(|v| v.as_array()) {
            r.roots = list.iter().filter_map(|x| x.get("uri").and_then(|u| u.as_str())).filter_map(file_uri_to_path).collect();
        }
        self.roots_cv.notify_all();
    }

    // --------------------------------------------------------------- projects
    fn guidance(&self, start: &Path, explicit: bool) -> ToolOutput {
        let init = format!("{} init", brand::BIN_NAME);
        let text = if explicit {
            format!(
                "No {dir}/ index found at or above `{p}`. Indexing is the user's decision — if it comes up, they can run `{init}` in that project. Until then use your built-in tools (Read/Grep/Glob) for it. If this is a monorepo, pass `projectPath` for a sub-project that has its own {dir}/ index.",
                dir = brand::DATA_DIR,
                p = start.display()
            )
        } else {
            format!(
                "This project (`{p}`) has no {dir}/ index, so codegraph-rust has nothing to answer from. Indexing is the user's decision — if it comes up, they can run `{init}` in the project; don't run it yourself. Use your built-in tools (Read/Grep/Glob) here. In a monorepo, pass `projectPath` for a sub-project that has its own {dir}/ index.",
                dir = brand::DATA_DIR,
                p = start.display()
            )
        };
        ToolOutput::ok(text)
    }

    fn locate_store(start: &Path) -> Option<Store> {
        let normalized = cg_core::path::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
        Store::locate(&normalized).map(|s| {
            let root = cg_core::path::canonicalize(&s.root).unwrap_or(s.root);
            Store::at(&root)
        })
    }

    /// Open (or refresh) the project for a tool call.
    fn project_for(&self, project_path: Option<&str>) -> Result<ProjectHandles, ToolOutput> {
        let default = self.default_root();
        let (start, explicit) = match project_path.map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) => {
                let pb = PathBuf::from(p);
                (if pb.is_absolute() { pb } else { default.join(pb) }, true)
            }
            None => (default, false),
        };
        let Some(store) = Self::locate_store(&start) else { return Err(self.guidance(&start, explicit)) };
        let root = store.root.clone();
        // Keep open/evict callbacks in cache mutation order. Otherwise a later eviction
        // could stop a watcher before an earlier open callback has actually started it.
        let _lifecycle = self.project_lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.tick += 1;
        let tick = cache.tick;
        let gen = store.generation();
        if let Some(e) = cache.map.get_mut(&root) {
            if e.project.generation() == gen || gen == 0 {
                e.last_used = tick;
                return Ok((e.project.clone(), e.history.clone(), e.noticed.clone()));
            }
        }
        let first_open = !cache.map.contains_key(&root);
        match Project::open(store) {
            Ok(p) => {
                let p = Arc::new(p);
                let (history, noticed) = match cache.map.get(&root) {
                    Some(old) => (old.history.clone(), old.noticed.clone()),
                    None => (Arc::new(Mutex::new(History::new())), Arc::new(AtomicBool::new(false))),
                };
                cache.map.insert(root.clone(), Entry { project: p.clone(), history: history.clone(), noticed: noticed.clone(), last_used: tick });
                let mut evicted = None;
                if cache.map.len() > MAX_PROJECTS {
                    if let Some(oldest) = cache.map.iter().min_by_key(|(_, e)| e.last_used).map(|(k, _)| k.clone()) {
                        cache.map.remove(&oldest);
                        evicted = Some(oldest);
                    }
                }
                drop(cache);
                if let Some(root) = evicted {
                    if let Some(close) = self.close_hook.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                        close(&root);
                    }
                }
                if first_open {
                    if let Some(h) = self.open_hook.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                        h(&root);
                    }
                }
                Ok((p, history, noticed))
            }
            Err(e) => {
                let had_project = cache.map.remove(&root).is_some();
                drop(cache);
                if had_project {
                    if let Some(close) = self.close_hook.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                        close(&root);
                    }
                }
                let rebuilding = self.request_rebuild(&root);
                let note = if rebuilding {
                    "It is being rebuilt in the background; retry in a few seconds."
                } else {
                    "A rebuild is needed; the background rebuild worker may be busy with another project."
                };
                Err(ToolOutput::err(format!("The index could not be opened ({e}). {note} If this persists the user can run `{} index`.", brand::BIN_NAME)))
            }
        }
    }

    /// Admit one background build for repeated old-format / corrupt-index requests.
    fn request_rebuild(&self, root: &Path) -> bool {
        if self.is_shutdown() {
            return false;
        }
        let mut slot = self.rebuilding.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = slot.as_ref() {
            return active == root;
        }
        let Some(hook) = self.rebuild_hook.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return false;
        };
        *slot = Some(root.to_path_buf());
        let state = self.rebuilding.clone();
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| hook(&root)));
            *state.lock().unwrap_or_else(|e| e.into_inner()) = None;
        });
        true
    }

    fn clear_projects(&self) {
        let _lifecycle = self.project_lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let roots: Vec<_> = self.cache.lock().unwrap_or_else(|e| e.into_inner()).map.drain().map(|(root, _)| root).collect();
        let close = self.close_hook.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(close) = close {
            for root in roots {
                close(&root);
            }
        }
    }

    fn run_tool(&self, name: &str, args: &Value) -> ToolOutput {
        let t0 = Instant::now();
        let short = name.strip_prefix(brand::MCP_TOOL_PREFIX).or_else(|| name.strip_prefix("codegraph_")).unwrap_or(name);
        if !defs::ALL_TOOLS.contains(&short) || !defs::is_enabled(short, &self.cfg.tools) {
            return ToolOutput::err(format!("Unknown tool: {name}"));
        }
        let project_path = args.get("projectPath").and_then(|v| v.as_str());
        let explicit_sync = args.get("sync").and_then(|v| v.as_bool());
        let want_sync = match explicit_sync {
            Some(explicit) => explicit,
            None => self.cfg.auto_sync && short != "status",
        };
        let is_explicit_sync = explicit_sync == Some(true);
        let mut sync_checked = false;
        let mut sync_res = SyncHookResult::default();
        if want_sync {
            // Drop the mutex guard before running disk I/O so concurrent projects do not serialize here.
            let hook = self.sync_hook.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(hook) = hook {
                let default = self.default_root();
                let start = match project_path.map(str::trim).filter(|s| !s.is_empty()) {
                    Some(p) => {
                        let pb = PathBuf::from(p);
                        if pb.is_absolute() {
                            pb
                        } else {
                            default.join(pb)
                        }
                    }
                    None => default,
                };
                if let Some(store) = Self::locate_store(&start) {
                    sync_checked = true;
                    sync_res = hook(&store.root, is_explicit_sync);
                }
            }
        }
        let (project, history, noticed) = match self.project_for(project_path) {
            Ok(x) => x,
            Err(o) => return o,
        };
        let mut status: StatusInfo = project.status_info();
        if short == "status" {
            let hook = self.status_hook.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(h) = hook {
                status.changes = h(&project);
            }
        }
        let gen = status.generation;
        let dedup_enabled = self.cfg.dedup && !args.get("noDedup").and_then(|v| v.as_bool()).unwrap_or(false);
        let env = ToolEnv { graph: &project.graph, history: if self.cfg.dedup { Some(history) } else { None }, status };
        let mut out = tools::call(&env, short, args);
        if is_explicit_sync && sync_res.skipped && !out.is_error {
            out.text = format!("> Note: explicit `sync: true` could not acquire the index lock in time; answering from the existing snapshot.\n\n{}", out.text);
        }
        if !out.is_error && project.snap.manifest.engine_version != brand::ENGINE_VERSION && !noticed.swap(true, Ordering::SeqCst) {
            out.text = format!(
                "> Note: this index was built by an older {} engine. It still answers; `{} index` rebuilds it with the current resolver.\n\n{}",
                brand::PROJECT_NAME,
                brand::BIN_NAME,
                out.text
            );
        }
        cg_query::telemetry::record(cg_query::telemetry::CallStats {
            mode: "mcp",
            tool: short,
            project: project.root(),
            duration_ms: t0.elapsed().as_secs_f64() * 1e3,
            sync_checked,
            synced: sync_res.synced,
            sync_ms: sync_res.sync_ms,
            generation: gen,
            output_bytes: out.text.len(),
            compact: args.get("compact").and_then(|v| v.as_bool()).unwrap_or(false) || args.get("symbolsOnly").and_then(|v| v.as_bool()).unwrap_or(false),
            max_chars: args.get("maxChars").and_then(|v| v.as_u64()).map(|n| n as usize),
            dedup_enabled,
            meta: out.meta.clone(),
            is_error: out.is_error,
        });
        out
    }

    // --------------------------------------------------------------- dispatch
    fn instructions(&self) -> String {
        let root = self.default_root();
        if Self::locate_store(&root).is_some() {
            strings::server_instructions()
        } else {
            strings::server_instructions_no_root_index()
        }
    }

    /// Handle one JSON-RPC message; responses go to `reply`.
    pub fn handle(self: &Arc<Self>, msg: Value, reply: Arc<dyn Reply>) {
        let method = msg.get("method").and_then(|m| m.as_str()).map(str::to_owned);
        let id = msg.get("id").cloned();
        if method.as_deref() == Some("notifications/cancelled") && id.is_none() {
            if let Some(id) = msg.pointer("/params/requestId") {
                if let Some(token) = self.requests.lock().unwrap_or_else(|e| e.into_inner()).get(&id.to_string()) {
                    token.cancel();
                }
            }
            return;
        }
        match (method, id) {
            (Some(m), Some(id)) => self.handle_request(&m, id, msg.get("params").cloned().unwrap_or(Value::Null), reply),
            (Some(m), None) => self.handle_notification(&m),
            (None, Some(id)) => {
                if id.as_str().is_some_and(|s| s.starts_with("cgr-roots-")) {
                    self.on_roots_response(&msg);
                }
            }
            (None, None) => {}
        }
    }

    fn handle_notification(self: &Arc<Self>, method: &str) {
        match method {
            "notifications/initialized" | "notifications/roots/list_changed" => self.request_roots(),
            _ => {}
        }
    }

    fn handle_request(self: &Arc<Self>, method: &str, id: Value, params: Value, reply: Arc<dyn Reply>) {
        let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id.clone(), "result": result});
        match method {
            "initialize" => {
                let want = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("");
                let proto = if SUPPORTED_PROTOCOLS.contains(&want) { want } else { SUPPORTED_PROTOCOLS[0] };
                *self.protocol.lock().unwrap_or_else(|e| e.into_inner()) = proto.to_owned();
                let has_roots = params.pointer("/capabilities/roots").is_some();
                self.client_roots.store(has_roots, Ordering::SeqCst);
                if has_roots {
                    // roots/list may only be requested after `initialized`; mark pending now so a
                    // fast first tool call waits for it.
                    self.roots.lock().unwrap_or_else(|e| e.into_inner()).pending = true;
                }
                reply.send(ok(json!({
                    "protocolVersion": proto,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": brand::MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": self.instructions(),
                })));
            }
            "ping" => reply.send(ok(json!({}))),
            "tools/list" => reply.send(ok(json!({"tools": defs::definitions(&self.cfg.tools)}))),
            "resources/list" => reply.send(ok(json!({"resources": []}))),
            "resources/templates/list" => reply.send(ok(json!({"resourceTemplates": []}))),
            "prompts/list" => reply.send(ok(json!({"prompts": []}))),
            "logging/setLevel" => reply.send(ok(json!({}))),
            "tools/call" => self.tools_call(id, params, reply),
            _ => reply.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Method not found: {method}")}})),
        }
    }

    fn tools_call(self: &Arc<Self>, id: Value, params: Value, reply: Arc<dyn Reply>) {
        let Some(name) = params.get("name").and_then(|n| n.as_str()).map(str::to_owned) else {
            reply.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "tools/call needs a tool name"}}));
            return;
        };
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let token = Cancellation::new(CALL_TIMEOUT);
        let key = id.to_string();
        {
            let mut requests = self.requests.lock().unwrap_or_else(|e| e.into_inner());
            if requests.contains_key(&key) {
                reply.send(json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32600,"message":"A request with this id is already running."}}));
                return;
            }
            requests.insert(key.clone(), token.clone());
        }
        let (tx, rx) = mpsc::channel::<Value>();
        let me = Arc::clone(self);
        let worker_token = token.clone();
        let submitted = self.submit(Box::new(move || {
            let r = std::panic::catch_unwind(AssertUnwindSafe(|| worker_token.run(|| me.run_tool(&name, &args))));
            let out = match r {
                Ok(o) => o,
                Err(e) if e.is::<Cancelled>() => ToolOutput::ok("This call was cancelled or exceeded its deadline. Narrow the query and try again."),
                Err(_) => {
                    me.clear_projects();
                    ToolOutput::err("Internal error while answering (the cached snapshot was dropped). Retry once.")
                }
            };
            let _ = tx.send(tool_result(out));
        }));
        if !submitted {
            self.requests.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            reply.send(json!({"jsonrpc": "2.0", "id": id, "result": tool_result(ToolOutput::ok("The server is busy or shutting down. Retry after the current calls finish."))}));
            return;
        }
        // The watchdog answers with a success-shaped note if the call overruns its budget.
        let in_flight = InFlight::enter(self);
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let _in_flight = in_flight;
            let result = match rx.recv_timeout(CALL_TIMEOUT) {
                Ok(v) => v,
                Err(_) => {
                    token.cancel();
                    tool_result(ToolOutput::ok("This call exceeded its 30 second deadline; cancellation was requested. Narrow the query and try again."))
                }
            };
            me.requests.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            reply.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
        });
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    struct ChannelReply(mpsc::Sender<Value>);
    impl Reply for ChannelReply {
        fn send(&self, v: Value) {
            let _ = self.0.send(v);
        }
    }

    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }

    fn blocked_pool() -> (Arc<Server>, Release) {
        let server = Server::new(Config::default());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Release(gate.clone());
        let (tx, rx) = mpsc::channel();
        for _ in 0..POOL_SIZE {
            let gate = gate.clone();
            let tx = tx.clone();
            assert!(server.submit(Box::new(move || {
                tx.send(()).unwrap();
                let mut open = gate.0.lock().unwrap();
                while !*open {
                    open = gate.1.wait(open).unwrap();
                }
            })));
        }
        for _ in 0..POOL_SIZE {
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        (server, release)
    }

    #[test]
    fn full_queue_rejects_immediately_and_keeps_control_requests_responsive() {
        let (server, release) = blocked_pool();
        for _ in 0..POOL_SIZE * 4 {
            assert!(server.submit(Box::new(|| {})));
        }
        assert!(!server.submit(Box::new(|| panic!("overflow job must not run"))));
        let (tx, rx) = mpsc::channel();
        let reply = Arc::new(ChannelReply(tx));
        server.handle(json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"explore","arguments":{"query":"foo"}}}), reply.clone());
        let busy = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(busy["result"]["content"][0]["text"].as_str().unwrap().contains("busy"));
        assert!(server.requests.lock().unwrap().is_empty());
        server.handle(json!({"jsonrpc":"2.0", "id":2, "method":"ping"}), reply);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap()["id"], 2);
        drop(release);
        server.shutdown();
    }

    #[test]
    fn repeated_rebuild_requests_share_one_background_worker() {
        let server = Server::new(Config::default());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Release(gate.clone());
        let (tx, rx) = mpsc::channel();
        server.set_rebuild_hook(Arc::new(move |_| {
            tx.send(()).unwrap();
            let mut open = gate.0.lock().unwrap();
            while !*open {
                open = gate.1.wait(open).unwrap();
            }
        }));
        assert!(server.request_rebuild(Path::new("one")));
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..50 {
            assert!(server.request_rebuild(Path::new("one")));
        }
        assert!(!server.request_rebuild(Path::new("two")));
        assert!(rx.try_recv().is_err());
        drop(release);
        server.shutdown();
    }

    #[test]
    fn cancelled_queued_call_never_opens_a_project_and_answers_once() {
        let (server, release) = blocked_pool();
        let opened = Arc::new(AtomicU64::new(0));
        let count = opened.clone();
        server.set_open_hook(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        let (tx, rx) = mpsc::channel();
        let reply = Arc::new(ChannelReply(tx));
        server
            .handle(json!({"jsonrpc":"2.0", "id":"cancel-me", "method":"tools/call", "params":{"name":"explore","arguments":{"query":"foo"}}}), reply.clone());
        server.handle(json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":"cancel-me"}}), reply);
        drop(release);
        let result = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result["result"]["content"][0]["text"].as_str().unwrap().contains("cancelled"));
        server.drain(Duration::from_secs(1));
        assert_eq!(opened.load(Ordering::SeqCst), 0);
        assert!(server.requests.lock().unwrap().is_empty());
        assert!(rx.try_recv().is_err());
        server.shutdown();
    }
}
