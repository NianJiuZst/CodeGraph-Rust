//! Facts for Rust: `use` trees flattened to (module path, item) pairs, `pub use` re-exports,
//! function / closure / impl / struct scopes, parameters, `let` bindings and struct fields.

use super::util::*;
use super::*;

pub fn extract(cx: &mut Cx, root: Node) {
    // Conditional item wrappers such as Tokio's cfg_rt! hide `pub use` from
    // tree-sitter. Reparse a position-preserving view with just those wrappers
    // blanked out. Never expand arbitrary macros or change the kernel's nodes.
    if let Some(tree) = conditional_items(cx, root) {
        walk(cx, tree.root_node(), 0);
    } else {
        walk(cx, root, 0);
    }
}

fn conditional_items(cx: &mut Cx, root: Node) -> Option<Tree> {
    use std::time::Instant;
    use tree_sitter::{ParseOptions, Parser};

    let src = cx.src;
    if src.len() > 1024 * 1024 || !src.contains("cfg_") {
        return None;
    }
    fn wrapper(name: &str) -> bool {
        name.starts_with("cfg_") && name != "cfg_if" && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    }
    fn blank(bytes: &mut [u8]) {
        for b in bytes {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
    }
    fn collect(n: Node, src: &str, ranges: &mut Vec<(usize, usize)>, depth: usize) {
        if depth > 128 || n.kind() == "macro_definition" {
            return;
        }
        if matches!(n.kind(), "macro_invocation" | "token_tree") {
            let mut c = n.walk();
            let children: Vec<_> = n.children(&mut c).collect();
            let mut i = 0;
            while i < children.len() {
                if i + 2 < children.len() && children[i + 1].kind() == "!" && children[i + 2].kind() == "token_tree" {
                    let name = &src[children[i].byte_range()];
                    let body = children[i + 2];
                    if wrapper(name) && src.as_bytes().get(body.start_byte()) == Some(&b'{') {
                        ranges.push((children[i].start_byte(), body.start_byte() + 1));
                        ranges.push((body.end_byte() - 1, body.end_byte()));
                        collect(body, src, ranges, depth + 1);
                    }
                    // An unknown macro's tokens are not ordinary Rust items.
                    i += 3;
                } else {
                    collect(children[i], src, ranges, depth + 1);
                    i += 1;
                }
            }
        } else {
            for ch in kids(n) {
                collect(ch, src, ranges, depth + 1);
            }
        }
    }
    let mut ranges = Vec::new();
    collect(root, src, &mut ranges, 0);
    if ranges.is_empty() {
        return None;
    }
    let mut masked = src.as_bytes().to_vec();
    for (start, end) in ranges {
        blank(&mut masked[start..end]);
    }
    // Use the configured parsing budget, not a separate 100 ms cutoff which
    // can silently change imports under normal indexing load.
    if cx.parse_timeout.is_zero() {
        cx.facts.timed_out = true;
        return None;
    }
    thread_local! {
        static RUST_PARSER: std::cell::RefCell<Option<Parser>> = const { std::cell::RefCell::new(None) };
    }
    let deadline = Instant::now() + cx.parse_timeout;
    let mut expired = false;
    let mut steps: u32 = 0;
    let mut cancel = |_: &tree_sitter::ParseState| {
        steps = steps.wrapping_add(1);
        if (steps & 4095) == 0 && Instant::now() >= deadline {
            expired = true;
            true
        } else {
            false
        }
    };
    let tree = RUST_PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut p = Parser::new();
            p.set_language(&cg_kernel::hooks::language("rust")?).ok()?;
            *slot = Some(p);
        }
        let parser = slot.as_mut().unwrap();
        let tree = parser.parse_with_options(
            &mut |offset, _| masked.get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut cancel)),
        );
        if tree.is_none() {
            parser.reset();
        }
        tree
    });
    cx.facts.timed_out |= expired;
    let tree = tree?;
    // Non-item wrappers (cfg_if!, DSLs, malformed syntax) are left opaque.
    (!tree.root_node().has_error()).then_some(tree)
}

fn walk_children(cx: &mut Cx, n: Node, scope: u32) {
    for ch in kids(n) {
        walk(cx, ch, scope);
    }
}

fn walk(cx: &mut Cx, n: Node, scope: u32) {
    match n.kind() {
        "use_declaration" => use_decl(cx, n, scope),
        "function_item" | "function_signature_item" => function(cx, n, scope),
        "closure_expression" => closure(cx, n, scope),
        "impl_item" | "trait_item" => {
            let generics = generics_of(cx, n);
            cx.generics.push(generics);
            let sc = cx.push_scope(ScopeKind::Class, scope, n);
            bind_type_params(cx, sc);
            if n.kind() == "impl_item" {
                deref_impl(cx, n, scope);
                trait_impl(cx, n, sc);
            }
            walk_children(cx, n, sc);
            cx.generics.pop();
        }
        "type_item" if cx.facts.scopes[scope as usize].kind != ScopeKind::Class => {
            // `type Outcome = plugin::Outcome<Config>`: a type whose members are the target's
            let generics = generics_of(cx, n);
            cx.generics.push(generics);
            let target = n.child_by_field_name("type").map(|t| subst_generics(cx, &rust_type_name(cx, t))).unwrap_or_default();
            cx.generics.pop();
            if let (Some(name), Some(row), false) = (n.child_by_field_name("name"), cx.row_at(cx.start(n)), target.is_empty()) {
                let name = cx.text(name).to_owned();
                let line = cx.line(n);
                cx.facts.bindings.push(BindingFact {
                    scope,
                    name,
                    kind: BindKind::TypeAlias,
                    decl_type: target,
                    init_ctor: String::new(),
                    line,
                    node_row: row,
                });
            }
        }
        "struct_item" => {
            let generics = generics_of(cx, n);
            cx.generics.push(generics);
            struct_item(cx, n, scope);
            cx.generics.pop();
        }
        "let_declaration" => {
            let_decl(cx, n, scope);
            walk_children(cx, n, scope);
        }
        "call_expression" => {
            if let Some(f) = n.child_by_field_name("function").filter(|f| f.kind() == "field_expression") {
                if let Some(field) = f.child_by_field_name("field") {
                    let name = value_path(cx, f, 0);
                    if !name.is_empty() {
                        let pos = cx.start(n);
                        let mut kernel_name = String::new();
                        let mut depth = 0u32;
                        for ch in name.chars() {
                            if ch == ')' {
                                depth = depth.saturating_sub(1);
                            }
                            if depth == 0 {
                                kernel_name.push(ch);
                            }
                            if ch == '(' {
                                depth += 1;
                            }
                        }
                        cx.facts.call_names.push(CallNameFact {
                            line: (pos >> 32) as u32,
                            col: pos as u32,
                            tail: cx.text(field).to_owned(),
                            kernel_name,
                            name,
                        });
                    }
                }
            }
            walk_children(cx, n, scope);
        }
        "scoped_identifier" => {
            scoped_receiver(cx, n);
            walk_children(cx, n, scope);
        }
        "for_expression" => {
            if let Some(p) = n.child_by_field_name("pattern") {
                bind_pattern(cx, p, scope, BindKind::Local);
            }
            walk_children(cx, n, scope);
        }
        "let_condition" => {
            if let Some(p) = n.child_by_field_name("pattern") {
                bind_pattern(cx, p, scope, BindKind::Local);
            }
            walk_children(cx, n, scope);
        }
        _ => walk_children(cx, n, scope),
    }
}

/// `mio::Interest::AIO`: the kernel names the receiver of a path's member by its last segment
/// (`Interest`); the path as written keeps a dependency's type apart from a local one.
fn scoped_receiver(cx: &mut Cx, n: Node) {
    let (Some(path), Some(member)) = (n.child_by_field_name("path"), n.child_by_field_name("name")) else { return };
    if path.kind() != "scoped_identifier" || !cx.text(member).starts_with(|c: char| c.is_ascii_uppercase()) {
        return;
    }
    let Some(recv) = path.child_by_field_name("name") else { return };
    let pos = cx.start(recv);
    let recv = cx.text(recv).to_owned();
    let name = cx.text(path).to_owned();
    cx.facts.call_names.push(CallNameFact { line: (pos >> 32) as u32, col: pos as u32, tail: recv.clone(), kernel_name: recv, name });
}

/// Bounds that add nothing a call goes through.
const MARKER_TRAITS: &[&str] =
    &["Clone", "Copy", "Send", "Sync", "Sized", "Debug", "Default", "Eq", "PartialEq", "Hash", "Ord", "PartialOrd", "Display", "Unpin", "Any"];

/// The traits a `trait_bounds` node names, marker traits and lifetimes left out.
fn bound_names(cx: &Cx, b: Node) -> Vec<String> {
    kids(b)
        .filter_map(|c| match c.kind() {
            "type_identifier" | "scoped_type_identifier" => Some(cx.text(c)),
            "generic_type" => c.child_by_field_name("type").map(|t| cx.text(t)),
            _ => None,
        })
        .filter(|b| !MARKER_TRAITS.contains(&b.rsplit("::").next().unwrap_or(b)))
        .map(str::to_owned)
        .collect()
}

/// The type parameters a declaration introduces, with the bounds of its parameter list and its
/// `where` clause: `T: Svc + Clone` -> (`T`, `impl Svc`), an unbounded `U` -> (`U`, `_`).
fn generics_of(cx: &Cx, n: Node) -> Vec<(String, String)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    if let Some(tp) = n.child_by_field_name("type_parameters") {
        for p in kids(tp).filter(|c| c.kind() == "type_parameter") {
            let Some(name) = p.child_by_field_name("name") else { continue };
            let bounds = p.child_by_field_name("bounds").map(|b| bound_names(cx, b)).unwrap_or_default();
            out.push((cx.text(name).to_owned(), bounds));
        }
    }
    for w in kids(n).filter(|c| c.kind() == "where_clause") {
        for pred in kids(w).filter(|c| c.kind() == "where_predicate") {
            let (Some(left), Some(b)) = (pred.child_by_field_name("left"), pred.child_by_field_name("bounds")) else { continue };
            if left.kind() != "type_identifier" {
                continue;
            }
            let name = cx.text(left);
            match out.iter_mut().find(|(n, _)| n == name) {
                Some(entry) => entry.1.extend(bound_names(cx, b)),
                // a bound on a parameter of the enclosing impl
                None => {
                    let outer = cx.generics.iter().rev().find_map(|g| g.iter().find(|(n, _)| n == name)).is_some();
                    if outer {
                        out.push((name.to_owned(), bound_names(cx, b)));
                    }
                }
            }
        }
    }
    out.into_iter().map(|(n, b)| (n, if b.is_empty() { "_".to_owned() } else { format!("impl {}", b.join(" + ")) })).collect()
}

/// A written type with the type parameters in scope replaced by their bounds: `Arc<T>` with
/// `T: Svc` is `Arc<impl Svc>`; a parameter with no bound is `_`, which names no type.
fn subst_generics(cx: &Cx, ty: &str) -> String {
    if cx.generics.iter().all(Vec::is_empty) || ty.is_empty() {
        return ty.to_owned();
    }
    let lookup = |w: &str| cx.generics.iter().rev().find_map(|g| g.iter().find(|(n, _)| n == w).map(|(_, b)| b.clone()));
    let b = ty.as_bytes();
    let mut out = String::with_capacity(ty.len());
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &ty[start..i];
            // a path segment (`T::Output`, `a::T`) is no parameter
            let in_path = ty[..start].ends_with("::") || ty[i..].starts_with("::") || ty[..start].ends_with('\'');
            match lookup(word).filter(|_| !in_path) {
                Some(bound) => out.push_str(&bound),
                None => out.push_str(word),
            }
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// `impl Deref for Wrapper { type Target = Inner; }`: method calls on a `Wrapper` reach `Inner`.
fn deref_impl(cx: &mut Cx, n: Node, scope: u32) {
    let deref = n.child_by_field_name("trait").map(|t| cx.text(t)).is_some_and(|t| t == "Deref" || t.ends_with("::Deref"));
    let (Some(ty), Some(body)) = (n.child_by_field_name("type"), n.child_by_field_name("body")) else { return };
    if !deref {
        return;
    }
    let owner = match ty.kind() {
        "generic_type" => ty.child_by_field_name("type").map(|t| cx.text(t)).unwrap_or(""),
        _ => cx.text(ty),
    }
    .to_owned();
    for item in kids(body).filter(|c| c.kind() == "type_item") {
        if item.child_by_field_name("name").map(|nm| cx.text(nm)) != Some("Target") {
            continue;
        }
        let Some(target) = item.child_by_field_name("type") else { continue };
        let target = subst_generics(cx, &rust_type_name(cx, target));
        if !owner.is_empty() && !target.is_empty() {
            let line = cx.line(n);
            cx.facts.bindings.push(BindingFact {
                scope,
                name: owner.clone(),
                kind: BindKind::Deref,
                decl_type: target,
                init_ctor: String::new(),
                line,
                node_row: NONE,
            });
        }
    }
}

/// `impl Trait for X { fn m() }`: `m` is the trait's, which an inherent `X::m` shadows.
fn trait_impl(cx: &mut Cx, n: Node, scope: u32) {
    let (Some(_), Some(body)) = (n.child_by_field_name("trait"), n.child_by_field_name("body")) else { return };
    for f in kids(body).filter(|c| c.kind() == "function_item") {
        let (Some(name), Some(row)) = (f.child_by_field_name("name"), cx.row_at(cx.start(f))) else { continue };
        let name = cx.text(name).to_owned();
        let line = cx.line(f);
        cx.facts.bindings.push(BindingFact { scope, name, kind: BindKind::TraitImpl, decl_type: String::new(), init_ctor: String::new(), line, node_row: row });
    }
}

fn rust_type_name(cx: &Cx, n: Node) -> String {
    match n.kind() {
        "type_identifier" | "scoped_type_identifier" => cx.text(n).to_owned(),
        "generic_type" => cx.text(n).to_owned(),
        "reference_type" | "pointer_type" => n.child_by_field_name("type").map(|c| rust_type_name(cx, c)).unwrap_or_default(),
        "abstract_type" | "dynamic_type" | "bounded_type" => {
            n.child_by_field_name("trait").or_else(|| n.named_child(0)).map(|c| rust_type_name(cx, c)).unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn bind_pattern(cx: &mut Cx, p: Node, scope: u32, kind: BindKind) {
    match p.kind() {
        "identifier" => {
            let nm = cx.text(p).to_owned();
            let line = cx.line(p);
            cx.add_binding(scope, &nm, kind, "", "", line);
        }
        "mut_pattern" | "reference_pattern" | "tuple_pattern" | "tuple_struct_pattern" | "slice_pattern" | "struct_pattern" | "ref_pattern" => {
            for c in kids(p) {
                bind_pattern(cx, c, scope, kind);
            }
        }
        "field_pattern" => {
            if let Some(c) = p.child_by_field_name("pattern").or_else(|| p.child_by_field_name("name")) {
                bind_pattern(cx, c, scope, kind);
            }
        }
        _ => {}
    }
}

fn value_info(cx: &Cx, v: Node) -> String {
    value_path(cx, v, 0)
}

fn value_path(cx: &Cx, v: Node, depth: u8) -> String {
    if depth >= 16 || v.end_byte() - v.start_byte() > 4096 {
        return String::new();
    }
    match v.kind() {
        "identifier" | "scoped_identifier" => cx.text(v).to_owned(),
        "generic_function" => v.child_by_field_name("function").map(|f| value_path(cx, f, depth + 1)).unwrap_or_default(),
        "struct_expression" => v.child_by_field_name("name").map(|n| cx.text(n).to_owned()).unwrap_or_default(),
        "call_expression" => {
            let callee = v.child_by_field_name("function").map(|f| value_path(cx, f, depth + 1)).unwrap_or_default();
            if callee.is_empty() {
                return String::new();
            }
            if callee.ends_with("::new") {
                if let Some(args) = v.child_by_field_name("arguments") {
                    let values: Vec<_> = kids(args).filter(|n| n.is_named() && !matches!(n.kind(), "comment" | "line_comment" | "block_comment")).collect();
                    if values.len() == 1 {
                        let inner = value_path(cx, values[0], depth + 1);
                        if !inner.is_empty() {
                            return format!("{callee}({inner})");
                        }
                    }
                }
            }
            format!("{callee}()")
        }
        "field_expression" => {
            let base = v.child_by_field_name("value").map(|v| value_path(cx, v, depth + 1)).unwrap_or_default();
            match v.child_by_field_name("field") {
                Some(field) if !base.is_empty() => format!("{base}.{}", cx.text(field)),
                _ => String::new(),
            }
        }
        "try_expression" => v.named_child(0).map(|c| value_path(cx, c, depth + 1)).filter(|p| !p.is_empty()).map(|p| format!("{p}?")).unwrap_or_default(),
        "await_expression" | "reference_expression" | "parenthesized_expression" => v.named_child(0).map(|c| value_path(cx, c, depth + 1)).unwrap_or_default(),
        _ => String::new(),
    }
}

fn let_decl(cx: &mut Cx, n: Node, scope: u32) {
    let Some(p) = n.child_by_field_name("pattern") else { return };
    let ty = n.child_by_field_name("type").map(|t| subst_generics(cx, &rust_type_name(cx, t))).unwrap_or_default();
    let init = n.child_by_field_name("value").map(|v| value_info(cx, v)).unwrap_or_default();
    let line = cx.line(n);
    let target = if p.kind() == "mut_pattern" { p.named_child(0).unwrap_or(p) } else { p };
    if target.kind() == "identifier" {
        let nm = cx.text(target).to_owned();
        cx.add_binding(scope, &nm, BindKind::Local, &ty, &init, line);
    } else if target.kind() == "tuple_pattern" && init.ends_with([')', '?']) && !kids(target).any(|c| c.kind() == "remaining_field_pattern") {
        // `let (tx, rx) = mpsc::channel()`: element i holds the call's `.i`
        let elems: Vec<Node> = kids(target).filter(|c| (c.is_named() || c.kind() == "_") && !c.kind().ends_with("comment")).collect();
        for (i, e) in elems.into_iter().enumerate() {
            // `mut rx`: `mutable_specifier` first, then the pattern
            let id = if e.kind() == "mut_pattern" { e.named_child(1).unwrap_or(e) } else { e };
            if id.kind() == "identifier" {
                let nm = cx.text(id).to_owned();
                cx.add_binding(scope, &nm, BindKind::Local, "", &format!("{init}.{i}"), line);
            } else {
                bind_pattern(cx, e, scope, BindKind::Local);
            }
        }
    } else {
        bind_pattern(cx, p, scope, BindKind::Local);
    }
}

fn params(cx: &mut Cx, list: Node, sc: u32) {
    for p in kids(list) {
        match p.kind() {
            "self_parameter" => {
                let line = cx.line(p);
                cx.add_binding(sc, "self", BindKind::Param, "Self", "", line);
            }
            "parameter" => {
                let ty = p.child_by_field_name("type").map(|t| subst_generics(cx, &rust_type_name(cx, t))).unwrap_or_default();
                if let Some(pat) = p.child_by_field_name("pattern") {
                    let target = if pat.kind() == "mut_pattern" { pat.named_child(0).unwrap_or(pat) } else { pat };
                    if target.kind() == "identifier" {
                        let nm = cx.text(target).to_owned();
                        let line = cx.line(p);
                        cx.add_binding(sc, &nm, BindKind::Param, &ty, "", line);
                    } else {
                        bind_pattern(cx, pat, sc, BindKind::Param);
                    }
                }
            }
            "identifier" => {
                let nm = cx.text(p).to_owned();
                let line = cx.line(p);
                cx.add_binding(sc, &nm, BindKind::Param, "", "", line);
            }
            _ => {}
        }
    }
}

fn function(cx: &mut Cx, n: Node, scope: u32) {
    let generics = generics_of(cx, n);
    cx.generics.push(generics);
    function_body(cx, n, scope);
    cx.generics.pop();
}

fn function_body(cx: &mut Cx, n: Node, scope: u32) {
    if let (Some(name), Some(ret)) = (n.child_by_field_name("name"), n.child_by_field_name("return_type")) {
        if let Some(row) = cx.row_at(cx.start(n)) {
            let decl_type = subst_generics(cx, cx.text(ret));
            cx.facts.bindings.push(BindingFact {
                scope,
                name: cx.text(name).to_owned(),
                kind: BindKind::ReturnType,
                decl_type,
                init_ctor: String::new(),
                line: cx.line(n),
                node_row: row,
            });
        }
    }
    if cx.facts.scopes[scope as usize].kind == ScopeKind::Function {
        if let Some(name) = n.child_by_field_name("name") {
            let nm = cx.text(name).to_owned();
            cx.add_binding(scope, &nm, BindKind::LocalFn, "", "", 0);
            let row = cx.row_at(cx.start(n));
            cx.facts.bindings.last_mut().unwrap().node_row = row.unwrap_or(NONE);
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    bind_type_params(cx, sc);
    if let Some(p) = n.child_by_field_name("parameters") {
        params(cx, p, sc);
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
    }
}

/// The type parameters of the item whose scope `sc` is (the innermost `cx.generics`).
fn bind_type_params(cx: &mut Cx, sc: u32) {
    let names: Vec<String> = cx.generics.last().map(|g| g.iter().map(|(n, _)| n.clone()).collect()).unwrap_or_default();
    for n in names {
        cx.add_binding(sc, &n, BindKind::TypeParam, "", "", 0);
    }
}

fn closure(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(p) = n.child_by_field_name("parameters") {
        params(cx, p, sc);
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
    }
}

fn struct_item(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    let Some(body) = n.child_by_field_name("body") else { return };
    for f in kids(body) {
        if f.kind() == "field_declaration" {
            let ty = f.child_by_field_name("type").map(|t| subst_generics(cx, &rust_type_name(cx, t))).unwrap_or_default();
            if let Some(nm) = f.child_by_field_name("name") {
                let t = cx.text(nm).to_owned();
                let line = cx.line(f);
                cx.add_binding(sc, &t, BindKind::Field, &ty, "", line);
            }
        }
    }
}

// ----------------------------------------------------------------------------- use trees

fn use_decl(cx: &mut Cx, n: Node, scope: u32) {
    let public = kids(n).any(|c| c.kind() == "visibility_modifier");
    let Some(arg) = n.child_by_field_name("argument") else { return };
    let line = cx.line(n);
    let mut out = Vec::new();
    flatten(cx, arg, &[], &mut out);
    for (path, local, wildcard) in out {
        // path = full item path segments; the item is the last one
        if wildcard {
            let spec = path.join("::");
            cx.add_import(ImportFact {
                local: "*".into(),
                imported: "*".into(),
                spec: spec.clone(),
                kind: ImportKind::Namespace,
                type_only: false,
                scope,
                line,
            });
            if public {
                cx.facts.exports.push(ExportFact { name: "*".into(), node_row: NONE, from_spec: spec, from_name: "*".into(), star: true });
            }
            continue;
        }
        let Some((item, module)) = path.split_last() else { continue };
        if module.is_empty() {
            // `use tokio_stream::{self as stream};` or `use other_crate as alias;`
            // imports the crate/module itself as a namespace alias.
            let local = local.unwrap_or_else(|| item.clone());
            cx.add_import(ImportFact {
                local: local.clone(),
                imported: "*".into(),
                spec: item.clone(),
                kind: ImportKind::Namespace,
                type_only: false,
                scope,
                line,
            });
            if public {
                cx.facts.exports.push(ExportFact { name: local, node_row: NONE, from_spec: item.clone(), from_name: "*".into(), star: false });
            }
            continue;
        }
        let spec = module.join("::");
        let local = local.unwrap_or_else(|| item.clone());
        cx.add_import(ImportFact { local: local.clone(), imported: item.clone(), spec: spec.clone(), kind: ImportKind::Named, type_only: false, scope, line });
        if public {
            cx.facts.exports.push(ExportFact { name: local, node_row: NONE, from_spec: spec, from_name: item.clone(), star: false });
        }
    }
}

/// (full path, alias, is_wildcard) triples of a use tree.
fn flatten(cx: &Cx, n: Node, prefix: &[String], out: &mut Vec<(Vec<String>, Option<String>, bool)>) {
    let segs = |node: Node| -> Vec<String> { cx.text(node).split("::").map(|s| s.trim().to_owned()).collect() };
    match n.kind() {
        "identifier" | "crate" | "super" | "self" | "scoped_identifier" => {
            let mut p = prefix.to_vec();
            let s = segs(n);
            if s.len() == 1 && s[0] == "self" && !p.is_empty() {
                // `use a::b::{self}` imports module `b`
            } else {
                p.extend(s);
            }
            out.push((p, None, false));
        }
        "use_as_clause" => {
            let alias = n.child_by_field_name("alias").map(|a| cx.text(a).to_owned());
            if let Some(path) = n.child_by_field_name("path") {
                let mut p = prefix.to_vec();
                let s = segs(path);
                if !(s.len() == 1 && s[0] == "self" && !p.is_empty()) {
                    p.extend(s);
                }
                out.push((p, alias, false));
            }
        }
        "scoped_use_list" => {
            let mut p = prefix.to_vec();
            if let Some(path) = n.child_by_field_name("path") {
                p.extend(segs(path));
            }
            if let Some(list) = n.child_by_field_name("list") {
                for c in kids(list) {
                    if c.is_named() {
                        flatten(cx, c, &p, out);
                    }
                }
            }
        }
        "use_list" => {
            for c in kids(n) {
                if c.is_named() {
                    flatten(cx, c, prefix, out);
                }
            }
        }
        "use_wildcard" => {
            let mut p = prefix.to_vec();
            if let Some(path) = n.named_child(0) {
                p.extend(segs(path));
            }
            out.push((p, None, true));
        }
        _ => {}
    }
}
