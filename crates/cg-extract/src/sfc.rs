//! Single-File Component (Vue `.vue` and Svelte `.svelte`) extraction.
//!
//! Instead of shipping separate HTML/Vue/Svelte tree-sitter C grammars, each SFC produces:
//! - Row 0: a `File` node spanning the full source.
//! - Row 1: an exported `Component` node named after the file stem (default export of the file).
//! - Each `<script>` block extracted through the existing TS/TSX/JS kernel and `facts::js`,
//!   folded back with exact line/column offsets and per-instance re-attribution.
//! - For Vue Options API `<script>` blocks, `Method` nodes for `methods`/`computed`/`watch` and
//!   component lifecycle hooks, with file-level refs/edges inside each method re-attributed to it.
//! - Template component tag usages (`<Modal>`, `<my-button>`) and template call expressions
//!   (`{{ fn() }}`, `@click="save(x)"`, `{fn()}`), attributed to the `Component` node.

use crate::facts::{ExportFact, Facts, ScopeFact};
use crate::pipeline::{run_script_kernel, ExtractConfig, ScriptOutcome};
use crate::post::node_digest;
use crate::rows::FileRows;
use cg_core::text::{col16, line_starts, pack_pos, unpack_pos};
use cg_core::{EdgeKind, Lang, NodeKind, Provenance, RefKind, Visibility, NONE};
use cg_kernel::{EmitOut, EDGE_ROW_SIZE, KERNEL_ABI_VERSION, META_SIZE, NODE_ROW_SIZE, REF_FLAG_FILE_PATH, REF_ROW_SIZE};
use rustc_hash::{FxHashMap, FxHashSet};

const VUE_BUILTIN_COMPONENTS: &[&str] = &["Transition", "TransitionGroup", "KeepAlive", "Suspense", "Teleport", "Component", "Slot"];

const SVELTE_RUNES: &[&str] = &["$props", "$state", "$derived", "$effect", "$bindable", "$inspect", "$host", "$snippet"];

const VUE_COMPONENT_FUNCTIONS: &[&str] = &[
    "data",
    "setup",
    "render",
    "beforeCreate",
    "created",
    "beforeMount",
    "mounted",
    "beforeUpdate",
    "updated",
    "activated",
    "deactivated",
    "beforeDestroy",
    "destroyed",
    "beforeUnmount",
    "unmounted",
    "errorCaptured",
    "renderTracked",
    "renderTriggered",
    "serverPrefetch",
    // Nuxt 2
    "asyncData",
    "fetch",
    "head",
    "validate",
    "middleware",
];

const VUE_MEMBER_GROUPS: &[&str] = &["methods", "computed", "watch"];

const VOID_ELEMENTS: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

const TEMPLATE_GLOBALS: &[&str] = &[
    "Infinity",
    "undefined",
    "NaN",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "Math",
    "Number",
    "Date",
    "Array",
    "Object",
    "Boolean",
    "String",
    "RegExp",
    "Map",
    "Set",
    "JSON",
    "Intl",
    "BigInt",
    "console",
    "Error",
    "Symbol",
];

const JS_KEYWORDS: &[&str] = &[
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "default",
    "break",
    "continue",
    "return",
    "throw",
    "try",
    "catch",
    "finally",
    "function",
    "class",
    "new",
    "delete",
    "typeof",
    "void",
    "instanceof",
    "in",
    "of",
    "var",
    "let",
    "const",
    "this",
    "super",
    "import",
    "export",
    "await",
    "async",
    "yield",
    "with",
    "debugger",
    "true",
    "false",
    "null",
    "undefined",
];

const VALUE_KEYWORDS: &[&str] = &["this", "super", "true", "false", "null", "undefined"];

/// Result of extracting one Vue or Svelte SFC file.
pub(crate) struct SfcOut {
    pub emit: EmitOut,
    pub facts: Facts,
    pub errors: Vec<String>,
    pub had_error_tree: bool,
    pub timed_out: bool,
}

// ---------------------------------------------------------------------------
// EmitOut binary table builder (ABI v2, matching cg-kernel/src/buffers.rs)
// ---------------------------------------------------------------------------

struct EmitBuilder {
    nodes: Vec<u8>,
    edges: Vec<u8>,
    refs: Vec<u8>,
    arena: Vec<u8>,
    node_count: u32,
    edge_count: u32,
    ref_count: u32,
}

struct NodeSpec<'a> {
    kind: NodeKind,
    vis: Visibility,
    raw_flags: u16,
    start_line: u32,
    end_line: u32,
    start_col: u32,
    end_col: u32,
    name: &'a str,
    qname: &'a str,
    id: &'a str,
    docstring: Option<&'a str>,
    signature: Option<&'a str>,
    decorators: Option<&'a str>,
    type_params: Option<&'a str>,
    return_type: Option<&'a str>,
    extra_json: Option<&'a str>,
}

struct EdgeSpec<'a> {
    src: u32,
    dst: u32,
    kind: EdgeKind,
    prov: Provenance,
    line: Option<u32>,
    col: Option<u32>,
    meta: Option<&'a str>,
}

struct RefSpec<'a> {
    from: u32,
    kind: RefKind,
    carries_file_path: bool,
    line: u32,
    col: u32,
    name: &'a str,
    candidates: Option<&'a str>,
}

impl EmitBuilder {
    fn new() -> Self {
        Self { nodes: Vec::new(), edges: Vec::new(), refs: Vec::new(), arena: Vec::new(), node_count: 0, edge_count: 0, ref_count: 0 }
    }

    fn put(&mut self, s: &str) -> (u32, u32) {
        let o = self.arena.len() as u32;
        self.arena.extend_from_slice(s.as_bytes());
        (o, s.len() as u32)
    }

    fn put_opt(&mut self, s: Option<&str>) -> (u32, u32) {
        match s {
            Some(s) => self.put(s),
            None => (NONE, 0),
        }
    }

    fn push_node(&mut self, n: NodeSpec<'_>) -> u32 {
        let row = self.node_count;
        self.node_count += 1;
        let start = self.nodes.len();
        self.nodes.push(n.kind as u8);
        self.nodes.push(n.vis as u8);
        self.nodes.extend_from_slice(&n.raw_flags.to_le_bytes());
        self.nodes.extend_from_slice(&n.start_line.to_le_bytes());
        self.nodes.extend_from_slice(&n.end_line.to_le_bytes());
        self.nodes.extend_from_slice(&n.start_col.to_le_bytes());
        self.nodes.extend_from_slice(&n.end_col.to_le_bytes());
        for (o, l) in [
            self.put(n.name),
            self.put(n.qname),
            self.put(n.id),
            self.put_opt(n.docstring),
            self.put_opt(n.signature),
            self.put_opt(n.decorators),
            self.put_opt(n.type_params),
            self.put_opt(n.return_type),
            self.put_opt(n.extra_json),
        ] {
            self.nodes.extend_from_slice(&o.to_le_bytes());
            self.nodes.extend_from_slice(&l.to_le_bytes());
        }
        self.nodes.extend_from_slice(&0u32.to_le_bytes()); // metrics slot
        debug_assert_eq!(self.nodes.len() - start, NODE_ROW_SIZE);
        row
    }

    fn push_edge(&mut self, e: EdgeSpec<'_>) {
        self.edge_count += 1;
        let start = self.edges.len();
        self.edges.extend_from_slice(&e.src.to_le_bytes());
        self.edges.extend_from_slice(&e.dst.to_le_bytes());
        self.edges.push(e.kind as u8);
        self.edges.push(e.prov as u8);
        self.edges.extend_from_slice(&0u16.to_le_bytes());
        self.edges.extend_from_slice(&e.line.unwrap_or(NONE).to_le_bytes());
        self.edges.extend_from_slice(&e.col.unwrap_or(NONE).to_le_bytes());
        for (o, l) in [self.put_opt(e.meta), (NONE, 0), (NONE, 0)] {
            self.edges.extend_from_slice(&o.to_le_bytes());
            self.edges.extend_from_slice(&l.to_le_bytes());
        }
        debug_assert_eq!(self.edges.len() - start, EDGE_ROW_SIZE);
    }

    fn push_ref(&mut self, r: RefSpec<'_>) {
        self.ref_count += 1;
        let start = self.refs.len();
        self.refs.extend_from_slice(&r.from.to_le_bytes());
        self.refs.push(r.kind.to_wire());
        self.refs.push(if r.carries_file_path { REF_FLAG_FILE_PATH } else { 0 });
        self.refs.extend_from_slice(&0u16.to_le_bytes());
        self.refs.extend_from_slice(&r.line.to_le_bytes());
        self.refs.extend_from_slice(&r.col.to_le_bytes());
        for (o, l) in [self.put(r.name), self.put_opt(r.candidates), (NONE, 0)] {
            self.refs.extend_from_slice(&o.to_le_bytes());
            self.refs.extend_from_slice(&l.to_le_bytes());
        }
        debug_assert_eq!(self.refs.len() - start, REF_ROW_SIZE);
    }

    fn finish(self) -> EmitOut {
        let mut meta = Vec::with_capacity(META_SIZE);
        meta.push(KERNEL_ABI_VERSION);
        meta.extend_from_slice(&[0u8; 3]);
        for v in [self.node_count, self.edge_count, self.ref_count, self.arena.len() as u32, NONE, 0] {
            meta.extend_from_slice(&v.to_le_bytes());
        }
        meta.extend_from_slice(&0f64.to_le_bytes());
        debug_assert_eq!(meta.len(), META_SIZE);
        EmitOut { meta, nodes: self.nodes, edges: self.edges, refs: self.refs, arena: self.arena }
    }
}

fn make_node_id(rel: &str, kind: NodeKind, name: &str, line: u32, col_suffix: Option<&str>) -> String {
    let hex = cg_core::hash::hex(&node_digest(rel, kind, name, line));
    match col_suffix {
        Some(c) => format!("{}:{hex}:{c}", kind.as_str()),
        None => format!("{}:{hex}", kind.as_str()),
    }
}

fn id_col_suffix(id: &str) -> Option<&str> {
    let (_, rest) = id.split_once(':')?;
    let (_, col) = rest.split_once(':')?;
    col.bytes().all(|b| b.is_ascii_digit()).then_some(col)
}

// ---------------------------------------------------------------------------
// Script / Style block scanner (comment-aware, quote-aware, self-closing-aware)
// ---------------------------------------------------------------------------

struct ScriptBlock<'a> {
    content: &'a str,
    line_offset: u32,
    col_offset_line1: u32,
    script_lang: Lang,
    per_instance: bool,
}

/// Find `needle` in `hay` (byte-level, always UTF-8 safe).
#[inline]
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Mask HTML comments `<!-- ... -->` with spaces, preserving newlines and byte offsets.
fn mask_html_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    while i + 3 < b.len() {
        if &b[i..i + 4] == b"<!--" {
            let end = find_bytes(&b[i + 4..], b"-->").map_or(b.len(), |k| i + 4 + k + 3);
            for ch in &mut out[i..end] {
                if *ch != b'\n' && *ch != b'\r' {
                    *ch = b' ';
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Find the end of an opening HTML tag starting at `<`, skipping quoted attribute values
/// (handles Vue 3.3 `generic="T extends Record<string, any>"`).
/// Returns `(gt_byte_index, self_closing)`.
fn scan_open_tag_end(b: &[u8], lt: usize) -> Option<(usize, bool)> {
    let mut i = lt + 1;
    let mut quote = 0u8;
    while i < b.len() {
        let ch = b[i];
        if quote != 0 {
            if ch == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if ch == b'"' || ch == b'\'' {
            quote = ch;
            i += 1;
            continue;
        }
        if ch == b'>' {
            let mut prev = i;
            while prev > lt + 1 && b[prev - 1].is_ascii_whitespace() {
                prev -= 1;
            }
            let self_closing = prev > lt + 1 && b[prev - 1] == b'/';
            return Some((i, self_closing));
        }
        i += 1;
    }
    None
}

/// Case-insensitive search for `</tag\s*>` starting at `from`.
/// Returns `(close_start, close_end_exclusive)`.
fn find_close_tag(b: &[u8], from: usize, tag: &[u8]) -> Option<(usize, usize)> {
    let mut i = from;
    let n = tag.len();
    while i + n + 2 < b.len() {
        if b[i] == b'<' && b[i + 1] == b'/' && b[i + 2..i + 2 + n].eq_ignore_ascii_case(tag) {
            let mut j = i + 2 + n;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b'>' {
                return Some((i, j + 1));
            }
        }
        i += 1;
    }
    None
}

/// Collect byte spans `(element_start, attrs_start, attrs_end, content_start, content_end, element_end, is_script)`
/// for every non-self-closing `<script>` and `<style>` element in `masked` (HTML comments already masked).
fn find_script_and_style_blocks(masked: &str) -> Vec<(usize, usize, usize, usize, usize, usize, bool)> {
    let b = masked.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let (is_script, tag_bytes): (bool, &[u8]) = if b.get(i + 1..i + 7).is_some_and(|s| s.eq_ignore_ascii_case(b"script")) {
            (true, b"script")
        } else if b.get(i + 1..i + 6).is_some_and(|s| s.eq_ignore_ascii_case(b"style")) {
            (false, b"style")
        } else {
            i += 1;
            continue;
        };
        let after_name = i + 1 + tag_bytes.len();
        if after_name < b.len() && !b[after_name].is_ascii_whitespace() && b[after_name] != b'>' && b[after_name] != b'/' {
            i += 1;
            continue;
        }
        let Some((gt, self_closing)) = scan_open_tag_end(b, i) else {
            break;
        };
        if self_closing {
            i = gt + 1;
            continue;
        }
        let content_start = gt + 1;
        let Some((close_start, close_end)) = find_close_tag(b, content_start, tag_bytes) else {
            i = gt + 1;
            continue;
        };
        out.push((i, after_name, gt, content_start, close_start, close_end, is_script));
        i = close_end;
    }
    out
}

/// Parse HTML opening-tag attributes into `(name, Option<value>)` pairs (quote-aware).
fn parse_tag_attrs(attrs: &str) -> Vec<(&str, Option<&str>)> {
    let b = attrs.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let name_start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/' | b'"' | b'\'') {
            i += 1;
        }
        if i == name_start {
            i += 1;
            continue;
        }
        let name = &attrs[name_start..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                let vs = i;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                out.push((name, Some(&attrs[vs..i])));
                if i < b.len() {
                    i += 1;
                }
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                out.push((name, Some(&attrs[vs..i])));
            }
        } else {
            out.push((name, None));
        }
    }
    out
}

fn sfc_script_lang(attrs: &[(&str, Option<&str>)], host: Lang) -> Lang {
    for (k, v) in attrs {
        if k.eq_ignore_ascii_case("lang") {
            if let Some(val) = v {
                let l = val.trim().to_ascii_lowercase();
                if l == "ts" || l == "typescript" {
                    return Lang::TypeScript;
                }
                if host == Lang::Vue && l == "tsx" {
                    return Lang::Tsx;
                }
            }
        }
    }
    Lang::JavaScript
}

fn extract_script_blocks<'a>(src: &'a str, blocks: &[(usize, usize, usize, usize, usize, usize, bool)], starts: &[u32], host: Lang) -> Vec<ScriptBlock<'a>> {
    let mut out = Vec::new();
    for &(_, a_start, a_end, c_start, c_end, _, is_script) in blocks {
        if !is_script {
            continue;
        }
        let attrs = parse_tag_attrs(&src[a_start..a_end]);
        let script_lang = sfc_script_lang(&attrs, host);
        let per_instance = match host {
            Lang::Vue => attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("setup")),
            Lang::Svelte => !attrs.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("module") || (k.eq_ignore_ascii_case("context") && v.is_some_and(|val| val.trim().eq_ignore_ascii_case("module")))
            }),
            _ => false,
        };
        let line_idx = starts.partition_point(|&s| (s as usize) <= c_start).saturating_sub(1);
        let ls = starts[line_idx] as usize;
        let col_offset_line1 = col16(&src[ls..c_start], c_start - ls);
        out.push(ScriptBlock { content: &src[c_start..c_end], line_offset: line_idx as u32, col_offset_line1, script_lang, per_instance });
    }
    out
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub(crate) fn extract_sfc(rel: &str, src: &str, host: Lang, cfg: &ExtractConfig) -> SfcOut {
    let starts = line_starts(src);
    let total_lines = starts.len() as u32;
    let last_line = &src[*starts.last().unwrap_or(&0) as usize..];
    let last_col = col16(last_line, last_line.len());

    let file_name = rel.rsplit('/').next().unwrap_or(rel);
    let comp_name = match host {
        Lang::Vue => file_name.strip_suffix(".vue").or_else(|| file_name.strip_suffix(".VUE")).unwrap_or(file_name),
        Lang::Svelte => file_name.strip_suffix(".svelte").or_else(|| file_name.strip_suffix(".SVELTE")).unwrap_or(file_name),
        _ => file_name,
    };

    let mut eb = EmitBuilder::new();
    // Row 0: File node
    let file_id = format!("file:{rel}");
    eb.push_node(NodeSpec {
        kind: NodeKind::File,
        vis: Visibility::None,
        raw_flags: 0b01,
        start_line: 1,
        end_line: total_lines,
        start_col: 0,
        end_col: 0,
        name: file_name,
        qname: rel,
        id: &file_id,
        docstring: None,
        signature: None,
        decorators: None,
        type_params: None,
        return_type: None,
        extra_json: None,
    });
    // Row 1: Component node (always exported)
    let comp_id = make_node_id(rel, NodeKind::Component, comp_name, 1, None);
    eb.push_node(NodeSpec {
        kind: NodeKind::Component,
        vis: Visibility::None,
        raw_flags: 0b11, // isExported present & true
        start_line: 1,
        end_line: total_lines,
        start_col: 0,
        end_col: last_col,
        name: comp_name,
        qname: comp_name,
        id: &comp_id,
        docstring: None,
        signature: None,
        decorators: None,
        type_params: None,
        return_type: None,
        extra_json: None,
    });
    eb.push_edge(EdgeSpec { src: 0, dst: 1, kind: EdgeKind::Contains, prov: Provenance::TreeSitter, line: None, col: None, meta: None });

    let mut merged_facts = Facts::new(pack_pos(total_lines + 1, 0));
    let mut errors = Vec::new();
    let mut had_error_tree = false;
    let mut timed_out = false;

    let masked = mask_html_comments(src);
    let blocks = find_script_and_style_blocks(&masked);
    let script_blocks = extract_script_blocks(src, &blocks, &starts, host);

    // Track Vue Options API methods (name -> row) so template bare calls link directly to them.
    let mut vue_own_methods: FxHashMap<String, u32> = FxHashMap::default();
    let mut fold_ctx = FoldCtx { rel, comp_name, host, eb: &mut eb, merged: &mut merged_facts, vue_own_methods: &mut vue_own_methods };

    for block in &script_blocks {
        if block.content.trim().is_empty() {
            continue;
        }
        match run_script_kernel(rel, block.content, block.script_lang, cfg) {
            ScriptOutcome::Ok { emit, facts, had_error_tree: err_tree, facts_failed } => {
                had_error_tree |= err_tree;
                if facts_failed {
                    errors.push("panic while building facts".to_owned());
                }
                if facts.timed_out {
                    timed_out = true;
                    errors.push(format!("supplementary facts parse timed out after {:?}", cfg.parse_timeout));
                }
                fold_script_block(&mut fold_ctx, block, &emit, &facts);
            }
            ScriptOutcome::TimedOut => {
                timed_out = true;
                errors.push(format!("parse timed out after {:?}", cfg.parse_timeout));
            }
            ScriptOutcome::Failed(m) => {
                had_error_tree = true;
                errors.push(m);
            }
        }
    }

    // The SFC's Component node (row 1) is always the module's default export.
    merged_facts.exports.push(ExportFact { name: "default".to_owned(), node_row: 1, from_spec: String::new(), from_name: String::new(), star: false });

    // Blank out <script> and <style> bodies for template scanning while keeping exact byte offsets.
    let mut tpl_masked = masked.into_bytes();
    for &(el_start, _, _, _, _, el_end, _) in &blocks {
        for ch in &mut tpl_masked[el_start..el_end] {
            if *ch != b'\n' && *ch != b'\r' {
                *ch = b' ';
            }
        }
    }
    let tpl_src = String::from_utf8(tpl_masked).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());

    match host {
        Lang::Vue => {
            extract_vue_template_components(&tpl_src, &starts, &mut eb);
            extract_vue_template_calls(&tpl_src, &starts, &vue_own_methods, &mut eb);
        }
        Lang::Svelte => {
            extract_svelte_template_components(&tpl_src, &starts, &mut eb);
            extract_svelte_template_calls(&tpl_src, &starts, &mut eb);
        }
        _ => {}
    }

    SfcOut { emit: eb.finish(), facts: merged_facts, errors, had_error_tree, timed_out }
}

// ---------------------------------------------------------------------------
// Folding one <script> block into the SFC's merged EmitOut + Facts
// ---------------------------------------------------------------------------

struct FoldCtx<'a> {
    rel: &'a str,
    comp_name: &'a str,
    host: Lang,
    eb: &'a mut EmitBuilder,
    merged: &'a mut Facts,
    vue_own_methods: &'a mut FxHashMap<String, u32>,
}

fn fold_script_block(ctx: &mut FoldCtx<'_>, block: &ScriptBlock<'_>, emit: &EmitOut, block_facts: &Facts) {
    let rel = ctx.rel;
    let comp_name = ctx.comp_name;
    let host = ctx.host;
    let eb = &mut *ctx.eb;
    let merged = &mut *ctx.merged;
    let vue_own_methods = &mut *ctx.vue_own_methods;
    let rows = FileRows::new(emit);
    let kcount = rows.node_count();
    let shift_line = |l: u32| -> u32 { l.saturating_add(block.line_offset) };
    let shift_col = |l: u32, c: u32| -> u32 {
        if l == 1 {
            c.saturating_add(block.col_offset_line1)
        } else {
            c
        }
    };
    let shift_pos = |pos: u64| -> u64 {
        let (l, c) = unpack_pos(pos);
        if l == 0 {
            return 0;
        }
        pack_pos(shift_line(l), shift_col(l, c))
    };

    // Map kernel string ids -> block row index (only needed if an edge/ref carries id instead of row).
    let mut id_to_old: Option<FxHashMap<&str, u32>> = None;
    let mut resolve_old = |row_opt: Option<u32>, id_opt: Option<&str>| -> Option<u32> {
        if let Some(r) = row_opt {
            return ((r as usize) < kcount).then_some(r);
        }
        let id = id_opt?;
        let map = id_to_old.get_or_insert_with(|| rows.nodes().enumerate().map(|(i, n)| (n.id(), i as u32)).collect());
        map.get(id).copied()
    };

    let file_old_row = rows.nodes().position(|n| n.kind() == NodeKind::File).map(|i| i as u32);

    // Which nodes already have a non-file `contains` parent inside this block.
    let mut parented = vec![false; kcount];
    for e in rows.edges() {
        if e.kind() == EdgeKind::Contains {
            let src = resolve_old(e.source_row(), e.source_id());
            let dst = resolve_old(e.target_row(), e.target_id());
            if let (Some(s), Some(d)) = (src, dst) {
                if Some(s) != file_old_row {
                    parented[d as usize] = true;
                }
            }
        }
    }

    // Top-level const/var nodes in a per-instance block run once per component instance.
    let mut instance_values = vec![false; kcount];
    if block.per_instance {
        for (i, n) in rows.nodes().enumerate() {
            if Some(i as u32) != file_old_row && !parented[i] && matches!(n.kind(), NodeKind::Constant | NodeKind::Variable) {
                instance_values[i] = true;
            }
        }
    }

    // Copy non-file nodes and build `old_row -> new_row`.
    let mut row_map = vec![NONE; kcount];
    for (i, n) in rows.nodes().enumerate() {
        if Some(i as u32) == file_old_row {
            row_map[i] = 0; // File row
            continue;
        }
        let sl = shift_line(n.start_line());
        let el = shift_line(n.end_line());
        let sc = shift_col(n.start_line(), n.start_col());
        let ec = shift_col(n.end_line(), n.end_col());
        let id = if block.line_offset > 0 { make_node_id(rel, n.kind(), n.name(), sl, id_col_suffix(n.id())) } else { n.id().to_owned() };
        let new_row = eb.push_node(NodeSpec {
            kind: n.kind(),
            vis: n.visibility(),
            raw_flags: n.raw_flags(),
            start_line: sl,
            end_line: el,
            start_col: sc,
            end_col: ec,
            name: n.name(),
            qname: n.qname(),
            id: &id,
            docstring: n.docstring(),
            signature: n.signature(),
            decorators: n.decorators(),
            type_params: n.type_params(),
            return_type: n.return_type(),
            extra_json: n.extra_json(),
        });
        row_map[i] = new_row;
        if !parented[i] {
            eb.push_edge(EdgeSpec { src: 1, dst: new_row, kind: EdgeKind::Contains, prov: Provenance::TreeSitter, line: None, col: None, meta: None });
        }
    }

    // Vue Options API: create Method nodes for `methods`, `computed`, `watch`, and lifecycle hooks.
    // Stored as `(rel_start_line, rel_end_line, new_row)` for re-attributing file-level refs/edges.
    let mut options_methods: Vec<(u32, u32, u32)> = Vec::new();
    if host == Lang::Vue && !block.per_instance {
        let b_starts = line_starts(block.content);
        let line_col = |off: usize| -> (u32, u32) {
            let li = b_starts.partition_point(|&s| (s as usize) <= off).saturating_sub(1);
            let ls = b_starts[li] as usize;
            let le = b_starts.get(li + 1).map_or(block.content.len(), |&v| v as usize);
            (li as u32 + 1, col16(&block.content[ls..le], off.saturating_sub(ls)))
        };
        for m in vue_options_members(block.content) {
            let (rsl, rsc) = line_col(m.start);
            let (rel_end, rec) = line_col(m.end);
            let sl = shift_line(rsl);
            let el = shift_line(rel_end);
            let sc = shift_col(rsl, rsc);
            let ec = shift_col(rel_end, rec);
            let qname = format!("{comp_name}::{}", m.name);
            let id_name = format!("{comp_name}.{}", m.name);
            let id = make_node_id(rel, NodeKind::Method, &id_name, sl, None);
            let new_row = eb.push_node(NodeSpec {
                kind: NodeKind::Method,
                vis: Visibility::None,
                raw_flags: 0,
                start_line: sl,
                end_line: el,
                start_col: sc,
                end_col: ec,
                name: &m.name,
                qname: &qname,
                id: &id,
                docstring: None,
                signature: None,
                decorators: None,
                type_params: None,
                return_type: None,
                extra_json: None,
            });
            eb.push_edge(EdgeSpec { src: 1, dst: new_row, kind: EdgeKind::Contains, prov: Provenance::TreeSitter, line: None, col: None, meta: None });
            vue_own_methods.entry(m.name).or_insert(new_row);
            options_methods.push((rsl, rel_end, new_row));
        }
    }

    let option_method_at = |rel_line: u32| -> Option<u32> {
        let mut best: Option<(u32, u32)> = None;
        for &(sl, el, row) in &options_methods {
            if sl <= rel_line && rel_line <= el && best.is_none_or(|(bsl, _)| sl >= bsl) {
                best = Some((sl, row));
            }
        }
        best.map(|(_, row)| row)
    };

    // Fold edges.
    for e in rows.edges() {
        let (Some(old_src), Some(old_dst)) = (resolve_old(e.source_row(), e.source_id()), resolve_old(e.target_row(), e.target_id())) else {
            continue;
        };
        if e.kind() == EdgeKind::Contains && Some(old_src) == file_old_row {
            continue;
        }
        let mut new_src = row_map[old_src as usize];
        let new_dst = row_map[old_dst as usize];
        if new_src == NONE || new_dst == NONE {
            continue;
        }
        if e.kind() != EdgeKind::Contains && Some(old_src) == file_old_row {
            if let Some(m_row) = e.line().and_then(option_method_at) {
                new_src = m_row;
            }
        }
        if block.per_instance
            && (Some(old_src) == file_old_row || instance_values[old_src as usize])
            && !matches!(e.kind(), EdgeKind::Imports | EdgeKind::Contains)
        {
            new_src = 1;
        }
        let shifted_line = e.line().map(shift_line);
        let shifted_col = match (e.line(), e.col()) {
            (Some(l), Some(c)) => Some(shift_col(l, c)),
            (_, c) => c,
        };
        eb.push_edge(EdgeSpec {
            src: new_src,
            dst: new_dst,
            kind: e.kind(),
            prov: e.provenance(),
            line: shifted_line,
            col: shifted_col,
            meta: e.metadata_json(),
        });
    }

    // Fold refs.
    for r in rows.refs() {
        let old_from = resolve_old(r.from_row(), r.from_id()).unwrap_or(0);
        let mut new_from = row_map.get(old_from as usize).copied().filter(|&v| v != NONE).unwrap_or(0);
        if Some(old_from) == file_old_row {
            if let Some(m_row) = option_method_at(r.line()) {
                new_from = m_row;
            }
        }
        let is_instance_ref = matches!(r.kind(), RefKind::FunctionRef | RefKind::Edge(EdgeKind::Calls | EdgeKind::References | EdgeKind::Instantiates));
        if block.per_instance && is_instance_ref && (Some(old_from) == file_old_row || instance_values.get(old_from as usize).copied().unwrap_or(false)) {
            new_from = 1;
        }
        eb.push_ref(RefSpec {
            from: new_from,
            kind: r.kind(),
            carries_file_path: r.carries_file_path(),
            line: shift_line(r.line()),
            col: shift_col(r.line(), r.col()),
            name: r.name(),
            candidates: r.candidates(),
        });
    }

    // Fold Facts (scopes, imports, exports, bindings, call_names, call_args).
    let mut scope_map = vec![0u32; block_facts.scopes.len()];
    for (i, sc) in block_facts.scopes.iter().enumerate().skip(1) {
        let new_idx = merged.scopes.len() as u32;
        scope_map[i] = new_idx;
        let parent = if sc.parent == NONE { 0 } else { scope_map.get(sc.parent as usize).copied().unwrap_or(0) };
        merged.scopes.push(ScopeFact { kind: sc.kind, parent, start: shift_pos(sc.start), end: shift_pos(sc.end) });
    }
    let map_scope = |s: u32| -> u32 {
        if s == NONE {
            NONE
        } else {
            scope_map.get(s as usize).copied().unwrap_or(0)
        }
    };
    let map_node = |r: u32| -> u32 {
        if r == NONE {
            NONE
        } else {
            row_map.get(r as usize).copied().unwrap_or(NONE)
        }
    };

    for imp in &block_facts.imports {
        let mut f = imp.clone();
        f.scope = map_scope(f.scope);
        f.line = shift_line(f.line);
        merged.imports.push(f);
    }
    for ex in &block_facts.exports {
        // The SFC's Component node is always the default export; skip anonymous `export default { ... }`.
        if ex.name == "default" && ex.node_row == NONE && ex.from_spec.is_empty() && ex.from_name.is_empty() {
            continue;
        }
        let mut f = ex.clone();
        f.node_row = map_node(f.node_row);
        merged.exports.push(f);
    }
    for bd in &block_facts.bindings {
        let mut f = bd.clone();
        f.scope = map_scope(f.scope);
        f.node_row = map_node(f.node_row);
        if f.line > 0 {
            f.line = shift_line(f.line);
        }
        merged.bindings.push(f);
    }
    for cn in &block_facts.call_names {
        let mut f = cn.clone();
        f.col = shift_col(f.line, f.col);
        f.line = shift_line(f.line);
        merged.call_names.push(f);
    }
    for ca in &block_facts.call_args {
        let mut f = ca.clone();
        f.col = shift_col(f.line, f.col);
        f.line = shift_line(f.line);
        merged.call_args.push(f);
    }
}

// ---------------------------------------------------------------------------
// Vue Options API member scanner (port of upstream vue-options-api.ts)
// ---------------------------------------------------------------------------

struct OptionsMember {
    name: String,
    start: usize,
    end: usize,
}

struct RawMember {
    name: String,
    start: usize,
    value_at: usize,
    end: usize,
    shorthand: bool,
}

fn skip_quoted(b: &[u8], at: usize) -> Option<usize> {
    let q = b[at];
    let mut i = at + 1;
    while i < b.len() {
        let ch = b[i];
        if ch == b'\\' {
            i += 2;
            continue;
        }
        if q == b'`' && ch == b'$' && b.get(i + 1) == Some(&b'{') {
            let close = match_close(b, i + 1)?;
            i = close + 1;
            continue;
        }
        if ch == q {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn match_close(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        let ch = b[i];
        if matches!(ch, b'"' | b'\'' | b'`') {
            i = skip_quoted(b, i)?;
            continue;
        }
        if ch == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if ch == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        match ch {
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn skip_ws_and_comments(b: &[u8], mut i: usize, end: usize) -> usize {
    while i < end {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < end && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < end && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(end);
        } else {
            break;
        }
    }
    i
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn object_members(s: &str, open: usize) -> Vec<RawMember> {
    let b = s.as_bytes();
    let Some(close) = match_close(b, open) else { return Vec::new() };
    let mut out = Vec::new();
    let mut i = open + 1;
    while i < close {
        i = skip_ws_and_comments(b, i, close);
        while i < close && b[i] == b',' {
            i = skip_ws_and_comments(b, i + 1, close);
        }
        if i >= close {
            break;
        }
        let start = i;
        // Spread `...mapMethods(...)`
        if b.get(i..i + 3) == Some(b"...") {
            let k = scan_member_end(b, i + 3, close);
            i = k + 1;
            continue;
        }
        // Optional `async` / `get` / `set` modifier before property name
        let mut p = i;
        for kw in [b"async".as_slice(), b"get".as_slice(), b"set".as_slice()] {
            if b[p..close].starts_with(kw) {
                let after = p + kw.len();
                if after < close && b[after].is_ascii_whitespace() {
                    let next = skip_ws_and_comments(b, after, close);
                    if next < close && (is_ident_start(b[next]) || matches!(b[next], b'\'' | b'"' | b'[' | b'*')) {
                        p = next;
                        break;
                    }
                }
            }
        }
        if p < close && b[p] == b'*' {
            p = skip_ws_and_comments(b, p + 1, close);
        }
        let (name, after_key): (Option<String>, usize) = if p < close && is_ident_start(b[p]) {
            let ks = p;
            while p < close && is_ident_char(b[p]) {
                p += 1;
            }
            (Some(s[ks..p].to_owned()), p)
        } else if p < close && (b[p] == b'\'' || b[p] == b'"') {
            let q = b[p];
            let ks = p + 1;
            p += 1;
            while p < close && b[p] != q {
                if b[p] == b'\\' {
                    p += 1;
                }
                p += 1;
            }
            let name = s[ks..p.min(close)].to_owned();
            (Some(name), (p + 1).min(close))
        } else if p < close && b[p] == b'[' {
            let Some(end_br) = match_close(b, p) else { break };
            (None, (end_br + 1).min(close))
        } else {
            i += 1;
            continue;
        };

        let k = scan_member_end(b, after_key, close);
        if let Some(name) = name {
            let after_ws = skip_ws_and_comments(b, after_key, k);
            if after_ws < k && b[after_ws] == b':' {
                out.push(RawMember { name, start, value_at: after_ws + 1, end: k, shorthand: false });
            } else if after_ws < k && b[after_ws] == b'(' {
                out.push(RawMember { name, start, value_at: after_ws, end: k, shorthand: true });
            }
        }
        i = k + 1;
    }
    out
}

fn scan_member_end(b: &[u8], mut k: usize, close: usize) -> usize {
    while k < close {
        let ch = b[k];
        if matches!(ch, b'"' | b'\'' | b'`') {
            match skip_quoted(b, k) {
                Some(end) => {
                    k = end.min(close);
                    continue;
                }
                None => return close,
            }
        }
        if ch == b'/' && b.get(k + 1) == Some(&b'/') {
            while k < close && b[k] != b'\n' {
                k += 1;
            }
            continue;
        }
        if ch == b'/' && b.get(k + 1) == Some(&b'*') {
            k += 2;
            while k + 1 < close && !(b[k] == b'*' && b[k + 1] == b'/') {
                k += 1;
            }
            k = (k + 2).min(close);
            continue;
        }
        if matches!(ch, b'{' | b'[' | b'(') {
            match match_close(b, k) {
                Some(end) => {
                    k = (end + 1).min(close);
                    continue;
                }
                None => return close,
            }
        }
        if ch == b',' {
            break;
        }
        k += 1;
    }
    k
}

fn is_function_value(s: &str, m: &RawMember) -> bool {
    if m.shorthand {
        return true;
    }
    let val = s[m.value_at..m.end].trim_start();
    let rest = val.strip_prefix("async").filter(|r| r.starts_with(|c: char| c.is_ascii_whitespace())).map(str::trim_start).unwrap_or(val);
    if rest.starts_with("function") && rest[8..].starts_with(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$') {
        return true;
    }
    // Arrow function: `(...) =>` or `ident =>`
    let b = rest.as_bytes();
    if b.first() == Some(&b'(') {
        if let Some(c) = match_close(b, 0) {
            return rest[c + 1..].trim_start().starts_with("=>");
        }
    } else if b.first().is_some_and(|&c| is_ident_start(c)) {
        let mut p = 1;
        while p < b.len() && is_ident_char(b[p]) {
            p += 1;
        }
        return rest[p..].trim_start().starts_with("=>");
    }
    false
}

/// Find the `{` of the Vue Options API object passed to `export default { ... }`,
/// `defineComponent({ ... })`, `defineNuxtComponent({ ... })`, `Vue.extend({ ... })`,
/// or `Vue.component('x', { ... })`.
fn options_object_start(script: &str) -> Option<usize> {
    let b = script.as_bytes();
    let mut i = 0;
    while i < b.len() {
        i = skip_ws_and_comments(b, i, b.len());
        if i >= b.len() {
            break;
        }
        if matches!(b[i], b'"' | b'\'' | b'`') {
            i = skip_quoted(b, i).unwrap_or(b.len());
            continue;
        }
        if b[i..].starts_with(b"export") && (i == 0 || !is_ident_char(b[i - 1])) && b.get(i + 6).is_some_and(|c| c.is_ascii_whitespace()) {
            let mut p = skip_ws_and_comments(b, i + 6, b.len());
            if b[p..].starts_with(b"default") && b.get(p + 7).is_none_or(|c| !is_ident_char(*c)) {
                p = skip_ws_and_comments(b, p + 7, b.len());
                if b.get(p) == Some(&b'{') {
                    return Some(p);
                }
                // `defineComponent({ ... })`, `defineNuxtComponent({ ... })`, `Vue.extend({ ... })`, `Vue.component('x', { ... })`
                for wrapper in [b"defineComponent".as_slice(), b"defineNuxtComponent".as_slice(), b"Vue".as_slice()] {
                    if b[p..].starts_with(wrapper) && b.get(p + wrapper.len()).is_none_or(|c| !is_ident_char(*c)) {
                        let mut q = skip_ws_and_comments(b, p + wrapper.len(), b.len());
                        if wrapper == b"Vue" {
                            if b.get(q) != Some(&b'.') {
                                continue;
                            }
                            q = skip_ws_and_comments(b, q + 1, b.len());
                            if b[q..].starts_with(b"extend") && b.get(q + 6).is_none_or(|c| !is_ident_char(*c)) {
                                q = skip_ws_and_comments(b, q + 6, b.len());
                            } else if b[q..].starts_with(b"component") && b.get(q + 9).is_none_or(|c| !is_ident_char(*c)) {
                                q = skip_ws_and_comments(b, q + 9, b.len());
                                if b.get(q) == Some(&b'(') {
                                    q = skip_ws_and_comments(b, q + 1, b.len());
                                    if b.get(q).is_some_and(|&c| c == b'\'' || c == b'"') {
                                        q = skip_quoted(b, q)?;
                                        q = skip_ws_and_comments(b, q, b.len());
                                        if b.get(q) == Some(&b',') {
                                            q = skip_ws_and_comments(b, q + 1, b.len());
                                            if b.get(q) == Some(&b'{') {
                                                return Some(q);
                                            }
                                        }
                                    }
                                }
                                continue;
                            } else {
                                continue;
                            }
                        }
                        if b.get(q) == Some(&b'(') {
                            q = skip_ws_and_comments(b, q + 1, b.len());
                            if b.get(q) == Some(&b'{') {
                                return Some(q);
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn vue_options_members(script: &str) -> Vec<OptionsMember> {
    let Some(open) = options_object_start(script) else { return Vec::new() };
    let mut out = Vec::new();
    for member in object_members(script, open) {
        if VUE_COMPONENT_FUNCTIONS.contains(&member.name.as_str()) && is_function_value(script, &member) {
            out.push(OptionsMember { name: member.name, start: member.start, end: member.end });
            continue;
        }
        if !VUE_MEMBER_GROUPS.contains(&member.name.as_str()) || member.shorthand {
            continue;
        }
        let val_trimmed_off = skip_ws_and_comments(script.as_bytes(), member.value_at, member.end);
        if val_trimmed_off >= member.end || script.as_bytes()[val_trimmed_off] != b'{' {
            continue;
        }
        for entry in object_members(script, val_trimmed_off) {
            let val = script[entry.value_at..entry.end].trim_start();
            if is_function_value(script, &entry) || (member.name != "methods" && val.starts_with('{')) {
                out.push(OptionsMember { name: entry.name, start: entry.start, end: entry.end });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Vue template component & call extraction
// ---------------------------------------------------------------------------

fn kebab_to_pascal(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for part in name.split('-') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

fn byte_to_line_col(src: &str, starts: &[u32], off: usize) -> (u32, u32) {
    let li = starts.partition_point(|&s| (s as usize) <= off).saturating_sub(1);
    let ls = starts[li] as usize;
    let le = starts.get(li + 1).map_or(src.len(), |&v| v as usize);
    (li as u32 + 1, col16(&src[ls..le], off.saturating_sub(ls)))
}

fn extract_vue_template_components(tpl_src: &str, starts: &[u32], eb: &mut EmitBuilder) {
    let (start, end) = root_vue_template(tpl_src).unwrap_or((0, tpl_src.len()));
    let b = tpl_src.as_bytes();
    let mut i = start;
    while i + 1 < end {
        if b[i] == b'<' && b[i + 1].is_ascii_alphabetic() {
            let name_start = i + 1;
            let mut j = name_start + 1;
            while j < end && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-') {
                j += 1;
            }
            let raw = &tpl_src[name_start..j];
            let comp_name = if raw.as_bytes()[0].is_ascii_uppercase() {
                Some(raw.to_owned())
            } else if raw.contains('-') {
                Some(kebab_to_pascal(raw))
            } else {
                None
            };
            if let Some(name) = comp_name {
                if !VUE_BUILTIN_COMPONENTS.contains(&name.as_str()) {
                    let (line, col) = byte_to_line_col(tpl_src, starts, name_start);
                    eb.push_ref(RefSpec {
                        from: 1,
                        kind: RefKind::Edge(EdgeKind::References),
                        carries_file_path: false,
                        line,
                        col,
                        name: &name,
                        candidates: None,
                    });
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
}

/// Find the content byte range `(start, end)` of the SFC's root `<template>`, returning `None`
/// for non-HTML templates (e.g. `lang="pug"`).
fn root_vue_template(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut start: Option<usize> = None;
    let mut depth = 0usize;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let closing = b.get(i + 1) == Some(&b'/');
        let name_off = if closing { i + 2 } else { i + 1 };
        if !b.get(name_off..name_off + 8).is_some_and(|t| t.eq_ignore_ascii_case(b"template")) {
            i += 1;
            continue;
        }
        let after = name_off + 8;
        if after < b.len() && !b[after].is_ascii_whitespace() && b[after] != b'>' && b[after] != b'/' {
            i += 1;
            continue;
        }
        let Some((gt, self_closing)) = scan_open_tag_end(b, i) else { break };
        if let Some(st) = start {
            if closing {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((st, i));
                }
            } else if !self_closing {
                depth += 1;
            }
        } else {
            if closing || self_closing {
                i = gt + 1;
                continue;
            }
            let attrs = parse_tag_attrs(&s[after..gt]);
            for (k, v) in attrs {
                if k.eq_ignore_ascii_case("lang") {
                    if let Some(val) = v {
                        if !val.trim().eq_ignore_ascii_case("html") {
                            return None;
                        }
                    }
                }
            }
            start = Some(gt + 1);
            depth = 1;
        }
        i = gt + 1;
    }
    start.map(|st| (st, s.len()))
}

struct TplAttr<'a> {
    name: &'a str,
    value: Option<(&'a str, usize)>,
}

struct TplOpenTag<'a> {
    name: &'a str,
    attrs: Vec<TplAttr<'a>>,
    close: usize,
    self_closing: bool,
}

fn parse_tpl_open_tag(s: &str, at: usize, end: usize) -> TplOpenTag<'_> {
    let b = s.as_bytes();
    let mut p = at + 1;
    let ns = p;
    while p < end && !b[p].is_ascii_whitespace() && b[p] != b'/' && b[p] != b'>' {
        p += 1;
    }
    let name = &s[ns..p];
    let mut attrs = Vec::new();
    while p < end {
        while p < end && b[p].is_ascii_whitespace() {
            p += 1;
        }
        if p >= end {
            break;
        }
        if b[p] == b'>' {
            return TplOpenTag { name, attrs, close: p + 1, self_closing: false };
        }
        if b[p] == b'/' && b.get(p + 1) == Some(&b'>') {
            return TplOpenTag { name, attrs, close: p + 2, self_closing: true };
        }
        let a_start = p;
        while p < end && !b[p].is_ascii_whitespace() && !matches!(b[p], b'"' | b'\'' | b'>' | b'/' | b'=') {
            p += 1;
        }
        if p == a_start {
            p += 1;
            continue;
        }
        let attr_name = &s[a_start..p];
        let mut q = p;
        while q < end && b[q].is_ascii_whitespace() {
            q += 1;
        }
        if q >= end || b[q] != b'=' {
            attrs.push(TplAttr { name: attr_name, value: None });
            continue;
        }
        q += 1;
        while q < end && b[q].is_ascii_whitespace() {
            q += 1;
        }
        if q < end && (b[q] == b'"' || b[q] == b'\'') {
            let quote = b[q];
            let vs = q + 1;
            let ve = b[vs..end].iter().position(|&ch| ch == quote).map_or(end, |k| vs + k);
            attrs.push(TplAttr { name: attr_name, value: Some((&s[vs..ve], vs)) });
            p = (ve + 1).min(end);
        } else {
            let vs = q;
            while q < end && !b[q].is_ascii_whitespace() && b[q] != b'>' {
                q += 1;
            }
            attrs.push(TplAttr { name: attr_name, value: Some((&s[vs..q], vs)) });
            p = q;
        }
    }
    TplOpenTag { name, attrs, close: end, self_closing: false }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectiveKind {
    For,
    Slot,
    On,
    Expr,
}

fn directive_kind(name: &str) -> Option<DirectiveKind> {
    if name == "v-for" {
        return Some(DirectiveKind::For);
    }
    if name.starts_with('#') || name == "v-slot" || name.starts_with("v-slot:") || name == "slot-scope" || name == "scope" {
        return Some(DirectiveKind::Slot);
    }
    if name.starts_with('@') || name == "v-on" || name.starts_with("v-on:") {
        return Some(DirectiveKind::On);
    }
    if name.starts_with(':') || name.starts_with('.') || name.starts_with("v-") {
        return Some(DirectiveKind::Expr);
    }
    None
}

fn collect_identifiers(s: &str, out: &mut FxHashSet<String>) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if is_ident_start(b[i]) {
            let st = i;
            i += 1;
            while i < b.len() && is_ident_char(b[i]) {
                i += 1;
            }
            out.insert(s[st..i].to_owned());
        } else {
            i += 1;
        }
    }
}

/// Split Vue `v-for="alias in source"` or `v-for="(item, idx) of items"`.
fn split_v_for(val: &str) -> Option<(&str, usize)> {
    let b = val.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && b[i].is_ascii_whitespace() => {
                let mut j = i;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                for kw in [b"in".as_slice(), b"of".as_slice()] {
                    if b[j..].starts_with(kw) && b.get(j + kw.len()).is_some_and(|c| c.is_ascii_whitespace()) {
                        let mut src_start = j + kw.len();
                        while src_start < b.len() && b[src_start].is_ascii_whitespace() {
                            src_start += 1;
                        }
                        if src_start < b.len() {
                            return Some((&val[..i], src_start));
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

struct TplCall {
    name: String,
    offset: usize,
}

/// If `s` (trimmed) is a plain identifier or dotted member path (`save` or `store.save`),
/// suitable as a bare event handler (`@click="save"`, `on:click={save}`), return `(trimmed, leading_ws_bytes)`.
fn bare_handler_path(s: &str, allow_dollar: bool) -> Option<(&str, usize)> {
    let lead = s.bytes().take_while(|b| b.is_ascii_whitespace()).count();
    let trimmed = s[lead..].trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let mut first_seg = None;
    for seg in trimmed.split('.') {
        let sb = seg.as_bytes();
        if sb.is_empty() || !is_ident_start(sb[0]) || !sb.iter().all(|&c| is_ident_char(c)) {
            return None;
        }
        first_seg.get_or_insert(seg);
    }
    let root = first_seg?;
    if JS_KEYWORDS.contains(&root) || TEMPLATE_GLOBALS.contains(&root) {
        return None;
    }
    if (!allow_dollar && root.starts_with('$')) || (allow_dollar && (root.starts_with("$$") || SVELTE_RUNES.contains(&root))) {
        return None;
    }
    Some((trimmed, lead))
}

fn extract_vue_template_calls(tpl_src: &str, starts: &[u32], own_methods: &FxHashMap<String, u32>, eb: &mut EmitBuilder) {
    let Some((start, end)) = root_vue_template(tpl_src) else { return };
    let mut calls = Vec::new();
    struct Frame<'a> {
        tag: &'a str,
        locals: Option<FxHashSet<String>>,
        pre: bool,
    }
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let in_scope = |stack: &[Frame<'_>], own: Option<&FxHashSet<String>>, name: &str| -> bool {
        own.is_some_and(|s| s.contains(name)) || stack.iter().any(|f| f.locals.as_ref().is_some_and(|s| s.contains(name)))
    };

    let b = tpl_src.as_bytes();
    let mut i = start;
    while i < end {
        let next_mu = find_bytes(&b[i..end], b"{{").map_or(end, |k| i + k);
        let next_lt = b[i..end].iter().position(|&c| c == b'<').map_or(end, |k| i + k);
        if next_mu < next_lt {
            let close = find_bytes(&b[next_mu + 2..end], b"}}").map_or(end, |k| next_mu + 2 + k);
            if !stack.iter().any(|f| f.pre) {
                scan_js_expr_calls(tpl_src, next_mu + 2, close, &|n| in_scope(&stack, None, n), false, &mut calls, None);
            }
            i = (close + 2).min(end);
            continue;
        }
        if next_lt >= end {
            break;
        }
        // Closing tag `</Name>`
        if b.get(next_lt + 1) == Some(&b'/') {
            if let Some(gt) = b[next_lt + 2..end].iter().position(|&c| c == b'>') {
                let cname = tpl_src[next_lt + 2..next_lt + 2 + gt].trim();
                if let Some(pos) = stack.iter().rposition(|f| f.tag.eq_ignore_ascii_case(cname)) {
                    stack.truncate(pos);
                }
                i = next_lt + 2 + gt + 1;
                continue;
            }
            i = next_lt + 2;
            continue;
        }
        if !b.get(next_lt + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            i = next_lt + 1;
            continue;
        }
        let tag = parse_tpl_open_tag(tpl_src, next_lt, end);
        i = tag.close;
        let mut own: Option<FxHashSet<String>> = None;
        let mut pre = false;
        let mut for_source: Option<(usize, usize)> = None;
        for a in &tag.attrs {
            if a.name == "v-pre" {
                pre = true;
            }
            let Some((val, vstart)) = a.value else { continue };
            match directive_kind(a.name) {
                Some(DirectiveKind::For) => {
                    if let Some((alias, rel_src)) = split_v_for(val) {
                        collect_identifiers(alias, own.get_or_insert_with(FxHashSet::default));
                        for_source = Some((vstart + rel_src, vstart + val.len()));
                    }
                }
                Some(DirectiveKind::Slot) => {
                    collect_identifiers(val, own.get_or_insert_with(FxHashSet::default));
                }
                _ => {}
            }
        }
        if !pre && !stack.iter().any(|f| f.pre) {
            for a in &tag.attrs {
                let Some((val, vstart)) = a.value else { continue };
                match directive_kind(a.name) {
                    Some(DirectiveKind::Expr) => {
                        scan_js_expr_calls(tpl_src, vstart, vstart + val.len(), &|n| in_scope(&stack, own.as_ref(), n), false, &mut calls, None);
                    }
                    Some(DirectiveKind::On) => {
                        if let Some((handler, lead)) = bare_handler_path(val, false) {
                            let root = handler.split('.').next().unwrap_or(handler);
                            if !in_scope(&stack, own.as_ref(), root) {
                                calls.push(TplCall { name: handler.to_owned(), offset: vstart + lead });
                            }
                        } else {
                            scan_js_expr_calls(tpl_src, vstart, vstart + val.len(), &|n| in_scope(&stack, own.as_ref(), n), false, &mut calls, None);
                        }
                    }
                    Some(DirectiveKind::For) => {
                        if let Some((fs, fe)) = for_source {
                            scan_js_expr_calls(tpl_src, fs, fe, &|n| in_scope(&stack, own.as_ref(), n), false, &mut calls, None);
                        }
                    }
                    _ => {}
                }
            }
        }
        let tag_lower = tag.name.to_ascii_lowercase();
        if !tag.self_closing && !VOID_ELEMENTS.contains(&tag_lower.as_str()) {
            stack.push(Frame { tag: tag.name, locals: own, pre });
        }
    }

    calls.sort_by_key(|c| c.offset);
    for c in calls {
        let (line, col) = byte_to_line_col(tpl_src, starts, c.offset);
        if !c.name.contains('.') {
            if let Some(&method_row) = own_methods.get(&c.name) {
                eb.push_edge(EdgeSpec {
                    src: 1,
                    dst: method_row,
                    kind: EdgeKind::Calls,
                    prov: Provenance::TreeSitter,
                    line: Some(line),
                    col: Some(col),
                    meta: None,
                });
                continue;
            }
        }
        eb.push_ref(RefSpec { from: 1, kind: RefKind::Edge(EdgeKind::Calls), carries_file_path: false, line, col, name: &c.name, candidates: None });
    }
}

// ---------------------------------------------------------------------------
// Svelte template component, call, and $store subscription extraction
// ---------------------------------------------------------------------------

fn extract_svelte_template_components(tpl_src: &str, starts: &[u32], eb: &mut EmitBuilder) {
    let b = tpl_src.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'<' && b[i + 1].is_ascii_uppercase() {
            let ns = i + 1;
            let mut j = ns + 1;
            while j < b.len() && is_ident_char(b[j]) {
                j += 1;
            }
            let name = &tpl_src[ns..j];
            let (line, col) = byte_to_line_col(tpl_src, starts, ns);
            eb.push_ref(RefSpec { from: 1, kind: RefKind::Edge(EdgeKind::References), carries_file_path: false, line, col, name, candidates: None });
            i = j;
        } else {
            i += 1;
        }
    }
}

fn extract_svelte_template_calls(tpl_src: &str, starts: &[u32], eb: &mut EmitBuilder) {
    let b = tpl_src.as_bytes();
    let mut calls = Vec::new();
    let mut stores = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'{' {
            i += 1;
            continue;
        }
        let close = match_svelte_brace(b, i + 1);
        let expr_start = i + 1;
        let expr_end = close;
        if expr_start < expr_end {
            let first = b[expr_start];
            // Skip `{/if}`, `{:else}` closers; for `{#if cond}` / `{#each list as item}` scan the condition/list too!
            if !matches!(first, b'/' | b':' | b'@' | b'#') {
                // Svelte event handler attribute: `on:click={handleClick}` or Svelte 5 `onclick={handleClick}`
                let is_event_attr = is_svelte_event_attr_before(b, i);
                if is_event_attr {
                    if let Some((handler, lead)) = bare_handler_path(&tpl_src[expr_start..expr_end], true) {
                        calls.push(TplCall { name: handler.to_owned(), offset: expr_start + lead });
                    } else {
                        scan_js_expr_calls(tpl_src, expr_start, expr_end, &|_| false, true, &mut calls, Some(&mut stores));
                    }
                } else {
                    scan_js_expr_calls(tpl_src, expr_start, expr_end, &|_| false, true, &mut calls, Some(&mut stores));
                }
            } else if first == b'#' {
                // `{#if check(x)}` or `{#each getItems() as item}`
                let mut p = expr_start + 1;
                while p < expr_end && b[p].is_ascii_alphabetic() {
                    p += 1;
                }
                let kw = &tpl_src[expr_start + 1..p];
                if matches!(kw, "if" | "await" | "key") {
                    scan_js_expr_calls(tpl_src, p, expr_end, &|_| false, true, &mut calls, Some(&mut stores));
                } else if kw == "each" {
                    let scan_end = find_bytes(&b[p..expr_end], b" as ").map_or(expr_end, |k| p + k);
                    scan_js_expr_calls(tpl_src, p, scan_end, &|_| false, true, &mut calls, Some(&mut stores));
                }
            }
        }
        i = (close + 1).min(b.len());
    }

    calls.sort_by_key(|c| c.offset);
    let mut call_offsets: FxHashSet<usize> = FxHashSet::default();
    for c in calls {
        if SVELTE_RUNES.contains(&c.name.as_str()) {
            continue;
        }
        call_offsets.insert(c.offset);
        let (line, col) = byte_to_line_col(tpl_src, starts, c.offset);
        eb.push_ref(RefSpec { from: 1, kind: RefKind::Edge(EdgeKind::Calls), carries_file_path: false, line, col, name: &c.name, candidates: None });
    }
    stores.sort_by_key(|c| c.offset);
    for st in stores {
        if call_offsets.contains(&st.offset) {
            continue;
        }
        let (line, col) = byte_to_line_col(tpl_src, starts, st.offset);
        eb.push_ref(RefSpec { from: 1, kind: RefKind::Edge(EdgeKind::References), carries_file_path: false, line, col, name: &st.name, candidates: None });
    }
}

/// True when `{` at `brace_pos` immediately follows an `on:*=` or Svelte 5 `on<event>=` attribute.
fn is_svelte_event_attr_before(b: &[u8], brace_pos: usize) -> bool {
    let mut k = brace_pos;
    while k > 0 && b[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    if k == 0 || b[k - 1] != b'=' {
        return false;
    }
    k -= 1;
    while k > 0 && b[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    let attr_end = k;
    while k > 0 && (b[k - 1].is_ascii_alphanumeric() || matches!(b[k - 1], b':' | b'|' | b'-' | b'_')) {
        k -= 1;
    }
    let attr = &b[k..attr_end];
    attr.starts_with(b"on:") || (attr.len() > 2 && attr.starts_with(b"on") && attr[2].is_ascii_lowercase())
}

fn match_svelte_brace(b: &[u8], start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    while i < b.len() {
        let ch = b[i];
        if matches!(ch, b'"' | b'\'' | b'`') {
            i = skip_quoted(b, i).unwrap_or(b.len());
            continue;
        }
        if ch == b'{' {
            depth += 1;
        } else if ch == b'}' {
            if depth == 0 {
                return i;
            }
            depth -= 1;
        }
        i += 1;
    }
    b.len()
}

// ---------------------------------------------------------------------------
// Shared JS template expression call scanner (port of vue-template-calls.ts)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrevTok {
    Start,
    Operand,
    Dot,
    New,
    Decl,
}

fn scan_js_expr_calls(
    s: &str,
    start: usize,
    end: usize,
    outer_local: &dyn Fn(&str) -> bool,
    allow_svelte_stores: bool,
    out: &mut Vec<TplCall>,
    mut stores: Option<&mut Vec<TplCall>>,
) {
    let b = s.as_bytes();
    let mut params: FxHashSet<String> = FxHashSet::default();
    struct FoundCall {
        name: String,
        offset: usize,
        dropped: bool,
    }
    struct ParenInfo {
        open: usize,
        call: Option<usize>,
        is_fn_params: bool,
    }
    let mut found: Vec<FoundCall> = Vec::new();
    let mut parens: Vec<ParenInfo> = Vec::new();
    let mut last_group: Option<(usize, usize)> = None;
    let mut last_ident = String::new();
    let mut fn_pending = false;
    let mut prev = PrevTok::Start;

    let skip_ws = |mut k: usize| -> usize {
        while k < end && b[k].is_ascii_whitespace() {
            k += 1;
        }
        k
    };
    let word_end = |mut k: usize| -> usize {
        while k < end && is_ident_char(b[k]) {
            k += 1;
        }
        k
    };

    let mut i = start;
    while i < end {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            i = skip_simple_str(b, i, end);
            prev = PrevTok::Operand;
            continue;
        }
        if c == b'`' {
            i = skip_tpl_literal(s, i, end, &mut |sub_s, sub_e| {
                scan_js_expr_calls(s, sub_s, sub_e, &|n| params.contains(n) || outer_local(n), allow_svelte_stores, out, None);
            });
            prev = PrevTok::Operand;
            continue;
        }
        if c == b'/' {
            if b.get(i + 1) == Some(&b'/') {
                while i < end && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if b.get(i + 1) == Some(&b'*') {
                i += 2;
                while i + 1 < end && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(end);
                continue;
            }
            if prev != PrevTok::Operand {
                i = skip_regex(b, i, end);
                prev = PrevTok::Operand;
                continue;
            }
            i += 1;
            prev = PrevTok::Start;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            i += 1;
            while i < end && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            prev = PrevTok::Operand;
            continue;
        }
        if is_ident_start(c) {
            let w_end = word_end(i);
            let word = &s[i..w_end];
            if prev == PrevTok::Dot {
                i = w_end;
                prev = PrevTok::Operand;
                last_ident.clear();
                continue;
            }
            if JS_KEYWORDS.contains(&word) {
                i = w_end;
                last_ident.clear();
                if word == "new" {
                    prev = PrevTok::New;
                } else if word == "function" || word == "class" {
                    prev = PrevTok::Decl;
                    if word == "function" {
                        fn_pending = true;
                    }
                } else {
                    prev = if VALUE_KEYWORDS.contains(&word) { PrevTok::Operand } else { PrevTok::Start };
                }
                continue;
            }
            if prev == PrevTok::Decl {
                i = w_end;
                prev = PrevTok::Operand;
                continue;
            }
            // Read dotted / optional-chaining member path: `a.b.c`, `a?.b`, `a!.b`
            let mut segs = vec![word];
            let mut p = w_end;
            loop {
                let mut q = skip_ws(p);
                if q + 1 < end && b[q] == b'!' && b[q + 1] == b'.' {
                    q += 1;
                }
                let r = if q < end && b[q] == b'.' && b.get(q + 1) != Some(&b'.') {
                    skip_ws(q + 1)
                } else if q + 1 < end && b[q] == b'?' && b[q + 1] == b'.' && !b.get(q + 2).is_some_and(|d| d.is_ascii_digit()) {
                    skip_ws(q + 2)
                } else {
                    break;
                };
                if r >= end || !is_ident_start(b[r]) {
                    break;
                }
                let se = word_end(r);
                segs.push(&s[r..se]);
                p = se;
            }
            let mut q = skip_ws(p);
            if q + 1 < end && b[q] == b'?' && b[q + 1] == b'.' {
                let after_opt = skip_ws(q + 2);
                if after_opt < end && b[after_opt] == b'(' {
                    q = after_opt;
                }
            }
            let root = segs[0];
            let is_local = params.contains(root) || outer_local(root);
            // Svelte `$store` subscription reference (`{$count}` or `{$user.name}`)
            if allow_svelte_stores && root.starts_with('$') && !root.starts_with("$$") && root.len() > 1 && !SVELTE_RUNES.contains(&root) && !is_local {
                if let Some(st) = stores.as_deref_mut() {
                    st.push(TplCall { name: root.to_owned(), offset: i });
                }
            }
            if q < end && b[q] == b'(' {
                let skip_dollar = !allow_svelte_stores && root.starts_with('$');
                let skip_rune = allow_svelte_stores && (root.starts_with("$$") || SVELTE_RUNES.contains(&root));
                let call_idx = if prev != PrevTok::New && !skip_dollar && !skip_rune && !TEMPLATE_GLOBALS.contains(&root) && !is_local {
                    found.push(FoundCall { name: segs.join("."), offset: i, dropped: false });
                    Some(found.len() - 1)
                } else {
                    None
                };
                parens.push(ParenInfo { open: q, call: call_idx, is_fn_params: false });
                i = q + 1;
                prev = PrevTok::Start;
                last_ident.clear();
                continue;
            }
            i = p;
            prev = PrevTok::Operand;
            last_ident = if segs.len() == 1 { word.to_owned() } else { String::new() };
            continue;
        }
        if c == b'(' {
            parens.push(ParenInfo { open: i, call: None, is_fn_params: fn_pending });
            fn_pending = false;
            i += 1;
            prev = PrevTok::Start;
            continue;
        }
        if c == b')' {
            if let Some(paren) = parens.pop() {
                last_group = Some((paren.open, i));
                if paren.is_fn_params {
                    collect_identifiers(&s[paren.open + 1..i], &mut params);
                }
                if let Some(ci) = paren.call {
                    let next = skip_ws(i + 1);
                    if next < end && b[next] == b'{' {
                        found[ci].dropped = true;
                    }
                }
            }
            i += 1;
            prev = PrevTok::Operand;
            last_ident.clear();
            continue;
        }
        if c == b'=' && b.get(i + 1) == Some(&b'>') {
            if !last_ident.is_empty() {
                params.insert(std::mem::take(&mut last_ident));
            } else if let Some((go, gc)) = last_group {
                let mut prev_nw = i;
                while prev_nw > start && b[prev_nw - 1].is_ascii_whitespace() {
                    prev_nw -= 1;
                }
                if prev_nw == gc + 1 {
                    collect_identifiers(&s[go + 1..gc], &mut params);
                }
            }
            i += 2;
            prev = PrevTok::Start;
            last_ident.clear();
            continue;
        }
        if c == b'.' {
            if b.get(i + 1) == Some(&b'.') && b.get(i + 2) == Some(&b'.') {
                i += 3;
                prev = PrevTok::Start;
            } else {
                i += 1;
                prev = PrevTok::Dot;
            }
            last_ident.clear();
            continue;
        }
        if c == b'?' && b.get(i + 1) == Some(&b'.') && !b.get(i + 2).is_some_and(|d| d.is_ascii_digit()) {
            i += 2;
            prev = PrevTok::Dot;
            last_ident.clear();
            continue;
        }
        i += 1;
        prev = if c == b']' || c == b'}' { PrevTok::Operand } else { PrevTok::Start };
        last_ident.clear();
    }

    for f in found {
        if !f.dropped {
            out.push(TplCall { name: f.name, offset: f.offset });
        }
    }
}

fn skip_simple_str(b: &[u8], i: usize, end: usize) -> usize {
    let q = b[i];
    let mut k = i + 1;
    while k < end {
        if b[k] == b'\\' {
            k += 2;
        } else if b[k] == q {
            return k + 1;
        } else {
            k += 1;
        }
    }
    end
}

fn skip_tpl_literal(s: &str, i: usize, end: usize, on_sub: &mut dyn FnMut(usize, usize)) -> usize {
    let b = s.as_bytes();
    let mut k = i + 1;
    while k < end {
        if b[k] == b'\\' {
            k += 2;
        } else if b[k] == b'`' {
            return k + 1;
        } else if b[k] == b'$' && b.get(k + 1) == Some(&b'{') {
            let mut depth = 0usize;
            let mut j = k + 2;
            while j < end {
                if b[j] == b'"' || b[j] == b'\'' {
                    j = skip_simple_str(b, j, end);
                } else if b[j] == b'`' {
                    j = skip_tpl_literal(s, j, end, on_sub);
                } else if b[j] == b'{' {
                    depth += 1;
                    j += 1;
                } else if b[j] == b'}' {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    j += 1;
                } else {
                    j += 1;
                }
            }
            on_sub(k + 2, j);
            k = (j + 1).min(end);
        } else {
            k += 1;
        }
    }
    end
}

fn skip_regex(b: &[u8], i: usize, end: usize) -> usize {
    let mut k = i + 1;
    let mut in_class = false;
    while k < end {
        let c = b[k];
        if c == b'\\' {
            k += 2;
            continue;
        }
        if c == b'\n' {
            return i + 1;
        }
        if c == b'[' {
            in_class = true;
        } else if c == b']' {
            in_class = false;
        } else if c == b'/' && !in_class {
            k += 1;
            while k < end && b[k].is_ascii_alphabetic() {
                k += 1;
            }
            return k;
        }
        k += 1;
    }
    i + 1
}
