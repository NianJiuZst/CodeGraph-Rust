//! Resolution tables (design doc 6.2, round 0): import targets, export tables with re-export
//! closure, package definition maps, member lookup and the type hierarchy. Everything here is
//! built once, then shared read-only by the parallel rounds.

use crate::modules::{ImportTarget, Modules, PkgId};
use crate::project::ProjectInfo;
use cg_core::{EdgeKind, FileId, Lang, LangFamily, NodeIdx, NodeKind, StrId, NONE};
use cg_extract::facts::COMMONJS_MODULE;
use cg_extract::{BindKind, ImportKind};
use cg_store::{outkind, status, View};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

#[derive(Clone, Copy, Debug)]
pub enum ExportEntry {
    Node(NodeIdx),
    /// `export { name as x } from './f'`: look `name` up in file.
    Re(FileId, StrId),
    /// `export * as ns from './f'` or a child module: the module itself.
    Module(FileId),
}

type ExportMap<'v> = FxHashMap<&'v str, ExportEntry>;
type FileExports<'v> = (usize, ExportMap<'v>, ExportMap<'v>, Vec<FileId>);

/// Result of binding a local name through an import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Node(NodeIdx),
    Module(FileId),
    Pkg(PkgId),
    External,
    Unknown,
}

pub struct Engine<'v> {
    pub budget: cg_core::budget::MemBudget,
    pub view: &'v View,
    pub project: &'v ProjectInfo,
    pub modules: Modules<'v>,
    /// Per file (indexed by `FileId`): targets aligned with `view.imports_of(file)`.
    pub imp_targets: Vec<Vec<ImportTarget>>,
    pub exports: Vec<FxHashMap<&'v str, ExportEntry>>,
    /// JS/TS type declarations sharing an exported name with a runtime value.
    pub type_exports: Vec<FxHashMap<&'v str, ExportEntry>>,
    pub stars: Vec<Vec<FileId>>,
    /// Per package: top-level definitions by name.
    pub pkg_defs: Vec<FxHashMap<&'v str, SmallVec<[NodeIdx; 2]>>>,
    /// Direct supertypes (`extends` / `implements` resolved in round 1), in the order written.
    pub supers: FxHashMap<NodeIdx, SmallVec<[NodeIdx; 3]>>,
    /// Python classes' method resolution order (C3), past the class itself.
    mro: FxHashMap<NodeIdx, Vec<NodeIdx>>,
    /// Rust methods of `impl Trait for X` blocks: an inherent `X::m` shadows them.
    trait_methods: rustc_hash::FxHashSet<NodeIdx>,
    /// C++ namespaces opened by a `#define` anywhere in the project.
    pub macro_namespaces: rustc_hash::FxHashSet<&'v str>,
    /// JavaScript functions with syntax evidence that they implement an object member.
    pub member_functions: FxHashMap<&'v str, SmallVec<[NodeIdx; 2]>>,
    pub return_types: FxHashMap<NodeIdx, &'v cg_store::BindingRec>,
    /// ES5 constructor functions: the functions given to each one's prototype
    /// (`X.prototype.m = function m() {}`), by member name.
    pub proto: FxHashMap<NodeIdx, SmallVec<[(&'v str, NodeIdx); 4]>>,
    /// A prototype member's constructor (what `this` is in it).
    pub proto_owner: FxHashMap<NodeIdx, NodeIdx>,
    /// Rust `impl Deref` facts by the name of the type they are written for.
    pub derefs: FxHashMap<&'v str, SmallVec<[(FileId, &'v cg_store::BindingRec); 1]>>,
    /// pytest fixtures by name.
    pub fixtures: FxHashMap<&'v str, SmallVec<[(FileId, &'v cg_store::BindingRec); 1]>>,
    /// Rust `type X = Y;` targets by the alias's node.
    pub type_aliases: FxHashMap<NodeIdx, &'v cg_store::BindingRec>,
    pub django_model: Option<NodeIdx>,
    pub n_files: usize,
}

fn is_top_level(view: &View, n: NodeIdx) -> bool {
    match view.node(n).parent() {
        Some(p) => view.hot(p).kind == NodeKind::File as u8,
        None => false,
    }
}

impl<'v> Engine<'v> {
    pub fn build(view: &'v View, project: &'v ProjectInfo) -> Engine<'v> {
        Self::build_with_budget(view, project, cg_core::budget::MemBudget::detect())
    }

    pub fn build_with_budget(view: &'v View, project: &'v ProjectInfo, budget: cg_core::budget::MemBudget) -> Engine<'v> {
        let modules = Modules::build(view, project);
        let n_files = modules.file_pkg.len().max(view.n_files());
        let files: Vec<FileId> = view.files().collect();

        // ---- import targets (parallel by file) ----
        let mut imp_targets: Vec<Vec<ImportTarget>> = vec![Vec::new(); n_files];
        let computed: Vec<(usize, Vec<ImportTarget>)> = files
            .par_iter()
            .map(|&f| {
                let lang = view.file(f).lang();
                let targets = view
                    .imports_of(f)
                    .iter()
                    .map(|imp| {
                        if imp.kind == ImportKind::SideEffect as u8 && imp.spec.0 == NONE {
                            return ImportTarget::External;
                        }
                        let spec = view.text_str(imp.spec);
                        let tgt = modules.resolve_import(lang, spec, f);
                        if lang == Lang::Rust && matches!(tgt, ImportTarget::External | ImportTarget::NotFound) {
                            let imported = view.text_str(imp.imported);
                            if !spec.is_empty() && !imported.is_empty() && imported != "*" {
                                let full = format!("{spec}::{imported}");
                                if let (ImportTarget::File(tf), 0) = modules.resolve_rust_path(&full, f) {
                                    return ImportTarget::File(tf);
                                }
                            }
                        }
                        tgt
                    })
                    .collect();
                (f.idx(), targets)
            })
            .collect();
        for (i, t) in computed {
            imp_targets[i] = t;
        }
        let mut member_functions: FxHashMap<&'v str, SmallVec<[NodeIdx; 2]>> = FxHashMap::default();
        for &f in &files {
            for b in view.bindings_of(f).iter().filter(|b| b.kind == BindKind::MemberFn as u8 && b.node.is_some()) {
                member_functions.entry(view.text_str(b.name)).or_default().push(b.node);
            }
        }

        let mut e = Engine {
            budget,
            view,
            project,
            modules,
            imp_targets,
            exports: vec![FxHashMap::default(); n_files],
            type_exports: vec![FxHashMap::default(); n_files],
            stars: vec![Vec::new(); n_files],
            pkg_defs: Vec::new(),
            supers: FxHashMap::default(),
            mro: FxHashMap::default(),
            trait_methods: files
                .iter()
                .filter(|f| view.file(**f).lang() == Lang::Rust)
                .flat_map(|&f| view.bindings_of(f))
                .filter(|b| b.kind == BindKind::TraitImpl as u8 && b.node.is_some())
                .map(|b| b.node)
                .collect(),
            macro_namespaces: files
                .iter()
                .filter(|f| view.file(**f).lang() == Lang::Cpp)
                .flat_map(|&f| view.bindings_of(f))
                .filter(|b| b.kind == BindKind::MacroNamespace as u8)
                .map(|b| view.text_str(b.name))
                .collect(),
            member_functions,
            return_types: files
                .iter()
                .flat_map(|&f| view.bindings_of(f))
                .filter(|b| b.kind == BindKind::ReturnType as u8 && b.node.is_some())
                .map(|b| (b.node, b))
                .collect(),
            proto: FxHashMap::default(),
            proto_owner: FxHashMap::default(),
            derefs: {
                let mut m: FxHashMap<&'v str, SmallVec<[(FileId, &'v cg_store::BindingRec); 1]>> = FxHashMap::default();
                for &f in files.iter().filter(|f| view.file(**f).lang() == Lang::Rust) {
                    for b in view.bindings_of(f).iter().filter(|b| b.kind == BindKind::Deref as u8) {
                        let name = view.text_str(b.name);
                        m.entry(name.rsplit("::").next().unwrap_or(name)).or_default().push((f, b));
                    }
                }
                m
            },
            fixtures: {
                let mut m: FxHashMap<&'v str, SmallVec<[(FileId, &'v cg_store::BindingRec); 1]>> = FxHashMap::default();
                for &f in files.iter().filter(|f| view.file(**f).lang() == Lang::Python) {
                    for b in view.bindings_of(f).iter().filter(|b| b.kind == BindKind::Fixture as u8) {
                        m.entry(view.text_str(b.name)).or_default().push((f, b));
                    }
                }
                m
            },
            type_aliases: files
                .iter()
                .filter(|f| matches!(view.file(**f).lang(), Lang::Rust | Lang::Cpp))
                .flat_map(|&f| view.bindings_of(f))
                .filter(|b| b.kind == BindKind::TypeAlias as u8 && b.node.is_some())
                .map(|b| (b.node, b))
                .collect(),
            django_model: None,
            n_files,
        };
        e.build_exports(&files);
        e.build_prototypes(&files);
        if project.py_deps.contains("django") {
            if let Some(f) = files.iter().copied().find(|f| view.file(*f).lang() == Lang::Python) {
                if let ImportTarget::File(base) = e.modules.resolve_import(Lang::Python, "django.db.models.base", f) {
                    if let Some(Bound::Node(n)) = e.lookup_export(base, "Model", 0) {
                        e.django_model = Some(n);
                    }
                }
            }
        }
        e.build_pkg_defs(&files);
        e
    }

    fn top_level_nodes(&self, f: FileId) -> impl Iterator<Item = NodeIdx> + use<'_, 'v> {
        let range = self.view.nodes_in_file(f);
        range.map(NodeIdx).filter(move |n| {
            let h = self.view.hot(*n);
            let k = NodeKind::from_u8(h.kind);
            k.is_some_and(|k| k.is_definition()) && is_top_level(self.view, *n)
        })
    }

    fn build_exports(&mut self, files: &[FileId]) {
        let view = self.view;
        let results: Vec<FileExports<'v>> = files
            .par_iter()
            .map(|&f| {
                let lang = view.file(f).lang();
                let mut map: FxHashMap<&'v str, ExportEntry> = FxHashMap::default();
                let mut types: FxHashMap<&'v str, ExportEntry> = FxHashMap::default();
                let mut stars = Vec::new();
                let family = lang.family();
                // Local definitions
                for n in self.top_level_nodes(f) {
                    let node = view.node(n);
                    let exported = match family {
                        LangFamily::Js => node.is_exported() == Some(true),
                        _ => true,
                    };
                    if exported {
                        let name = node.name();
                        if family == LangFamily::Js && node.kind().is_type() {
                            match types.get(name) {
                                Some(ExportEntry::Node(prev)) if node.kind() == NodeKind::Class && view.node(*prev).kind() != NodeKind::Class => {
                                    types.insert(name, ExportEntry::Node(n));
                                }
                                Some(_) => {}
                                None => {
                                    types.insert(name, ExportEntry::Node(n));
                                }
                            }
                        }
                        match map.get(name) {
                            // class beats interface/type alias for merged declarations
                            Some(ExportEntry::Node(prev))
                                if family == LangFamily::Js
                                    && matches!(view.node(*prev).kind(), NodeKind::Interface | NodeKind::TypeAlias)
                                    && !node.kind().is_type() =>
                            {
                                map.insert(name, ExportEntry::Node(n));
                            }
                            Some(ExportEntry::Node(prev)) if !(node.kind() == NodeKind::Class && view.node(*prev).kind() != NodeKind::Class) => {}
                            _ => {
                                map.insert(name, ExportEntry::Node(n));
                            }
                        }
                    }
                }
                // Explicit export facts (JS re-exports, `pub use`) and default exports
                for ex in view.exports_of(f) {
                    let name = view.text_str(ex.name);
                    if name == cg_extract::facts::js::AMBIENT_MODULE {
                        continue; // a `declare module` block (see `Modules::ambient`)
                    }
                    let spec = view.text_str(ex.from_spec);
                    if name == COMMONJS_MODULE && spec.is_empty() && view.text_str(ex.from_name) == "*" {
                        map.insert(name, ExportEntry::Module(f));
                        continue;
                    }
                    if ex.star != 0 {
                        if let Some(ImportTarget::File(tf)) = Some(self.modules.resolve_import(lang, spec, f)) {
                            stars.push(tf);
                        }
                        continue;
                    }
                    if !spec.is_empty() {
                        if let ImportTarget::File(tf) = self.modules.resolve_import(lang, spec, f) {
                            if view.text_str(ex.from_name) == "*" {
                                map.insert(name, ExportEntry::Module(tf));
                            } else {
                                map.insert(name, ExportEntry::Re(tf, ex.from_name));
                            }
                        }
                    } else if ex.node.is_some() {
                        map.insert(name, ExportEntry::Node(ex.node));
                    } else {
                        // `export { a as b }` / `export default a`: alias of a local definition
                        let local = view.text_str(ex.from_name);
                        if family == LangFamily::Js {
                            if let Some(n) = self.top_level_nodes(f).find(|n| view.node(*n).name() == local && view.node(*n).kind().is_type()) {
                                types.insert(name, ExportEntry::Node(n));
                            }
                        }
                        let imported = (family == LangFamily::Js)
                            .then(|| {
                                view.imports_of(f).iter().enumerate().find(|(_, imp)| view.text_str(imp.local) == local && imp.scope.0 == view.scope_base(f))
                            })
                            .flatten();
                        if let Some((i, imp)) = imported {
                            if let Some(ImportTarget::File(tf)) = self.imp_targets[f.idx()].get(i).copied() {
                                let from = view.text_str(imp.imported);
                                let entry = if imp.kind == ImportKind::Namespace as u8 || from == "*" {
                                    ExportEntry::Module(tf)
                                } else {
                                    ExportEntry::Re(tf, imp.imported)
                                };
                                map.insert(name, entry);
                            }
                        } else if let Some(n) = (family == LangFamily::Js)
                            .then(|| {
                                self.top_level_nodes(f)
                                    .find(|n| view.node(*n).name() == local && !matches!(view.node(*n).kind(), NodeKind::Interface | NodeKind::TypeAlias))
                            })
                            .flatten()
                            .or_else(|| self.top_level_nodes(f).find(|n| view.node(*n).name() == local))
                        {
                            map.insert(name, ExportEntry::Node(n));
                        }
                    }
                }
                // Python packages re-export what their `__init__` imports
                if family == LangFamily::Python {
                    let targets = &self.imp_targets[f.idx()];
                    for (i, imp) in view.imports_of(f).iter().enumerate() {
                        let local = view.text_str(imp.local);
                        if local.is_empty() || local == "*" {
                            if local == "*" {
                                if let Some(ImportTarget::File(tf)) = targets.get(i).copied() {
                                    stars.push(tf);
                                }
                            }
                            continue;
                        }
                        if imp.kind == ImportKind::Named as u8 {
                            if let Some(ImportTarget::File(tf)) = targets.get(i).copied() {
                                map.entry(local).or_insert(ExportEntry::Re(tf, imp.imported));
                            }
                        }
                    }
                }
                (f.idx(), map, types, stars)
            })
            .collect();
        for (i, m, t, s) in results {
            self.exports[i] = m;
            self.type_exports[i] = t;
            self.stars[i] = s;
        }
        // Rust child modules (`mod x;` -> x.rs) are exports of their parent module
        for &f in files {
            if self.view.file(f).lang() != Lang::Rust {
                continue;
            }
            let Some((crate_idx, modpath)) = self.modules.rust_module_of(f).cloned() else { continue };
            if modpath.is_empty() && self.modules.rust_module_file(crate_idx, "") != Some(f) {
                continue;
            }
            // children are discovered from the other direction: register each file under its parent
            let _ = (crate_idx, modpath);
        }
        let rust_children: Vec<(FileId, &'v str, FileId)> = files
            .iter()
            .filter_map(|&f| {
                if self.view.file(f).lang() != Lang::Rust {
                    return None;
                }
                let (ci, mp) = self.modules.rust_module_of(f)?;
                if mp.is_empty() {
                    return None;
                }
                let (parent, last) = match mp.rfind("::") {
                    Some(i) => (&mp[..i], &mp[i + 2..]),
                    None => ("", mp.as_str()),
                };
                let pf = self.modules.rust_module_file(*ci, parent)?;
                // the &str must live as long as the view: take it from the path's own text
                let path = self.view.file(f).path();
                let stem = path.rsplit('/').next().unwrap_or("");
                let name: &'v str = if stem == "mod.rs" { path.rsplit('/').nth(1).unwrap_or("") } else { stem.strip_suffix(".rs").unwrap_or(stem) };
                let _ = last;
                Some((pf, name, f))
            })
            .collect();
        for (pf, name, f) in rust_children {
            self.exports[pf.idx()].entry(name).or_insert(ExportEntry::Module(f));
        }
    }

    /// Prototype members by their ES5 constructor, the function of that name at the top of the
    /// member's file.
    fn build_prototypes(&mut self, files: &[FileId]) {
        let view = self.view;
        for &f in files {
            if view.file(f).lang().family() != LangFamily::Js {
                continue;
            }
            for b in view.bindings_of(f) {
                if b.kind != BindKind::MemberFn as u8 || b.node.0 == cg_core::NONE || b.decl_type.0 == cg_core::NONE {
                    continue;
                }
                let owner = view.text_str(b.decl_type);
                if owner.is_empty() {
                    continue;
                }
                let ctor = self.top_level_nodes(f).find(|n| view.node(*n).name() == owner && view.node(*n).kind() == NodeKind::Function).or_else(|| {
                    self.top_level_nodes(f).find(|n| view.node(*n).name() == owner && matches!(view.node(*n).kind(), NodeKind::Variable | NodeKind::Constant))
                });
                let Some(ctor) = ctor.filter(|c| *c != b.node) else { continue };
                self.proto.entry(ctor).or_default().push((view.text_str(b.name), b.node));
                self.proto_owner.insert(b.node, ctor);
            }
        }
    }

    /// A function given to the prototype of ES5 constructor `t` under `name`.
    pub fn proto_member(&self, t: NodeIdx, name: &str) -> Option<NodeIdx> {
        self.proto.get(&t)?.iter().find(|(n, _)| *n == name).map(|(_, m)| *m)
    }

    fn build_pkg_defs(&mut self, files: &[FileId]) {
        let view = self.view;
        let mut defs: Vec<FxHashMap<&'v str, SmallVec<[NodeIdx; 2]>>> = (0..self.modules.pkgs.len()).map(|_| FxHashMap::default()).collect();
        for &f in files {
            let p = self.modules.pkg_of(f);
            if p.is_none() {
                continue;
            }
            let namespaced = matches!(view.file(f).lang().family(), LangFamily::CSharp | LangFamily::Php);
            let jvm = view.file(f).lang().family() == LangFamily::Jvm;
            for n in self.top_level_nodes(f) {
                let node = view.node(n);
                // Go methods are not package-level names; Java/Kotlin nest members under classes
                if node.kind() == NodeKind::Method {
                    continue;
                }
                // a Java / Kotlin file's declarations sit inside its `package` namespace node
                if jvm && node.kind() == NodeKind::Namespace {
                    for e in view.out_edges(n).filter(|e| e.kind == EdgeKind::Contains) {
                        let child = view.node(e.dst);
                        if child.kind().is_definition() && child.kind() != NodeKind::Method {
                            defs[p.0 as usize].entry(child.name()).or_default().push(e.dst);
                        }
                    }
                    continue;
                }
                defs[p.0 as usize].entry(node.name()).or_default().push(n);
                // C# / PHP parent every declaration under a `namespace` node: the
                // namespace's children are the package-level names
                if namespaced && node.kind() == NodeKind::Namespace {
                    self.namespace_members(n, 0, &mut |m| defs[p.0 as usize].entry(view.node(m).name()).or_default().push(m));
                }
            }
        }
        self.pkg_defs = defs;
    }

    /// Whether `from` reaches `to` through resolved `#include`s, breadth-first over at most
    /// four levels and 256 files (headers usually reach the declaring header in one or two).
    pub(crate) fn includes_transitively(&self, from: FileId, to: FileId) -> bool {
        let mut frontier = vec![from];
        let mut seen = rustc_hash::FxHashSet::default();
        seen.insert(from);
        for _ in 0..4 {
            let mut next = Vec::new();
            for f in frontier {
                for t in self.imp_targets.get(f.idx()).map(|v| v.as_slice()).unwrap_or_default() {
                    if let ImportTarget::File(tf) = *t {
                        if tf == to {
                            return true;
                        }
                        if seen.len() < 256 && seen.insert(tf) {
                            next.push(tf);
                        }
                    }
                }
            }
            if next.is_empty() {
                return false;
            }
            frontier = next;
        }
        false
    }

    /// Declarations directly inside namespace node `n` (and its nested namespaces).
    fn namespace_members(&self, n: NodeIdx, depth: u8, out: &mut dyn FnMut(NodeIdx)) {
        if depth > 8 {
            return;
        }
        for e in self.view.out_edges(n) {
            if e.kind != EdgeKind::Contains {
                continue;
            }
            let k = self.view.node(e.dst).kind();
            if k == NodeKind::Namespace {
                self.namespace_members(e.dst, depth + 1, out);
            } else if k != NodeKind::File && k != NodeKind::Method {
                out(e.dst);
            }
        }
    }

    /// Module file a re-export (`export ... from 'x'`) points to, or NONE.
    pub fn export_target(&self, f: FileId, ex: &cg_store::ExportRec) -> u32 {
        if ex.from_spec.0 == NONE || self.view.text_str(ex.name) == cg_extract::facts::js::AMBIENT_MODULE {
            return NONE;
        }
        let spec = self.view.text_str(ex.from_spec);
        if spec.is_empty() {
            return NONE;
        }
        match self.modules.resolve_import(self.view.file(f).lang(), spec, f) {
            crate::modules::ImportTarget::File(t) => t.0,
            _ => NONE,
        }
    }

    // ---------------------------------------------------------------- lookups
    pub fn lookup_export(&self, f: FileId, name: &str, depth: u8) -> Option<Bound> {
        self.lookup_export_in_space(f, name, depth, false)
    }

    pub fn lookup_type_export(&self, f: FileId, name: &str, depth: u8) -> Option<Bound> {
        self.lookup_export_in_space(f, name, depth, true)
    }

    fn lookup_export_in_space(&self, f: FileId, name: &str, depth: u8, type_space: bool) -> Option<Bound> {
        if depth > 8 || f.idx() >= self.exports.len() {
            return None;
        }
        if type_space {
            if let Some(ExportEntry::Node(n)) = self.type_exports[f.idx()].get(name) {
                return Some(Bound::Node(*n));
            }
        }
        if let Some(e) = self.exports[f.idx()].get(name) {
            match *e {
                ExportEntry::Node(n) => return Some(Bound::Node(n)),
                ExportEntry::Module(m) => return Some(Bound::Module(m)),
                ExportEntry::Re(tf, from) => {
                    if let Some(b) = self.lookup_export_in_space(tf, self.view.text_str(from), depth + 1, type_space) {
                        return Some(b);
                    }
                }
            }
        }
        if name != "default" {
            for &tf in &self.stars[f.idx()] {
                if let Some(b) = self.lookup_export_in_space(tf, name, depth + 1, type_space) {
                    return Some(b);
                }
            }
        }
        None
    }

    /// Any top-level definition called `name` in the file, exported or not (CommonJS).
    pub fn lookup_top_level(&self, f: FileId, name: &str) -> Option<NodeIdx> {
        self.top_level_nodes(f).find(|n| self.view.node(*n).name() == name)
    }

    /// Python / Rust: sibling submodule `name` of the package or module file `f`.
    pub fn submodule(&self, f: FileId, name: &str) -> Option<FileId> {
        let path = self.view.file(f).path();
        match self.view.file(f).lang() {
            Lang::Python => {
                let dir = crate::project::parent_dir(path);
                for cand in [format!("{dir}/{name}.py"), format!("{dir}/{name}/__init__.py")] {
                    if let Some(sf) = self.modules.file_by_path(cand.trim_start_matches('/')) {
                        return Some(sf);
                    }
                }
                None
            }
            Lang::Rust => match self.exports.get(f.idx())?.get(name)? {
                ExportEntry::Module(m) => Some(*m),
                _ => None,
            },
            _ => None,
        }
    }

    /// Bind the name introduced by an import record.
    pub fn bind_import(&self, f: FileId, idx: usize) -> Bound {
        self.bind_import_in_space(f, idx, false)
    }

    pub fn bind_type_import(&self, f: FileId, idx: usize) -> Bound {
        self.bind_import_in_space(f, idx, true)
    }

    fn bind_import_in_space(&self, f: FileId, idx: usize, type_space: bool) -> Bound {
        let imp = &self.view.imports_of(f)[idx];
        let tgt = self.imp_targets[f.idx()].get(idx).copied().unwrap_or(ImportTarget::External);
        let imported = self.view.text_str(imp.imported);
        match tgt {
            ImportTarget::External | ImportTarget::NotFound => Bound::External,
            ImportTarget::Node(n) => Bound::Node(n),
            ImportTarget::Pkg(p) => Bound::Pkg(p),
            ImportTarget::File(tf) => {
                if imp.kind == ImportKind::Default as u8 && imported == "default" {
                    if let Some(value) = self.lookup_export(tf, COMMONJS_MODULE, 0) {
                        return value;
                    }
                }
                if imp.kind == ImportKind::Namespace as u8 || imported == "*" {
                    return Bound::Module(tf);
                }
                if imp.kind == ImportKind::Require as u8 {
                    if let Some(b) = self.lookup_export(tf, imported, 0) {
                        return b;
                    }
                    return Bound::Unknown;
                }
                if let Some(b) = self.lookup_export_in_space(tf, imported, 0, type_space) {
                    return b;
                }
                if let Some(sm) = self.submodule(tf, imported) {
                    return Bound::Module(sm);
                }
                if self.view.file(f).lang() == Lang::Rust {
                    let path = self.view.file(tf).path();
                    if path.ends_with(&format!("/{imported}.rs")) || path.ends_with(&format!("/{imported}/mod.rs")) || path == format!("{imported}.rs") {
                        return Bound::Module(tf);
                    }
                }
                // Python `import mod` binds the module; `from pkg import name` may name a submodule
                Bound::Unknown
            }
        }
    }

    // --------------------------------------------------------------- hierarchy
    pub fn direct_supers(&self, n: NodeIdx) -> &[NodeIdx] {
        self.supers.get(&n).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Breadth-first ancestors of a type (excluding itself), cycle-safe, depth-limited; a
    /// Python class's in its method resolution order.
    pub fn ancestors(&self, n: NodeIdx) -> Vec<NodeIdx> {
        if let Some(mro) = self.mro.get(&n) {
            return mro.clone();
        }
        let mut out = Vec::new();
        let mut frontier = vec![n];
        let mut seen = rustc_hash::FxHashSet::default();
        seen.insert(n);
        for _ in 0..10 {
            let mut next = Vec::new();
            for t in &frontier {
                for s in self.direct_supers(*t) {
                    if seen.insert(*s) {
                        out.push(*s);
                        next.push(*s);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }

    /// Install the hierarchy from resolved `extends` / `implements` outcomes, each with the
    /// (line, column) it is written at: a class's bases are in the order written.
    pub fn set_hierarchy(&mut self, edges: impl Iterator<Item = (NodeIdx, NodeIdx, (u32, u32))>) {
        let mut written: Vec<_> = edges.filter(|(sub, sup, _)| sub != sup).map(|(sub, sup, pos)| (sub, pos, sup)).collect();
        written.sort();
        for (sub, _, sup) in written {
            let v = self.supers.entry(sub).or_default();
            if !v.contains(&sup) {
                v.push(sup);
            }
        }
        let view = self.view;
        let python: Vec<NodeIdx> = self.supers.keys().copied().filter(|n| view.file(view.hot(*n).file).lang() == Lang::Python).collect();
        let mut memo = FxHashMap::default();
        for t in python {
            if let Some(mro) = self.c3(t, &mut memo, 0) {
                self.mro.insert(t, mro[1..].to_vec());
            }
        }
    }

    /// Python's C3 linearization of `t`, `t` first. `None` for a cycle or for bases no order
    /// satisfies (Python refuses such a class); breadth-first lookup stands in then.
    fn c3(&self, t: NodeIdx, memo: &mut FxHashMap<NodeIdx, Option<Vec<NodeIdx>>>, depth: u8) -> Option<Vec<NodeIdx>> {
        if let Some(m) = memo.get(&t) {
            return m.clone();
        }
        if depth > 16 {
            return None;
        }
        memo.insert(t, None);
        let bases = self.direct_supers(t).to_vec();
        let mut seqs = Vec::with_capacity(bases.len() + 1);
        for b in &bases {
            seqs.push(self.c3(*b, memo, depth + 1)?);
        }
        seqs.push(bases);
        let mut out = vec![t];
        loop {
            seqs.retain(|s| !s.is_empty());
            if seqs.is_empty() {
                break;
            }
            let head = seqs.iter().map(|s| s[0]).find(|h| !seqs.iter().any(|s| s[1..].contains(h)))?;
            out.push(head);
            for s in &mut seqs {
                if s[0] == head {
                    s.remove(0);
                }
            }
        }
        memo.insert(t, Some(out.clone()));
        Some(out)
    }

    /// Hierarchy from refs already resolved in the view (incremental runs).
    pub fn hierarchy_from_view(&mut self) {
        let mut pairs = Vec::new();
        for f in self.view.files() {
            for r in self.view.refs_of_file(f) {
                if r.status == status::RESOLVED
                    && matches!(cg_core::RefKind::from_wire(r.kind), Some(cg_core::RefKind::Edge(EdgeKind::Extends | EdgeKind::Implements)))
                {
                    pairs.push((r.from, r.target, (r.line, r.col)));
                }
            }
        }
        self.set_hierarchy(pairs.into_iter());
    }

    // ------------------------------------------------------------------ members
    /// Member `name` declared directly by type `t` (contained child, or `Type::name` by
    /// qualified name for methods declared outside the type body: Go receivers, Rust impls).
    pub fn direct_member(&self, t: NodeIdx, name: &str) -> Option<NodeIdx> {
        let tn = self.view.node(t);
        // Rust: an inherent method shadows a trait's, wherever either impl is written
        let mut of_trait = None;
        let mut php_ci = None;
        let is_php = self.view.file(tn.file()).lang().family() == LangFamily::Php;
        for e in self.view.out_edges(t) {
            if e.kind == EdgeKind::Contains {
                let c = self.view.node(e.dst);
                if c.name() == name && c.kind() != NodeKind::File {
                    if !self.trait_methods.contains(&e.dst) {
                        return Some(e.dst);
                    }
                    of_trait.get_or_insert(e.dst);
                } else if is_php && php_ci.is_none() && c.kind().is_callable() && c.name().eq_ignore_ascii_case(name) {
                    php_ci = Some(e.dst);
                }
            }
        }
        if let Some(m) = php_ci {
            return Some(m);
        }
        let q = format!("{}::{}", tn.qname(), name);
        let tfile = tn.file();
        let tpkg = self.modules.pkg_of(tfile);
        let family = self.view.file(tfile).lang().family();
        let mut best = None;
        let mut cross: SmallVec<[NodeIdx; 4]> = SmallVec::new();
        let mut owned: SmallVec<[NodeIdx; 2]> = SmallVec::new();
        for cand in self.view.by_qname(&q) {
            let cf = self.view.hot(cand).file;
            // Go: same package; Rust: the owning module or a proved type import.
            if cf == tfile || (!tpkg.is_none() && self.modules.pkg_of(cf) == tpkg) || self.rust_type_owner(t, cf) {
                if family != LangFamily::Rust {
                    return Some(cand);
                }
                owned.push(cand);
                continue;
            }
            best.get_or_insert(cand);
            cross.push(cand);
        }
        // Rust impls of one type in several files: inherent first, then by path and line, so
        // a base/delta split cannot decide
        let by_place = owned.into_iter().min_by_key(|c| {
            let h = self.view.hot(*c);
            (self.trait_methods.contains(c), self.view.file(h.file).path(), self.view.node(*c).start_line())
        });
        if let Some(m) = by_place.or(of_trait) {
            return Some(m);
        }
        match family {
            // Java/Kotlin nested types (pre-existing behaviour), then a Kotlin extension function
            LangFamily::Jvm => best.filter(|_| tn.qname().contains("::")).or_else(|| self.kotlin_extension(t, name)),
            // Swift extensions (emitted as class nodes) add members in other files and other
            // modules (a test target extending the library's type). Pick deterministically:
            // posting order (base before delta) must never decide.
            LangFamily::Swift => cross.into_iter().filter(|c| self.view.file(self.view.hot(*c).file).lang() == Lang::Swift).min_by_key(|c| {
                let h = self.view.hot(*c);
                (self.view.file(h.file).path(), self.view.node(*c).start_line())
            }),
            // C# partial classes, Ruby reopened classes and C++ out-of-line definitions
            // declare members in other files. Chosen by path and line, so a base/delta split
            // cannot change the answer.
            fam @ (LangFamily::CSharp | LangFamily::Ruby | LangFamily::C) => {
                cross.into_iter().filter(|c| self.cross_file_member_ok(t, *c, fam)).min_by_key(|c| {
                    let h = self.view.hot(*c);
                    (self.view.file(h.file).path(), self.view.node(*c).start_line())
                })
            }
            _ => None,
        }
    }

    /// Kotlin `fun Lockable.awaitNanos()` (top-level or member extension inside another class):
    /// the kernel names it `Lockable::awaitNanos`, without the package. One in `t`'s package
    /// first, else the only one.
    fn kotlin_extension(&self, t: NodeIdx, name: &str) -> Option<NodeIdx> {
        let view = self.view;
        let tn = view.node(t);
        let mut cands: SmallVec<[NodeIdx; 4]> = view
            .by_qname(&format!("{}::{}", tn.name(), name))
            .filter(|&c| {
                view.file(view.hot(c).file).lang() == Lang::Kotlin
                    && view.node(c).parent().is_none_or(|p| !view.node(p).kind().is_type() || view.node(p).name() != tn.name())
            })
            .collect();
        cands.sort_by_key(|&c| (view.file(view.hot(c).file).path(), view.node(c).start_line()));
        let tpkg = self.modules.pkg_of(tn.file());
        match cands.as_slice() {
            [only] => Some(*only),
            _ => cands.iter().copied().find(|&c| self.modules.pkg_of(view.hot(c).file) == tpkg),
        }
    }

    /// Whether a member with type `t`'s qualified name found in another file belongs to `t`:
    /// C# / Ruby when that file re-declares the type (partial class, reopened class or
    /// module); C/C++ when the member's file includes the type's file or vice versa.
    fn cross_file_member_ok(&self, t: NodeIdx, cand: NodeIdx, fam: LangFamily) -> bool {
        let view = self.view;
        let tn = view.node(t);
        let tfile = tn.file();
        let cf = view.hot(cand).file;
        if view.file(cf).lang().family() != fam {
            return false;
        }
        // C++: an out-of-line definition is owned by its file or namespace; a candidate inside
        // another class body belongs to that (same-named) class
        if fam == LangFamily::C && view.node(cand).parent().is_some_and(|p| p != t && view.node(p).kind().is_type()) {
            return false;
        }
        match fam {
            LangFamily::CSharp | LangFamily::Ruby => view.by_qname(tn.qname()).any(|x| {
                let k = view.node(x).kind();
                view.hot(x).file == cf && (k.is_type() || k == NodeKind::Module)
            }),
            LangFamily::C => {
                // the type's simple name is unique among C/C++ types: no other type to confuse
                let unique =
                    view.by_name(tn.name()).filter(|x| view.node(*x).kind().is_type() && view.file(view.hot(*x).file).lang().family() == fam).take(2).count()
                        == 1;
                unique || self.includes_transitively(cf, tfile) || self.includes_transitively(tfile, cf)
            }
            _ => false,
        }
    }

    /// Equal unqualified type names in different Rust modules are distinct.
    /// An out-of-module impl needs an import that identifies this exact type.
    pub fn rust_type_owner(&self, t: NodeIdx, file: FileId) -> bool {
        let node = self.view.node(t);
        match (self.modules.rust_module_of(node.file()), self.modules.rust_module_of(file)) {
            (Some(a), Some(b)) if a == b => return true,
            (Some((a, _)), Some((b, _))) if a == b => {}
            _ => return false,
        }
        self.view
            .imports_of(file)
            .iter()
            .enumerate()
            .any(|(i, import)| self.view.text_str(import.local) == node.qname() && self.bind_import(file, i) == Bound::Node(t))
    }

    /// Member lookup through the hierarchy.
    pub fn find_member(&self, t: NodeIdx, name: &str) -> Option<NodeIdx> {
        if let Some(m) = self.direct_member(t, name).or_else(|| self.proto_member(t, name)) {
            return Some(m);
        }
        for a in self.ancestors(t) {
            if let Some(m) = self.direct_member(a, name).or_else(|| self.proto_member(a, name)) {
                return Some(m);
            }
        }
        None
    }

    /// Every callable member called `name` declared in the body of `t` or of one of its
    /// ancestors, nearest type first and in declaration order: the overload set a call picks
    /// from. Members declared outside a body (Go receivers, Rust impls) have no overloads.
    pub fn member_overloads(&self, t: NodeIdx, name: &str) -> SmallVec<[NodeIdx; 4]> {
        let mut out = SmallVec::new();
        for ty in std::iter::once(t).chain(self.ancestors(t)) {
            for e in self.view.out_edges(ty) {
                if e.kind == EdgeKind::Contains {
                    let c = self.view.node(e.dst);
                    if c.name() == name && c.kind().is_callable() && !out.contains(&e.dst) {
                        out.push(e.dst);
                    }
                }
            }
            if out.len() > 32 {
                break;
            }
        }
        out
    }

    /// Go: promoted members through embedded structs are covered by `ancestors`
    /// (embedding is recorded as `extends`).
    pub fn node_is_type(&self, n: NodeIdx) -> bool {
        self.view.node(n).kind().is_type()
    }
}

#[allow(dead_code)]
fn _unused() -> (BindKind, u8) {
    (BindKind::Local, outkind::CLASS_RESOLVE)
}
