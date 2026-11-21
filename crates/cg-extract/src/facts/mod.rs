//! Facts tables (design doc 6.1): what resolution needs beyond the kernel's tables,
//! taken from the syntax tree the kernel already built (patch P4): import bindings,
//! re-exports, scopes and local bindings with their declared types.

use crate::rows::FileRows;
use cg_core::{text, Lang, NONE};
use tree_sitter::{Node, Tree};

pub mod args;
mod c;
mod csharp;
mod go;
mod java;
pub mod js;
mod kotlin;
mod php;
mod python;
mod ruby;
mod rust;
mod swift;
mod util;
pub use util::Cx;

/// Internal export key for the module value itself, distinct from a named/ES
/// `default` export. The leading NUL keeps it out of ordinary source identifiers.
pub const COMMONJS_MODULE: &str = "\0cjs:module.exports";
/// Prefix of a Swift call the kernel names by its method alone although it is made on some
/// other value: `<prefix><receiver>.<method>` (`session.request(..).validate()`).
pub const SWIFT_CHAIN: &str = "\0chain:";

/// Marks initializer evidence collected through a TypeScript `satisfies` expression.
pub const JS_SATISFIES_VALUE: &str = "\0js:satisfies:";

/// Initializer evidence that names a JavaScript/TypeScript lexical binding.
/// The NUL prefix distinguishes it from a constructor or call path.
pub const JS_BINDING_ALIAS: &str = "\0js:binding:";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ImportKind {
    /// `import X from 'm'`, Python `import m` (binds the module itself)
    Default = 0,
    /// `import { a as b } from 'm'`, `from m import a as b`, `use m::a`, `import a.b.C`
    Named = 1,
    /// `import * as ns from 'm'`, wildcard imports (`from m import *`, `import a.*`, `use m::*`)
    Namespace = 2,
    /// `import 'm'`
    SideEffect = 3,
    /// `const x = require('m')`, `const { a } = require('m')`
    Require = 4,
    /// Java `import static a.b.C.member;` (spec = `a.b.C.member`, imported = `member` or `*`)
    Static = 5,
}

impl ImportKind {
    pub fn from_u8(v: u8) -> ImportKind {
        match v {
            1 => ImportKind::Named,
            2 => ImportKind::Namespace,
            3 => ImportKind::SideEffect,
            4 => ImportKind::Require,
            5 => ImportKind::Static,
            _ => ImportKind::Default,
        }
    }
}

/// One name bound by an import (or a wildcard / side-effect import).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportFact {
    /// Name bound in the file. Empty for side-effect imports; `*` for wildcards.
    pub local: String,
    /// Name inside the module: `default`, `*` (whole module / wildcard) or an export name.
    pub imported: String,
    /// Module specifier as written (TS `./foo`, Python `.sibling`, Go path, Java package or
    /// class path, Rust `crate::util`).
    pub spec: String,
    pub kind: ImportKind,
    pub type_only: bool,
    /// Index into `Facts::scopes` the binding is visible in (0 = file).
    pub scope: u32,
    pub line: u32,
}

/// `export { name } from spec`, `export * from spec`, `export default`, local exports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportFact {
    pub name: String,
    /// Kernel node row of the exported declaration; `NONE` for re-exports.
    pub node_row: u32,
    pub from_spec: String,
    pub from_name: String,
    pub star: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ScopeKind {
    File = 0,
    Class = 1,
    Function = 2,
    Block = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeFact {
    pub kind: ScopeKind,
    /// Parent scope index; `NONE` for the file scope.
    pub parent: u32,
    /// Packed (line, UTF-16 column) of the start / end, comparable with `<`.
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BindKind {
    Param = 0,
    Local = 1,
    LocalFn = 2,
    Field = 3,
    /// The file's declared package (Go `package x`, Java/Kotlin `package a.b`); `name` is the package.
    Package = 4,
    /// Function assigned to a JavaScript object member, rather than a free function.
    /// This is declaration metadata and must not participate in lexical lookup.
    MemberFn = 5,
    /// Return annotation or a bounded inferred return expression, keyed by the
    /// function's node. An empty TS fact records an unsupported explicit
    /// annotation. Metadata only; not a lexical binding.
    ReturnType = 6,
    /// `const local = other`: a name bound to nothing but another name (`init_ctor`). A call
    /// through it is a call of what the other name is.
    Alias = 7,
    /// Rust `impl Deref for X { type Target = Y; }`: `name` is the type as the impl writes it,
    /// `decl_type` the target. Metadata only; not a lexical binding.
    Deref = 8,
    /// A pytest fixture, keyed by the function's node: what it returns, as `ReturnType`.
    /// Metadata only; not a lexical binding.
    Fixture = 9,
    /// Rust `type X = Y;` outside an impl, keyed by the alias's node: `decl_type` is `Y`.
    /// Metadata only; not a lexical binding.
    TypeAlias = 10,
    /// A JS factory returns an object it mixed with an imported module's members.
    /// Metadata only; `decl_type` is the source module specifier.
    FactoryMixin = 11,
    /// A JavaScript name is assigned after declaration. Metadata only.
    Mutation = 12,
    /// A top-level JavaScript object is assigned to `module.exports` in its initializer.
    /// Metadata only; `name` is the local object name.
    CommonJsObject = 13,
    /// A `this.member()` inside a function assigned to a named object property.
    /// Metadata only; `decl_type` identifies that object and `init_ctor` the call column.
    AssignedThisCall = 14,
    /// C++ `#define BEGIN namespace fmt {`: `name` is a namespace opened only by a macro, which
    /// no node of the index stands for. Metadata only; not a lexical binding.
    MacroNamespace = 15,
    /// A type parameter of the Rust fn, impl or trait, or the Java class or method, whose
    /// scope holds it. Only a type reference sees it: `T` in `fn new(value: T)` names no
    /// `struct T` (or Java enum constant `T`) elsewhere.
    TypeParam = 16,
    /// A Rust method of an `impl Trait for X` block, keyed by its node: an inherent `X::m`
    /// shadows it. Metadata only; not a lexical binding.
    TraitImpl = 17,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingFact {
    pub scope: u32,
    pub name: String,
    pub kind: BindKind,
    /// Declared type, usually reduced to its head (`Foo<T>` -> `Foo`). Rust
    /// retains generic arguments to distinguish containers and pointer targets.
    pub decl_type: String,
    /// Constructor, bounded initializer call path, tagged lexical alias, or empty.
    pub init_ctor: String,
    /// 1-based declaration line; the binding is visible from here on (hoisted functions
    /// are visible throughout their scope and use line 0).
    pub line: u32,
    /// Kernel node row if the binding has a node of its own (local functions), else `NONE`.
    pub node_row: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub imports: Vec<ImportFact>,
    pub exports: Vec<ExportFact>,
    /// `scopes[0]` is the file scope.
    pub scopes: Vec<ScopeFact>,
    pub bindings: Vec<BindingFact>,
    /// Precise call paths lost by the compatibility kernel. Applied to matching
    /// call references before storage; node identities and sites stay unchanged.
    pub call_names: Vec<CallNameFact>,
    /// Argument shapes of call sites, for overload selection (see `args`).
    pub call_args: Vec<CallArgsFact>,
    /// Go type declarations whose names stand for other types: true aliases and the type
    /// parameters of generic types (structural interface matching leaves them open).
    pub go_types: Vec<GoTypeFact>,
    /// Go types a struct or interface embeds, as written (`io.Reader`, `*os.File`, `error`).
    pub go_embeds: Vec<GoEmbedFact>,
    /// Go struct fields of a func type, and the values code puts in struct fields.
    pub go_fields: Vec<GoFieldFact>,
    /// A supplementary parse exhausted its deadline. The pipeline exposes this
    /// as an incomplete file rather than silently advertising complete facts.
    pub timed_out: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallNameFact {
    pub line: u32,
    pub col: u32,
    pub tail: String,
    pub kernel_name: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoTypeFact {
    pub name: String,
    /// `type A = B`
    pub alias: bool,
    pub type_params: Vec<String>,
    /// `type HandlerFunc func(...)`: a value of the type is a function.
    pub func_type: bool,
}

/// One embedded type of a Go struct or interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoEmbedFact {
    /// Kernel node row of the embedding type.
    pub row: u32,
    /// The package qualifier as written (`io` in `io.Reader`), empty for none.
    pub qualifier: String,
    /// The embedded type's name.
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoFieldFact {
    /// A struct field declared with a literal func type (`refreshf func(ctx) error`).
    FuncField { row: u32, field: String },
    /// A value put in a field by a composite literal: `T{F: v}`, or positional `T{a, v}`
    /// (`field` is then `#index`).
    Literal { ty: String, field: String, value: String, line: u32, col: u32 },
    /// An assignment `x.F = v` / `x.y.F = v`: `receiver` is `x` / `x.y`.
    Assign { receiver: String, field: String, value: String, line: u32, col: u32 },
}

/// The arguments of one call, keyed like the kernel's call reference: its position and the
/// called name's last segment. Stored with the reference (`args::ARGS_MARK` list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallArgsFact {
    pub line: u32,
    pub col: u32,
    pub tail: String,
    /// One shape per argument, in order (see `args`).
    pub args: Vec<String>,
}

impl Facts {
    pub fn new(file_end: u64) -> Facts {
        Facts { scopes: vec![ScopeFact { kind: ScopeKind::File, parent: NONE, start: 0, end: file_end }], ..Default::default() }
    }

    /// Innermost scope containing `pos` (packed line/col).
    pub fn scope_at(&self, pos: u64) -> u32 {
        let mut best = 0u32;
        for (i, s) in self.scopes.iter().enumerate() {
            if s.start <= pos && pos <= s.end && s.start >= self.scopes[best as usize].start {
                best = i as u32;
            }
        }
        best
    }
}

/// Build the facts for one parsed file. `rows` are the kernel tables of the same parse.
pub fn extract(lang: Lang, tree: &Tree, src: &str, rows: FileRows<'_>) -> Facts {
    extract_with_timeout(lang, tree, src, rows, std::time::Duration::from_secs(5))
}

pub fn extract_with_timeout(lang: Lang, tree: &Tree, src: &str, rows: FileRows<'_>, timeout: std::time::Duration) -> Facts {
    let mut cx = Cx::new(src, rows);
    cx.parse_timeout = timeout;
    let root = tree.root_node();
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Jsx => js::extract(&mut cx, root),
        Lang::Python => python::extract(&mut cx, root),
        Lang::Go => go::extract(&mut cx, root),
        Lang::Java => java::extract(&mut cx, root),
        Lang::Rust => rust::extract(&mut cx, root),
        Lang::Kotlin => kotlin::extract(&mut cx, root),
        Lang::C => c::extract(&mut cx, root),
        Lang::Cpp => {
            c::extract(&mut cx, root);
            args::cpp_calls(&mut cx, root);
        }
        Lang::CSharp => csharp::extract(&mut cx, root),
        Lang::Php => php::extract(&mut cx, root),
        Lang::Ruby => ruby::extract(&mut cx, root),
        Lang::Swift => {
            swift::extract(&mut cx, root);
            args::swift_calls(&mut cx, root);
        }
        _ => {}
    }
    cx.finish()
}

#[allow(dead_code)]
fn _keep(_: Node, _: u32) -> u64 {
    text::pack_pos(0, 0)
}
