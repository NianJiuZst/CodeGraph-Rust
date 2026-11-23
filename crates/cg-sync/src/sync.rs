//! The sync algorithm (design doc 9.3). A delta layer is always computed against the *base*
//! segment and the current source tree, so deltas never stack and "incremental equals full" is a
//! statement about one function:
//!
//!   View(base, delta(base, tree)) == fresh index of tree
//!
//! 1. diff the tree against the base (changed files C: replaced / new / removed / touched);
//! 2. re-extract C, build a provisional delta, and from it a resolution engine over the new state;
//! 3. find the references whose result may differ (affected refs) and resolve exactly those;
//! 4. write the delta: nodes/refs/facts of replaced and new files, rewritten refs and edges of
//!    unchanged files whose references changed.
//!
//! When too much is affected, or the configuration changed, the base is rebuilt instead.

use crate::detect::{diff_view, Diff};
use cg_core::budget::MemBudget;
use cg_core::config::ProjectConfig;
use cg_core::{brand, CgError, EdgeKind, FileId, LangFamily, NodeIdx, NodeKind, RefKind, Result, StrId, NONE};
use cg_extract::{fileflags, try_extract_stream_in_pool, BindKind, ExtractConfig, FileBatch};
use cg_resolve::driver::{make_edge, round_of, update_of, FNREF_META};
use cg_resolve::{hooks_for, index_with_guard, resolve_refs, Engine, IndexOptions, Outcome, ProjectInfo, Round_};
use cg_scan::{scan, ScanOptions};
use cg_store::{
    assemble_delta, base_offsets, build_draft_at, outkind, status, DeltaParts, DeltaState, EdgeBuild, EdgesOnlyFile, FileRec, ProjectMeta, RefRec, RefUpdate,
    Seg, Store, View, WriteGuard,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    /// Rebuild the base instead of writing a delta.
    pub force_full: bool,
    pub index: IndexOptions,
    /// Exact watcher paths. `None` performs a full scan; directory/config/burst events
    /// conservatively fall back to one. Periodic full scans recover dropped events.
    pub paths: Option<Vec<PathBuf>>,
    /// Write-lock wait timeout (defaults to 30 seconds).
    pub lock_timeout: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    pub up_to_date: bool,
    /// A base + delta snapshot was repacked without extracting or resolving source.
    pub compacted: bool,
    /// Why the base was rebuilt instead of writing a delta.
    pub full_rebuild: Option<String>,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub touched: usize,
    pub affected_files: usize,
    pub affected_refs: usize,
    pub timings: Vec<(&'static str, Duration)>,
}

impl SyncReport {
    pub fn total(&self) -> Duration {
        self.timings.iter().map(|t| t.1).sum()
    }
    pub fn summary(&self) -> String {
        if self.up_to_date {
            return "index is up to date".into();
        }
        let how = match &self.full_rebuild {
            Some(why) => format!("rebuilt the index ({why})"),
            None => format!("{} file(s) re-resolved", self.affected_files),
        };
        format!(
            "synced: {} added, {} modified, {} removed, {} touched; {how} in {:.0} ms",
            self.added,
            self.modified,
            self.removed,
            self.touched,
            self.total().as_secs_f64() * 1e3
        )
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

/// Fraction limits for incremental delta vs full rebuild.
/// Extraction accounts for the majority of full-rebuild time, so an edges-only delta stays much
/// faster than re-extracting the repository even when many references are re-resolved.
const MAX_CHANGED_FILE_PCT: usize = 25;
const MAX_AFFECTED_REF_PCT: usize = 75;
const MAX_AFFECTED_FILE_PCT: usize = 50;
/// Merge when the delta covers this much of the project, or after this many syncs (5.7).
const MERGE_FILE_PCT: usize = 5;
const MERGE_AFTER_SYNCS: u32 = 200;
const MERGE_BYTES: u64 = 64 << 20;
const MERGE_IDLE_MS: u64 = 10 * 60 * 1000;
const MERGE_QUIET_MS: u64 = 30 * 1000;

fn merge_due(view: &View, manifest: &cg_store::Manifest) -> bool {
    let Some(delta) = &view.delta else {
        return false;
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let idle = now.saturating_sub(manifest.updated_ms);
    let files = view.base.p.files.len();
    idle >= MERGE_QUIET_MS
        && ((files >= 40 && delta.seg.p.delta_files.len() / 3 * 100 > files * MERGE_FILE_PCT)
            || manifest.syncs_since_merge >= MERGE_AFTER_SYNCS
            || delta.seg.file_len >= MERGE_BYTES
            || idle >= MERGE_IDLE_MS)
}

fn compact_snapshot(store: &Store, guard: &WriteGuard, view: &View, rep: &mut SyncReport, ph: &mut Phase, budget: MemBudget) -> Result<()> {
    let pool = cg_resolve::index::indexing_pool(budget, true)?;
    let seg = pool.install(|| cg_store::compact::compact(view));
    ph.lap("compact");
    store.publish_base(&seg, guard)?;
    ph.lap("write");
    rep.compacted = true;
    Ok(())
}

/// Idle maintenance, called by the elected watcher. This never scans, stats or extracts
/// source files. An active writer wins immediately; maintenance retries on the next tick.
pub fn compact_if_due(root: &Path) -> Result<SyncReport> {
    let store = Store::at(root);
    let mut rep = SyncReport { up_to_date: true, ..Default::default() };
    let mut phase = Phase::new();
    let guard = match store.write_lock(Duration::ZERO) {
        Ok(guard) => guard,
        Err(CgError::Lock(_)) => return Ok(rep),
        Err(e) => return Err(e),
    };
    let snapshot = store.open_snapshot()?;
    if merge_due(&snapshot.view, &snapshot.manifest) {
        compact_snapshot(&store, &guard, &snapshot.view, &mut rep, &mut phase, MemBudget::detect())?;
        rep.timings = phase.timings;
    }
    Ok(rep)
}

fn event_paths(root: &Path, view: &View, paths: Option<&[PathBuf]>) -> Option<Vec<String>> {
    let paths = paths?;
    if paths.len() > 500 {
        return None;
    }
    let mut rels = Vec::new();
    for p in paths {
        let p = if p.is_absolute() { p.strip_prefix(root).ok()?.to_path_buf() } else { p.clone() };
        if p.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return None;
        }
        let rel = p.to_str()?.replace('\\', "/");
        if rel.is_empty() || crate::watch::is_config_name(rel.rsplit('/').next()?) || root.join(&p).is_dir() {
            return None;
        }
        // A removed directory is no longer identifiable with stat.
        let prefix = format!("{rel}/");
        if view.files().any(|f| view.file(f).path().starts_with(&prefix)) {
            return None;
        }
        rels.push(rel);
    }
    rels.sort();
    rels.dedup();
    Some(rels)
}

fn scan_events(root: &Path, config: &ProjectConfig, view: &View, paths: &[String]) -> Option<Vec<cg_scan::FileEntry>> {
    let rules = cg_scan::IgnoreRules::new(root, config);
    let mut entries: BTreeMap<String, cg_scan::FileEntry> = view
        .files()
        .map(|f| {
            let file = view.file(f);
            let e = cg_scan::FileEntry { rel: file.path().to_owned(), lang: file.lang(), size: file.rec.size as u64, mtime_ns: file.rec.mtime_ns };
            (e.rel.clone(), e)
        })
        .collect();
    for rel in paths {
        match cg_scan::stat_entry(&rules, config, rel) {
            Some(e) => {
                entries.insert(rel.clone(), e);
            }
            None => {
                // A tracked-but-ignored file can be included by the full scanner's git
                // supplement; use that scanner rather than silently dropping the file.
                if entries.contains_key(rel) && root.join(rel).is_file() && rules.is_ignored(rel, false) {
                    return None;
                }
                entries.remove(rel);
            }
        }
    }
    Some(entries.into_values().collect())
}

/// Bring the index of `root` up to date.
pub fn sync(root: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    let store = Store::at(root);
    if store.read_manifest()?.is_none() {
        return Err(CgError::NoIndex(root.to_path_buf()));
    }
    let guard = store.write_lock(opts.lock_timeout.unwrap_or(Duration::from_secs(30)))?;
    store.cleanup_tmp();
    sync_locked(&store, &guard, root, opts)
}

fn full_rebuild(store: &Store, guard: &WriteGuard, root: &Path, opts: &SyncOptions, mut rep: SyncReport, why: String) -> Result<SyncReport> {
    let r = index_with_guard(store, guard, root, &opts.index)?;
    rep.full_rebuild = Some(why);
    rep.timings.extend(r.timings.iter().copied());
    Ok(rep)
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DefKey {
    kind: u8,
    name: String,
    qname: String,
}

#[derive(Clone, PartialEq, Eq)]
struct DefDetail {
    flags: u16,
    ret: Option<String>,
    sig: Option<String>,
    vis: u8,
}

fn defs_of(view: &View, f: FileId) -> BTreeMap<DefKey, Vec<DefDetail>> {
    let mut m: BTreeMap<DefKey, Vec<DefDetail>> = BTreeMap::new();
    for n in view.nodes_in_file(f) {
        let n = NodeIdx(n);
        let node = view.node(n);
        if !node.kind().is_definition() {
            continue;
        }
        m.entry(DefKey { kind: node.kind() as u8, name: node.name().to_owned(), qname: node.qname().to_owned() }).or_default().push(DefDetail {
            flags: view.hot(n).flags,
            ret: node.return_type().map(str::to_owned),
            sig: node.signature().map(str::to_owned),
            vis: node.visibility() as u8,
        });
    }
    m
}

type ExportSig = Vec<(String, String, String, u8)>;

type ResolverFactSig = BTreeSet<(u8, String, String, String, String)>;
type JsValueFactSig = Vec<(u8, String, String, String)>;

/// Module-level value evidence can be followed through imported aliases.
fn js_value_fact_sig(view: &View, f: FileId) -> JsValueFactSig {
    if view.file(f).lang().family() != LangFamily::Js {
        return Vec::new();
    }
    let scope = view.scope_base(f);
    view.bindings_of(f)
        .iter()
        .filter(|b| b.scope.0 == scope && (b.kind == BindKind::Local as u8 || b.kind == BindKind::Alias as u8))
        .map(|b| (b.kind, view.text_str(b.name).to_owned(), view.text_str(b.decl_type).to_owned(), view.text_str(b.init_ctor).to_owned()))
        .collect()
}

/// JS `interface X` beside `const X`: importing `X` names whichever comes first, so swapping
/// same-named declarations changes what importers link.
fn js_twin_order_sig(view: &View, f: FileId) -> Vec<(String, u8)> {
    if view.file(f).lang().family() != LangFamily::Js {
        return Vec::new();
    }
    let mut count: FxHashMap<&str, u32> = FxHashMap::default();
    for n in view.nodes_in_file(f) {
        *count.entry(view.node(NodeIdx(n)).qname()).or_default() += 1;
    }
    let mut twins: Vec<_> = view
        .nodes_in_file(f)
        .map(|n| view.node(NodeIdx(n)))
        .filter(|n| count[n.qname()] > 1)
        .map(|n| (n.start_line(), n.start_col(), n.qname().to_owned(), n.kind() as u8))
        .collect();
    twins.sort();
    twins.into_iter().map(|(_, _, q, k)| (q, k)).collect()
}

/// These facts change resolution even when the kernel's declaration identity
/// stays the same (e.g. `obj.old = function internal` -> `obj.new = ...`, or the target of a
/// Rust `impl Deref`, which has no node of its own).
fn resolver_fact_sig(view: &View, f: FileId) -> ResolverFactSig {
    let scopes = view.scopes_of(f);
    let scope_base = view.scope_base(f);
    let field_owners: FxHashMap<_, _> = view
        .nodes_in_file(f)
        .map(|n| view.node(NodeIdx(n)))
        .filter(|n| n.kind().is_type())
        .map(|n| (cg_core::text::pack_pos(n.start_line(), n.start_col()), n.qname().to_owned()))
        .collect();
    view.bindings_of(f)
        .iter()
        .filter(|b| {
            ((b.kind == BindKind::MemberFn as u8
                || b.kind == BindKind::ReturnType as u8
                || b.kind == BindKind::Fixture as u8
                || b.kind == BindKind::TypeAlias as u8
                || b.kind == BindKind::FactoryMixin as u8)
                && b.node.is_some())
                || b.kind == BindKind::TraitImpl as u8
                || b.kind == BindKind::Deref as u8
                || b.kind == BindKind::Mutation as u8
                || b.kind == BindKind::CommonJsObject as u8
                || b.kind == BindKind::MacroNamespace as u8
                || b.kind == BindKind::Field as u8
                || (b.kind == BindKind::TypeParam as u8 && view.file(f).lang() == cg_core::Lang::Rust)
                || (b.kind == BindKind::Local as u8
                    && matches!(view.file(f).lang(), cg_core::Lang::Go | cg_core::Lang::Rust | cg_core::Lang::Python | cg_core::Lang::Kotlin)
                    && b.scope.0 == scope_base)
        })
        .map(|b| {
            // A field's declared type or initializer can redirect calls in unchanged
            // consumers (same-package siblings, subclasses, importers). Include its owner:
            // two types may share a field name.
            let owner = if b.kind == BindKind::Field as u8 {
                if b.node.is_some() {
                    view.node(b.node).qname().to_owned()
                } else {
                    scopes.get(b.scope.0.wrapping_sub(scope_base) as usize).and_then(|s| field_owners.get(&s.start)).cloned().unwrap_or_default()
                }
            } else if b.node.is_some() {
                view.node(b.node).qname().to_owned()
            } else {
                String::new()
            };
            (b.kind, view.text_str(b.name).to_owned(), owner, view.text_str(b.decl_type).to_owned(), view.text_str(b.init_ctor).to_owned())
        })
        .collect()
}

/// What a file declares as exports (re-exports, aliases, default exports, star exports). Exported
/// definitions are covered by the definition diff and by dead-target handling.
fn export_sig(view: &View, f: FileId) -> ExportSig {
    let mut v: ExportSig = view
        .exports_of(f)
        .iter()
        .map(|e| (view.text_str(e.name).to_owned(), view.text_str(e.from_spec).to_owned(), view.text_str(e.from_name).to_owned(), e.star))
        .collect();
    v.sort();
    v
}

/// A local export can forward an imported binding (`import * as z ...; export { z }`).
/// Changing that import changes the exported module even when the export syntax is unchanged.
fn reexported_import_sig(view: &View, f: FileId) -> BTreeSet<(String, String, String, u8, u8)> {
    let locals: FxHashSet<&str> =
        view.exports_of(f).iter().filter(|e| view.text_str(e.from_spec).is_empty() && e.node.is_none()).map(|e| view.text_str(e.from_name)).collect();
    view.imports_of(f)
        .iter()
        .filter(|i| i.scope.0 == view.scope_base(f) && locals.contains(view.text_str(i.local)))
        .map(|i| (view.text_str(i.local).to_owned(), view.text_str(i.spec).to_owned(), view.text_str(i.imported).to_owned(), i.kind, i.type_only))
        .collect()
}

/// File-scope imports whose targets are used to interpret exported inference facts (e.g. a
/// Python decorator `from py.eng_a import Engine` switched to `from py.eng_b import Engine` while
/// `self.engine = Engine()` stays textually identical).
fn file_import_sig(view: &View, f: FileId) -> BTreeSet<(String, String, String, u8, u8)> {
    view.imports_of(f)
        .iter()
        .filter(|i| i.scope.0 == view.scope_base(f))
        .map(|i| (view.text_str(i.local).to_owned(), view.text_str(i.spec).to_owned(), view.text_str(i.imported).to_owned(), i.kind, i.type_only))
        .collect()
}

/// Pair the nodes of a replaced file before and after the edit by identity (kind + qualified
/// name + occurrence). A pair whose definition details are unchanged keeps its meaning, so
/// references that resolved to the old node can simply point at the new one.
fn pair_nodes(old_view: &View, new_view: &View, f: FileId, remap: &mut FxHashMap<u32, u32>) {
    type Group = Vec<(u32, Option<DefDetail>)>;
    fn groups(view: &View, f: FileId) -> BTreeMap<(u8, String), Group> {
        let mut m: BTreeMap<(u8, String), Group> = BTreeMap::new();
        for n in view.nodes_in_file(f) {
            let n = NodeIdx(n);
            let node = view.node(n);
            let detail = node.kind().is_definition().then(|| DefDetail {
                flags: view.hot(n).flags,
                ret: node.return_type().map(str::to_owned),
                sig: node.signature().map(str::to_owned),
                vis: node.visibility() as u8,
            });
            m.entry((node.kind() as u8, node.qname().to_owned())).or_default().push((n.0, detail));
        }
        m
    }
    let old = groups(old_view, f);
    let new = groups(new_view, f);
    for (k, olds) in &old {
        let Some(news) = new.get(k) else { continue };
        if olds.len() != news.len() {
            continue;
        }
        for ((o, od), (n, nd)) in olds.iter().zip(news) {
            if od == nd {
                remap.insert(*o, *n);
            }
        }
    }
}

fn is_hierarchy_kind(kind: u8) -> bool {
    matches!(RefKind::from_wire(kind), Some(RefKind::Edge(EdgeKind::Extends | EdgeKind::Implements)))
}

/// Hierarchy as a set of id- and position-independent (sub, super) keys.
///
/// A key is `path#kind@qualified-name` plus the occurrence number among nodes of the same file
/// that share kind and name, so that inserting a line above a class does not look like a change
/// of the class hierarchy (which would widen the re-resolution to every reference in the
/// project), while a renamed class, a moved class or a changed parent still does.
/// Each supertype with its rank among the subtype's: reordering a class's bases changes lookup.
fn hierarchy_keys(view: &View, pairs: impl Iterator<Item = (NodeIdx, NodeIdx, (u32, u32))>) -> BTreeSet<(String, usize, String)> {
    let mut ordinals: FxHashMap<u32, FxHashMap<u32, u32>> = FxHashMap::default();
    let mut key = |n: NodeIdx| -> String {
        let node = view.node(n);
        let file = node.file();
        let ord = ordinals.entry(file.0).or_insert_with(|| {
            let mut seen: FxHashMap<(u8, &str), u32> = FxHashMap::default();
            let mut out: FxHashMap<u32, u32> = FxHashMap::default();
            for m in view.nodes_in_file(file) {
                let nm = view.node(NodeIdx(m));
                let c = seen.entry((nm.kind() as u8, nm.qname())).or_insert(0);
                out.insert(m, *c);
                *c += 1;
            }
            out
        });
        let dead = if view.is_alive(n) { "" } else { "!dead" };
        format!("{}#{}@{}~{}{dead}", view.file(file).path(), node.kind(), node.qname(), ord.get(&n.0).copied().unwrap_or(0))
    };
    let mut written: Vec<(String, (u32, u32), String)> = pairs.map(|(a, b, pos)| (key(a), pos, key(b))).collect();
    written.sort();
    let mut out = BTreeSet::new();
    let mut rank = 0;
    for (i, (a, _, b)) in written.iter().enumerate() {
        rank = if i > 0 && written[i - 1].0 == *a { rank + 1 } else { 0 };
        out.insert((a.clone(), rank, b.clone()));
    }
    out
}

pub fn sync_locked(store: &Store, guard: &WriteGuard, root: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    let mut rep = SyncReport::default();
    let mut ph = Phase::new();
    let budget = MemBudget::configured(opts.index.memory_mb, opts.index.threads);

    let snap = match store.open_snapshot() {
        Ok(s) => s,
        Err(CgError::NoIndex(p)) => return Err(CgError::NoIndex(p)),
        Err(e) => return full_rebuild(store, guard, root, opts, rep, format!("snapshot unreadable: {e}")),
    };
    let manifest = snap.manifest.clone();
    let base: Arc<Seg> = snap.view.base.clone();
    if opts.force_full {
        return full_rebuild(store, guard, root, opts, rep, "requested".into());
    }
    if base.meta.engine_version != brand::ENGINE_VERSION {
        return full_rebuild(store, guard, root, opts, rep, "index built by an older engine".into());
    }

    let config = ProjectConfig::load(root);
    if base.meta.config_digest != config.digest() {
        return full_rebuild(store, guard, root, opts, rep, "project configuration changed".into());
    }
    let paths = event_paths(root, &snap.view, opts.paths.as_deref());

    // ---- 1. scan + config hash check (single walk for CLI sync) ----
    let mut sopts = ScanOptions::new(root);
    sopts.config = config.clone();
    let (entries, config_files): (Vec<cg_scan::FileEntry>, Vec<String>) = match paths.as_ref().and_then(|p| scan_events(root, &config, &snap.view, p)) {
        Some(entries) => (entries, base.meta.config_digests.keys().cloned().collect()),
        None => {
            let scanned = scan(&sopts);
            (scanned.files, scanned.config_files)
        }
    };
    let config_hashes = ProjectInfo::hash_known(root, &config_files);
    if base.meta.config_digests != config_hashes {
        return full_rebuild(store, guard, root, opts, rep, "a build/config file (tsconfig, package.json, go.mod, Cargo.toml, ...) changed".into());
    }
    ph.lap("scan");
    let current = diff_view(&snap.view, root, &entries);
    if current.is_empty() {
        if merge_due(&snap.view, &manifest) {
            compact_snapshot(store, guard, &snap.view, &mut rep, &mut ph, budget)?;
        }
        rep.up_to_date = true;
        rep.timings = ph.timings;
        return Ok(rep);
    }
    let project = ProjectInfo::load_known(root, &config_files);
    let base_view = View::new(base.clone()).with_root(root.to_path_buf());
    let d: Diff = diff_view(&base_view, root, &entries);
    ph.lap("diff");
    if d.is_empty() {
        // the tree equals the base again
        store.clear_delta(guard)?;
        rep.up_to_date = true;
        rep.timings = ph.timings;
        return Ok(rep);
    }
    rep.added = d.added.len();
    rep.modified = d.replaced.len();
    rep.removed = d.removed.len();
    rep.touched = d.touched.len();
    let n_base_files = base.p.files.len();
    let changed_content = d.added.len() + d.replaced.len() + d.removed.len();
    if n_base_files >= 40 && changed_content * 100 > n_base_files * MAX_CHANGED_FILE_PCT {
        let why = format!("{changed_content} of {n_base_files} files changed");
        return full_rebuild(store, guard, root, opts, rep, why);
    }

    // ---- 2. re-extract the changed files ----
    let pool = cg_resolve::index::indexing_pool(budget, opts.index.low_priority)?;
    let replaced_ids: FxHashMap<&str, FileId> = d.replaced.iter().map(|(f, e)| (e.rel.as_str(), *f)).collect();
    let mut entries: Vec<_> = d.replaced.iter().map(|x| x.1.clone()).chain(d.added.iter().cloned()).collect();
    entries.sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    let mut xcfg = ExtractConfig::new(root);
    xcfg.threads = budget.threads;
    xcfg.batch_bytes = budget.extract_batch_bytes();
    xcfg.overrides = config.extensions.clone();
    xcfg.low_priority = opts.index.low_priority;
    xcfg.set_hooks(hooks_for(&project));
    let deprio = cg_resolve::index::deprioritize_matcher(root, &config);
    let mut extracted: Vec<Option<FileBatch>> = (0..entries.len()).map(|_| None).collect();
    let mut resident = 0u64;
    let spill_dir = root.join(brand::DATA_DIR).join("tmp");
    try_extract_stream_in_pool(&pool, &entries, &xcfg, |i, b| {
        if let Some(mut b) = b {
            if deprio.as_ref().is_some_and(|g| g.matched_path_or_any_parents(&entries[i].rel, false).is_ignore()) {
                b.flags |= fileflags::DEPRIORITIZED;
            }
            resident += cg_extract::spill::resident_bytes(&b);
            extracted[i] = Some(b);
            if resident >= budget.extract / 2 {
                cg_extract::spill::spill(extracted.iter_mut().filter_map(Option::as_mut), &spill_dir)?;
                resident = 0;
            }
        }
        Ok::<_, std::io::Error>(())
    })?;
    ph.lap("extract");

    let mut batches: Vec<FileBatch> = Vec::new();
    let mut states: Vec<DeltaState> = Vec::new();
    let mut ids: Vec<FileId> = Vec::new();
    let mut removed: Vec<FileId> = d.removed.clone();
    let mut next_new = n_base_files as u32;
    for (i, e) in entries.iter().enumerate() {
        match (extracted[i].take(), replaced_ids.get(e.rel.as_str())) {
            (Some(b), Some(id)) => {
                batches.push(b);
                states.push(DeltaState::Replaced);
                ids.push(*id);
            }
            (Some(b), None) => {
                batches.push(b);
                states.push(DeltaState::New);
                ids.push(FileId(next_new));
                next_new += 1;
            }
            (None, Some(id)) => removed.push(*id), // no longer extractable
            (None, None) => {}
        }
    }
    removed.sort();
    removed.dedup();
    let replaced: Vec<FileId> = ids.iter().zip(&states).filter(|(_, s)| **s == DeltaState::Replaced).map(|(f, _)| *f).collect();
    let mut dead_nodes: Vec<u32> = Vec::new();
    for f in replaced.iter().chain(removed.iter()) {
        let r = &base.p.files[f.idx()];
        dead_nodes.extend(r.node_start.0..r.node_start.0 + r.node_count);
    }
    let c_files: FxHashSet<u32> = ids.iter().chain(removed.iter()).map(|f| f.0).collect();

    // ---- provisional delta -> resolution engine over the new state ----
    let bb = base_offsets(&base);
    let meta =
        ProjectMeta { config_digest: base.meta.config_digest, frameworks: base.meta.frameworks.clone(), config_digests: base.meta.config_digests.clone() };
    let draft = pool.install(|| build_draft_at(&batches, Some((&ids, bb)), &meta));
    drop(batches);
    let removed_recs: Vec<(FileId, FileRec)> = removed.iter().map(|f| (*f, base.p.files[f.idx()])).collect();
    let provisional = |draft: &cg_store::Draft, indexes: bool| DeltaParts {
        draft: draft.duplicate(),
        states: states.clone(),
        ids: ids.clone(),
        removed: removed_recs.clone(),
        edges_only: Vec::new(),
        ref_updates: Vec::new(),
        edges: Vec::new(),
        emeta: Vec::new(),
        import_targets: Vec::new(),
        export_targets: Vec::new(),
        sites: Vec::new(),
        site_ranges: Vec::new(),
        dead_base_nodes: dead_nodes.clone(),
        total_files: 0,
        total_nodes: 0,
        total_edges: 0,
        base_gen: manifest.base_gen,
        indexes,
    };
    let v1_seg = assemble_delta(&base, provisional(&draft, false));
    let view1 = base_view.with_delta(Arc::new(v1_seg))?;
    ph.lap("draft");
    let mut engine = pool.install(|| Engine::build_with_budget(&view1, &project, budget));
    if project.web_root.is_none() && !engine.modules.php_web_roots().is_empty() {
        let php_touched = d.added.iter().any(|e| e.lang == cg_core::Lang::Php)
            || d.replaced.iter().any(|(_, e)| e.lang == cg_core::Lang::Php)
            || d.removed.iter().any(|f| base_view.file(*f).lang() == cg_core::Lang::Php);
        if php_touched {
            let old_mods = cg_resolve::Modules::build(&base_view, &project);
            let moved = engine.modules.php_web_roots().iter().any(|(dir, root)| old_mods.php_web_roots().get(dir).is_some_and(|prev| prev != root));
            if moved {
                return full_rebuild(store, guard, root, opts, rep, "inferred PHP webRoot changed".into());
            }
        }
    }
    ph.lap("tables");

    // ---- 3. affected references ----
    let n_base_refs = base.p.refs.len() as u32;
    let n_draft_refs = draft.seg.p.refs.len() as u32;
    let mut affected: FxHashSet<u32> = FxHashSet::default();
    for f in ids.iter() {
        for id in view1.ref_range(*f) {
            affected.insert(id);
        }
    }
    // definition / export differences of replaced, new and removed files
    let mut d_names: BTreeSet<String> = BTreeSet::new();
    let mut definitions_changed = false;
    // File-wide export/visibility changes (star/default exports, removed files, Deref/TraitImpl/etc.)
    // invalidate every direct/barrel importer of the file.
    let mut broad_export_changed: Vec<FileId> = Vec::new();
    // Symbol-level changes (renamed/modified definitions, return types, named exports) propagate
    // surgically through re-exporting barrels and value forwarders without blowing up unrelated
    // importers of the same package or barrel.
    let mut symbol_export_changed: FxHashMap<u32, FxHashSet<String>> = FxHashMap::default();
    // Receiver segments whose type/initializer/return type changed (`recv.m()`, `fn().m()`,
    // `pkg.Var.m()` where `r.tail` is `m`, not `recv`/`fn`/`Var`).
    let mut changed_receivers: FxHashSet<String> = FxHashSet::default();
    let mut remap: FxHashMap<u32, u32> = FxHashMap::default();
    // pytest injects a fixture by parameter name, with no import: a changed fixture re-resolves
    // the Python files of its directory and below
    let mut fixture_dirs: Vec<String> = Vec::new();
    let record_sym = |f: FileId, name: &str, d_names: &mut BTreeSet<String>, sym_map: &mut FxHashMap<u32, FxHashSet<String>>| {
        if !name.is_empty() {
            d_names.insert(name.to_owned());
            sym_map.entry(f.0).or_default().insert(name.to_owned());
        }
    };
    let is_receiver_def = |kind: u8| {
        matches!(
            NodeKind::from_u8(kind),
            Some(NodeKind::Variable | NodeKind::Constant | NodeKind::Property | NodeKind::Field | NodeKind::Function | NodeKind::Method)
        )
    };
    for (i, f) in ids.iter().enumerate() {
        let old = if states[i] == DeltaState::Replaced { defs_of(&base_view, *f) } else { BTreeMap::new() };
        let new = defs_of(&view1, *f);
        definitions_changed |= old != new;
        let is_js = view1.file(*f).lang().family() == LangFamily::Js;
        let mut changed_js_fns: Vec<String> = Vec::new();
        for (k, details) in &new {
            // a changed signature or return type can move a call to another overload or redirect a receiver
            if old.get(k) != Some(details) {
                record_sym(*f, &k.name, &mut d_names, &mut symbol_export_changed);
                if let Some(owner) = k.qname.rsplit("::").nth(1) {
                    if !owner.is_empty() && !owner.contains('.') {
                        symbol_export_changed.entry(f.0).or_default().insert(owner.to_owned());
                    }
                }
                if is_receiver_def(k.kind) && !k.name.is_empty() {
                    changed_receivers.insert(k.name.clone());
                }
                if is_js && matches!(NodeKind::from_u8(k.kind), Some(NodeKind::Function | NodeKind::Method | NodeKind::Component)) {
                    changed_js_fns.push(k.name.clone());
                }
            }
        }
        for k in old.keys() {
            if !new.contains_key(k) {
                record_sym(*f, &k.name, &mut d_names, &mut symbol_export_changed);
                if let Some(owner) = k.qname.rsplit("::").nth(1) {
                    if !owner.is_empty() && !owner.contains('.') {
                        symbol_export_changed.entry(f.0).or_default().insert(owner.to_owned());
                    }
                }
                if is_receiver_def(k.kind) && !k.name.is_empty() {
                    changed_receivers.insert(k.name.clone());
                }
                if is_js && matches!(NodeKind::from_u8(k.kind), Some(NodeKind::Function | NodeKind::Method | NodeKind::Component)) {
                    changed_js_fns.push(k.name.clone());
                }
            }
        }
        if !changed_js_fns.is_empty() {
            // A module-level value in the same file (`export const svc = make()`) may inherit a
            // changed function's return annotation; record only bindings referencing a changed fn.
            let scope = view1.scope_base(*f);
            for b in view1.bindings_of(*f) {
                if b.scope.0 == scope && (b.kind == BindKind::Local as u8 || b.kind == BindKind::Alias as u8) {
                    let init = view1.text_str(b.init_ctor);
                    if !init.is_empty() && changed_js_fns.iter().any(|name| init.contains(name.as_str())) {
                        record_sym(*f, view1.text_str(b.name), &mut d_names, &mut symbol_export_changed);
                    }
                }
            }
        }
        let old_facts = if states[i] == DeltaState::Replaced { resolver_fact_sig(&base_view, *f) } else { BTreeSet::new() };
        let new_facts = resolver_fact_sig(&view1, *f);
        let old_values: BTreeSet<_> = if states[i] == DeltaState::Replaced { js_value_fact_sig(&base_view, *f).into_iter().collect() } else { BTreeSet::new() };
        let new_values: BTreeSet<_> = js_value_fact_sig(&view1, *f).into_iter().collect();
        for v in old_values.symmetric_difference(&new_values) {
            if v.1.is_empty() || v.1 == "default" {
                broad_export_changed.push(*f);
            } else {
                record_sym(*f, &v.1, &mut d_names, &mut symbol_export_changed);
            }
        }
        if old_facts != new_facts {
            definitions_changed = true;
            for fact in old_facts.symmetric_difference(&new_facts) {
                let kind = fact.0;
                record_sym(*f, &fact.1, &mut d_names, &mut symbol_export_changed);
                if !fact.2.is_empty() {
                    let owner_tail = fact.2.rsplit("::").next().unwrap_or(&fact.2);
                    record_sym(*f, owner_tail, &mut d_names, &mut symbol_export_changed);
                }
                if kind == BindKind::Fixture as u8 {
                    fixture_dirs.push(cg_resolve::project::parent_dir(view1.file(*f).path()).to_owned());
                } else if kind == BindKind::Field as u8 {
                    changed_receivers.insert(fact.1.clone());
                    // Another field in the same file initialized from this field
                    // (e.g. Python `cls.client = cls.client_class()`) changes type too.
                    for b in view1.bindings_of(*f) {
                        if b.kind == BindKind::Field as u8 && view1.text_str(b.init_ctor).contains(&fact.1) {
                            let dep = view1.text_str(b.name).to_owned();
                            record_sym(*f, &dep, &mut d_names, &mut symbol_export_changed);
                            changed_receivers.insert(dep);
                        }
                    }
                } else if kind == BindKind::ReturnType as u8 || kind == BindKind::Local as u8 {
                    if !fact.1.is_empty() {
                        changed_receivers.insert(fact.1.clone());
                    }
                    if kind == BindKind::Local as u8 && fact.3 == "\x03mod" {
                        broad_export_changed.push(*f);
                    }
                } else if kind == BindKind::Mutation as u8 {
                    // `obj.member = fn`: record both the full dotted name and the bare member tail.
                    if let Some((_, tail)) = fact.1.rsplit_once('.') {
                        record_sym(*f, tail, &mut d_names, &mut symbol_export_changed);
                    }
                } else if kind == BindKind::Deref as u8
                    || kind == BindKind::FactoryMixin as u8
                    || kind == BindKind::CommonJsObject as u8
                    || kind == BindKind::MacroNamespace as u8
                    || kind == BindKind::TraitImpl as u8
                    || kind == BindKind::TypeAlias as u8
                    || kind == BindKind::TypeParam as u8
                {
                    broad_export_changed.push(*f);
                }
            }
        }
        if states[i] == DeltaState::Replaced && (!old_facts.is_empty() || !new_facts.is_empty()) {
            let old_imps = file_import_sig(&base_view, *f);
            let new_imps = file_import_sig(&view1, *f);
            if old_imps != new_imps {
                let mut changed_locals: Vec<&str> = Vec::new();
                let mut wildcard_imp_changed = false;
                for imp in old_imps.symmetric_difference(&new_imps) {
                    if imp.0.is_empty() || imp.0 == "*" {
                        wildcard_imp_changed = true;
                    } else {
                        changed_locals.push(imp.0.as_str());
                    }
                }
                if wildcard_imp_changed {
                    broad_export_changed.push(*f);
                } else {
                    for fact in old_facts.iter().chain(new_facts.iter()) {
                        let mentions = changed_locals.iter().any(|l| fact.3.contains(*l) || fact.4.contains(*l));
                        if !mentions {
                            continue;
                        }
                        record_sym(*f, &fact.1, &mut d_names, &mut symbol_export_changed);
                        if !fact.2.is_empty() {
                            let owner_tail = fact.2.rsplit("::").next().unwrap_or(&fact.2);
                            record_sym(*f, owner_tail, &mut d_names, &mut symbol_export_changed);
                        }
                        if fact.0 == BindKind::Fixture as u8 {
                            fixture_dirs.push(cg_resolve::project::parent_dir(view1.file(*f).path()).to_owned());
                        } else if matches!(fact.0, k if k == BindKind::Field as u8 || k == BindKind::ReturnType as u8 || k == BindKind::Local as u8) {
                            if !fact.1.is_empty() {
                                changed_receivers.insert(fact.1.clone());
                            }
                        } else if matches!(
                            fact.0,
                            k if k == BindKind::Deref as u8
                                || k == BindKind::TypeAlias as u8
                                || k == BindKind::TraitImpl as u8
                                || k == BindKind::MacroNamespace as u8
                        ) {
                            broad_export_changed.push(*f);
                        }
                    }
                }
            }
        }
        if states[i] == DeltaState::Replaced {
            pair_nodes(&base_view, &view1, *f, &mut remap);
            let old_exp: BTreeSet<_> = export_sig(&base_view, *f).into_iter().collect();
            let new_exp: BTreeSet<_> = export_sig(&view1, *f).into_iter().collect();
            for ex in old_exp.symmetric_difference(&new_exp) {
                // Star exports, default exports, CommonJS module exports, or re-export spec changes
                // can change arbitrary names in importers.
                if ex.3 != 0 || ex.0.is_empty() || ex.0 == "default" || ex.0 == cg_extract::facts::COMMONJS_MODULE {
                    broad_export_changed.push(*f);
                } else {
                    record_sym(*f, &ex.0, &mut d_names, &mut symbol_export_changed);
                    if !ex.2.is_empty() {
                        record_sym(*f, &ex.2, &mut d_names, &mut symbol_export_changed);
                    }
                }
            }
            let old_reimp = reexported_import_sig(&base_view, *f);
            let new_reimp = reexported_import_sig(&view1, *f);
            for imp in old_reimp.symmetric_difference(&new_reimp) {
                if imp.0.is_empty() || imp.0 == "*" || imp.2 == "*" || imp.3 == cg_extract::ImportKind::Namespace as u8 {
                    broad_export_changed.push(*f);
                } else {
                    record_sym(*f, &imp.0, &mut d_names, &mut symbol_export_changed);
                    // Propagate under every public export name backed by local `imp.0`
                    // (e.g. `export { inner as execute }` or `export default inner`).
                    for view in [&base_view, &view1] {
                        for ex in view.exports_of(*f) {
                            if view.text_str(ex.from_spec).is_empty() && ex.node.is_none() && view.text_str(ex.from_name) == imp.0 {
                                let ename = view.text_str(ex.name);
                                if ename.is_empty() || ename == "default" {
                                    broad_export_changed.push(*f);
                                } else {
                                    record_sym(*f, ename, &mut d_names, &mut symbol_export_changed);
                                }
                            }
                        }
                    }
                }
            }
            if js_twin_order_sig(&base_view, *f) != js_twin_order_sig(&view1, *f) {
                broad_export_changed.push(*f);
            }
        }
    }
    for f in &removed {
        let facts = resolver_fact_sig(&base_view, *f);
        if facts.iter().any(|fact| fact.0 == BindKind::Fixture as u8) {
            fixture_dirs.push(cg_resolve::project::parent_dir(base_view.file(*f).path()).to_owned());
        }
        for fact in facts {
            d_names.insert(fact.1);
        }
        for k in defs_of(&base_view, *f).keys() {
            d_names.insert(k.name.clone());
        }
        broad_export_changed.push(*f);
    }
    broad_export_changed.sort();
    broad_export_changed.dedup();
    let is_dead = {
        let mut bits = vec![0u64; base.p.nodes_hot.len().div_ceil(64)];
        for n in &dead_nodes {
            bits[*n as usize / 64] |= 1u64 << (*n % 64);
        }
        move |n: u32| (n as usize / 64) < bits.len() && bits[n as usize / 64] >> (n % 64) & 1 == 1
    };
    // consumers (direct importers, through re-exporting barrels) of files whose exports changed
    let mut consumer_files: FxHashSet<u32> = FxHashSet::default();
    if !broad_export_changed.is_empty() || !symbol_export_changed.is_empty() {
        let live: Vec<FileId> = view1.files().collect();
        let mut importers: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        let mut value_forwarders: FxHashSet<u32> = FxHashSet::default();
        for g in &live {
            if view1.file(*g).lang().family() == LangFamily::Js {
                let scope = view1.scope_base(*g);
                let values: FxHashSet<&str> = view1
                    .bindings_of(*g)
                    .iter()
                    .filter(|b| {
                        b.scope.0 == scope
                            && (b.kind == BindKind::Local as u8 || b.kind == BindKind::Alias as u8)
                            && (!view1.text_str(b.decl_type).is_empty() || !view1.text_str(b.init_ctor).is_empty())
                    })
                    .map(|b| view1.text_str(b.name))
                    .collect();
                if engine.exports[g.idx()].values().any(|e| match e {
                    cg_resolve::ExportEntry::Node(n) if view1.hot(*n).file == *g => values.contains(view1.node(*n).name()),
                    _ => false,
                }) {
                    value_forwarders.insert(g.0);
                }
            }
            let mut srcs: FxHashSet<u32> = FxHashSet::default();
            for t in &engine.imp_targets[g.idx()] {
                match t {
                    cg_resolve::ImportTarget::Pkg(pkg) => srcs.extend(engine.modules.pkgs[pkg.0 as usize].files.iter().map(|file| file.0)),
                    _ => {
                        if let Some(file) = engine.modules.target_file(*t) {
                            srcs.insert(file.0);
                        }
                    }
                }
            }
            for x in &engine.stars[g.idx()] {
                srcs.insert(x.0);
            }
            for e in engine.exports[g.idx()].values() {
                match e {
                    cg_resolve::ExportEntry::Re(x, _) | cg_resolve::ExportEntry::Module(x) => {
                        srcs.insert(x.0);
                    }
                    _ => {}
                }
            }
            for x in srcs {
                importers.entry(x).or_default().push(g.0);
            }
        }
        // removed files are not in `live`: their importers come from the base facts
        for f in &removed {
            for g in base_view.files() {
                if base_view.imports_of(g).iter().any(|i| i.target == *f) || base_view.exports_of(g).iter().any(|x| x.target == *f) {
                    importers.entry(f.0).or_default().push(g.0);
                }
            }
        }
        // 1. Broad file-wide export changes cascade unconditionally through barrels/forwarders.
        let mut broad_frontier: Vec<u32> = broad_export_changed.iter().map(|f| f.0).collect();
        let mut broad_seen: FxHashSet<u32> = broad_frontier.iter().copied().collect();
        while let Some(x) = broad_frontier.pop() {
            for g in importers.get(&x).into_iter().flatten() {
                consumer_files.insert(*g);
                let base_reexports = |g: u32, x: u32| -> bool {
                    (g as usize) < base.p.files.len() && {
                        let ff = base.p.file_facts[g as usize];
                        base.p.exports[ff.exp_start as usize..(ff.exp_start + ff.exp_count) as usize].iter().any(|e| e.target.0 == x)
                    }
                };
                let reexports = engine.stars[*g as usize].iter().any(|y| y.0 == x)
                    || engine.exports[*g as usize]
                        .values()
                        .any(|e| matches!(e, cg_resolve::ExportEntry::Re(y, _) | cg_resolve::ExportEntry::Module(y) if y.0 == x))
                    || base_reexports(*g, x);
                if (reexports || value_forwarders.contains(g)) && broad_seen.insert(*g) {
                    broad_frontier.push(*g);
                }
            }
        }
        // 2. Symbol-level export changes propagate only the affected symbol names through barrels,
        //    re-export aliases (`export { make as create }`), and JS value forwarders, and only mark
        //    importers that actually import one of those names (aliased or direct).
        let mut sym_queue: Vec<(u32, String)> = Vec::new();
        let mut sym_seen: FxHashSet<(u32, String)> = FxHashSet::default();
        for (f, syms) in &symbol_export_changed {
            if broad_seen.contains(f) {
                continue;
            }
            for s in syms {
                if sym_seen.insert((*f, s.clone())) {
                    sym_queue.push((*f, s.clone()));
                }
            }
        }
        while let Some((x, sym)) = sym_queue.pop() {
            for &g in importers.get(&x).into_iter().flatten() {
                let gf = FileId(g);
                // Direct/aliased import of `sym` from `x` marks `g` as a consumer.
                let imports_sym = view1.imports_of(gf).iter().enumerate().any(|(ii, imp)| {
                    let targets_x = engine.imp_targets[g as usize].get(ii).is_some_and(|t| match t {
                        cg_resolve::ImportTarget::Pkg(pkg) => engine.modules.pkgs[pkg.0 as usize].files.iter().any(|file| file.0 == x),
                        _ => engine.modules.target_file(*t).is_some_and(|file| file.0 == x),
                    });
                    if !targets_x {
                        return false;
                    }
                    let imported = view1.text_str(imp.imported);
                    let local = view1.text_str(imp.local);
                    imported == "*" || local == "*" || imported == sym || local == sym
                });
                if imports_sym {
                    consumer_files.insert(g);
                    // If `g` forwards computed JS values (e.g. `export const svc = make()`),
                    // propagate only the exported value bindings whose initializer/type mentions
                    // `sym` (or the local alias `g` imported `sym` under), not every unrelated
                    // export in a large barrel/schema module.
                    if value_forwarders.contains(&g) {
                        let scope = view1.scope_base(gf);
                        let local_names: Vec<&str> = view1
                            .imports_of(gf)
                            .iter()
                            .filter(|imp| view1.text_str(imp.imported) == sym || view1.text_str(imp.local) == sym)
                            .map(|imp| view1.text_str(imp.local))
                            .collect();
                        for b in view1.bindings_of(gf) {
                            if b.scope.0 != scope || (b.kind != BindKind::Local as u8 && b.kind != BindKind::Alias as u8) {
                                continue;
                            }
                            let init = view1.text_str(b.init_ctor);
                            let dty = view1.text_str(b.decl_type);
                            let mentions = init.contains(sym.as_str())
                                || dty.contains(sym.as_str())
                                || local_names.iter().any(|l| !l.is_empty() && (init.contains(*l) || dty.contains(*l)));
                            if mentions {
                                let bname = view1.text_str(b.name);
                                if engine.exports[g as usize].contains_key(bname) {
                                    d_names.insert(bname.to_owned());
                                    if sym_seen.insert((g, bname.to_owned())) {
                                        sym_queue.push((g, bname.to_owned()));
                                    }
                                }
                            }
                        }
                    }
                }
                // Star re-export (`export * from './x'`) passes `sym` unchanged.
                if engine.stars[g as usize].iter().any(|y| y.0 == x) && sym_seen.insert((g, sym.clone())) {
                    sym_queue.push((g, sym.clone()));
                }
                // Named re-export (`export { make as create } from './x'` or `export * as ns from './x'`).
                for (exp_name, entry) in &engine.exports[g as usize] {
                    match entry {
                        cg_resolve::ExportEntry::Re(y, orig) if y.0 == x && view1.text_str(*orig) == sym => {
                            consumer_files.insert(g);
                            d_names.insert((*exp_name).to_owned());
                            if sym_seen.insert((g, (*exp_name).to_owned())) {
                                sym_queue.push((g, (*exp_name).to_owned()));
                            }
                        }
                        cg_resolve::ExportEntry::Module(y) if y.0 == x && sym_seen.insert((g, sym.clone())) => {
                            // `pub mod x` / `export * as ns from './x'`: callers access `ns.sym`,
                            // already matched by `d_names`; propagate `sym` (not the module name
                            // itself, which would cascade to every unrelated user of the module).
                            sym_queue.push((g, sym.clone()));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    let tail_ids: FxHashSet<u32> = d_names.iter().filter_map(|n| base.names.find(n.as_bytes())).map(|s| s.0).collect();
    // PHP method/function lookup is ASCII case-insensitive (#3297): fold `d_names` when any PHP
    // file changed so an unchanged `$x->SAYHELLO()` invalidates when `sayHello()` changes.
    let php_changed =
        ids.iter().any(|f| view1.file(*f).lang() == cg_core::Lang::Php) || removed.iter().any(|f| base_view.file(*f).lang() == cg_core::Lang::Php);
    let php_tails_lower: FxHashSet<String> = if php_changed { d_names.iter().map(|n| n.to_ascii_lowercase()).collect() } else { FxHashSet::default() };
    let php_file_added = d.added.iter().any(|e| e.lang == cg_core::Lang::Php);
    for dir in &fixture_dirs {
        for (fi, rec) in base.p.files.iter().enumerate() {
            let path = base.paths.get(rec.path);
            if path.ends_with(".py") && (dir.is_empty() || path.starts_with(&format!("{dir}/"))) {
                consumer_files.insert(fi as u32);
            }
        }
    }
    let all_refs_of_file = |f: u32, affected: &mut FxHashSet<u32>| {
        let rec = &base.p.files[f as usize];
        for id in rec.ref_start..rec.ref_start + rec.ref_count {
            affected.insert(id);
        }
    };
    let mut retarget: FxHashMap<u32, u32> = FxHashMap::default();
    let debug = std::env::var_os("CGR_SYNC_DEBUG").is_some();
    let (mut hit_imports, mut hit_tail, mut hit_dead, mut hit_consumer) = (0usize, 0usize, 0usize, 0usize);
    for (fi, rec) in base.p.files.iter().enumerate() {
        if c_files.contains(&(fi as u32)) {
            continue;
        }
        if consumer_files.contains(&(fi as u32)) {
            hit_consumer += 1;
            all_refs_of_file(fi as u32, &mut affected);
            continue;
        }
        // A file with one affected reference is re-resolved as a whole: references depend on
        // each other through local bindings and receiver types (`x = f(); x.m()`).
        let mut file_hit = false;
        // an import now resolving to another file (or to none) changes what the file binds
        let facts = base.p.file_facts[fi];
        for (i, old) in base.p.imports[facts.imp_start as usize..(facts.imp_start + facts.imp_count) as usize].iter().enumerate() {
            let new_t = engine.imp_targets[fi].get(i).and_then(|t| engine.modules.target_file(*t)).map(|file| view1.file(file).path());
            let old_t = old.target.opt().map(|t| base.paths.get(base.p.files[t.idx()].path));
            if new_t != old_t {
                file_hit = true;
                hit_imports += 1;
                if debug {
                    eprintln!("sync: import #{i} of {} now {:?} (was {:?})", base.paths.get(rec.path), new_t, old_t);
                }
                break;
            }
        }
        if !file_hit {
            let check_receivers = !changed_receivers.is_empty();
            let file_lang = cg_core::Lang::from_u8(rec.lang);
            let is_php_file = file_lang == cg_core::Lang::Php;
            let is_svelte_file = file_lang == cg_core::Lang::Svelte;
            let check_php_ci = is_php_file && !php_tails_lower.is_empty();
            let check_php_includes = is_php_file && php_file_added;
            for i in 0..rec.ref_count {
                let r = &base.p.refs[(rec.ref_start + i) as usize];
                let by_tail = tail_ids.contains(&r.tail.0)
                    || (check_php_ci && php_tails_lower.contains(&base.names.get(r.tail).to_ascii_lowercase()))
                    || (is_svelte_file && {
                        let t = base.names.get(r.tail);
                        t.starts_with('$') && !t.starts_with("$$") && d_names.contains(&t[1..])
                    });
                let by_dead = r.status == status::RESOLVED && is_dead(r.target.0) && !remap.contains_key(&r.target.0);
                let by_recv = check_receivers && {
                    let name = base.text.get(r.name);
                    (name.contains('.') || name.contains("::") || name.contains("->"))
                        && name.split(['.', ':']).flat_map(|s| s.split("->")).any(|seg| {
                            let head = seg.split(['(', '[']).next().unwrap_or(seg);
                            !head.is_empty() && changed_receivers.contains(head)
                        })
                };
                // Function-body PHP `require`/`include` has no `ImportFact`, so re-check unresolved
                // import refs when a new PHP file is added.
                let by_php_inc = check_php_includes
                    && r.status != status::RESOLVED
                    && RefKind::from_wire(r.kind) == Some(RefKind::Edge(EdgeKind::Imports))
                    && matches!(engine.modules.resolve_import(cg_core::Lang::Php, base.text.get(r.name), FileId(fi as u32)), cg_resolve::ImportTarget::File(_));
                if by_tail || by_dead || by_recv || by_php_inc {
                    file_hit = true;
                    hit_tail += by_tail as usize;
                    hit_dead += by_dead as usize;
                    if debug {
                        eprintln!(
                            "sync: {} hit by {} ref {:?}",
                            base.paths.get(rec.path),
                            if by_tail { "tail" } else { "dead/recv/inc" },
                            base.text.get(r.name)
                        );
                    }
                    break;
                }
            }
        }
        if file_hit {
            all_refs_of_file(fi as u32, &mut affected);
        } else {
            for i in 0..rec.ref_count {
                let id = rec.ref_start + i;
                let r = &base.p.refs[id as usize];
                if r.status == status::RESOLVED {
                    if let Some(n) = remap.get(&r.target.0) {
                        retarget.insert(id, *n);
                    }
                }
            }
        }
    }
    if debug {
        eprintln!(
            "sync: files hit: imports {hit_imports}, tail {hit_tail}, dead {hit_dead}, consumers {hit_consumer}; remapped nodes {}; d_names {:?}",
            remap.len(),
            d_names
        );
    }
    // (the files' own refs of C files are in `affected` already via view1.ref_range)

    let file_of = |id: u32| -> FileId { view1.hot(view1.reference(id).from).file };
    // Early threshold bailout BEFORE spending time on resolution & synthesis when the initial
    // affected set already exceeds delta limits.
    let check_thresholds = |affected: &FxHashSet<u32>, rep: &mut SyncReport| -> Option<String> {
        let affected_base_files: BTreeSet<u32> =
            affected.iter().filter(|id| **id < n_base_refs).map(|id| file_of(*id).0).filter(|f| !c_files.contains(f)).collect();
        rep.affected_refs = affected.len();
        rep.affected_files = affected_base_files.len() + ids.len();
        if affected.len() * 100 > (n_base_refs as usize).max(1) * MAX_AFFECTED_REF_PCT && n_base_refs > 2000 {
            return Some(format!("{} of {} references affected", affected.len(), n_base_refs));
        }
        if rep.affected_files * 100 > n_base_files.max(1) * MAX_AFFECTED_FILE_PCT && n_base_files >= 40 {
            return Some(format!("{} of {} files affected", rep.affected_files, n_base_files));
        }
        None
    };
    if let Some(why) = check_thresholds(&affected, &mut rep) {
        return full_rebuild(store, guard, root, opts, rep, why);
    }

    // ---- 3b. resolve, widening when imports or the hierarchy change ----
    let all_files: Vec<FileId> = view1.files().collect();
    let mut outcomes: FxHashMap<u32, Outcome> = FxHashMap::default();
    {
        // round 1: imports, supertypes, type references
        let todo: FxHashSet<u32> = affected.iter().copied().filter(|id| round_of(view1.reference(*id).kind) == Round_::One).collect();
        if !todo.is_empty() {
            let mut files: Vec<FileId> = todo.iter().map(|id| file_of(*id)).collect();
            files.sort();
            files.dedup();
            let res = pool.install(|| resolve_refs(&engine, &files, Round_::One, Some(&todo)));
            for (id, o) in res {
                outcomes.insert(id, o);
            }
        }
    }
    // hierarchy from all resolved extends / implements refs
    let mut pairs: Vec<(NodeIdx, NodeIdx, (u32, u32))> = Vec::new();
    for &f in &all_files {
        let range = view1.ref_range(f);
        for (i, r) in view1.refs_of_file(f).iter().enumerate() {
            if !is_hierarchy_kind(r.kind) {
                continue;
            }
            let id = range.start + i as u32;
            let (st, tg) = match outcomes.get(&id) {
                Some(o) => (o.status, o.target),
                None => (r.status, retarget.get(&id).copied().unwrap_or(r.target.0)),
            };
            if st == status::RESOLVED {
                pairs.push((r.from, NodeIdx(tg), (r.line, r.col)));
            }
        }
    }
    let old_pairs = {
        let mut v = Vec::new();
        for r in base.p.refs.iter() {
            if is_hierarchy_kind(r.kind) && r.status == status::RESOLVED {
                v.push((r.from, r.target, (r.line, r.col)));
            }
        }
        hierarchy_keys(&base_view, v.into_iter())
    };
    let new_pairs = hierarchy_keys(&view1, pairs.iter().copied());
    let hierarchy_changed = old_pairs != new_pairs;
    if debug && hierarchy_changed {
        for p in old_pairs.difference(&new_pairs).take(5) {
            eprintln!("sync: hierarchy only before: {p:?}");
        }
        for p in new_pairs.difference(&old_pairs).take(5) {
            eprintln!("sync: hierarchy only after: {p:?}");
        }
    }
    engine.set_hierarchy(pairs.into_iter());
    if hierarchy_changed {
        for &f in &all_files {
            let range = view1.ref_range(f);
            for (i, r) in view1.refs_of_file(f).iter().enumerate() {
                if round_of(r.kind) == Round_::Two {
                    affected.insert(range.start + i as u32);
                }
            }
        }
        if let Some(why) = check_thresholds(&affected, &mut rep) {
            return full_rebuild(store, guard, root, opts, rep, why);
        }
    }
    let todo2: FxHashSet<u32> = affected.iter().copied().filter(|id| round_of(view1.reference(*id).kind) == Round_::Two).collect();
    if !todo2.is_empty() {
        let mut files: Vec<FileId> = todo2.iter().map(|id| file_of(*id)).collect();
        files.sort();
        files.dedup();
        let res = pool.install(|| resolve_refs(&engine, &files, Round_::Two, Some(&todo2)));
        for (id, o) in res {
            outcomes.insert(id, o);
        }
    }
    ph.lap("resolve");

    // ---- synthesised edges: recomputed over the new state, compared with the base ----
    let synth = pool.install(|| {
        cg_resolve::synth::run_incremental(
            &engine,
            &base_view,
            &c_files,
            &remap,
            hierarchy_changed
                || definitions_changed
                || !broad_export_changed.is_empty()
                || !symbol_export_changed.is_empty()
                || hit_imports > 0
                || !removed.is_empty(),
        )
    });
    let mut synth_by_file: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
    for (i, e) in synth.edges.iter().enumerate() {
        synth_by_file.entry(view1.hot(NodeIdx(e.src)).file.0).or_default().push(i);
    }
    let mut synth_dirty: BTreeSet<u32> = BTreeSet::new();
    {
        // synthesised edges of the base, grouped by the file of their source
        let mut base_synth: FxHashMap<u32, Vec<(u32, u32, usize)>> = FxHashMap::default(); // file -> (src, dst, eid)
        for (eid, kb) in base.p.out_kind.iter().enumerate() {
            if outkind::class(*kb) == outkind::CLASS_SYNTH {
                let src = base.p.out_off.partition_point(|o| *o as usize <= eid) as u32 - 1;
                base_synth.entry(base.p.nodes_hot[src as usize].file.0).or_default().push((src, base.p.out_dst[eid], eid));
            }
        }
        let new_key = |e: &EdgeBuild| -> String {
            format!(
                "{}>{}|{}|{}|{}|{}",
                cg_store::dump::node_key(&view1, NodeIdx(e.src)),
                cg_store::dump::node_key(&view1, NodeIdx(e.dst)),
                e.kind,
                e.line,
                e.col,
                synth.emeta.get(e.meta as usize).map_or("", |m| m.as_str())
            )
        };
        let mut files: BTreeSet<u32> = synth_by_file.keys().copied().collect();
        files.extend(base_synth.keys().copied());
        for f in files {
            if f as usize >= n_base_files || c_files.contains(&f) {
                continue; // draft file, replaced or removed: their edges are rewritten anyway
            }
            let old = base_synth.get(&f).map(|v| v.as_slice()).unwrap_or(&[]);
            let mut old_keys: Vec<String> = Vec::with_capacity(old.len());
            let mut dirty = false;
            for (src, dst, eid) in old {
                if is_dead(*dst) {
                    dirty = true;
                    break;
                }
                let kb = base.p.out_kind[*eid];
                let site = base.p.out_site[*eid];
                let meta = base_view.edge_meta(*eid as u32).unwrap_or("");
                old_keys.push(format!(
                    "{}>{}|{}|{}|{}|{}",
                    cg_store::dump::node_key(&base_view, NodeIdx(*src)),
                    cg_store::dump::node_key(&base_view, NodeIdx(*dst)),
                    outkind::kind(kb) as u8,
                    site.line,
                    site.col,
                    meta
                ));
            }
            if !dirty {
                let mut new_keys: Vec<String> = synth_by_file.get(&f).map(|v| v.iter().map(|i| new_key(&synth.edges[*i])).collect()).unwrap_or_default();
                old_keys.sort();
                new_keys.sort();
                dirty = old_keys != new_keys;
            }
            if dirty {
                synth_dirty.insert(f);
            }
        }
    }

    // ---- 4. assemble the delta ----
    let affected_base_files: BTreeSet<u32> =
        affected.iter().filter(|id| **id < n_base_refs).map(|id| file_of(*id).0).filter(|f| !c_files.contains(f)).collect();
    rep.affected_refs = affected.len();
    rep.affected_files = affected_base_files.len() + ids.len();
    // edges-only files: unchanged content, rewritten refs/edges (plus touched files)
    let mut eo_set: BTreeSet<u32> = affected_base_files.clone();
    eo_set.extend(synth_dirty.iter().copied());
    for id in retarget.keys() {
        let f = base.p.files.partition_point(|r| r.ref_start + r.ref_count <= *id);
        eo_set.insert(f as u32);
    }
    for (f, _) in &d.touched {
        if !c_files.contains(&f.0) {
            eo_set.insert(f.0);
        }
    }
    let touched_recs: FxHashMap<u32, &cg_scan::FileEntry> = d.touched.iter().map(|(f, e)| (f.0, e)).collect();
    let mut eo_start: FxHashMap<u32, u32> = FxHashMap::default();
    let mut next = n_base_refs + n_draft_refs;
    for f in &eo_set {
        eo_start.insert(*f, next);
        next += base.p.files[*f as usize].ref_count;
    }
    let final_ref = |id: u32| -> u32 {
        if id >= n_base_refs {
            return id;
        }
        let f = base.p.files.partition_point(|r| r.ref_start + r.ref_count <= id);
        // files with no refs share a ref_start; `partition_point` lands on the file owning `id`
        let rec = &base.p.files[f];
        eo_start[&(f as u32)] + (id - rec.ref_start)
    };
    let mut emeta: Vec<String> = vec![FNREF_META.to_owned()];
    let mut emeta_map: FxHashMap<String, u32> = FxHashMap::default();
    emeta_map.insert(FNREF_META.to_owned(), 0);
    let mut edges: Vec<EdgeBuild> = Vec::new();

    // draft files: one edge per resolved reference
    for id in n_base_refs..n_base_refs + n_draft_refs {
        let o = outcomes.get(&id).copied().unwrap_or(Outcome::PENDING);
        if let Some(e) = make_edge(&engine, id, &o, 0) {
            edges.push(e);
        }
    }
    let ref_updates: Vec<RefUpdate> =
        (n_base_refs..n_base_refs + n_draft_refs).map(|id| update_of(&outcomes.get(&id).copied().unwrap_or(Outcome::PENDING))).collect();
    let import_targets: Vec<u32> =
        ids.iter().flat_map(|f| engine.imp_targets[f.idx()].iter().map(|t| engine.modules.target_file(*t).map_or(NONE, |file| file.0))).collect();

    let export_targets: Vec<u32> = ids.iter().flat_map(|f| view1.exports_of(*f).iter().map(|ex| engine.export_target(*f, ex)).collect::<Vec<_>>()).collect();
    let mut edges_only: Vec<EdgesOnlyFile> = Vec::new();
    for &f in &eo_set {
        let rec = base.p.files[f as usize];
        let mut refs: Vec<RefRec> = base.p.refs[rec.ref_start as usize..(rec.ref_start + rec.ref_count) as usize].to_vec();
        for (i, r) in refs.iter_mut().enumerate() {
            let id = rec.ref_start + i as u32;
            if let Some(o) = outcomes.get(&id) {
                let u = update_of(o);
                r.status = u.status;
                r.by = u.by;
                r.target = NodeIdx(u.target);
            } else if let Some(n) = retarget.get(&id) {
                r.target = NodeIdx(*n);
            }
        }
        // out-edges: extraction edges and untouched resolution edges are copied
        for n in rec.node_start.0..rec.node_start.0 + rec.node_count {
            for e in base_view.out_edges(NodeIdx(n)) {
                let keep = match e.class {
                    outkind::CLASS_EXTRACT => true,
                    outkind::CLASS_RESOLVE => e.ref_idx.is_some_and(|r| !outcomes.contains_key(&r)),
                    _ => false,
                };
                if !keep {
                    continue;
                }
                let meta = match base_view.edge_meta(e.eid) {
                    Some(m) => *emeta_map.entry(m.to_owned()).or_insert_with(|| {
                        emeta.push(m.to_owned());
                        (emeta.len() - 1) as u32
                    }),
                    None => NONE,
                };
                let dst = match e.ref_idx.and_then(|r| retarget.get(&r)) {
                    Some(nt) => *nt,
                    None => e.dst.0,
                };
                edges.push(EdgeBuild {
                    src: n,
                    dst,
                    kind: outkind::pack(e.kind, e.provenance, e.class),
                    line: e.line.unwrap_or(NONE),
                    col: e.col.unwrap_or(NONE),
                    res: e.res,
                    ref_idx: e.ref_idx.map_or(NONE, final_ref),
                    meta,
                    order: 0,
                });
            }
        }
        for i in 0..rec.ref_count {
            let id = rec.ref_start + i;
            if let Some(o) = outcomes.get(&id) {
                if let Some(mut e) = make_edge(&engine, id, o, 0) {
                    e.ref_idx = final_ref(id);
                    edges.push(e);
                }
            }
        }
        let mut new_rec = rec;
        if let Some(entry) = touched_recs.get(&f) {
            new_rec.mtime_ns = entry.mtime_ns;
            new_rec.size = entry.size as u32;
        }
        edges_only.push(EdgesOnlyFile { file: FileId(f), rec: new_rec, refs });
    }

    // synthesised edges: those whose source file is rewritten in this delta
    let synth_meta_base = emeta.len() as u32;
    emeta.extend(synth.emeta.iter().cloned());
    let rewritten: FxHashSet<u32> = ids.iter().map(|f| f.0).chain(eo_set.iter().copied()).collect();
    for (f, idxs) in &synth_by_file {
        if !rewritten.contains(f) {
            continue;
        }
        for i in idxs {
            let mut e = synth.edges[*i];
            if e.meta != NONE {
                e.meta += synth_meta_base;
            }
            edges.push(e);
        }
    }

    // totals for the manifest
    let dead_count = dead_nodes.len() as u32;
    let total_nodes = base.p.nodes_hot.len() as u32 - dead_count + draft.seg.p.nodes_hot.len() as u32;
    let total_files = view1.files().count() as u32;
    let mut dropped_edges = 0u32;
    for f in replaced.iter().map(|f| f.0).chain(removed.iter().map(|f| f.0)).chain(eo_set.iter().copied()) {
        let rec = &base.p.files[f as usize];
        for n in rec.node_start.0..rec.node_start.0 + rec.node_count {
            dropped_edges += base.p.out_off[n as usize + 1] - base.p.out_off[n as usize];
        }
    }
    let parts = DeltaParts {
        draft: draft.duplicate(),
        states: states.clone(),
        ids: ids.clone(),
        removed: removed_recs.clone(),
        edges_only,
        ref_updates,
        edges,
        emeta,
        import_targets,
        export_targets,
        sites: Vec::new(),
        site_ranges: Vec::new(),
        dead_base_nodes: dead_nodes.clone(),
        total_files,
        total_nodes,
        total_edges: 0,
        base_gen: manifest.base_gen,
        indexes: true,
    };
    let mut delta = assemble_delta(&base, parts);
    delta.meta.n_edges = base.meta.n_edges - dropped_edges + delta.p.out_dst.len() as u32;
    ph.lap("assemble");
    // Publishing a source edit never waits for compaction. The quiet-period maintenance
    // repacks this snapshot later, under the same writer lock.
    store.publish_delta(&delta, guard)?;
    ph.lap("write");
    rep.timings = ph.timings;
    Ok(rep)
}

#[allow(dead_code)]
fn _unused(_: StrId) {}
