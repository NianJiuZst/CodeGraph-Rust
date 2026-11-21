//! Retrieval (design doc 8.2): the query is cut into words, each word is looked up through
//! several independent routes, and a node keeps the best score any route gave it.

use crate::graph::Graph;
use cg_core::{FileId, NodeIdx, NodeKind, StrId};
use cg_store::index::{F_DOC, F_NAME, F_PATH, F_QNAME, F_SIG};
use cg_store::tokenize::{is_stopword, looks_like_code, split_identifier};
use cg_store::Seg;
use fst::automaton::{Automaton, Levenshtein, Str};
use fst::{IntoStreamer, Streamer};
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Clone, Debug)]
pub struct SearchOptions {
    pub limit: usize,
    /// Restrict to these kinds (empty = any definition).
    pub kinds: Vec<NodeKind>,
    /// Allow file nodes among the results.
    pub include_files: bool,
    /// Allow fuzzy and substring routes when the exact routes find little.
    pub fuzzy: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions { limit: 10, kinds: Vec::new(), include_files: false, fuzzy: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub node: NodeIdx,
    pub score: f32,
    /// Number of distinct query words that reached this node.
    pub terms: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokKind {
    /// A plain lower-cased word.
    Plain,
    /// camelCase / snake_case / PascalCase: kept verbatim.
    Code,
    /// `Class.method`, `a::b`, `pkg/Type.method`.
    Qualified,
    /// A path or file name, possibly with a `:line` anchor.
    Path,
}

#[derive(Clone, Debug)]
pub struct QueryToken {
    pub raw: String,
    pub lower: String,
    pub kind: TokKind,
    /// Components of a qualified name.
    pub parts: Vec<String>,
    /// `file.py:776` line anchor.
    pub anchor: Option<u32>,
}

const FILE_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "py", "pyi", "go", "java", "kt", "kts", "rs", "c", "h", "cc", "cpp", "hpp", "cs", "php", "rb",
    "swift", "dart", "scala", "lua", "r", "json", "toml", "yaml", "yml", "md", "vue", "svelte", "gradle", "xml", "sql", "sh",
];

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn has_file_ext(s: &str) -> bool {
    match s.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && FILE_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// Cut a query into tokens. Code-shaped words are kept verbatim, others are lower-cased and
/// stop words dropped. Very long queries are protected (design doc 8.2, #2620): words are
/// de-duplicated and capped, and past 10,000 characters only code-shaped words and the first
/// 200 words survive.
pub fn tokenize_query(q: &str) -> Vec<QueryToken> {
    let long = q.len() > 10_000;
    let separators = |c: char| {
        c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | '`' | '|' | '=' | '+' | '*' | '?' | '!')
    };
    let mut out: Vec<QueryToken> = Vec::new();
    let mut seen: FxHashSet<(String, TokKind)> = FxHashSet::default();
    let mut words = 0usize;
    for piece in q.split(separators) {
        let piece = piece.trim_matches(|c: char| matches!(c, '.' | ':' | '-' | '@' | '$' | '~' | '&' | '#' | '\\'));
        if piece.is_empty() {
            continue;
        }
        words += 1;
        if long && words > 200 && !(looks_like_code(piece) || piece.contains('/')) {
            continue;
        }
        let mut anchor = None;
        let mut text = piece.to_owned();
        // `file.py:776` or `file.py:776-790`
        if let Some((head, tail)) = piece.rsplit_once(':') {
            let first = tail.split('-').next().unwrap_or("");
            if !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) && (has_file_ext(head) || head.contains('/')) {
                anchor = first.parse().ok();
                text = head.to_owned();
            }
        }
        let t = text.replace('#', ".");
        let tok = if t.contains('/') || (has_file_ext(&t) && !t.contains("::")) {
            QueryToken { lower: t.to_lowercase(), raw: t, kind: TokKind::Path, parts: Vec::new(), anchor }
        } else if t.contains("::") || (t.contains('.') && t.split('.').filter(|p| !p.is_empty()).count() >= 2) {
            let parts: Vec<String> = t.split(['.', ':']).filter(|p| !p.is_empty()).map(|p| p.to_owned()).collect();
            if parts.len() < 2
                || !parts.iter().all(|p| p.chars().all(is_ident_char) && p.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$'))
                || parts.iter().all(|p| p.len() < 2)
            {
                // prose such as "e.g." or "v1.2": treat the pieces as words
                for p in parts {
                    push_plain(&p, &mut out, &mut seen);
                }
                continue;
            }
            QueryToken { lower: t.to_lowercase(), raw: t, kind: TokKind::Qualified, parts, anchor }
        } else if looks_like_code(&t) {
            QueryToken { lower: t.to_lowercase(), raw: t, kind: TokKind::Code, parts: Vec::new(), anchor }
        } else {
            push_plain(&t, &mut out, &mut seen);
            continue;
        };
        if seen.insert((tok.lower.clone(), tok.kind)) {
            out.push(tok);
        }
    }
    out
}

fn push_plain(word: &str, out: &mut Vec<QueryToken>, seen: &mut FxHashSet<(String, TokKind)>) {
    let w: String = word.chars().filter(|c| is_ident_char(*c)).collect();
    if w.is_empty() {
        return;
    }
    // A bare identifier-like word with inner capitals / underscores is code-shaped.
    if looks_like_code(&w) {
        let lower = w.to_lowercase();
        if seen.insert((lower.clone(), TokKind::Code)) {
            out.push(QueryToken { raw: w, lower, kind: TokKind::Code, parts: Vec::new(), anchor: None });
        }
        return;
    }
    let lower = w.to_lowercase();
    if lower.len() < 2 || is_stopword(&lower) {
        return;
    }
    if seen.insert((lower.clone(), TokKind::Plain)) {
        out.push(QueryToken { raw: w, lower, kind: TokKind::Plain, parts: Vec::new(), anchor: None });
    }
}

#[derive(Clone, Copy, Default)]
struct Cand {
    score: f32,
    mask: u64,
}

/// Per-search accumulator: best score per node and bitmask of tokens that reached it.
struct Acc {
    best: FxHashMap<NodeIdx, Cand>,
    /// Bitmask of token slots that already achieved a strong exact/case-insensitive hit (>= 85).
    exact_slots: u64,
}

impl Acc {
    fn new() -> Acc {
        Acc { best: FxHashMap::default(), exact_slots: 0 }
    }
    #[inline]
    fn add(&mut self, tok: usize, n: NodeIdx, score: f32) {
        let bit = 1u64 << tok.min(63);
        let c = self.best.entry(n).or_default();
        if score > c.score {
            c.score = score;
        }
        c.mask |= bit;
        if score >= 85.0 {
            self.exact_slots |= bit;
        }
    }
}

fn norm_qname(s: &str) -> String {
    s.replace(['.', '/', '#'], "::")
}

impl Graph {
    #[inline]
    fn for_each_layer(&self, mut f: impl FnMut(&Seg, bool)) {
        f(&self.view.base, false);
        if let Some(d) = &self.view.delta {
            f(&d.seg, true);
        }
    }

    fn layers(&self) -> smallvec::SmallVec<[(&Seg, bool); 2]> {
        let mut v = smallvec::SmallVec::new();
        v.push((&*self.view.base, false));
        if let Some(d) = &self.view.delta {
            v.push((&*d.seg, true));
        }
        v
    }

    #[inline]
    fn live(&self, n: u32, delta_layer: bool) -> bool {
        delta_layer || self.view.is_alive(NodeIdx(n))
    }

    fn nodes_of_name_in(&self, seg: &Seg, delta_layer: bool, id: StrId, mut f: impl FnMut(NodeIdx)) {
        let off = &seg.p.name_post_off;
        if id.idx() + 1 >= off.len() {
            return;
        }
        for &n in &seg.p.name_post[off[id.idx()] as usize..off[id.idx() + 1] as usize] {
            if self.live(n, delta_layer) {
                f(NodeIdx(n));
            }
        }
    }

    /// Names (per layer StrIds) whose lower-cased form equals `lower`.
    fn lower_names_slice<'a>(&self, seg: &'a Seg, lower: &str) -> &'a [u32] {
        let (Some(fst), off, post) = (&seg.lower_fst, &seg.p.lower_post_off, &seg.p.lower_post) else { return &[] };
        match fst.get(lower) {
            Some(i) if (i as usize) + 1 < off.len() => &post[off[i as usize] as usize..off[i as usize + 1] as usize],
            _ => &[],
        }
    }

    fn segment_names_slice<'a>(&self, seg: &'a Seg, segment: &str) -> &'a [u32] {
        let (Some(fst), off, post) = (&seg.segs_fst, &seg.p.seg_post_off, &seg.p.seg_post) else { return &[] };
        match fst.get(segment) {
            Some(i) if (i as usize) + 1 < off.len() => &post[off[i as usize] as usize..off[i as usize + 1] as usize],
            _ => &[],
        }
    }

    fn name_str_in<'a>(&self, seg: &'a Seg, id: StrId) -> &'a str {
        seg.names.get(id)
    }

    /// All definition nodes whose simple name is exactly `name` (case-sensitive).
    pub fn nodes_named(&self, name: &str) -> Vec<NodeIdx> {
        self.view.by_name(name).collect()
    }

    /// Definition nodes matching a (possibly qualified) symbol name, most specific first.
    /// `Class.method`, `Class::method`, `a/b/Class.method` all work.
    pub fn resolve_symbol(&self, symbol: &str) -> Vec<NodeIdx> {
        let sym = symbol.trim();
        if sym.is_empty() {
            return Vec::new();
        }
        let norm = norm_qname(sym);
        let last = norm.rsplit("::").next().unwrap_or(&norm);
        let mut exact: Vec<NodeIdx> = Vec::new();
        let mut suffix: Vec<NodeIdx> = Vec::new();
        let mut loose: Vec<NodeIdx> = Vec::new();
        let multi = norm.contains("::");
        for n in self.view.by_name(last) {
            if !self.is_definition(n) {
                continue;
            }
            if !multi {
                exact.push(n);
                continue;
            }
            let q = self.qname(n).replace(['.', '/', '#'], "::");
            if q == norm {
                exact.push(n);
            } else if q.ends_with(&format!("::{norm}")) {
                suffix.push(n);
            } else if q.to_lowercase().ends_with(&format!("::{}", norm.to_lowercase())) || q.eq_ignore_ascii_case(&norm) {
                loose.push(n);
            }
        }
        // a qualified query naming a container that holds the member as a field/nested definition
        if exact.is_empty() && suffix.is_empty() && loose.is_empty() && multi {
            for n in self.view.by_qname(&norm) {
                exact.push(n);
            }
        }
        exact.extend(suffix);
        exact.extend(loose);
        // Case-insensitive fallback for a simple name.
        if exact.is_empty() && !multi {
            let lower = sym.to_lowercase();
            self.for_each_layer(|seg, d| {
                for &id in self.lower_names_slice(seg, &lower) {
                    self.nodes_of_name_in(seg, d, StrId(id), |n| {
                        if self.is_definition(n) {
                            exact.push(n);
                        }
                    });
                }
            });
        }
        exact.sort_by_key(|n| (self.node_path(*n).to_owned(), self.line(*n)));
        exact.dedup();
        exact
    }

    /// File ids whose path equals `p`, ends with `/p`, or whose basename is `p`.
    pub fn files_matching(&self, p: &str) -> Vec<FileId> {
        let p = p.trim_start_matches("./").trim_start_matches('/');
        if p.is_empty() {
            return Vec::new();
        }
        if let Some(f) = self.view.file_by_path(p) {
            return vec![f];
        }
        let mut out: Vec<FileId> = Vec::new();
        let suffix = format!("/{p}");
        if p.contains('/') {
            // suffix scan over files (bounded by the file count)
            for f in self.view.files() {
                if self.path(f).ends_with(&suffix) {
                    out.push(f);
                }
            }
            return out;
        }
        for (seg, _) in self.layers() {
            if let Some(id) = seg.names.find(p.as_bytes()) {
                let recs = &seg.p.fname_idx;
                let lo = recs.partition_point(|r| r.name.0 < id.0);
                let mut i = lo;
                while i < recs.len() && recs[i].name == id {
                    let f = recs[i].file;
                    if !self.view.file_removed(f) {
                        out.push(f);
                    }
                    i += 1;
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Full retrieval pipeline.
    pub fn search(&self, query: &str, opts: &SearchOptions) -> Vec<Hit> {
        let toks = tokenize_query(query);
        self.search_tokens(&toks, opts)
    }

    fn rank_acc(&self, acc: &Acc, n_tok: usize, opts: &SearchOptions) -> Vec<Hit> {
        let mut hits: Vec<Hit> = Vec::with_capacity(acc.best.len().min(256));
        for (&n, &c) in acc.best.iter() {
            let kind = self.kind(n);
            if kind == NodeKind::File {
                if !opts.include_files {
                    continue;
                }
            } else if !kind.is_definition() {
                continue;
            }
            if !opts.kinds.is_empty() && !opts.kinds.contains(&kind) {
                continue;
            }
            let reached = c.mask.count_ones() as u8;
            let mut score = c.score;
            if reached > 1 && n_tok > 1 {
                score *= 1.0 + 0.15 * (reached as f32 - 1.0).min(4.0);
            }
            score *= self.file_weight(self.view.hot(n).file);
            score *= kind_weight(kind);
            hits.push(Hit { node: n, score, terms: reached });
        }
        let cmp = |a: &Hit, b: &Hit| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.name(a.node).len().cmp(&self.name(b.node).len()))
                .then_with(|| self.node_path(a.node).cmp(self.node_path(b.node)))
                .then_with(|| self.line(a.node).cmp(&self.line(b.node)))
        };
        if hits.len() > opts.limit && opts.limit > 0 {
            hits.select_nth_unstable_by(opts.limit, cmp);
            hits.truncate(opts.limit);
        }
        hits.sort_by(cmp);
        hits.truncate(opts.limit);
        hits
    }

    /// Single-pass retrieval for `explore`: collects each query token ONCE, then scores both the
    /// combined multi-token query (weight 0.9) and each individual word (weight 0.8) without
    /// repeating FST, segment, BM25 or fuzzy scans.
    pub(crate) fn search_for_explore(&self, toks: &[QueryToken], combined_limit: usize, per_word_limit: usize) -> FxHashMap<NodeIdx, f32> {
        if toks.is_empty() {
            return FxHashMap::default();
        }
        let mut order: Vec<usize> = (0..toks.len()).collect();
        if toks.len() > 64 {
            order.sort_by_key(|i| self.token_df(&toks[*i]));
            order.truncate(64);
            order.sort();
        }
        let mut combined = Acc::new();
        let mut per_tok: Vec<Acc> = Vec::with_capacity(order.len());
        let opts_word = SearchOptions { limit: per_word_limit, ..Default::default() };
        let opts_all = SearchOptions { limit: combined_limit, ..Default::default() };
        for (slot, &ti) in order.iter().enumerate() {
            let mut single = Acc::new();
            self.collect(&toks[ti], 0, &mut single);
            // Run fuzzy only when this token has no exact/case-insensitive hit and few strong hits.
            if single.exact_slots == 0 && single.best.values().filter(|c| c.score >= 60.0).count() < per_word_limit.max(5) {
                self.collect_fuzzy(&toks[ti], 0, &mut single);
            }
            let bit = 1u64 << slot.min(63);
            if single.exact_slots != 0 {
                combined.exact_slots |= bit;
            }
            for (&n, &c) in &single.best {
                let e = combined.best.entry(n).or_default();
                if c.score > e.score {
                    e.score = c.score;
                }
                e.mask |= bit;
            }
            per_tok.push(single);
        }
        let mut seeds: FxHashMap<NodeIdx, f32> = FxHashMap::default();
        let mut add_hits = |hits: Vec<Hit>, w: f32| {
            let max = hits.first().map_or(1.0, |h| h.score).max(1.0);
            for h in hits {
                let s = (h.score / max).min(1.0) * w;
                let e = seeds.entry(h.node).or_insert(0.0);
                if s > *e {
                    *e = s;
                }
            }
        };
        add_hits(self.rank_acc(&combined, order.len(), &opts_all), 0.9);
        for single in &per_tok {
            add_hits(self.rank_acc(single, 1, &opts_word), 0.8);
        }
        seeds
    }

    pub fn search_tokens(&self, toks: &[QueryToken], opts: &SearchOptions) -> Vec<Hit> {
        if toks.is_empty() {
            return Vec::new();
        }
        // Rarity ordering and cap (design doc 8.2): at most 64 words, rarest first.
        let mut order: Vec<usize> = (0..toks.len()).collect();
        if toks.len() > 64 {
            order.sort_by_key(|i| self.token_df(&toks[*i]));
            order.truncate(64);
            order.sort();
        }
        let mut acc = Acc::new();
        for (slot, &ti) in order.iter().enumerate() {
            self.collect(&toks[ti], slot, &mut acc);
        }
        if opts.fuzzy {
            let strong = acc.best.values().filter(|c| c.score >= 60.0).count();
            if strong < opts.limit.max(5) {
                for (slot, &ti) in order.iter().enumerate() {
                    // Skip expensive fuzzy/Levenshtein scans for tokens that already matched exactly.
                    if acc.exact_slots & (1u64 << slot.min(63)) != 0 {
                        continue;
                    }
                    self.collect_fuzzy(&toks[ti], slot, &mut acc);
                }
            }
        }
        self.rank_acc(&acc, order.len(), opts)
    }

    /// Document frequency of a token (how many names/nodes it reaches); used to rank rarity.
    fn token_df(&self, t: &QueryToken) -> usize {
        let mut df = 0;
        for (seg, _) in self.layers() {
            if let Some(f) = &seg.tokens_fst {
                if let Some(r) = f.get(&t.lower) {
                    df += seg.p.token_df.get(r as usize).copied().unwrap_or(0) as usize;
                }
            }
        }
        df
    }

    fn collect(&self, t: &QueryToken, slot: usize, acc: &mut Acc) {
        match t.kind {
            TokKind::Path => {
                for f in self.files_matching(&t.raw) {
                    acc.add(slot, self.file_node(f), 95.0);
                }
                // `path/to/file.ts` also names its basename's symbols weakly
                if let Some(base) = t.raw.rsplit('/').next() {
                    let stem = base.rsplit_once('.').map_or(base, |x| x.0);
                    self.name_routes(stem, slot, acc, 0.5);
                }
            }
            TokKind::Qualified => {
                let full = norm_qname(&t.raw);
                let last = t.parts.last().map(|s| s.as_str()).unwrap_or("");
                for n in self.view.by_name(last) {
                    let q = self.qname(n).replace(['.', '/', '#'], "::");
                    if q == full {
                        acc.add(slot, n, 110.0);
                    } else if q.ends_with(&format!("::{full}")) {
                        acc.add(slot, n, 105.0);
                    } else if t.parts.len() >= 2 {
                        let owner = &t.parts[t.parts.len() - 2];
                        if self.containers(n).first().is_some_and(|c| self.name(*c).eq_ignore_ascii_case(owner)) {
                            acc.add(slot, n, 100.0);
                        } else {
                            acc.add(slot, n, 50.0);
                        }
                    }
                }
                for n in self.view.by_qname(&full) {
                    acc.add(slot, n, 110.0);
                }
                // the last part as an ordinary word, weakly
                if acc.best.is_empty() {
                    self.name_routes(last, slot, acc, 0.6);
                }
            }
            TokKind::Code | TokKind::Plain => {
                self.name_routes(&t.raw, slot, acc, 1.0);
                self.bm25(t, slot, acc);
                // file named after the word (`Cache` -> cache.rs / Cache.java)
                for cand in [t.raw.clone(), capitalize(&t.raw), t.lower.clone()] {
                    for f in self.files_matching(&cand) {
                        if !cand.contains('/') && self.path(f).rsplit('/').next().is_some_and(|b| b == cand || b.rsplit_once('.').is_some_and(|x| x.0 == cand))
                        {
                            acc.add(slot, self.file_node(f), if t.kind == TokKind::Code { 80.0 } else { 55.0 });
                        }
                    }
                }
            }
        }
    }

    /// Routes keyed by a name: exact, case-insensitive, identifier segments, class prefixes.
    fn name_routes(&self, word: &str, slot: usize, acc: &mut Acc, scale: f32) {
        if word.is_empty() {
            return;
        }
        let lower = word.to_lowercase();
        // 1. exact (case-sensitive)
        for n in self.view.by_name(word) {
            acc.add(slot, n, 100.0 * scale);
        }
        // 2. case-insensitive
        self.for_each_layer(|seg, d| {
            for &id in self.lower_names_slice(seg, &lower) {
                let sid = StrId(id);
                if self.name_str_in(seg, sid) != word {
                    self.nodes_of_name_in(seg, d, sid, |n| acc.add(slot, n, 85.0 * scale));
                }
            }
        });
        // 3. identifier segments: every segment of the word must occur in the name
        let mut segs: Vec<String> = Vec::new();
        split_identifier(word, &mut segs);
        segs.dedup();
        if segs.is_empty() {
            segs.push(lower.clone());
        }
        let mut ns: Vec<String> = Vec::new();
        self.for_each_layer(|seg, d| {
            let mut best: Option<&[u32]> = None;
            for s in &segs {
                let names = self.segment_names_slice(seg, s);
                best = Some(match best {
                    None => names,
                    Some(prev) if prev.len() <= names.len() => prev,
                    Some(_) => names,
                });
            }
            let Some(raw_cands) = best else { return };
            if raw_cands.is_empty() {
                return;
            }
            let mut cands: Vec<u32> = raw_cands.to_vec();
            if cands.len() > 400 {
                cands.select_nth_unstable_by_key(400, |&id| self.name_str_in(seg, StrId(id)).len());
                cands.truncate(400);
            }
            cands.sort_unstable_by_key(|&id| self.name_str_in(seg, StrId(id)).len());
            for id in cands {
                let sid = StrId(id);
                let name = self.name_str_in(seg, sid);
                ns.clear();
                split_identifier(name, &mut ns);
                if !segs.iter().all(|s| ns.contains(s)) {
                    continue;
                }
                let extra = ns.len().saturating_sub(segs.len());
                let base = if extra == 0 { 75.0 } else { (48.0 - 3.0 * extra as f32).max(30.0) };
                self.nodes_of_name_in(seg, d, sid, |n| acc.add(slot, n, base * scale));
            }
        });
        // 4. definition prefix: class-like names starting with the (capitalised / stemmed) word
        if word.chars().next().is_some_and(|c| c.is_alphabetic()) && lower.len() >= 3 {
            for variant in prefix_variants(&lower) {
                let prefix = capitalize(&variant);
                for (seg, d) in self.layers() {
                    let Some(fst) = &seg.names_fst else { continue };
                    let mut stream = fst.search(Str::new(&prefix).starts_with()).into_stream();
                    let mut taken = 0;
                    while let Some((key, val)) = stream.next() {
                        taken += 1;
                        if taken > 300 {
                            break;
                        }
                        let id = StrId(val as u32);
                        let name_len = key.len();
                        self.nodes_of_name_in(seg, d, id, |n| {
                            let k = self.kind(n);
                            let base = if k.is_type() {
                                58.0
                            } else if k.is_callable() {
                                38.0
                            } else {
                                30.0
                            };
                            let short = 1.0 / (1.0 + 0.02 * name_len.saturating_sub(prefix.len()) as f32);
                            acc.add(slot, n, base * short * scale);
                        });
                    }
                }
            }
        }
    }

    /// BM25 over the token index (design doc 8.2 route 5).
    fn bm25(&self, t: &QueryToken, slot: usize, acc: &mut Acc) {
        let mut words: Vec<String> = Vec::new();
        if t.kind == TokKind::Code {
            split_identifier(&t.raw, &mut words);
            words.push(t.lower.clone());
        } else {
            words.push(t.lower.clone());
        }
        words.sort();
        words.dedup();
        let n_docs = self.view.n_nodes().max(1) as f32;
        let avg = self.graph_avg();
        for w in &words {
            let mut df = 0usize;
            let mut lists: Vec<(&Seg, bool, &[u8], usize)> = Vec::new();
            for (seg, d) in self.layers() {
                let Some(fst) = &seg.tokens_fst else { continue };
                let Some(rank) = fst.get(w) else { continue };
                let off = &seg.p.token_post_off;
                let r = rank as usize;
                if r + 1 >= off.len() {
                    continue;
                }
                let Some(list) = seg.p.token_post.get(off[r] as usize..off[r + 1] as usize) else {
                    continue;
                };
                df += seg.p.token_df.get(r).copied().unwrap_or(0) as usize;
                lists.push((seg, d, list, r));
            }
            if df == 0 {
                continue;
            }
            let idf = (1.0 + (n_docs - df as f32 + 0.5) / (df as f32 + 0.5)).ln();
            let n_base = self.view.base.p.nodes_hot.len() as u32;
            for (seg, d, list, _) in lists {
                for e in cg_store::index::TokenPostings::new(list) {
                    cg_core::cancel::check();
                    if !self.live(e.node, d) {
                        continue;
                    }
                    let w_field = field_weight(e.mask);
                    let norm_len = if d {
                        seg.p.node_norm.get((e.node.saturating_sub(n_base)) as usize).copied().unwrap_or(1) as f32
                    } else {
                        seg.p.node_norm.get(e.node as usize).copied().unwrap_or(1) as f32
                    };
                    let tf = (e.tf as f32) * w_field;
                    let k1 = 1.2f32;
                    let b = 0.75f32;
                    let score = idf * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * norm_len.max(1.0) / avg));
                    // squash into [0, 70)
                    let s = 70.0 * score / (score + 4.0);
                    acc.add(slot, NodeIdx(e.node), s);
                }
            }
        }
    }

    fn graph_avg(&self) -> f32 {
        self.avg_node_len()
    }

    /// Fuzzy (edit distance) and substring routes, used only when the others found little.
    fn collect_fuzzy(&self, t: &QueryToken, slot: usize, acc: &mut Acc) {
        let word = match t.kind {
            TokKind::Qualified => t.parts.last().cloned().unwrap_or_default(),
            TokKind::Path => return,
            _ => t.raw.clone(),
        };
        if word.chars().count() < 4 {
            return;
        }
        let lower = word.to_lowercase();
        let char_len = lower.chars().count();
        if (5..=24).contains(&char_len) {
            let dist = if (8..=16).contains(&char_len) { 2 } else { 1 };
            if let Ok(lev) = Levenshtein::new(&lower, dist) {
                for (seg, d) in self.layers() {
                    let Some(fst) = &seg.lower_fst else { continue };
                    let (off, post) = (&seg.p.lower_post_off, &seg.p.lower_post);
                    let mut stream = fst.search(&lev).into_stream();
                    let mut taken = 0;
                    while let Some((key, val)) = stream.next() {
                        taken += 1;
                        if taken > 200 {
                            break;
                        }
                        let i = val as usize;
                        if i + 1 >= off.len() || key == lower.as_bytes() {
                            continue;
                        }
                        for id in &post[off[i] as usize..off[i + 1] as usize] {
                            self.nodes_of_name_in(seg, d, StrId(*id), |n| acc.add(slot, n, 34.0));
                        }
                    }
                }
            }
        }
        // substring through the trigram index
        if lower.len() >= 3 && lower.is_ascii() {
            let tris: Vec<u32> = lower.as_bytes().windows(3).map(|w| (w[0] as u32) << 16 | (w[1] as u32) << 8 | w[2] as u32).collect();
            for (seg, d) in self.layers() {
                let mut cand: Option<Vec<u32>> = None;
                for tri in &tris {
                    let Ok(pos) = seg.p.tri_keys.binary_search(tri) else {
                        cand = Some(Vec::new());
                        break;
                    };
                    let list = &seg.p.tri_post[seg.p.tri_off[pos] as usize..seg.p.tri_off[pos + 1] as usize];
                    cand = Some(match cand {
                        None => list.to_vec(),
                        Some(prev) => intersect_sorted(&prev, list),
                    });
                    if cand.as_ref().is_some_and(|c| c.is_empty()) {
                        break;
                    }
                }
                for id in cand.unwrap_or_default().into_iter().take(300) {
                    let name = seg.names.get(StrId(id));
                    if name.to_lowercase().contains(&lower) {
                        self.nodes_of_name_in(seg, d, StrId(id), |n| acc.add(slot, n, 33.0));
                    }
                }
            }
        }
    }

    /// Names (strings) in the index close to `word`, for did-you-mean (design doc 10.4).
    pub fn suggest_names(&self, word: &str, limit: usize) -> Vec<String> {
        let lower = word.to_lowercase();
        let mut out: Vec<(usize, String)> = Vec::new();
        let dist = if lower.chars().count() >= 8 { 2 } else { 1 };
        if let Ok(lev) = Levenshtein::new(&lower, dist) {
            for (seg, _) in self.layers() {
                let Some(fst) = &seg.lower_fst else { continue };
                let (off, post) = (&seg.p.lower_post_off, &seg.p.lower_post);
                let mut stream = fst.search(&lev).into_stream();
                while let Some((_, val)) = stream.next() {
                    let i = val as usize;
                    if i + 1 >= off.len() {
                        continue;
                    }
                    for id in &post[off[i] as usize..off[i + 1] as usize] {
                        let n = seg.names.get(StrId(*id)).to_owned();
                        out.push((edit_distance(&n.to_lowercase(), &lower), n));
                    }
                    if out.len() > 200 {
                        break;
                    }
                }
            }
        }
        if out.len() < limit {
            for (seg, _) in self.layers() {
                let Some(fst) = &seg.names_fst else { continue };
                let prefix: String = word.chars().take(4).collect();
                let mut stream = fst.search(Str::new(&prefix).starts_with()).into_stream();
                let mut n = 0;
                while let Some((key, _)) = stream.next() {
                    n += 1;
                    if n > 60 {
                        break;
                    }
                    let k = String::from_utf8_lossy(key).into_owned();
                    out.push((edit_distance(&k.to_lowercase(), &lower) + 2, k));
                }
            }
        }
        out.sort();
        out.dedup_by(|a, b| a.1 == b.1);
        out.into_iter().map(|x| x.1).take(limit).collect()
    }
}

fn kind_weight(k: NodeKind) -> f32 {
    match k {
        NodeKind::Class
        | NodeKind::Interface
        | NodeKind::Struct
        | NodeKind::Trait
        | NodeKind::Function
        | NodeKind::Method
        | NodeKind::Route
        | NodeKind::Component
        | NodeKind::Enum => 1.0,
        NodeKind::File => 0.9,
        NodeKind::Module | NodeKind::Namespace | NodeKind::TypeAlias | NodeKind::Protocol | NodeKind::Union => 0.95,
        NodeKind::Constant => 0.85,
        _ => 0.75,
    }
}

fn field_weight(mask: u8) -> f32 {
    let mut w = 0.0f32;
    if mask & F_NAME != 0 {
        w = w.max(4.0);
    }
    if mask & F_QNAME != 0 {
        w = w.max(2.0);
    }
    if mask & F_SIG != 0 {
        w = w.max(1.5);
    }
    if mask & (F_PATH | F_DOC) != 0 {
        w = w.max(1.0);
    }
    w.max(0.5)
}

pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// `caching` -> caching, cach, cache; `routes` -> routes, route.
fn prefix_variants(lower: &str) -> Vec<String> {
    let mut v = vec![lower.to_owned()];
    for suf in ["ing", "ed", "es", "s", "er", "ion"] {
        if let Some(stem) = lower.strip_suffix(suf) {
            if stem.len() >= 3 {
                v.push(stem.to_owned());
                if matches!(suf, "ing" | "ed" | "er" | "ion") {
                    v.push(format!("{stem}e"));
                }
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_classifies_words() {
        let t = tokenize_query("How does AuthService.loginUser handle session_manager in src/auth/session.ts:42?");
        let kinds: Vec<(String, TokKind)> = t.iter().map(|x| (x.raw.clone(), x.kind)).collect();
        assert!(kinds.contains(&("AuthService.loginUser".into(), TokKind::Qualified)), "{kinds:?}");
        assert!(kinds.contains(&("session_manager".into(), TokKind::Code)), "{kinds:?}");
        let p = t.iter().find(|x| x.kind == TokKind::Path).unwrap();
        assert_eq!(p.raw, "src/auth/session.ts");
        assert_eq!(p.anchor, Some(42));
        assert!(kinds.contains(&("handle".into(), TokKind::Plain)));
        assert!(!kinds.iter().any(|(r, _)| r == "how" || r == "does" || r == "in"));
    }

    #[test]
    fn prose_dots_are_not_qualified_names() {
        let t = tokenize_query("e.g. caching, i.e. v1.2");
        assert!(t.iter().all(|x| x.kind != TokKind::Qualified), "{t:?}");
        assert!(t.iter().any(|x| x.lower == "caching"));
    }

    #[test]
    fn long_queries_are_capped() {
        let q = (0..5000).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ");
        let t = tokenize_query(&q);
        assert!(t.len() <= 5000);
        assert!(!t.is_empty());
    }

    #[test]
    fn variants_and_distance() {
        assert!(prefix_variants("caching").contains(&"cache".to_string()));
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }
}
