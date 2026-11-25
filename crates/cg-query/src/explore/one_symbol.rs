//! One-symbol focused explore mode (upstream #3407).
//!
//! When an explore query asks about a single symbol (e.g. `MODELS_MODULE_NAME`,
//! `toValidURL callers`, `where is Engine.ServeHTTP defined?`), return the
//! definition's verbatim source plus every dependent site with enclosing symbol
//! and line text instead of filling the full budget with loosely related files.

use crate::explore::{ExploreOptions, ExploreResult, History, SentFile};
use crate::graph::Graph;
use crate::source::SourceView;
use cg_core::{EdgeKind, FileId, Lang, NodeIdx, NodeKind};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::fmt::Write as _;

const MAX_ONE_SYMBOL_DEFS: usize = 3;
const MAX_SITE_TEXT: usize = 160;
const MAX_LINES_PER_TEST: usize = 5;

const QUESTION_WORDS: &[&str] = &[
    // relations
    "caller",
    "callers",
    "callee",
    "callees",
    "call",
    "calls",
    "called",
    "calling",
    "use",
    "uses",
    "used",
    "usage",
    "usages",
    "using",
    "user",
    "users",
    "reference",
    "references",
    "referenced",
    "referencing",
    "ref",
    "refs",
    "dependent",
    "dependents",
    "depend",
    "depends",
    "dependency",
    "dependencies",
    "impact",
    "blast",
    "radius",
    "affect",
    "affects",
    "affected",
    "break",
    "breaks",
    "definition",
    "defined",
    "define",
    "defines",
    "declaration",
    "declared",
    "declare",
    "declares",
    "implementation",
    "implementations",
    "implemented",
    "implementer",
    "implementers",
    "subclass",
    "subclasses",
    "override",
    "overrides",
    "overridden",
    "instantiated",
    "instantiation",
    "instantiations",
    "instances",
    "instance",
    "import",
    "imports",
    "imported",
    "importer",
    "importers",
    "value",
    "type",
    "signature",
    "source",
    "body",
    "code",
    "site",
    "sites",
    "location",
    "locations",
    "place",
    "places",
    "occurrences",
    // scaffolding
    "what",
    "where",
    "who",
    "which",
    "how",
    "is",
    "are",
    "was",
    "the",
    "a",
    "an",
    "of",
    "to",
    "in",
    "on",
    "by",
    "for",
    "from",
    "and",
    "or",
    "it",
    "its",
    "this",
    "that",
    "does",
    "do",
    "all",
    "every",
    "any",
    "find",
    "show",
    "list",
    "if",
    "would",
    "when",
    "change",
    "changes",
    "changed",
    "changing",
    "work",
    "works",
];

const PATH_EXTS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".vue", ".svelte", ".py", ".go", ".rs", ".java", ".kt", ".kts", ".c", ".h", ".cc", ".cpp", ".hpp", ".cs",
    ".php", ".rb", ".swift", ".json", ".toml", ".yaml", ".yml", ".md",
];

fn is_question_word(tok: &str) -> bool {
    tok.bytes().all(|b| b.is_ascii_lowercase()) && QUESTION_WORDS.contains(&tok)
}

fn looks_like_file_path(tok: &str) -> bool {
    if tok.contains('/') || tok.contains('\\') {
        return true;
    }
    let lower = tok.to_ascii_lowercase();
    PATH_EXTS.iter().any(|ext| lower.ends_with(ext))
}

fn is_ident_seg(seg: &str, allow_trailing_ruby: bool) -> bool {
    let seg = if allow_trailing_ruby { seg.strip_suffix(['?', '!']).unwrap_or(seg) } else { seg };
    let mut chars = seg.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_alphabetic() || matches!(first, '_' | '$')) {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
}

fn is_symbol_token(tok: &str, had_question_words: bool) -> bool {
    if tok.is_empty() || looks_like_file_path(tok) {
        return false;
    }
    // Validate segments separated by `::` or `.` (no empty segments, no single `:`).
    let mut segs: Vec<&str> = Vec::new();
    for part in tok.split("::") {
        if part.is_empty() || part.contains(':') {
            return false;
        }
        for sub in part.split('.') {
            if sub.is_empty() {
                return false;
            }
            segs.push(sub);
        }
    }
    if segs.is_empty() {
        return false;
    }
    let last_idx = segs.len() - 1;
    if !segs.iter().enumerate().all(|(i, s)| is_ident_seg(s, i == last_idx)) {
        return false;
    }
    let char_len = tok.chars().count();
    if char_len < 2 {
        return false;
    }
    if had_question_words || !tok.is_ascii() {
        return true;
    }
    char_len >= 3 || tok.contains(['_', '.', ':']) || tok.bytes().any(|b| b.is_ascii_uppercase())
}

/// Extract the single symbol name an explore query asks about, or `None` when the
/// query names zero or multiple symbols / content terms.
pub fn one_symbol_query_name(query: &str) -> Option<String> {
    let mut name: Option<String> = None;
    let mut had_qword = false;
    for raw in query.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '?' | '!')) {
        let tok = raw.trim_start_matches(['`', '\'', '"', '(', '[', '{', '<']).trim_end_matches(['`', '\'', '"', ')', ']', '}', '>', '.', ':']);
        let tok = tok.strip_suffix("()").unwrap_or(tok);
        if tok.is_empty() {
            continue;
        }
        if is_question_word(tok) {
            had_qword = true;
            continue;
        }
        if name.is_some() {
            return None;
        }
        name = Some(tok.to_owned());
    }
    let sym = name?;
    is_symbol_token(&sym, had_qword).then_some(sym)
}

fn is_def_kind(k: NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Function
            | NodeKind::Method
            | NodeKind::Component
            | NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Interface
            | NodeKind::Trait
            | NodeKind::Protocol
            | NodeKind::Enum
            | NodeKind::Union
            | NodeKind::TypeAlias
            | NodeKind::Variable
            | NodeKind::Constant
            | NodeKind::Property
            | NodeKind::Field
            | NodeKind::EnumMember
    )
}

/// Resolve `name` to up to `MAX_ONE_SYMBOL_DEFS` canonical definitions (preferring
/// hand-written, non-test definitions when any exist).
pub fn one_symbol_definitions(g: &Graph, name: &str) -> Option<Vec<NodeIdx>> {
    let mut nodes: Vec<NodeIdx> = g.resolve_symbol(name).into_iter().filter(|n| is_def_kind(g.kind(*n))).collect();
    nodes.sort_unstable_by_key(|n| n.0);
    nodes.dedup();
    if nodes.is_empty() {
        return None;
    }
    let hand_written: Vec<NodeIdx> = nodes.iter().copied().filter(|n| !g.is_generated_file(g.view.hot(*n).file)).collect();
    if !hand_written.is_empty() {
        nodes = hand_written;
    }
    let non_test: Vec<NodeIdx> = nodes.iter().copied().filter(|n| !g.is_test_file(g.view.hot(*n).file)).collect();
    if !non_test.is_empty() {
        nodes = non_test;
    }
    if nodes.len() > MAX_ONE_SYMBOL_DEFS {
        return None;
    }
    nodes.sort_by(|a, b| g.node_path(*a).cmp(g.node_path(*b)).then_with(|| g.line(*a).cmp(&g.line(*b))));
    Some(nodes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rel {
    Extends,
    Implements,
    Overrides,
    Instantiates,
    Calls,
    Uses,
    Imports,
    Exports,
}

impl Rel {
    fn from_edge(k: EdgeKind) -> Option<Self> {
        match k {
            EdgeKind::Extends => Some(Rel::Extends),
            EdgeKind::Implements => Some(Rel::Implements),
            EdgeKind::Overrides => Some(Rel::Overrides),
            EdgeKind::Instantiates => Some(Rel::Instantiates),
            EdgeKind::Calls => Some(Rel::Calls),
            EdgeKind::References | EdgeKind::Decorates | EdgeKind::TypeOf | EdgeKind::Returns | EdgeKind::Navigates => Some(Rel::Uses),
            EdgeKind::Imports => Some(Rel::Imports),
            EdgeKind::Exports => Some(Rel::Exports),
            EdgeKind::Contains => None,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Rel::Extends => "Extended by",
            Rel::Implements => "Implemented by",
            Rel::Overrides => "Overridden by",
            Rel::Instantiates => "Instantiated by",
            Rel::Calls => "Called by",
            Rel::Uses => "Used by",
            Rel::Imports => "Imported by",
            Rel::Exports => "Re-exported by",
        }
    }
}

struct Site {
    file: FileId,
    line: u32,
    owner: NodeIdx,
    rel: Rel,
    synth: Option<String>,
}

fn display_name(g: &Graph, n: NodeIdx) -> String {
    let q = g.qname(n);
    let raw = if q.is_empty() { g.name(n) } else { q };
    let lang = g.view.file(g.view.hot(n).file).lang();
    if matches!(lang, Lang::Rust | Lang::Cpp | Lang::C | Lang::Php) {
        raw.to_owned()
    } else {
        raw.replace("::", ".")
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn inline_code(text: &str) -> String {
    let mut longest = 0usize;
    let mut cur = 0usize;
    for b in text.bytes() {
        if b == b'`' {
            cur += 1;
            longest = longest.max(cur);
        } else {
            cur = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') { " " } else { "" };
    format!("{fence}{pad}{text}{pad}{fence}")
}

fn trimmed_line(src: &SourceView, line: u32) -> Option<String> {
    let text = src.line(line)?.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().count() > MAX_SITE_TEXT {
        let mut s: String = text.chars().take(MAX_SITE_TEXT - 1).collect();
        s.push('…');
        Some(s)
    } else {
        Some(text.to_owned())
    }
}

#[derive(Clone, Copy)]
struct Caps {
    sites: usize,
    test_files: usize,
    test_owners: usize,
    callees: usize,
}

const FULL_CAPS: Caps = Caps { sites: 40, test_files: 12, test_owners: 60, callees: 12 };

impl Caps {
    fn halve(self) -> Self {
        Caps {
            sites: (self.sites / 2).max(5),
            test_files: (self.test_files / 2).max(3),
            test_owners: (self.test_owners / 2).max(10),
            callees: (self.callees / 2).max(4),
        }
    }
}

fn collect_sites(g: &Graph, def: NodeIdx) -> Vec<Site> {
    let mut sites: Vec<Site> = Vec::new();
    let mut seen: FxHashSet<(u32, u32, u32, u8)> = FxHashSet::default();
    for e in g.view.in_edges(def) {
        let Some(rel) = Rel::from_edge(e.kind) else { continue };
        if e.src == def && rel == Rel::Uses {
            continue;
        }
        let file = g.view.hot(e.src).file;
        let line = e.line.unwrap_or_else(|| g.line(e.src));
        if !seen.insert((file.0, line, e.src.0, rel as u8)) {
            continue;
        }
        let synth = g.synth_label(&e);
        sites.push(Site { file, line, owner: e.src, rel, synth });
    }
    sites.sort_by(|a, b| g.path(a.file).cmp(g.path(b.file)).then_with(|| a.line.cmp(&b.line)).then_with(|| a.owner.0.cmp(&b.owner.0)));
    sites
}

fn render_body(src: &SourceView, start: u32, end: u32, max_chars: usize) -> (String, u32, bool) {
    let total_end = end.min(src.len() as u32).max(start);
    let mut out = String::new();
    let mut shown_to = start;
    let mut cut = false;
    for l in start..=total_end {
        let Some(line_txt) = src.line(l) else { break };
        let needed = line_txt.len() + 10;
        if !out.is_empty() && out.len() + needed > max_chars {
            cut = true;
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(out, "{l}\t{line_txt}");
        shown_to = l;
    }
    (out, shown_to, cut)
}

/// Try to answer `query` using the focused one-symbol explore path. Returns `None` when
/// the query does not name a single symbol (or names too many definitions / unreadable files),
/// letting the general explore pipeline run.
pub fn try_explore_one_symbol(g: &Graph, query: &str, max_chars: usize, opts: &ExploreOptions) -> Option<ExploreResult> {
    if opts.max_files.is_some() {
        return None;
    }
    let name = one_symbol_query_name(query)?;
    let defs = one_symbol_definitions(g, &name)?;
    let mut sources: FxHashMap<FileId, SourceView> = FxHashMap::default();
    for &d in &defs {
        let f = g.view.hot(d).file;
        if g.file_is_stale(f) {
            return None;
        }
        if !opts.compact {
            if let std::collections::hash_map::Entry::Vacant(e) = sources.entry(f) {
                let abs = g.abs_path(g.path(f))?;
                let sv = SourceView::read(&abs, 1 << 20).ok()?;
                e.insert(sv);
            }
        }
    }
    let collected: Vec<(NodeIdx, Vec<Site>)> = defs.iter().map(|&d| (d, collect_sites(g, d))).collect();
    // Pre-load source lines only for files referenced in the displayed production sites
    // (capped at FULL_CAPS.sites per def and 40 unique files total; stale files skip snippets).
    let site_cap = if collected.len() == 1 { FULL_CAPS.sites } else { FULL_CAPS.halve().sites };
    for (_, sites) in &collected {
        let mut prod_seen = 0usize;
        for s in sites {
            if prod_seen >= site_cap || sources.len() >= 40 {
                break;
            }
            if g.is_test_file(s.file) || matches!(s.rel, Rel::Imports | Rel::Exports) {
                continue;
            }
            prod_seen += 1;
            if !g.file_is_stale(s.file) {
                if let std::collections::hash_map::Entry::Vacant(e) = sources.entry(s.file) {
                    if let Some(abs) = g.abs_path(g.path(s.file)) {
                        if let Ok(sv) = SourceView::read(&abs, 1 << 20) {
                            e.insert(sv);
                        }
                    }
                }
            }
        }
    }

    let mut history = if opts.compact { None } else { opts.history.as_ref().map(|h| h.lock().unwrap_or_else(|e| e.into_inner())) };

    let mut caps = FULL_CAPS;
    let mut body_chars = (max_chars / 2).min(8_000).max(max_chars.min(600));
    for attempt in 0..=4 {
        let cfg = ComposeCfg { caps, body_chars, compact: opts.compact, history: history.as_deref() };
        let (mut text, files, sent_now, meta) = compose(g, &name, &collected, &sources, &cfg);
        if text.len() <= max_chars || attempt == 4 {
            let mut truncated = false;
            if text.len() > max_chars {
                truncated = true;
                let mut cut = max_chars.saturating_sub(42);
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                let at = text[..cut].rfind('\n').unwrap_or(cut);
                text.truncate(at);
                text.push_str("\n… (cut to fit the answer's size limit)");
            }
            if !truncated {
                if let Some(h) = history.as_mut() {
                    h.record(sent_now);
                }
            }
            return Some(ExploreResult { text, files, symbols: defs.len(), found_named: true, meta });
        }
        caps = caps.halve();
        body_chars = (body_chars * 7) / 10;
    }
    unreachable!()
}

struct ComposeCfg<'a> {
    caps: Caps,
    body_chars: usize,
    compact: bool,
    history: Option<&'a History>,
}

fn compose(
    g: &Graph,
    name: &str,
    collected: &[(NodeIdx, Vec<Site>)],
    sources: &FxHashMap<FileId, SourceView>,
    cfg: &ComposeCfg<'_>,
) -> (String, Vec<String>, BTreeMap<String, SentFile>, crate::tools::ToolMeta) {
    let mut out = String::new();
    let mut file_set: FxHashSet<FileId> = FxHashSet::default();
    let mut sent_now: BTreeMap<String, SentFile> = BTreeMap::new();
    let mut meta = crate::tools::ToolMeta::default();
    let per_def_body = (cfg.body_chars / collected.len().max(1)).max(600);
    let per_def_caps = if collected.len() == 1 { cfg.caps } else { cfg.caps.halve() };

    if collected.len() > 1 {
        let _ = writeln!(out, "**{} definitions named `{name}`** — each with every place that depends on it.\n", collected.len());
    }
    for (idx, (def, sites)) in collected.iter().enumerate() {
        if idx > 0 {
            out.push_str("---\n\n");
        }
        let def_file = g.view.hot(*def).file;
        file_set.insert(def_file);
        let def_path = g.path(def_file);
        let start = g.line(*def);
        let end = g.end_line(*def).max(start);
        let span = if end > start { format!("{start}-{end}") } else { format!("{start}") };

        let mut prod_usage: Vec<&Site> = Vec::new();
        let mut prod_entries: Vec<&Site> = Vec::new();
        let mut tests: Vec<&Site> = Vec::new();
        for s in sites {
            let is_entry = matches!(s.rel, Rel::Imports | Rel::Exports);
            if g.is_test_file(s.file) {
                if !is_entry {
                    tests.push(s);
                }
            } else if is_entry {
                prod_entries.push(s);
            } else {
                prod_usage.push(s);
            }
        }
        let prod_files: FxHashSet<FileId> = prod_usage.iter().map(|s| s.file).collect();
        let kind_label = g.kind(*def).as_str().replace('_', " ");
        let summary = if !prod_usage.is_empty() {
            let verb = if prod_usage.len() == 1 { "depends" } else { "depend" };
            let t_tail = if tests.is_empty() { String::new() } else { format!(", plus {} in tests", plural(tests.len(), "site")) };
            format!("{} in {} {verb} on it{t_tail}", plural(prod_usage.len(), "site"), plural(prod_files.len(), "file"))
        } else if !tests.is_empty() {
            format!("no references outside tests; {} in tests", plural(tests.len(), "site"))
        } else {
            "nothing in the index refers to it".to_owned()
        };

        let _ = writeln!(out, "**`{}`** ({kind_label}) — defined at `{def_path}:{start}`; {summary}.\n", display_name(g, *def));

        let lang = g.view.file(def_file).lang().as_str();
        let _ = writeln!(out, "**`{def_path}`** — `{}`, line{} {span}\n", g.name(*def), if end > start { "s" } else { "" });
        if cfg.compact {
            if let Some(sig) = g.view.node(*def).signature() {
                let _ = writeln!(out, "**Signature:** `{}`\n", sig.split_whitespace().collect::<Vec<_>>().join(" "));
            }
            let _ = writeln!(out, "> Compact mode omits source bodies. Drop `compact` or call `node` for verbatim code.\n");
        } else if let Some(sv) = sources.get(&def_file) {
            let (body, shown_to, cut) = render_body(sv, start, end, per_def_body);
            let fenced_len = body.len() + lang.len() + 8;
            let fp = g.fingerprint(def_file);
            let covered = cfg.history.is_some_and(|h| h.is_covered(def_path, fp, start, shown_to));
            if let Some((notice, saved)) =
                covered.then(|| crate::explore::dedup_notice(&format!("lines {start}-{shown_to} of `{def_path}`"), fenced_len, "use that copy.")).flatten()
            {
                meta.dedup_hits += 1;
                meta.dedup_saved_bytes += saved;
                let _ = writeln!(out, "{notice}\n");
            } else {
                let _ = writeln!(out, "```{lang}\n{body}\n```");
                if cut {
                    let _ = writeln!(
                        out,
                        "(lines {start}–{shown_to} of {start}–{end} shown — pass `node` with `file: \"{def_path}\"` and `offset: {}` for the rest)",
                        shown_to + 1
                    );
                }
                out.push('\n');
                if !covered {
                    let entry = sent_now.entry(def_path.to_owned()).or_insert_with(|| SentFile { fingerprint: fp, ranges: Vec::new() });
                    entry.ranges.push((start, shown_to));
                }
            }
        }

        if prod_usage.is_empty() && tests.is_empty() {
            let unresolved = g.view.unresolved_refs_by_tail(g.name(*def), cg_store::UnresolvedScope::Dependency, 5);
            meta.record_tail_idx(&unresolved);
            let hint = crate::tools::format_unresolved_hint(g, g.name(*def), "references", &unresolved);
            if !hint.is_empty() {
                let _ = writeln!(out, "{hint}\n");
            }
        }

        // Production sites grouped by relation
        let listed = &prod_usage[..prod_usage.len().min(per_def_caps.sites)];
        for rel in [Rel::Extends, Rel::Implements, Rel::Overrides, Rel::Instantiates, Rel::Calls, Rel::Uses] {
            let group: Vec<&&Site> = listed.iter().filter(|s| s.rel == rel).collect();
            if group.is_empty() {
                continue;
            }
            let total = prod_usage.iter().filter(|s| s.rel == rel).count();
            let gfiles: FxHashSet<FileId> = prod_usage.iter().filter(|s| s.rel == rel).map(|s| s.file).collect();
            let _ = writeln!(out, "**{}** ({}, {})", rel.title(), plural(total, "site"), plural(gfiles.len(), "file"));
            for s in group {
                file_set.insert(s.file);
                let spath = g.path(s.file);
                let owner_part = if g.kind(s.owner) == NodeKind::File { String::new() } else { format!(" (`{}`)", display_name(g, s.owner)) };
                let synth_part = s.synth.as_deref().map(|l| format!(" — via {l}")).unwrap_or_default();
                let snippet = sources.get(&s.file).and_then(|sv| trimmed_line(sv, s.line)).map(|t| format!(": {}", inline_code(&t))).unwrap_or_default();
                let _ = writeln!(out, "- `{spath}:{}`{owner_part}{synth_part}{snippet}", s.line);
            }
            out.push('\n');
        }
        if prod_usage.len() > listed.len() {
            let mut by_file: Vec<(FileId, usize)> = Vec::new();
            for s in &prod_usage[listed.len()..] {
                if let Some(e) = by_file.iter_mut().find(|(f, _)| *f == s.file) {
                    e.1 += 1;
                } else {
                    by_file.push((s.file, 1));
                }
            }
            let rest_n = prod_usage.len() - listed.len();
            let list: Vec<String> = by_file.iter().take(12).map(|(f, n)| format!("`{}` ({n})", g.path(*f))).collect();
            let tail = if by_file.len() > 12 { ", …" } else { "" };
            let _ = writeln!(out, "+{} in {}: {}{tail}\n", plural(rest_n, "more site"), plural(by_file.len(), "file"), list.join(", "));
        }

        // Imports / re-exports
        if !prod_entries.is_empty() {
            let shown: Vec<String> = prod_entries
                .iter()
                .take(per_def_caps.sites)
                .map(|s| if s.line > 0 { format!("`{}:{}`", g.path(s.file), s.line) } else { format!("`{}`", g.path(s.file)) })
                .collect();
            let more = if prod_entries.len() > shown.len() { format!(", +{} more", prod_entries.len() - shown.len()) } else { String::new() };
            let _ = writeln!(out, "**Imported or re-exported in** {}{more}\n", shown.join(", "));
        }

        // Tests
        if !tests.is_empty() {
            let mut by_file: Vec<(FileId, Vec<&Site>)> = Vec::new();
            for s in &tests {
                if let Some(e) = by_file.iter_mut().find(|(f, _)| *f == s.file) {
                    e.1.push(s);
                } else {
                    by_file.push((s.file, vec![s]));
                }
            }
            by_file.sort_by(|a, b| g.path(a.0).cmp(g.path(b.0)));
            let _ = writeln!(out, "**In tests** ({}, {})", plural(tests.len(), "site"), plural(by_file.len(), "file"));
            let mut named = 0usize;
            for (tf, tsites) in by_file.iter().take(per_def_caps.test_files) {
                let mut owners: Vec<(Option<NodeIdx>, Vec<u32>)> = Vec::new();
                for s in tsites {
                    let key = (g.kind(s.owner) != NodeKind::File).then_some(s.owner);
                    if let Some((_, ls)) = owners.iter_mut().find(|(k, _)| *k == key) {
                        if s.line > 0 && !ls.contains(&s.line) {
                            ls.push(s.line);
                        }
                    } else {
                        owners.push((key, if s.line > 0 { vec![s.line] } else { Vec::new() }));
                    }
                }
                let take_n = per_def_caps.test_owners.saturating_sub(named);
                let mut parts: Vec<String> = Vec::new();
                for (owner, ls) in owners.iter().take(take_n) {
                    named += 1;
                    let lstr = if ls.is_empty() {
                        String::new()
                    } else {
                        let head: Vec<String> = ls.iter().take(MAX_LINES_PER_TEST).map(|l| l.to_string()).collect();
                        let extra = if ls.len() > MAX_LINES_PER_TEST { format!(", +{} more", ls.len() - MAX_LINES_PER_TEST) } else { String::new() };
                        format!("{}{extra}", head.join(", "))
                    };
                    match owner {
                        Some(o) if !lstr.is_empty() => parts.push(format!("`{}` ({lstr})", display_name(g, *o))),
                        Some(o) => parts.push(format!("`{}`", display_name(g, *o))),
                        None if !lstr.is_empty() => parts.push(lstr),
                        None => {}
                    }
                }
                let rem = owners.len().saturating_sub(take_n);
                if rem > 0 {
                    parts.push(format!("+{rem} more"));
                }
                let detail = if parts.is_empty() { String::new() } else { format!(": {}", parts.join(", ")) };
                let _ = writeln!(out, "- `{}`{detail}", g.path(*tf));
            }
            if by_file.len() > per_def_caps.test_files {
                let _ = writeln!(out, "- +{}", plural(by_file.len() - per_def_caps.test_files, "more file"));
            }
            out.push('\n');
        }

        // Callees for callables
        if g.kind(*def).is_callable() {
            let callees = g.callees(*def, 1, per_def_caps.callees + 1);
            if !callees.is_empty() {
                let shown: Vec<String> = callees
                    .iter()
                    .take(per_def_caps.callees)
                    .map(|c| format!("`{}` ({}:{})", display_name(g, c.node), g.node_path(c.node), g.line(c.node)))
                    .collect();
                let more = if callees.len() > per_def_caps.callees { ", …" } else { "" };
                let _ = writeln!(out, "**Calls** {}{more}\n", shown.join(", "));
            } else {
                let outgoing = g.view.unresolved_refs_from(*def, cg_store::UnresolvedScope::CallLike, 5);
                meta.record_tail_idx(&outgoing);
                let hint = crate::tools::format_unresolved_hint(g, g.name(*def), "callees", &outgoing);
                if !hint.is_empty() {
                    let _ = writeln!(out, "{hint}\n");
                }
            }
        }
    }

    let mut files: Vec<String> = file_set.into_iter().map(|f| g.path(f).to_owned()).collect();
    files.sort();
    (out.trim_end().to_owned(), files, sent_now, meta)
}
