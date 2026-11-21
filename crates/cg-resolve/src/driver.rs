//! Resolution rounds (design doc 6.4). Round 1 resolves imports and type references, which yields
//! the type hierarchy; round 2 resolves calls and everything else against it. Inside a round
//! references are independent, so results never depend on thread scheduling.

use crate::engine::Engine;
use crate::resolve::{by, FileCx, Outcome, WorkerCx};
use cg_core::{EdgeKind, FileId, Lang, NodeIdx, NodeKind, Provenance, RefKind, NONE};
use cg_store::{outkind, status, EdgeBuild, RefUpdate};
use rayon::prelude::*;

#[derive(Default)]
pub struct ResolveOutput {
    /// One entry per reference of the view (global ref ids).
    pub updates: Vec<RefUpdate>,
    pub edges: Vec<EdgeBuild>,
    pub emeta: Vec<String>,
    /// Per `ImportRec` in segment order.
    pub import_targets: Vec<u32>,
    /// Per `ExportRec` in segment order: the module a re-export points to.
    pub export_targets: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Round {
    One,
    Two,
}

fn in_round(kind: u8, round: Round) -> bool {
    let first = matches!(
        RefKind::from_wire(kind),
        Some(RefKind::Edge(
            EdgeKind::Imports | EdgeKind::Extends | EdgeKind::Implements | EdgeKind::TypeOf | EdgeKind::Returns | EdgeKind::References | EdgeKind::Instantiates
        ))
    );
    (round == Round::One) == first
}

/// Which resolution round handles a reference of this wire kind.
pub fn round_of(kind: u8) -> Round_ {
    if in_round(kind, Round::One) {
        Round_::One
    } else {
        Round_::Two
    }
}

/// Resolve the references `ids` (global ids) of the given files. Used by full builds (all files
/// and refs) and by incremental sync (a subset).
pub fn resolve_refs<'e, 'v>(e: &'e Engine<'v>, files: &[FileId], round: Round_, only: Option<&rustc_hash::FxHashSet<u32>>) -> Vec<(u32, Outcome)> {
    let round = match round {
        Round_::One => Round::One,
        Round_::Two => Round::Two,
    };
    let parts: Vec<Vec<(u32, Outcome)>> = files
        .par_iter()
        .map_init(WorkerCx::new, |wcx, &f| {
            let view = e.view;
            let range = view.ref_range(f);
            let refs = view.refs_of_file(f);
            let wanted = refs.iter().enumerate().any(|(i, r)| in_round(r.kind, round) && only.is_none_or(|s| s.contains(&(range.start + i as u32))));
            if !wanted {
                return Vec::new();
            }
            let fcx = FileCx::new(e, f);
            let mut out = Vec::with_capacity(refs.len());
            for (i, r) in refs.iter().enumerate() {
                let id = range.start + i as u32;
                if !in_round(r.kind, round) || only.is_some_and(|s| !s.contains(&id)) {
                    continue;
                }
                out.push((id, fcx.resolve_ref(wcx, r)));
            }
            out
        })
        .collect();
    parts.into_iter().flatten().collect()
}

/// Full builds write directly into disjoint file ranges in the final outcome array. There
/// is no per-file result vector followed by a flattened round vector and another copy.
fn resolve_into(e: &Engine<'_>, files: &[FileId], round: Round, outcomes: &mut [Outcome]) {
    let mut files = files.to_vec();
    files.sort_unstable_by_key(|f| e.view.ref_range(*f).start);
    let mut jobs = Vec::with_capacity(files.len());
    let mut remaining = outcomes;
    let mut cursor = 0usize;
    for f in files {
        let range = e.view.ref_range(f);
        if range.is_empty() {
            continue;
        }
        let (_, rest) = remaining.split_at_mut(range.start as usize - cursor);
        let (slots, rest) = rest.split_at_mut(range.len());
        jobs.push((f, slots));
        remaining = rest;
        cursor = range.end as usize;
    }
    // Balance work across Rayon's binary-tree splits: sort descending by reference count, then
    // place rank `r` by `r.reverse_bits()` so heavy files distribute evenly across every half.
    jobs.sort_unstable_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0 .0.cmp(&b.0 .0)));
    let mut ranked: Vec<(usize, (FileId, &mut [Outcome]))> = jobs.into_iter().enumerate().collect();
    ranked.sort_unstable_by_key(|(r, _)| r.reverse_bits());
    ranked.into_par_iter().with_max_len(4).for_each_init(WorkerCx::new, |wcx, (_, (file, slots))| {
        let refs = e.view.refs_of_file(file);
        if !refs.iter().any(|r| in_round(r.kind, round)) {
            return;
        }
        let cx = FileCx::new(e, file);
        for (slot, reference) in slots.iter_mut().zip(refs) {
            if in_round(reference.kind, round) {
                *slot = cx.resolve_ref(wcx, reference);
            }
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Round_ {
    One,
    Two,
}

pub fn hierarchy_pairs(e: &Engine<'_>, outcomes: &[(u32, Outcome)]) -> Vec<(NodeIdx, NodeIdx)> {
    outcomes
        .iter()
        .filter(|(_, o)| o.status == status::RESOLVED)
        .filter_map(|(id, o)| {
            let r = e.view.reference(*id);
            matches!(RefKind::from_wire(r.kind), Some(RefKind::Edge(EdgeKind::Extends | EdgeKind::Implements))).then_some((r.from, NodeIdx(o.target)))
        })
        .collect()
}

/// Kind of the edge a resolved reference becomes (design doc 6.3, last paragraph).
pub fn final_kind(rk: RefKind, src: NodeKind, dst: NodeKind, lang: Lang) -> EdgeKind {
    match rk {
        RefKind::FunctionRef => EdgeKind::References,
        RefKind::Edge(EdgeKind::Extends)
            if matches!(dst, NodeKind::Interface | NodeKind::Protocol | NodeKind::Trait)
                && !matches!(src, NodeKind::Interface | NodeKind::Protocol | NodeKind::Trait) =>
        {
            EdgeKind::Implements
        }
        RefKind::Edge(EdgeKind::Calls) => {
            if lang == Lang::Go && dst.is_type() {
                EdgeKind::References
            } else if matches!(dst, NodeKind::Class | NodeKind::Struct | NodeKind::Union) {
                EdgeKind::Instantiates
            } else {
                EdgeKind::Calls
            }
        }
        RefKind::Edge(k) => k,
    }
}

pub fn make_edge(e: &Engine<'_>, id: u32, o: &Outcome, fnref_meta: u32) -> Option<EdgeBuild> {
    if o.status != status::RESOLVED {
        return None;
    }
    let view = e.view;
    let r = view.reference(id);
    let dst = NodeIdx(o.target);
    let rk = RefKind::from_wire(r.kind)?;
    let from_file = view.hot(r.from).file;
    let lang = view.file(from_file).lang();
    let src_kind = NodeKind::from_u8(view.hot(r.from).kind).unwrap_or(NodeKind::File);
    let dst_kind = NodeKind::from_u8(view.hot(dst).kind).unwrap_or(NodeKind::Variable);
    let kind = final_kind(rk, src_kind, dst_kind, lang);
    if r.from == dst && kind == EdgeKind::Imports {
        return None;
    }
    let prov = if matches!(o.by, by::PACKAGE | by::GLOBAL) { Provenance::Heuristic } else { Provenance::TreeSitter };
    Some(EdgeBuild {
        src: r.from.0,
        dst: dst.0,
        kind: outkind::pack(kind, prov, outkind::CLASS_RESOLVE),
        line: r.line,
        col: r.col,
        res: o.by as u16 | (o.conf as u16) << 8,
        ref_idx: id,
        meta: if matches!(rk, RefKind::FunctionRef) { fnref_meta } else { NONE },
        order: 0,
    })
}

pub const FNREF_META: &str = r#"{"fnRef":true}"#;

pub fn update_of(o: &Outcome) -> RefUpdate {
    RefUpdate { status: o.status, by: o.by, target: o.target }
}

/// Full resolution of the view held by `e` (design doc 6.4). Consumes the hierarchy slot of the
/// engine, so call it on a fresh engine.
pub fn resolve_all(e: &mut Engine<'_>) -> ResolveOutput {
    let view = e.view;
    let n_refs = view.n_refs();
    let files: Vec<FileId> = view.files().collect();
    let mut outcomes = vec![Outcome::PENDING; n_refs];

    resolve_into(e, &files, Round::One, &mut outcomes);
    let mut pairs = Vec::new();
    for (id, o) in outcomes.iter().enumerate() {
        let r = view.reference(id as u32);
        if o.status == status::RESOLVED && matches!(RefKind::from_wire(r.kind), Some(RefKind::Edge(EdgeKind::Extends | EdgeKind::Implements))) {
            pairs.push((r.from, NodeIdx(o.target), (r.line, r.col)));
        }
    }
    e.set_hierarchy(pairs.into_iter());
    resolve_into(e, &files, Round::Two, &mut outcomes);

    let mut edges = Vec::new();
    for (id, o) in outcomes.iter().enumerate() {
        if let Some(edge) = make_edge(e, id as u32, o, 0) {
            edges.push(edge);
        }
    }
    let mut import_targets = Vec::new();
    for f in view.files() {
        for t in &e.imp_targets[f.idx()] {
            import_targets.push(e.modules.target_file(*t).map_or(NONE, |file| file.0));
        }
    }
    let mut export_targets = Vec::new();
    for f in view.files() {
        for ex in view.exports_of(f) {
            export_targets.push(e.export_target(f, ex));
        }
    }
    ResolveOutput { export_targets, updates: outcomes.iter().map(update_of).collect(), edges, emeta: vec![FNREF_META.to_owned()], import_targets }
}
