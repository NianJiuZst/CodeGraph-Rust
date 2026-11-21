//! Bounded graph traversals (design doc 8.1): callers, callees, impact, shortest path, type
//! hierarchy, and the file-level import graph (affected files, cycles).

use crate::graph::Graph;
use cg_core::{EdgeKind, FileId, NodeIdx, NodeKind};
use cg_store::{outkind, EdgeRef};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

/// Default edge kinds for callers / callees: calls, instantiates, references (function
/// references included) and every synthesised edge.
pub fn is_call_like(e: &EdgeRef) -> bool {
    matches!(e.kind, EdgeKind::Calls | EdgeKind::Instantiates | EdgeKind::References | EdgeKind::Navigates)
}

pub fn is_synth(e: &EdgeRef) -> bool {
    e.class == outkind::CLASS_SYNTH || e.class == outkind::CLASS_FRAMEWORK
}

/// Kinds followed by impact analysis (reverse direction).
pub fn is_dependency_edge(e: &EdgeRef) -> bool {
    matches!(
        e.kind,
        EdgeKind::Calls
            | EdgeKind::Instantiates
            | EdgeKind::References
            | EdgeKind::Extends
            | EdgeKind::Implements
            | EdgeKind::Imports
            | EdgeKind::Overrides
            | EdgeKind::Navigates
    )
}

#[derive(Clone, Debug)]
pub struct Related {
    pub node: NodeIdx,
    pub kind: EdgeKind,
    /// Distinct source lines of the relationship, ascending.
    pub lines: Vec<u32>,
    /// `synthesizedBy` when the relationship comes from a synthesis pass.
    pub synth: Option<String>,
    /// True when a `References` edge is a function/callback reference (`{"fnRef":true}`).
    pub fn_ref: bool,
    pub depth: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Impact {
    /// Affected nodes with their distance from the start node (start excluded).
    pub nodes: Vec<(NodeIdx, u32)>,
    pub edges: Vec<(NodeIdx, NodeIdx, EdgeKind)>,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct Step {
    pub node: NodeIdx,
    /// Edge from the previous step to this one (`None` for the first).
    pub edge: Option<EdgeRef>,
}

#[derive(Clone, Debug, Default)]
pub struct Hierarchy {
    pub ancestors: Vec<(NodeIdx, u32)>,
    pub descendants: Vec<(NodeIdx, u32)>,
}

/// File-level import graph: `fwd[f]` = files that `f` imports, `rev[f]` = files importing `f`.
pub struct FileGraph {
    pub fwd: Vec<Vec<u32>>,
    pub rev: Vec<Vec<u32>>,
}

impl Graph {
    pub fn synth_label(&self, e: &EdgeRef) -> Option<String> {
        if !is_synth(e) {
            return None;
        }
        let meta = self.view.edge_meta(e.eid)?;
        let v: serde_json::Value = serde_json::from_str(meta).ok()?;
        v.get("synthesizedBy").and_then(|s| s.as_str()).map(str::to_owned)
    }

    /// `registeredAt` of a synthesised edge (the wiring site as `path:line`).
    pub fn synth_site(&self, e: &EdgeRef) -> Option<String> {
        let meta = self.view.edge_meta(e.eid)?;
        let v: serde_json::Value = serde_json::from_str(meta).ok()?;
        v.get("registeredAt").and_then(|s| s.as_str()).map(str::to_owned)
    }

    /// True when `e` is a function/callback reference (`{"fnRef":true}`).
    pub fn is_fn_ref(&self, e: &EdgeRef) -> bool {
        e.kind == EdgeKind::References && self.view.edge_meta(e.eid).is_some_and(|m| m.contains("\"fnRef\":true"))
    }

    /// True when `e` crosses a process/tier boundary (HTTP client, queue job, socket event, Celery).
    /// Local in-process `eventEmitter.emit()` -> `@OnEvent` links carry no `"tier"` field and stay
    /// in-process.
    pub fn is_cross_tier(&self, e: &EdgeRef) -> bool {
        if !is_synth(e) {
            return false;
        }
        let Some(meta) = self.view.edge_meta(e.eid) else { return false };
        meta.contains("\"tier\"")
            || meta.contains("\"synthesizedBy\":\"http-client\"")
            || meta.contains("\"synthesizedBy\":\"queue-job\"")
            || meta.contains("\"synthesizedBy\":\"celery-dispatch\"")
    }

    fn collect_related(&self, start: NodeIdx, depth: u32, limit: usize, reverse: bool) -> Vec<Related> {
        let mut out: Vec<Related> = Vec::new();
        let mut seen: FxHashSet<NodeIdx> = FxHashSet::default();
        seen.insert(start);
        let mut frontier = vec![start];
        for d in 1..=depth.max(1) {
            let mut next: Vec<NodeIdx> = Vec::new();
            for n in frontier {
                let mut grouped: Vec<Related> = Vec::new();
                let mut index: FxHashMap<NodeIdx, usize> = FxHashMap::default();
                let mut visit = |e: EdgeRef, other: NodeIdx| {
                    if !is_call_like(&e) {
                        return;
                    }
                    let i = *index.entry(other).or_insert_with(|| {
                        grouped.push(Related { node: other, kind: e.kind, lines: Vec::new(), synth: None, fn_ref: false, depth: d });
                        grouped.len() - 1
                    });
                    let g = &mut grouped[i];
                    if let Some(l) = e.line {
                        if !g.lines.contains(&l) {
                            g.lines.push(l);
                        }
                    }
                    if g.synth.is_none() {
                        g.synth = self.synth_label(&e);
                    }
                    if !g.fn_ref && self.is_fn_ref(&e) {
                        g.fn_ref = true;
                    }
                    // calls beat references for the displayed kind
                    if e.kind == EdgeKind::Calls {
                        g.kind = EdgeKind::Calls;
                    }
                };
                if reverse {
                    for e in self.view.in_edges(n) {
                        let src = e.src;
                        visit(e, src);
                    }
                } else {
                    for e in self.view.out_edges(n) {
                        let dst = e.dst;
                        visit(e, dst);
                    }
                }
                for mut g in grouped {
                    if !seen.insert(g.node) {
                        continue;
                    }
                    g.lines.sort_unstable();
                    next.push(g.node);
                    out.push(g);
                }
            }
            if next.is_empty() || out.len() >= limit {
                break;
            }
            frontier = next;
        }
        out.sort_by(|a, b| {
            a.depth.cmp(&b.depth).then_with(|| self.node_path(a.node).cmp(self.node_path(b.node))).then_with(|| self.line(a.node).cmp(&self.line(b.node)))
        });
        out.truncate(limit);
        out
    }

    /// Symbols that call / instantiate / reference `n`.
    pub fn callers(&self, n: NodeIdx, depth: u32, limit: usize) -> Vec<Related> {
        self.collect_related(n, depth, limit, true)
    }

    /// Symbols that `n` calls / instantiates / references.
    pub fn callees(&self, n: NodeIdx, depth: u32, limit: usize) -> Vec<Related> {
        self.collect_related(n, depth, limit, false)
    }

    /// Reverse breadth-first traversal over dependency edges, bounded by `depth` and a node
    /// budget; `truncated` is set when the budget cut the traversal short.
    pub fn impact(&self, start: NodeIdx, depth: u32, node_budget: usize) -> Impact {
        let mut imp = Impact::default();
        let mut seen: FxHashSet<NodeIdx> = FxHashSet::default();
        seen.insert(start);
        let mut frontier = VecDeque::new();
        frontier.push_back((start, 0u32));
        'outer: while let Some((n, d)) = frontier.pop_front() {
            if d >= depth {
                continue;
            }
            // a container's members are changed along with it: follow callers of members too
            let mut sources = vec![n];
            if self.kind(n).is_container() && self.kind(n) != NodeKind::File {
                for e in self.view.out_edges(n) {
                    if e.kind == EdgeKind::Contains {
                        sources.push(e.dst);
                    }
                }
            }
            for s in sources {
                for e in self.view.in_edges(s) {
                    if !is_dependency_edge(&e) {
                        continue;
                    }
                    if seen.insert(e.src) {
                        if imp.nodes.len() >= node_budget {
                            imp.truncated = true;
                            break 'outer;
                        }
                        imp.nodes.push((e.src, d + 1));
                        imp.edges.push((e.src, n, e.kind));
                        frontier.push_back((e.src, d + 1));
                    }
                }
            }
        }
        imp
    }

    /// Shortest directed path from `from` to `to` along `follow` edges: breadth-first with one
    /// predecessor per node, so cost is linear in the explored region (upstream d494e43).
    pub fn find_path(&self, from: NodeIdx, to: NodeIdx, follow: impl Fn(&EdgeRef) -> bool, node_budget: usize) -> Option<Vec<Step>> {
        if from == to {
            return Some(vec![Step { node: from, edge: None }]);
        }
        let mut pred: FxHashMap<NodeIdx, (NodeIdx, EdgeRef)> = FxHashMap::default();
        let mut seen: FxHashSet<NodeIdx> = FxHashSet::default();
        seen.insert(from);
        let mut q = VecDeque::new();
        q.push_back(from);
        let mut explored = 0usize;
        while let Some(n) = q.pop_front() {
            explored += 1;
            if explored > node_budget {
                return None;
            }
            for e in self.view.out_edges(n) {
                if !follow(&e) || !seen.insert(e.dst) {
                    continue;
                }
                pred.insert(e.dst, (n, e));
                if e.dst == to {
                    let mut steps = vec![Step { node: to, edge: Some(e) }];
                    let mut cur = n;
                    while cur != from {
                        let (p, pe) = pred[&cur];
                        steps.push(Step { node: cur, edge: Some(pe) });
                        cur = p;
                    }
                    steps.push(Step { node: from, edge: None });
                    steps.reverse();
                    return Some(steps);
                }
                q.push_back(e.dst);
            }
        }
        None
    }

    /// Ancestors (supertypes) and descendants (subtypes) through extends / implements.
    pub fn hierarchy(&self, n: NodeIdx, depth: u32) -> Hierarchy {
        let mut h = Hierarchy::default();
        let walk = |up: bool| -> Vec<(NodeIdx, u32)> {
            let mut out = Vec::new();
            let mut seen: FxHashSet<NodeIdx> = FxHashSet::default();
            seen.insert(n);
            let mut q = VecDeque::new();
            q.push_back((n, 0u32));
            while let Some((cur, d)) = q.pop_front() {
                if d >= depth {
                    continue;
                }
                let mut push = |other: NodeIdx| {
                    if seen.insert(other) {
                        out.push((other, d + 1));
                        q.push_back((other, d + 1));
                    }
                };
                if up {
                    for e in self.view.out_edges(cur) {
                        if matches!(e.kind, EdgeKind::Extends | EdgeKind::Implements) {
                            push(e.dst);
                        }
                    }
                } else {
                    for e in self.view.in_edges(cur) {
                        if matches!(e.kind, EdgeKind::Extends | EdgeKind::Implements) {
                            push(e.src);
                        }
                    }
                }
            }
            out
        };
        h.ancestors = walk(true);
        h.descendants = walk(false);
        h
    }

    /// Implementations of an interface / abstract class or method: descendants of a type, or
    /// the overriding methods of a method, up to `depth` inheritance hops.
    pub fn implementations(&self, n: NodeIdx, depth: u32) -> Vec<NodeIdx> {
        let depth = depth.max(1);
        let mut out: Vec<NodeIdx> = Vec::new();
        if self.kind(n).is_type() {
            out.extend(self.hierarchy(n, depth).descendants.into_iter().map(|x| x.0));
        } else {
            for e in self.view.in_edges(n) {
                if e.kind == EdgeKind::Overrides {
                    out.push(e.src);
                }
            }
            // methods of subtypes with the same name
            if let Some(owner) = self.containers(n).first().copied() {
                let name = self.name(n).to_owned();
                for (sub, _) in self.hierarchy(owner, depth).descendants {
                    for e in self.view.out_edges(sub) {
                        if e.kind == EdgeKind::Contains && self.name(e.dst) == name && self.kind(e.dst).is_callable() {
                            out.push(e.dst);
                        }
                    }
                }
            }
        }
        out.sort_by_key(|x| (self.node_path(*x).to_owned(), self.line(*x)));
        out.dedup();
        out
    }

    /// Build the file-level import graph from `imports` edges and the resolved import / re-export
    /// records.
    pub fn file_graph(&self) -> FileGraph {
        let total = self.view.files().map(|f| f.idx() + 1).max().unwrap_or(0);
        let mut fwd: Vec<Vec<u32>> = vec![Vec::new(); total];
        for f in self.view.files() {
            let mut targets: Vec<u32> = Vec::new();
            for imp in self.view.imports_of(f) {
                if imp.target.is_some() && imp.target != f && !self.view.file_removed(imp.target) {
                    targets.push(imp.target.0);
                }
            }
            for ex in self.view.exports_of(f) {
                if ex.target.is_some() && ex.target != f && !self.view.file_removed(ex.target) {
                    targets.push(ex.target.0);
                }
            }
            for n in self.view.nodes_in_file(f) {
                let n = NodeIdx(n);
                if !matches!(self.kind(n), NodeKind::File | NodeKind::Namespace | NodeKind::Module) {
                    continue;
                }
                for e in self.view.out_edges(n) {
                    if e.kind == EdgeKind::Imports {
                        let tf = self.view.hot(e.dst).file;
                        if tf != f && !self.view.file_removed(tf) {
                            targets.push(tf.0);
                        }
                    }
                }
            }
            targets.sort_unstable();
            targets.dedup();
            fwd[f.idx()] = targets;
        }
        let mut rev: Vec<Vec<u32>> = vec![Vec::new(); total];
        for (f, ts) in fwd.iter().enumerate() {
            for t in ts {
                rev[*t as usize].push(f as u32);
            }
        }
        FileGraph { fwd, rev }
    }

    /// Build the full file-level dependency graph: imports, re-exports, and cross-file symbol
    /// edges (calls, references, extends, implements, instantiates, overrides, ...).
    /// When `include_cross_tier` is false, process-boundary edges (HTTP, queues, events, Celery)
    /// are excluded so unit-test blast radius does not spill across mocked boundaries (#3320).
    pub fn dependent_files_graph(&self, include_cross_tier: bool) -> FileGraph {
        let total = self.view.files().map(|f| f.idx() + 1).max().unwrap_or(0);
        let mut fwd: Vec<Vec<u32>> = vec![Vec::new(); total];
        for f in self.view.files() {
            let mut targets: Vec<u32> = Vec::new();
            for imp in self.view.imports_of(f) {
                if imp.target.is_some() && imp.target != f && !self.view.file_removed(imp.target) {
                    targets.push(imp.target.0);
                }
            }
            for ex in self.view.exports_of(f) {
                if ex.target.is_some() && ex.target != f && !self.view.file_removed(ex.target) {
                    targets.push(ex.target.0);
                }
            }
            for n in self.view.nodes_in_file(f) {
                for e in self.view.out_edges(NodeIdx(n)) {
                    if e.kind == EdgeKind::Contains {
                        continue;
                    }
                    if !include_cross_tier && self.is_cross_tier(&e) {
                        continue;
                    }
                    let tf = self.view.hot(e.dst).file;
                    if tf != f && !self.view.file_removed(tf) {
                        targets.push(tf.0);
                    }
                }
            }
            targets.sort_unstable();
            targets.dedup();
            fwd[f.idx()] = targets;
        }
        let mut rev: Vec<Vec<u32>> = vec![Vec::new(); total];
        for (f, ts) in fwd.iter().enumerate() {
            for t in ts {
                rev[*t as usize].push(f as u32);
            }
        }
        FileGraph { fwd, rev }
    }

    /// Direct dependent files of a single file `f` (imports, re-exports, and cross-file symbol
    /// edges) without materializing the whole-repository file graph.
    pub fn file_dependents(&self, f: FileId, include_cross_tier: bool) -> Vec<FileId> {
        let mut seen: FxHashSet<FileId> = FxHashSet::default();
        for n in self.view.nodes_in_file(f) {
            for e in self.view.in_edges(NodeIdx(n)) {
                if e.kind == EdgeKind::Contains {
                    continue;
                }
                if !include_cross_tier && self.is_cross_tier(&e) {
                    continue;
                }
                let sf = self.view.hot(e.src).file;
                if sf != f && !self.view.file_removed(sf) {
                    seen.insert(sf);
                }
            }
        }
        for u in self.view.files() {
            if u == f || seen.contains(&u) {
                continue;
            }
            if self.view.imports_of(u).iter().any(|imp| imp.target == f) || self.view.exports_of(u).iter().any(|ex| ex.target == f) {
                seen.insert(u);
            }
        }
        let mut out: Vec<FileId> = seen.into_iter().collect();
        out.sort_by(|a, b| self.path(*a).cmp(self.path(*b)));
        out
    }

    /// Files that depend (transitively, up to `depth`) on any of `changed`; `changed` included.
    pub fn affected_files(&self, g: &FileGraph, changed: &[FileId], depth: u32) -> Vec<FileId> {
        let mut seen: FxHashSet<u32> = changed.iter().map(|f| f.0).collect();
        let mut q: VecDeque<(u32, u32)> = changed.iter().map(|f| (f.0, 0)).collect();
        while let Some((f, d)) = q.pop_front() {
            if d >= depth {
                continue;
            }
            if let Some(users) = g.rev.get(f as usize) {
                for u in users {
                    if seen.insert(*u) {
                        q.push_back((*u, d + 1));
                    }
                }
            }
        }
        let mut out: Vec<FileId> = seen.into_iter().map(FileId).collect();
        out.sort_by(|a, b| self.path(*a).cmp(self.path(*b)));
        out
    }

    /// Import cycles: strongly connected components with more than one file (Tarjan,
    /// iterative), each sorted by path, the list sorted by first path.
    pub fn import_cycles(&self, g: &FileGraph) -> Vec<Vec<FileId>> {
        let n = g.fwd.len();
        let mut index = vec![u32::MAX; n];
        let mut low = vec![0u32; n];
        let mut on = vec![false; n];
        let mut stack: Vec<u32> = Vec::new();
        let mut counter = 0u32;
        let mut sccs: Vec<Vec<FileId>> = Vec::new();
        for root in 0..n as u32 {
            if index[root as usize] != u32::MAX || g.fwd[root as usize].is_empty() {
                continue;
            }
            let mut call: Vec<(u32, usize)> = vec![(root, 0)];
            index[root as usize] = counter;
            low[root as usize] = counter;
            counter += 1;
            stack.push(root);
            on[root as usize] = true;
            while let Some(&mut (v, ref mut ci)) = call.last_mut() {
                if *ci < g.fwd[v as usize].len() {
                    let w = g.fwd[v as usize][*ci];
                    *ci += 1;
                    if index[w as usize] == u32::MAX {
                        index[w as usize] = counter;
                        low[w as usize] = counter;
                        counter += 1;
                        stack.push(w);
                        on[w as usize] = true;
                        call.push((w, 0));
                    } else if on[w as usize] {
                        low[v as usize] = low[v as usize].min(index[w as usize]);
                    }
                } else {
                    call.pop();
                    if let Some(&(parent, _)) = call.last() {
                        low[parent as usize] = low[parent as usize].min(low[v as usize]);
                    }
                    if low[v as usize] == index[v as usize] {
                        let mut comp = Vec::new();
                        loop {
                            let w = stack.pop().unwrap();
                            on[w as usize] = false;
                            comp.push(FileId(w));
                            if w == v {
                                break;
                            }
                        }
                        if comp.len() > 1 {
                            comp.sort_by(|a, b| self.path(*a).cmp(self.path(*b)));
                            sccs.push(comp);
                        }
                    }
                }
            }
        }
        sccs.sort_by(|a, b| self.path(a[0]).cmp(self.path(b[0])));
        sccs
    }
}
