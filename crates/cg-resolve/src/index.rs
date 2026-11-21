//! Full index build (design doc 5.5): scan -> extract -> merge -> resolve -> synthesise ->
//! assemble -> write. The same entry point serves `cgr index`, the benchmark commands and the
//! "rebuild" fallback of incremental sync.

use crate::driver::resolve_all;
use crate::engine::Engine;
use crate::project::ProjectInfo;
use cg_core::budget::{peak_rss_mb, MemBudget};
use cg_core::config::ProjectConfig;
use cg_core::{CgError, Result};
use cg_extract::{fileflags, try_extract_stream_in_pool, ExtractConfig, FileBatch, FileHook};
use cg_scan::{scan, ScanOptions};
use cg_store::{build_draft, finish, Draft, FinishInput, ProjectMeta, Seg, Store, View};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct IndexOptions {
    pub threads: Option<usize>,
    pub memory_mb: Option<u64>,
    pub low_priority: bool,
}

pub fn indexing_pool(budget: MemBudget, low_priority: bool) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(budget.threads)
        .stack_size(64 << 20)
        .start_handler(move |_| {
            if low_priority {
                cg_core::budget::lower_thread_priority();
            }
        })
        .build()
        .map_err(|e| CgError::other(format!("thread pool: {e}")))
}

#[derive(Clone, Debug, Default)]
pub struct IndexReport {
    pub files: usize,
    pub unsupported_files: usize,
    pub too_large_files: usize,
    pub parse_error_files: usize,
    pub panicked_files: usize,
    pub nodes: usize,
    pub edges: usize,
    pub refs: usize,
    pub resolved_refs: usize,
    pub source_bytes: u64,
    pub segment_bytes: u64,
    pub threads: usize,
    pub budget_mb: u64,
    pub peak_rss_mb: u64,
    /// Kernel heap capacity released to immutable temporary mappings during extraction.
    pub spilled_kernel_bytes: u64,
    pub timings: Vec<(&'static str, Duration)>,
    pub frameworks: Vec<String>,
    pub warnings: Vec<String>,
}

impl IndexReport {
    pub fn total(&self) -> Duration {
        self.timings.iter().map(|t| t.1).sum()
    }
    pub fn render_timings(&self) -> String {
        let mut s = String::new();
        for (name, d) in &self.timings {
            s.push_str(&format!("  {:<14} {:>9.1} ms\n", name, d.as_secs_f64() * 1e3));
        }
        s.push_str(&format!("  {:<14} {:>9.1} ms   peak RSS {} MB\n", "total", self.total().as_secs_f64() * 1e3, self.peak_rss_mb));
        if self.spilled_kernel_bytes > 0 {
            s.push_str(&format!("  kernel spill: {:.1} MiB of heap buffers released to temporary maps\n", self.spilled_kernel_bytes as f64 / (1 << 20) as f64));
        }
        s
    }
}

struct Phase {
    t: Instant,
    timings: Vec<(&'static str, Duration)>,
}

impl Phase {
    fn new() -> Phase {
        Phase { t: Instant::now(), timings: Vec::new() }
    }
    fn lap(&mut self, name: &'static str) {
        let now = Instant::now();
        self.timings.push((name, now - self.t));
        self.t = now;
    }
}

/// Hooks (framework extract hooks, synthesis site scanners) for a project.
pub fn hooks_for(project: &ProjectInfo) -> Vec<Arc<dyn FileHook>> {
    crate::frameworks::hooks(project)
}

pub struct Built {
    pub seg: Seg,
    pub report: IndexReport,
    pub project: ProjectInfo,
}

/// Build a complete base segment for the project at `root`.
pub fn build_seg(root: &Path, opts: &IndexOptions) -> Result<Built> {
    let mut phase = Phase::new();
    let budget = MemBudget::configured(opts.memory_mb, opts.threads);
    let pool = indexing_pool(budget, opts.low_priority)?;
    let mut report = IndexReport { threads: budget.threads, budget_mb: budget.total >> 20, ..Default::default() };

    let config = ProjectConfig::load(root);
    report.warnings.extend(config.warnings.iter().cloned());
    let mut sopts = ScanOptions::new(root);
    sopts.config = config.clone();
    let scanned = scan(&sopts);
    report.unsupported_files = scanned.unsupported;
    report.too_large_files = scanned.too_large.len();
    phase.lap("scan");

    let project = ProjectInfo::load_known(root, &scanned.config_files);
    let frameworks = project.detect_frameworks();
    report.frameworks = frameworks.clone();
    phase.lap("project");

    let mut cfg = ExtractConfig::new(root);
    cfg.threads = budget.threads;
    cfg.batch_bytes = budget.extract_batch_bytes();
    cfg.overrides = config.extensions.clone();
    cfg.low_priority = opts.low_priority;
    cfg.set_hooks(hooks_for(&project));
    let deprio = deprioritize_matcher(root, &config);
    let mut batches: Vec<FileBatch> = Vec::with_capacity(scanned.files.len());
    let mut resident = 0u64;
    let spill_dir = root.join(cg_core::brand::DATA_DIR).join("tmp");
    try_extract_stream_in_pool(&pool, &scanned.files, &cfg, |i, b| {
        if let Some(mut b) = b {
            if deprio.as_ref().is_some_and(|g| g.matched_path_or_any_parents(&scanned.files[i].rel, false).is_ignore()) {
                b.flags |= fileflags::DEPRIORITIZED;
            }
            resident += cg_extract::spill::resident_bytes(&b);
            batches.push(b);
            if resident >= budget.extract / 2 {
                report.spilled_kernel_bytes += cg_extract::spill::spill(&mut batches, &spill_dir)?;
                resident = 0;
            }
        }
        Ok::<_, std::io::Error>(())
    })?;
    report.files = batches.len();
    report.source_bytes = batches.iter().map(|b| b.size as u64).sum();
    report.parse_error_files = batches.iter().filter(|b| b.flags & fileflags::PARSE_ERROR != 0).count();
    report.panicked_files = batches.iter().filter(|b| b.flags & (fileflags::PANICKED | fileflags::TIMED_OUT) != 0).count();
    phase.lap("extract");

    let meta = ProjectMeta { config_digest: config.digest(), frameworks, config_digests: project.config_hashes.clone() };
    let Draft { seg, edges, emeta } = pool.install(|| build_draft(&batches, &meta));
    drop(batches);
    phase.lap("merge");

    let seg = Arc::new(seg);
    let view = View::new(seg.clone()).with_root(root.to_path_buf());
    let (out, synth) = {
        let mut engine = pool.install(|| Engine::build_with_budget(&view, &project, budget));
        phase.lap("tables");
        let out = pool.install(|| resolve_all(&mut engine));
        phase.lap("resolve");
        let synth = pool.install(|| crate::synth::run_all(&engine));
        phase.lap("synthesis");
        (out, synth)
    };
    report.resolved_refs = out.updates.iter().filter(|u| u.status == cg_store::status::RESOLVED).count();
    drop(view);
    let seg = Arc::try_unwrap(seg).map_err(|_| CgError::other("segment still shared after resolution"))?;

    let mut edges = edges;
    let _ = &mut edges;
    let mut emeta = emeta;
    let mut all = out.edges;
    let base = out.emeta.len() as u32;
    emeta.truncate(emeta.len()); // extraction metadata stays; resolution metadata is appended by `finish`
    for mut e in synth.edges {
        if e.meta != cg_core::NONE {
            e.meta += base;
        }
        all.push(e);
    }
    let mut finish_emeta = out.emeta;
    finish_emeta.extend(synth.emeta);
    let input = FinishInput {
        ref_updates: out.updates,
        edges: all,
        emeta: finish_emeta,
        import_targets: out.import_targets,
        export_targets: out.export_targets,
        ..Default::default()
    };
    let seg = pool.install(|| finish(Draft { seg, edges, emeta }, input));
    phase.lap("assemble");

    report.nodes = seg.meta.n_nodes as usize;
    report.edges = seg.meta.n_edges as usize;
    report.refs = seg.meta.n_refs as usize;
    report.timings = phase.timings;
    report.peak_rss_mb = peak_rss_mb();
    Ok(Built { seg, report, project })
}

pub fn deprioritize_matcher(root: &Path, config: &ProjectConfig) -> Option<ignore::gitignore::Gitignore> {
    if config.deprioritize.is_empty() {
        return None;
    }
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    for p in &config.deprioritize {
        let _ = b.add_line(None, p);
    }
    b.build().ok()
}

/// Build and publish the index of `root` under `.cgr/`.
pub fn index_project(root: &Path, opts: &IndexOptions) -> Result<IndexReport> {
    let store = Store::at(root);
    store.create()?;
    let guard = store.write_lock(Duration::from_secs(30))?;
    store.cleanup_tmp();
    index_with_guard(&store, &guard, root, opts)
}

/// Same as [`index_project`] for a caller that already holds the write lock (incremental sync's
/// full-rebuild fallback, merges).
pub fn index_with_guard(store: &Store, guard: &cg_store::WriteGuard, root: &Path, opts: &IndexOptions) -> Result<IndexReport> {
    let Built { seg, mut report, .. } = build_seg(root, opts)?;
    let t = Instant::now();
    let m = store.publish_base(&seg, guard)?;
    report.segment_bytes = std::fs::metadata(store.dir.join(&m.base)).map(|m| m.len()).unwrap_or(0);
    report.timings.push(("write", t.elapsed()));
    report.peak_rss_mb = peak_rss_mb();
    Ok(report)
}
