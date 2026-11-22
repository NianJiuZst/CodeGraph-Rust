//! Builder, stage 1 (design doc 5.5 steps 1-3): merge per-file extraction results into the
//! global node/file/reference/facts tables with final string ids. The result (`Draft`) is a
//! complete, readable `Seg` apart from resolution/synthesis edges and the search indexes,
//! which `finish` adds. Resolution runs against the draft through `View`.

use crate::intern::{Interner, StrArena};
use crate::part::Part;
use crate::records::*;
use crate::seg::{Seg, SegMeta, SegParts};
use crate::strpool::StrPool;
use cg_core::{brand, kinds::NodeKind, EdgeKind, FileId, ListId, NodeIdx, Provenance, RefKind, ScopeId, StrId, NONE};
use cg_extract::FileBatch;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

/// Project-level facts recorded in the segment meta.
#[derive(Default, Clone, Debug)]
pub struct ProjectMeta {
    pub config_digest: u64,
    pub frameworks: Vec<String>,
    pub config_digests: BTreeMap<String, u64>,
}

/// An edge under construction (global node indexes, interned metadata).
#[derive(Clone, Copy, Debug)]
pub struct EdgeBuild {
    pub src: u32,
    pub dst: u32,
    /// `outkind::pack(kind, provenance, class)`.
    pub kind: u8,
    pub line: u32,
    pub col: u32,
    /// Resolution strategy (low byte) and confidence 0-100 (high byte), or 0.
    pub res: u16,
    /// Index of the `RefRec` that produced the edge, or NONE.
    pub ref_idx: u32,
    /// Index into the `emeta` table, or NONE.
    pub meta: u32,
    /// Registration order of a synthesis pass (tie-break between synthetic duplicates).
    pub order: u16,
}

impl EdgeBuild {
    #[inline]
    pub fn edge_kind(&self) -> EdgeKind {
        outkind::kind(self.kind)
    }
    #[inline]
    pub fn class(&self) -> u8 {
        outkind::class(self.kind)
    }
}

/// Output of stage 1.
pub struct Draft {
    pub seg: Seg,
    /// Edges the extraction produced (contains, in-file extends, ...), global ids.
    pub edges: Vec<EdgeBuild>,
    /// Edge metadata strings referenced by `EdgeBuild::meta`, insertion order.
    pub emeta: Vec<String>,
}

impl Draft {
    /// A copy sharing the (immutable) partitions; used to assemble a provisional delta for
    /// resolution and the final one from the same draft.
    pub fn duplicate(&self) -> Draft {
        Draft { seg: Seg::from_parts(self.seg.meta.clone(), self.seg.p.clone()), edges: self.edges.clone(), emeta: self.emeta.clone() }
    }
}

/// Decode an upstream-style node id into (digest, column suffix).
pub fn parse_node_id(id: &str) -> ([u8; 16], u32) {
    let mut digest = [0u8; 16];
    let mut suffix = NONE;
    if let Some((kind, rest)) = id.split_once(':') {
        if kind != "file" {
            let (hex, col) = match rest.split_once(':') {
                Some((h, c)) => (h, c.parse::<u32>().ok()),
                None => (rest, None),
            };
            if hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                for i in 0..16 {
                    digest[i] = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0);
                }
                if let Some(c) = col {
                    suffix = c;
                }
                return (digest, suffix);
            }
        }
    }
    (cg_core::hash::hash128(id.as_bytes()), suffix)
}

/// Rebuild the upstream id string of a node (for oracle comparison and display).
pub fn node_id_string(kind: NodeKind, path: &str, digest: &[u8; 16], col_suffix: u32) -> String {
    if kind == NodeKind::File {
        return format!("file:{path}");
    }
    let mut s = String::with_capacity(kind.as_str().len() + 40);
    s.push_str(kind.as_str());
    s.push(':');
    s.push_str(&cg_core::hash::hex(digest));
    if col_suffix != NONE {
        s.push(':');
        s.push_str(&col_suffix.to_string());
    }
    s
}

/// The key used by incremental sync to find references a definition change may affect:
/// the last name segment, or `module:<spec>` for module specifiers.
pub fn tail_of(name: &str) -> &str {
    let cut = name.rfind(['.', ':', '/', '>', '#']).map(|i| i + 1).unwrap_or(0);
    let t = &name[cut..];
    if t.is_empty() {
        name
    } else {
        t
    }
}

fn opt_text<'a>(t: &mut Interner<'a>, s: Option<&'a str>) -> StrId {
    match s {
        Some(s) => StrId(t.intern(s)),
        None => StrId::NONE,
    }
}

/// Row of the node with kernel id `id`, building the id map on first use.
fn row_by_id<'b>(map: &mut Option<FxHashMap<&'b str, u32>>, rows: &cg_extract::FileRows<'b>, id: &str) -> Option<u32> {
    let m = map.get_or_insert_with(|| rows.nodes().enumerate().map(|(i, n)| (n.id(), i as u32)).collect());
    m.get(id).copied()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

struct ListBuilder {
    off: Vec<u32>,
    items: Vec<u32>,
}

impl ListBuilder {
    fn new() -> Self {
        ListBuilder { off: vec![0], items: Vec::new() }
    }
    fn push_nul_joined<'a>(&mut self, joined: &'a str, text: &mut Interner<'a>) -> ListId {
        let id = ListId::new(self.off.len() - 1);
        for item in joined.split('\0') {
            self.items.push(text.intern(item));
        }
        self.off.push(self.items.len() as u32);
        id
    }
}

/// Id spaces of the base segment that a delta layer continues (design doc 5.2): records of the
/// delta carry global ids (`base length + local id`), while the delta's own pools and index
/// partitions stay local.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildBase {
    pub nodes: u32,
    pub names: u32,
    pub paths: u32,
    pub text: u32,
    pub lists: u32,
}

pub fn build_draft(batches: &[FileBatch], project: &ProjectMeta) -> Draft {
    build_draft_at(batches, None, project)
}

/// `delta`: the global `FileId` of each batch and the base id spaces. With `None` this builds a
/// base segment (dense edge arrays); with `Some` the result has the delta layout.
// ~30 partitions are assigned in file order near the end; one literal would be less readable
#[allow(clippy::field_reassign_with_default)]
pub fn build_draft_at(batches: &[FileBatch], delta: Option<(&[FileId], BuildBase)>, project: &ProjectMeta) -> Draft {
    debug_assert!(batches.windows(2).all(|w| w[0].rel.as_bytes() < w[1].rel.as_bytes()), "batches must be sorted by path");
    let bb = delta.map(|d| d.1).unwrap_or_default();
    let is_delta = delta.is_some();
    let file_id_of = |fi: usize| -> FileId { delta.map_or(FileId::new(fi), |d| d.0[fi]) };
    let arena = StrArena::default();
    let mut names: Interner = Interner::default();
    let mut paths: Interner = Interner::default();
    let mut text: Interner = Interner::default();
    let mut lists = ListBuilder::new();
    let mut emeta: Vec<String> = Vec::new();
    let mut emeta_map: FxHashMap<&str, u32> = FxHashMap::default();

    let total_nodes: usize = batches.iter().map(|b| b.rows().node_count() + b.extra_nodes.len()).sum();
    let total_refs: usize = batches.iter().map(|b| b.rows().ref_count() + b.extra_refs.len()).sum();
    let mut files: Vec<FileRec> = Vec::with_capacity(batches.len());
    let mut hot: Vec<NodeHot> = Vec::with_capacity(total_nodes);
    let mut cold: Vec<NodeCold> = Vec::with_capacity(total_nodes);
    let mut refs: Vec<RefRec> = Vec::with_capacity(total_refs);
    let mut edges: Vec<EdgeBuild> = Vec::with_capacity(total_nodes);
    let mut imports: Vec<ImportRec> = Vec::new();
    let mut exports: Vec<ExportRec> = Vec::new();
    let mut scopes: Vec<ScopeRec> = Vec::new();
    let mut bindings: Vec<BindingRec> = Vec::new();
    let mut file_facts: Vec<FileFacts> = Vec::with_capacity(batches.len());
    let mut sites: Vec<SiteRec> = Vec::new();
    let mut fnames: Vec<(u32, u32)> = Vec::with_capacity(batches.len() * 2);

    for (fi, b) in batches.iter().enumerate() {
        let file = file_id_of(fi);
        let rows = b.rows();
        let nstart = hot.len() as u32 + bb.nodes;
        let kcount = rows.node_count();
        let ncount = kcount + b.extra_nodes.len();
        let rstart = refs.len() as u32;
        let path_t = paths.intern(&b.rel);
        let base = basename(&b.rel);
        fnames.push((names.intern(base), file.0));
        let st = stem(base);
        if st != base {
            fnames.push((names.intern(st), file.0));
        }

        // ---- nodes ----
        for n in rows.nodes() {
            let (digest, col_suffix) = parse_node_id(n.id());
            hot.push(NodeHot {
                file,
                kind: n.kind() as u8,
                vis: n.visibility() as u8,
                flags: n.raw_flags(),
                name: StrId(names.intern(n.name())),
                qname: StrId(names.intern(n.qname())),
                start_line: n.start_line(),
                end_line: n.end_line(),
                parent: NodeIdx(if hot.len() as u32 + bb.nodes == nstart { NONE } else { nstart }),
                _p: 0,
            });
            let sig = opt_text(&mut text, n.signature());
            let doc = opt_text(&mut text, n.docstring());
            let ret = opt_text(&mut text, n.return_type());
            let extra = opt_text(&mut text, n.extra_json());
            let decorators = n.decorators().map(|d| lists.push_nul_joined(d, &mut text)).unwrap_or(ListId::NONE);
            let type_params = n.type_params().map(|d| lists.push_nul_joined(d, &mut text)).unwrap_or(ListId::NONE);
            cold.push(NodeCold {
                start_col: n.start_col(),
                end_col: n.end_col(),
                signature: sig,
                docstring: doc,
                return_type: ret,
                decorators,
                type_params,
                extra,
                digest,
                col_suffix,
                _p: 0,
            });
        }
        for (i, en) in b.extra_nodes.iter().enumerate() {
            let row = (kcount + i) as u32;
            let _ = row;
            let mut flags = 0u16;
            if let Some(e) = en.exported {
                flags |= 1;
                if e {
                    flags |= 2;
                }
            }
            hot.push(NodeHot {
                file,
                kind: en.kind as u8,
                vis: 0,
                flags,
                name: StrId(names.intern(&en.name)),
                qname: StrId(names.intern(&en.qname)),
                start_line: en.start_line,
                end_line: en.end_line,
                parent: NodeIdx(nstart + en.parent_row.min(ncount as u32 - 1)),
                _p: 0,
            });
            cold.push(NodeCold {
                start_col: en.start_col,
                end_col: en.end_col,
                signature: en.signature.as_deref().map(|s| StrId(text.intern(s))).unwrap_or(StrId::NONE),
                docstring: StrId::NONE,
                return_type: StrId::NONE,
                decorators: ListId::NONE,
                type_params: ListId::NONE,
                extra: StrId::NONE,
                digest: en.digest,
                col_suffix: NONE,
                _p: 0,
            });
            edges.push(EdgeBuild {
                src: nstart + en.parent_row.min(ncount as u32 - 1),
                dst: nstart + kcount as u32 + i as u32,
                kind: outkind::pack(EdgeKind::Contains, Provenance::TreeSitter, outkind::CLASS_EXTRACT),
                line: NONE,
                col: NONE,
                res: 0,
                ref_idx: NONE,
                meta: NONE,
                order: 0,
            });
        }

        // ---- kernel edges (also sets `parent` from `contains`) ----
        let mut id_to_row: Option<FxHashMap<&str, u32>> = None;
        fn endpoint<'b>(
            map: &mut Option<FxHashMap<&'b str, u32>>,
            rows: &cg_extract::FileRows<'b>,
            kcount: u32,
            nstart: u32,
            row: Option<u32>,
            id: Option<&str>,
        ) -> Option<u32> {
            if let Some(r) = row {
                return (r < kcount).then_some(nstart + r);
            }
            row_by_id(map, rows, id?).map(|r| nstart + r)
        }
        for e in rows.edges() {
            let (Some(src), Some(dst)) = (
                endpoint(&mut id_to_row, &rows, kcount as u32, nstart, e.source_row(), e.source_id()),
                endpoint(&mut id_to_row, &rows, kcount as u32, nstart, e.target_row(), e.target_id()),
            ) else {
                continue;
            };
            if e.kind() == EdgeKind::Contains {
                hot[(dst - bb.nodes) as usize].parent = NodeIdx(src);
            }
            let meta = match e.metadata_json() {
                Some(m) => {
                    let id = *emeta_map.entry(m).or_insert_with(|| {
                        emeta.push(m.to_owned());
                        (emeta.len() - 1) as u32
                    });
                    id
                }
                None => NONE,
            };
            edges.push(EdgeBuild {
                src,
                dst,
                kind: outkind::pack(e.kind(), e.provenance(), outkind::CLASS_EXTRACT),
                line: e.line().unwrap_or(NONE),
                col: e.col().unwrap_or(NONE),
                res: 0,
                ref_idx: NONE,
                meta,
                order: 0,
            });
        }

        // ---- facts (needed before refs: module-specifier tails) ----
        let import_specs: rustc_hash::FxHashSet<&str> = b.facts.imports.iter().map(|i| i.spec.as_str()).collect();
        let scope_base = scopes.len() as u32;
        let ff_imp = imports.len() as u32;
        let ff_exp = exports.len() as u32;
        let ff_bind = bindings.len() as u32;
        let gs = |local: u32| -> ScopeId { ScopeId(if local == NONE { NONE } else { scope_base + local }) };
        for (si, sc) in b.facts.scopes.iter().enumerate() {
            let _ = si;
            scopes.push(ScopeRec { file, parent: gs(sc.parent), kind: sc.kind as u8, _p: [0; 3], _p2: 0, start: sc.start, end: sc.end });
        }
        for imp in &b.facts.imports {
            imports.push(ImportRec {
                file,
                local: StrId(text.intern(&imp.local)),
                imported: StrId(text.intern(&imp.imported)),
                spec: StrId(text.intern(&imp.spec)),
                kind: imp.kind as u8,
                type_only: imp.type_only as u8,
                _p: 0,
                scope: gs(imp.scope),
                line: imp.line,
                target: FileId::NONE,
            });
        }
        for ex in &b.facts.exports {
            exports.push(ExportRec {
                file,
                name: StrId(text.intern(&ex.name)),
                node: NodeIdx(if ex.node_row == NONE || ex.node_row >= ncount as u32 { NONE } else { nstart + ex.node_row }),
                from_spec: StrId(text.intern(&ex.from_spec)),
                from_name: StrId(text.intern(&ex.from_name)),
                star: ex.star as u8,
                _p: [0; 3],
                target: FileId::NONE,
            });
        }
        for bd in &b.facts.bindings {
            bindings.push(BindingRec {
                scope: gs(bd.scope),
                name: StrId(text.intern(&bd.name)),
                kind: bd.kind as u8,
                _p: [0; 3],
                decl_type: StrId(text.intern(&bd.decl_type)),
                init_ctor: StrId(text.intern(&bd.init_ctor)),
                line: bd.line,
                node: NodeIdx(if bd.node_row == NONE || bd.node_row >= ncount as u32 { NONE } else { nstart + bd.node_row }),
            });
        }
        let ff_site = sites.len() as u32;
        for st in &b.sites {
            sites.push(SiteRec {
                key_kind: st.key_kind,
                role: st.role,
                pass: st.pass,
                key_a: text.intern(&st.key_a),
                key_b: text.intern(&st.key_b),
                node: NodeIdx(if st.node_row == NONE || st.node_row >= ncount as u32 { nstart } else { nstart + st.node_row }),
                line: st.line,
                payload: StrId(text.intern(&st.payload)),
                _p: [0; 8],
            });
        }
        file_facts.push(FileFacts {
            imp_start: ff_imp,
            imp_count: imports.len() as u32 - ff_imp,
            exp_start: ff_exp,
            exp_count: exports.len() as u32 - ff_exp,
            scope_start: scope_base,
            scope_count: scopes.len() as u32 - scope_base,
            bind_start: ff_bind,
            bind_count: bindings.len() as u32 - ff_bind,
            site_start: ff_site,
            site_count: sites.len() as u32 - ff_site,
        });

        // ---- references ----
        enum CallNameEntry<'a> {
            One(&'a str),
            /// Chained calls repeating the same tail at one `(line, col)` (`a.fetch().next().next()`):
            /// kept in AST pre-order (outermost first), matching kernel reference emission order.
            Chain(std::collections::VecDeque<&'a str>),
            Ambiguous,
        }
        let mut call_names: rustc_hash::FxHashMap<(u32, u32, &str), CallNameEntry<'_>> = rustc_hash::FxHashMap::default();
        for call in &b.facts.call_names {
            let keys: [&str; 2] = [call.tail.as_str(), call.kernel_name.as_str()];
            let n_keys = if keys[0] == keys[1] { 1 } else { 2 };
            for &key in &keys[..n_keys] {
                call_names
                    .entry((call.line, call.col, key))
                    .and_modify(|entry| match entry {
                        CallNameEntry::One(prev) if *prev == call.name.as_str() => {}
                        CallNameEntry::One(prev) => {
                            let (short, long) = if prev.len() <= call.name.len() { (*prev, call.name.as_str()) } else { (call.name.as_str(), *prev) };
                            if long.starts_with(short) && long[short.len()..].starts_with("().") && long.rsplit('.').next() == Some(call.tail.as_str()) {
                                let mut q = std::collections::VecDeque::with_capacity(2);
                                q.push_back(*prev);
                                q.push_back(call.name.as_str());
                                *entry = CallNameEntry::Chain(q);
                            } else {
                                *entry = CallNameEntry::Ambiguous;
                            }
                        }
                        CallNameEntry::Chain(q) => {
                            if let Some(&last) = q.back() {
                                if last == call.name.as_str() {
                                    return;
                                }
                                let (short, long) = if last.len() <= call.name.len() { (last, call.name.as_str()) } else { (call.name.as_str(), last) };
                                if long.starts_with(short) && long[short.len()..].starts_with("().") && long.rsplit('.').next() == Some(call.tail.as_str()) {
                                    q.push_back(call.name.as_str());
                                } else {
                                    *entry = CallNameEntry::Ambiguous;
                                }
                            }
                        }
                        CallNameEntry::Ambiguous => {}
                    })
                    .or_insert(CallNameEntry::One(call.name.as_str()));
            }
        }
        // Argument shapes ride on the call reference as its candidate list (the kernel gives
        // calls none). Two calls at one position with one name are told apart by nothing.
        let mut call_args: rustc_hash::FxHashMap<(u32, u32, &str), Option<&cg_extract::facts::CallArgsFact>> = rustc_hash::FxHashMap::default();
        for call in &b.facts.call_args {
            call_args
                .entry((call.line, call.col, call.tail.as_str()))
                .and_modify(|prev| {
                    if prev.is_some_and(|p| p.args != call.args) {
                        *prev = None;
                    }
                })
                .or_insert(Some(call));
        }
        for r in rows.refs() {
            let from_row = r.from_row().filter(|x| *x < kcount as u32).or_else(|| row_by_id(&mut id_to_row, &rows, r.from_id()?));
            let from = nstart + from_row.unwrap_or(0);
            let is_call = r.kind() == RefKind::Edge(EdgeKind::Calls);
            // Rust `mio::Interest::AIO` and TypeScript `core.output<T>` reference the type as the
            // path writes it
            let renamed =
                is_call || (matches!(b.lang.family(), cg_core::LangFamily::Rust | cg_core::LangFamily::Js) && r.kind() == RefKind::Edge(EdgeKind::References));
            let name = if renamed {
                match call_names.get_mut(&(r.line(), r.col(), r.name())) {
                    Some(CallNameEntry::One(s)) => *s,
                    Some(CallNameEntry::Chain(q)) => q.pop_front().unwrap_or_else(|| r.name()),
                    _ => r.name(),
                }
            } else {
                r.name()
            };
            let args = if is_call && r.candidates().is_none() {
                call_args.get(&(r.line(), r.col(), tail_of(name))).copied().flatten().map(|call| {
                    let mut joined = String::from(cg_extract::facts::args::ARGS_MARK);
                    for a in &call.args {
                        joined.push('\0');
                        joined.push_str(a);
                    }
                    arena.alloc(joined)
                })
            } else {
                None
            };
            let tail_src: &str =
                if r.kind().edge_kind() == EdgeKind::Imports && import_specs.contains(name) { arena.alloc(format!("module:{name}")) } else { tail_of(name) };
            refs.push(RefRec {
                from: NodeIdx(from),
                name: StrId(text.intern(name)),
                kind: r.kind().to_wire(),
                flags: r.carries_file_path() as u8,
                status: status::PENDING,
                by: 0,
                line: r.line(),
                col: r.col(),
                candidates: r.candidates().or(args).map(|c| lists.push_nul_joined(c, &mut text)).unwrap_or(ListId::NONE),
                target: NodeIdx::NONE,
                tail: StrId(names.intern(tail_src)),
            });
        }
        for r in &b.extra_refs {
            let from = nstart + r.from_row.min(ncount as u32 - 1);
            let tail_src: &str = if r.kind.edge_kind() == EdgeKind::Imports { arena.alloc(format!("module:{}", r.name)) } else { tail_of(&r.name) };
            refs.push(RefRec {
                from: NodeIdx(from),
                name: StrId(text.intern(&r.name)),
                kind: r.kind.to_wire(),
                flags: 0,
                status: status::PENDING,
                by: 0,
                line: r.line,
                col: r.col,
                candidates: ListId::NONE,
                target: NodeIdx::NONE,
                tail: StrId(names.intern(tail_src)),
            });
        }

        let errors = if b.errors.is_empty() { StrId::NONE } else { StrId(text.intern(arena.alloc(serde_json::to_string(&b.errors).unwrap_or_default()))) };
        files.push(FileRec {
            path: StrId(path_t),
            lang: b.lang as u8,
            flags: b.flags,
            _p: 0,
            size: b.size,
            node_start: NodeIdx(nstart),
            node_count: ncount as u32,
            ref_start: rstart,
            ref_count: refs.len() as u32 - rstart,
            errors,
            mtime_ns: b.mtime_ns,
            hash: b.hash,
            _r: [0; 8],
        });
    }

    // ---- final string ids ----
    let (names_sorted, names_map) = names.finalize();
    let (paths_sorted, paths_map) = paths.finalize();
    let (text_sorted, text_map) = text.finalize();
    let rn = |s: &mut StrId| {
        if s.0 != NONE {
            s.0 = names_map[s.0 as usize] + bb.names
        }
    };
    let rt = |s: &mut StrId| {
        if s.0 != NONE {
            s.0 = text_map[s.0 as usize] + bb.text
        }
    };
    files.par_iter_mut().for_each(|f| {
        f.path.0 = paths_map[f.path.0 as usize] + bb.paths;
        let mut e = f.errors;
        rt(&mut e);
        f.errors = e;
    });
    hot.par_iter_mut().for_each(|h| {
        rn(&mut h.name);
        rn(&mut h.qname);
    });
    cold.par_iter_mut().for_each(|c| {
        rt(&mut c.signature);
        rt(&mut c.docstring);
        rt(&mut c.return_type);
        rt(&mut c.extra);
        if c.decorators.0 != NONE {
            c.decorators.0 += bb.lists;
        }
        if c.type_params.0 != NONE {
            c.type_params.0 += bb.lists;
        }
    });
    refs.par_iter_mut().for_each(|r| {
        rt(&mut r.name);
        rn(&mut r.tail);
        if r.candidates.0 != NONE {
            r.candidates.0 += bb.lists;
        }
    });
    imports.par_iter_mut().for_each(|i| {
        rt(&mut i.local);
        rt(&mut i.imported);
        rt(&mut i.spec);
    });
    exports.par_iter_mut().for_each(|e| {
        rt(&mut e.name);
        rt(&mut e.from_spec);
        rt(&mut e.from_name);
    });
    bindings.par_iter_mut().for_each(|b| {
        rt(&mut b.name);
        rt(&mut b.decl_type);
        rt(&mut b.init_ctor);
    });
    lists.items.par_iter_mut().for_each(|i| *i = text_map[*i as usize] + bb.text);
    sites.par_iter_mut().for_each(|st| {
        st.key_a = text_map[st.key_a as usize] + bb.text;
        st.key_b = text_map[st.key_b as usize] + bb.text;
        rt(&mut st.payload);
    });
    let mut fname_idx: Vec<FileNameRec> = fnames.iter().map(|(n, f)| FileNameRec { name: StrId(names_map[*n as usize]), file: FileId(*f) }).collect();
    fname_idx.par_sort_unstable_by_key(|r| (r.name.0, r.file.0));
    fname_idx.dedup();

    let names_pool = StrPool::from_sorted_unique(&names_sorted);
    let paths_pool = StrPool::from_sorted_unique(&paths_sorted);
    let text_pool = StrPool::from_sorted_unique(&text_sorted);

    // ---- edge metadata strings ----
    let mut em_off = Vec::with_capacity(emeta.len() + 1);
    let mut em_data = Vec::new();
    em_off.push(0u32);
    for m in &emeta {
        em_data.extend_from_slice(m.as_bytes());
        em_off.push(em_data.len() as u32);
    }

    // ---- name postings (definitions only) ----
    let skip = |k: u8| matches!(NodeKind::from_u8(k), Some(NodeKind::File | NodeKind::Import | NodeKind::Export | NodeKind::Parameter) | None);
    let (name_post_off, name_post) = postings(names_pool.len(), &hot, bb, |h| StrId(h.name.0 - bb.names), &skip);
    let (qname_post_off, qname_post) = postings(names_pool.len(), &hot, bb, |h| StrId(h.qname.0 - bb.names), &skip);

    let meta = SegMeta {
        total_tokens: 0,
        kind: if is_delta { "delta" } else { "base" }.into(),
        format_version: brand::FORMAT_VERSION,
        engine_version: brand::ENGINE_VERSION,
        kernel_abi: cg_extract::KERNEL_ABI_VERSION as u32,
        upstream_commit: brand::UPSTREAM_KERNEL_COMMIT.trim().to_owned(),
        langs: {
            let mut l: Vec<String> = batches.iter().map(|b| b.lang.as_str().to_owned()).collect();
            l.sort();
            l.dedup();
            l
        },
        config_digest: project.config_digest,
        frameworks: project.frameworks.clone(),
        config_digests: project.config_digests.clone(),
        n_files: files.len() as u32,
        n_nodes: hot.len() as u32,
        n_edges: 0,
        n_refs: refs.len() as u32,
        base_gen: 0,
    };
    let mut p = SegParts::default();
    p.files = Part::owned(files);
    p.nodes_hot = Part::owned(hot);
    p.nodes_cold = Part::owned(cold);
    p.lists_off = Part::owned(lists.off);
    p.lists = Part::owned(lists.items);
    p.names_off = names_pool.off.clone();
    p.names_data = names_pool.data.clone();
    p.paths_off = paths_pool.off.clone();
    p.paths_data = paths_pool.data.clone();
    p.text_off = text_pool.off.clone();
    p.text_data = text_pool.data.clone();
    p.emeta_off = Part::owned(em_off);
    p.emeta_data = Part::owned(em_data);
    p.name_post_off = Part::owned(name_post_off);
    p.name_post = Part::owned(name_post);
    p.qname_post_off = Part::owned(qname_post_off);
    p.qname_post = Part::owned(qname_post);
    p.refs = Part::owned(refs);
    p.imports = Part::owned(imports);
    p.exports = Part::owned(exports);
    p.scopes = Part::owned(scopes);
    p.bindings = Part::owned(bindings);
    p.file_facts = Part::owned(file_facts);
    p.sites = Part::owned(sites);
    p.fname_idx = Part::owned(fname_idx);
    // Draft edges: the extraction edges only, so the View is usable before resolution.
    let mut seg = Seg::from_parts(meta, p);
    if is_delta {
        let mut copy = edges.clone();
        let asm = crate::edges::assemble_sparse(&mut copy);
        crate::edges::install_sparse(&mut seg, asm);
    } else {
        let asm = crate::edges::assemble(&mut edges, seg.p.nodes_hot.len());
        crate::edges::install(&mut seg, asm);
    }
    Draft { seg, edges, emeta }
}

/// CSR of node indexes per string id (ascending node order).
pub(crate) fn postings(n_strings: usize, hot: &[NodeHot], bb: BuildBase, key: impl Fn(&NodeHot) -> StrId, skip: &impl Fn(u8) -> bool) -> (Vec<u32>, Vec<u32>) {
    let mut off = vec![0u32; n_strings + 1];
    for h in hot {
        if !skip(h.kind) {
            off[key(h).0 as usize + 1] += 1;
        }
    }
    for i in 0..n_strings {
        off[i + 1] += off[i];
    }
    let mut fill = off.clone();
    let mut post = vec![0u32; *off.last().unwrap() as usize];
    for (i, h) in hot.iter().enumerate() {
        if !skip(h.kind) {
            let k = key(h).0 as usize;
            post[fill[k] as usize] = i as u32 + bb.nodes;
            fill[k] += 1;
        }
    }
    (off, post)
}
