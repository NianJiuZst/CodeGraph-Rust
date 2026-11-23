//! Extraction pipeline (design doc 4.4, 4.6): read -> detect -> kernel -> facts -> post,
//! with panic isolation, a big-stack retry for deeply nested files and a parse timeout.
//! Files are processed in memory-bounded batches on a rayon pool; results are placed by
//! file index, so the output never depends on thread scheduling.

use crate::facts::{self, Facts};
use crate::post::{commonjs_require_refs, ExtraNode, ExtraRef, FileHook, HookCtx, HookOut, SiteFact};
use crate::rows::FileRows;
use cg_core::{hash, Lang};
use cg_kernel::{EmitOut, ExtractOptions, KernelError};
use cg_scan::{generated, lang as scanlang, testpath, FileEntry};
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bits of `FileBatch::flags` (stored in `FileRec::flags`, design doc 5.3).
pub use cg_core::fileflags;

#[derive(Clone)]
pub struct ExtractConfig {
    pub root: PathBuf,
    pub threads: usize,
    /// Source bytes read per batch.
    pub batch_bytes: u64,
    pub parse_timeout: Duration,
    pub allow_error_trees: bool,
    pub overrides: BTreeMap<String, Lang>,
    /// Lower thread priority (Unix nice 10) so a build does not starve the editor.
    pub low_priority: bool,
    /// Framework extract hooks and synthesis site scanners.
    hooks: Vec<Arc<dyn FileHook>>,
    hook_plan: Option<Arc<crate::hook_plan::HookPlan>>,
}

impl ExtractConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ExtractConfig {
            root: root.into(),
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2),
            batch_bytes: 64 << 20,
            parse_timeout: Duration::from_secs(5),
            allow_error_trees: true,
            overrides: BTreeMap::new(),
            low_priority: false,
            hooks: Vec::new(),
            hook_plan: None,
        }
    }

    /// Compile all hook trigger literals once for the extraction pool. Replacing hooks
    /// also replaces the plan, so no stale automaton can survive a configuration change.
    pub fn set_hooks(&mut self, hooks: Vec<Arc<dyn FileHook>>) {
        self.hook_plan = Some(Arc::new(crate::hook_plan::HookPlan::new(&hooks)));
        self.hooks = hooks;
    }
}

/// Everything extraction learned about one file.
pub struct FileBatch {
    pub rel: String,
    pub lang: Lang,
    pub size: u32,
    pub mtime_ns: i64,
    pub hash: [u8; 16],
    pub flags: u8,
    /// Resident kernel tables. After spilling, use `rows()` to access the mapped tables.
    pub emit: EmitOut,
    pub(crate) spilled: Option<crate::spill::SpilledTables>,
    pub extra_nodes: Vec<ExtraNode>,
    pub extra_refs: Vec<ExtraRef>,
    pub sites: Vec<SiteFact>,
    pub facts: Facts,
    /// Human-readable problems, e.g. `panic: ...`.
    pub errors: Vec<String>,
}

impl FileBatch {
    pub fn rows(&self) -> FileRows<'_> {
        self.spilled.as_ref().map_or_else(|| FileRows::new(&self.emit), |s| s.rows())
    }
}

/// Extraction output for a file the kernel could not handle: the file node alone.
pub fn file_node_only(rel: &str, line_count: u32) -> EmitOut {
    use cg_kernel::{KERNEL_ABI_VERSION, META_SIZE, NODE_ROW_SIZE, NONE};
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let id = format!("file:{rel}");
    let mut arena = Vec::new();
    let mut put = |s: &str| -> (u32, u32) {
        let o = arena.len() as u32;
        arena.extend_from_slice(s.as_bytes());
        (o, s.len() as u32)
    };
    let (n, q, i) = (put(name), put(rel), put(&id));
    let mut node = Vec::with_capacity(NODE_ROW_SIZE);
    node.push(0); // NodeKind::File
    node.push(0);
    node.extend_from_slice(&0b01u16.to_le_bytes()); // isExported present, false
    node.extend_from_slice(&1u32.to_le_bytes());
    node.extend_from_slice(&line_count.max(1).to_le_bytes());
    node.extend_from_slice(&0u32.to_le_bytes());
    node.extend_from_slice(&0u32.to_le_bytes());
    for (o, l) in [n, q, i] {
        node.extend_from_slice(&o.to_le_bytes());
        node.extend_from_slice(&l.to_le_bytes());
    }
    // docstring, signature, decorators, type_parameters, return_type
    for _ in 0..5 {
        node.extend_from_slice(&NONE.to_le_bytes());
        node.extend_from_slice(&0u32.to_le_bytes());
    }
    node.extend_from_slice(&NONE.to_le_bytes()); // extra_json
    node.extend_from_slice(&0u32.to_le_bytes());
    node.extend_from_slice(&0u32.to_le_bytes()); // metrics
    debug_assert_eq!(node.len(), NODE_ROW_SIZE);
    let mut meta = Vec::with_capacity(META_SIZE);
    meta.push(KERNEL_ABI_VERSION);
    meta.extend_from_slice(&[0u8; 3]);
    for v in [1u32, 0, 0, arena.len() as u32, NONE, 0] {
        meta.extend_from_slice(&v.to_le_bytes());
    }
    meta.extend_from_slice(&0f64.to_le_bytes());
    EmitOut { meta, nodes: node, edges: Vec::new(), refs: Vec::new(), arena }
}

enum KernelOutcome {
    Ok { emit: EmitOut, facts: Box<Facts>, had_error_tree: bool, error_nodes: usize, facts_failed: bool },
    TooDeep,
    TimedOut,
    Failed(String),
}

fn run_kernel(rel: &str, src: &str, lang: Lang, cfg: &ExtractConfig) -> KernelOutcome {
    cg_kernel::hooks::set_parse_timeout(Some(cfg.parse_timeout));
    cg_kernel::hooks::clear();
    let opts = ExtractOptions { allow_error_trees: cfg.allow_error_trees };
    let res = catch_unwind(AssertUnwindSafe(|| cg_kernel::extract_file(rel, src, lang.as_str(), &opts)));
    let timed_out = cg_kernel::hooks::parse_timed_out();
    let tree = cg_kernel::hooks::take_tree();
    match res {
        Err(p) => KernelOutcome::Failed(format!("panic: {}", panic_message(&p))),
        Ok(Err(KernelError::Defer(m))) => {
            if m.contains("nesting too deep") {
                KernelOutcome::TooDeep
            } else {
                // `allow_error_trees = false` path: treat as a parse error with no tables
                KernelOutcome::Failed(m)
            }
        }
        Ok(Err(KernelError::Failed(m))) => {
            if timed_out {
                KernelOutcome::TimedOut
            } else {
                KernelOutcome::Failed(m)
            }
        }
        Ok(Ok(emit)) => {
            let had_error_tree = tree.as_ref().map(|t| t.root_node().has_error()).unwrap_or(false);
            let error_nodes = if had_error_tree { tree.as_ref().map(crate::macro_mask::error_nodes).unwrap_or(0) } else { 0 };
            let mut facts_failed = false;
            let facts = match &tree {
                Some(t) => {
                    let r = catch_unwind(AssertUnwindSafe(|| facts::extract_with_timeout(lang, t, src, FileRows::new(&emit), cfg.parse_timeout)));
                    match r {
                        Ok(f) => f,
                        Err(_) => {
                            facts_failed = true;
                            Facts::default()
                        }
                    }
                }
                None => Facts::default(),
            };
            KernelOutcome::Ok { emit, facts: Box::new(facts), had_error_tree, error_nodes, facts_failed }
        }
    }
}

fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// Only one dedicated big-stack retry at a time (design doc 4.4).
static BIG_STACK: Mutex<()> = Mutex::new(());
const BIG_STACK_BYTES: usize = 512 << 20;

fn run_kernel_big_stack(rel: &str, src: &str, lang: Lang, cfg: &ExtractConfig) -> KernelOutcome {
    let _g = BIG_STACK.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::scope(|s| match std::thread::Builder::new().stack_size(BIG_STACK_BYTES).spawn_scoped(s, || run_kernel(rel, src, lang, cfg)) {
        Ok(h) => h.join().unwrap_or_else(|_| KernelOutcome::Failed("panic in big-stack retry".into())),
        Err(e) => KernelOutcome::Failed(format!("cannot spawn big-stack thread: {e}")),
    })
}

pub(crate) enum ScriptOutcome {
    Ok { emit: EmitOut, facts: Box<Facts>, had_error_tree: bool, facts_failed: bool },
    TimedOut,
    Failed(String),
}

pub(crate) fn run_script_kernel(rel: &str, src: &str, lang: Lang, cfg: &ExtractConfig) -> ScriptOutcome {
    let mut outcome = run_kernel(rel, src, lang, cfg);
    if matches!(outcome, KernelOutcome::TooDeep) {
        outcome = run_kernel_big_stack(rel, src, lang, cfg);
    }
    match outcome {
        KernelOutcome::Ok { emit, facts, had_error_tree, facts_failed, .. } => ScriptOutcome::Ok { emit, facts, had_error_tree, facts_failed },
        KernelOutcome::TooDeep => ScriptOutcome::Failed("nesting too deep for the walker".to_owned()),
        KernelOutcome::TimedOut => ScriptOutcome::TimedOut,
        KernelOutcome::Failed(m) => ScriptOutcome::Failed(m),
    }
}

/// Extract one file. `None` when the file vanished or is unreadable.
pub fn extract_one(entry: &FileEntry, cfg: &ExtractConfig) -> Option<FileBatch> {
    let bytes = std::fs::read(cfg.root.join(&entry.rel)).ok()?;
    Some(extract_bytes(&entry.rel, &bytes, entry.mtime_ns, entry.lang, cfg))
}

/// Extract from file contents already in memory (`lang_hint` is used if detection from the
/// text alone finds no supported language).
pub fn extract_bytes(rel: &str, bytes: &[u8], mtime_ns: i64, lang_hint: Lang, cfg: &ExtractConfig) -> FileBatch {
    let h = hash::hash128(bytes);
    let src = String::from_utf8_lossy(bytes);
    let detected = scanlang::detect(rel, Some(&src), &cfg.overrides);
    let lang = if detected.is_supported() { detected } else { lang_hint };
    let mut flags = 0u8;
    let minified = generated::is_minified_script(rel, &src);
    if minified || generated::is_generated_path(rel) || generated::has_generated_header(&src) {
        flags |= fileflags::GENERATED;
    }
    if testpath::is_test_path(rel) {
        flags |= fileflags::TEST;
    }
    if detected == Lang::ObjC || minified {
        // Objective-C headers have no walker, and minified/compiled JS bundles (`*.min.js`,
        // Emscripten/webpack output) choke Tree-sitter with obfuscated single-line bodies.
        // Keep the file node (so `status`/`files`/`node <file>` still see it) without symbols.
        let line_count = src.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
        return FileBatch {
            rel: rel.to_owned(),
            lang: if detected == Lang::ObjC { Lang::ObjC } else { lang },
            size: bytes.len() as u32,
            mtime_ns,
            hash: h,
            flags,
            emit: file_node_only(rel, line_count),
            spilled: None,
            extra_nodes: Vec::new(),
            extra_refs: Vec::new(),
            sites: Vec::new(),
            facts: Facts::default(),
            errors: Vec::new(),
        };
    }
    let mut errors = Vec::new();
    let (emit, facts, src) = if matches!(lang, Lang::Vue | Lang::Svelte) {
        let out = crate::sfc::extract_sfc(rel, &src, lang, cfg);
        if out.had_error_tree {
            flags |= fileflags::PARSE_ERROR;
        }
        if out.timed_out {
            flags |= fileflags::TIMED_OUT;
        }
        errors.extend(out.errors);
        (out.emit, out.facts, src)
    } else {
        // Macros the C++ grammar can't parse are blanked in place; see cpp_preparse.rs.
        let src = if lang == Lang::Cpp { std::borrow::Cow::Owned(crate::cpp_preparse::pre_parse(&src, rel)) } else { src };
        // Rust items inside conditional wrappers (`cfg_rt! { .. }`) are extracted from a
        // position-preserving view with the wrapper tokens blanked; see macro_mask.rs. The
        // original text is used again if blanking introduced syntax errors.
        let mut mask = if lang == Lang::Rust { crate::macro_mask::rust_item_wrappers(&src, cfg.parse_timeout) } else { crate::macro_mask::Mask::None };
        let mut mask_timed_out = false;
        if matches!(mask, crate::macro_mask::Mask::Expired) {
            mask_timed_out = true;
            mask = crate::macro_mask::Mask::None;
        }
        let mut outcome = match &mask {
            crate::macro_mask::Mask::Masked(m, _) => run_kernel(rel, m, lang, cfg),
            _ => run_kernel(rel, &src, lang, cfg),
        };
        // blanking a non-item `cfg_*!` macro can introduce syntax errors: keep the mask only when
        // the masked parse has no more error nodes than the original
        if let (crate::macro_mask::Mask::Masked(_, original_errors), KernelOutcome::Ok { error_nodes, .. }) = (&mask, &outcome) {
            if *error_nodes > *original_errors {
                mask = crate::macro_mask::Mask::None;
                outcome = run_kernel(rel, &src, lang, cfg);
            }
        }
        let src = match mask {
            crate::macro_mask::Mask::Masked(m, _) => std::borrow::Cow::Owned(m),
            _ => src,
        };
        if matches!(outcome, KernelOutcome::TooDeep) {
            outcome = run_kernel_big_stack(rel, &src, lang, cfg);
        }
        let line_count = src.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
        let (emit, facts) = match outcome {
            KernelOutcome::Ok { emit, facts, had_error_tree, facts_failed, .. } => {
                if had_error_tree {
                    flags |= fileflags::PARSE_ERROR;
                }
                if facts_failed {
                    errors.push("panic while building facts".to_owned());
                }
                if facts.timed_out {
                    flags |= fileflags::TIMED_OUT;
                    errors.push(format!("supplementary facts parse timed out after {:?}", cfg.parse_timeout));
                }
                if mask_timed_out {
                    flags |= fileflags::TIMED_OUT;
                    errors.push(format!("conditional-wrapper pre-pass timed out after {:?}", cfg.parse_timeout));
                }
                (emit, *facts)
            }
            KernelOutcome::TooDeep => {
                errors.push("nesting too deep for the walker".to_owned());
                flags |= fileflags::PANICKED;
                (file_node_only(rel, line_count), Facts::default())
            }
            KernelOutcome::TimedOut => {
                errors.push(format!("parse timed out after {:?}", cfg.parse_timeout));
                flags |= fileflags::TIMED_OUT;
                (file_node_only(rel, line_count), Facts::default())
            }
            KernelOutcome::Failed(m) => {
                if m.starts_with("panic") {
                    flags |= fileflags::PANICKED;
                } else {
                    flags |= fileflags::PARSE_ERROR;
                }
                errors.push(m);
                (file_node_only(rel, line_count), Facts::default())
            }
        };
        (emit, facts, src)
    };
    let mut extra_refs = commonjs_require_refs(&src, lang);
    let (mut extra_nodes, mut sites) = (Vec::new(), Vec::new());
    if !cfg.hooks.is_empty() && flags & (fileflags::PANICKED | fileflags::TIMED_OUT) == 0 {
        let rows = FileRows::new(&emit);
        let ctx = HookCtx { rel, lang, src: &src, rows, facts: &facts };
        let kcount = rows.node_count() as u32;
        let selected = cfg.hook_plan.as_ref().expect("set_hooks builds the trigger plan").select(&cfg.hooks, lang, rel, &src);
        for (hook, selected) in cfg.hooks.iter().zip(selected) {
            if !selected {
                continue;
            }
            let mut out = HookOut::default();
            let r = catch_unwind(AssertUnwindSafe(|| hook.run(&ctx, &mut out)));
            if r.is_err() {
                errors.push(format!("panic in hook {}", hook.name()));
                continue;
            }
            // Hook-created nodes are numbered after the kernel's (and after earlier hooks').
            let base = kcount + extra_nodes.len() as u32;
            for mut r in out.refs {
                // refs from hook-created nodes are expressed relative to `kcount`
                if r.from_row >= kcount {
                    r.from_row = base + (r.from_row - kcount);
                }
                extra_refs.push(r);
            }
            for mut s in out.sites {
                if s.node_row != cg_core::NONE && s.node_row >= kcount {
                    s.node_row = base + (s.node_row - kcount);
                }
                sites.push(s);
            }
            for mut n in out.nodes {
                if n.parent_row >= kcount {
                    n.parent_row = base + (n.parent_row - kcount);
                }
                extra_nodes.push(n);
            }
        }
    }
    FileBatch {
        rel: rel.to_owned(),
        lang,
        size: bytes.len() as u32,
        mtime_ns,
        hash: h,
        flags,
        emit,
        spilled: None,
        extra_nodes,
        extra_refs,
        sites,
        facts,
        errors,
    }
}

fn build_pool(cfg: &ExtractConfig) -> rayon::ThreadPool {
    let low = cfg.low_priority;
    rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.threads.max(1))
        .stack_size(64 << 20)
        .thread_name(|i| format!("cg-extract-{i}"))
        .start_handler(move |_| {
            if low {
                cg_core::budget::lower_thread_priority();
            }
        })
        .build()
        .expect("extract thread pool")
}

/// Extract all `entries`, feeding results to `sink` in input order. Files that vanished
/// are reported as `None`.
pub fn extract_stream(entries: &[FileEntry], cfg: &ExtractConfig, mut sink: impl FnMut(usize, Option<FileBatch>)) {
    let result = try_extract_stream(entries, cfg, |i, b| {
        sink(i, b);
        Ok::<_, std::convert::Infallible>(())
    });
    match result {
        Ok(()) => {}
        Err(never) => match never {},
    }
}

/// Fallible sink variant using a caller-owned Rayon pool. Processes each batch in descending
/// file-size order using a dynamic atomic work cursor (LPT scheduling) so every worker thread
/// stays balanced and large files never form tail stragglers, while delivering results to `sink`
/// in deterministic input order.
pub fn try_extract_stream_in_pool<E>(
    pool: &rayon::ThreadPool,
    entries: &[FileEntry],
    cfg: &ExtractConfig,
    mut sink: impl FnMut(usize, Option<FileBatch>) -> Result<(), E>,
) -> Result<(), E> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut start = 0usize;
    while start < entries.len() {
        let mut end = start;
        let mut bytes = 0u64;
        while end < entries.len() && (end == start || bytes + entries[end].size <= cfg.batch_bytes) {
            bytes += entries[end].size;
            end += 1;
        }
        let batch = &entries[start..end];
        let mut order: Vec<usize> = (0..batch.len()).collect();
        order.sort_unstable_by(|&a, &b| batch[b].size.cmp(&batch[a].size).then(a.cmp(&b)));
        let cursor = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<FileBatch>>> = (0..batch.len()).map(|_| Mutex::new(None)).collect();
        pool.broadcast(|_| loop {
            let pos = cursor.fetch_add(1, Ordering::Relaxed);
            if pos >= order.len() {
                break;
            }
            let idx = order[pos];
            let res = extract_one(&batch[idx], cfg);
            *slots[idx].lock().unwrap_or_else(|e| e.into_inner()) = res;
        });
        for (i, slot) in slots.into_iter().enumerate() {
            let r = slot.into_inner().unwrap_or_else(|e| e.into_inner());
            sink(start + i, r)?;
        }
        start = end;
    }
    Ok(())
}

/// Fallible sink variant. Stops before starting the next batch if a spill or downstream
/// write fails, instead of retaining the rest of the repository in memory.
pub fn try_extract_stream<E>(entries: &[FileEntry], cfg: &ExtractConfig, sink: impl FnMut(usize, Option<FileBatch>) -> Result<(), E>) -> Result<(), E> {
    let pool = build_pool(cfg);
    try_extract_stream_in_pool(&pool, entries, cfg, sink)
}

/// Convenience: all batches in memory, aligned with `entries`.
pub fn extract_all(entries: &[FileEntry], cfg: &ExtractConfig) -> Vec<Option<FileBatch>> {
    let mut out = Vec::with_capacity(entries.len());
    extract_stream(entries, cfg, |_, b| out.push(b));
    out
}

#[cfg(test)]
mod file_node_tests {
    use super::*;

    #[test]
    fn file_node_only_row_has_the_abi_size() {
        let out = file_node_only("src/a/b.rs", 12);
        assert_eq!(out.nodes.len(), cg_kernel::NODE_ROW_SIZE);
        let rows = FileRows::new(&out);
        let n = rows.nodes().next().expect("the file node");
        assert_eq!((n.name(), n.qname(), n.start_line(), n.end_line()), ("b.rs", "src/a/b.rs", 1, 12));
        assert_eq!(rows.nodes().count(), 1);
    }
}
