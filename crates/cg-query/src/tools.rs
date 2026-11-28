//! Tool handlers shared by the MCP server and the CLI (design doc 8.4, 11.1): the same code
//! produces the same text on both paths. Argument names, defaults and output shapes follow the
//! upstream tool set.

use crate::explore::{ExploreOptions, History, SentQuery};
use crate::graph::Graph;
use crate::search::SearchOptions;
use crate::strings;
pub use crate::strings::OutputMode;
use cg_core::{EdgeKind, FileId, NodeIdx, NodeKind};
use rustc_hash::FxHashMap;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Upstream's cap on non-explore tool output.
pub const MAX_OUTPUT_LENGTH: usize = 15_000;

/// Per-call instrumentation emitted alongside tool text (used by opt-in telemetry).
#[derive(Clone, Debug, Default)]
pub struct ToolMeta {
    pub one_symbol: bool,
    pub dedup_hits: u32,
    pub dedup_saved_bytes: usize,
    pub tail_idx_checked: bool,
    pub tail_idx_triggered: bool,
    pub tail_idx_ambiguous: usize,
    pub tail_idx_dynamic: usize,
    pub tail_idx_no_candidate: usize,
}

impl ToolMeta {
    pub fn record_tail_idx(&mut self, summary: &cg_store::UnresolvedRefSummary) {
        self.tail_idx_checked = true;
        if summary.total() > 0 {
            self.tail_idx_triggered = true;
            self.tail_idx_ambiguous += summary.ambiguous;
            self.tail_idx_dynamic += summary.dynamic;
            self.tail_idx_no_candidate += summary.no_candidate;
        }
    }

    pub fn record_tail_idx_dynamic(&mut self, summary: &cg_store::UnresolvedRefSummary) {
        self.tail_idx_checked = true;
        if summary.ambiguous + summary.dynamic > 0 {
            self.tail_idx_triggered = true;
            self.tail_idx_ambiguous += summary.ambiguous;
            self.tail_idx_dynamic += summary.dynamic;
        }
    }

    pub fn merge(&mut self, other: &ToolMeta) {
        self.one_symbol |= other.one_symbol;
        self.dedup_hits += other.dedup_hits;
        self.dedup_saved_bytes += other.dedup_saved_bytes;
        self.tail_idx_checked |= other.tail_idx_checked;
        self.tail_idx_triggered |= other.tail_idx_triggered;
        self.tail_idx_ambiguous += other.tail_idx_ambiguous;
        self.tail_idx_dynamic += other.tail_idx_dynamic;
        self.tail_idx_no_candidate += other.tail_idx_no_candidate;
    }
}

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    pub meta: ToolMeta,
}

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> ToolOutput {
        ToolOutput { text: text.into(), is_error: false, meta: ToolMeta::default() }
    }
    pub fn ok_with_meta(text: impl Into<String>, meta: ToolMeta) -> ToolOutput {
        ToolOutput { text: text.into(), is_error: false, meta }
    }
    pub fn err(text: impl Into<String>) -> ToolOutput {
        ToolOutput { text: text.into(), is_error: true, meta: ToolMeta::default() }
    }
}

/// Facts about the snapshot that the graph itself does not carry.
#[derive(Clone, Default)]
pub struct StatusInfo {
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
    pub generation: u64,
    pub bytes: u64,
    pub built_at: String,
    pub engine_version: u32,
    pub engine_outdated: bool,
    /// "N added, M modified, R removed" when measured.
    pub changes: Option<String>,
    pub notes: Vec<String>,
}

pub struct ToolEnv<'a> {
    pub graph: &'a Graph,
    pub history: Option<Arc<Mutex<History>>>,
    pub status: StatusInfo,
    pub mode: OutputMode,
}

fn clamp(v: f64, lo: f64, hi: f64) -> usize {
    v.max(lo).min(hi) as usize
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
}

fn num_arg(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| v.as_f64())
}

fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn cap_arg(args: &Value) -> usize {
    num_arg(args, "maxChars").map(|n| clamp(n, 500.0, 50_000.0)).unwrap_or(MAX_OUTPUT_LENGTH)
}

fn truncate_output(text: String) -> String {
    truncate_output_to(text, MAX_OUTPUT_LENGTH)
}

fn truncate_output_to(text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    const TAIL: &str = "\n\n... (output truncated)";
    let mut cut = cap.saturating_sub(TAIL.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &text[..cut];
    let nl = head.rfind('\n').filter(|n| *n as f64 > cut as f64 * 0.8).unwrap_or(cut);
    format!("{}{TAIL}", &head[..nl])
}

/// Dispatch a tool by its short name (`explore`, `node`, ...).
pub fn call(env: &ToolEnv<'_>, short: &str, args: &Value) -> ToolOutput {
    match short {
        "explore" => handle_explore(env, args),
        "search" => handle_search(env, args),
        "callers" => handle_callers(env, args, true),
        "callees" => handle_callers(env, args, false),
        "impact" => handle_impact(env, args),
        "implementations" => handle_implementations(env, args),
        "node" => handle_node(env, args),
        "files" => handle_files(env, args),
        "status" => handle_status(env),
        other => ToolOutput::err(format!("Unknown tool: {other}")),
    }
}

fn handle_explore(env: &ToolEnv<'_>, args: &Value) -> ToolOutput {
    let Some(query) = str_arg(args, "query") else {
        return ToolOutput::err("Error: query must be a non-empty string");
    };
    let no_dedup = bool_arg(args, "noDedup", false);
    let opts = ExploreOptions {
        max_files: num_arg(args, "maxFiles").map(|n| clamp(n, 1.0, 20.0)),
        max_output: num_arg(args, "maxChars").map(|n| clamp(n, 1_000.0, 50_000.0)),
        compact: bool_arg(args, "compact", false),
        history: if no_dedup { None } else { env.history.clone() },
        mode: env.mode,
    };
    let r = env.graph.explore(query, &opts);
    ToolOutput::ok_with_meta(r.text, r.meta)
}

fn kind_filter(k: &str) -> Option<NodeKind> {
    if k == "type" {
        return Some(NodeKind::TypeAlias);
    }
    NodeKind::parse(k)
}

fn handle_search(env: &ToolEnv<'_>, args: &Value) -> ToolOutput {
    let g = env.graph;
    let Some(query) = str_arg(args, "query") else {
        return ToolOutput::err("Error: query must be a non-empty string");
    };
    let limit = clamp(num_arg(args, "limit").unwrap_or(10.0), 1.0, 100.0);
    let kinds: Vec<NodeKind> = str_arg(args, "kind").and_then(kind_filter).into_iter().collect();
    let mut hits = g.search(query, &SearchOptions { limit, kinds, ..Default::default() });
    if hits.is_empty() {
        let suggest = g.suggest_names(query, 5);
        let tail = if suggest.is_empty() {
            String::new()
        } else {
            format!("\n\nDid you mean: {}", suggest.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "))
        };
        return ToolOutput::ok(format!("No results found for \"{query}\"{tail}"));
    }
    hits.sort_by_key(|h| g.is_generated_file(g.view.hot(h.node).file));
    let mut lines = vec![format!("**Search Results ({} found)**", hits.len()), String::new()];
    for h in &hits {
        let n = h.node;
        lines.push(format!("**{}** ({})", g.name(n), g.kind(n)));
        lines.push(format!("{}:{}", g.node_path(n), g.line(n)));
        if let Some(sig) = g.view.node(n).signature() {
            lines.push(format!("`{sig}`"));
        }
        lines.push(String::new());
    }
    ToolOutput::ok(truncate_output_to(lines.join("\n"), cap_arg(args)))
}

/// Matches for a symbol, grouped into distinct definitions (one per file + qualified name).
fn definition_groups(g: &Graph, nodes: &[NodeIdx], file_filter: Option<&str>) -> (Vec<Vec<NodeIdx>>, bool) {
    let mut keyed: Vec<((String, String), Vec<NodeIdx>)> = Vec::new();
    for n in nodes {
        let key = (g.node_path(*n).to_owned(), g.qname(*n).to_owned());
        match keyed.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(*n),
            None => keyed.push((key, vec![*n])),
        }
    }
    let mut filtered_out = false;
    if let Some(f) = file_filter {
        let f = f.replace('\\', "/").to_lowercase();
        let narrowed: Vec<_> = keyed.iter().filter(|((p, _), _)| p.to_lowercase().ends_with(&f) || p.to_lowercase().contains(&f)).cloned().collect();
        if narrowed.is_empty() {
            filtered_out = true;
        } else {
            keyed = narrowed;
        }
    }
    (keyed.into_iter().map(|x| x.1).collect(), filtered_out)
}

fn symbol_note(g: &Graph, symbol: &str) -> String {
    let sugg = g.suggest_names(symbol, 5);
    if sugg.is_empty() {
        String::new()
    } else {
        format!("\n\nDid you mean: {}", sugg.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "))
    }
}

fn edge_label(kind: EdgeKind, synth: Option<&str>, fn_ref: bool) -> Option<String> {
    if let Some(s) = synth {
        return Some(s.to_owned());
    }
    match kind {
        EdgeKind::Calls => None,
        EdgeKind::References if fn_ref => Some("callback registration".into()),
        EdgeKind::Instantiates => Some("instantiation".into()),
        EdgeKind::Imports => Some("import".into()),
        EdgeKind::References => Some("reference".into()),
        k => Some(k.as_str().into()),
    }
}

fn format_lines(lines: &[u32], max: usize) -> String {
    let shown: Vec<String> = lines.iter().take(max).map(|l| l.to_string()).collect();
    let s = shown.join(", ");
    if lines.len() > max {
        format!("{s}, +{} more", lines.len() - max)
    } else {
        s
    }
}

fn stale_banner_for_files(g: &Graph, files: impl IntoIterator<Item = FileId>) -> String {
    let mut seen: rustc_hash::FxHashSet<FileId> = rustc_hash::FxHashSet::default();
    let mut stale: Vec<String> = Vec::new();
    for f in files {
        if seen.len() >= 200 {
            break;
        }
        if seen.insert(f) && g.file_is_stale(f) {
            stale.push(g.path(f).to_owned());
        }
    }
    if stale.is_empty() {
        return String::new();
    }
    stale.sort();
    format!("{}\n\n", strings::stale_banner(&stale))
}

pub(crate) fn format_unresolved_hint(g: &Graph, subject: &str, what: &str, summary: &cg_store::UnresolvedRefSummary, mode: OutputMode) -> String {
    let total = summary.total();
    if total == 0 {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    if summary.ambiguous > 0 {
        parts.push(format!("{} ambiguous", summary.ambiguous));
    }
    if summary.dynamic > 0 {
        parts.push(format!("{} dynamic-receiver", summary.dynamic));
    }
    if summary.no_candidate > 0 {
        parts.push(format!("{} unresolved", summary.no_candidate));
    }
    let counts = parts.join(", ");
    let mut out = format!(
        "> **Note (unresolved / ambiguous references):** static graph found 0 resolved {what}, but the index records {counts} reference(s) for `{subject}` (possible dynamic dispatch or overload):"
    );
    for s in &summary.sites {
        let tag = match s.status {
            cg_store::status::AMBIGUOUS => "ambiguous",
            cg_store::status::LOCAL_BINDING => "dynamic",
            _ => "unresolved",
        };
        out.push_str(&format!("\n> - `{}:{}` (`{}`) [{tag}]", g.path(s.file), s.line, s.expr));
    }
    if total > summary.sites.len() {
        out.push_str(&format!("\n> - … (+{} more)", total - summary.sites.len()));
    }
    out.push_str(&format!(
        "\n> Tip: verify these dynamic/overload sites with targeted `rg -n \"{subject}\"` (or narrow with `{}`).",
        mode.flag("file", "--file")
    ));
    out
}

fn format_low_caller_dynamic_hint(g: &Graph, subject: &str, summary: &cg_store::UnresolvedRefSummary) -> String {
    let dyn_total = summary.ambiguous + summary.dynamic;
    if dyn_total == 0 {
        return String::new();
    }
    let examples: Vec<String> = summary
        .sites
        .iter()
        .filter(|s| matches!(s.status, cg_store::status::AMBIGUOUS | cg_store::status::LOCAL_BINDING))
        .take(3)
        .map(|s| format!("`{}:{}`", g.path(s.file), s.line))
        .collect();
    let ex_part = if examples.is_empty() { String::new() } else { format!(" (e.g. {})", examples.join(", ")) };
    format!(
        "> **Dynamic-dispatch note (`tail_idx`):** +{dyn_total} unresolved dynamic/ambiguous reference(s) for `{subject}`{ex_part} — verify with targeted `rg -n \"{subject}\"` if refactoring."
    )
}

fn unresolved_outgoing_for_defs(g: &Graph, defs: &[NodeIdx], max_sites: usize) -> cg_store::UnresolvedRefSummary {
    let mut merged = cg_store::UnresolvedRefSummary::default();
    for d in defs {
        let s = g.view.unresolved_refs_from(*d, cg_store::UnresolvedScope::CallLike, max_sites);
        merged.ambiguous += s.ambiguous;
        merged.dynamic += s.dynamic;
        merged.no_candidate += s.no_candidate;
        merged.sites.extend(s.sites);
    }
    let rank = |s: u8| match s {
        cg_store::status::AMBIGUOUS => 0u8,
        cg_store::status::LOCAL_BINDING => 1u8,
        _ => 2u8,
    };
    merged.sites.sort_by(|a, b| {
        rank(a.status).cmp(&rank(b.status)).then_with(|| g.path(a.file).cmp(g.path(b.file))).then_with(|| a.line.cmp(&b.line)).then_with(|| a.expr.cmp(&b.expr))
    });
    merged.sites.dedup_by(|b, a| a.file == b.file && a.line == b.line && a.expr == b.expr);
    merged.sites.truncate(max_sites);
    merged
}

fn handle_callers(env: &ToolEnv<'_>, args: &Value, callers: bool) -> ToolOutput {
    let g = env.graph;
    let Some(symbol) = str_arg(args, "symbol") else {
        return ToolOutput::err("Error: symbol must be a non-empty string");
    };
    let limit = clamp(num_arg(args, "limit").unwrap_or(20.0), 1.0, 100.0);
    let cap = cap_arg(args);
    let no_dedup = bool_arg(args, "noDedup", false);
    let file_filter = str_arg(args, "file");
    let all = g.resolve_symbol(symbol);
    let what = if callers { "callers" } else { "callees" };
    let tool_name = what;
    if all.is_empty() {
        return ToolOutput::ok(format!("Symbol \"{symbol}\" not found in the codebase{}", symbol_note(g, symbol)));
    }
    let (groups, filtered_out) = definition_groups(g, &all, file_filter);
    let filter_note = if filtered_out {
        format!("\n\n> **Note:** no definition of \"{symbol}\" matches file \"{}\" — showing all definitions instead.", file_filter.unwrap_or(""))
    } else {
        String::new()
    };
    let title = if callers { "Callers of" } else { "Callees of" };
    let cache_key = format!("{symbol}|{}|{limit}|{cap}", file_filter.unwrap_or(""));
    let collect = |defs: &[NodeIdx]| -> (Vec<String>, Vec<FileId>) {
        let mut seen: FxHashMap<NodeIdx, usize> = FxHashMap::default();
        type Row = (NodeIdx, Vec<u32>, EdgeKind, Option<String>, bool, bool);
        let mut rows: Vec<Row> = Vec::new();
        for d in defs {
            let rel = if callers { g.callers(*d, 1, 1000) } else { g.callees(*d, 1, 1000) };
            for r in rel {
                match seen.get(&r.node) {
                    Some(i) => {
                        let row = &mut rows[*i];
                        for l in r.lines {
                            if !row.1.contains(&l) {
                                row.1.push(l);
                            }
                        }
                        if r.kind == EdgeKind::Calls {
                            row.2 = EdgeKind::Calls;
                        }
                        if row.3.is_none() {
                            row.3 = r.synth;
                        }
                        row.4 |= r.fn_ref;
                    }
                    None => {
                        seen.insert(r.node, rows.len());
                        rows.push((r.node, r.lines, r.kind, r.synth, r.fn_ref, r.node == *d));
                    }
                }
            }
        }
        let mut files: Vec<FileId> = defs.iter().map(|d| g.view.hot(*d).file).collect();
        files.extend(rows.iter().take(limit).map(|(n, ..)| g.view.hot(*n).file));
        let formatted = rows
            .into_iter()
            .map(|(n, mut lines, kind, synth, fn_ref, recursive)| {
                lines.sort_unstable();
                let loc = if lines.is_empty() { format!(":{}", g.line(n)) } else { format!(":{}", format_lines(&lines, 10)) };
                let label = edge_label(kind, synth.as_deref(), fn_ref).map(|l| format!(" — via {l}")).unwrap_or_default();
                let itself = if recursive { " — calls itself" } else { "" };
                format!("- {} ({}) - {}{loc}{label}{itself}", g.name(n), g.kind(n), g.node_path(n))
            })
            .collect();
        (formatted, files)
    };
    let mut meta = ToolMeta::default();
    let reuse_hint = strings::dedup_reuse_hint(env.mode);
    let file_flag = env.mode.flag("file", "--file");
    let limit_flag = env.mode.flag("limit", "--limit");
    if groups.len() == 1 {
        let (rows, files) = collect(&groups[0]);
        let stale = stale_banner_for_files(g, files);
        if stale.is_empty() && !no_dedup {
            if let Some(hist) = &env.history {
                let guard = hist.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(prev) = guard.find_query(tool_name, &cache_key, env.status.generation) {
                    let subj = format!("{} (gen {})", prev.summary, env.status.generation);
                    if let Some((notice, saved)) = crate::explore::dedup_notice(&subj, prev.bytes, reuse_hint) {
                        meta.dedup_hits += 1;
                        meta.dedup_saved_bytes += saved;
                        return ToolOutput::ok_with_meta(notice, meta);
                    }
                }
            }
        }
        if rows.is_empty() {
            let simple = g.name(groups[0][0]);
            let summary = if callers {
                g.view.unresolved_refs_by_tail(simple, cg_store::UnresolvedScope::CallLike, 5)
            } else {
                unresolved_outgoing_for_defs(g, &groups[0], 5)
            };
            meta.record_tail_idx(&summary);
            let hint = format_unresolved_hint(g, simple, what, &summary, env.mode);
            let hint_block = if hint.is_empty() { String::new() } else { format!("\n\n{hint}") };
            return ToolOutput::ok_with_meta(truncate_output_to(format!("{stale}No {what} found for \"{symbol}\"{hint_block}{filter_note}"), cap), meta);
        }
        let dyn_note = if callers && rows.len() <= 3 {
            let simple = g.name(groups[0][0]);
            let summary = g.view.unresolved_refs_by_tail(simple, cg_store::UnresolvedScope::CallLike, 5);
            meta.record_tail_idx_dynamic(&summary);
            let h = format_low_caller_dynamic_hint(g, simple, &summary);
            if h.is_empty() {
                String::new()
            } else {
                format!("\n\n{h}")
            }
        } else {
            String::new()
        };
        let total_rows = rows.len();
        let cut =
            if total_rows > limit { format!("\n\n> Showing {limit} of {total_rows} {what}; pass `{limit_flag}` (up to 100) to widen.") } else { String::new() };
        let header = format!("**{title} {symbol} ({} found)**", total_rows.min(limit));
        let mut lines = vec![header.clone(), String::new()];
        lines.extend(rows.into_iter().take(limit));
        let full = stale.clone() + &lines.join("\n") + &dyn_note + &cut + &filter_note;
        let truncated = full.len() > cap;
        let out_text = truncate_output_to(full, cap);
        if stale.is_empty() && !truncated {
            let summary = header.trim_matches('*').to_owned();
            let subj = format!("{summary} (gen {})", env.status.generation);
            if crate::explore::dedup_notice(&subj, out_text.len(), reuse_hint).is_some() {
                if let Some(hist) = &env.history {
                    hist.lock().unwrap_or_else(|e| e.into_inner()).record_query(SentQuery {
                        tool: tool_name.to_owned(),
                        key: cache_key,
                        generation: env.status.generation,
                        summary,
                        bytes: out_text.len(),
                    });
                }
            }
        }
        return ToolOutput::ok_with_meta(out_text, meta);
    }
    let mut touched: Vec<FileId> = Vec::new();
    let mut any_rows = false;
    let header = format!("**{title} {symbol} — {} distinct definitions (narrow with `{file_flag}`)**", groups.len());
    let mut lines = vec![header.clone()];
    for grp in &groups {
        let head = grp[0];
        lines.push(String::new());
        lines.push(format!("**{}** ({}) — {}:{}", g.qname(head), g.kind(head), g.node_path(head), g.line(head)));
        let (rows, files) = collect(grp);
        touched.extend(files);
        if rows.is_empty() {
            lines.push(format!("- (no {what})"));
            if !callers {
                let summary = unresolved_outgoing_for_defs(g, grp, 3);
                meta.record_tail_idx(&summary);
                let hint = format_unresolved_hint(g, g.name(head), what, &summary, env.mode);
                if !hint.is_empty() {
                    lines.push(hint);
                }
            }
            continue;
        }
        any_rows = true;
        let total = rows.len();
        lines.extend(rows.into_iter().take(limit));
        if total > limit {
            lines.push(format!("- … +{} more (pass `{limit_flag}` to widen)", total - limit));
        }
    }
    let stale = stale_banner_for_files(g, touched);
    if stale.is_empty() && !no_dedup && any_rows {
        if let Some(hist) = &env.history {
            let guard = hist.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(prev) = guard.find_query(tool_name, &cache_key, env.status.generation) {
                let subj = format!("{} (gen {})", prev.summary, env.status.generation);
                if let Some((notice, saved)) = crate::explore::dedup_notice(&subj, prev.bytes, reuse_hint) {
                    meta.dedup_hits += 1;
                    meta.dedup_saved_bytes += saved;
                    return ToolOutput::ok_with_meta(notice, meta);
                }
            }
        }
    }
    let hint_block = if !any_rows && callers {
        let simple = g.name(groups[0][0]);
        let summary = g.view.unresolved_refs_by_tail(simple, cg_store::UnresolvedScope::CallLike, 5);
        meta.record_tail_idx(&summary);
        let hint = format_unresolved_hint(g, simple, what, &summary, env.mode);
        if hint.is_empty() {
            String::new()
        } else {
            format!("\n\n{hint}")
        }
    } else {
        String::new()
    };
    let full = stale.clone() + &lines.join("\n") + &hint_block + &filter_note;
    let truncated = full.len() > cap;
    let out_text = truncate_output_to(full, cap);
    if stale.is_empty() && !truncated && any_rows {
        let summary = header.trim_matches('*').to_owned();
        let subj = format!("{summary} (gen {})", env.status.generation);
        if crate::explore::dedup_notice(&subj, out_text.len(), reuse_hint).is_some() {
            if let Some(hist) = &env.history {
                hist.lock().unwrap_or_else(|e| e.into_inner()).record_query(SentQuery {
                    tool: tool_name.to_owned(),
                    key: cache_key,
                    generation: env.status.generation,
                    summary,
                    bytes: out_text.len(),
                });
            }
        }
    }
    ToolOutput::ok_with_meta(out_text, meta)
}

fn format_impact(g: &Graph, label: &str, root: NodeIdx, imp: &crate::traverse::Impact, mode: OutputMode) -> String {
    let mut nodes: Vec<NodeIdx> = vec![root];
    nodes.extend(imp.nodes.iter().map(|x| x.0));
    let mut lines = vec![
        format!("**Impact: \"{label}\" affects {} symbols{}**", nodes.len(), if imp.truncated { " (truncated at safety limit)" } else { "" }),
        String::new(),
    ];
    if imp.truncated {
        lines.push(strings::impact_truncated_note(mode).into());
        lines.push(String::new());
    }
    let mut by_file: Vec<(String, Vec<NodeIdx>)> = Vec::new();
    for n in nodes {
        let p = g.node_path(n).to_owned();
        match by_file.iter_mut().find(|(f, _)| *f == p) {
            Some((_, v)) => v.push(n),
            None => by_file.push((p, vec![n])),
        }
    }
    for (file, ns) in by_file {
        lines.push(format!("**{file}:**"));
        lines.push(ns.iter().map(|n| format!("{}:{}", g.name(*n), g.line(*n))).collect::<Vec<_>>().join(", "));
        lines.push(String::new());
    }
    lines.join("\n")
}

fn handle_impact(env: &ToolEnv<'_>, args: &Value) -> ToolOutput {
    let g = env.graph;
    let Some(symbol) = str_arg(args, "symbol") else {
        return ToolOutput::err("Error: symbol must be a non-empty string");
    };
    let depth = clamp(num_arg(args, "depth").unwrap_or(2.0), 1.0, 10.0) as u32;
    let cap = cap_arg(args);
    let all = g.resolve_symbol(symbol);
    if all.is_empty() {
        return ToolOutput::ok(format!("Symbol \"{symbol}\" not found in the codebase{}", symbol_note(g, symbol)));
    }
    let (groups, filtered_out) = definition_groups(g, &all, str_arg(args, "file"));
    let filter_note = if filtered_out {
        format!("\n\n> **Note:** no definition of \"{symbol}\" matches file \"{}\" — showing all definitions instead.", str_arg(args, "file").unwrap_or(""))
    } else {
        String::new()
    };
    let mut meta = ToolMeta::default();
    if groups.len() == 1 {
        let root = groups[0][0];
        let imp = g.impact(root, depth, 5_000);
        let stale = stale_banner_for_files(g, std::iter::once(g.view.hot(root).file).chain(imp.nodes.iter().map(|(n, _)| g.view.hot(*n).file)));
        let hint_block = if imp.nodes.is_empty() {
            let simple = g.name(root);
            let summary = g.view.unresolved_refs_by_tail(simple, cg_store::UnresolvedScope::Dependency, 5);
            meta.record_tail_idx(&summary);
            let hint = format_unresolved_hint(g, simple, "dependents", &summary, env.mode);
            if hint.is_empty() {
                String::new()
            } else {
                format!("\n{hint}")
            }
        } else if imp.nodes.len() <= 3 {
            let simple = g.name(root);
            let summary = g.view.unresolved_refs_by_tail(simple, cg_store::UnresolvedScope::Dependency, 5);
            meta.record_tail_idx_dynamic(&summary);
            let hint = format_low_caller_dynamic_hint(g, simple, &summary);
            if hint.is_empty() {
                String::new()
            } else {
                format!("\n{hint}")
            }
        } else {
            String::new()
        };
        return ToolOutput::ok_with_meta(truncate_output_to(stale + &format_impact(g, symbol, root, &imp, env.mode) + &hint_block + &filter_note, cap), meta);
    }
    let mut touched: Vec<FileId> = Vec::new();
    let mut any_dependents = false;
    let file_flag = env.mode.flag("file", "--file");
    let mut lines = vec![format!("**Impact of {symbol} — {} distinct definitions (each with its own blast radius; narrow with `{file_flag}`)**", groups.len())];
    for grp in groups.iter().take(12) {
        let root = grp[0];
        let imp = g.impact(root, depth, 5_000);
        any_dependents |= !imp.nodes.is_empty();
        touched.push(g.view.hot(root).file);
        touched.extend(imp.nodes.iter().map(|(n, _)| g.view.hot(*n).file));
        lines.push(String::new());
        lines.push(format_impact(g, &format!("{} ({}:{})", g.qname(root), g.node_path(root), g.line(root)), root, &imp, env.mode));
    }
    let hint_block = if !any_dependents {
        let simple = g.name(groups[0][0]);
        let summary = g.view.unresolved_refs_by_tail(simple, cg_store::UnresolvedScope::Dependency, 5);
        meta.record_tail_idx(&summary);
        let hint = format_unresolved_hint(g, simple, "dependents", &summary, env.mode);
        if hint.is_empty() {
            String::new()
        } else {
            format!("\n{hint}")
        }
    } else {
        String::new()
    };
    let stale = stale_banner_for_files(g, touched);
    ToolOutput::ok_with_meta(truncate_output_to(stale + &lines.join("\n") + &hint_block + &filter_note, cap), meta)
}

fn handle_implementations(env: &ToolEnv<'_>, args: &Value) -> ToolOutput {
    let g = env.graph;
    let Some(symbol) = str_arg(args, "symbol") else {
        return ToolOutput::err("Error: symbol must be a non-empty string");
    };
    let depth = clamp(num_arg(args, "depth").unwrap_or(10.0), 1.0, 20.0) as u32;
    let cap = cap_arg(args);
    let all = g.resolve_symbol(symbol);
    if all.is_empty() {
        return ToolOutput::ok(format!("Symbol \"{symbol}\" not found in the codebase{}", symbol_note(g, symbol)));
    }
    let (groups, filtered_out) = definition_groups(g, &all, str_arg(args, "file"));
    let filter_note = if filtered_out {
        format!("\n\n> **Note:** no definition of \"{symbol}\" matches file \"{}\" — showing all definitions instead.", str_arg(args, "file").unwrap_or(""))
    } else {
        String::new()
    };
    let mut meta = ToolMeta::default();
    let mut touched: Vec<FileId> = Vec::new();
    let mut any_impls = false;
    let mut lines: Vec<String> = Vec::new();
    const CAP: usize = 12;
    for (idx, grp) in groups.iter().take(CAP).enumerate() {
        let n = grp[0];
        if idx > 0 {
            lines.push(String::new());
        }
        let mut seen: rustc_hash::FxHashSet<NodeIdx> = rustc_hash::FxHashSet::default();
        let mut impls: Vec<NodeIdx> = Vec::new();
        for &m in grp {
            touched.push(g.view.hot(m).file);
            for i in g.implementations(m, depth) {
                if seen.insert(i) {
                    impls.push(i);
                }
            }
        }
        touched.extend(impls.iter().map(|i| g.view.hot(*i).file));
        lines.push(format!("**Implementations of {} ({}:{}) — {} found**", g.qname(n), g.node_path(n), g.line(n), impls.len()));
        if impls.is_empty() {
            lines.push("- (none)".into());
        } else {
            any_impls = true;
            for i in &impls {
                lines.push(format!("- {} ({}) - {}:{}", g.name(*i), g.kind(*i), g.node_path(*i), g.line(*i)));
            }
        }
    }
    if groups.len() > CAP {
        lines.push(String::new());
        lines.push(format!("> … +{} more definitions of \"{symbol}\" (narrow with `{}`).", groups.len() - CAP, env.mode.flag("file", "--file")));
    }
    let hint_block = if !any_impls {
        let simple = g.name(groups[0][0]);
        let summary = g.view.unresolved_refs_by_tail(simple, cg_store::UnresolvedScope::Dependency, 5);
        meta.record_tail_idx(&summary);
        let hint = format_unresolved_hint(g, simple, "implementations", &summary, env.mode);
        if hint.is_empty() {
            String::new()
        } else {
            format!("\n\n{hint}")
        }
    } else {
        String::new()
    };
    let stale = stale_banner_for_files(g, touched);
    ToolOutput::ok_with_meta(truncate_output_to(stale + &lines.join("\n") + &hint_block + &filter_note, cap), meta)
}

/// Safety checks for a path argument (design doc 8.4, 10.4): inside the project only, no
/// credentials directories.
pub fn check_path_arg(g: &Graph, p: &str) -> Result<(), String> {
    let norm = p.replace('\\', "/");
    if norm.starts_with('~') {
        return Err("Error: paths outside the project are not readable through this tool".into());
    }
    let path = std::path::Path::new(&norm);
    if path.is_absolute() {
        match path.strip_prefix(&g.root) {
            Ok(_) => {}
            Err(_) => return Err("Error: path is outside the project root".into()),
        }
    }
    if norm.split('/').any(|c| c == "..") {
        return Err("Error: path escapes the project root".into());
    }
    for s in [".ssh", ".gnupg", ".aws", ".config"] {
        if norm.split('/').any(|c| c == s) && path.is_absolute() {
            return Err(format!("Error: refusing to read inside {s}"));
        }
    }
    Ok(())
}

struct SentSpan {
    path: String,
    fp: (u64, i64),
    span: (u32, u32),
}

fn handle_node(env: &ToolEnv<'_>, args: &Value) -> ToolOutput {
    let g = env.graph;
    let symbol = str_arg(args, "symbol");
    let file_hint = str_arg(args, "file");
    let include_code = bool_arg(args, "includeCode", false);
    let compact = bool_arg(args, "compact", false);
    let no_dedup = bool_arg(args, "noDedup", false);
    if let Some(f) = file_hint {
        if let Err(e) = check_path_arg(g, f) {
            return ToolOutput::err(e);
        }
    }
    if symbol.is_none() {
        let Some(file) = file_hint else {
            return ToolOutput::err(match env.mode {
                OutputMode::Mcp => "Error: pass `symbol`, or `file` to read a file",
                OutputMode::Cli => "Error: pass a symbol or file path",
            });
        };
        let opts = FileViewOpts {
            offset: num_arg(args, "offset").filter(|n| *n > 0.0).map(|n| n as usize),
            limit: num_arg(args, "limit").filter(|n| *n > 0.0).map(|n| n as usize),
            symbols_only: bool_arg(args, "symbolsOnly", false) || compact,
            max_chars: num_arg(args, "maxChars").map(|n| clamp(n, 500.0, 100_000.0)),
            no_dedup,
            mode: env.mode,
        };
        return file_view(g, file, opts, env.history.as_ref());
    }
    let symbol = symbol.unwrap();
    let mut matches = g.resolve_symbol(symbol);
    if matches.is_empty() {
        return ToolOutput::ok(format!("Symbol \"{symbol}\" not found in the codebase{}", symbol_note(g, symbol)));
    }
    let line_hint = num_arg(args, "line").filter(|n| *n > 0.0).map(|n| n as u32);
    if matches.len() > 1 && (file_hint.is_some() || line_hint.is_some()) {
        let mut narrowed = matches.clone();
        if let Some(f) = file_hint {
            let f = f.replace('\\', "/").to_lowercase();
            let by_file: Vec<NodeIdx> =
                narrowed.iter().copied().filter(|n| g.node_path(*n).to_lowercase().ends_with(&f) || g.node_path(*n).to_lowercase().contains(&f)).collect();
            if !by_file.is_empty() {
                narrowed = by_file;
            }
        }
        if let Some(l) = line_hint {
            if narrowed.len() > 1 {
                let containing: Vec<NodeIdx> = narrowed.iter().copied().filter(|n| g.line(*n) <= l && g.end_line(*n) >= l).collect();
                narrowed = if !containing.is_empty() {
                    containing
                } else {
                    narrowed.sort_by_key(|n| (g.line(*n) as i64 - l as i64).abs());
                    narrowed.truncate(1);
                    narrowed
                };
            }
        }
        if !narrowed.is_empty() {
            matches = narrowed;
        }
    }
    const CAP: usize = 12;
    let shown = matches.len().min(CAP);
    let effective_code = include_code && !compact;
    let active_history = if effective_code && !no_dedup { env.history.as_ref() } else { None };
    let mut meta = ToolMeta::default();
    let mut sections: Vec<String> = Vec::with_capacity(shown + 1);
    let mut pending_sent: Vec<(usize, SentSpan)> = Vec::new();
    let mut running_len = 0usize;
    for (idx, n) in matches.iter().take(CAP).enumerate() {
        if idx > 0 {
            running_len += 7; // "\n\n---\n\n"
        }
        let (sec, sent) = node_section(g, *n, effective_code, active_history, env.mode, &mut meta);
        running_len += sec.len();
        if let Some(s) = sent {
            pending_sent.push((running_len, s));
        }
        sections.push(sec);
    }
    if matches.len() > CAP {
        sections.push(format!("> … +{} more definitions of \"{symbol}\" (narrow with `{}`).", matches.len() - shown, env.mode.flag("file", "--file")));
    }
    let cap = cap_arg(args);
    let joined = sections.join("\n\n---\n\n");
    let truncated = joined.len() > cap;
    let cut_limit = if truncated { cap.saturating_sub(24) } else { usize::MAX };
    let out_text = truncate_output_to(joined, cap);
    if let Some(hist) = active_history {
        let mut sent_map: std::collections::BTreeMap<String, crate::explore::SentFile> = std::collections::BTreeMap::new();
        for (end_off, s) in pending_sent {
            if end_off <= cut_limit {
                let e = sent_map.entry(s.path).or_insert_with(|| crate::explore::SentFile { fingerprint: s.fp, ranges: Vec::new() });
                e.ranges.push(s.span);
            }
        }
        if !sent_map.is_empty() {
            hist.lock().unwrap_or_else(|e| e.into_inner()).record(sent_map);
        }
    }
    ToolOutput::ok_with_meta(out_text, meta)
}

fn node_section(
    g: &Graph,
    n: NodeIdx,
    include_code: bool,
    history: Option<&Arc<Mutex<History>>>,
    mode: OutputMode,
    meta: &mut ToolMeta,
) -> (String, Option<SentSpan>) {
    let node = g.view.node(n);
    let kind = g.kind(n);
    let mut sent_span: Option<SentSpan> = None;
    let mut lines: Vec<String> = vec![format!("**{}** ({})", node.name(), kind), String::new(), format!("**Location:** {}:{}", g.node_path(n), g.line(n))];
    if let Some(sig) = node.signature() {
        lines.push(format!("**Signature:** `{sig}`"));
    }
    if let Some(doc) = node.docstring() {
        if doc.len() < 200 {
            lines.push(String::new());
            lines.push(doc.to_owned());
        }
    }
    if include_code {
        let f = g.view.hot(n).file;
        if g.file_is_stale(f) {
            lines.push(String::new());
            lines.push(format!("> ⚠ `{}` changed on disk after the last index sync — Read the file for current content.", g.node_path(n)));
        } else if kind.is_container() && kind != NodeKind::File {
            let mut members: Vec<NodeIdx> = g
                .view
                .out_edges(n)
                .filter(|e| e.kind == EdgeKind::Contains && !matches!(g.kind(e.dst), NodeKind::Import | NodeKind::Export))
                .map(|e| e.dst)
                .collect();
            members.sort_by_key(|m| g.line(*m));
            if !members.is_empty() {
                lines.push(String::new());
                lines.push(format!("**Members ({}):**", members.len()));
                lines.push(String::new());
                for m in &members {
                    let sig = g.view.node(*m).signature().map(|s| format!(" — `{s}`")).unwrap_or_default();
                    lines.push(format!("- {} ({}):{}{sig}", g.name(*m), g.kind(*m), g.line(*m)));
                }
                lines.push(String::new());
                lines.push(strings::container_outline_note(mode, g.node_path(n)));
            }
        } else if let Ok(src) = g.view.read_source(f) {
            let (s, e) = (g.line(n) as usize, g.end_line(n).max(g.line(n)) as usize);
            let body: Vec<String> = src.lines().enumerate().filter(|(i, _)| *i + 1 >= s && *i < e).map(|(i, l)| format!("{}\t{l}", i + 1)).collect();
            if !body.is_empty() {
                let body_text = body.join("\n");
                let fenced = format!("```{}\n{body_text}\n```", g.view.file(f).lang().as_str());
                let path = g.node_path(n);
                let fp = g.fingerprint(f);
                let covered = history.is_some_and(|h| h.lock().unwrap_or_else(|err| err.into_inner()).is_covered(path, fp, s as u32, e as u32));
                if let Some((notice, saved)) =
                    covered.then(|| crate::explore::dedup_notice(&format!("lines {s}-{e} of `{path}`"), fenced.len(), strings::dedup_copy_hint(mode))).flatten()
                {
                    meta.dedup_hits += 1;
                    meta.dedup_saved_bytes += saved;
                    lines.push(String::new());
                    lines.push(notice);
                } else {
                    lines.push(String::new());
                    lines.push(fenced);
                    if !covered {
                        sent_span = Some(SentSpan { path: path.to_owned(), fp, span: (s as u32, e as u32) });
                    }
                }
            }
        }
    }
    // trail
    let callees: Vec<_> = g.callees(n, 1, 200).into_iter().filter(|r| r.node != n).collect();
    let callers = g.callers(n, 1, 200);
    if !callees.is_empty() || !callers.is_empty() {
        lines.push(String::new());
        lines.push(strings::node_trail_header(mode));
        let fmt = |r: &crate::traverse::Related, sites: bool| -> String {
            let at = if sites && !r.lines.is_empty() { format_lines(&r.lines, 3) } else { g.line(r.node).to_string() };
            let base = format!("{} ({}:{at})", g.name(r.node), g.node_path(r.node));
            match &r.synth {
                Some(s) => format!("{base} [dynamic: {}]", crate::explore::flow::synth_summary(s)),
                None => base,
            }
        };
        if !callees.is_empty() {
            let shown: Vec<String> = callees.iter().take(12).map(|r| fmt(r, false)).collect();
            lines.push(format!(
                "**Calls →** {}{}",
                shown.join(", "),
                if callees.len() > 12 { format!(", +{} more", callees.len() - 12) } else { String::new() }
            ));
        }
        if !callers.is_empty() {
            let shown: Vec<String> = callers.iter().take(12).map(|r| fmt(r, true)).collect();
            lines.push(format!(
                "**Called by ←** {}{}",
                shown.join(", "),
                if callers.len() > 12 { format!(", +{} more", callers.len() - 12) } else { String::new() }
            ));
        }
    }
    if kind.is_callable() {
        if callers.is_empty() {
            let summary = g.view.unresolved_refs_by_tail(g.name(n), cg_store::UnresolvedScope::CallLike, 5);
            meta.record_tail_idx(&summary);
            let hint = format_unresolved_hint(g, g.name(n), "callers", &summary, mode);
            if !hint.is_empty() {
                lines.push(String::new());
                lines.push(hint);
            }
        }
        if callees.is_empty() {
            let summary = unresolved_outgoing_for_defs(g, &[n], 5);
            meta.record_tail_idx(&summary);
            let hint = format_unresolved_hint(g, g.name(n), "callees", &summary, mode);
            if !hint.is_empty() {
                lines.push(String::new());
                lines.push(hint);
            }
        }
    }
    (lines.join("\n"), sent_span)
}

struct FileViewOpts {
    offset: Option<usize>,
    limit: Option<usize>,
    symbols_only: bool,
    max_chars: Option<usize>,
    no_dedup: bool,
    mode: OutputMode,
}

fn file_view(g: &Graph, file_arg: &str, mut opts: FileViewOpts, history: Option<&Arc<Mutex<History>>>) -> ToolOutput {
    let norm = |p: &str| p.replace('\\', "/").trim_start_matches("./").trim_start_matches('/').trim_end_matches('/').to_owned();
    let mut want = norm(file_arg);
    let mut matches = g.files_matching(&want);
    // `path:12-40` or `path#L12-L40`
    if matches.is_empty() {
        if let Some((head, tail)) = want.rsplit_once(':').or_else(|| want.rsplit_once("#L")) {
            let tail = tail.trim_start_matches('L');
            let mut it = tail.split('-');
            if let Some(Ok(s)) = it.next().map(|x| x.trim_start_matches('L').parse::<usize>()) {
                let alt = g.files_matching(head);
                if !alt.is_empty() {
                    matches = alt;
                    opts.offset = opts.offset.or(Some(s));
                    if let Some(Ok(e)) = it.next().map(|x| x.trim_start_matches('L').parse::<usize>()) {
                        if e >= s {
                            opts.limit = opts.limit.or(Some(e - s + 1));
                        }
                    }
                    want = head.to_owned();
                }
            }
        }
    }
    if matches.is_empty() {
        // substring fallback
        let lw = want.to_lowercase();
        matches = g.view.files().filter(|f| g.path(*f).to_lowercase().contains(&lw)).take(26).collect();
    }
    if matches.len() > 1 {
        let mut lines = vec![format!("\"{want}\" matches {} indexed files — pass a longer path:", matches.len()), String::new()];
        lines.extend(matches.iter().take(25).map(|f| format!("- {}", g.path(*f))));
        return ToolOutput::ok(lines.join("\n"));
    }
    let Some(&f) = matches.first() else {
        return ToolOutput::ok(format!(
            "No indexed file matches \"{file_arg}\". Codegraph indexes source files; configs/docs it doesn't parse won't appear — Read those directly."
        ));
    };
    let path = g.path(f).to_owned();
    let test_label = if g.is_test_file(f) { " · test file" } else { "" };
    let mut nodes: Vec<NodeIdx> =
        g.view.nodes_in_file(f).map(NodeIdx).filter(|n| !matches!(g.kind(*n), NodeKind::File | NodeKind::Import | NodeKind::Export)).collect();
    nodes.sort_by_key(|n| g.line(*n));
    let dependents: Vec<FileId> = g.file_dependents(f, true);
    let dep_summary = if dependents.is_empty() {
        "no other indexed file depends on it".to_owned()
    } else {
        let names: Vec<&str> = dependents.iter().take(8).map(|d| g.path(*d)).collect();
        format!(
            "used by {} file{}: {}{}",
            dependents.len(),
            if dependents.len() == 1 { "" } else { "s" },
            names.join(", "),
            if dependents.len() > 8 { format!(", +{} more", dependents.len() - 8) } else { String::new() }
        )
    };
    let symbol_map = |heading: &str| -> Vec<String> {
        let mut lines = vec![heading.to_owned()];
        for n in nodes.iter().take(200) {
            let sig = g.view.node(*n).signature().map(|s| format!(" {}", s.split_whitespace().collect::<Vec<_>>().join(" "))).unwrap_or_default();
            lines.push(format!("- `{}` ({}){sig} — :{}", g.name(*n), g.kind(*n), g.line(*n)));
        }
        if nodes.len() > 200 {
            lines.push(format!("- … +{} more", nodes.len() - 200));
        }
        lines
    };
    if opts.symbols_only {
        let mut out = vec![format!("**{path}**{test_label} — {} symbol{}, {dep_summary}", nodes.len(), if nodes.len() == 1 { "" } else { "s" }), String::new()];
        if nodes.is_empty() {
            out.push("_No indexed symbols in this file._".into());
        } else {
            out.extend(symbol_map("**Symbols**"));
        }
        out.push(String::new());
        out.push(strings::symbols_only_footer(opts.mode).into());
        return ToolOutput::ok(truncate_output_to(out.join("\n"), opts.max_chars.unwrap_or(MAX_OUTPUT_LENGTH)));
    }
    let Some(abs) = g.abs_path(&path) else {
        return ToolOutput::err("Error: path escapes the project root");
    };
    let Ok(content) = std::fs::read(&abs).map(|b| String::from_utf8_lossy(&b).into_owned()) else {
        let mut out = vec![format!("**{path}**{test_label} — could not read from disk (it may have moved since indexing). {dep_summary}"), String::new()];
        if !nodes.is_empty() {
            out.extend(symbol_map("**Symbols**"));
        }
        out.push(String::new());
        out.push(format!("> Read `{path}` directly for its current content."));
        return ToolOutput::ok(truncate_output(out.join("\n")));
    };
    let file_lines: Vec<&str> = content.split('\n').collect();
    let total = if content.ends_with('\n') { file_lines.len() - 1 } else { file_lines.len() };
    let offset = opts.offset.unwrap_or(1).max(1);
    if offset > total {
        return ToolOutput::ok(format!(
            "**{path}**{test_label} has {total} line{} — offset {offset} is past the end. {dep_summary}",
            if total == 1 { "" } else { "s" }
        ));
    }
    let max_lines = opts.limit.unwrap_or(2000).max(1);
    let header = format!("**{path}**{test_label} — {total} lines, {} symbol{} · {dep_summary}", nodes.len(), if nodes.len() == 1 { "" } else { "s" });
    let char_budget = opts.max_chars.unwrap_or(38_000);
    let footer_reserve = 140usize;
    let body_budget = char_budget.saturating_sub(footer_reserve).max(200);
    let mut numbered: Vec<String> = Vec::new();
    let mut used = header.len() + 2;
    let mut line_clipped = false;
    let mut i = offset - 1;
    while i < total && numbered.len() < max_lines {
        let ln = format!("{}\t{}", i + 1, file_lines[i].strip_suffix('\r').unwrap_or(file_lines[i]));
        if used + ln.len() + 1 > body_budget {
            if numbered.is_empty() {
                // Clip an oversized single line (e.g. minified bundle) so `maxChars` is respected.
                const CLIP_NOTE: &str = " … (line truncated to fit budget)";
                let avail = body_budget.saturating_sub(used + CLIP_NOTE.len()).max(64);
                let mut cut = avail.min(ln.len());
                while cut > 0 && !ln.is_char_boundary(cut) {
                    cut -= 1;
                }
                numbered.push(format!("{}{CLIP_NOTE}", &ln[..cut]));
                line_clipped = true;
            }
            break;
        }
        used += ln.len() + 1;
        numbered.push(ln);
        i += 1;
    }
    let shown_end = offset - 1 + numbered.len();
    let complete = offset == 1 && shown_end >= total && !line_clipped;
    let mut meta = ToolMeta::default();
    if !g.file_is_stale(f) && !line_clipped && shown_end >= offset {
        let fp = g.fingerprint(f);
        if !opts.no_dedup {
            if let Some(hist) = history {
                let mut guard = hist.lock().unwrap_or_else(|e| e.into_inner());
                let covered = guard.is_covered(&path, fp, offset as u32, shown_end as u32);
                let body_bytes: usize = numbered.iter().map(|l| l.len() + 1).sum();
                if let Some((notice, saved)) = covered
                    .then(|| crate::explore::dedup_notice(&format!("lines {offset}-{shown_end} of `{path}`"), body_bytes, strings::dedup_copy_hint(opts.mode)))
                    .flatten()
                {
                    meta.dedup_hits += 1;
                    meta.dedup_saved_bytes += saved;
                    let mut out = vec![header, String::new(), notice];
                    if !complete {
                        out.push(String::new());
                        out.push(strings::file_view_range_footer(opts.mode, offset, shown_end, total, false));
                    }
                    return ToolOutput::ok_with_meta(out.join("\n"), meta);
                }
                if !covered {
                    guard.record_ranges(path.clone(), fp, vec![(offset as u32, shown_end as u32)]);
                }
            }
        }
    }
    let mut out = vec![header, String::new()];
    out.extend(numbered);
    if !complete {
        out.push(String::new());
        out.push(strings::file_view_range_footer(opts.mode, offset, shown_end, total, line_clipped));
    }
    ToolOutput::ok_with_meta(out.join("\n"), meta)
}

fn read_manifest_pkg_name(dir: &std::path::Path) -> Option<Option<String>> {
    for json_file in ["package.json", "composer.json"] {
        let p = dir.join(json_file);
        if p.is_file() {
            let name = std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned));
            return Some(name);
        }
    }
    for (toml_file, section) in [("Cargo.toml", "[package]"), ("pyproject.toml", "[project]")] {
        let p = dir.join(toml_file);
        if p.is_file() {
            let name = std::fs::read_to_string(&p).ok().and_then(|s| {
                let mut in_sec = false;
                for raw in s.lines() {
                    let line = raw.trim();
                    if line.starts_with('[') {
                        in_sec = line == section;
                        continue;
                    }
                    if in_sec {
                        if let Some((k, v)) = line.split_once('=') {
                            if k.trim() == "name" {
                                let val = v.trim().trim_matches('"').trim_matches('\'').trim();
                                if !val.is_empty() {
                                    return Some(val.to_owned());
                                }
                            }
                        }
                    }
                }
                None
            });
            return Some(name);
        }
    }
    let gomod = dir.join("go.mod");
    if gomod.is_file() {
        let name = std::fs::read_to_string(&gomod).ok().and_then(|s| {
            s.lines().map(str::trim).find_map(|l| l.strip_prefix("module ").map(|m| m.trim().trim_matches('"').to_owned()).filter(|m| !m.is_empty()))
        });
        return Some(name);
    }
    for build_file in ["pom.xml", "build.gradle", "build.gradle.kts"] {
        if dir.join(build_file).is_file() {
            return Some(None);
        }
    }
    None
}

fn package_label_for_file(g: &Graph, rel: &str, cache: &mut FxHashMap<String, Option<String>>) -> String {
    let mut cur = match rel.rsplit_once('/') {
        Some((head, _)) => head,
        None => "",
    };
    loop {
        if !cache.contains_key(cur) {
            let label = g.abs_path(cur).and_then(|abs| read_manifest_pkg_name(&abs)).map(|maybe_name| {
                let dir_disp = if cur.is_empty() { "." } else { cur };
                match maybe_name {
                    Some(n) if cur.is_empty() => format!("{n} (.)"),
                    Some(n) => format!("{n} ({dir_disp})"),
                    None => dir_disp.to_owned(),
                }
            });
            cache.insert(cur.to_owned(), label);
        }
        if let Some(Some(label)) = cache.get(cur) {
            return label.clone();
        }
        if cur.is_empty() {
            return ".".to_owned();
        }
        cur = match cur.rsplit_once('/') {
            Some((head, _)) => head,
            None => "",
        };
    }
}

fn handle_files(env: &ToolEnv<'_>, args: &Value) -> ToolOutput {
    let g = env.graph;
    let dir = str_arg(args, "path").map(|p| p.replace('\\', "/").trim_matches('/').to_owned());
    let pattern = str_arg(args, "pattern");
    let format = str_arg(args, "format").unwrap_or("tree");
    let meta = args.get("includeMetadata").and_then(|v| v.as_bool()).unwrap_or(true);
    let tests_only = args.get("testsOnly").and_then(|v| v.as_bool()).unwrap_or(false);
    let exclude_tests = args.get("excludeTests").and_then(|v| v.as_bool()).unwrap_or(false);
    let max_depth = num_arg(args, "maxDepth").map(|n| n as usize);
    let glob = pattern.and_then(|p| {
        let p = if p.contains('/') { p.to_owned() } else { format!("**/{p}") };
        globset::Glob::new(&p).ok().map(|g| g.compile_matcher())
    });
    let mut files: Vec<FileId> = g
        .view
        .files()
        .filter(|f| dir.as_ref().is_none_or(|d| g.path(*f) == d || g.path(*f).starts_with(&format!("{d}/"))))
        .filter(|f| glob.as_ref().is_none_or(|m| m.is_match(g.path(*f))))
        .filter(|f| (!tests_only || g.is_test_file(*f)) && (!exclude_tests || !g.is_test_file(*f)))
        .collect();
    files.sort_by(|a, b| g.path(*a).cmp(g.path(*b)));
    if files.is_empty() {
        return ToolOutput::ok("No files found matching the criteria.");
    }
    let sym_count = |f: FileId| {
        g.view
            .nodes_in_file(f)
            .map(NodeIdx)
            .filter(|n| !matches!(g.kind(*n), NodeKind::File | NodeKind::Import | NodeKind::Export | NodeKind::Parameter))
            .count()
    };
    let describe = |f: FileId| -> String {
        if meta {
            format!("{} symbols", sym_count(f))
        } else {
            String::new()
        }
    };
    let label = |f: FileId, name: &str| -> String {
        if meta {
            format!("{name} ({}, {})", g.view.file(f).lang(), describe(f))
        } else {
            name.to_owned()
        }
    };
    let mut lines = vec![format!("**Project Files ({})**", files.len()), String::new()];
    match format {
        "flat" => {
            for f in &files {
                lines.push(format!("- {}", label(*f, g.path(*f))));
            }
        }
        "grouped" => {
            let mut by_lang: std::collections::BTreeMap<String, Vec<FileId>> = std::collections::BTreeMap::new();
            for f in &files {
                by_lang.entry(g.view.file(*f).lang().to_string()).or_default().push(*f);
            }
            for (lang, fs) in by_lang {
                lines.push(format!("**{lang} ({})**", fs.len()));
                for f in fs {
                    lines.push(format!("- {}", label(f, g.path(f))));
                }
                lines.push(String::new());
            }
        }
        "package" => {
            let mut by_pkg: std::collections::BTreeMap<String, Vec<FileId>> = std::collections::BTreeMap::new();
            let mut cache: FxHashMap<String, Option<String>> = FxHashMap::default();
            for f in &files {
                let pkg = package_label_for_file(g, g.path(*f), &mut cache);
                by_pkg.entry(pkg).or_default().push(*f);
            }
            for (pkg, fs) in by_pkg {
                lines.push(format!("**{pkg} ({})**", fs.len()));
                for f in fs {
                    lines.push(format!("- {}", label(f, g.path(f))));
                }
                lines.push(String::new());
            }
        }
        _ => {
            let mut printed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for f in &files {
                let parts: Vec<&str> = g.path(*f).split('/').collect();
                for (i, part) in parts.iter().enumerate() {
                    let depth = i;
                    if max_depth.is_some_and(|m| depth >= m) {
                        break;
                    }
                    let key = parts[..=i].join("/");
                    if i + 1 == parts.len() {
                        lines.push(format!("{}{}", "  ".repeat(i), label(*f, part)));
                    } else if printed.insert(key) {
                        lines.push(format!("{}{part}/", "  ".repeat(i)));
                    }
                }
            }
        }
    }
    ToolOutput::ok(truncate_output_to(lines.join("\n"), cap_arg(args)))
}

fn handle_status(env: &ToolEnv<'_>) -> ToolOutput {
    let s = &env.status;
    let g = env.graph;
    let mut lines = vec![
        "**codegraph-rust Status**".to_owned(),
        String::new(),
        format!("**Files indexed:** {}", s.files),
        format!("**Total nodes:** {}", s.nodes),
        format!("**Total edges:** {}", s.edges),
        format!("**Index size:** {:.2} MB", s.bytes as f64 / 1024.0 / 1024.0),
        format!("**Latest index written:** {}", if s.built_at.is_empty() { "never".into() } else { s.built_at.clone() }),
        match &s.changes {
            Some(c) => format!("**Changes since index:** {c}"),
            None => "**Changes since index:** unknown (measurement skipped; do not assume the index is current)".to_owned(),
        },
        format!("**Backend:** memory-mapped immutable segments (generation {})", s.generation),
        format!("**Engine version:** {}", s.engine_version),
        format!("**Re-index recommended:** {}", if s.engine_outdated { "yes — the user can run `cgr index`" } else { "no" }),
    ];
    for n in &s.notes {
        lines.push(n.clone());
    }
    let mut by_kind: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
    let mut by_lang: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for f in g.view.files() {
        *by_lang.entry(g.view.file(f).lang().to_string()).or_default() += 1;
        for n in g.view.nodes_in_file(f) {
            *by_kind.entry(g.kind(NodeIdx(n)).as_str()).or_default() += 1;
        }
    }
    lines.push(String::new());
    lines.push("**Nodes by Kind:**".into());
    for (k, c) in by_kind {
        lines.push(format!("- {k}: {c}"));
    }
    lines.push(String::new());
    lines.push("**Languages:**".into());
    for (l, c) in by_lang {
        lines.push(format!("- {l}: {c}"));
    }
    ToolOutput::ok(lines.join("\n"))
}
