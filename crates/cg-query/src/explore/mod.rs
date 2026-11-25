//! The explore pipeline (design doc 8.3). One call answers "how does X work" with the
//! verbatim line-numbered source of the relevant symbols, the call path among the symbols the
//! query names, and a blast-radius summary. The output shape follows the upstream tool, whose
//! A/B results the design inherits.

pub mod flow;
pub mod one_symbol;
pub mod window;

use crate::graph::Graph;
use crate::search::{tokenize_query, QueryToken, SearchOptions, TokKind};
use crate::strings;
use cg_core::{fileflags, EdgeKind, FileId, NodeIdx, NodeKind};
use flow::NamedTok;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use window::{Want, WantCtx};

/// Output budget for a project size (design doc 8.3, upstream `getExploreOutputBudget`).
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub calls: u32,
    pub max_chars: usize,
    pub default_files: usize,
    pub per_file: usize,
    pub gap: u32,
    /// Relationships, extra-file pointers, completeness and budget notes.
    pub extras: bool,
}

pub fn budget_for(files: usize) -> Budget {
    match files {
        0..=149 => Budget { calls: 1, max_chars: 13_000, default_files: 4, per_file: 3_800, gap: 7, extras: false },
        150..=499 => Budget { calls: 1, max_chars: 18_000, default_files: 5, per_file: 3_800, gap: 8, extras: false },
        500..=4_999 => Budget { calls: 2, max_chars: 24_000, default_files: 8, per_file: 6_500, gap: 12, extras: true },
        5_000..=14_999 => Budget { calls: 3, max_chars: 24_000, default_files: 8, per_file: 7_000, gap: 15, extras: true },
        _ => Budget { calls: if files >= 25_000 { 5 } else { 4 }, max_chars: 24_000, default_files: 8, per_file: 7_000, gap: 15, extras: true },
    }
}

const MAX_HISTORY_CALLS: usize = 32;

/// Cached summary for a structural graph query (`callers` / `callees`) at a fixed index generation.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SentQuery {
    pub(crate) tool: String,
    pub(crate) key: String,
    pub(crate) generation: u64,
    pub(crate) summary: String,
    pub(crate) bytes: usize,
}

/// Source ranges and structural query results already given in this session (kept for the last 32
/// calls per project). Serializable so CLI sessions (`--session` / `CGR_SESSION`) share the same
/// dedup state across consecutive invocations as MCP sessions do in-process.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    calls: VecDeque<BTreeMap<String, SentFile>>,
    #[serde(default)]
    queries: VecDeque<SentQuery>,
    #[serde(skip)]
    track_pending: bool,
    #[serde(skip)]
    pending_calls: VecDeque<BTreeMap<String, SentFile>>,
    #[serde(skip)]
    pending_queries: VecDeque<SentQuery>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SentFile {
    pub(crate) fingerprint: (u64, i64),
    pub(crate) ranges: Vec<(u32, u32)>,
}

/// Build a concise back-reference notice if and only if it is strictly shorter than
/// re-sending `raw_bytes`. Returns `(notice_text, net_saved_bytes)`.
pub(crate) fn dedup_notice(subject: &str, raw_bytes: usize, hint: &str) -> Option<(String, usize)> {
    let draft = format!("> Already sent earlier in this conversation: {subject} (saved {raw_bytes} B) — {hint}");
    if raw_bytes <= draft.len() {
        return None;
    }
    let net = raw_bytes - draft.len();
    let notice = format!("> Already sent earlier in this conversation: {subject} (saved {net} B) — {hint}");
    let actual_saved = raw_bytes.saturating_sub(notice.len());
    if actual_saved == 0 {
        return None;
    }
    Some((notice, actual_saved))
}

impl History {
    pub fn new() -> History {
        History::default()
    }

    /// Load a persisted session history, returning an empty history if missing or unreadable.
    pub fn load(path: &Path) -> History {
        let mut h: History = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        while h.calls.len() > MAX_HISTORY_CALLS {
            h.calls.pop_front();
        }
        while h.queries.len() > MAX_HISTORY_CALLS {
            h.queries.pop_front();
        }
        h.track_pending = true;
        h.pending_calls.clear();
        h.pending_queries.clear();
        h
    }

    /// Atomically persist session history (temp file + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }

    /// Merge every call/query recorded by this process into the latest on-disk session file
    /// under a per-session advisory lock so parallel CLI calls never overwrite each other.
    pub fn save_merged(&self, path: &Path) -> std::io::Result<()> {
        if self.pending_calls.is_empty() && self.pending_queries.is_empty() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_extension("lock");
        let lock = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&lock_path)?;
        lock.lock()?;
        let mut disk = History::load(path);
        disk.track_pending = false;
        for call in &self.pending_calls {
            disk.record(call.clone());
        }
        for q in &self.pending_queries {
            disk.record_query(q.clone());
        }
        let res = disk.save(path);
        drop(lock);
        res
    }

    pub(crate) fn covered(&self, path: &str, fp: (u64, i64)) -> Vec<(u32, u32)> {
        let mut raw: Vec<(u32, u32)> = Vec::new();
        for call in &self.calls {
            if let Some(s) = call.get(path) {
                if s.fingerprint == fp {
                    raw.extend(s.ranges.iter().copied());
                }
            }
        }
        if raw.len() <= 1 {
            return raw;
        }
        raw.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(raw.len());
        for (s, e) in raw {
            match merged.last_mut() {
                Some(last) if s <= last.1.saturating_add(1) => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        merged
    }

    pub(crate) fn is_covered(&self, path: &str, fp: (u64, i64), start: u32, end: u32) -> bool {
        self.covered(path, fp).iter().any(|c| c.0 <= start && c.1 >= end)
    }

    pub(crate) fn record(&mut self, call: BTreeMap<String, SentFile>) {
        // Advance the 32-call sliding window on every history-enabled call so old ranges expire.
        if self.track_pending {
            self.pending_calls.push_back(call.clone());
            while self.pending_calls.len() > MAX_HISTORY_CALLS {
                self.pending_calls.pop_front();
            }
        }
        self.calls.push_back(call);
        while self.calls.len() > MAX_HISTORY_CALLS {
            self.calls.pop_front();
        }
    }

    pub(crate) fn record_ranges(&mut self, path: String, fp: (u64, i64), ranges: Vec<(u32, u32)>) {
        if ranges.is_empty() {
            return;
        }
        let mut call = BTreeMap::new();
        call.insert(path, SentFile { fingerprint: fp, ranges });
        self.record(call);
    }

    pub(crate) fn find_query(&self, tool: &str, key: &str, generation: u64) -> Option<&SentQuery> {
        self.queries.iter().rev().find(|q| q.tool == tool && q.key == key && q.generation == generation)
    }

    pub(crate) fn record_query(&mut self, q: SentQuery) {
        if self.track_pending {
            self.pending_queries.push_back(q.clone());
            while self.pending_queries.len() > MAX_HISTORY_CALLS {
                self.pending_queries.pop_front();
            }
        }
        self.queries.push_back(q);
        while self.queries.len() > MAX_HISTORY_CALLS {
            self.queries.pop_front();
        }
    }
}

#[derive(Default, Clone)]
pub struct ExploreOptions {
    pub max_files: Option<usize>,
    /// Hard output cap override (the prompt hook uses 9,000; `--max-chars` / `maxChars` sets this).
    pub max_output: Option<usize>,
    /// Omit verbatim source bodies and return call flow, blast radius, and symbol outlines only.
    pub compact: bool,
    /// Session de-duplication history; `None` re-serves source on every call.
    pub history: Option<Arc<Mutex<History>>>,
}

pub struct ExploreResult {
    pub text: String,
    /// Paths of the files whose source was included.
    pub files: Vec<String>,
    pub symbols: usize,
    /// The query named something that exists (exact symbol or file).
    pub found_named: bool,
    pub meta: crate::tools::ToolMeta,
}

struct FileGroup {
    file: FileId,
    nodes: Vec<(NodeIdx, f32)>,
    score: f32,
    pinned: Option<usize>,
    named: bool,
    on_flow: bool,
    anchors: Vec<u32>,
}

fn is_listed(k: NodeKind) -> bool {
    !matches!(k, NodeKind::File | NodeKind::Import | NodeKind::Export | NodeKind::Parameter)
}

impl Graph {
    /// Is the file on disk different from the indexed one?
    pub fn file_is_stale(&self, f: FileId) -> bool {
        let rec = self.view.file(f);
        let Some(abs) = self.abs_path(rec.path()) else { return false };
        match std::fs::metadata(&abs) {
            Ok(m) => {
                let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64);
                m.len() != rec.rec.size as u64 || mtime != rec.rec.mtime_ns
            }
            Err(_) => true,
        }
    }

    pub(crate) fn fingerprint(&self, f: FileId) -> (u64, i64) {
        let rec = self.view.file(f);
        (rec.rec.size as u64, rec.rec.mtime_ns)
    }

    fn read_lines(&self, f: FileId) -> Option<crate::source::SourceView> {
        let abs = self.abs_path(self.path(f))?;
        crate::source::SourceView::read(&abs, 1 << 20).ok()
    }

    /// Run the explore pipeline for `query`.
    pub fn explore(&self, query: &str, opts: &ExploreOptions) -> ExploreResult {
        let budget = budget_for(self.indexed_files());
        let hard = opts.max_output.unwrap_or_else(|| ((budget.max_chars as f64 * 1.5).round() as usize).min(25_000));
        let max_files = opts.max_files.map(|n| n.clamp(1, 20)).unwrap_or(budget.default_files);
        let mut meta = crate::tools::ToolMeta::default();

        // 1. normalise
        let q = query.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(res) = one_symbol::try_explore_one_symbol(self, &q, hard, opts) {
            return res;
        }
        let toks = tokenize_query(&q);

        // 2. pin files named by path
        let mut pinned: Vec<(FileId, Option<u32>)> = Vec::new();
        let mut unresolved_paths: Vec<String> = Vec::new();
        for t in toks.iter().filter(|t| t.kind == TokKind::Path) {
            let files = self.files_matching(&t.raw);
            if files.len() == 1 {
                if !pinned.iter().any(|(f, _)| *f == files[0]) {
                    pinned.push((files[0], t.anchor));
                } else if let Some(p) = pinned.iter_mut().find(|(f, _)| *f == files[0]) {
                    p.1 = p.1.or(t.anchor);
                }
            } else {
                unresolved_paths.push(t.raw.clone());
            }
        }
        pinned.truncate(max_files);
        let words: Vec<QueryToken> = toks.iter().filter(|t| t.kind != TokKind::Path).cloned().collect();

        // 3. retrieval and named symbols
        let mut rel: FxHashMap<NodeIdx, f32> = FxHashMap::default();
        let mut exact: FxHashSet<NodeIdx> = FxHashSet::default();
        let mut named: Vec<NamedTok> = Vec::new();
        let mut unseen: Vec<String> = Vec::new();
        for (i, t) in words.iter().enumerate() {
            // Only code-shaped words and qualified names name symbols; a plain word such as
            // "request" or "route" is a search term, not a flow endpoint.
            let mut nodes: Vec<NodeIdx> = match t.kind {
                TokKind::Code | TokKind::Qualified => self.resolve_symbol(&t.raw),
                TokKind::Plain => Vec::new(),
                TokKind::Path => Vec::new(),
            };
            nodes.retain(|n| is_listed(self.kind(*n)));
            if nodes.is_empty() && matches!(t.kind, TokKind::Code | TokKind::Qualified) {
                let hit = self.search_tokens(std::slice::from_ref(t), &SearchOptions { limit: 3, ..Default::default() });
                if hit.iter().all(|h| h.score < 40.0) {
                    unseen.push(t.raw.clone());
                }
            }
            if !nodes.is_empty() {
                named.push(NamedTok { tok: i, nodes });
            }
        }
        // disambiguate overloaded names by the types the query also names
        let named_types: FxHashSet<NodeIdx> = named.iter().flat_map(|n| n.nodes.iter().copied()).filter(|n| self.kind(*n).is_type()).collect();
        let pascal_types: FxHashSet<NodeIdx> = words
            .iter()
            .filter(|t| t.raw.chars().next().is_some_and(|c| c.is_uppercase()))
            .flat_map(|t| self.view.by_name(&t.raw).collect::<Vec<_>>())
            .filter(|n| self.kind(*n).is_type())
            .collect();
        for nt in named.iter_mut() {
            if nt.nodes.len() > 1 {
                for set in [&named_types, &pascal_types] {
                    let pref: Vec<NodeIdx> = nt.nodes.iter().copied().filter(|n| self.containers(*n).first().is_some_and(|c| set.contains(c))).collect();
                    if !pref.is_empty() && pref.len() < nt.nodes.len() {
                        nt.nodes = pref;
                        break;
                    }
                }
            }
            nt.nodes.truncate(12);
            for n in &nt.nodes {
                exact.insert(*n);
            }
        }
        // search: single pass computes both whole-query and per-word top seeds
        let seeds = self.search_for_explore(&words, 24, 8);
        for (n, s) in seeds {
            if s >= 0.3 {
                rel.insert(n, s);
            }
        }
        for n in &exact {
            rel.insert(*n, 1.0);
        }

        // 4. relevant subgraph: expand 3 layers from strong seeds along non-contains edges
        let mut frontier: Vec<NodeIdx> = rel.iter().filter(|(_, s)| **s >= 0.55).map(|(n, _)| *n).collect();
        frontier.sort_by_key(|n| n.0);
        for _ in 0..3 {
            let mut next: Vec<NodeIdx> = Vec::new();
            for n in frontier {
                let base = rel[&n];
                let push = |other: NodeIdx, decay: f32, rel: &mut FxHashMap<NodeIdx, f32>, next: &mut Vec<NodeIdx>| {
                    let k = self.kind(other);
                    if !is_listed(k) || rel.len() >= 200 {
                        return;
                    }
                    let nr = base * decay;
                    if nr < 0.2 {
                        return;
                    }
                    let e = rel.entry(other).or_insert(0.0);
                    if nr > *e {
                        *e = nr;
                        next.push(other);
                    }
                };
                for e in self.view.out_edges(n).take(60) {
                    if e.kind != EdgeKind::Contains && e.kind != EdgeKind::Imports && e.kind != EdgeKind::Exports {
                        push(e.dst, 0.55, &mut rel, &mut next);
                    }
                }
                for e in self.view.in_edges(n).take(60) {
                    if e.kind != EdgeKind::Contains && e.kind != EdgeKind::Imports && e.kind != EdgeKind::Exports {
                        push(e.src, 0.45, &mut rel, &mut next);
                    }
                }
            }
            next.sort_by_key(|n| n.0);
            next.dedup();
            frontier = next;
            if frontier.is_empty() || rel.len() >= 200 {
                break;
            }
        }
        // pinned files contribute their symbols unconditionally
        let mut pinned_nodes: FxHashSet<NodeIdx> = FxHashSet::default();
        for (f, _) in &pinned {
            for n in self.view.nodes_in_file(*f).take(300) {
                let n = NodeIdx(n);
                if is_listed(self.kind(n)) {
                    pinned_nodes.insert(n);
                    let e = rel.entry(n).or_insert(0.0);
                    if *e < 0.5 {
                        *e = 0.5;
                    }
                }
            }
        }

        // named-symbol flow
        let flow = self.build_flow(&named);
        let mut question: FxHashSet<NodeIdx> = exact.clone();
        question.extend(flow.path_nodes.iter().copied());
        question.extend(flow.named_nodes.iter().copied());

        // 5. file groups and ranking
        let mut groups: FxHashMap<FileId, FileGroup> = FxHashMap::default();
        for (n, s) in &rel {
            let f = self.view.hot(*n).file;
            let g = groups.entry(f).or_insert_with(|| FileGroup {
                file: f,
                nodes: Vec::new(),
                score: 0.0,
                pinned: None,
                named: false,
                on_flow: false,
                anchors: Vec::new(),
            });
            g.nodes.push((*n, *s));
        }
        for (i, (f, anchor)) in pinned.iter().enumerate() {
            let g = groups.entry(*f).or_insert_with(|| FileGroup {
                file: *f,
                nodes: Vec::new(),
                score: 0.0,
                pinned: None,
                named: false,
                on_flow: false,
                anchors: Vec::new(),
            });
            g.pinned = Some(i);
            if let Some(a) = anchor {
                g.anchors.push(*a);
            }
        }
        for g in groups.values_mut() {
            g.nodes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0 .0.cmp(&b.0 .0)));
            let top: f32 = g.nodes.iter().take(5).map(|x| x.1).sum();
            let extra = g.nodes.len().saturating_sub(5) as f32 * 0.05;
            g.score = (top + extra) * self.file_weight(g.file);
            g.named = g.nodes.iter().any(|(n, _)| exact.contains(n));
            g.on_flow = g.nodes.iter().any(|(n, _)| flow.path_nodes.contains(n));
        }
        let mut ranked: Vec<FileGroup> = groups.into_values().collect();
        ranked.sort_by(|a, b| {
            let key = |g: &FileGroup| (g.pinned.is_none(), g.pinned.unwrap_or(0), !g.on_flow, !g.named, self.is_generated_file(g.file));
            key(a).cmp(&key(b)).then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)).then_with(|| self.path(a.file).cmp(self.path(b.file)))
        });

        // 6-7. budget and source windows
        let mut total = 0usize;
        // Each rendered section plus the optional path recorded into `sent_now` if the full section survives truncation.
        let mut sections: Vec<(Option<String>, String)> = Vec::new();
        let mut shown_files: Vec<String> = Vec::new();
        let mut shown_symbols: FxHashSet<NodeIdx> = FxHashSet::default();
        let mut stale_files: Vec<String> = Vec::new();
        let mut omitted: Vec<(FileId, Vec<NodeIdx>)> = Vec::new();
        let mut sent_now: BTreeMap<String, SentFile> = BTreeMap::new();
        let mut trimmed_any = false;
        let mut history = if opts.compact { None } else { opts.history.as_ref().map(|h| h.lock().unwrap_or_else(|e| e.into_inner())) };
        let flow_text = self.render_flow(&flow);
        let blast = self.blast_radius(&exact, &named);
        let relationships = if budget.extras && !opts.compact { self.relationships(&rel) } else { String::new() };
        let header_reserve = flow_text.len() + blast.len() + relationships.len() + 900 + q.len();
        let epilogue_reserve = if opts.max_output.is_some() { (hard / 8).min(1_200) } else { 1_200 };
        let room_for_sources = hard.saturating_sub(header_reserve + epilogue_reserve).max(hard / 3);
        let min_remaining = if opts.compact { 160 } else { (hard / 6).clamp(300, 700) };
        let effective_per_file = if opts.max_output.is_some() { budget.per_file.min((hard * 3 / 5).max(800)) } else { budget.per_file };
        let mut backrefs_only: Vec<String> = Vec::new();

        for grp in &ranked {
            if shown_files.len() >= max_files {
                omitted.push((grp.file, grp.nodes.iter().map(|x| x.0).collect()));
                continue;
            }
            let remaining = room_for_sources.saturating_sub(total);
            if remaining < min_remaining {
                omitted.push((grp.file, grp.nodes.iter().map(|x| x.0).collect()));
                continue;
            }
            let path = self.path(grp.file).to_owned();
            let listed: Vec<(NodeIdx, f32)> = grp.nodes.iter().copied().filter(|(n, _)| is_listed(self.kind(*n))).collect();
            let stale = self.file_is_stale(grp.file);

            if opts.compact {
                if stale {
                    stale_files.push(path.clone());
                }
                let mut sym_lines: Vec<String> = Vec::new();
                let mut seen_nodes: FxHashSet<NodeIdx> = FxHashSet::default();
                let mut sorted_listed = listed.clone();
                sorted_listed.sort_by_key(|(n, _)| self.line(*n));
                for (n, _) in &sorted_listed {
                    if !seen_nodes.insert(*n) {
                        continue;
                    }
                    shown_symbols.insert(*n);
                    let start = self.line(*n);
                    let end = self.end_line(*n).max(start);
                    let span = if end > start { format!("{start}-{end}") } else { format!("{start}") };
                    let sig = self.view.node(*n).signature().map(|s| format!(" `{}`", s.split_whitespace().collect::<Vec<_>>().join(" "))).unwrap_or_default();
                    sym_lines.push(format!("- `{}` ({}){sig} — :{span}", self.name(*n), self.kind(*n)));
                    if sym_lines.len() >= 16 {
                        break;
                    }
                }
                let stale_tag = if stale { " · ⚠ changed on disk" } else { "" };
                let head = format!("**`{path}`** ({}{stale_tag})", plural(sym_lines.len(), "symbol"));
                let mut section = String::new();
                section.push_str(&head);
                section.push('\n');
                if sym_lines.is_empty() {
                    section.push_str("- _(file matched)_\n");
                } else {
                    for l in &sym_lines {
                        section.push_str(l);
                        section.push('\n');
                    }
                }
                if total + section.len() > room_for_sources && !sections.is_empty() {
                    omitted.push((grp.file, grp.nodes.iter().map(|x| x.0).collect()));
                    continue;
                }
                total += section.len() + 1;
                sections.push((None, section));
                shown_files.push(path);
                continue;
            }

            let lang = self.view.file(grp.file).lang().as_str();
            let Some(lines) = self.read_lines(grp.file) else {
                omitted.push((grp.file, grp.nodes.iter().map(|x| x.0).collect()));
                continue;
            };
            let costs = window::line_costs(&lines);
            let mut cap = if grp.pinned.is_some() || grp.named { (effective_per_file as f32 * 1.6) as usize } else { effective_per_file };
            cap = cap.min(remaining);

            let mut header_syms: Vec<String> = Vec::new();
            let mut ranges: Vec<window::Range>;
            let mut flag = String::new();
            if stale {
                stale_files.push(path.clone());
                if lines.len() <= 400 && costs.last().copied().unwrap_or(0) as usize <= cap * 2 {
                    // show the full CURRENT source: valid as a Read
                    let all = vec![Want { start: 1, end: lines.len() as u32, imp: 1.0, must: true }];
                    ranges = window::plan(all, 0, &costs, cap * 2);
                    flag = "⚠ changed on disk after the last index sync — showing the file's full current source".to_owned();
                } else {
                    sections.push((
                        None,
                        format!(
                            "**`{path}`** — ⚠ changed on disk after the last index sync — source omitted (indexed line ranges no longer match, so a slice could show the wrong code). Read this file directly for current content; the change is picked up on that project's next index sync.\n"
                        ),
                    ));
                    total += 400;
                    continue;
                }
            } else {
                let first_child = window::first_child_starts(self, grp.file);
                let ctx = WantCtx { g: self, question: &question, anchors: &grp.anchors, first_child: &first_child };
                let mut wants: Vec<Want> = Vec::new();
                let only_pin = grp.pinned.is_some() && !grp.nodes.iter().any(|(n, _)| !pinned_nodes.contains(n) || exact.contains(n));
                if only_pin {
                    // a pinned file named by path: anchors first, else a skeleton of its definitions
                    if !grp.anchors.is_empty() {
                        for a in &grp.anchors {
                            wants.push(Want { start: a.saturating_sub(12).max(1), end: (a + 30).min(lines.len() as u32), imp: 1.0, must: true });
                        }
                    }
                    let whole = costs.last().copied().unwrap_or(0) as usize;
                    if grp.anchors.is_empty() && whole <= cap {
                        wants.push(Want { start: 1, end: lines.len() as u32, imp: 1.0, must: true });
                    } else {
                        for (n, s) in &listed {
                            let h = self.view.hot(*n);
                            if h.start_line > 0 {
                                wants.push(Want { start: h.start_line, end: (h.start_line + 2).min(h.end_line.max(h.start_line)), imp: *s * 0.5, must: false });
                            }
                        }
                    }
                } else {
                    for (n, s) in &listed {
                        let must = exact.contains(n) || flow.path_nodes.contains(n);
                        ctx.for_node(*n, *s, must, &mut wants);
                    }
                    for a in &grp.anchors {
                        wants.push(Want { start: a.saturating_sub(12).max(1), end: (a + 30).min(lines.len() as u32), imp: 1.0, must: true });
                    }
                }
                if wants.is_empty() {
                    omitted.push((grp.file, grp.nodes.iter().map(|x| x.0).collect()));
                    continue;
                }
                ranges = window::plan(wants.clone(), budget.gap, &costs, cap);
                let wanted_lines: usize = wants.iter().map(|w| (w.end - w.start + 1) as usize).sum();
                let shown_lines: usize = ranges.iter().map(|r| (r.end - r.start + 1) as usize).sum();
                if (shown_lines + 2 < wanted_lines.min(lines.len()) && ranges.len() < 2 && !only_pin) || ranges.iter().any(|r| r.end - r.start > 400) {
                    trimmed_any = true;
                }
            }
            // symbols covered by the shown ranges, for the file header
            for (n, _) in &listed {
                let h = self.view.hot(*n);
                if ranges.iter().any(|r| h.start_line >= r.start && h.start_line <= r.end) {
                    shown_symbols.insert(*n);
                    if header_syms.len() < 8 {
                        let label = format!("{}({})", self.name(*n), self.kind(*n));
                        if !header_syms.contains(&label) {
                            header_syms.push(label);
                        }
                    }
                }
            }

            // 9. session de-duplication: subtract ranges already sent for this unchanged file (skip stale files)
            let fp = self.fingerprint(grp.file);
            let mut covered_note = String::new();
            if !stale {
                if let Some(h) = history.as_mut() {
                    let covered = h.covered(&path, fp);
                    if !covered.is_empty() {
                        let (done, fresh) = subtract_covered(ranges.clone(), &covered);
                        if !done.is_empty() {
                            let mut omitted_bytes: usize =
                                done.iter().flat_map(|&(s, e)| s..=e).filter_map(|l| lines.line(l).map(|txt| l.to_string().len() + 1 + txt.len() + 1)).sum();
                            if fresh.is_empty() {
                                omitted_bytes += lang.len() + 8;
                            }
                            let spans: Vec<String> = done.iter().map(|(s, e)| format!("{s}-{e}")).collect();
                            if let Some((notice, saved)) = dedup_notice(&format!("lines {} of `{path}`", spans.join(", ")), omitted_bytes, "use that copy.") {
                                meta.dedup_hits += 1;
                                meta.dedup_saved_bytes += saved;
                                covered_note = notice;
                                ranges = fresh;
                            }
                        }
                    }
                }
            }
            let sent = SentFile { fingerprint: fp, ranges: ranges.iter().map(|r| (r.start, r.end)).collect() };
            if ranges.is_empty() {
                if !covered_note.is_empty() && backrefs_only.len() < 6 {
                    let note = format!("**`{path}`**\n\n{covered_note}\n");
                    total += note.len() + 1;
                    backrefs_only.push(note);
                }
                continue;
            }
            let body = window::render(self, grp.file, &lines, &ranges);
            let suffix = if flag.is_empty() { header_syms.join(", ") } else { flag.clone() };
            let head = if suffix.is_empty() { format!("**`{path}`**") } else { format!("**`{path}`** — {suffix}") };
            let mut section = String::new();
            section.push_str(&head);
            section.push_str("\n\n");
            if !covered_note.is_empty() {
                section.push_str(&covered_note);
                section.push_str("\n\n");
            }
            section.push_str(&format!("```{lang}\n{body}\n```\n"));
            total += section.len() + 1;
            let sent_key = if !stale {
                sent_now.insert(path.clone(), sent);
                Some(path.clone())
            } else {
                None
            };
            sections.push((sent_key, section));
            shown_files.push(path);
        }
        let n_backrefs = backrefs_only.len();
        if !backrefs_only.is_empty() {
            sections.extend(backrefs_only.into_iter().take(6).map(|s| (None, s)));
        }

        // 8. render
        let n_syms = shown_symbols.len();
        let n_files = shown_files.len();
        let mut summary = if n_files == 0 && n_backrefs > 0 {
            format!("Found {} across {} (already sent earlier in this conversation).", plural(n_syms, "symbol"), plural(n_backrefs, "file"))
        } else {
            format!("Found {} across {}.", plural(n_syms, "symbol"), plural(n_files, "file"))
        };
        if !pinned.is_empty() {
            summary.push_str(&format!(" {} pinned from the query.", plural(pinned.len(), "file")));
        }
        if !unresolved_paths.is_empty() {
            let spans: Vec<String> = unresolved_paths.iter().map(|s| format!("`{s}`")).collect();
            summary.push_str(&format!(" No indexed file uniquely matches {}.", spans.join(", ")));
        }
        let mut out: Vec<String> = Vec::new();
        if !stale_files.is_empty() {
            out.push(strings::stale_banner(&stale_files));
            out.push(String::new());
        }
        out.push(format!("**Exploration: {q}**"));
        out.push(String::new());
        out.push(summary);
        out.push(String::new());
        if !unseen.is_empty() {
            unseen.truncate(5);
            out.push(strings::not_in_index_note(&unseen, n_files > 0));
            out.push(String::new());
        }
        if !blast.is_empty() {
            out.push(blast.clone());
        }
        if !relationships.is_empty() {
            out.push(relationships.clone());
        }
        let mut text = String::new();
        // The upstream tool puts the Flow section first in the response.
        text.push_str(&flow_text);
        if !flow_text.is_empty() {
            text.push('\n');
        }
        text.push_str(&out.join("\n"));
        text.push('\n');
        let mut section_ends: Vec<(String, usize)> = Vec::new();
        let append_sections = |text: &mut String, sections: &[(Option<String>, String)], ends: &mut Vec<(String, usize)>| {
            for (idx, (key, sec)) in sections.iter().enumerate() {
                if idx > 0 {
                    text.push('\n');
                }
                text.push_str(sec);
                if let Some(p) = key {
                    ends.push((p.clone(), text.len()));
                }
            }
        };
        if n_files > 0 {
            if opts.compact {
                text.push_str("**Symbols (compact outline)**\n\n");
                text.push_str(&format!(
                    "> Compact mode omits source bodies. Drop `compact` or call `{}` on a symbol for verbatim code.\n\n",
                    strings::tool("node")
                ));
            } else {
                text.push_str("**Source Code**\n\n");
                text.push_str(strings::VERBATIM_NOTE);
                text.push_str("\n\n");
            }
            append_sections(&mut text, &sections, &mut section_ends);
        } else if !sections.is_empty() {
            append_sections(&mut text, &sections, &mut section_ends);
        }

        // epilogue: files whose source was not shown, completeness and budget notes
        let mut epilogue = String::new();
        if !omitted.is_empty() {
            let mut lines: Vec<String> = vec![String::new(), strings::POINTER_HEADER.to_owned(), String::new()];
            for (f, nodes) in omitted.iter().take(10) {
                let mut names: Vec<String> = nodes
                    .iter()
                    .copied()
                    .filter(|n| is_listed(self.kind(*n)) && !matches!(self.kind(*n), NodeKind::Property | NodeKind::Field))
                    .take(4)
                    .map(|n| self.name(n).to_owned())
                    .collect();
                names.dedup();
                if names.is_empty() {
                    continue;
                }
                lines.push(format!("- `{}` — {}", self.path(*f), names.join(", ")));
            }
            if omitted.len() > 10 {
                lines.push(format!("- … and {} more files", omitted.len() - 10));
            }
            if lines.len() > 3 {
                epilogue.push_str(&lines.join("\n"));
                epilogue.push('\n');
            }
        }
        if trimmed_any {
            epilogue.push('\n');
            epilogue.push_str(&strings::trimmed_sections_note());
            epilogue.push('\n');
        }
        if budget.extras && !opts.compact {
            epilogue.push('\n');
            epilogue.push_str(&strings::budget_note(self.indexed_files(), budget.calls));
            epilogue.push('\n');
        }
        if text.len() + epilogue.len() <= hard {
            text.push_str(&epilogue);
        } else if text.len() + strings::cut_note(trimmed_any).len() <= hard {
            text.push_str(&strings::cut_note(trimmed_any));
        }
        let mut cut_boundary: Option<usize> = None;
        if text.len() > hard {
            let note = strings::truncated_note(trimmed_any);
            // Reserve space for the truncation note so the final output stays strictly within `hard`.
            let mut cut = hard.saturating_sub(note.len()).min(text.len());
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let head = &text[..cut];
            let boundary = head.rfind("\n**`").filter(|b| *b > cut / 2).or_else(|| head.rfind('\n')).unwrap_or(cut);
            text.truncate(boundary);
            text.push_str(&note);
            cut_boundary = Some(boundary);
        }
        if let Some(h) = history.as_mut() {
            if let Some(boundary) = cut_boundary {
                // Only record files whose ENTIRE rendered section survived truncation.
                let intact: FxHashSet<&str> = section_ends.iter().filter(|(_, end)| *end <= boundary).map(|(p, _)| p.as_str()).collect();
                sent_now.retain(|p, _| intact.contains(p.as_str()));
            }
            h.record(sent_now);
        }
        ExploreResult { text, files: shown_files, symbols: n_syms, found_named: !exact.is_empty() || !pinned.is_empty(), meta }
    }

    /// Blast radius of the named symbols: who depends on them and which tests cover them.
    fn blast_radius(&self, exact: &FxHashSet<NodeIdx>, named: &[NamedTok]) -> String {
        let mut roots: Vec<NodeIdx> = Vec::new();
        for nt in named {
            for n in &nt.nodes {
                if exact.contains(n) && !roots.contains(n) {
                    roots.push(*n);
                }
            }
        }
        let meaningful = |k: NodeKind| {
            matches!(
                k,
                NodeKind::Function
                    | NodeKind::Method
                    | NodeKind::Class
                    | NodeKind::Interface
                    | NodeKind::Struct
                    | NodeKind::Union
                    | NodeKind::Trait
                    | NodeKind::Protocol
                    | NodeKind::Enum
                    | NodeKind::TypeAlias
                    | NodeKind::Component
                    | NodeKind::Constant
                    | NodeKind::Variable
                    | NodeKind::Property
                    | NodeKind::Field
            )
        };
        roots.retain(|n| meaningful(self.kind(*n)));
        roots.truncate(5);
        let mut entries: Vec<String> = Vec::new();
        for r in roots {
            let callers = self.callers(r, 1, 500);
            if callers.is_empty() {
                continue;
            }
            let mut files: Vec<FileId> = callers.iter().map(|c| self.view.hot(c.node).file).collect();
            files.sort_by(|a, b| self.path(*a).cmp(self.path(*b)));
            files.dedup();
            let (tests, code): (Vec<FileId>, Vec<FileId>) = files.into_iter().partition(|f| self.file_flags(*f) & fileflags::TEST != 0);
            let shown = code.iter().take(4).map(|f| format!("`{}`", self.path(*f))).collect::<Vec<_>>().join(", ");
            let more = if code.len() > 4 { format!(" +{} more", code.len() - 4) } else { String::new() };
            let where_ = if code.is_empty() { String::new() } else { format!(" in {shown}{more}") };
            let tests_s = if tests.is_empty() {
                String::new()
            } else {
                format!(
                    "; tests: {}{}",
                    tests.iter().take(4).map(|f| format!("`{}`", self.path(*f))).collect::<Vec<_>>().join(", "),
                    if tests.len() > 4 { format!(" +{}", tests.len() - 4) } else { String::new() }
                )
            };
            entries.push(format!(
                "- `{}` ({}:{}) — {} caller{}{where_}{tests_s}",
                self.name(r),
                self.node_path(r),
                self.line(r),
                callers.len(),
                if callers.len() == 1 { "" } else { "s" }
            ));
        }
        if entries.is_empty() {
            return String::new();
        }
        format!("{}\n\n{}\n", strings::blast_radius_header(), entries.join("\n"))
    }

    /// Edges among the subgraph's nodes, grouped by kind (large projects only).
    fn relationships(&self, rel: &FxHashMap<NodeIdx, f32>) -> String {
        let mut by_kind: std::collections::BTreeMap<&'static str, Vec<(String, String)>> = std::collections::BTreeMap::new();
        let mut nodes: Vec<NodeIdx> = rel.keys().copied().collect();
        nodes.sort_by_key(|n| n.0);
        for n in nodes {
            for e in self.view.out_edges(n) {
                if e.kind == EdgeKind::Contains || !rel.contains_key(&e.dst) {
                    continue;
                }
                by_kind.entry(e.kind.as_str()).or_default().push((self.name(n).to_owned(), self.name(e.dst).to_owned()));
            }
        }
        if by_kind.is_empty() {
            return String::new();
        }
        let cap = 12;
        let mut lines: Vec<String> = vec!["**Relationships**".into(), String::new()];
        for (kind, mut edges) in by_kind {
            edges.sort();
            edges.dedup();
            lines.push(format!("**{kind}:**"));
            for (a, b) in edges.iter().take(cap) {
                lines.push(format!("- {a} → {b}"));
            }
            if edges.len() > cap {
                lines.push(format!("- ... and {} more", edges.len() - cap));
            }
            lines.push(String::new());
        }
        lines.join("\n")
    }
}

fn subtract_covered(ranges: Vec<window::Range>, covered: &[(u32, u32)]) -> (Vec<(u32, u32)>, Vec<window::Range>) {
    let mut done: Vec<(u32, u32)> = Vec::new();
    let mut fresh: Vec<window::Range> = Vec::new();
    for r in ranges {
        let mut cur = r.start;
        for &(cs, ce) in covered {
            if ce < cur {
                continue;
            }
            if cs > r.end {
                break;
            }
            if cs > cur {
                fresh.push(window::Range { start: cur, end: cs - 1, imp: r.imp, must: r.must });
            }
            let ov_s = cur.max(cs);
            let ov_e = r.end.min(ce);
            done.push((ov_s, ov_e));
            cur = ov_e.saturating_add(1);
            if cur > r.end {
                break;
            }
        }
        if cur <= r.end {
            fresh.push(window::Range { start: cur, end: r.end, imp: r.imp, must: r.must });
        }
    }
    (done, fresh)
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}
