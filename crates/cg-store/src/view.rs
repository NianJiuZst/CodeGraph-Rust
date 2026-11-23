//! `View { base, delta }`: the only way query and resolution code reads a snapshot (design doc
//! 5.6, 16.3). A concrete type, not a trait object. With no delta every accessor is a direct
//! slice access into the mmap'd base segment.
//!
//! Index spaces (identical for base-only views):
//!   * `FileId`  - base files `0..F_b`, then files added by the delta.
//!   * `NodeIdx` - base nodes `0..N_b`, then the delta's nodes.
//!   * `StrId`   - per pool; ids `>= base pool length` address the delta's pool.
//!   * edge ids  - base edges `0..E_b`, then the delta's.
//!   * ref ids   - base refs `0..R_b`, then the delta's.

use crate::records::*;
use crate::seg::Seg;
use crate::strpool::StrPool;
use cg_core::{CgError, EdgeKind, FileId, Lang, NodeIdx, NodeKind, Provenance, RefKind, Result, StrId, Visibility, NONE};
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// State of a file in a delta layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum DeltaState {
    /// Content changed: nodes, refs and facts are in the delta.
    Replaced = 0,
    /// Content unchanged, outgoing edges and refs changed: nodes stay in the base.
    EdgesOnly = 1,
    New = 2,
    Removed = 3,
}

impl DeltaState {
    fn from_u32(v: u32) -> DeltaState {
        match v {
            0 => DeltaState::Replaced,
            1 => DeltaState::EdgesOnly,
            2 => DeltaState::New,
            _ => DeltaState::Removed,
        }
    }
}

/// Derived lookups over a delta segment, built once when the view is created.
pub struct DeltaIdx {
    pub seg: Arc<Seg>,
    /// Global file id -> position in the delta's `files` / `delta_files` arrays.
    pub file_pos: FxHashMap<u32, usize>,
    pub n_base_files: u32,
    pub n_base_nodes: u32,
    pub n_base_edges: u32,
    pub n_base_refs: u32,
    pub base_names: u32,
    pub base_text: u32,
    pub base_paths: u32,
}

#[derive(Clone)]
pub struct View {
    pub base: Arc<Seg>,
    pub delta: Option<Arc<DeltaIdx>>,
    pub root: Option<Arc<PathBuf>>,
}

#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    pub view: &'a View,
    pub idx: NodeIdx,
    pub hot: &'a NodeHot,
    pub cold: &'a NodeCold,
}

#[derive(Clone, Copy, Debug)]
pub struct EdgeRef {
    pub eid: u32,
    pub src: NodeIdx,
    pub dst: NodeIdx,
    pub kind: EdgeKind,
    pub provenance: Provenance,
    /// `outkind::CLASS_*`
    pub class: u8,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Resolution strategy (low byte) and confidence (high byte), 0 for non-resolution edges.
    pub res: u16,
    pub ref_idx: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnresolvedScope {
    /// Call-like edges (`callers` / `callees`): Calls, Instantiates, References, FunctionRef, Navigates.
    CallLike,
    /// Any non-import dependency reference (`impact` / focused `explore`).
    Dependency,
}

#[inline]
fn matches_unresolved_scope(kind: u8, scope: UnresolvedScope) -> bool {
    match (RefKind::from_wire(kind), scope) {
        (Some(RefKind::Edge(EdgeKind::Imports)), _) => false,
        (Some(RefKind::FunctionRef), _) => true,
        (Some(RefKind::Edge(EdgeKind::Calls | EdgeKind::Instantiates | EdgeKind::References | EdgeKind::Navigates)), _) => true,
        (Some(_), UnresolvedScope::Dependency) => true,
        _ => false,
    }
}

#[inline]
fn unresolved_status_rank(s: u8) -> u8 {
    match s {
        status::AMBIGUOUS => 0,
        status::LOCAL_BINDING => 1,
        _ => 2,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedRefSite {
    pub file: FileId,
    pub line: u32,
    pub status: u8,
    pub expr: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnresolvedRefSummary {
    pub ambiguous: usize,
    pub dynamic: usize,
    pub no_candidate: usize,
    pub sites: Vec<UnresolvedRefSite>,
}

impl UnresolvedRefSummary {
    pub fn total(&self) -> usize {
        self.ambiguous + self.dynamic + self.no_candidate
    }
}

#[inline]
fn is_member_expr(expr: &str) -> bool {
    expr.contains('.') || expr.contains("::") || expr.contains("->")
}

#[derive(Clone, Copy)]
pub struct FileRef<'a> {
    pub view: &'a View,
    pub id: FileId,
    pub rec: &'a FileRec,
}

impl<'a> FileRef<'a> {
    pub fn path(&self) -> &'a str {
        self.view.path_str(self.rec.path)
    }
    pub fn lang(&self) -> Lang {
        Lang::from_u8(self.rec.lang)
    }
    pub fn flags(&self) -> u8 {
        self.rec.flags
    }
    pub fn nodes(&self) -> std::ops::Range<u32> {
        self.rec.node_start.0..self.rec.node_start.0 + self.rec.node_count
    }
    pub fn errors(&self) -> Option<&'a str> {
        self.view.text_opt(self.rec.errors)
    }
}

impl<'a> NodeRef<'a> {
    pub fn name(&self) -> &'a str {
        self.view.name_str(self.hot.name)
    }
    pub fn qname(&self) -> &'a str {
        self.view.name_str(self.hot.qname)
    }
    pub fn kind(&self) -> NodeKind {
        NodeKind::from_u8(self.hot.kind).unwrap_or(NodeKind::Variable)
    }
    pub fn file(&self) -> FileId {
        self.hot.file
    }
    pub fn start_line(&self) -> u32 {
        self.hot.start_line
    }
    pub fn end_line(&self) -> u32 {
        self.hot.end_line
    }
    pub fn start_col(&self) -> u32 {
        self.cold.start_col
    }
    pub fn end_col(&self) -> u32 {
        self.cold.end_col
    }
    pub fn visibility(&self) -> Visibility {
        Visibility::from_u8(self.hot.vis).unwrap_or(Visibility::None)
    }
    fn flag(&self, pair: u16) -> Option<bool> {
        let f = self.hot.flags;
        if f & (1 << (pair * 2)) == 0 {
            None
        } else {
            Some(f & (1 << (pair * 2 + 1)) != 0)
        }
    }
    pub fn is_exported(&self) -> Option<bool> {
        self.flag(0)
    }
    pub fn is_async(&self) -> Option<bool> {
        self.flag(1)
    }
    pub fn is_static(&self) -> Option<bool> {
        self.flag(2)
    }
    pub fn is_abstract(&self) -> Option<bool> {
        self.flag(3)
    }
    pub fn signature(&self) -> Option<&'a str> {
        self.view.text_opt(self.cold.signature)
    }
    pub fn docstring(&self) -> Option<&'a str> {
        self.view.text_opt(self.cold.docstring)
    }
    pub fn return_type(&self) -> Option<&'a str> {
        self.view.text_opt(self.cold.return_type)
    }
    pub fn extra_json(&self) -> Option<&'a str> {
        self.view.text_opt(self.cold.extra)
    }
    pub fn decorators(&self) -> Vec<&'a str> {
        self.view.list(self.cold.decorators)
    }
    pub fn type_params(&self) -> Vec<&'a str> {
        self.view.list(self.cold.type_params)
    }
    /// Containing node (`contains` source); `None` for a file node.
    pub fn parent(&self) -> Option<NodeIdx> {
        self.hot.parent.opt()
    }
}

/// Postings of one name: base entries (alive only) followed by delta entries.
#[derive(Clone)]
pub struct Posting<'a> {
    view: &'a View,
    base: std::slice::Iter<'a, u32>,
    delta: std::slice::Iter<'a, u32>,
}

impl<'a> Iterator for Posting<'a> {
    type Item = NodeIdx;
    fn next(&mut self) -> Option<NodeIdx> {
        for &n in self.base.by_ref() {
            if self.view.is_alive(NodeIdx(n)) {
                return Some(NodeIdx(n));
            }
        }
        self.delta.next().map(|&n| NodeIdx(n))
    }
}

impl View {
    pub fn new(base: Arc<Seg>) -> View {
        View { base, delta: None, root: None }
    }

    pub fn with_root(mut self, root: PathBuf) -> View {
        self.root = Some(Arc::new(root));
        self
    }

    /// Attach a delta layer. Fails if it does not belong to this base.
    pub fn with_delta(&self, delta: Arc<Seg>) -> Result<View> {
        if delta.meta.kind != "delta" {
            return Err(CgError::corrupt("segment is not a delta layer"));
        }
        let b = &self.base;
        let mut file_pos = FxHashMap::default();
        for (i, tri) in delta.p.delta_files.as_chunks::<3>().0.iter().enumerate() {
            file_pos.insert(tri[0], i);
        }
        let idx = DeltaIdx {
            file_pos,
            n_base_files: b.p.files.len() as u32,
            n_base_nodes: b.p.nodes_hot.len() as u32,
            n_base_edges: b.p.out_dst.len() as u32,
            n_base_refs: b.p.refs.len() as u32,
            base_names: b.names.len() as u32,
            base_text: b.text.len() as u32,
            base_paths: b.paths.len() as u32,
            seg: delta,
        };
        Ok(View { base: self.base.clone(), delta: Some(Arc::new(idx)), root: self.root.clone() })
    }

    #[inline]
    fn d(&self) -> Option<&DeltaIdx> {
        self.delta.as_deref()
    }

    // ---------------------------------------------------------------- strings
    pub fn name_str(&self, id: StrId) -> &str {
        cg_core::cancel::check();
        match self.d() {
            Some(d) if id.0 != NONE && id.0 >= d.base_names => d.seg.names.get(StrId(id.0 - d.base_names)),
            _ => self.base.names.get(id),
        }
    }
    pub fn text_str(&self, id: StrId) -> &str {
        match self.d() {
            Some(d) if id.0 != NONE && id.0 >= d.base_text => d.seg.text.get(StrId(id.0 - d.base_text)),
            _ => self.base.text.get(id),
        }
    }
    pub fn text_opt(&self, id: StrId) -> Option<&str> {
        (id.0 != NONE).then(|| self.text_str(id))
    }
    pub fn path_str(&self, id: StrId) -> &str {
        match self.d() {
            Some(d) if id.0 != NONE && id.0 >= d.base_paths => d.seg.paths.get(StrId(id.0 - d.base_paths)),
            _ => self.base.paths.get(id),
        }
    }

    /// Items of a shared list (decorators, type parameters, candidate names).
    pub fn list(&self, id: cg_core::ListId) -> Vec<&str> {
        if id.0 == NONE {
            return Vec::new();
        }
        // List ids of delta nodes are offset by the base list count.
        let (seg, i) = match self.d() {
            Some(d) if id.0 as usize >= self.base.p.lists_off.len().saturating_sub(1) => {
                (&d.seg, id.0 as usize - (self.base.p.lists_off.len().saturating_sub(1)))
            }
            _ => (&self.base, id.0 as usize),
        };
        let off = &seg.p.lists_off;
        if i + 1 >= off.len() {
            return Vec::new();
        }
        let is_delta = !Arc::ptr_eq(seg, &self.base);
        seg.p.lists[off[i] as usize..off[i + 1] as usize]
            .iter()
            .map(|s| if is_delta { self.text_str(StrId(*s)) } else { self.base.text.get(StrId(*s)) })
            .collect()
    }

    // ------------------------------------------------------------------ files
    pub fn n_files(&self) -> usize {
        self.base.p.files.len() + self.d().map_or(0, |d| d.seg.p.delta_files.as_chunks::<3>().0.iter().filter(|t| t[1] == DeltaState::New as u32).count())
    }

    fn delta_file(&self, f: FileId) -> Option<(usize, DeltaState)> {
        let d = self.d()?;
        let pos = *d.file_pos.get(&f.0)?;
        Some((pos, DeltaState::from_u32(d.seg.p.delta_files[pos * 3 + 1])))
    }

    pub fn file_state(&self, f: FileId) -> Option<DeltaState> {
        self.delta_file(f).map(|x| x.1)
    }

    /// Is the file's outgoing-edge / ref / (for Replaced/New) node data in the delta?
    pub fn file_in_delta(&self, f: FileId) -> bool {
        matches!(self.delta_file(f), Some((_, s)) if s != DeltaState::Removed)
    }

    pub fn file_removed(&self, f: FileId) -> bool {
        matches!(self.delta_file(f), Some((_, DeltaState::Removed)))
    }

    pub fn file(&self, f: FileId) -> FileRef<'_> {
        cg_core::cancel::check();
        let rec = match self.delta_file(f) {
            Some((pos, _)) => &self.d().unwrap().seg.p.files[pos],
            None => &self.base.p.files[f.idx()],
        };
        FileRef { view: self, id: f, rec }
    }

    /// All live file ids in ascending order (base order, then added files).
    pub fn files(&self) -> impl Iterator<Item = FileId> + '_ {
        let nb = self.base.p.files.len() as u32;
        let added: Vec<u32> = match self.d() {
            Some(d) => {
                let mut v: Vec<u32> = d.seg.p.delta_files.as_chunks::<3>().0.iter().filter(|t| t[1] == DeltaState::New as u32).map(|t| t[0]).collect();
                v.sort_unstable();
                v
            }
            None => Vec::new(),
        };
        (0..nb).chain(added).map(FileId).filter(move |f| !self.file_removed(*f))
    }

    pub fn file_by_path(&self, p: &str) -> Option<FileId> {
        if let Some(d) = self.d() {
            // The delta's own paths: replaced / added files.
            if let Some(local) = d.seg.paths.find(p.as_bytes()) {
                let global = StrId(local.0 + d.base_paths);
                for (i, tri) in d.seg.p.delta_files.as_chunks::<3>().0.iter().enumerate() {
                    if d.seg.p.files[i].path == global && tri[1] != DeltaState::Removed as u32 {
                        return Some(FileId(tri[0]));
                    }
                }
            }
        }
        let sid = self.base.paths.find(p.as_bytes())?;
        // Base files are sorted by path, so the path id is the file id.
        let f = FileId(sid.0);
        if self.file_removed(f) {
            return None;
        }
        // A base path whose id equals a file id only if the pool is exactly the file list.
        (self.base.p.files.get(f.idx()).map(|r| r.path) == Some(sid))
            .then_some(f)
            .or_else(|| self.base.p.files.iter().position(|r| r.path == sid).map(|i| FileId(i as u32)))
    }

    // ------------------------------------------------------------------ nodes
    pub fn n_nodes(&self) -> usize {
        self.base.p.nodes_hot.len() + self.d().map_or(0, |d| d.seg.p.nodes_hot.len())
    }

    #[inline]
    pub fn hot(&self, n: NodeIdx) -> &NodeHot {
        cg_core::cancel::check();
        let i = n.idx();
        match self.d() {
            Some(d) if i >= d.n_base_nodes as usize => &d.seg.p.nodes_hot[i - d.n_base_nodes as usize],
            _ => &self.base.p.nodes_hot[i],
        }
    }

    #[inline]
    pub fn cold(&self, n: NodeIdx) -> &NodeCold {
        let i = n.idx();
        match self.d() {
            Some(d) if i >= d.n_base_nodes as usize => &d.seg.p.nodes_cold[i - d.n_base_nodes as usize],
            _ => &self.base.p.nodes_cold[i],
        }
    }

    pub fn node(&self, n: NodeIdx) -> NodeRef<'_> {
        NodeRef { view: self, idx: n, hot: self.hot(n), cold: self.cold(n) }
    }

    /// False for base nodes replaced by the delta.
    #[inline]
    pub fn is_alive(&self, n: NodeIdx) -> bool {
        match self.d() {
            Some(d) if n.0 < d.n_base_nodes => {
                let bits = &d.seg.p.dead_nodes;
                let (w, b) = (n.idx() / 64, n.idx() % 64);
                !(w < bits.len() && bits[w] >> b & 1 == 1)
            }
            _ => true,
        }
    }

    pub fn nodes_in_file(&self, f: FileId) -> std::ops::Range<u32> {
        self.file(f).nodes()
    }

    // ----------------------------------------------------------------- lookup
    fn postings_for<'a>(
        &'a self,
        s: &[u8],
        base_off: &'a [u32],
        base_post: &'a [u32],
        delta_off: impl Fn(&'a DeltaIdx) -> (&'a [u32], &'a [u32]),
    ) -> Posting<'a> {
        let empty: &[u32] = &[];
        let base = match self.base.names.find(s) {
            Some(id) if (id.idx() + 1) < base_off.len() => &base_post[base_off[id.idx()] as usize..base_off[id.idx() + 1] as usize],
            _ => empty,
        };
        let delta = match self.d() {
            Some(d) => match d.seg.names.find(s) {
                Some(id) => {
                    let (off, post) = delta_off(d);
                    if id.idx() + 1 < off.len() {
                        &post[off[id.idx()] as usize..off[id.idx() + 1] as usize]
                    } else {
                        empty
                    }
                }
                None => empty,
            },
            None => empty,
        };
        Posting { view: self, base: base.iter(), delta: delta.iter() }
    }

    /// Definitions whose simple name is exactly `s` (not imports / exports / parameters / files).
    pub fn by_name<'a>(&'a self, s: &str) -> Posting<'a> {
        self.postings_for(s.as_bytes(), &self.base.p.name_post_off, &self.base.p.name_post, |d| (&d.seg.p.name_post_off, &d.seg.p.name_post))
    }

    /// Definitions whose qualified name is exactly `s`.
    pub fn by_qname<'a>(&'a self, s: &str) -> Posting<'a> {
        self.postings_for(s.as_bytes(), &self.base.p.qname_post_off, &self.base.p.qname_post, |d| (&d.seg.p.qname_post_off, &d.seg.p.qname_post))
    }

    /// Definitions named like the names-pool string `id` (base or delta id).
    pub fn by_name_id<'a>(&'a self, id: StrId) -> Posting<'a> {
        self.by_name(self.name_str(id))
    }

    // ------------------------------------------------------------------ edges
    fn file_edges_in_delta(&self, f: FileId) -> bool {
        matches!(self.delta_file(f), Some((_, s)) if s != DeltaState::Removed)
    }

    /// Outgoing edges of `n`.
    pub fn out_edges(&self, n: NodeIdx) -> OutEdges<'_> {
        cg_core::cancel::check();
        let hot = self.hot(n);
        if let Some(d) = self.d() {
            let in_delta_nodes = n.0 >= d.n_base_nodes;
            if in_delta_nodes || self.file_edges_in_delta(hot.file) {
                // sparse CSR over the delta's source list
                let srcs = &d.seg.p.d_src_nodes;
                return match srcs.binary_search(&n.0) {
                    Ok(i) => {
                        let (a, b) = (d.seg.p.out_off[i], d.seg.p.out_off[i + 1]);
                        OutEdges { view: self, seg: &d.seg, src: n, range: a..b, eid_base: d.n_base_edges }
                    }
                    Err(_) => OutEdges { view: self, seg: &d.seg, src: n, range: 0..0, eid_base: d.n_base_edges },
                };
            }
            if !self.is_alive(n) {
                return OutEdges { view: self, seg: &self.base, src: n, range: 0..0, eid_base: 0 };
            }
        }
        let (a, b) = (self.base.p.out_off[n.idx()], self.base.p.out_off[n.idx() + 1]);
        OutEdges { view: self, seg: &self.base, src: n, range: a..b, eid_base: 0 }
    }

    /// Incoming edges of `n`.
    pub fn in_edges(&self, n: NodeIdx) -> impl Iterator<Item = EdgeRef> + '_ {
        let base_part: Box<dyn Iterator<Item = EdgeRef> + '_> = match self.d() {
            Some(d) if n.0 >= d.n_base_nodes => Box::new(std::iter::empty()),
            _ if !self.is_alive(n) => Box::new(std::iter::empty()),
            _ => {
                let (a, b) = (self.base.p.in_off[n.idx()], self.base.p.in_off[n.idx() + 1]);
                Box::new((a..b).filter_map(move |k| {
                    let src = NodeIdx(self.base.p.in_src[k as usize]);
                    let eid = self.base.p.in_eid[k as usize];
                    if !self.is_alive(src) || self.file_edges_in_delta(self.hot(src).file) {
                        return None;
                    }
                    Some(edge_ref(&self.base, eid, src, 0))
                }))
            }
        };
        let delta_part: Box<dyn Iterator<Item = EdgeRef> + '_> = match self.d() {
            Some(d) => match d.seg.p.d_dst_nodes.binary_search(&n.0) {
                Ok(i) => {
                    let (a, b) = (d.seg.p.in_off[i], d.seg.p.in_off[i + 1]);
                    Box::new((a..b).map(move |k| {
                        let src = NodeIdx(d.seg.p.in_src[k as usize]);
                        edge_ref(&d.seg, d.seg.p.in_eid[k as usize], src, d.n_base_edges)
                    }))
                }
                Err(_) => Box::new(std::iter::empty()),
            },
            None => Box::new(std::iter::empty()),
        };
        base_part.chain(delta_part)
    }

    /// JSON metadata of an edge (synthesised edges: `synthesizedBy`, `registeredAt`, ...).
    pub fn edge_meta(&self, eid: u32) -> Option<&str> {
        let (seg, local, delta) = match self.d() {
            Some(d) if eid >= d.n_base_edges => (&d.seg, eid - d.n_base_edges, true),
            _ => (&self.base, eid, false),
        };
        let m = &seg.p.edge_meta;
        let i = m.binary_search_by_key(&local, |r| r.eid).ok()?;
        let s = &seg.emeta;
        let _ = delta;
        Some(std::str::from_utf8(s.bytes(StrId(m[i].text.0))).unwrap_or(""))
    }

    // ------------------------------------------------------------ refs & facts
    pub fn n_refs(&self) -> usize {
        self.base.p.refs.len() + self.d().map_or(0, |d| d.seg.p.refs.len())
    }

    pub fn reference(&self, i: u32) -> &RefRec {
        match self.d() {
            Some(d) if i >= d.n_base_refs => &d.seg.p.refs[(i - d.n_base_refs) as usize],
            _ => &self.base.p.refs[i as usize],
        }
    }

    /// Reference id range of a file (in the global ref space).
    pub fn ref_range(&self, f: FileId) -> std::ops::Range<u32> {
        match self.delta_file(f) {
            Some((pos, s)) if s != DeltaState::Removed => {
                let d = self.d().unwrap();
                let r = &d.seg.p.files[pos];
                d.n_base_refs + r.ref_start..d.n_base_refs + r.ref_start + r.ref_count
            }
            Some(_) => 0..0,
            None => {
                let r = &self.base.p.files[f.idx()];
                r.ref_start..r.ref_start + r.ref_count
            }
        }
    }

    pub fn refs_of_file(&self, f: FileId) -> &[RefRec] {
        match self.delta_file(f) {
            Some((pos, s)) if s != DeltaState::Removed => {
                let d = self.d().unwrap();
                let r = &d.seg.p.files[pos];
                &d.seg.p.refs[r.ref_start as usize..(r.ref_start + r.ref_count) as usize]
            }
            Some(_) => &[],
            None => {
                let r = &self.base.p.files[f.idx()];
                &self.base.p.refs[r.ref_start as usize..(r.ref_start + r.ref_count) as usize]
            }
        }
    }

    fn facts_of(&self, f: FileId) -> (&Seg, FileFacts) {
        // Facts follow content: Replaced / New files carry theirs in the delta.
        match self.delta_file(f) {
            Some((pos, DeltaState::Replaced | DeltaState::New)) => {
                let d = self.d().unwrap();
                (&d.seg, d.seg.p.file_facts[pos])
            }
            Some((_, DeltaState::Removed)) => (&self.base, bytemuck::Zeroable::zeroed()),
            _ => (&self.base, self.base.p.file_facts[f.idx()]),
        }
    }

    pub fn imports_of(&self, f: FileId) -> &[ImportRec] {
        let (seg, ff) = self.facts_of(f);
        &seg.p.imports[ff.imp_start as usize..(ff.imp_start + ff.imp_count) as usize]
    }
    pub fn exports_of(&self, f: FileId) -> &[ExportRec] {
        let (seg, ff) = self.facts_of(f);
        &seg.p.exports[ff.exp_start as usize..(ff.exp_start + ff.exp_count) as usize]
    }
    pub fn scopes_of(&self, f: FileId) -> &[ScopeRec] {
        let (seg, ff) = self.facts_of(f);
        &seg.p.scopes[ff.scope_start as usize..(ff.scope_start + ff.scope_count) as usize]
    }
    pub fn bindings_of(&self, f: FileId) -> &[BindingRec] {
        let (seg, ff) = self.facts_of(f);
        &seg.p.bindings[ff.bind_start as usize..(ff.bind_start + ff.bind_count) as usize]
    }
    /// First scope id of the file (scope ids in records are global to the segment that holds them).
    pub fn scope_base(&self, f: FileId) -> u32 {
        self.facts_of(f).1.scope_start
    }
    pub fn sites_of(&self, f: FileId) -> &[SiteRec] {
        let (seg, ff) = self.facts_of(f);
        &seg.p.sites[ff.site_start as usize..(ff.site_start + ff.site_count) as usize]
    }

    /// Unresolved, ambiguous, or dynamic-receiver references across the view whose tail matches
    /// `tail` within `scope`. Uses `base.p.tail_idx` ($O(\log R)$) plus active delta files with
    /// order-independent top-K site selection across both layers.
    pub fn unresolved_refs_by_tail(&self, tail: &str, scope: UnresolvedScope, max_sites: usize) -> UnresolvedRefSummary {
        let mut out = UnresolvedRefSummary::default();
        if tail.len() < 2 {
            return out;
        }
        let mut push_ref = |file: FileId, r: &RefRec, expr: &str| {
            if !matches_unresolved_scope(r.kind, scope) {
                return;
            }
            match r.status {
                status::AMBIGUOUS => out.ambiguous += 1,
                status::LOCAL_BINDING if is_member_expr(expr) => out.dynamic += 1,
                status::NO_CANDIDATE => out.no_candidate += 1,
                _ => return,
            }
            if max_sites > 0 {
                self.insert_top_site(&mut out.sites, max_sites, UnresolvedRefSite { file, line: r.line, status: r.status, expr: expr.to_owned() });
            }
        };
        if !self.base.p.tail_idx.is_empty() {
            if let Some(tid) = self.base.names.find(tail.as_bytes()) {
                let idx = self.base.p.tail_idx.partition_point(|t| t.tail.0 < tid.0);
                for t in &self.base.p.tail_idx[idx..] {
                    if t.tail != tid {
                        break;
                    }
                    let r = &self.base.p.refs[t.ref_idx as usize];
                    let file = self.base.p.nodes_hot[r.from.idx()].file;
                    if self.delta_file(file).is_some() {
                        continue;
                    }
                    push_ref(file, r, self.base.text.get(r.name));
                }
            }
        }
        if let Some(d) = self.d() {
            for (pos, tri) in d.seg.p.delta_files.as_chunks::<3>().0.iter().enumerate() {
                if tri[1] == DeltaState::Removed as u32 {
                    continue;
                }
                let file = FileId(tri[0]);
                let rec = &d.seg.p.files[pos];
                for r in &d.seg.p.refs[rec.ref_start as usize..(rec.ref_start + rec.ref_count) as usize] {
                    if self.name_str(r.tail) == tail {
                        push_ref(file, r, self.text_str(r.name));
                    }
                }
            }
        }
        out
    }

    /// Outgoing unresolved, ambiguous, or dynamic-receiver references originating from `owner`.
    pub fn unresolved_refs_from(&self, owner: NodeIdx, scope: UnresolvedScope, max_sites: usize) -> UnresolvedRefSummary {
        let mut out = UnresolvedRefSummary::default();
        let file = self.hot(owner).file;
        for r in self.refs_of_file(file) {
            if r.from != owner || !matches_unresolved_scope(r.kind, scope) {
                continue;
            }
            let expr = self.text_str(r.name);
            match r.status {
                status::AMBIGUOUS => out.ambiguous += 1,
                status::LOCAL_BINDING if is_member_expr(expr) => out.dynamic += 1,
                status::NO_CANDIDATE => out.no_candidate += 1,
                _ => continue,
            }
            if max_sites > 0 {
                self.insert_top_site(&mut out.sites, max_sites, UnresolvedRefSite { file, line: r.line, status: r.status, expr: expr.to_owned() });
            }
        }
        out
    }

    /// Maintain an order-independent sorted, deduplicated top-K site list across base and delta.
    fn insert_top_site(&self, sites: &mut Vec<UnresolvedRefSite>, max_sites: usize, cand: UnresolvedRefSite) {
        let cmp = |a: &UnresolvedRefSite, b: &UnresolvedRefSite| {
            unresolved_status_rank(a.status)
                .cmp(&unresolved_status_rank(b.status))
                .then_with(|| self.file(a.file).path().cmp(self.file(b.file).path()))
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.expr.cmp(&b.expr))
        };
        if let Some(pos) = sites.iter().position(|s| s.file == cand.file && s.line == cand.line && s.expr == cand.expr) {
            if unresolved_status_rank(cand.status) < unresolved_status_rank(sites[pos].status) {
                sites.remove(pos);
            } else {
                return;
            }
        }
        let idx = sites.partition_point(|s| cmp(s, &cand).is_lt());
        if idx < max_sites {
            sites.insert(idx, cand);
            if sites.len() > max_sites {
                sites.pop();
            }
        }
    }

    // ----------------------------------------------------------------- source
    /// Current on-disk content of a file (not the indexed version).
    pub fn read_source(&self, f: FileId) -> std::io::Result<String> {
        let root = self.root.as_ref().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no project root"))?;
        let bytes = std::fs::read(root.join(self.file(f).path()))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn root_path(&self) -> Option<&Path> {
        self.root.as_ref().map(|r| r.as_path())
    }
}

fn edge_ref(seg: &Seg, local_eid: u32, src: NodeIdx, eid_base: u32) -> EdgeRef {
    let i = local_eid as usize;
    let kb = seg.p.out_kind[i];
    let site = seg.p.out_site[i];
    let r = seg.p.out_ref[i];
    EdgeRef {
        eid: eid_base + local_eid,
        src,
        dst: NodeIdx(seg.p.out_dst[i]),
        kind: outkind::kind(kb),
        provenance: outkind::provenance(kb),
        class: outkind::class(kb),
        line: (site.line != NONE).then_some(site.line),
        col: (site.col != NONE).then_some(site.col),
        res: seg.p.out_res[i],
        ref_idx: (r != NONE).then_some(r),
    }
}

pub struct OutEdges<'a> {
    view: &'a View,
    seg: &'a Seg,
    src: NodeIdx,
    range: std::ops::Range<u32>,
    eid_base: u32,
}

impl<'a> Iterator for OutEdges<'a> {
    type Item = EdgeRef;
    fn next(&mut self) -> Option<EdgeRef> {
        let i = self.range.next()?;
        let _ = self.view;
        Some(edge_ref(self.seg, i, self.src, self.eid_base))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.range.size_hint()
    }
}

impl StrPool {
    #[allow(dead_code)]
    fn _touch(&self) {}
}
