//! Builder, stage 2 (design doc 5.5 steps 4-6): fold resolution/synthesis output into the
//! draft, assemble the final edge arrays and build every search index.

use crate::build::{Draft, EdgeBuild};
use crate::edges;
use crate::index::{self, TokenInputs};
use crate::part::Part;
use crate::records::*;
use crate::seg::Seg;
use cg_core::{NodeKind, StrId, NONE};
use rayon::prelude::*;

/// Outcome for one reference (parallel to the refs array).
#[derive(Clone, Copy, Debug, Default)]
pub struct RefUpdate {
    pub status: u8,
    pub by: u8,
    pub target: u32,
}

#[derive(Default)]
pub struct FinishInput {
    /// Empty = leave all references pending.
    pub ref_updates: Vec<RefUpdate>,
    /// Resolution / synthesis edges; `EdgeBuild::meta` indexes `emeta` below.
    pub edges: Vec<EdgeBuild>,
    pub emeta: Vec<String>,
    /// Per `ImportRec`: the module's file, or NONE. Empty = unchanged.
    pub import_targets: Vec<u32>,
    pub export_targets: Vec<u32>,
    pub sites: Vec<SiteRec>,
    pub site_ranges: Vec<(u32, u32)>,
}

fn into_vec<T: bytemuck::Pod>(p: &Part<T>) -> Vec<T> {
    p.to_vec()
}

pub fn finish(draft: Draft, input: FinishInput) -> Seg {
    let Draft { mut seg, edges: mut all_edges, mut emeta } = draft;
    let n_nodes = seg.p.nodes_hot.len();

    // ---- references ----
    if !input.ref_updates.is_empty() {
        let mut refs = into_vec(&seg.p.refs);
        assert_eq!(refs.len(), input.ref_updates.len(), "one update per reference");
        refs.par_iter_mut().zip(input.ref_updates.par_iter()).for_each(|(r, u)| {
            r.status = u.status;
            r.by = u.by;
            r.target = cg_core::NodeIdx(u.target);
        });
        seg.p.refs = Part::owned(refs);
    }
    if !input.import_targets.is_empty() {
        let mut v = into_vec(&seg.p.imports);
        for (r, t) in v.iter_mut().zip(&input.import_targets) {
            r.target = cg_core::FileId(*t);
        }
        seg.p.imports = Part::owned(v);
    }
    if !input.export_targets.is_empty() {
        let mut v = into_vec(&seg.p.exports);
        for (r, t) in v.iter_mut().zip(&input.export_targets) {
            r.target = cg_core::FileId(*t);
        }
        seg.p.exports = Part::owned(v);
    }
    if !input.sites.is_empty() {
        seg.p.sites = Part::owned(input.sites);
        let mut ff = into_vec(&seg.p.file_facts);
        for (f, (s, c)) in ff.iter_mut().zip(&input.site_ranges) {
            f.site_start = *s;
            f.site_count = *c;
        }
        seg.p.file_facts = Part::owned(ff);
    }

    // ---- edges, tail index, name indexes and token index are independent: build in parallel ----
    let meta_base = emeta.len() as u32;
    for mut e in input.edges {
        if e.meta != NONE {
            e.meta += meta_base;
        }
        all_edges.push(e);
    }
    emeta.extend(input.emeta);
    let mut em_off = Vec::with_capacity(emeta.len() + 1);
    let mut em_data = Vec::new();
    em_off.push(0u32);
    for m in &emeta {
        em_data.extend_from_slice(m.as_bytes());
        em_off.push(em_data.len() as u32);
    }
    seg.p.emeta_off = Part::owned(em_off);
    seg.p.emeta_data = Part::owned(em_data);

    let name_post_off = &seg.p.name_post_off;
    let qname_post_off = &seg.p.qname_post_off;
    let ((asm, tails), (ni, ti)) = rayon::join(
        || {
            rayon::join(
                || edges::assemble(&mut all_edges, n_nodes),
                || {
                    let mut tails: Vec<TailRec> = seg.p.refs.par_iter().enumerate().map(|(i, r)| TailRec { tail: r.tail, ref_idx: i as u32 }).collect();
                    tails.par_sort_unstable_by_key(|t| (t.tail.0, t.ref_idx));
                    tails
                },
            )
        },
        || {
            rayon::join(
                || index::build_name_indexes(&seg.names, |i| name_post_off[i + 1] > name_post_off[i] || qname_post_off[i + 1] > qname_post_off[i]),
                || {
                    index::build_token_index(&TokenInputs {
                        hot: &seg.p.nodes_hot,
                        cold: &seg.p.nodes_cold,
                        files: &seg.p.files,
                        names: &seg.names,
                        paths: &seg.paths,
                        text: &seg.text,
                        names_base: 0,
                        paths_base: 0,
                        text_base: 0,
                        node_base: 0,
                        file_slot: None,
                    })
                },
            )
        },
    );
    edges::install(&mut seg, asm);
    seg.p.tail_idx = Part::owned(tails);
    seg.p.names_fst = Part::owned(ni.names_fst);
    seg.p.lower_fst = Part::owned(ni.lower_fst);
    seg.p.lower_post_off = Part::owned(ni.lower_post_off);
    seg.p.lower_post = Part::owned(ni.lower_post);
    seg.p.segs_fst = Part::owned(ni.segs_fst);
    seg.p.seg_post_off = Part::owned(ni.seg_post_off);
    seg.p.seg_post = Part::owned(ni.seg_post);
    seg.p.tri_keys = Part::owned(ni.tri_keys);
    seg.p.tri_off = Part::owned(ni.tri_off);
    seg.p.tri_post = Part::owned(ni.tri_post);
    seg.p.tokens_fst = Part::owned(ti.fst);
    seg.p.token_post_off = Part::owned(ti.post_off);
    seg.p.token_post = Part::owned(ti.post);
    seg.p.token_df = Part::owned(ti.df);
    seg.p.node_norm = Part::owned(ti.node_norm);
    seg.meta.total_tokens = seg.p.node_norm.iter().map(|n| *n as u64).sum();
    seg.p.filestats = Part::owned(file_stats(&seg));

    seg.meta.n_edges = seg.p.out_dst.len() as u32;
    seg.rebuild_derived()
}

fn file_stats(seg: &Seg) -> Vec<FileStat> {
    let hot = &seg.p.nodes_hot;
    let files = &seg.p.files;
    files
        .par_iter()
        .enumerate()
        .map(|(fi, f)| {
            let mut st = FileStat { intra_edges: 0, out_edges: 0, in_edges: 0, symbols: 0, flags: f.flags as u32, _p: 0 };
            let (a, b) = (f.node_start.0, f.node_start.0 + f.node_count);
            for n in a..b {
                let h = &hot[n as usize];
                if NodeKind::from_u8(h.kind).is_some_and(|k| k.is_definition()) {
                    st.symbols += 1;
                }
                for e in seg.p.out_off[n as usize]..seg.p.out_off[n as usize + 1] {
                    let d = seg.p.out_dst[e as usize] as usize;
                    if outkind::kind(seg.p.out_kind[e as usize]) == cg_core::EdgeKind::Contains {
                        continue;
                    }
                    if hot[d].file.0 as usize == fi {
                        st.intra_edges += 1;
                    } else {
                        st.out_edges += 1;
                    }
                }
                for k in seg.p.in_off[n as usize]..seg.p.in_off[n as usize + 1] {
                    let s = seg.p.in_src[k as usize] as usize;
                    if hot[s].file.0 as usize != fi {
                        st.in_edges += 1;
                    }
                }
            }
            st
        })
        .collect()
}

#[allow(dead_code)]
fn _id(_: StrId) {}
