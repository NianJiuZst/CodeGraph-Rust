//! Reference resolution (design doc 6.3-6.5): the strategy order is
//! framework rules, lexical scope, import binding, receiver type, qualified name,
//! same package / module, global name match. A reference is resolved by the first strategy that
//! gives an answer, and "better unresolved than wrong": ambiguity produces no edge.

use crate::builtins::{is_builtin, is_common_method};
use crate::engine::{Bound, Engine};
use crate::modules::{ImportTarget, PkgId};
use crate::overload::{self, ArgTy, CallArgs};
use crate::shape::{normalize_call_name, parse_shape, split_head, Shape};
use cg_core::{text::pack_pos, EdgeKind, FileId, Lang, LangFamily, NodeIdx, NodeKind, RefKind, Visibility, NONE};
use cg_extract::facts::{COMMONJS_MODULE, JS_BINDING_ALIAS, JS_SATISFIES_VALUE, SWIFT_CHAIN};
use cg_extract::{BindKind, ImportKind, ScopeKind};
use cg_store::{status, BindingRec, ImportRec, RefRec, ScopeRec, View};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::rc::Rc;

/// Which strategy produced an edge (stored in `out_res`, design doc 5.3).
pub mod by {
    pub const FRAMEWORK: u8 = 1;
    pub const LEXICAL: u8 = 2;
    pub const IMPORT: u8 = 3;
    pub const RECEIVER: u8 = 4;
    pub const QUALIFIED: u8 = 5;
    pub const PACKAGE: u8 = 6;
    pub const GLOBAL: u8 = 7;
    pub const EXTRACT: u8 = 8;

    pub fn name(b: u8) -> &'static str {
        match b {
            FRAMEWORK => "framework",
            LEXICAL => "scope",
            IMPORT => "import",
            RECEIVER => "receiver-type",
            QUALIFIED => "qualified-name",
            PACKAGE => "package",
            GLOBAL => "global-name",
            EXTRACT => "extraction",
            _ => "",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub status: u8,
    pub by: u8,
    /// Target node, or the candidate count for `Ambiguous`.
    pub target: u32,
    /// Confidence 0-100.
    pub conf: u8,
}

impl Outcome {
    pub const PENDING: Outcome = Outcome { status: status::PENDING, by: 0, target: NONE, conf: 0 };
    pub fn found(n: NodeIdx, by: u8, conf: u8) -> Outcome {
        Outcome { status: status::RESOLVED, by, target: n.0, conf }
    }
    pub fn st(s: u8) -> Outcome {
        Outcome { status: s, by: 0, target: NONE, conf: 0 }
    }
    pub fn ambiguous(k: u32) -> Outcome {
        Outcome { status: status::AMBIGUOUS, by: 0, target: k, conf: 0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Accept {
    Type,
    Callable,
    Any,
}

impl Accept {
    fn ok(self, k: NodeKind) -> bool {
        match self {
            Accept::Type => k.is_type() || matches!(k, NodeKind::Namespace | NodeKind::Module),
            Accept::Callable => k.is_callable() || k.is_type() || matches!(k, NodeKind::Variable | NodeKind::Constant | NodeKind::Property | NodeKind::Field),
            Accept::Any => k.is_definition(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameRes {
    Found(NodeIdx, u8, u8),
    Module(FileId),
    Pkg(PkgId),
    External,
    Local,
    Builtin,
    Ambiguous(u32),
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeRes {
    Node(NodeIdx),
    External,
    Builtin,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StdWrapper {
    Result,
    Option,
    Pointer,
    /// `Mutex<T>` / `RwLock<T>` (std, tokio, parking_lot): `lock()` gives a guard.
    Lock,
    /// A lock's guard: it dereferences to the value, and std's `unwrap` keeps it.
    Guard,
}

/// The methods of a lock that hand out a guard.
fn lock_method(method: &str) -> bool {
    matches!(
        method,
        "lock"
            | "read"
            | "write"
            | "try_lock"
            | "try_read"
            | "try_write"
            | "blocking_lock"
            | "blocking_read"
            | "blocking_write"
            | "lock_owned"
            | "read_owned"
            | "write_owned"
    )
}

/// A project type, possibly still inside a standard Rust container, or an
/// explicit return annotation whose receiver we cannot model.
/// Keep the container until an operation such as `unwrap` actually removes it.
struct ValueType {
    node: Option<NodeIdx>,
    wrappers: SmallVec<[StdWrapper; 2]>,
}

impl ValueType {
    fn plain(node: NodeIdx) -> Self {
        Self { node: Some(node), wrappers: SmallVec::new() }
    }

    fn unknown() -> Self {
        Self { node: None, wrappers: SmallVec::new() }
    }

    fn receiver(self) -> Option<NodeIdx> {
        // The field cache stores one node, so it cannot preserve wrapper
        // methods such as Arc::clone. Only method_receiver may autoderef.
        self.wrappers.is_empty().then_some(self.node).flatten()
    }

    fn method_receiver(&self, method: &str) -> Option<NodeIdx> {
        if self.wrappers.contains(&StdWrapper::Pointer) && pointer_method(method) {
            return None;
        }
        self.wrappers.iter().all(|w| matches!(w, StdWrapper::Pointer | StdWrapper::Guard)).then_some(self.node).flatten()
    }
}

fn pointer_method(method: &str) -> bool {
    matches!(
        method,
        "clone"
            | "clone_from"
            | "as_ref"
            | "as_mut"
            | "deref"
            | "deref_mut"
            | "get_mut"
            | "make_mut"
            | "into_inner"
            | "try_unwrap"
            | "downgrade"
            | "strong_count"
            | "weak_count"
            | "ptr_eq"
    )
}

/// `A<B, C>, D` -> [`A<B, C>`, `D`]: the items of a written type list, nested lists whole.
fn type_list(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0u32, 0);
    for (i, c) in s.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out.retain(|t| !t.is_empty());
    out
}

/// Paths here contain no source arguments except bounded constructor paths.
/// Find the final complete call, allowing earlier calls in its receiver.
fn split_call(value: &str) -> Option<(&str, &str)> {
    let (mut depth, mut open) = (0u32, 0usize);
    for (i, byte) in value.bytes().enumerate() {
        match byte {
            b'(' => {
                if depth == 0 {
                    open = i;
                }
                depth += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && i + 1 == value.len() {
                    return Some((&value[..open], &value[open + 1..i]));
                }
            }
            _ => {}
        }
    }
    None
}

/// The name a binding's initializer hands on (upstream `aliasTargetName`), read from the
/// declaration's signature: `= realImpl` (a cast allowed), or with `member` the value of that
/// key in an object literal (`= { run: impl }`, shorthand `= { impl }`).
fn alias_target_name<'s>(sig: &'s str, member: Option<&str>) -> Option<&'s str> {
    let init = sig.trim().strip_prefix('=')?.trim().trim_end_matches(';').trim();
    let is_ident =
        |s: &str| s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$') && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    let Some(key) = member else {
        let name = init.split_once(" as ").map_or(init, |(a, _)| a).trim();
        return is_ident(name).then_some(name);
    };
    let body = init.strip_prefix('{')?;
    let (mut depth, mut start) = (0i32, 0usize);
    let mut entries = Vec::new();
    for (i, c) in body.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' => depth -= 1,
            '}' if depth == 0 => {
                entries.push(&body[start..i]);
                start = body.len();
                break;
            }
            '}' => depth -= 1,
            ',' if depth == 0 => {
                entries.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    entries.extend((start < body.len()).then(|| &body[start..]));
    for entry in entries {
        let entry = entry.trim();
        match entry.split_once(':') {
            Some((k, v)) if k.trim().trim_matches(['\'', '"']) == key => {
                let v = v.trim();
                return is_ident(v).then_some(v);
            }
            None if entry == key && is_ident(entry) => return Some(entry),
            _ => {}
        }
    }
    None
}

/// Per-file lookup structures, built once per file and reused for all its references.
pub struct FileCx<'e, 'v> {
    pub e: &'e Engine<'v>,
    pub view: &'v View,
    pub file: FileId,
    pub lang: Lang,
    pub family: LangFamily,
    pub path: &'v str,
    pub pkg: PkgId,
    pub is_test: bool,
    scopes: &'v [ScopeRec],
    scope_base: u32,
    bindings: FxHashMap<(u32, &'v str), SmallVec<[&'v BindingRec; 2]>>,
    pub imports: &'v [ImportRec],
    import_by_local: FxHashMap<&'v str, SmallVec<[u32; 2]>>,
    imports_by_line: FxHashMap<u32, SmallVec<[u32; 4]>>,
    wildcards: SmallVec<[u32; 2]>,
    imported_files: FxHashSet<FileId>,
    top_dir: &'v str,
}

/// Fields of a class: name and declared type (when it resolves).
type FieldList = Vec<(Box<str>, Option<NodeIdx>)>;

pub struct WorkerCx<'e, 'v> {
    cache: Vec<(FileId, Rc<FileCx<'e, 'v>>, usize)>,
    cache_bytes: usize,
    field_cache: FxHashMap<NodeIdx, FieldList>,
    field_cache_bytes: usize,
    /// Rust: the type `Self` / `self` stand for in the call being resolved.
    self_ty: Option<NodeIdx>,
}

impl<'e, 'v> WorkerCx<'e, 'v> {
    pub fn new() -> Self {
        WorkerCx { cache: Vec::new(), cache_bytes: 0, field_cache: FxHashMap::default(), field_cache_bytes: 0, self_ty: None }
    }

    pub fn file_cx(&mut self, e: &'e Engine<'v>, f: FileId) -> Rc<FileCx<'e, 'v>> {
        if let Some(pos) = self.cache.iter().position(|(id, _, _)| *id == f) {
            let item = self.cache.remove(pos);
            let rc = item.1.clone();
            self.cache.push(item);
            return rc;
        }
        let rc = Rc::new(FileCx::new(e, f));
        let bytes = rc.heap_bytes();
        let limit = e.budget.worker_cache_bytes();
        if bytes > limit {
            return rc;
        }
        while self.cache.len() >= 6 || self.cache_bytes.saturating_add(bytes) > limit {
            let (_, _, evicted) = self.cache.remove(0);
            self.cache_bytes -= evicted;
        }
        self.cache.push((f, rc.clone(), bytes));
        self.cache_bytes += bytes;
        rc
    }

    fn cache_field(&mut self, t: NodeIdx, name: &str, value: Option<NodeIdx>, limit: usize) {
        if name.len().saturating_add(256) > limit {
            return;
        }
        if self.field_cache_bytes.saturating_add(name.len() + 256) > limit {
            self.field_cache = FxHashMap::default();
            self.field_cache_bytes = 0;
        }
        let old_map_capacity = self.field_cache.capacity();
        let list = self.field_cache.entry(t).or_default();
        let old_capacity = list.capacity();
        list.push((name.into(), value));
        self.field_cache_bytes += name.len()
            + (list.capacity() - old_capacity) * std::mem::size_of::<(Box<str>, Option<NodeIdx>)>()
            + (self.field_cache.capacity() - old_map_capacity) * (std::mem::size_of::<(NodeIdx, FieldList)>() + 8);
        if self.field_cache_bytes > limit {
            self.field_cache = FxHashMap::default();
            self.field_cache_bytes = 0;
        }
    }
}

impl<'e, 'v> Default for WorkerCx<'e, 'v> {
    fn default() -> Self {
        Self::new()
    }
}

fn kind_of(view: &View, n: NodeIdx) -> NodeKind {
    NodeKind::from_u8(view.hot(n).kind).unwrap_or(NodeKind::Variable)
}

impl<'e, 'v> FileCx<'e, 'v> {
    fn heap_bytes(&self) -> usize {
        fn table<K, V>(map: &FxHashMap<K, V>) -> usize {
            map.capacity() * (std::mem::size_of::<(K, V)>() + 8)
        }
        let extra_bindings = self.bindings.values().filter(|v| v.spilled()).map(|v| v.capacity() * std::mem::size_of::<&BindingRec>()).sum::<usize>();
        let extra_imports = self.import_by_local.values().filter(|v| v.spilled()).map(|v| v.capacity() * 4).sum::<usize>()
            + self.imports_by_line.values().filter(|v| v.spilled()).map(|v| v.capacity() * 4).sum::<usize>();
        table(&self.bindings)
            + table(&self.import_by_local)
            + table(&self.imports_by_line)
            + extra_bindings
            + extra_imports
            + self.imported_files.capacity() * (std::mem::size_of::<FileId>() + 8)
            + if self.wildcards.spilled() { self.wildcards.capacity() * 4 } else { 0 }
    }
    pub fn new(e: &'e Engine<'v>, file: FileId) -> Self {
        let view = e.view;
        let fr = view.file(file);
        let path = fr.path();
        let lang = fr.lang();
        let scopes = view.scopes_of(file);
        let scope_base = view.scope_base(file);
        let mut bindings: FxHashMap<(u32, &'v str), SmallVec<[&'v BindingRec; 2]>> = FxHashMap::default();
        for b in view.bindings_of(file) {
            bindings.entry((b.scope.0.wrapping_sub(scope_base), view.text_str(b.name))).or_default().push(b);
        }
        let imports = view.imports_of(file);
        let mut import_by_local: FxHashMap<&'v str, SmallVec<[u32; 2]>> = FxHashMap::default();
        let mut imports_by_line: FxHashMap<u32, SmallVec<[u32; 4]>> = FxHashMap::default();
        let mut wildcards = SmallVec::new();
        let mut imported_files = FxHashSet::default();
        for (i, imp) in imports.iter().enumerate() {
            let local = view.text_str(imp.local);
            if local == "*" {
                wildcards.push(i as u32);
            } else if !local.is_empty() {
                import_by_local.entry(local).or_default().push(i as u32);
            }
            imports_by_line.entry(imp.line).or_default().push(i as u32);
            if let Some(ImportTarget::File(tf)) = e.imp_targets[file.idx()].get(i) {
                imported_files.insert(*tf);
            }
        }
        let top_dir = path.split('/').next().unwrap_or("");
        FileCx {
            e,
            view,
            file,
            lang,
            family: lang.family(),
            path,
            pkg: e.modules.pkg_of(file),
            is_test: fr.flags() & cg_extract::fileflags::TEST != 0,
            scopes,
            scope_base,
            bindings,
            imports,
            import_by_local,
            imports_by_line,
            wildcards,
            imported_files,
            top_dir,
        }
    }

    // ------------------------------------------------------------------ scopes
    pub fn scope_at(&self, line: u32, col: u32) -> u32 {
        if self.scopes.is_empty() {
            return 0;
        }
        let pos = pack_pos(line, col);
        let i = self.scopes.partition_point(|s| s.start <= pos);
        if i == 0 {
            return 0;
        }
        let mut cur = (i - 1) as u32;
        loop {
            let s = &self.scopes[cur as usize];
            if s.start <= pos && pos <= s.end {
                return cur;
            }
            if s.parent.0 == NONE {
                return 0;
            }
            cur = s.parent.0.wrapping_sub(self.scope_base);
            if cur as usize >= self.scopes.len() {
                return 0;
            }
        }
    }

    fn scope_parent(&self, s: u32) -> Option<u32> {
        let p = self.scopes.get(s as usize)?.parent.0;
        (p != NONE).then(|| p.wrapping_sub(self.scope_base))
    }

    fn scope_kind(&self, s: u32) -> u8 {
        self.scopes.get(s as usize).map_or(ScopeKind::File as u8, |x| x.kind)
    }

    fn scope_within(&self, mut inner: u32, outer: u32) -> bool {
        loop {
            if inner == outer {
                return true;
            }
            match self.scope_parent(inner) {
                Some(p) => inner = p,
                None => return outer == 0,
            }
        }
    }

    pub fn lookup_binding(&self, name: &str, scope: u32, line: u32, allow_class: bool) -> Option<&'v BindingRec> {
        let mut s = scope;
        loop {
            if allow_class || self.scope_kind(s) != ScopeKind::Class as u8 {
                if let Some(list) = self.bindings.get(&(s, name)) {
                    let mut best = None;
                    for b in list {
                        if b.kind == BindKind::Package as u8
                            || b.kind == BindKind::MemberFn as u8
                            || b.kind == BindKind::ReturnType as u8
                            || b.kind == BindKind::Deref as u8
                            || b.kind == BindKind::Fixture as u8
                            || b.kind == BindKind::TypeAlias as u8
                            || b.kind == BindKind::FactoryMixin as u8
                            || b.kind == BindKind::Mutation as u8
                            || b.kind == BindKind::CommonJsObject as u8
                            || b.kind == BindKind::AssignedThisCall as u8
                            || b.kind == BindKind::MacroNamespace as u8
                            || b.kind == BindKind::TypeParam as u8
                            || b.kind == BindKind::TraitImpl as u8
                        {
                            continue;
                        }
                        if b.line == 0 || b.line <= line {
                            best = Some(*b);
                        }
                    }
                    if best.is_some() {
                        return best;
                    }
                }
            }
            s = self.scope_parent(s)?;
        }
    }

    /// A type parameter `name` of an item enclosing `scope` (Rust `fn new<T>(value: T)`).
    fn type_param(&self, name: &str, scope: u32) -> bool {
        let mut s = Some(scope);
        while let Some(c) = s {
            if self.bindings.get(&(c, name)).is_some_and(|l| l.iter().any(|b| b.kind == BindKind::TypeParam as u8)) {
                return true;
            }
            s = self.scope_parent(c);
        }
        false
    }

    fn visible_import(&self, name: &str, scope: u32) -> Option<u32> {
        let list = self.import_by_local.get(name)?;
        list.iter().copied().find(|i| {
            let imp = &self.imports[*i as usize];
            imp.scope.0 == NONE || self.scope_within(scope, imp.scope.0.wrapping_sub(self.scope_base))
        })
    }

    // ----------------------------------------------------------- type context
    pub fn enclosing_type(&self, from: NodeIdx) -> Option<NodeIdx> {
        let view = self.view;
        let mut cur = from;
        for _ in 0..24 {
            let n = view.node(cur);
            if self.holds_members(n.kind()) {
                return Some(cur);
            }
            match n.parent() {
                Some(p) if kind_of(view, p) != NodeKind::File => cur = p,
                _ => break,
            }
        }
        // methods declared outside their type's body (Go receivers, Rust impls): `Type::method`
        let q = view.node(from).qname();
        let prefix = &q[..q.rfind("::")?];
        let mut best = None;
        for cand in view.by_qname(prefix) {
            if !self.holds_members(view.node(cand).kind()) {
                continue;
            }
            let cf = view.hot(cand).file;
            if cf == self.file {
                return Some(cand);
            }
            if self.family == LangFamily::Rust {
                if self.e.rust_type_owner(cand, self.file) {
                    return Some(cand);
                }
                continue;
            }
            // a candidate in the caller's package wins over earlier ones; otherwise the first stays
            if (self.pkg.0 != NONE && self.e.modules.pkg_of(cf) == self.pkg) || best.is_none() {
                best = Some(cand);
            }
        }
        best
    }

    /// Kinds whose members are reached through the type: every type, and Ruby modules
    /// (`module Helpers; def halt; end; end`).
    fn holds_members(&self, k: NodeKind) -> bool {
        k.is_type() || (self.family == LangFamily::Ruby && k == NodeKind::Module)
    }

    /// Node of the class scope that corresponds to type node `t` (same start position).
    fn class_scope_for(&self, t: NodeIdx) -> Option<u32> {
        let n = self.view.node(t);
        let pos = pack_pos(n.start_line(), n.start_col());
        let end = pack_pos(n.end_line(), n.end_col());
        let mut best: Option<(u32, u64)> = None;
        for (i, s) in self.scopes.iter().enumerate() {
            if s.kind != ScopeKind::Class as u8 {
                continue;
            }
            if s.start == pos {
                return Some(i as u32);
            }
            if s.start <= pos && end <= s.end {
                let size = s.end - s.start;
                if best.is_none_or(|(_, b)| size < b) {
                    best = Some((i as u32, size));
                }
            }
        }
        best.map(|b| b.0)
    }

    /// The fields type `t` declares in its body in this file, in declaration order.
    pub(crate) fn declared_fields(&self, t: NodeIdx) -> Vec<&'v BindingRec> {
        let Some(cs) = self.class_scope_for(t) else { return Vec::new() };
        let scope = self.scope_base.wrapping_add(cs);
        self.view.bindings_of(self.file).iter().filter(|b| b.scope.0 == scope && b.kind == BindKind::Field as u8).collect()
    }

    /// Binding of a file-level variable called `name` (module singletons such as `app = Flask()`).
    pub fn file_binding(&self, name: &str) -> Option<&'v BindingRec> {
        self.bindings
            .get(&(0, name))
            .and_then(|l| l.iter().copied().find(|b| b.kind == BindKind::Local as u8 || b.kind == BindKind::Field as u8 || b.kind == BindKind::Alias as u8))
    }

    /// Follow `segs` from a module / type: exports, top-level definitions, submodules, members.
    fn walk_path(&self, start: Bound, segs: &[&str]) -> Option<Bound> {
        self.walk_path_in_space(start, segs, false)
    }

    fn walk_path_in_space(&self, start: Bound, segs: &[&str], type_space: bool) -> Option<Bound> {
        let mut cur = start;
        for (i, seg) in segs.iter().enumerate() {
            cur = match cur {
                Bound::Module(m) => {
                    match if type_space && i + 1 == segs.len() { self.e.lookup_type_export(m, seg, 0) } else { self.e.lookup_export(m, seg, 0) } {
                        Some(b) => b,
                        None => match (self.view.file(m).lang().family() != LangFamily::Js).then(|| self.e.lookup_top_level(m, seg)).flatten() {
                            Some(n) => Bound::Node(n),
                            None => Bound::Module(self.e.submodule(m, seg)?),
                        },
                    }
                }
                Bound::Node(n) => Bound::Node(self.e.direct_member(n, seg).or_else(|| self.e.find_member(n, seg))?),
                Bound::Pkg(p) => {
                    let n = self.e.pkg_defs.get(p.0 as usize).and_then(|m| m.get(*seg)).and_then(|l| l.iter().find(|n| self.visible(**n, seg)))?;
                    Bound::Node(*n)
                }
                _ => return None,
            };
        }
        Some(cur)
    }

    // ------------------------------------------------------------ type lookup
    /// Type (or namespace) denoted by `name` in this file's context.
    pub fn lookup_type(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str) -> TypeRes {
        self.lookup_type_at(wcx, name, u32::MAX)
    }

    /// [`Self::lookup_type`] as written on `line` (`u32::MAX`: anywhere), where a JS class
    /// declared in the enclosing function shadows the file's.
    fn lookup_type_at(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str, line: u32) -> TypeRes {
        let name = name.trim();
        if name.is_empty() {
            return TypeRes::Unknown;
        }
        if let Some((head, rest)) = split_head(name) {
            return match self.qualified_type(wcx, head, rest, name) {
                TypeRes::Unknown if self.family == LangFamily::C => self.cpp_scoped(name, true).map_or(TypeRes::Unknown, TypeRes::Node),
                t => t,
            };
        }
        match self.resolve_name(wcx, name, Accept::Type, 0, line, None, true) {
            NameRes::Found(n, ..) => TypeRes::Node(n),
            NameRes::External => TypeRes::External,
            NameRes::Builtin => TypeRes::Builtin,
            _ => TypeRes::Unknown,
        }
    }

    /// `a::b::T` as written in C++: a namespace is one node per block, and the enclosing
    /// namespaces (or a macro-opened outer one) can be left out of what is written or indexed.
    // ponytail: the shortest qualified name wins, not the one C++ lookup would find from here.
    fn cpp_scoped(&self, name: &str, only_types: bool) -> Option<NodeIdx> {
        let name = name.trim_start_matches("::");
        if name.starts_with("std::") {
            return None;
        }
        let view = self.view;
        let leaf = name.rsplit("::").next()?;
        let mut rest = name;
        loop {
            let tail = format!("::{rest}");
            let hit = view
                .by_name(leaf)
                .filter(|n| view.file(view.hot(*n).file).lang().family() == LangFamily::C)
                .filter(|n| kind_of(view, *n).is_type() || (!only_types && kind_of(view, *n).is_callable()))
                .filter(|n| {
                    let q = view.node(*n).qname();
                    q == rest || (rest.contains("::") && q.ends_with(&tail))
                })
                .min_by_key(|n| (view.node(*n).qname().len(), view.file(view.hot(*n).file).path(), view.node(*n).start_line()));
            if hit.is_some() {
                return hit;
            }
            rest = rest.split_once("::")?.1;
        }
    }

    /// C++: a type declared by the class around `from`, its bases or the classes around it.
    fn cpp_nested_type(&self, from: NodeIdx, name: &str) -> Option<NodeIdx> {
        if self.lang != Lang::Cpp {
            return None;
        }
        let view = self.view;
        let mut t = self.enclosing_type(from);
        for _ in 0..8 {
            let ty = t?;
            for owner in std::iter::once(ty).chain(self.e.ancestors(ty)) {
                if let Some(m) = self.e.direct_member(owner, name).filter(|m| kind_of(view, *m).is_type() && *m != ty) {
                    return Some(m);
                }
            }
            t = view.node(ty).parent().filter(|p| kind_of(view, *p).is_type());
        }
        None
    }

    /// What a C++ alias finally names, or the type itself.
    fn cpp_unalias(&self, wcx: &mut WorkerCx<'e, 'v>, mut t: NodeIdx) -> NodeIdx {
        for _ in 0..4 {
            if kind_of(self.view, t) != NodeKind::TypeAlias || self.view.file(self.view.hot(t).file).lang() != Lang::Cpp {
                break;
            }
            let Some(b) = self.e.type_aliases.get(&t) else { break };
            let cx = wcx.file_cx(self.e, self.view.hot(t).file);
            match cx.lookup_type(wcx, self.view.text_str(b.decl_type)) {
                TypeRes::Node(n) if n != t => t = n,
                _ => break,
            }
        }
        t
    }

    /// The constructors of a C++ class, in file and line order.
    fn cpp_constructors(&self, t: NodeIdx) -> SmallVec<[NodeIdx; 4]> {
        let view = self.view;
        let node = view.node(t);
        let q = format!("{}::{}", node.qname(), node.name());
        let mut out: SmallVec<[NodeIdx; 4]> = view.by_qname(&q).filter(|n| kind_of(view, *n) == NodeKind::Method).collect();
        out.sort_by_key(|n| (view.file(view.hot(*n).file).path(), view.node(*n).start_line()));
        out
    }

    /// `Foo x(1, 2);` reaches the index as a call of `Foo::Foo/2`.
    fn cpp_constructor_ref(&self, wcx: &mut WorkerCx<'e, 'v>, path: &str, arity: usize) -> Outcome {
        let Some((ty, _)) = path.rsplit_once("::") else { return Outcome::st(status::NO_CANDIDATE) };
        match self.lookup_type(wcx, ty) {
            TypeRes::Node(t) => {
                let t = self.cpp_unalias(wcx, t);
                let shapes = vec!["?"; arity];
                match self.cpp_overload(&self.cpp_constructors(t), &shapes) {
                    Some(c) => Outcome::found(c, by::QUALIFIED, 90),
                    None => Outcome::st(status::NO_CANDIDATE),
                }
            }
            TypeRes::External => Outcome::st(status::EXTERNAL),
            TypeRes::Builtin => Outcome::st(status::BUILTIN),
            TypeRes::Unknown if ty.starts_with("std::") => Outcome::st(status::BUILTIN),
            TypeRes::Unknown => Outcome::st(status::NO_CANDIDATE),
        }
    }

    fn qualified_type(&self, wcx: &mut WorkerCx<'e, 'v>, head: &str, rest: &str, full: &str) -> TypeRes {
        if self.family == LangFamily::Rust {
            if let Some(Bound::Node(n)) = self.rust_path_bound(full) {
                if kind_of(self.view, n).is_type() {
                    return TypeRes::Node(n);
                }
            }
        }
        let segs: Vec<&str> = rest.split(['.', ':']).filter(|s| !s.is_empty()).collect();
        match self.resolve_name(wcx, head, Accept::Any, 0, u32::MAX, None, false) {
            NameRes::Module(tf) => match self.walk_path_in_space(Bound::Module(tf), &segs, true) {
                Some(Bound::Node(n)) => TypeRes::Node(n),
                _ => TypeRes::Unknown,
            },
            NameRes::Pkg(p) => match self.walk_path_in_space(Bound::Pkg(p), &segs, true) {
                Some(Bound::Node(n)) => TypeRes::Node(n),
                _ => TypeRes::Unknown,
            },
            NameRes::Found(n, ..) => self.nested_type(wcx, n, rest),
            NameRes::External => TypeRes::External,
            _ => {
                // fully qualified Java / Kotlin / Rust name
                let q = full.replace('.', "::");
                if let Some(n) = self.view.by_qname(&q).find(|n| kind_of(self.view, *n).is_type()) {
                    return TypeRes::Node(n);
                }
                let segs: Vec<&str> = full.split(['.', ':']).filter(|s| !s.is_empty()).collect();
                for i in (1..segs.len()).rev() {
                    let q = format!("{}::{}", segs[..i].join("."), segs[i..].join("::"));
                    if let Some(n) = self.view.by_qname(&q).find(|n| kind_of(self.view, *n).is_type()) {
                        return TypeRes::Node(n);
                    }
                }
                TypeRes::Unknown
            }
        }
    }

    fn nested_type(&self, _wcx: &mut WorkerCx<'e, 'v>, mut n: NodeIdx, rest: &str) -> TypeRes {
        for seg in rest.split(['.', ':']).filter(|s| !s.is_empty()) {
            match self.e.direct_member(n, seg) {
                Some(m) if self.holds_members(self.view.node(m).kind()) => n = m,
                _ => return TypeRes::Unknown,
            }
        }
        TypeRes::Node(n)
    }

    fn rust_path_bound(&self, name: &str) -> Option<Bound> {
        // A module and a value can share a Rust name (`mod once; pub use
        // once::once`). Resolve the parent, then consult its exports before
        // treating the final segment as a module filename.
        let (prefix, last) = name.rsplit_once("::").unwrap_or((name, ""));
        let (target, tail) = self.e.modules.resolve_rust_path(prefix, self.file);
        let ImportTarget::File(file) = target else { return None };
        let mut segs: Vec<_> = prefix.split("::").filter(|s| !s.is_empty()).collect();
        let start = segs.len().checked_sub(tail)?;
        if !last.is_empty() {
            segs.push(last);
        }
        self.walk_path(Bound::Module(file), &segs[start..])
    }

    /// The type a binding holds, from its declared type or initialiser.
    pub fn binding_type(&self, wcx: &mut WorkerCx<'e, 'v>, b: &BindingRec) -> Option<NodeIdx> {
        self.binding_value(wcx, b, 0)?.receiver()
    }

    fn binding_value(&self, wcx: &mut WorkerCx<'e, 'v>, b: &BindingRec, depth: u8) -> Option<ValueType> {
        if depth >= 16 {
            return None;
        }
        let dt = self.view.text_str(b.decl_type);
        if !dt.is_empty() {
            return self.type_value(wcx, dt, None, b.line, depth + 1);
        }
        let init = self.view.text_str(b.init_ctor);
        if init.is_empty() && b.kind == BindKind::Param as u8 && self.family == LangFamily::Python {
            return self.fixture_value(wcx, self.view.text_str(b.name), depth + 1);
        }
        if init.is_empty() || init == "embedded" {
            return None;
        }
        let scope = b.scope.0.wrapping_sub(self.scope_base);
        if b.kind == BindKind::Alias as u8 {
            // `const s = service`: what the other name holds
            if let Some(other) = self.lookup_binding(init, scope, b.line, false).filter(|o| !std::ptr::eq(*o, b)) {
                return self.binding_value(wcx, other, depth + 1);
            }
            if self.visible_import(init, scope).is_some() {
                return self.imported_binding_value(wcx, init, scope, depth + 1);
            }
        }
        // The new variable is not in scope in its own initializer (`let rt = rt()`).
        let root = init.strip_prefix(JS_SATISFIES_VALUE).unwrap_or(init);
        let root = root.strip_prefix(JS_BINDING_ALIAS).unwrap_or(root);
        let shadows = root.split(['.', ':', '(']).next() == Some(self.view.text_str(b.name));
        let line = if shadows { b.line.saturating_sub(1) } else { b.line };
        self.expression_value(wcx, init, scope, line, depth + 1)
    }

    fn imported_binding_value(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str, scope: u32, depth: u8) -> Option<ValueType> {
        if depth >= 16 {
            return None;
        }
        let import = self.visible_import(name, scope)?;
        let Bound::Node(node) = self.e.bind_import(self.file, import as usize) else { return None };
        if self.view.node(node).kind().is_type() {
            return Some(ValueType::plain(node));
        }
        let owner = self.view.hot(node).file;
        let cx = wcx.file_cx(self.e, owner);
        let binding = cx.file_binding(self.view.node(node).name())?;
        cx.binding_value(wcx, binding, depth + 1)
    }

    /// Do not guess a member of an unresolved value when an alias leads back
    /// to an explicit type or a `satisfies` operand.
    fn blocks_member_fallback(&self, wcx: &mut WorkerCx<'e, 'v>, binding: &BindingRec, depth: u8) -> bool {
        if depth >= 16 {
            return true;
        }
        let init = self.view.text_str(binding.init_ctor);
        if !self.view.text_str(binding.decl_type).is_empty() || init.starts_with(JS_SATISFIES_VALUE) || init.starts_with(JS_BINDING_ALIAS) {
            return true;
        }
        if binding.kind != BindKind::Alias as u8 {
            return false;
        }
        let scope = binding.scope.0.wrapping_sub(self.scope_base);
        if let Some(other) = self.lookup_binding(init, scope, binding.line, false) {
            return std::ptr::eq(other, binding) || self.blocks_member_fallback(wcx, other, depth + 1);
        }
        let Some(import) = self.visible_import(init, scope) else { return false };
        let Bound::Node(node) = self.e.bind_import(self.file, import as usize) else { return true };
        let cx = wcx.file_cx(self.e, self.view.hot(node).file);
        match cx.file_binding(self.view.node(node).name()) {
            Some(other) => cx.blocks_member_fallback(wcx, other, depth + 1),
            None => true,
        }
    }

    fn expression_value(&self, wcx: &mut WorkerCx<'e, 'v>, init: &str, scope: u32, line: u32, depth: u8) -> Option<ValueType> {
        if depth >= 16 {
            return None;
        }
        if let Some(inner) = init.strip_prefix(JS_SATISFIES_VALUE) {
            return self.expression_value(wcx, inner, scope, line, depth + 1);
        }
        if let Some(name) = init.strip_prefix(JS_BINDING_ALIAS) {
            if let Some(binding) = self.lookup_binding(name, scope, line, false) {
                return self.binding_value(wcx, binding, depth + 1);
            }
            return self.imported_binding_value(wcx, name, scope, depth + 1);
        }
        // `A()|B()`: a factory returning different classes in different branches is typed by
        // the nearest class they all derive from, or not at all
        if self.family == LangFamily::Python && init.contains('|') {
            let mut types = Vec::new();
            for alt in init.split('|') {
                let v = self.expression_value(wcx, alt, scope, line, depth + 1)?;
                types.push(v.receiver()?);
            }
            return self.common_base(&types).map(ValueType::plain);
        }
        if let Some(inner) = init.strip_suffix('?') {
            let mut value = self.expression_value(wcx, inner, scope, line, depth + 1)?;
            if !matches!(value.wrappers.pop()?, StdWrapper::Result | StdWrapper::Option) {
                return None;
            }
            return Some(value);
        }
        if self.family == LangFamily::Rust {
            if let Some((call, index)) = init.rsplit_once('.').and_then(|(c, i)| Some((c, i.parse::<usize>().ok()?))).filter(|(c, _)| c.ends_with([')', '?'])) {
                return self.tuple_element(wcx, call, index, scope, line, depth + 1);
            }
            if let Some((callee, args)) = split_call(init) {
                if let Some(owner) = callee.strip_suffix("::new") {
                    if self.std_wrapper(wcx, owner) == Some(StdWrapper::Pointer) {
                        let mut value = self.expression_value(wcx, args, scope, line, depth + 1)?;
                        value.wrappers.push(StdWrapper::Pointer);
                        return Some(value);
                    }
                }
                return self.call_value(wcx, callee, scope, line, depth + 1);
            }
        }
        if let Some(call) = init.strip_suffix("()") {
            return self.call_value(wcx, call, scope, line, depth + 1);
        }
        if self.family == LangFamily::Rust {
            if let Some(other) = self.lookup_binding(init, scope, line, false) {
                return self.binding_value(wcx, other, depth + 1);
            }
        }
        self.type_value(wcx, init, None, line, depth + 1)
    }

    /// `let (tx, rx) = mpsc::channel()`: element `index` of the tuple a Rust function returns,
    /// through the `Result` or `Option` a `?` opens.
    fn tuple_element(&self, wcx: &mut WorkerCx<'e, 'v>, call: &str, index: usize, scope: u32, line: u32, depth: u8) -> Option<ValueType> {
        if depth >= 16 {
            return None;
        }
        let (call, tried) = match call.strip_suffix('?') {
            Some(c) => (c, true),
            None => (call, false),
        };
        let (callee, _) = split_call(call)?;
        let f = self.rust_callee(wcx, callee, scope, line)?;
        let node = self.view.node(f);
        let cx = wcx.file_cx(self.e, node.file());
        let mut ty = match self.e.return_types.get(&f) {
            Some(b) => self.view.text_str(b.decl_type),
            None => node.return_type()?,
        }
        .trim();
        if tried {
            let open = ty.find('<')?;
            if !matches!(cx.std_wrapper(wcx, ty[..open].trim()), Some(StdWrapper::Result | StdWrapper::Option)) {
                return None;
            }
            ty = *type_list(ty[open + 1..].strip_suffix('>')?).first()?;
        }
        let elems = type_list(ty.strip_prefix('(')?.strip_suffix(')')?);
        cx.type_value(wcx, elems.get(index)?, None, u32::MAX, depth + 1)
    }

    /// The function a Rust call path names: `helper`, `mpsc::channel`, `Pair::new`.
    fn rust_callee(&self, wcx: &mut WorkerCx<'e, 'v>, callee: &str, scope: u32, line: u32) -> Option<NodeIdx> {
        // a method's tuple (`stream.into_split()`) stays untyped: its receiver is not modelled here
        if callee.contains('.') {
            return None;
        }
        let Some((head, rest)) = callee.split_once("::") else {
            return match self.resolve_name(wcx, callee, Accept::Callable, scope, line, None, true) {
                NameRes::Found(n, ..) => Some(n),
                _ => None,
            };
        };
        if let Some(Bound::Node(n)) = self.rust_path_bound(callee) {
            return Some(n);
        }
        let start = match self.resolve_name(wcx, head, Accept::Any, scope, line, None, false) {
            NameRes::Found(n, ..) => Bound::Node(n),
            NameRes::Module(m) => Bound::Module(m),
            _ => return None,
        };
        match self.walk_path(start, &rest.split("::").collect::<Vec<_>>())? {
            Bound::Node(n) if kind_of(self.view, n).is_callable() => Some(n),
            _ => None,
        }
    }

    /// A pytest fixture's value (upstream `pythonFixtureReturnType`): in a test file or a
    /// `conftest.py`, a parameter named after a fixture declared in the same file or in a
    /// `conftest.py` of its directory or above holds what the fixture returns.
    fn fixture_value(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str, depth: u8) -> Option<ValueType> {
        if !(self.is_test || self.path.ends_with("conftest.py")) || matches!(name, "self" | "cls") {
            return None;
        }
        let dir = crate::project::parent_dir(self.path);
        let mut best: Option<(usize, FileId, &'v BindingRec)> = None;
        for &(f, b) in self.e.fixtures.get(name)?.iter() {
            let path = self.view.file(f).path();
            let fdir = crate::project::parent_dir(path);
            let visible = f == self.file || (path.ends_with("conftest.py") && (fdir.is_empty() || dir == fdir || dir.starts_with(&format!("{fdir}/"))));
            if !visible {
                continue;
            }
            // the nearest declaration wins: this file, then the deepest conftest
            let rank = if f == self.file { usize::MAX } else { fdir.len() };
            match best {
                Some((r, ..)) if r > rank => {}
                Some((r, ..)) if r == rank => return None,
                _ => best = Some((rank, f, b)),
            }
        }
        let (_, f, b) = best?;
        let cx = wcx.file_cx(self.e, f);
        let declared = self.view.text_str(b.decl_type);
        if !declared.is_empty() {
            return cx.type_value(wcx, declared, None, u32::MAX, depth + 1);
        }
        cx.expression_value(wcx, self.view.text_str(b.init_ctor), b.scope.0.wrapping_sub(cx.scope_base), u32::MAX, depth + 1)
    }

    /// The nearest type each of `types` is or derives from.
    fn common_base(&self, types: &[NodeIdx]) -> Option<NodeIdx> {
        let first = *types.first()?;
        let mut chain = vec![first];
        chain.extend(self.e.ancestors(first));
        chain.into_iter().find(|c| types.iter().all(|t| t == c || self.e.ancestors(*t).contains(c)))
    }

    /// The class a scope sits in (`self` / `cls` / `this` there), through enclosing scopes.
    fn class_at(&self, mut scope: u32) -> Option<NodeIdx> {
        loop {
            let s = self.scopes.get(scope as usize)?;
            if s.kind == ScopeKind::Class as u8 {
                let start = s.start;
                return self.view.nodes_in_file(self.file).map(NodeIdx).find(|n| {
                    let node = self.view.node(*n);
                    node.kind().is_type()
                        && pack_pos(node.start_line(), node.start_col()) <= start
                        && start <= pack_pos(node.end_line(), node.end_col())
                        && pack_pos(node.start_line(), 0) == pack_pos((start >> 32) as u32, 0)
                });
            }
            scope = self.scope_parent(scope)?;
        }
    }

    fn std_wrapper(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str) -> Option<StdWrapper> {
        let (prefix, leaf) = name.rsplit_once("::").unwrap_or(("", name));
        if prefix.is_empty() {
            if let Some(i) = self.visible_import(name, 0) {
                let imp = &self.imports[i as usize];
                let spec = self.view.text_str(imp.spec);
                let imported = self.view.text_str(imp.imported);
                return match (spec, imported) {
                    ("std::sync" | "alloc::sync", "Arc") | ("std::rc" | "alloc::rc", "Rc") | ("std::boxed" | "alloc::boxed", "Box") => {
                        Some(StdWrapper::Pointer)
                    }
                    ("std::result" | "core::result" | "std::io", "Result") => Some(StdWrapper::Result),
                    ("std::option" | "core::option", "Option") => Some(StdWrapper::Option),
                    ("std::sync" | "tokio::sync" | "parking_lot" | "futures::lock" | "async_lock" | "spin", "Mutex" | "RwLock") => Some(StdWrapper::Lock),
                    _ => None,
                };
            }
        }
        let wrapper = match leaf {
            "Result" => StdWrapper::Result,
            "Option" => StdWrapper::Option,
            "Arc" | "Rc" | "Box" => StdWrapper::Pointer,
            "Mutex" | "RwLock" => StdWrapper::Lock,
            _ => return None,
        };
        let standard = match prefix {
            "std::result" | "core::result" | "std::io" => wrapper == StdWrapper::Result,
            "std::option" | "core::option" => wrapper == StdWrapper::Option,
            "std::sync" | "tokio::sync" | "parking_lot" | "futures::lock" if wrapper == StdWrapper::Lock => true,
            "std::sync" | "alloc::sync" => leaf == "Arc",
            "std::rc" | "alloc::rc" => leaf == "Rc",
            "std::boxed" | "alloc::boxed" => leaf == "Box",
            "" => matches!(self.lookup_type(wcx, name), TypeRes::Builtin),
            "io" if wrapper == StdWrapper::Result => self.visible_import("io", 0).is_some_and(|i| {
                let imp = &self.imports[i as usize];
                self.view.text_str(imp.spec) == "std" && self.view.text_str(imp.imported) == "io"
            }),
            _ => false,
        };
        standard.then_some(wrapper)
    }

    fn type_value(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str, receiver: Option<NodeIdx>, line: u32, depth: u8) -> Option<ValueType> {
        if depth >= 16 || name.len() > 4096 {
            return None;
        }
        let mut name = name.trim();
        while let Some(rest) = name.strip_prefix('&') {
            name = rest.trim_start();
            if name.starts_with('\'') {
                name = name.split_once(char::is_whitespace)?.1.trim_start();
            }
            name = name.strip_prefix("mut ").unwrap_or(name).trim_start();
        }
        if matches!(name, "Self" | "self" | "this") {
            return receiver.or(wcx.self_ty).map(ValueType::plain);
        }
        if self.family == LangFamily::Rust {
            if name == "_" {
                return None; // a type parameter with no bound
            }
            // `impl Svc + Send` / `dyn Svc`: the first bound that is a project trait
            if let Some(bounds) = name.strip_prefix("impl ").or_else(|| name.strip_prefix("dyn ")) {
                return bounds.split('+').map(str::trim).filter(|b| !b.starts_with('\'')).find_map(|b| match self.lookup_type(wcx, b) {
                    TypeRes::Node(t) => Some(ValueType::plain(t)),
                    _ => None,
                });
            }
            if let Some(open) = name.find('<') {
                let base = name[..open].trim();
                let Some(wrapper) = self.std_wrapper(wcx, base) else {
                    return match self.lookup_type(wcx, base) {
                        TypeRes::Node(t) => Some(ValueType::plain(t)),
                        _ => None,
                    };
                };
                let args = name[open + 1..].strip_suffix('>')?;
                let mut nesting = 0u32;
                let mut end = args.len();
                for (i, c) in args.char_indices() {
                    match c {
                        '<' | '(' | '[' => nesting += 1,
                        '>' | ')' | ']' => nesting = nesting.checked_sub(1)?,
                        ',' if nesting == 0 => {
                            end = i;
                            break;
                        }
                        _ => {}
                    }
                }
                let mut inner = self.type_value(wcx, &args[..end], receiver, line, depth + 1)?;
                inner.wrappers.push(wrapper);
                return Some(inner);
            }
        }
        match self.lookup_type_at(wcx, name, line) {
            TypeRes::Node(t) => Some(ValueType::plain(t)),
            _ if self.family == LangFamily::Js => self.es5_constructor(wcx, name).map(ValueType::plain),
            _ => None,
        }
    }

    /// An ES5 constructor function called `name` (one with prototype members): what `new X()`
    /// makes in code written before classes.
    fn es5_constructor(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str) -> Option<NodeIdx> {
        match self.resolve_name(wcx, name, Accept::Callable, 0, u32::MAX, None, false) {
            NameRes::Found(n, ..) if self.e.proto.contains_key(&n) => Some(n),
            _ => None,
        }
    }

    fn returned_value(&self, wcx: &mut WorkerCx<'e, 'v>, n: NodeIdx, receiver: Option<NodeIdx>, depth: u8) -> Option<ValueType> {
        let node = self.view.node(n);
        if node.kind().is_type() {
            return Some(ValueType::plain(n));
        }
        let cx = wcx.file_cx(self.e, node.file());
        if let Some(b) = self.e.return_types.get(&n) {
            let rt = self.view.text_str(b.decl_type);
            if cx.family == LangFamily::Js {
                // TS facts also retain unsupported annotations. Failure to
                // resolve one must not enable the unique-member fallback.
                return Some(cx.ts_return_value(n, rt).unwrap_or_else(ValueType::unknown));
            }
            if !rt.is_empty() {
                return cx.type_value(wcx, rt, receiver, u32::MAX, depth + 1);
            }
            return cx.expression_value(wcx, self.view.text_str(b.init_ctor), b.scope.0.wrapping_sub(cx.scope_base), u32::MAX, depth + 1);
        }
        cx.type_value(wcx, node.return_type()?, receiver, u32::MAX, depth + 1)
    }

    /// TS return facts contain only nominal names. Look in the declaration's
    /// enclosing scopes and explicit imports, never another file's same-named
    /// class or an unrelated nested declaration.
    fn ts_return_value(&self, function: NodeIdx, name: &str) -> Option<ValueType> {
        if name.is_empty() || name.len() > 4096 {
            return None;
        }
        let (head, tail) = name.split_once('.').unwrap_or((name, ""));
        // The annotation is outside the function body: declarations inside
        // that body cannot shadow a type from an enclosing scope or import.
        let mut owner = self.view.node(function).parent();
        let mut bound = None;
        while let Some(scope) = owner {
            let mut candidates = self.view.by_name(head).filter(|n| {
                let node = self.view.node(*n);
                node.file() == self.file && node.parent() == Some(scope) && Accept::Type.ok(node.kind())
            });
            if let Some(n) = candidates.next() {
                if candidates.next().is_some() {
                    return None;
                }
                bound = Some(Bound::Node(n));
                break;
            }
            owner = self.view.node(scope).parent();
        }
        let bound = bound.or_else(|| self.visible_import(head, 0).map(|i| self.e.bind_type_import(self.file, i as usize)))?;
        let target = if tail.is_empty() { bound } else { self.walk_path_in_space(bound, &tail.split('.').collect::<Vec<_>>(), true)? };
        match target {
            Bound::Node(n) if self.view.node(n).kind().is_type() => Some(ValueType::plain(n)),
            _ => None,
        }
    }

    /// Follow declared return types through a bounded chain. Static methods do
    /// not implicitly return their owner, and Result<T> is not treated as T.
    fn call_value(&self, wcx: &mut WorkerCx<'e, 'v>, callee: &str, scope: u32, line: u32, depth: u8) -> Option<ValueType> {
        if depth >= 16 || callee.len() > 4096 {
            return None;
        }
        // `detail::counting_buffer<>()`, `detail::get_buffer<char>(out)`
        if self.family == LangFamily::C && callee.contains("::") {
            if let Some(n) = self.cpp_scoped(callee, false) {
                return self.returned_value(wcx, n, self.enclosing_type(n), depth + 1);
            }
        }
        if let Some((inner, method)) = callee.rsplit_once('.') {
            if inner.ends_with(['?', ')']) {
                let receiver = self.expression_value(wcx, inner, scope, line, depth + 1)?;
                return self.member_return(wcx, receiver, method, depth + 1);
            }
        }
        // the last separator of either kind: `Foo::Bar.create` is `create` on `Foo::Bar`
        let split = match (callee.rfind("::"), callee.rfind('.')) {
            (Some(c), Some(d)) if d > c => callee.rsplit_once('.'),
            (Some(_), _) => callee.rsplit_once("::"),
            (None, _) => callee.rsplit_once('.'),
        };
        if let Some((owner, method)) = split {
            // Django's default manager returns the concrete model for these
            // operations. Require the actual Model base in the resolved
            // hierarchy; a coincidentally named `.objects.create` is insufficient.
            if self.family == LangFamily::Python && matches!(method, "create" | "get" | "first" | "last" | "earliest" | "latest") {
                if let (Some(base), Some(model)) = (self.e.django_model, owner.strip_suffix(".objects")) {
                    if let TypeRes::Node(t) = self.lookup_type(wcx, model) {
                        if t == base || self.e.ancestors(t).contains(&base) {
                            return Some(ValueType::plain(t));
                        }
                    }
                }
            }
            let implicit = matches!(owner, "self" | "cls" | "this")
                && matches!(self.family, LangFamily::Python | LangFamily::Js | LangFamily::Php | LangFamily::CSharp | LangFamily::Swift | LangFamily::Ruby);
            let receiver = if let Some(b) = self
                .lookup_binding(owner, scope, line, false)
                .filter(|b| !(implicit && self.view.text_str(b.decl_type).is_empty() && self.view.text_str(b.init_ctor).is_empty()))
            {
                self.binding_value(wcx, b, depth + 1)
            } else if implicit {
                // `self.make()`: a method of the class the code sits in
                self.class_at(scope).map(ValueType::plain)
            } else if self.family == LangFamily::Rust && owner.contains('.') && !owner.contains("::") {
                self.path_value(wcx, owner, scope, line, depth + 1)
            } else {
                self.type_value(wcx, owner, None, line, depth + 1)
            };
            if let Some(t) = receiver {
                return self.member_return(wcx, t, method, depth + 1);
            }
        }
        match self.resolve_name(wcx, callee, Accept::Callable, scope, line, None, true) {
            NameRes::Found(n, ..) => self.returned_value(wcx, n, self.enclosing_type(n), depth + 1),
            _ => None,
        }
    }

    /// The value a field path holds (`self.state`): the declared type of each field in turn,
    /// standard wrappers kept; a smart pointer or a lock guard is looked through on the way.
    fn path_value(&self, wcx: &mut WorkerCx<'e, 'v>, path: &str, scope: u32, line: u32, depth: u8) -> Option<ValueType> {
        if depth >= 16 {
            return None;
        }
        let mut segs = path.split('.');
        let root = segs.next()?;
        let b = self.lookup_binding(root, scope, line, false)?;
        let mut value = self.binding_value(wcx, b, depth + 1)?;
        for f in segs {
            let t = value.method_receiver("")?;
            value = self.field_value(wcx, t, f, depth + 1)?;
        }
        Some(value)
    }

    /// The declared value of field `name` of type `t` (through the hierarchy), wrappers kept.
    fn field_value(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, name: &str, depth: u8) -> Option<ValueType> {
        for ty in std::iter::once(t).chain(self.e.ancestors(t)) {
            let cx = wcx.file_cx(self.e, self.view.hot(ty).file);
            let Some(cs) = cx.class_scope_for(ty) else { continue };
            if let Some(b) = cx.bindings.get(&(cs, name)).and_then(|l| l.iter().copied().find(|b| b.kind == BindKind::Field as u8)) {
                return cx.binding_value(wcx, b, depth + 1);
            }
        }
        None
    }

    fn member_return(&self, wcx: &mut WorkerCx<'e, 'v>, mut receiver: ValueType, method: &str, depth: u8) -> Option<ValueType> {
        // a smart pointer's own methods stay its own; any other call goes through to the value
        while receiver.wrappers.last() == Some(&StdWrapper::Pointer) && !pointer_method(method) && receiver.wrappers.len() > 1 {
            receiver.wrappers.pop();
        }
        if receiver.wrappers.last().is_some_and(|w| matches!(w, StdWrapper::Result | StdWrapper::Option)) {
            return match method {
                "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" | "unwrap_or_default" => {
                    receiver.wrappers.pop();
                    Some(receiver)
                }
                "as_ref" | "as_mut" => Some(receiver),
                _ => None,
            };
        }
        if receiver.wrappers.last() == Some(&StdWrapper::Pointer) {
            match method {
                "clone" => return Some(receiver),
                "as_ref" | "as_mut" | "deref" | "deref_mut" => {
                    receiver.wrappers.pop();
                    return Some(receiver);
                }
                _ => {}
            }
        }
        match receiver.wrappers.last() {
            Some(StdWrapper::Lock) if lock_method(method) => {
                receiver.wrappers.pop();
                receiver.wrappers.push(StdWrapper::Guard);
                return Some(receiver);
            }
            Some(StdWrapper::Lock) => return None,
            // std's `lock()` returns a `LockResult` of the guard
            Some(StdWrapper::Guard) if matches!(method, "unwrap" | "expect" | "unwrap_or_else") => return Some(receiver),
            _ => {}
        }
        let node = receiver.method_receiver(method)?;
        let (m, owner) = self.rust_member(wcx, node, method)?;
        self.returned_value(wcx, m, Some(owner), depth + 1)
    }

    /// Member `method` of `t`, else (Rust) of what `t` dereferences to through `impl Deref`,
    /// a few hops at most: the member and the type that declares it.
    fn rust_member(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, method: &str) -> Option<(NodeIdx, NodeIdx)> {
        let mut cur = t;
        for _ in 0..4 {
            if let Some(m) = self.e.find_member(cur, method) {
                return Some((m, cur));
            }
            if self.family != LangFamily::Rust {
                return None;
            }
            // `type Outcome = plugin::Outcome<Config>` has the members of what it names
            let next = match self.e.type_aliases.get(&cur) {
                Some(b) => {
                    let cx = wcx.file_cx(self.e, self.view.hot(cur).file);
                    cx.type_value(wcx, self.view.text_str(b.decl_type), None, u32::MAX, 0).and_then(|v| v.receiver())
                }
                None => self.deref_target(wcx, cur),
            };
            cur = next?;
            if cur == t {
                return None;
            }
        }
        None
    }

    /// What Rust type `t` dereferences to (`impl Deref for T { type Target = U; }`), the impl
    /// found by the type's name and checked to name `t` where it is written.
    fn deref_target(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx) -> Option<NodeIdx> {
        let entries = self.e.derefs.get(self.view.node(t).name())?;
        let mut found = None;
        for &(f, b) in entries {
            let cx = wcx.file_cx(self.e, f);
            if cx.lookup_type(wcx, self.view.text_str(b.name)) != TypeRes::Node(t) {
                continue;
            }
            let target = cx.type_value(wcx, self.view.text_str(b.decl_type), Some(t), u32::MAX, 0)?.receiver()?;
            if found.is_some_and(|x| x != target) {
                return None;
            }
            found = Some(target);
        }
        found
    }

    /// Declared type of field `name` of type `t` (through the hierarchy).
    pub fn field_type(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, name: &str) -> Option<NodeIdx> {
        if let Some(list) = wcx.field_cache.get(&t) {
            if let Some((_, v)) = list.iter().find(|(n, _)| &**n == name) {
                return *v;
            }
        }
        let mut result = None;
        for ty in self.field_owners(t) {
            let owner = self.view.hot(ty).file;
            let cx = wcx.file_cx(self.e, owner);
            if let Some(cs) = cx.class_scope_for(ty) {
                if let Some(list) = cx.bindings.get(&(cs, name)) {
                    if let Some(b) = list.iter().find(|b| b.kind == BindKind::Field as u8) {
                        if let Some(ft) = cx.binding_type(wcx, b) {
                            result = Some(ft);
                            break;
                        }
                    }
                }
            }
        }
        wcx.cache_field(t, name, result, self.e.budget.worker_cache_bytes());
        result
    }

    // ------------------------------------------------------- overload selection
    /// What each argument of the call `r` is, from its shape and the bindings in scope.
    fn arg_types(&self, wcx: &mut WorkerCx<'e, 'v>, args: &CallArgs<'v>, r: &RefRec) -> SmallVec<[ArgTy; 6]> {
        let scope = self.scope_at(r.line, r.col);
        args.shapes.iter().map(|s| self.arg_type(wcx, s, scope, r)).collect()
    }

    fn named_arg(&self, wcx: &mut WorkerCx<'e, 'v>, written: &str) -> ArgTy {
        let simple = overload::simplify_type(written);
        if simple.is_empty() {
            return ArgTy::Unknown;
        }
        let node = if overload::is_value_type(&simple) || simple.ends_with("[]") {
            None
        } else {
            match self.lookup_type(wcx, written) {
                TypeRes::Node(n) => Some(n),
                _ => None,
            }
        };
        ArgTy::Named(simple, node)
    }

    fn node_arg(&self, t: NodeIdx) -> ArgTy {
        ArgTy::Named(self.view.node(t).name().to_owned(), Some(t))
    }

    /// Recover a primitive return annotation only from a single unambiguous JS callee.
    fn primitive_call_arg(&self, wcx: &mut WorkerCx<'e, 'v>, callee: &str, scope: u32, line: u32) -> Option<ArgTy> {
        if self.family != LangFamily::Js || callee.is_empty() || !callee.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
            return None;
        }
        let NameRes::Found(node, ..) = self.resolve_name(wcx, callee, Accept::Callable, scope, line, None, false) else { return None };
        if self.sibling_overloads(node).len() != 1 {
            return None;
        }
        let (_, ret) = self.view.node(node).signature()?.rsplit_once(':')?;
        let ret = ret.trim().trim_end_matches(';').trim();
        matches!(ret, "string" | "number" | "boolean" | "bigint" | "symbol").then(|| self.named_arg(wcx, ret))
    }

    fn binding_arg_type(&self, wcx: &mut WorkerCx<'e, 'v>, b: &BindingRec, depth: u8) -> ArgTy {
        if depth >= 16 {
            return ArgTy::Unknown;
        }
        let declared = self.view.text_str(b.decl_type);
        if !declared.is_empty() {
            return self.named_arg(wcx, declared);
        }
        let init = self.view.text_str(b.init_ctor);
        let satisfied = init.strip_prefix(JS_SATISFIES_VALUE);
        let value = satisfied.unwrap_or(init);
        let alias = value.strip_prefix(JS_BINDING_ALIAS).or_else(|| (b.kind == BindKind::Alias as u8).then_some(value));
        let scope = b.scope.0.wrapping_sub(self.scope_base);
        if let Some(name) = alias {
            if let Some(other) = self.lookup_binding(name, scope, b.line, false).filter(|other| !std::ptr::eq(*other, b)) {
                return self.binding_arg_type(wcx, other, depth + 1);
            }
            if let Some(import) = self.visible_import(name, scope) {
                if let Bound::Node(node) = self.e.bind_import(self.file, import as usize) {
                    let cx = wcx.file_cx(self.e, self.view.hot(node).file);
                    if let Some(other) = cx.file_binding(self.view.node(node).name()) {
                        return cx.binding_arg_type(wcx, other, depth + 1);
                    }
                }
            }
        }
        if let Some(callee) = satisfied.and_then(|s| s.strip_suffix("()")) {
            if let Some(arg) = self.primitive_call_arg(wcx, callee, scope, b.line) {
                return arg;
            }
        }
        self.binding_type(wcx, b).map_or(ArgTy::Unknown, |t| self.node_arg(t))
    }

    fn arg_type(&self, wcx: &mut WorkerCx<'e, 'v>, shape: &str, scope: u32, r: &RefRec) -> ArgTy {
        match shape {
            "?" => ArgTy::Unknown,
            "N" => ArgTy::Null,
            "L" => ArgTy::Lambda(None),
            "#this" => self.enclosing_type(r.from).map_or(ArgTy::Unknown, |t| self.node_arg(t)),
            _ => {
                if let Some(t) = shape.strip_prefix("T:") {
                    return self.named_arg(wcx, t);
                }
                if let Some(callee) = shape.strip_prefix("C:") {
                    return self.primitive_call_arg(wcx, callee, scope, r.line).unwrap_or(ArgTy::Unknown);
                }
                if let Some(v) = shape.strip_prefix("V:") {
                    if let Some(b) = self.lookup_binding(v, scope, r.line, false) {
                        if b.kind == BindKind::LocalFn as u8 {
                            return ArgTy::Lambda(None);
                        }
                        return self.binding_arg_type(wcx, b, 0);
                    }
                    // a bare field: Java / Kotlin only (JS reads fields through `this.`)
                    return if self.family == LangFamily::Jvm { self.field_arg(wcx, r, v) } else { ArgTy::Unknown };
                }
                if let Some(f) = shape.strip_prefix("F:") {
                    return self.field_arg(wcx, r, f);
                }
                match shape.strip_prefix('L').map(str::parse::<u8>) {
                    Some(Ok(k)) => ArgTy::Lambda(Some(k)),
                    _ => ArgTy::Unknown,
                }
            }
        }
    }

    /// Field `name` of the type enclosing the reference, by its declared type (primitives too).
    fn field_arg(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str) -> ArgTy {
        let Some(t) = self.enclosing_type(r.from) else { return ArgTy::Unknown };
        for ty in std::iter::once(t).chain(self.e.ancestors(t)) {
            let cx = wcx.file_cx(self.e, self.view.hot(ty).file);
            let Some(cs) = cx.class_scope_for(ty) else { continue };
            let Some(b) = cx.bindings.get(&(cs, name)).and_then(|l| l.iter().copied().find(|b| b.kind == BindKind::Field as u8)) else { continue };
            let dt = self.view.text_str(b.decl_type);
            if !dt.is_empty() {
                return cx.named_arg(wcx, dt);
            }
            return cx.binding_type(wcx, b).map_or(ArgTy::Unknown, |n| cx.node_arg(n));
        }
        ArgTy::Unknown
    }

    /// The overload of `cands` (same name, preference order) the call's arguments reach:
    /// by count first, then by type. `None` when nothing is known or nothing fits.
    fn pick_overload(&self, wcx: &mut WorkerCx<'e, 'v>, cands: &[NodeIdx], args: &CallArgs<'v>, r: &RefRec) -> Option<NodeIdx> {
        if cands.len() < 2 {
            return None;
        }
        if self.lang == Lang::Cpp {
            return self.cpp_overload(cands, &args.shapes);
        }
        let view = self.view;
        let n = args.shapes.len();
        let lang_of = |c: NodeIdx| view.file(view.hot(c).file).lang();
        // arity settles most sets without typing a single argument
        let by_count: SmallVec<[NodeIdx; 4]> =
            cands.iter().copied().filter(|c| overload::takes(lang_of(*c), view.node(*c).signature(), n) != Some(false)).collect();
        match by_count.len() {
            0 => return None,
            1 => return Some(by_count[0]),
            _ => {}
        }
        let types = self.arg_types(wcx, args, r);
        let mut sigs = Vec::with_capacity(by_count.len());
        let mut tparams: Vec<SmallVec<[&str; 2]>> = Vec::with_capacity(by_count.len());
        let mut param_nodes: Vec<FxHashMap<String, Option<NodeIdx>>> = Vec::with_capacity(by_count.len());
        for &c in &by_count {
            let node = view.node(c);
            let params = node.signature().and_then(|s| overload::parse_params(lang_of(c), s));
            let mut map = FxHashMap::default();
            if let Some(p) = &params {
                let cx = wcx.file_cx(self.e, node.file());
                for item in &p.items {
                    let base = item.ty.trim_end_matches("[]");
                    if base.is_empty() || map.contains_key(&item.ty) || overload::is_value_type(base) || !base.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                        continue;
                    }
                    let t = match cx.lookup_type(wcx, base) {
                        TypeRes::Node(t) => Some(t),
                        _ => None,
                    };
                    map.insert(item.ty.clone(), t);
                }
            }
            sigs.push(params);
            tparams.push(node.type_params().into_iter().filter_map(|t| t.split(|c: char| !(c.is_alphanumeric() || c == '_')).next()).collect());
            param_nodes.push(map);
        }
        let e = self.e;
        let relate = |i: usize, a: &ArgTy, pt: &str| -> Option<bool> {
            let p = (*param_nodes[i].get(pt)?)?;
            match a {
                ArgTy::Named(_, Some(at)) => Some(*at == p || e.ancestors(*at).contains(&p)),
                ArgTy::Named(name, None) => overload::is_value_type(name).then_some(false),
                ArgTy::Lambda(_) => {
                    (!matches!(view.node(p).kind(), NodeKind::Interface | NodeKind::Trait | NodeKind::Protocol | NodeKind::TypeAlias)).then_some(false)
                }
                _ => None,
            }
        };
        overload::choose(self.family, &sigs, &types, &tparams, &relate).map(|i| by_count[i])
    }

    /// The overload of member `name` of `t` (or its ancestors) a call reaches, when the type
    /// declares several.
    fn overloaded_member(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, name: &str, args: Option<&CallArgs<'v>>, r: &RefRec) -> Option<NodeIdx> {
        let args = args?;
        if !matches!(self.family, LangFamily::Jvm | LangFamily::Js) && self.lang != Lang::Cpp {
            return None;
        }
        let cands = self.e.member_overloads(t, name);
        // Kotlin `val socketFactory` beside `fun socketFactory()`, a Java field beside its method:
        // a call names the one method, whichever member is declared first
        if cands.len() == 1 && self.family == LangFamily::Jvm {
            return cands.first().copied();
        }
        self.pick_overload(wcx, &cands, args, r)
    }

    /// Same-named siblings of `pick` in its file (functions declared several times).
    fn sibling_overloads(&self, pick: NodeIdx) -> SmallVec<[NodeIdx; 4]> {
        let view = self.view;
        let h = view.hot(pick);
        let name = view.name_str(h.name);
        let mut out: SmallVec<[NodeIdx; 4]> = view
            .by_name(name)
            .filter(|c| {
                let ch = view.hot(*c);
                ch.file == h.file && ch.parent == h.parent && view.node(*c).kind().is_callable()
            })
            .collect();
        out.sort();
        out
    }

    // ------------------------------------------------------------ name lookup
    fn visible(&self, cand: NodeIdx, name: &str) -> bool {
        let h = self.view.hot(cand);
        if h.file == self.file {
            return true;
        }
        match self.family {
            LangFamily::Go => {
                let same_pkg = self.pkg.0 != NONE && self.e.modules.pkg_of(h.file) == self.pkg;
                same_pkg || name.chars().next().is_some_and(|c| c.is_uppercase())
            }
            LangFamily::Jvm => h.vis != Visibility::Private as u8,
            _ => true,
        }
    }

    fn score(&self, cand: NodeIdx) -> u32 {
        let h = self.view.hot(cand);
        let cf = h.file;
        let cpath = self.view.file(cf).path();
        let mut s = if cf == self.file {
            100
        } else if crate::project::parent_dir(cpath) == crate::project::parent_dir(self.path) || (self.pkg.0 != NONE && self.e.modules.pkg_of(cf) == self.pkg) {
            80
        } else if self.imported_files.contains(&cf) {
            60
        } else if cpath.split('/').next().unwrap_or("") == self.top_dir {
            40
        } else {
            20
        };
        let node = self.view.node(cand);
        if node.is_exported() == Some(true) {
            s += 5;
        }
        let cflags = self.view.file(cf).flags();
        if (cflags & cg_extract::fileflags::TEST != 0) == self.is_test {
            s += 5;
        }
        if cflags & cg_extract::fileflags::GENERATED == 0 {
            s += 3;
        }
        s
    }

    /// Name match over the whole project (design doc 6.5).
    fn global_match(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str, accept: Accept, bare_callable: bool, args: Option<(&CallArgs<'v>, &RefRec)>) -> NameRes {
        let mut best: SmallVec<[NodeIdx; 4]> = SmallVec::new();
        let mut best_score = 0u32;
        let mut seen = 0usize;
        for cand in self.view.by_name(name) {
            seen += 1;
            if seen > 4000 {
                break;
            }
            let node = self.view.node(cand);
            let k = node.kind();
            if !accept.ok(k) || (bare_callable && (matches!(k, NodeKind::Method | NodeKind::Property | NodeKind::Field) || self.not_a_callee(k))) {
                continue;
            }
            let cf = node.file();
            if self.view.file(cf).lang().family() != self.family || self.test_only(cf) {
                continue;
            }
            if !self.visible(cand, name) {
                continue;
            }
            let sc = self.score(cand);
            if sc > best_score {
                best_score = sc;
                best.clear();
                best.push(cand);
            } else if sc == best_score {
                best.push(cand);
            }
        }
        match best.len() {
            0 => NameRes::NotFound,
            1 => {
                let (by_, conf) = if best_score >= 180 {
                    (by::LEXICAL, 98)
                } else if best_score >= 80 {
                    (by::PACKAGE, 85)
                } else if best_score >= 60 {
                    (by::GLOBAL, 70)
                } else if best_score >= 40 {
                    (by::GLOBAL, 55)
                } else {
                    (by::GLOBAL, 40)
                };
                NameRes::Found(best[0], by_, conf)
            }
            k => {
                // overloads under one parent: the arguments pick one, else the first declared
                let p0 = self.view.hot(best[0]).parent;
                if best.iter().all(|c| self.view.hot(*c).parent == p0 && self.view.node(*c).file() == self.view.node(best[0]).file()) {
                    best.sort();
                    let pick = args.and_then(|(a, r)| self.pick_overload(wcx, &best, a, r)).unwrap_or(best[0]);
                    return NameRes::Found(pick, by::PACKAGE, 80);
                }
                // one declaration copied into several source roots; only JVM qualified
                // names carry the package, so equal names elsewhere are distinct symbols
                let q0 = self.view.node(best[0]).qname();
                if self.family == LangFamily::Jvm && best.iter().all(|c| self.view.node(*c).qname() == q0) {
                    if let Some(n) = self.nearest_copy(&best) {
                        return NameRes::Found(n, by::PACKAGE, 80);
                    }
                }
                if self.lang == Lang::Cpp {
                    // a name written alone is no class's member while something else answers it
                    let view = self.view;
                    let free: SmallVec<[NodeIdx; 4]> =
                        best.iter().copied().filter(|c| !view.node(*c).parent().is_some_and(|p| kind_of(view, p).is_type())).collect();
                    let mut named = if free.is_empty() { best.clone() } else { free };
                    // one declaration, repeated across `#if` branches or headers (`call_bare`
                    // picks a function's overload by the arguments)
                    if named.iter().all(|c| view.node(*c).qname() == view.node(named[0]).qname()) {
                        named.sort_by_key(|c| (view.file(view.hot(*c).file).path(), view.node(*c).start_line()));
                        return NameRes::Found(named[0], by::PACKAGE, 80);
                    }
                }
                NameRes::Ambiguous(k as u32)
            }
        }
    }

    /// The candidate in this file's own source root: the only one nearest it by directory
    /// distance.
    fn nearest_copy(&self, cands: &[NodeIdx]) -> Option<NodeIdx> {
        let affinity = |n: NodeIdx| crate::project::path_affinity(self.view.file(self.view.hot(n).file).path(), self.path);
        let best = cands.iter().map(|n| affinity(*n)).max()?;
        let mut top = cands.iter().copied().filter(|n| affinity(*n) == best);
        let first = top.next()?;
        top.next().is_none().then_some(first)
    }

    /// C / C++: a macro invocation is not a call (upstream keeps macros out of the call graph).
    /// Ruby: a variable is never reached by a bare call from another scope.
    /// A C++ `using X = ..` is also bound as a typed local: `X(1)` constructs, it calls no variable.
    fn is_cpp_alias(&self, b: &BindingRec, name: &str) -> bool {
        self.family == LangFamily::C
            && self.bindings.get(&(b.scope.0.wrapping_sub(self.scope_base), name)).is_some_and(|l| l.iter().any(|o| o.kind == BindKind::TypeAlias as u8))
    }

    fn not_a_callee(&self, k: NodeKind) -> bool {
        match self.family {
            LangFamily::C => k == NodeKind::Constant,
            LangFamily::Ruby => k == NodeKind::Variable,
            _ => false,
        }
    }

    /// Production code cannot bind to a definition that exists only in a test file; a name
    /// guess across that line is another program's helper (a spec's `def use`).
    fn test_only(&self, f: FileId) -> bool {
        !self.is_test
            && matches!(self.family, LangFamily::C | LangFamily::CSharp | LangFamily::Php | LangFamily::Ruby | LangFamily::Swift)
            && self.view.file(f).flags() & cg_extract::fileflags::TEST != 0
    }

    /// Member `name` of the type enclosing `from` (and of its outer types for Java / Kotlin).
    fn enclosing_member(&self, wcx: &mut WorkerCx<'e, 'v>, from: Option<NodeIdx>, name: &str, args: Option<(&CallArgs<'v>, &RefRec)>) -> Option<NodeIdx> {
        let view = self.view;
        let ext = self.enclosing_type(from?)?;
        // `extension Outer.Inner { .. }` is a node of its own: the members are the declaration's
        let decl = self.is_swift_extension(ext).then(|| self.swift_declaration(ext)).flatten().filter(|d| *d != ext);
        for t in std::iter::once(ext).chain(decl) {
            if let Some((a, r)) = args {
                if let Some(m) = self.overloaded_member(wcx, t, name, Some(a), r) {
                    return Some(m);
                }
            }
            if let Some(m) = self.e.find_member(t, name) {
                return Some(m);
            }
        }
        let t = ext;
        if self.family == LangFamily::Jvm {
            let mut outer = view.node(t).parent();
            while let Some(o) = outer {
                if kind_of(view, o) == NodeKind::File {
                    break;
                }
                if kind_of(view, o).is_type() {
                    if let Some(m) = self.e.find_member(o, name) {
                        return Some(m);
                    }
                }
                outer = view.node(o).parent();
            }
        }
        None
    }

    /// PHP: the kernel emits `$this->m()`, `self::m()`, `static::m()` and `parent::m()` all as a
    /// bare `m`. A bare `__construct` is always the parent's constructor, and a bare name equal
    /// to the enclosing method is `parent::m()` when an ancestor declares it (otherwise a
    /// recursive call); everything else is the class's own member first.
    fn php_bare_member(&self, from: Option<NodeIdx>, name: &str) -> Option<NodeIdx> {
        let from = from?;
        let t = self.enclosing_type(from)?;
        let inherited = || self.e.ancestors(t).into_iter().find_map(|a| self.e.direct_member(a, name));
        if name == "__construct" {
            return inherited();
        }
        let own = self.e.direct_member(t, name);
        if self.view.node(from).name() == name {
            return inherited().or(own);
        }
        own.or_else(inherited)
    }

    /// `const local = other`: resolve `other` where the alias is declared, through at most a few
    /// aliases of aliases. A local value that is not a function stays local.
    #[allow(clippy::too_many_arguments)]
    fn follow_alias(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        mut b: &'v BindingRec,
        accept: Accept,
        from: Option<NodeIdx>,
        allow_global: bool,
        args: Option<(&CallArgs<'v>, &RefRec)>,
    ) -> NameRes {
        for _ in 0..4 {
            let target = self.view.text_str(b.init_ctor);
            let scope = b.scope.0.wrapping_sub(self.scope_base);
            match self.lookup_binding(target, scope, b.line, false) {
                Some(next) if next.kind == BindKind::Alias as u8 && !std::ptr::eq(next, b) => b = next,
                Some(next) if next.kind == BindKind::LocalFn as u8 && next.node.0 != NONE => return NameRes::Found(next.node, by::LEXICAL, 95),
                Some(_) => return NameRes::Local,
                None => {
                    return match self.resolve_name_args(wcx, target, accept, scope, b.line, from, allow_global, args) {
                        NameRes::Found(n, by_, conf) => NameRes::Found(n, by_, conf.min(95)),
                        other => other,
                    };
                }
            }
        }
        NameRes::Local
    }

    /// The callable a JS constant or variable hands on (upstream `alias-binding`): `export const
    /// alias = realImpl` for a call of `alias`, `export const api = { run: impl }` for `api.run`.
    /// Resolved where the binding is declared; nothing when the initializer is anything else or
    /// names nothing callable.
    pub(crate) fn forward_alias_node(&self, wcx: &mut WorkerCx<'e, 'v>, n: NodeIdx, member: Option<&str>) -> Option<NodeIdx> {
        let view = self.view;
        let node = view.node(n);
        if !matches!(node.kind(), NodeKind::Constant | NodeKind::Variable | NodeKind::Property) || view.file(node.file()).lang().family() != LangFamily::Js {
            return None;
        }
        let target = alias_target_name(node.signature()?, member)?;
        if member.is_none() && target == node.name() {
            return None;
        }
        let cx = wcx.file_cx(self.e, node.file());
        match cx.resolve_name(wcx, target, Accept::Callable, 0, node.start_line(), None, true) {
            NameRes::Found(m, ..) if m != n && matches!(kind_of(view, m), NodeKind::Function | NodeKind::Method | NodeKind::Class | NodeKind::Component) => {
                Some(m)
            }
            _ => None,
        }
    }

    /// Resolve a simple name (design doc 6.3 steps 2, 3, 6, 7).
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_name(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        name: &str,
        accept: Accept,
        scope: u32,
        line: u32,
        from: Option<NodeIdx>,
        allow_global: bool,
    ) -> NameRes {
        self.resolve_name_args(wcx, name, accept, scope, line, from, allow_global, None)
    }

    /// [`Self::resolve_name`] for a call whose arguments may pick among overloads.
    #[allow(clippy::too_many_arguments)]
    fn resolve_name_args(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        name: &str,
        accept: Accept,
        scope: u32,
        line: u32,
        from: Option<NodeIdx>,
        allow_global: bool,
        args: Option<(&CallArgs<'v>, &RefRec)>,
    ) -> NameRes {
        let res = self.lookup_name(wcx, name, accept, scope, line, from, allow_global, args);
        match res {
            NameRes::Found(n, by_, conf) if self.is_swift_extension(n) => NameRes::Found(self.swift_declaration(n).unwrap_or(n), by_, conf),
            NameRes::Found(n, ..) if from.is_some_and(|f| self.swift_nested_elsewhere(n, f)) => NameRes::NotFound,
            _ => res,
        }
    }

    /// Swift: a type nested in another and named like a standard one (`PathMonitor.Result`) is
    /// named bare only inside that type, its extensions and subtypes; elsewhere `Result` is the
    /// standard one. (The kernel names `Outer.Inner` by `Inner` alone, so other nested names
    /// cannot be told from a qualified use.)
    fn swift_nested_elsewhere(&self, n: NodeIdx, from: NodeIdx) -> bool {
        let view = self.view;
        if self.family != LangFamily::Swift || !kind_of(view, n).is_type() || !crate::builtins::is_swift_std_type(view.node(n).name()) {
            return false;
        }
        let Some(owner) = view.node(n).parent().filter(|p| kind_of(view, *p).is_type()) else { return false };
        let owner = view.node(owner).qname();
        let mut t = Some(from).filter(|f| kind_of(view, *f).is_type()).or_else(|| self.enclosing_type(from));
        for _ in 0..8 {
            let Some(ty) = t else { return true };
            if std::iter::once(ty).chain(self.e.ancestors(ty)).any(|a| view.node(a).qname() == owner) {
                return false;
            }
            t = view.node(ty).parent().filter(|p| kind_of(view, *p).is_type());
        }
        true
    }

    /// Swift `extension Foo { .. }` is a class node named `Foo`, but it does not declare `Foo`.
    fn is_swift_extension(&self, n: NodeIdx) -> bool {
        self.family == LangFamily::Swift
            && kind_of(self.view, n) == NodeKind::Class
            && self.view.node(n).signature().is_some_and(|s| s.starts_with("extension "))
    }

    /// Swift `guard let request = request(for: task)`: the name a declaration binds is not in
    /// scope in its own initializer, so the call there is to the method of that name.
    /// `let rt = rt();` / `copy = self.copy()`: an initializer cannot see the local it
    /// declares, so a call of that name on its line is not a call of the local. Only a local
    /// initialised by a call of this very name, so `[f() for f in fs]` keeps its `f`.
    fn self_init_call(&self, b: &BindingRec, name: &str, line: u32) -> bool {
        let callee = self.view.text_str(b.init_ctor).split('(').next().unwrap_or("");
        b.kind == BindKind::Local as u8 && b.line == line && callee.rsplit(['.', ':']).next() == Some(name)
    }

    fn swift_self_init(&self, b: &BindingRec, name: &str, line: u32) -> bool {
        self.family == LangFamily::Swift && b.line == line && self.view.text_str(b.init_ctor).strip_suffix("()") == Some(name)
    }

    /// `t` and what it inherits, for a field lookup; a Swift extension stores no fields, so its
    /// type's declaration and what that inherits too.
    fn field_owners(&self, t: NodeIdx) -> Vec<NodeIdx> {
        let mut chain = vec![t];
        if self.is_swift_extension(t) {
            chain.extend(self.swift_declaration(t));
        }
        let supers: Vec<NodeIdx> = chain.iter().flat_map(|&c| self.e.ancestors(c)).collect();
        chain.extend(supers);
        chain
    }

    /// The declaration an extension extends: a top-level one (this module's first), else the
    /// only nested one of that name. `None` when the type comes from outside the project.
    fn swift_declaration(&self, ext: NodeIdx) -> Option<NodeIdx> {
        let view = self.view;
        let name = view.node(ext).name();
        let decls: SmallVec<[NodeIdx; 4]> = view
            .by_name(name)
            .filter(|&n| kind_of(view, n).is_type() && view.file(view.hot(n).file).lang() == Lang::Swift && !self.is_swift_extension(n))
            .collect();
        let top = decls.iter().copied().filter(|&n| view.node(n).parent().is_none_or(|p| kind_of(view, p) == NodeKind::File)).min_by_key(|&n| {
            let f = view.hot(n).file;
            (self.e.modules.pkg_of(f) != self.pkg, view.file(f).path(), view.node(n).start_line())
        });
        // `extension Outer.Inner` extends that nested type; a bare `extension Result` never does
        let written = view.node(ext).signature().and_then(|s| s.strip_prefix("extension ")).unwrap_or("");
        let mut segs: Vec<&str> = written.split(['<', ' ']).next().unwrap_or("").split('.').collect();
        // a module qualifier (`extension Alamofire.HTTPHeader`) names no type of the project
        while segs.len() > 1 && !self.swift_project_type(segs[0]) {
            segs.remove(0);
        }
        if segs.len() < 2 {
            return top;
        }
        let path = segs.join("::");
        let tail = format!("::{path}");
        let nested: SmallVec<[NodeIdx; 2]> = decls.iter().copied().filter(|&n| view.node(n).qname() == path || view.node(n).qname().ends_with(&tail)).collect();
        if let [only] = nested.as_slice() {
            Some(*only)
        } else {
            None
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lookup_name(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        name: &str,
        accept: Accept,
        scope: u32,
        line: u32,
        from: Option<NodeIdx>,
        allow_global: bool,
        args: Option<(&CallArgs<'v>, &RefRec)>,
    ) -> NameRes {
        let view = self.view;
        let e = self.e;
        // 2. lexical scope
        if accept != Accept::Type {
            let mut found = self.lookup_binding(name, scope, line, false);
            // the local this line declares is not yet in scope: an earlier binding (a callable
            // parameter in `default = default()`) still is
            if accept == Accept::Callable && found.is_some_and(|b| self.self_init_call(b, name, line)) {
                found = self.lookup_binding(name, scope, line.saturating_sub(1), false);
            }
            if let Some(b) = found.filter(|b| !self.is_cpp_alias(b, name) && !self.swift_self_init(b, name, line)) {
                if b.kind == BindKind::Alias as u8 {
                    return self.follow_alias(wcx, b, accept, from, allow_global, args);
                }
                if b.kind == BindKind::LocalFn as u8 && b.node.0 != NONE {
                    return NameRes::Found(b.node, by::LEXICAL, 98);
                }
                if b.kind == BindKind::LocalFn as u8 {
                    if self.family == LangFamily::Js {
                        // Without the declaration's exact node, a same-named function in
                        // another closure is not evidence for this binding.
                        return NameRes::Local;
                    }
                    // a local function declared in this scope: its node is the same-named nested definition
                    let hit = view.by_name(name).find(|n| view.hot(*n).file == self.file && matches!(kind_of(view, *n), NodeKind::Function | NodeKind::Method));
                    if let Some(n) = hit {
                        return NameRes::Found(n, by::LEXICAL, 95);
                    }
                }
                return NameRes::Local;
            }
            // members of the enclosing class are visible by bare name in these languages
            if self.family == LangFamily::Php {
                if let Some(m) = self.php_bare_member(from, name) {
                    return NameRes::Found(m, by::RECEIVER, 88);
                }
            } else if matches!(self.family, LangFamily::Jvm | LangFamily::Python | LangFamily::CSharp | LangFamily::Ruby | LangFamily::Swift | LangFamily::C) {
                if let Some(m) = self.enclosing_member(wcx, from, name, args) {
                    return NameRes::Found(m, by::RECEIVER, 90);
                }
            }
        }
        // 3. import binding
        if let Some(i) = self.visible_import(name, scope) {
            match if accept == Accept::Type { e.bind_type_import(self.file, i as usize) } else { e.bind_import(self.file, i as usize) } {
                Bound::Node(n) => {
                    if accept.ok(kind_of(view, n)) || accept == Accept::Any {
                        return NameRes::Found(n, by::IMPORT, 95);
                    }
                    // imported value used as a namespace (rare)
                    return NameRes::Found(n, by::IMPORT, 80);
                }
                Bound::Module(m) => {
                    // `require('./factory')()` invokes module.exports. Keep the
                    // module namespace for member calls; never substitute its
                    // default function for a missing property such as .static.
                    let imp = &self.imports[i as usize];
                    if self.family == LangFamily::Js && imp.kind == ImportKind::Require as u8 {
                        if let Some(Bound::Node(n)) = e.lookup_export(m, COMMONJS_MODULE, 0) {
                            if accept.ok(kind_of(view, n)) {
                                return NameRes::Found(n, by::IMPORT, 95);
                            }
                        }
                    }
                    return NameRes::Module(m);
                }
                Bound::Pkg(p) => return NameRes::Pkg(p),
                Bound::External => return NameRes::External,
                Bound::Unknown if self.family == LangFamily::Js => return NameRes::NotFound,
                Bound::Unknown => {}
            }
        }
        // 4. same file
        let bare_callable = accept == Accept::Callable;
        let mut same: SmallVec<[NodeIdx; 4]> = SmallVec::new();
        for cand in view.by_name(name) {
            let h = view.hot(cand);
            if h.file != self.file {
                continue;
            }
            let k = NodeKind::from_u8(h.kind).unwrap_or(NodeKind::Variable);
            if !accept.ok(k) || (bare_callable && (matches!(k, NodeKind::Method | NodeKind::Property | NodeKind::Field) || self.not_a_callee(k))) {
                continue;
            }
            same.push(cand);
        }
        if !same.is_empty() {
            // prefer top-level definitions, then the first declared; overloads by the arguments
            let mut pick = same.iter().copied().find(|n| view.node(*n).parent().is_some_and(|p| kind_of(view, p) == NodeKind::File)).unwrap_or(same[0]);
            // JS: one declared in the function the code sits in shadows those outside it
            // ponytail: a block's class counts as the whole function's
            if self.family == LangFamily::Js && line != u32::MAX {
                let around = |n: &NodeIdx| {
                    let p = view.node(*n).parent().filter(|p| kind_of(view, *p) != NodeKind::File)?;
                    let pn = view.node(p);
                    (pn.start_line() <= line && line <= pn.end_line()).then_some((pn.start_line(), pn.start_col()))
                };
                if let Some(inner) = same.iter().filter(|n| around(n).is_some()).max_by_key(|n| around(n)) {
                    pick = *inner;
                }
            }
            if let Some((a, r)) = args.filter(|_| same.len() > 1 && matches!(self.family, LangFamily::Jvm | LangFamily::Js)) {
                let set = self.sibling_overloads(pick);
                if let Some(m) = self.pick_overload(wcx, &set, a, r) {
                    pick = m;
                }
            }
            return NameRes::Found(pick, by::LEXICAL, 92);
        }
        // 5. same package / module
        if self.pkg.0 != NONE {
            if let Some(list) = e.pkg_defs.get(self.pkg.0 as usize).and_then(|m| m.get(name)) {
                let hits: SmallVec<[NodeIdx; 2]> = list.iter().copied().filter(|n| accept.ok(kind_of(view, *n)) && view.hot(*n).file != self.file).collect();
                if let Some(n) = self.nearest_copy(&hits).or_else(|| hits.first().copied()) {
                    return NameRes::Found(n, by::PACKAGE, 85);
                }
            }
        }
        // C#: types of the enclosing namespaces need no `using`
        if self.family == LangFamily::CSharp {
            for p in e.modules.parent_namespaces(self.pkg) {
                if let Some(list) = e.pkg_defs.get(p.0 as usize).and_then(|m| m.get(name)) {
                    if let Some(n) = list.iter().copied().find(|n| accept.ok(kind_of(view, *n))) {
                        return NameRes::Found(n, by::PACKAGE, 85);
                    }
                }
            }
        }
        // Rust: a child module is visible by name
        if self.family == LangFamily::Rust && accept != Accept::Callable {
            if let Some(m) = e.submodule(self.file, name) {
                return NameRes::Module(m);
            }
        }
        // 6. wildcard imports
        for &wi in &self.wildcards {
            let imp = &self.imports[wi as usize];
            let tgt = e.imp_targets[self.file.idx()].get(wi as usize).copied().unwrap_or(ImportTarget::External);
            match tgt {
                ImportTarget::Pkg(p) => {
                    if let Some(list) = e.pkg_defs.get(p.0 as usize).and_then(|m| m.get(name)) {
                        if let Some(n) = list.iter().find(|n| accept.ok(kind_of(view, **n))) {
                            return NameRes::Found(*n, by::IMPORT, 85);
                        }
                    }
                }
                ImportTarget::File(tf) => {
                    if let Some(Bound::Node(n)) = e.lookup_export(tf, name, 0) {
                        if accept.ok(kind_of(view, n)) {
                            return NameRes::Found(n, by::IMPORT, 85);
                        }
                    }
                }
                ImportTarget::Node(c) if imp.kind == ImportKind::Static as u8 => {
                    if let Some(m) = e.find_member(c, name) {
                        return NameRes::Found(m, by::IMPORT, 85);
                    }
                }
                _ => {}
            }
        }
        // 7. global name match
        if is_builtin(self.family, name) || (self.lang == Lang::Cpp && crate::builtins::is_cpp_std_free_function(name)) {
            return NameRes::Builtin;
        }
        if allow_global {
            return self.global_match(wcx, name, accept, bare_callable, args);
        }
        NameRes::NotFound
    }

    // ------------------------------------------------------------ call forms
    fn member_of(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, path: &[&str], method: &str, conf: u8) -> Outcome {
        self.member_call(wcx, t, path, method, conf, None)
    }

    /// `member_of` for a call: its arguments pick among overloads of the final member.
    fn member_call(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        mut t: NodeIdx,
        path: &[&str],
        method: &str,
        conf: u8,
        args: Option<(&CallArgs<'v>, &RefRec)>,
    ) -> Outcome {
        for (i, f) in path.iter().enumerate() {
            t = self.cpp_unalias(wcx, t);
            if let Some(ft) = self.field_type(wcx, t, f) {
                t = ft;
                continue;
            }
            // Rust `worker: Arc<Worker>`: the value behind the smart pointer, though the pointer's
            // own methods (`clone`) stay the pointer's
            if self.family == LangFamily::Rust {
                if let Some(v) = self.field_value(wcx, t, f, 0) {
                    match v.method_receiver(if i + 1 == path.len() { method } else { "" }) {
                        Some(ft) => t = ft,
                        None => return Outcome::st(status::NO_CANDIDATE),
                    }
                    continue;
                }
            }
            match self.e.direct_member(t, f) {
                // `Outer.Inner.m()` / `Mod::Klass.m`: a nested type rather than a field
                Some(inner) if self.holds_members(self.view.node(inner).kind()) => t = inner,
                // `NotificationCenter.default.postNotification(..)`: a member of an outside type
                // the project only extends, its type unknown
                // ponytail: guessed to be that type (`.default`, `.shared`); a wrong guess finds no method
                _ if self.is_swift_extension(t) && self.swift_declaration(t).is_none() => {}
                _ => return Outcome::st(status::NO_CANDIDATE),
            }
        }
        t = self.cpp_unalias(wcx, t);
        if let Some((a, r)) = args {
            if let Some(m) = self.overloaded_member(wcx, t, method, Some(a), r) {
                return Outcome::found(m, by::RECEIVER, conf);
            }
        }
        match self.rust_member(wcx, t, method) {
            Some((m, _)) => Outcome::found(m, by::RECEIVER, conf),
            None => Outcome::st(status::NO_CANDIDATE),
        }
    }

    /// Method of an untyped receiver: only when the name is unique in the language family.
    fn fallback_method(&self, method: &str) -> Outcome {
        // A unique property name does not identify an arbitrary JavaScript receiver.
        // `router.use()` and `app.use()` can denote completely unrelated objects.
        if self.family == LangFamily::Js {
            return Outcome::st(status::NO_CANDIDATE);
        }
        // Ruby duck-types everything: a unique name says nothing of the receiver (`response.body`
        // is Rack's, not the project's only `body`)
        if self.family == LangFamily::Ruby || is_common_method(self.family, method) || is_builtin(self.family, method) {
            return Outcome::st(status::NO_CANDIDATE);
        }
        let mut found: SmallVec<[NodeIdx; 2]> = SmallVec::new();
        let assigned = self.e.member_functions.get(method).map(|v| v.as_slice()).unwrap_or_default();
        for cand in self.view.by_name(method).chain(assigned.iter().copied()) {
            if found.contains(&cand) {
                continue;
            }
            let node = self.view.node(cand);
            if !matches!(node.kind(), NodeKind::Method | NodeKind::Function | NodeKind::Property | NodeKind::Field) {
                continue;
            }
            if node.kind() == NodeKind::Function && !assigned.contains(&cand) {
                continue;
            }
            if self.view.file(node.file()).lang().family() != self.family || !self.visible(cand, method) || self.test_only(node.file()) {
                continue;
            }
            found.push(cand);
            if found.len() > 1 {
                break;
            }
        }
        match found.len() {
            1 => Outcome::found(found[0], by::GLOBAL, 45),
            0 => Outcome::st(status::NO_CANDIDATE),
            _ => Outcome::ambiguous(2),
        }
    }

    /// Express' own factory returns an application with `lib/application.js` mixed in.
    /// Resolve only a direct call to that repository's actual exported factory, never
    /// an untyped variable or a router merely sharing a method name.
    fn express_application_member(&self, b: &BindingRec, method: &str) -> Option<NodeIdx> {
        if self.family != LangFamily::Js {
            return None;
        }
        let init = self.view.text_str(b.init_ctor);
        let head = init.strip_suffix("()")?;
        if head.is_empty() || head.contains(['.', ':', '(', ')']) {
            return None;
        }
        let scope = b.scope.0.wrapping_sub(self.scope_base);
        if self.lookup_binding(head, scope, b.line, false).is_some() {
            return None;
        }
        let i = self.visible_import(head, scope)?;
        let factory = match self.e.bind_import(self.file, i as usize) {
            Bound::Node(n) => n,
            Bound::Module(f) => match self.e.lookup_export(f, COMMONJS_MODULE, 0)? {
                Bound::Node(n) => n,
                _ => return None,
            },
            _ => return None,
        };
        let node = self.view.node(factory);
        if node.kind() != NodeKind::Function || node.name() != "createApplication" {
            return None;
        }
        let factory_path = self.view.file(node.file()).path();
        let package = self.e.project.packages.iter().find(|p| {
            p.name.as_deref() == Some("express")
                && factory_path == if p.dir.is_empty() { "lib/express.js".to_owned() } else { format!("{}/lib/express.js", p.dir) }
        })?;
        let local = self.view.text_str(b.name);
        let overridden_member = format!("{local}.{method}");
        if self
            .view
            .bindings_of(self.file)
            .iter()
            .any(|fact| fact.kind == BindKind::Mutation as u8 && [local, head, overridden_member.as_str()].contains(&self.view.text_str(fact.name)))
        {
            return None;
        }
        let application = if package.dir.is_empty() { "lib/application.js".to_owned() } else { format!("{}/lib/application.js", package.dir) };
        let mixin = self.view.bindings_of(node.file()).iter().find_map(|fact| {
            if fact.kind != BindKind::FactoryMixin as u8 || fact.node != factory {
                return None;
            }
            match self.e.modules.resolve_import(self.view.file(node.file()).lang(), self.view.text_str(fact.decl_type), node.file()) {
                ImportTarget::File(f) if self.view.file(f).path() == application => Some(f),
                _ => None,
            }
        })?;
        let facts = self.view.bindings_of(mixin);
        // Only members actually assigned to the exported object qualify. A
        // same-named method of another object in the module is not evidence.
        let mut candidates = facts.iter().filter(|fact| {
            if fact.kind != BindKind::MemberFn as u8 || fact.node.is_none() || self.view.text_str(fact.name) != method {
                return false;
            }
            let object = self.view.text_str(fact.init_ctor);
            !object.is_empty()
                && facts.iter().any(|b| b.kind == BindKind::CommonJsObject as u8 && self.view.text_str(b.name) == object && b.line <= fact.line)
                && !facts.iter().any(|b| b.kind == BindKind::Mutation as u8 && self.view.text_str(b.name) == object)
                && facts.iter().filter(|b| b.kind == BindKind::Mutation as u8 && self.view.text_str(b.name) == "module.exports").count() == 1
        });
        let candidate = candidates.next()?.node;
        candidates.next().is_none().then_some(candidate)
    }

    fn in_module(&self, wcx: &mut WorkerCx<'e, 'v>, tf: FileId, path: &[&str], method: &str) -> Outcome {
        // walk module / type segments; the last segment may be a member of a type reached on the way
        let mut segs: Vec<&str> = path.to_vec();
        let mut cur = Bound::Module(tf);
        let mut consumed = 0;
        for (i, seg) in segs.iter().enumerate() {
            match self.walk_path(cur, &[seg]) {
                Some(next) => {
                    cur = next;
                    consumed = i + 1;
                }
                None => return Outcome::st(status::NO_CANDIDATE),
            }
        }
        segs.truncate(consumed);
        match cur {
            Bound::Module(m) => match self.walk_path(Bound::Module(m), &[method]) {
                Some(Bound::Node(n)) => Outcome::found(n, by::IMPORT, 92),
                _ => Outcome::st(status::NO_CANDIDATE),
            },
            Bound::Node(n) => {
                if self.view.node(n).kind().is_type() {
                    self.member_of(wcx, n, &[], method, 90)
                } else {
                    // a variable holding an instance: use the type of its initialiser
                    let owner = self.view.hot(n).file;
                    let cx = wcx.file_cx(self.e, owner);
                    let nm = self.view.node(n).name();
                    if let Some(b) = cx.file_binding(nm) {
                        if let Some(t) = cx.binding_type(wcx, b) {
                            return self.member_of(wcx, t, &[], method, 88);
                        }
                    }
                    Outcome::st(status::NO_CANDIDATE)
                }
            }
            _ => Outcome::st(status::NO_CANDIDATE),
        }
    }

    fn is_call(r: &RefRec) -> bool {
        RefKind::from_wire(r.kind) == Some(RefKind::Edge(EdgeKind::Calls))
    }

    fn call_bare(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str, accept: Accept, args: Option<&CallArgs<'v>>) -> Outcome {
        let scope = self.scope_at(r.line, r.col);
        match self.resolve_name_args(wcx, name, accept, scope, r.line, Some(r.from), true, args.map(|a| (a, r))) {
            // a call through `export const alias = realImpl` is a call of `realImpl`
            NameRes::Found(n, b, c) if self.family == LangFamily::Js && Self::is_call(r) => match self.forward_alias_node(wcx, n, None) {
                Some(m) => Outcome::found(m, b, c.min(90)),
                None => Outcome::found(n, b, c),
            },
            // `Foo(1)` runs the constructor its arguments fit
            NameRes::Found(n, b, c) if self.lang == Lang::Cpp && matches!(kind_of(self.view, n), NodeKind::Class | NodeKind::Struct) => {
                let ctors = self.cpp_constructors(n);
                let ctor = args.and_then(|a| self.cpp_overload(&ctors, &a.shapes)).or(ctors.first().copied());
                Outcome::found(ctor.unwrap_or(n), b, c)
            }
            NameRes::Found(n, b, c) if self.family == LangFamily::Ruby => Outcome::found(self.ruby_twin(n, r.from), b, c),
            NameRes::Found(n, b, c) => Outcome::found(self.cpp_function_overload(n, args), b, c),
            // a Ruby call written with a name alone is a method call, whatever local shares it
            res @ (NameRes::Local | NameRes::NotFound) if self.family == LangFamily::Ruby => match self.ruby_bare_method(r, name) {
                Some(o) => o,
                None if res == NameRes::Local => Outcome::st(status::LOCAL_BINDING),
                None => Outcome::st(status::NO_CANDIDATE),
            },
            NameRes::Local => Outcome::st(status::LOCAL_BINDING),
            NameRes::External | NameRes::Module(_) | NameRes::Pkg(_) => Outcome::st(status::EXTERNAL),
            NameRes::Builtin => Outcome::st(status::BUILTIN),
            NameRes::Ambiguous(k) => Outcome::ambiguous(k),
            NameRes::NotFound => Outcome::st(status::NO_CANDIDATE),
        }
    }

    fn call_member(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, root: &str, path: &[&str], method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        let scope = self.scope_at(r.line, r.col);
        let view = self.view;
        let call = args.map(|a| (a, r));
        // PHP `\Vendor\Lib\Logger::make()`: a namespaced root is resolved as written, never
        // reduced to a same-named project class
        if self.family == LangFamily::Php && root.contains('\\') {
            return match self.e.modules.resolve_import(self.lang, root, self.file) {
                ImportTarget::Node(n) if self.holds_members(kind_of(view, n)) => self.member_call(wcx, n, path, method, 90, call),
                ImportTarget::External => Outcome::st(status::EXTERNAL),
                _ => Outcome::st(status::NO_CANDIDATE),
            };
        }
        // (a) a local variable, parameter or field; C++ `a::f` never reads one called `a`
        let scoped_root = self.lang == Lang::Cpp && r_name_is_scoped(view.text_str(r.name));
        if let Some(b) = self.lookup_binding(root, scope, r.line, false).filter(|_| !scoped_root) {
            if b.kind != BindKind::LocalFn as u8 {
                // With fields, the final method belongs to the last field, not
                // to the root pointer (arc.field.clone() may call Field::clone).
                let receiver_method = if path.is_empty() { method } else { "" };
                return match self.binding_value(wcx, b, 0) {
                    Some(t) if t.method_receiver(receiver_method).is_some() => {
                        let o = self.member_call(wcx, t.method_receiver(receiver_method).unwrap(), path, method, 90, call);
                        if o.status == status::RESOLVED {
                            o
                        } else {
                            Outcome::st(status::NO_CANDIDATE)
                        }
                    }
                    Some(_) => Outcome::st(status::NO_CANDIDATE),
                    None => {
                        if !self.view.text_str(b.decl_type).is_empty() {
                            return Outcome::st(status::NO_CANDIDATE);
                        }
                        if path.is_empty() {
                            if let Some(member) = self.express_application_member(b, method) {
                                return Outcome::found(member, by::FRAMEWORK, 90);
                            }
                        }
                        // `const api = { run: impl }` at the top of this file: `api.run()` runs `impl`
                        if self.family == LangFamily::Js && path.is_empty() && b.scope.0 == self.scope_base {
                            if let Some(n) = self.e.lookup_top_level(self.file, root) {
                                if let Some(m) = self
                                    .e
                                    .direct_member(n, method)
                                    .or_else(|| Self::is_call(r).then(|| self.forward_alias_node(wcx, n, Some(method))).flatten())
                                {
                                    return Outcome::found(m, by::LEXICAL, 90);
                                }
                            }
                        }
                        let init = self.view.text_str(b.init_ctor);
                        // Unknown satisfies operands and their aliases do not
                        // justify guessing an unrelated project member.
                        if self.blocks_member_fallback(wcx, b, 0) {
                            return Outcome::st(status::LOCAL_BINDING);
                        }
                        // Neither does an initializer rooted in an external import.
                        let head = init.split(['.', ':', '(']).next().unwrap_or("");
                        if let Some(i) = self.visible_import(head, scope).filter(|_| self.lookup_binding(head, scope, r.line, false).is_none()) {
                            if self.e.bind_import(self.file, i as usize) == Bound::External {
                                return Outcome::st(status::EXTERNAL);
                            }
                        }
                        if self.family == LangFamily::Swift && path.is_empty() {
                            if let Some(n) = self.swift_chained(r, root, method, true) {
                                return Outcome::found(n, by::GLOBAL, 40);
                            }
                        }
                        let o = self.fallback_method(method);
                        if o.status == status::RESOLVED && path.is_empty() {
                            o
                        } else {
                            Outcome::st(status::LOCAL_BINDING)
                        }
                    }
                };
            }
        }
        // (a2) Java / Kotlin / C# / Swift / C++: a field of the enclosing type or an outer one, `this.`
        // implied (the kernel writes `this.repo.find()` as `repo.find`)
        if matches!(self.family, LangFamily::Jvm | LangFamily::CSharp | LangFamily::Swift) || self.lang == Lang::Cpp {
            if let Some(o) = self.call_on_field(wcx, r, root, path, method, call) {
                return o;
            }
        }
        // (b) imported root
        if let Some(i) = self.visible_import(root, scope) {
            match self.e.bind_import(self.file, i as usize) {
                Bound::Module(tf) => return self.in_module(wcx, tf, path, method),
                Bound::Pkg(p) => {
                    let first = path.first().copied().unwrap_or(method);
                    let list = self.e.pkg_defs.get(p.0 as usize).and_then(|m| m.get(first));
                    let hit = list.and_then(|l| l.iter().find(|n| self.visible(**n, first)).copied());
                    return match hit {
                        Some(n) if path.is_empty() => Outcome::found(n, by::IMPORT, 92),
                        Some(n) => self.member_of(wcx, n, &path[1..], method, 90),
                        None => Outcome::st(status::NO_CANDIDATE),
                    };
                }
                Bound::Node(n) => {
                    let k = kind_of(view, n);
                    if k.is_type() {
                        return self.member_call(wcx, n, path, method, 92, call);
                    }
                    // an imported `export const api = { run: impl }`: `api.run()` runs `impl`
                    if self.family == LangFamily::Js && path.is_empty() && Self::is_call(r) {
                        if let Some(m) = self.forward_alias_node(wcx, n, Some(method)) {
                            return Outcome::found(m, by::IMPORT, 90);
                        }
                    }
                    // an imported module-level variable: the type of its initialiser
                    {
                        let owner = view.hot(n).file;
                        let cx = wcx.file_cx(self.e, owner);
                        let nm = view.node(n).name();
                        if let Some(b) = cx.file_binding(nm) {
                            if let Some(t) = cx.binding_type(wcx, b) {
                                return self.member_of(wcx, t, path, method, 88);
                            }
                        }
                    }
                    if let Some(rt) = view.node(n).return_type() {
                        let owner = view.hot(n).file;
                        let cx = wcx.file_cx(self.e, owner);
                        if let TypeRes::Node(t) = cx.lookup_type(wcx, rt) {
                            return self.member_of(wcx, t, path, method, 85);
                        }
                    }
                    return Outcome::st(status::NO_CANDIDATE);
                }
                Bound::External => return Outcome::st(status::EXTERNAL),
                Bound::Unknown if self.family == LangFamily::Js => return Outcome::st(status::NO_CANDIDATE),
                Bound::Unknown => {}
            }
        }
        // (c) a type (static member) or, in Rust, a child module
        if self.family == LangFamily::Rust {
            let mut qualified = root.to_owned();
            for seg in path.iter().copied().chain(std::iter::once(method)) {
                qualified.push_str("::");
                qualified.push_str(seg);
            }
            if let Some(Bound::Node(n)) = self.rust_path_bound(&qualified) {
                if Accept::Callable.ok(kind_of(view, n)) {
                    return Outcome::found(n, by::IMPORT, 92);
                }
            }
            if let Some(mf) = self.e.submodule(self.file, root) {
                return self.in_module(wcx, mf, path, method);
            }
        }
        match self.lookup_type(wcx, root) {
            TypeRes::Node(t) => {
                if self.holds_members(view.node(t).kind()) {
                    let o = self.member_call(wcx, t, path, method, 90, call);
                    if o.status != status::RESOLVED && matches!(self.family, LangFamily::C | LangFamily::CSharp | LangFamily::Php | LangFamily::Ruby) {
                        // `Mod::Klass.m` / `ns::f`: the root may be a namespace-like node whose
                        // members are only reachable by qualified name
                        if let Some(n) = self.qualified_callable(r.from, root, path, method, false) {
                            return Outcome::found(self.cpp_function_overload(n, args), by::QUALIFIED, 90);
                        }
                    }
                    // A known receiver constrains the lookup to its own hierarchy: no
                    // unique-name fallback when the member is missing.
                    return o;
                }
            }
            TypeRes::External => return Outcome::st(status::EXTERNAL),
            TypeRes::Builtin => return Outcome::st(status::BUILTIN),
            TypeRes::Unknown => {}
        }
        if is_builtin(self.family, root) {
            return Outcome::st(status::BUILTIN);
        }
        // JS `window.SelectFilter = { init: .. }` in one script, `SelectFilter.init()` in another
        if self.family == LangFamily::Js && path.is_empty() {
            let mut globals = view.by_qname(&format!("window.{root}"));
            if let (Some(g), None) = (globals.next(), globals.next()) {
                if let Some(m) = self.e.direct_member(g, method) {
                    return Outcome::found(m, by::GLOBAL, 80);
                }
            }
        }
        if matches!(self.family, LangFamily::C | LangFamily::CSharp | LangFamily::Php | LangFamily::Ruby) {
            if let Some(n) = self.qualified_callable(r.from, root, path, method, false) {
                return Outcome::found(self.cpp_function_overload(n, args), by::QUALIFIED, 90);
            }
        }
        // (d) nothing known about the receiver. A `::`-qualified call in C++, PHP or Ruby names
        // a namespace or class we could not find (a dependency): no unique-name guess.
        let qualified = matches!(self.family, LangFamily::C | LangFamily::Php | LangFamily::Ruby) && r_name_is_scoped(self.view.text_str(r.name));
        // A capitalized receiver that no project type answers is a dependency's type (`Task.Run`,
        // `File.join`), not a value whose method could be guessed by name.
        if matches!(self.family, LangFamily::CSharp | LangFamily::Swift | LangFamily::Ruby) && root.starts_with(|c: char| c.is_uppercase()) {
            return Outcome::st(status::EXTERNAL);
        }
        if self.lang == Lang::Cpp && qualified && self.e.macro_namespaces.contains(root) {
            return self.macro_namespace_call(path, method, args);
        }
        if path.is_empty() && !qualified {
            self.fallback_method(method)
        } else {
            Outcome::st(status::NO_CANDIDATE)
        }
    }

    /// C++ `fmt::format(..)` where `fmt` is opened by a macro (`FMT_BEGIN_NAMESPACE`), so the
    /// index holds no such namespace and `format` sits at the top level of a header.
    fn macro_namespace_call(&self, path: &[&str], method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        let mut q = path.join("::");
        if !q.is_empty() {
            q.push_str("::");
        }
        q.push_str(method);
        self.global_namespace_call(&q, args, true)
    }

    /// A call on field `root` of the type enclosing `r` (or of an outer type). `None` when no
    /// such field is declared; a field whose declared type is outside the project ends here.
    fn call_on_field(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        r: &RefRec,
        root: &str,
        path: &[&str],
        method: &str,
        call: Option<(&CallArgs<'v>, &RefRec)>,
    ) -> Option<Outcome> {
        let view = self.view;
        let mut t = self.enclosing_type(r.from);
        for _ in 0..8 {
            let ty = t?;
            if let Some(ft) = self.field_type(wcx, ty, root) {
                let o = self.member_call(wcx, ft, path, method, 90, call);
                return Some(if o.status == status::RESOLVED { o } else { Outcome::st(status::NO_CANDIDATE) });
            }
            if let Some(declared) = self.field_decl_type(wcx, ty, root) {
                // declared, not a project type: nothing in the project is its member
                return (!declared.is_empty()).then(|| Outcome::st(status::EXTERNAL));
            }
            t = view.node(ty).parent().filter(|p| kind_of(view, *p).is_type());
        }
        None
    }

    /// Declared type text of field `name` of `t` or an ancestor (`Some("")`: untyped).
    fn field_decl_type(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, name: &str) -> Option<String> {
        for ty in self.field_owners(t) {
            let cx = wcx.file_cx(self.e, self.view.hot(ty).file);
            let Some(cs) = cx.class_scope_for(ty) else { continue };
            if let Some(b) = cx.bindings.get(&(cs, name)).and_then(|l| l.iter().copied().find(|b| b.kind == BindKind::Field as u8)) {
                return Some(self.view.text_str(b.decl_type).to_owned());
            }
        }
        None
    }

    fn call_this(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, kw: &str, fields: &[&str], method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        // in a function given to an ES5 constructor's prototype, `this` is the constructor's object
        let t = self.enclosing_type(r.from).or_else(|| self.e.proto_owner.get(&r.from).copied().filter(|_| self.family == LangFamily::Js));
        let Some(t) = t else {
            if self.family == LangFamily::Js && fields.is_empty() {
                if let Some(n) = self.assigned_this_member(r, method) {
                    return Outcome::found(n, by::RECEIVER, 90);
                }
                // `this.m()` in an object literal's method (`_.mixin(Input.prototype, Emitter, {..})`):
                // `this` is no arbitrary receiver, and the file defines one function `m`
                let view = self.view;
                let mut own =
                    view.by_name(method).filter(|&n| view.hot(n).file == self.file && matches!(kind_of(view, n), NodeKind::Function | NodeKind::Method));
                if let (Some(n), None) = (own.next(), own.next()) {
                    return Outcome::found(n, by::LEXICAL, 50);
                }
            }
            return if fields.is_empty() { self.fallback_method(method) } else { Outcome::st(status::NO_CANDIDATE) };
        };
        let _ = kw;
        let o = self.member_call(wcx, t, fields, method, 95, args.map(|a| (a, r)));
        if o.status == status::RESOLVED {
            return o;
        }
        // `self.m()` in `extension Outer.Inner`: the nested declaration's member
        if let Some(d) = self.is_swift_extension(t).then(|| self.swift_declaration(t)).flatten().filter(|d| *d != t) {
            let o = self.member_call(wcx, d, fields, method, 95, args.map(|a| (a, r)));
            if o.status == status::RESOLVED {
                return o;
            }
        }
        if fields.is_empty() {
            // a callable stored in a field of this type
            let tf = self.view.hot(t).file;
            let cx = wcx.file_cx(self.e, tf);
            if let Some(cs) = cx.class_scope_for(t) {
                if let Some(list) = cx.bindings.get(&(cs, self.view.name_str(self.view.hot(t).name))) {
                    let _ = list;
                }
                for key in cx.bindings.keys() {
                    if key.0 == cs && key.1 == method {
                        return Outcome::st(status::LOCAL_BINDING);
                    }
                }
            }
            // A known receiver constrains member lookup to its own hierarchy.
            return Outcome::st(status::NO_CANDIDATE);
        }
        Outcome::st(status::NO_CANDIDATE)
    }

    fn assigned_this_member(&self, r: &RefRec, method: &str) -> Option<NodeIdx> {
        let facts = self.view.bindings_of(self.file);
        let call = facts.iter().find(|b| {
            b.kind == BindKind::AssignedThisCall as u8
                && b.node == r.from
                && b.line == r.line
                && self.view.text_str(b.name) == method
                && self.view.text_str(b.init_ctor).parse::<u32>().ok() == Some(r.col)
        })?;
        let object = self.view.text_str(call.decl_type);
        let from = facts.iter().find(|b| b.kind == BindKind::MemberFn as u8 && b.node == r.from && self.view.text_str(b.init_ctor) == object)?;
        if facts.iter().any(|b| b.kind == BindKind::Mutation as u8 && self.view.text_str(b.name) == object) {
            return None;
        }
        let property = format!("{object}.{method}");
        if facts.iter().filter(|b| b.kind == BindKind::Mutation as u8 && self.view.text_str(b.name) == property).count() > 1 {
            return None;
        }
        let mut targets = facts.iter().filter(|b| {
            b.kind == BindKind::MemberFn as u8
                && b.node.is_some()
                && b.scope == from.scope
                && self.view.text_str(b.name) == method
                && self.view.text_str(b.init_ctor) == object
        });
        let node = targets.next()?.node;
        targets.next().is_none().then_some(node)
    }

    fn call_super(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        let Some(t) = self.enclosing_type(r.from) else { return Outcome::st(status::NO_CANDIDATE) };
        for a in self.e.ancestors(t) {
            if let Some(m) = self.e.direct_member(a, method) {
                // the overloads the nearest declaring supertype sees, its own and inherited
                let pick = self.overloaded_member(wcx, a, method, args, r).unwrap_or(m);
                return Outcome::found(pick, by::RECEIVER, 90);
            }
        }
        Outcome::st(status::NO_CANDIDATE)
    }

    fn call_result(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, inner: &str, method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        // type produced by calling `inner`
        let value = match self.call_value(wcx, inner, self.scope_at(r.line, r.col), r.line, 0) {
            Some(value) => Some(value),
            None => {
                // `inner` may be a method of a typed receiver: resolve it as a call first (its
                // own arguments are not this reference's)
                let o = self.call_name_with(wcx, r, inner, None);
                if o.status != status::RESOLVED {
                    return Outcome::st(status::NO_CANDIDATE);
                }
                let n = NodeIdx(o.target);
                self.returned_value(wcx, n, self.enclosing_type(n), 0)
            }
        };
        let Some(t) = value.and_then(|v| v.method_receiver(method)) else { return Outcome::st(status::NO_CANDIDATE) };
        self.member_call(wcx, t, &[], method, 85, args.map(|a| (a, r)))
    }

    /// A namespace-qualified callable (`fmt::detail::write`, `Ns.Util.Run`, `Rack::Utils.escape`)
    /// by its qualified name, as written or relative to the namespaces enclosing the caller.
    /// Lookup order follows lexical scoping: the caller's innermost enclosing namespace first,
    /// then the outer ones, then the name as an absolute path (`absolute_only` skips the
    /// relative levels: C++ `::util::log`). Within one level all hits share one qualified name
    /// (a header declaration and its definition, overloads, an amalgamated copy): the caller's
    /// own file wins, then a file it includes or imports, then the first by path and line.
    fn qualified_callable(&self, from: NodeIdx, root: &str, path: &[&str], method: &str, absolute_only: bool) -> Option<NodeIdx> {
        let view = self.view;
        let mut segs: SmallVec<[&str; 6]> = SmallVec::new();
        segs.push(root);
        segs.extend(path.iter().copied());
        segs.push(method);
        let joined = segs.join("::");
        let mut levels: Vec<Vec<String>> = Vec::new();
        if !absolute_only {
            let mut prefix = view.node(from).qname();
            while let Some(i) = prefix.rfind("::") {
                prefix = &prefix[..i];
                levels.push(vec![format!("{prefix}::{joined}")]);
            }
        }
        let mut absolute = vec![joined.clone()];
        let sep = match self.family {
            LangFamily::CSharp => Some("."),
            LangFamily::Php => Some("\\"),
            _ => None,
        };
        if let Some(sep) = sep {
            // the namespace part of a qualified name uses the language's separator
            for i in (1..segs.len() - 1).rev() {
                absolute.push(format!("{}::{}", segs[..i].join(sep), segs[i..].join("::")));
            }
        }
        levels.push(absolute);
        for cands in levels {
            let mut hits: SmallVec<[NodeIdx; 4]> = SmallVec::new();
            for q in &cands {
                for n in view.by_qname(q) {
                    let node = view.node(n);
                    if view.file(node.file()).lang().family() == self.family && Accept::Callable.ok(node.kind()) && node.kind() != NodeKind::File {
                        hits.push(n);
                    }
                }
            }
            if hits.is_empty() {
                continue;
            }
            return hits.iter().copied().min_by_key(|h| {
                let f = view.hot(*h).file;
                let rank = if f == self.file {
                    0
                } else if self.imported_files.contains(&f) {
                    1
                } else {
                    2
                };
                (rank, view.file(f).path(), view.node(*h).start_line())
            });
        }
        None
    }

    /// Swift: `Data()`, `String(x)`, `URL(string:)` construct standard-library types. A
    /// project `extension Data { .. }` is emitted as a class node named `Data`, but it does not
    /// declare the type, so the call is a builtin rather than an edge to the extension. Member
    /// lookups through the extension (`d.projectMethod()`) are unaffected.
    fn swift_extended_builtin(&self, name: &str, r: &RefRec) -> bool {
        if self.family != LangFamily::Swift || !crate::builtins::is_swift_std_type(name) {
            return false;
        }
        if self.lookup_binding(name, self.scope_at(r.line, r.col), r.line, false).is_some() {
            return false;
        }
        // Extensions are class nodes; a struct, enum, protocol or alias of that name in this
        // module is a real declaration that shadows the standard type: a top-level one
        // everywhere, a nested one (`struct PathMonitor { enum Result { .. } }`) only inside
        // its enclosing declaration.
        let view = self.view;
        !view.by_name(name).any(|n| {
            let h = view.hot(n);
            matches!(kind_of(view, n), NodeKind::Struct | NodeKind::Enum | NodeKind::Protocol | NodeKind::TypeAlias)
                && view.file(h.file).lang() == Lang::Swift
                && self.e.modules.pkg_of(h.file) == self.pkg
                && match view.node(n).parent() {
                    Some(p) if kind_of(view, p) != NodeKind::File => self.is_ancestor(p, r.from),
                    _ => true,
                }
        })
    }

    /// Whether `anc` is `n` or one of its enclosing declarations.
    fn is_ancestor(&self, anc: NodeIdx, n: NodeIdx) -> bool {
        let mut cur = n;
        for _ in 0..256 {
            if cur == anc {
                return true;
            }
            match self.view.node(cur).parent() {
                Some(p) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// PHP `\App\Helpers\format_id()` / C++ `::util::log()`: a qualified free function.
    fn qualified_free_call(&self, r: &RefRec, name: &str) -> Outcome {
        if self.family == LangFamily::Php {
            return match self.e.modules.resolve_import(self.lang, name, self.file) {
                ImportTarget::Node(n) if Accept::Callable.ok(kind_of(self.view, n)) => Outcome::found(n, by::QUALIFIED, 90),
                ImportTarget::External => Outcome::st(status::EXTERNAL),
                _ => Outcome::st(status::NO_CANDIDATE),
            };
        }
        let abs = name.trim_start_matches("::");
        let segs: SmallVec<[&str; 6]> = abs.split("::").filter(|s| !s.is_empty()).collect();
        match segs.split_last() {
            // `::std::string(..)` is `std::string(..)`
            Some((_, rest)) if !rest.is_empty() && is_builtin(self.family, rest[0]) => Outcome::st(status::BUILTIN),
            Some((method, rest)) if !rest.is_empty() => match self.qualified_callable(r.from, rest[0], &rest[1..], method, true) {
                Some(n) => Outcome::found(n, by::QUALIFIED, 90),
                None => Outcome::st(status::NO_CANDIDATE),
            },
            Some((only, _)) => self.global_namespace_call(only, None, false),
            None => Outcome::st(status::NO_CANDIDATE),
        }
    }

    /// C++ `::open(..)`: a function of the global namespace, never the enclosing
    /// `test::open` that wraps it. Only declarations at the top level of the calling file or
    /// of a file it includes (transitively) qualify; a same-named one elsewhere is a different
    /// translation unit's (POSIX/Win32 functions are not in the project at all).
    /// `anywhere`: the namespace itself vouches for the name, so a file with no `#include` of it
    /// (`import fmt;`) still reaches it, behind what it does include; `fmt::printf` is the
    /// namespace's, not the C library's.
    fn global_namespace_call(&self, name: &str, args: Option<&CallArgs<'v>>, anywhere: bool) -> Outcome {
        if !anywhere && is_builtin(self.family, name) {
            return Outcome::st(status::BUILTIN);
        }
        let view = self.view;
        let mut cands: SmallVec<[_; 4]> = view
            .by_qname(name)
            .filter(|&n| Accept::Callable.ok(kind_of(view, n)) && !self.not_a_callee(kind_of(view, n)))
            .filter_map(|n| {
                let f = view.hot(n).file;
                let rank = if f == self.file {
                    0u8
                } else if self.e.includes_transitively(self.file, f) {
                    1
                } else if anywhere {
                    2
                } else {
                    return None;
                };
                Some((rank, view.file(f).path(), view.node(n).start_line(), n))
            })
            .collect();
        cands.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
        let order: SmallVec<[NodeIdx; 4]> = cands.iter().map(|c| c.3).collect();
        match args.and_then(|a| self.cpp_overload(&order, &a.shapes)).or(order.first().copied()) {
            Some(n) => Outcome::found(n, by::QUALIFIED, 90),
            None => Outcome::st(status::NO_CANDIDATE),
        }
    }

    /// Upstream's last resort (`findBestMatch`) for a call it could only match by name: the file,
    /// then the directories it shares with the caller, then a function over anything else and an
    /// exported definition, then the nearer line; the first in path order on a tie.
    // ponytail: a guess by name and place, as upstream makes it; it is gated to Ruby and Swift.
    fn nearest_by_name(&self, r: &RefRec, cands: impl Iterator<Item = NodeIdx>) -> Option<NodeIdx> {
        let view = self.view;
        let dirs: Vec<&str> = self.path.split('/').collect();
        let dirs = &dirs[..dirs.len().saturating_sub(1)];
        let mut list: Vec<NodeIdx> = cands.collect();
        list.sort_by_key(|n| (view.file(view.hot(*n).file).path(), view.node(*n).start_line()));
        let mut best: Option<(f64, NodeIdx)> = None;
        for n in list {
            let node = view.node(n);
            let f = view.hot(n).file;
            let p: Vec<&str> = view.file(f).path().split('/').collect();
            let shared = dirs.iter().zip(&p[..p.len().saturating_sub(1)]).take_while(|(a, b)| a == b).count();
            let mut s = (shared * 15).min(80) as f64;
            if f == self.file {
                s += 100.0 + (20.0 - (node.start_line() as f64 - r.line as f64).abs() / 10.0).max(0.0);
            }
            if matches!(node.kind(), NodeKind::Function | NodeKind::Method) {
                s += 25.0;
            }
            if node.is_exported() == Some(true) {
                s += 10.0;
            }
            if best.is_none_or(|(b, _)| s > b) {
                best = Some((s, n));
            }
        }
        best.map(|(_, n)| n)
    }

    /// Ruby `def build` and `def self.build` share a name: a bare call is on `self`, so a class
    /// method's call reaches the class method and an instance method's the instance one.
    fn ruby_twin(&self, n: NodeIdx, from: NodeIdx) -> NodeIdx {
        let view = self.view;
        if kind_of(view, n) != NodeKind::Method || kind_of(view, from) != NodeKind::Method {
            return n;
        }
        let want = view.node(from).is_static() == Some(true);
        if (view.node(n).is_static() == Some(true)) == want {
            return n;
        }
        view.by_qname(view.node(n).qname())
            .filter(|m| kind_of(view, *m) == NodeKind::Method && view.hot(*m).file == view.hot(n).file && (view.node(*m).is_static() == Some(true)) == want)
            .min_by_key(|m| view.node(*m).start_line())
            .unwrap_or(n)
    }

    /// A Ruby call by a name alone: a method of the class around it and its ancestry; in a
    /// module or top-level code, whose `self` is whatever includes or runs it, any method of
    /// that name, the nearest by place (upstream `isRubyMethodInScope`).
    fn ruby_bare_method(&self, r: &RefRec, name: &str) -> Option<Outcome> {
        let view = self.view;
        let t = self.enclosing_type(r.from);
        if let Some(m) = t.and_then(|t| self.e.find_member(t, name)).filter(|m| kind_of(view, *m) == NodeKind::Method) {
            return Some(Outcome::found(self.ruby_twin(m, r.from), by::RECEIVER, 88));
        }
        if t.is_some_and(|t| kind_of(view, t) == NodeKind::Class) {
            return None;
        }
        let cands = view
            .by_name(name)
            .filter(|n| view.file(view.hot(*n).file).lang() == Lang::Ruby && matches!(kind_of(view, *n), NodeKind::Method | NodeKind::Function));
        self.nearest_by_name(r, cands).map(|n| Outcome::found(self.ruby_twin(n, r.from), by::GLOBAL, 40))
    }

    /// A Swift call on a value of no known type (`session.request(..).validate()`): a member of
    /// that name of some type, the nearest by place (upstream `isSwiftCallTarget`). Called on a
    /// variable (`named`), a member of the type the variable names (`session` a `Session`,
    /// declaration before extensions) or shares a word with comes first, and failing those only
    /// a member no other type has a namesake of. A name the standard
    /// collections and strings carry stays theirs unless the receiver names the member's owner.
    fn swift_chained(&self, r: &RefRec, receiver: &str, method: &str, named: bool) -> Option<NodeIdx> {
        let view = self.view;
        let last = receiver.rsplit('.').next().unwrap_or("");
        let std_name = SWIFT_STD_METHODS.contains(&method);
        let mut tiers: [Vec<NodeIdx>; 3] = Default::default();
        for n in view.by_name(method) {
            let node = view.node(n);
            let Some(owner) = node.parent().filter(|p| kind_of(view, *p).is_type()) else { continue };
            let owner = view.node(owner).name();
            if view.file(node.file()).lang() != Lang::Swift
                || !matches!(
                    node.kind(),
                    NodeKind::Method | NodeKind::Function | NodeKind::Property | NodeKind::Field | NodeKind::Constant | NodeKind::Variable
                )
                || !swift_target_visible(view.file(node.file()).path(), self.path)
            {
                continue;
            }
            let shares = shares_receiver_word(last, owner, node.qname());
            if std_name && !shares && self.swift_project_type(owner) {
                continue;
            }
            let owns = named && !last.is_empty() && flat_word(last) == flat_word(owner);
            tiers[if owns {
                0
            } else if named && shares {
                1
            } else {
                2
            }]
            .push(n);
        }
        if !tiers[0].is_empty() {
            let declared = |n: &NodeIdx| view.node(*n).parent().is_some_and(|p| self.is_swift_extension(p));
            return tiers[0].iter().copied().min_by_key(|n| (declared(n), view.file(view.hot(*n).file).path(), view.node(*n).start_line()));
        }
        let [_, words, rest] = tiers;
        // a variable that neither names nor shares a word with an owner says nothing of its type:
        // only a name one member carries (upstream's unique-name match)
        let tier = if !words.is_empty() {
            words
        } else if named && rest.len() > 1 {
            return None;
        } else {
            rest
        };
        self.nearest_by_name(r, tier.into_iter())
    }

    /// Whether the project declares a Swift type of that name (not just extends an outside one).
    fn swift_project_type(&self, name: &str) -> bool {
        self.view
            .by_name(name)
            .any(|n| kind_of(self.view, n).is_type() && self.view.file(self.view.hot(n).file).lang() == Lang::Swift && !self.is_swift_extension(n))
    }

    /// A Swift call to an enum case (`AFError.invalidURL(url:)`) or, through an implicit
    /// member, to a property (`.failure(error)` is the standard `Result`'s case, not a
    /// project extension's `failure` property) reaches no callable; nor does an implicit member
    /// some enum has a case of (`.image(type)` is as likely `Path.image` as a static `image`).
    fn swift_call_target(&self, o: Outcome, name: &str) -> Outcome {
        if self.family != LangFamily::Swift || o.status != status::RESOLVED {
            return o;
        }
        let view = self.view;
        let k = kind_of(view, NodeIdx(o.target));
        let case = |m: &str| view.by_name(m).any(|n| kind_of(view, n) == NodeKind::EnumMember);
        if k == NodeKind::EnumMember || name.strip_prefix('.').is_some_and(|m| matches!(k, NodeKind::Property | NodeKind::Field) || case(m)) {
            return Outcome::st(status::NO_CANDIDATE);
        }
        o
    }

    /// A bare or `self.` Swift call among overloads its type declares in the caller's file: the
    /// one nearest the call by line, as upstream ranks any name match (`findBestMatch`, its
    /// line score fading out 200 lines away), before the labels have their say.
    fn swift_nearest_sibling(&self, o: Outcome, r: &RefRec) -> Outcome {
        if self.family != LangFamily::Swift || o.status != status::RESOLVED {
            return o;
        }
        let view = self.view;
        let t = NodeIdx(o.target);
        let node = view.node(t);
        if view.hot(t).file != self.file || !matches!(node.kind(), NodeKind::Method | NodeKind::Function) {
            return o;
        }
        let owner = node.qname().rsplit_once("::").map_or("", |(p, _)| p);
        let mut sibs: SmallVec<[NodeIdx; 4]> = view
            .by_name(node.name())
            .filter(|&n| {
                let m = view.node(n);
                view.hot(n).file == self.file
                    && matches!(m.kind(), NodeKind::Method | NodeKind::Function)
                    && m.qname().rsplit_once("::").map_or("", |(p, _)| p) == owner
            })
            .collect();
        sibs.sort_by_key(|&n| view.node(n).start_line());
        let near = |n: NodeIdx| (20.0 - (view.node(n).start_line() as f64 - r.line as f64).abs() / 10.0).max(0.0);
        let mut best = t;
        let mut score = f64::MIN;
        for n in sibs {
            if near(n) > score {
                score = near(n);
                best = n;
            }
        }
        Outcome::found(best, o.by, o.conf)
    }

    /// Swift overloads by argument label: a target whose parameters the call's labels do not fit
    /// gives way to the one same-owner overload in its file they do fit
    /// (upstream `retargetSelfOverload`).
    fn swift_label_overload(&self, o: Outcome, args: Option<&CallArgs<'v>>) -> Outcome {
        let view = self.view;
        let Some(a) = args.filter(|_| self.family == LangFamily::Swift && o.status == status::RESOLVED) else { return o };
        let t = NodeIdx(o.target);
        let node = view.node(t);
        if !matches!(node.kind(), NodeKind::Method | NodeKind::Function) {
            return o;
        }
        let fits = |n: NodeIdx| view.node(n).signature().map(|s| overload::swift_labels_fit(&a.shapes, s));
        if fits(t) != Some(false) {
            return o;
        }
        let owner = node.qname().rsplit_once("::").map_or("", |(p, _)| p);
        let fit: SmallVec<[NodeIdx; 2]> = view
            .by_name(node.name())
            .filter(|&n| {
                let m = view.node(n);
                n != t
                    && view.hot(n).file == view.hot(t).file
                    && matches!(m.kind(), NodeKind::Method | NodeKind::Function)
                    && m.qname().rsplit_once("::").map_or("", |(p, _)| p) == owner
                    && fits(n) == Some(true)
            })
            .collect();
        match fit.as_slice() {
            [only] => Outcome::found(*only, o.by, o.conf),
            _ => o,
        }
    }

    /// A C++ function's overloads are every function of its qualified name the caller's file can
    /// see: the arguments pick among them, `chosen` on a tie.
    fn cpp_function_overload(&self, chosen: NodeIdx, args: Option<&CallArgs<'v>>) -> NodeIdx {
        let view = self.view;
        let Some(a) = args.filter(|_| self.lang == Lang::Cpp && kind_of(view, chosen) == NodeKind::Function) else { return chosen };
        let mut set: SmallVec<[NodeIdx; 4]> = view
            .by_qname(view.node(chosen).qname())
            .filter(|&n| kind_of(view, n) == NodeKind::Function && (view.hot(n).file == self.file || self.e.includes_transitively(self.file, view.hot(n).file)))
            .collect();
        set.sort_by_key(|&n| (n != chosen, view.hot(n).file != self.file, view.file(view.hot(n).file).path(), view.node(n).start_line()));
        self.cpp_overload(&set, &a.shapes).unwrap_or(chosen)
    }

    /// C++ overloads, in preference order: the first the arguments fit best.
    fn cpp_overload(&self, cands: &[NodeIdx], shapes: &[&str]) -> Option<NodeIdx> {
        cands.iter().rev().copied().max_by_key(|c| overload::cpp_fit(self.view.node(*c).signature(), shapes))
    }

    pub fn resolve_call_name(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str) -> Outcome {
        if self.lang == Lang::Cpp {
            if let Some((path, arity)) = name.rsplit_once('/').and_then(|(p, a)| Some((p, a.parse().ok()?))) {
                return self.cpp_constructor_ref(wcx, path, arity);
            }
        }
        let normalized = normalize_call_name(self.family, name);
        let name = normalized.as_ref();
        let args = CallArgs::of(self.view, r);
        let o = if self.family != LangFamily::Rust || !name.contains("self") {
            self.call_name_with(wcx, r, name, args.as_ref())
        } else {
            // `self.state.lock()`: `self` is the type the calling method belongs to
            let previous = wcx.self_ty.replace(self.enclosing_type(r.from).unwrap_or(NodeIdx(NONE)));
            if wcx.self_ty == Some(NodeIdx(NONE)) {
                wcx.self_ty = None;
            }
            let result = self.call_name_with(wcx, r, name, args.as_ref());
            wcx.self_ty = previous;
            result
        };
        if self.family == LangFamily::Js
            && Self::is_call(r)
            && o.status == status::RESOLVED
            && matches!(kind_of(self.view, NodeIdx(o.target)), NodeKind::Interface | NodeKind::TypeAlias)
        {
            return Outcome::st(status::NO_CANDIDATE);
        }
        if self.family == LangFamily::Swift {
            let o = self.swift_call_target(o, name);
            let own = match parse_shape(name) {
                Shape::Bare(_) => true,
                Shape::This { fields, .. } => fields.is_empty(),
                _ => false,
            };
            let o = if own { self.swift_nearest_sibling(o, r) } else { o };
            return self.swift_label_overload(o, args.as_ref());
        }
        o
    }

    fn call_name_with(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        if let Some((receiver, method)) = name.strip_prefix(SWIFT_CHAIN).and_then(|c| c.rsplit_once('.').or(Some(("", c)))) {
            return match self.swift_chained(r, receiver, method, false) {
                Some(n) => Outcome::found(n, by::GLOBAL, 40),
                None => Outcome::st(status::NO_CANDIDATE),
            };
        }
        // Swift `$0.m()`, PHP `$fn()` callables held in variables or closure parameters
        if self.family == LangFamily::Swift && name.starts_with('$') && name[1..].starts_with(|c: char| c.is_ascii_digit()) {
            return Outcome::st(status::LOCAL_BINDING);
        }
        if (self.family == LangFamily::Php && name.contains('\\') && !name.contains(['.', ':'])) || (self.family == LangFamily::C && name.starts_with("::")) {
            return self.qualified_free_call(r, name);
        }
        match parse_shape(name) {
            Shape::Bare(n) if self.swift_extended_builtin(n, r) => Outcome::st(status::BUILTIN),
            Shape::Bare(n) => self.call_bare(wcx, r, n, Accept::Callable, args),
            Shape::Member { root, path, method } => {
                let o = self.js_first_overload(self.call_member(wcx, r, root, &path, method, args));
                if o.status == status::RESOLVED || !path.is_empty() {
                    return o;
                }
                self.js_required_export(r, root, method).map_or(o, |n| Outcome::found(n, by::IMPORT, 90))
            }
            Shape::This { kw, fields, method } if self.family == LangFamily::Js && kw != "this" => self.call_member(wcx, r, kw, &fields, method, args),
            Shape::This { kw, fields, method } => self.call_this(wcx, r, kw, &fields, method, args),
            Shape::Super { method } => self.call_super(wcx, r, method, args),
            Shape::New { class, method } => match self.lookup_type_at(wcx, class, r.line) {
                TypeRes::Node(t) => self.member_call(wcx, t, &[], method, 90, args.map(|a| (a, r))),
                TypeRes::External => Outcome::st(status::EXTERNAL),
                _ => match self.es5_constructor(wcx, class).filter(|_| self.family == LangFamily::Js) {
                    Some(c) => self.member_call(wcx, c, &[], method, 85, args.map(|a| (a, r))),
                    None => Outcome::st(status::NO_CANDIDATE),
                },
            },
            Shape::CallResult { inner, method } => self.js_first_overload(self.call_result(wcx, r, inner, method, args)),
            Shape::ValueResult { inner, method } => {
                let value = self.expression_value(wcx, inner, self.scope_at(r.line, r.col), r.line, 0);
                match value.and_then(|v| v.method_receiver(method)) {
                    Some(t) => self.js_first_overload(self.member_call(wcx, t, &[], method, 85, args.map(|a| (a, r)))),
                    None => Outcome::st(status::NO_CANDIDATE),
                }
            }
        }
    }

    // ------------------------------------------------------------ reference kinds
    fn resolve_type_ref(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str, accept: Accept) -> Outcome {
        if let Some(c) = self.view.list(r.candidates).first() {
            // walkers sometimes hand over qualified candidates
            if let Some(n) = self.view.by_qname(c).next() {
                return Outcome::found(n, by::QUALIFIED, 90);
            }
        }
        let scope = self.scope_at(r.line, r.col);
        let res = if let Some((head, rest)) = split_head(name) {
            match self.qualified_type(wcx, head, rest, name) {
                TypeRes::Node(n) => NameRes::Found(n, by::QUALIFIED, 90),
                TypeRes::External => NameRes::External,
                TypeRes::Builtin => NameRes::Builtin,
                // `fmt::detail::buffer<T>` with `fmt` opened by a macro
                TypeRes::Unknown if self.family == LangFamily::C => {
                    self.cpp_scoped(name, true).map_or(NameRes::NotFound, |n| NameRes::Found(n, by::QUALIFIED, 90))
                }
                TypeRes::Unknown => NameRes::NotFound,
            }
        } else if self.type_param(name, scope) {
            NameRes::Local
        } else if let Some(n) = self.cpp_nested_type(r.from, name) {
            NameRes::Found(n, by::RECEIVER, 95)
        } else {
            let first = self.resolve_name(wcx, name, accept, scope, r.line, Some(r.from), true);
            // `Self` and friends; Swift's `Self` is the dynamic type (a subclass, the conforming
            // type in a protocol extension), not the declaration it is written in
            match (first, name) {
                (NameRes::NotFound | NameRes::Builtin, "Self") if self.family != LangFamily::Swift => match self.enclosing_type(r.from) {
                    Some(t) => NameRes::Found(t, by::RECEIVER, 95),
                    None => first,
                },
                _ => first,
            }
        };
        match res {
            NameRes::Found(n, b, c) => Outcome::found(n, b, c),
            NameRes::External | NameRes::Module(_) | NameRes::Pkg(_) => Outcome::st(status::EXTERNAL),
            NameRes::Builtin => Outcome::st(status::BUILTIN),
            NameRes::Local => Outcome::st(status::LOCAL_BINDING),
            NameRes::Ambiguous(k) => Outcome::ambiguous(k),
            NameRes::NotFound => Outcome::st(status::NO_CANDIDATE),
        }
    }

    /// Framework rule for a route's handler that no scope or import names: the method the
    /// route decorates (`@Get() list()`, `@GetMapping get()`), declared right after the
    /// decorator in the route's file. A bare method name is invisible to lexical lookup, but a
    /// decorator route sits on the method it routes to.
    fn route_handler(&self, r: &RefRec, name: &str, kind: RefKind) -> Option<Outcome> {
        let view = self.view;
        if kind_of(view, r.from) != NodeKind::Route || !matches!(kind, RefKind::FunctionRef | RefKind::Edge(EdgeKind::Calls | EdgeKind::References)) {
            return None;
        }
        let Shape::Bare(name) = parse_shape(name) else { return None };
        let mut after: Option<(u32, NodeIdx)> = None;
        for c in view.by_name(name) {
            let h = view.hot(c);
            let k = NodeKind::from_u8(h.kind).unwrap_or(NodeKind::Variable);
            // stacked decorators and annotations sit between the route and its method
            if h.file != self.file || !matches!(k, NodeKind::Method | NodeKind::Function) || h.start_line < r.line || h.start_line > r.line + 12 {
                continue;
            }
            if after.is_none_or(|(l, n)| (h.start_line, c) < (l, n)) {
                after = Some((h.start_line, c));
            }
        }
        Some(Outcome::found(after?.1, by::FRAMEWORK, 92))
    }

    fn file_node(&self, f: FileId) -> NodeIdx {
        self.view.file(f).rec.node_start
    }

    fn bound_outcome(&self, b: Bound) -> Outcome {
        match b {
            Bound::Node(n) => Outcome::found(n, by::IMPORT, 95),
            Bound::Module(m) => Outcome::found(self.file_node(m), by::IMPORT, 95),
            Bound::Pkg(p) => {
                let pk = &self.e.modules.pkgs[p.0 as usize];
                match pk.files.first() {
                    Some(f) => Outcome::found(self.file_node(*f), by::IMPORT, 70),
                    None => Outcome::st(status::NO_CANDIDATE),
                }
            }
            Bound::External => Outcome::st(status::EXTERNAL),
            Bound::Unknown => Outcome::st(status::NO_CANDIDATE),
        }
    }

    /// C# `using A.B;` names the namespace, not one of the files that declare it. A namespace
    /// has a node per declaration: the first by path stands for it.
    fn namespace_node(&self, name: &str) -> Option<Outcome> {
        let view = self.view;
        if self.family != LangFamily::CSharp {
            return None;
        }
        let n = view
            .by_qname(name)
            .filter(|&n| kind_of(view, n) == NodeKind::Namespace)
            .min_by_key(|&n| (view.file(view.hot(n).file).path(), view.node(n).start_line()))?;
        Some(Outcome::found(n, by::IMPORT, 95))
    }

    /// C#: a bare name reaches a member only through the enclosing type (or a `using static`).
    /// A same-named member elsewhere is no match (`Task` is not `Unit.Task`), and neither is the
    /// member a reference sits on (`public Type Type { get; }` names the type).
    fn stray_csharp_member(&self, o: Outcome, r: &RefRec) -> bool {
        if self.family != LangFamily::CSharp || o.status != status::RESOLVED {
            return false;
        }
        let view = self.view;
        let t = NodeIdx(o.target);
        t == r.from
            || (!matches!(o.by, by::RECEIVER | by::IMPORT)
                && matches!(
                    kind_of(view, t),
                    NodeKind::Method | NodeKind::Property | NodeKind::Field | NodeKind::Constant | NodeKind::Variable | NodeKind::EnumMember
                )
                && view.node(t).parent().is_some_and(|p| kind_of(view, p).is_type()))
    }

    fn resolve_import_ref(&self, name: &str, r: &RefRec) -> Outcome {
        let Some(idxs) = self.imports_by_line.get(&r.line) else { return self.continued_import_ref(name, r) };
        let view = self.view;
        let tgt = |i: u32| self.e.imp_targets[self.file.idx()].get(i as usize).copied().unwrap_or(ImportTarget::External);
        // Rust `crate`, `self`, `super` (alone in `use super::Addr`, or a path's root) name this
        // crate's own modules, which the path's bound names already reach: no dependency
        if self.family == LangFamily::Rust && matches!(name, "crate" | "self" | "super") {
            return Outcome::st(status::BUILTIN);
        }
        // 1. the module specifier itself (C facts keep `<..>` around angle includes and Ruby
        // facts prefix `require_relative` specs with `./`; the kernel's reference names carry
        // neither, and Ruby also emits the `.rb` path form)
        for &i in idxs {
            let imp = &self.imports[i as usize];
            let spec = view.text_str(imp.spec);
            let same = spec == name
                || match self.family {
                    LangFamily::C => spec.strip_prefix('<').and_then(|s| s.strip_suffix('>')).is_some_and(|s| s == name),
                    LangFamily::Ruby => {
                        let plain = spec.strip_prefix("./").unwrap_or(spec);
                        plain == name
                            || name.strip_suffix(".rb").is_some_and(|n| n == plain)
                            || matches!(tgt(i), ImportTarget::File(tf) if view.file(tf).path() == name)
                    }
                    _ => false,
                };
            if same {
                return match tgt(i) {
                    ImportTarget::File(tf) => Outcome::found(self.file_node(tf), by::IMPORT, 95),
                    ImportTarget::Node(n) => Outcome::found(n, by::IMPORT, 95),
                    ImportTarget::Pkg(p) => self.namespace_node(name).unwrap_or_else(|| self.bound_outcome(Bound::Pkg(p))),
                    ImportTarget::External => Outcome::st(status::EXTERNAL),
                    ImportTarget::NotFound => Outcome::st(status::NO_CANDIDATE),
                };
            }
        }
        // 2. a name bound by the import
        if let Some(o) = self.import_bound_name(name, idxs) {
            return o;
        }
        // 3. Rust: the root crate / module segment of a `use` path
        if self.family == LangFamily::Rust {
            for &i in idxs {
                let imp = &self.imports[i as usize];
                let spec = view.text_str(imp.spec);
                if spec.split("::").next() == Some(name) {
                    return match tgt(i) {
                        ImportTarget::External => Outcome::st(status::EXTERNAL),
                        _ => {
                            let first = self.e.modules.resolve_import(self.lang, name, self.file);
                            match first {
                                ImportTarget::File(tf) => Outcome::found(self.file_node(tf), by::IMPORT, 90),
                                _ => Outcome::st(status::EXTERNAL),
                            }
                        }
                    };
                }
            }
        }
        Outcome::st(status::NO_CANDIDATE)
    }

    fn import_bound_name(&self, name: &str, idxs: &[u32]) -> Option<Outcome> {
        let view = self.view;
        idxs.iter().find_map(|&i| {
            let imp = &self.imports[i as usize];
            let local = view.text_str(imp.local);
            let spec = view.text_str(imp.spec);
            let imported = view.text_str(imp.imported);
            // a JS re-export (`export { a as b } from 'm'`) binds no local: the reference names `a`
            let reexport = local.is_empty() && self.family == LangFamily::Js && imported == name;
            let bound = (!local.is_empty() && local == name) || reexport || format!("{spec}::{imported}") == name || format!("{spec}.{imported}") == name;
            if !bound {
                return None;
            }
            // JS `import x from "./m"` names the module, as upstream does; calls of `x` still
            // reach its default export
            if self.family == LangFamily::Js && imp.kind == ImportKind::Default as u8 && imported == "default" {
                if let Some(ImportTarget::File(tf)) = self.e.imp_targets[self.file.idx()].get(i as usize) {
                    return Some(Outcome::found(self.file_node(*tf), by::IMPORT, 95));
                }
            }
            Some(self.js_first_twin(self.bound_outcome(self.e.bind_import(self.file, i as usize))))
        })
    }

    /// TS `doc.write(..)` on a typed receiver: the method's first declaration stands for its
    /// overload signatures, as upstream links it (`this.write(..)` keeps the overload that fits).
    fn js_first_overload(&self, o: Outcome) -> Outcome {
        // overload signatures are TS; plain JS's repeated method is a redefinition, the last wins
        if !matches!(self.lang, Lang::TypeScript | Lang::Tsx) || o.status != status::RESOLVED || kind_of(self.view, NodeIdx(o.target)) != NodeKind::Method {
            return o;
        }
        let view = self.view;
        let m = view.node(NodeIdx(o.target));
        view.by_name(m.name())
            .filter(|&n| view.node(n).parent() == m.parent() && kind_of(view, n) == NodeKind::Method)
            .min_by_key(|&n| (view.node(n).start_line(), view.node(n).start_col()))
            .map_or(o, |n| Outcome::found(n, o.by, o.conf))
    }

    /// CommonJS `var express = require('./express'); express.Router()`: the variable the module
    /// exports under that name (`exports.Router = Router`).
    fn js_required_export(&self, r: &RefRec, root: &str, method: &str) -> Option<NodeIdx> {
        if self.family != LangFamily::Js {
            return None;
        }
        let i = self.visible_import(root, self.scope_at(r.line, r.col))?;
        if self.imports[i as usize].kind != ImportKind::Require as u8 {
            return None;
        }
        let Some(ImportTarget::File(tf)) = self.e.imp_targets[self.file.idx()].get(i as usize).copied() else { return None };
        let view = self.view;
        let variable = |n: NodeIdx| matches!(kind_of(view, n), NodeKind::Variable | NodeKind::Constant);
        if let Some(Bound::Node(n)) = self.e.lookup_export(tf, method, 0) {
            return variable(n).then_some(n);
        }
        // `var Router = require('router'); exports.Router = Router` forwards a dependency: the
        // variable holding it is what the project can link. The file may sit behind a package
        // root's `module.exports = require('./lib/express')`.
        let mut files = vec![tf];
        let mut i = 0;
        while i < files.len() && files.len() < 8 {
            for &s in &self.e.stars[files[i].idx()] {
                if !files.contains(&s) {
                    files.push(s);
                }
            }
            i += 1;
        }
        files.into_iter().find_map(|f| {
            let spec_of = |name: &str| view.imports_of(f).iter().find(|i| view.text_str(i.local) == name).map(|i| view.text_str(i.spec));
            let forwarded = view.exports_of(f).iter().any(|e| view.text_str(e.name) == method && spec_of(method) == Some(view.text_str(e.from_spec)));
            self.e.lookup_top_level(f, method).filter(|&n| forwarded && variable(n))
        })
    }

    /// JS `interface X` and `const X` in one file: importing `X` names whichever comes first
    /// (as upstream does); `new X()` and calls still take the value.
    fn js_first_twin(&self, o: Outcome) -> Outcome {
        if self.family != LangFamily::Js || o.status != status::RESOLVED {
            return o;
        }
        let view = self.view;
        let t = view.node(NodeIdx(o.target));
        let is_type = |n| matches!(kind_of(view, n), NodeKind::Interface | NodeKind::TypeAlias | NodeKind::Class | NodeKind::Enum);
        let is_value = |n| matches!(kind_of(view, n), NodeKind::Variable | NodeKind::Constant | NodeKind::Function);
        let twins: Vec<_> = view.by_name(t.name()).filter(|&n| view.hot(n).file == t.file() && view.node(n).parent() == t.parent()).collect();
        if !(twins.iter().any(|&n| is_type(n)) && twins.iter().any(|&n| is_value(n))) {
            return o;
        }
        twins.into_iter().min_by_key(|&n| (view.node(n).start_line(), view.node(n).start_col())).map_or(o, |n| Outcome::found(n, o.by, o.conf))
    }

    /// A name on a continuation line of a multi-line import (`from m import (\n    a,\n)`,
    /// `import {\n  a,\n} from "m"`, `use m::{\n    a,\n}`): the facts carry the statement's first
    /// line, the nearest import line above the reference.
    fn continued_import_ref(&self, name: &str, r: &RefRec) -> Outcome {
        let start = self.imports.iter().map(|i| i.line).filter(|&l| l < r.line).max();
        start.and_then(|l| self.import_bound_name(name, &self.imports_by_line[&l])).unwrap_or_else(|| Outcome::st(status::NO_CANDIDATE))
    }

    /// TypeScript `interface ZodUnknown` beside `const ZodUnknown`: `new ZodUnknown()` makes the
    /// value; the type only describes it.
    fn js_value_twin(&self, o: Outcome) -> Outcome {
        let (view, target) = (self.view, NodeIdx(o.target));
        if self.family != LangFamily::Js || o.status != status::RESOLVED || !matches!(kind_of(view, target), NodeKind::Interface | NodeKind::TypeAlias) {
            return o;
        }
        let ty = view.node(target);
        let twin = view.by_name(ty.name()).find(|&n| {
            view.hot(n).file == ty.file()
                && view.node(n).parent() == ty.parent()
                && matches!(kind_of(view, n), NodeKind::Variable | NodeKind::Constant | NodeKind::Function | NodeKind::Class)
        });
        twin.map_or(o, |n| Outcome::found(n, o.by, o.conf))
    }

    /// JS `const $ = django.jQuery; $(el)`, `Router = express.Router; new Router()`: a call of a
    /// name a `var` / `let` / `const` declares runs what that declaration's node holds.
    fn js_declared_value(&self, r: &RefRec, name: &str) -> Option<NodeIdx> {
        if self.family != LangFamily::Js || !name.chars().all(|c| c == '$' || c == '_' || c.is_alphanumeric()) {
            return None;
        }
        let b = self.lookup_binding(name, self.scope_at(r.line, r.col), r.line, false).filter(|b| b.kind == BindKind::Local as u8)?;
        let view = self.view;
        view.by_name(name).find(|&n| {
            view.hot(n).file == self.file && view.node(n).start_line() == b.line && matches!(kind_of(view, n), NodeKind::Variable | NodeKind::Constant)
        })
    }

    /// Resolve one reference. `name` is the reference's text.
    pub fn resolve_ref(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec) -> Outcome {
        let name = self.view.text_str(r.name);
        let Some(kind) = RefKind::from_wire(r.kind) else { return Outcome::st(status::NO_CANDIDATE) };
        let o = self.resolve_kind(wcx, r, name, kind);
        // a name-only guess for an untyped receiver is never the caller itself:
        // `std.sync_all()` inside `File::sync_all` calls the std file's method (dynamic
        // languages do recurse through untyped children: `child.relabel(..)`)
        let typed = matches!(self.family, LangFamily::Rust | LangFamily::Jvm | LangFamily::Go);
        if typed && kind == RefKind::Edge(EdgeKind::Calls) && o.status == status::RESOLVED && o.by == by::GLOBAL && o.target == r.from.0 {
            return Outcome::st(status::NO_CANDIDATE);
        }
        if o.status == status::LOCAL_BINDING && matches!(kind, RefKind::Edge(EdgeKind::Calls | EdgeKind::Instantiates)) {
            if let Some(n) = self.js_declared_value(r, name) {
                return Outcome::found(n, by::LEXICAL, 90);
            }
        }
        // an extension no project declaration answers extends a dependency's type
        if o.status == status::RESOLVED && self.is_swift_extension(NodeIdx(o.target)) {
            return Outcome::st(status::EXTERNAL);
        }
        if o.status == status::NO_CANDIDATE {
            if let Some(found) = self.route_handler(r, name, kind) {
                return found;
            }
        }
        o
    }

    /// Whether `n` is declared inside `outer`.
    fn within(&self, n: NodeIdx, outer: NodeIdx) -> bool {
        let mut cur = self.view.node(n).parent();
        while let Some(p) = cur {
            if p == outer {
                return true;
            }
            cur = self.view.node(p).parent();
        }
        false
    }

    /// A supertype named `name` that is not one of `decl`'s own members: an import, the
    /// package, then another type of this file.
    fn supertype_outside(&self, name: &str, decl: NodeIdx) -> Option<NodeIdx> {
        let view = self.view;
        let is_type = |n: NodeIdx| kind_of(view, n).is_type() && !self.within(n, decl) && n != decl;
        if let Some(i) = self.visible_import(name, 0) {
            if let Bound::Node(n) = self.e.bind_import(self.file, i as usize) {
                return is_type(n).then_some(n);
            }
        }
        if let Some(list) = self.e.pkg_defs.get(self.pkg.0 as usize).and_then(|m| m.get(name)) {
            let hits: SmallVec<[NodeIdx; 2]> = list.iter().copied().filter(|n| is_type(*n)).collect();
            if let Some(n) = self.nearest_copy(&hits).or_else(|| hits.first().copied()) {
                return Some(n);
            }
        }
        view.by_name(name).find(|n| view.hot(*n).file == self.file && is_type(*n))
    }

    fn resolve_kind(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str, kind: RefKind) -> Outcome {
        match kind {
            RefKind::Edge(EdgeKind::Imports) => self.resolve_import_ref(name, r),
            // `util.inherits(Child, Parent)`: an ES5 constructor extends another constructor
            RefKind::Edge(EdgeKind::Extends) if self.family == LangFamily::Js => {
                let o = self.resolve_type_ref(wcx, r, name, Accept::Type);
                if o.status != status::NO_CANDIDATE {
                    return o;
                }
                let c = self.resolve_type_ref(wcx, r, name, Accept::Callable);
                if c.status == status::RESOLVED && kind_of(self.view, NodeIdx(c.target)) == NodeKind::Function {
                    c
                } else {
                    o
                }
            }
            RefKind::Edge(EdgeKind::Extends | EdgeKind::Implements) => {
                let o = self.resolve_type_ref(wcx, r, name, Accept::Type);
                // `class C implements Builder` cannot mean C's own nested `Builder`:
                // a supertype clause is outside the body that declares C's members
                if o.status == status::RESOLVED && self.within(NodeIdx(o.target), r.from) {
                    return match self.supertype_outside(name, r.from) {
                        Some(n) => Outcome::found(n, by::PACKAGE, 85),
                        None => Outcome::st(status::NO_CANDIDATE),
                    };
                }
                o
            }
            RefKind::Edge(EdgeKind::TypeOf | EdgeKind::Returns) => self.resolve_type_ref(wcx, r, name, Accept::Type),
            RefKind::Edge(EdgeKind::Instantiates) if self.swift_extended_builtin(name, r) => Outcome::st(status::BUILTIN),
            // JS `new Class(..)` where `Class` is a parameter or a local: the value it holds, never a
            // same-named class elsewhere (type lookup does not see lexical bindings)
            RefKind::Edge(EdgeKind::Instantiates)
                if self.family == LangFamily::Js
                    && self
                        .lookup_binding(name, self.scope_at(r.line, r.col), r.line, false)
                        .is_some_and(|b| b.kind == BindKind::Param as u8 || b.kind == BindKind::Local as u8) =>
            {
                self.resolve_type_ref(wcx, r, name, Accept::Callable)
            }
            RefKind::Edge(EdgeKind::Instantiates) => {
                let o = self.resolve_type_ref(wcx, r, name, Accept::Type);
                if o.status == status::NO_CANDIDATE {
                    self.resolve_type_ref(wcx, r, name, Accept::Callable)
                } else {
                    let o = self.js_value_twin(o);
                    // `new File([], name)` with only `interface File` in reach: a global's value (the
                    // DOM's), since an interface makes none
                    let bare_type = o.status == status::RESOLVED && matches!(kind_of(self.view, NodeIdx(o.target)), NodeKind::Interface | NodeKind::TypeAlias);
                    if self.family == LangFamily::Js && bare_type {
                        Outcome::st(status::NO_CANDIDATE)
                    } else {
                        o
                    }
                }
            }
            RefKind::Edge(EdgeKind::References) => {
                let o = self.resolve_type_ref(wcx, r, name, Accept::Type);
                let o = if matches!(o.status, status::NO_CANDIDATE) {
                    let any = self.resolve_type_ref(wcx, r, name, Accept::Any);
                    if self.stray_csharp_member(any, r) {
                        o
                    } else {
                        any
                    }
                } else {
                    o
                };
                // `errorUtil.ErrMessage`: the kernel flattens a TS `namespace`'s members into its file,
                // where no path walk enters; the last segment, as the kernel names it
                match name.rsplit_once('.') {
                    Some((path, tail)) if o.status == status::NO_CANDIDATE && self.family == LangFamily::Js => match self.lookup_type(wcx, path) {
                        // `typeName: ZodFirstPartyTypeKind.ZodLiteral` names the enum's member, not `class ZodLiteral`
                        TypeRes::Node(e) if kind_of(self.view, e) == NodeKind::Enum => {
                            self.e.direct_member(e, tail).map_or(o, |m| Outcome::found(m, by::QUALIFIED, 90))
                        }
                        _ => self.resolve_kind(wcx, r, tail, kind),
                    },
                    _ => o,
                }
            }
            RefKind::FunctionRef => self.resolve_call_name(wcx, r, name),
            RefKind::Edge(EdgeKind::Calls | EdgeKind::Decorates | EdgeKind::Navigates | EdgeKind::Overrides | EdgeKind::Exports | EdgeKind::Contains) => {
                self.resolve_call_name(wcx, r, name)
            }
        }
    }
}

/// Methods of Swift's standard collections, strings and optionals and of Foundation / UIKit
/// views: a project type rarely carries them itself (upstream `SWIFT_STD_METHODS`).
const SWIFT_STD_METHODS: &[&str] = &[
    "append",
    "insert",
    "remove",
    "removeAll",
    "removeFirst",
    "removeLast",
    "removeValue",
    "contains",
    "map",
    "compactMap",
    "flatMap",
    "filter",
    "reduce",
    "forEach",
    "sorted",
    "sort",
    "first",
    "last",
    "min",
    "max",
    "firstIndex",
    "lastIndex",
    "index",
    "enumerated",
    "reversed",
    "joined",
    "split",
    "prefix",
    "suffix",
    "dropFirst",
    "dropLast",
    "allSatisfy",
    "randomElement",
    "shuffled",
    "popLast",
    "replaceSubrange",
    "replacingOccurrences",
    "components",
    "trimmingCharacters",
    "hasPrefix",
    "hasSuffix",
    "lowercased",
    "uppercased",
    "appending",
    "updateValue",
    "merge",
    "merging",
    "union",
    "intersection",
    "subtracting",
    "formUnion",
    "isEqual",
    "addSubview",
    "removeFromSuperview",
    "setNeedsDisplay",
    "setNeedsLayout",
    "layoutIfNeeded",
    "addGestureRecognizer",
    "addTarget",
    "addObserver",
    "removeObserver",
    "eraseToAnyPublisher",
];

/// A Swift declaration under a `...Tests` directory or a playground is seen only from inside it.
fn swift_target_visible(decl: &str, from: &str) -> bool {
    let segs: Vec<&str> = decl.split('/').collect();
    let dirs = &segs[..segs.len().saturating_sub(1)];
    match dirs.iter().position(|s| s.ends_with("Tests") || s.ends_with(".playground")) {
        Some(i) => from.starts_with(&format!("{}/", dirs[..=i].join("/"))),
        None => true,
    }
}

/// Whether a receiver's name is the owner's, or shares a word of three letters or more with
/// the owner's qualified name (`sessionManager` and `Session`).
/// A name's letters and digits, lowercased, a trailing number dropped: `executor1` is `executor`.
fn flat_word(w: &str) -> String {
    w.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().trim_end_matches(|c: char| c.is_ascii_digit()).to_ascii_lowercase()
}

fn shares_receiver_word(receiver: &str, owner: &str, qname: &str) -> bool {
    let flat = flat_word;
    if !receiver.is_empty() && flat(receiver) == flat(owner) {
        return true;
    }
    let words = |s: &str| {
        let mut out: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut prev_lower = false;
        for c in s.chars() {
            if !c.is_ascii_alphanumeric() || (prev_lower && c.is_ascii_uppercase()) {
                if !cur.is_empty() {
                    out.push(flat(&cur));
                }
                cur.clear();
            }
            if c.is_ascii_alphanumeric() {
                cur.push(c);
            }
            prev_lower = c.is_ascii_lowercase();
        }
        if !cur.is_empty() {
            out.push(flat(&cur));
        }
        out
    };
    let scope = qname.rsplit_once("::").map_or("", |(s, _)| s);
    let owned: Vec<String> = words(scope).into_iter().filter(|w| w.len() > 2).collect();
    words(receiver).iter().any(|w| owned.contains(w))
}

/// Whether a reference name uses a `::` scope qualifier before its last segment
/// (`ns::f`, `Klass::m`, `Mod::Klass.m`), as opposed to a value receiver (`x.m`).
fn r_name_is_scoped(name: &str) -> bool {
    let head = name.find(['.', '(']).map(|i| &name[..i]).unwrap_or(name);
    head.contains("::")
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn optional_field_cache_releases_capacity_at_its_budget() {
        let mut worker = WorkerCx::new();
        for i in 0..1000 {
            worker.cache_field(NodeIdx(i / 3), &format!("field{i}"), Some(NodeIdx(i)), 1024);
            assert!(worker.field_cache_bytes <= 1024);
        }
        let before = worker.field_cache_bytes;
        worker.cache_field(NodeIdx(0), &"x".repeat(2048), None, 1024);
        assert_eq!(worker.field_cache_bytes, before);
    }
}
