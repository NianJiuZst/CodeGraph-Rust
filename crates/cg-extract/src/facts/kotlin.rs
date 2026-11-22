//! Facts for Kotlin: package, imports (with aliases and wildcards), class / function / lambda
//! scopes, parameters, properties and primary-constructor `val` fields with declared types.
//! The grammar has no named fields, so everything is matched by child kind.

use super::util::*;
use super::*;

pub fn extract(cx: &mut Cx, root: Node) {
    walk(cx, root, 0);
}

fn walk_children(cx: &mut Cx, n: Node, scope: u32) {
    for ch in kids(n) {
        walk(cx, ch, scope);
    }
}

fn first<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    kids(n).find(|c| c.kind() == kind)
}

fn dotted(cx: &Cx, n: Node) -> String {
    kids(n).filter(|c| c.kind() == "simple_identifier").map(|c| cx.text(c)).collect::<Vec<_>>().join(".")
}

fn walk(cx: &mut Cx, n: Node, scope: u32) {
    match n.kind() {
        "package_header" => {
            if let Some(id) = first(n, "identifier") {
                let p = dotted(cx, id);
                cx.add_binding(0, &p, BindKind::Package, "", "", 1);
            }
        }
        "import_header" => import(cx, n, scope),
        "class_declaration" | "object_declaration" => class(cx, n, scope),
        "companion_object" => walk_children(cx, n, scope),
        "function_declaration" | "secondary_constructor" | "anonymous_function" => function(cx, n, scope),
        "lambda_literal" => lambda(cx, n, scope, None, None),
        "call_expression" => call_expr(cx, n, scope),
        "property_declaration" => {
            property(cx, n, scope);
            walk_children(cx, n, scope);
        }
        "for_statement" => {
            for vd in kids(n).filter(|c| c.kind() == "variable_declaration") {
                if let Some(id) = first(vd, "simple_identifier") {
                    let t = cx.text(id).to_owned();
                    let line = cx.line(id);
                    cx.add_binding(scope, &t, BindKind::Local, "", "", line);
                }
            }
            walk_children(cx, n, scope);
        }
        _ => walk_children(cx, n, scope),
    }
}

fn type_name(cx: &Cx, n: Node) -> String {
    match n.kind() {
        "user_type" => {
            // Foo.Bar<T> -> "Foo.Bar"
            let parts: Vec<&str> = kids(n)
                .filter(|c| c.kind() == "type_identifier" || c.kind() == "simple_user_type")
                .map(|c| if c.kind() == "simple_user_type" { first(c, "type_identifier").map(|t| cx.text(t)).unwrap_or("") } else { cx.text(c) })
                .collect();
            parts.join(".")
        }
        "nullable_type" | "parenthesized_type" => kids(n).find(|c| c.is_named()).map(|c| type_name(cx, c)).unwrap_or_default(),
        "type_identifier" => cx.text(n).to_owned(),
        _ => String::new(),
    }
}

fn find_type(cx: &Cx, n: Node) -> String {
    kids(n).find(|c| matches!(c.kind(), "user_type" | "nullable_type" | "parenthesized_type")).map(|t| type_name(cx, t)).unwrap_or_default()
}

fn expr_path(cx: &Cx, n: Node, depth: u8) -> Option<String> {
    if depth >= 32 {
        return None;
    }
    match n.kind() {
        "simple_identifier" => Some(cx.text(n).to_owned()),
        "this_expression" => (cx.text(n) == "this").then(|| "this".to_owned()),
        "super_expression" => (cx.text(n) == "super").then(|| "super".to_owned()),
        "parenthesized_expression" => n.named_child(0).and_then(|c| expr_path(cx, c, depth + 1)),
        "postfix_expression" => {
            // `expr!!`
            let inner = n.named_child(0)?;
            if cx.text(n).trim_end().ends_with("!!") {
                expr_path(cx, inner, depth + 1)
            } else {
                None
            }
        }
        "as_expression" => {
            let ty = n.named_child(1).map(|t| type_name(cx, t)).unwrap_or_default();
            (!ty.is_empty()).then_some(ty)
        }
        "navigation_expression" => {
            let obj = n.named_child(0)?;
            let suffix = first(n, "navigation_suffix")?;
            let id = first(suffix, "simple_identifier")?;
            let base = expr_path(cx, obj, depth + 1)?;
            Some(format!("{base}.{}", cx.text(id)))
        }
        "call_expression" => {
            let callee = n.named_child(0)?;
            // `f(a) { ... }` wraps an inner call_expression with a trailing-lambda suffix.
            if callee.kind() == "call_expression" {
                return expr_path(cx, callee, depth + 1);
            }
            let path = expr_path(cx, callee, depth + 1)?;
            Some(format!("{path}()"))
        }
        _ => None,
    }
}

pub(super) fn object_literal_super(cx: &Cx, n: Node) -> String {
    let mut first_ty = String::new();
    for spec in kids(n).filter(|c| c.kind() == "delegation_specifier") {
        let holder = kids(spec).find(|c| matches!(c.kind(), "constructor_invocation" | "explicit_delegation"));
        let from = holder.unwrap_or(spec);
        let Some(ut) = first(from, "user_type") else { continue };
        let ty = type_name(cx, ut);
        if ty.is_empty() {
            continue;
        }
        if holder.is_some_and(|h| h.kind() == "constructor_invocation") {
            return ty;
        }
        if first_ty.is_empty() {
            first_ty = ty;
        }
    }
    first_ty
}

fn is_simple_nav_chain(text: &str) -> bool {
    let chain: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let chain = chain.replace("?.", ".");
    let segs: Vec<&str> = chain.split('.').collect();
    (2..=4).contains(&segs.len())
        && segs.iter().all(|s| {
            let b = s.as_bytes();
            !b.is_empty() && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
        })
}

fn call_name(cx: &mut Cx, call: Node, callee: Node) {
    if callee.kind() != "navigation_expression" {
        return;
    }
    let Some(obj) = callee.named_child(0) else { return };
    if obj.kind() == "simple_identifier" || (obj.kind() == "navigation_expression" && is_simple_nav_chain(cx.text(obj))) {
        return;
    }
    let Some(suffix) = first(callee, "navigation_suffix") else { return };
    let Some(id) = first(suffix, "simple_identifier") else { return };
    let method = cx.text(id);
    let Some(base) = expr_path(cx, obj, 0) else { return };
    let name = format!("{base}.{method}");
    let kernel_name = if obj.kind() == "call_expression" {
        let inner: String = obj.named_child(0).map(|c| cx.text(c).chars().filter(|ch| !ch.is_whitespace()).collect()).unwrap_or_default();
        if inner.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
            format!("{inner}().{method}")
        } else {
            method.to_owned()
        }
    } else {
        method.to_owned()
    };
    if name == kernel_name {
        return;
    }
    let pos = cx.start(call);
    cx.facts.call_names.push(CallNameFact { line: (pos >> 32) as u32, col: pos as u32, tail: method.to_owned(), kernel_name, name });
}

fn call_expr(cx: &mut Cx, n: Node, scope: u32) {
    super::args::kotlin_call(cx, n);
    let Some(callee) = n.named_child(0) else {
        walk_children(cx, n, scope);
        return;
    };
    call_name(cx, n, callee);

    // Scope-function trailing lambdas: `.use`/`.let`/`.also`/`.takeIf`/`.takeUnless` bind the
    // lambda's parameter (`it` or first explicit param); `.apply`/`.run`/`with(x)` bind `this`.
    let suffix = first(n, "call_suffix");
    let has_lambda = suffix.and_then(|s| first(s, "annotated_lambda")).and_then(|a| first(a, "lambda_literal")).is_some();
    if has_lambda {
        let (target, value_args) = if callee.kind() == "call_expression" {
            (callee.named_child(0), first(callee, "call_suffix").and_then(|s| first(s, "value_arguments")))
        } else {
            (Some(callee), suffix.and_then(|s| first(s, "value_arguments")))
        };
        let mut param_init = None;
        let mut this_init = None;
        if let Some(target) = target {
            if target.kind() == "navigation_expression" {
                if let (Some(recv), Some(fn_name)) =
                    (target.named_child(0), first(target, "navigation_suffix").and_then(|s| first(s, "simple_identifier")).map(|id| cx.text(id)))
                {
                    if matches!(fn_name, "use" | "let" | "also" | "takeIf" | "takeUnless") {
                        param_init = expr_path(cx, recv, 0);
                    } else if matches!(fn_name, "apply" | "run") {
                        this_init = expr_path(cx, recv, 0);
                    }
                }
            } else if target.kind() == "simple_identifier" && cx.text(target) == "with" {
                if let Some(arg) = value_args.and_then(|va| first(va, "value_argument")).and_then(|a| a.named_child(0)) {
                    this_init = expr_path(cx, arg, 0);
                }
            }
            // Kotlin DSL receiver lambda (`testStateMachineDnsCall { ... }`, `buildRequest { ... }`):
            // mark the callee so the resolver can read `Receiver.() -> R` from its signature.
            if param_init.is_none() && this_init.is_none() {
                if let Some(path) = expr_path(cx, target, 0) {
                    this_init = Some(format!("\x01{path}"));
                }
            }
        }
        if param_init.is_some() || this_init.is_some() {
            for ch in kids(n) {
                if ch.kind() == "call_suffix" {
                    for part in kids(ch) {
                        if part.kind() == "annotated_lambda" {
                            for sub in kids(part) {
                                if sub.kind() == "lambda_literal" {
                                    lambda(cx, sub, scope, param_init.as_deref(), this_init.as_deref());
                                } else {
                                    walk(cx, sub, scope);
                                }
                            }
                        } else {
                            walk(cx, part, scope);
                        }
                    }
                } else {
                    walk(cx, ch, scope);
                }
            }
            return;
        }
    }
    walk_children(cx, n, scope);
}

fn body_expr(body: Node) -> Option<Node> {
    (body.child(0).is_some_and(|c| c.kind() == "=")).then(|| body.named_child(0)).flatten()
}

fn init_expr(n: Node) -> Option<Node> {
    for c in kids(n).collect::<Vec<_>>().into_iter().rev() {
        match c.kind() {
            "binding_pattern_kind" | "variable_declaration" | "modifiers" | "type_constraints" | "property_delegate" => break,
            "comment" | "multiline_comment" | "getter" | "setter" => {}
            _ => return Some(c),
        }
    }
    first(n, "getter").and_then(|g| first(g, "function_body")).and_then(body_expr)
}

fn property(cx: &mut Cx, n: Node, scope: u32) {
    let Some(vd) = first(n, "variable_declaration") else { return };
    let Some(id) = first(vd, "simple_identifier") else { return };
    let mut ty = find_type(cx, vd);
    let mut init = String::new();
    if let Some(expr) = init_expr(n) {
        match expr.kind() {
            "object_literal" if ty.is_empty() => ty = object_literal_super(cx, expr),
            "as_expression" if ty.is_empty() => {
                if let Some(t) = expr.named_child(1) {
                    ty = type_name(cx, t);
                }
            }
            "simple_identifier" => {
                let s = cx.text(expr);
                if !matches!(s, "null" | "true" | "false") {
                    init = s.to_owned();
                }
            }
            _ => init = expr_path(cx, expr, 0).unwrap_or_default(),
        }
    }
    let name = cx.text(id).to_owned();
    let line = cx.line(id);
    let kind = if cx.facts.scopes[scope as usize].kind == ScopeKind::Class { BindKind::Field } else { BindKind::Local };
    cx.add_binding(scope, &name, kind, &ty, &init, line);
}

fn import(cx: &mut Cx, n: Node, scope: u32) {
    let Some(id) = first(n, "identifier") else { return };
    let path = dotted(cx, id);
    if path.is_empty() {
        return;
    }
    let line = cx.line(n);
    if first(n, "wildcard_import").is_some() {
        cx.add_import(ImportFact { local: "*".into(), imported: "*".into(), spec: path, kind: ImportKind::Namespace, type_only: false, scope, line });
        return;
    }
    let last = path.rsplit('.').next().unwrap_or("").to_owned();
    let alias = first(n, "import_alias").and_then(|a| first(a, "type_identifier")).map(|t| cx.text(t).to_owned());
    cx.add_import(ImportFact {
        local: alias.unwrap_or_else(|| last.clone()),
        imported: last,
        spec: path,
        kind: ImportKind::Named,
        type_only: false,
        scope,
        line,
    });
}

fn class(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    if let Some(pc) = first(n, "primary_constructor") {
        for cp in kids(pc).filter(|c| c.kind() == "class_parameter") {
            let ty = find_type(cx, cp);
            let is_prop = first(cp, "binding_pattern_kind").is_some();
            if let Some(id) = first(cp, "simple_identifier") {
                let t = cx.text(id).to_owned();
                let line = cx.line(id);
                let kind = if is_prop { BindKind::Field } else { BindKind::Param };
                cx.add_binding(sc, &t, kind, &ty, "", line);
            }
        }
    }
    walk_children(cx, n, sc);
}

fn has_explicit_return_type(n: Node) -> bool {
    let mut seen_params = false;
    for ch in kids(n) {
        if ch.kind() == "function_value_parameters" {
            seen_params = true;
            continue;
        }
        if !seen_params {
            continue;
        }
        if matches!(ch.kind(), "function_body" | "type_constraints") {
            return false;
        }
        if matches!(ch.kind(), "user_type" | "nullable_type" | "function_type") {
            return true;
        }
    }
    false
}

fn function(cx: &mut Cx, n: Node, scope: u32) {
    if cx.facts.scopes[scope as usize].kind == ScopeKind::Function && n.kind() == "function_declaration" {
        if let Some(id) = first(n, "simple_identifier") {
            let t = cx.text(id).to_owned();
            cx.add_binding(scope, &t, BindKind::LocalFn, "", "", 0);
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    let recv_ty = if n.kind() == "function_declaration" { first(n, "receiver_type").map(|rt| find_type(cx, rt)).unwrap_or_default() } else { String::new() };
    if !recv_ty.is_empty() {
        let line = cx.line(n);
        cx.add_binding(sc, "this", BindKind::Param, &recv_ty, "", line);
    }
    if let Some(params) = first(n, "function_value_parameters") {
        for p in kids(params).filter(|c| c.kind() == "parameter") {
            let ty = find_type(cx, p);
            if let Some(id) = first(p, "simple_identifier") {
                let t = cx.text(id).to_owned();
                let line = cx.line(id);
                cx.add_binding(sc, &t, BindKind::Param, &ty, "", line);
            }
        }
    }
    if n.kind() == "function_declaration" && !has_explicit_return_type(n) {
        if let Some(expr) = first(n, "function_body").and_then(body_expr) {
            if let (Some(id), Some(row)) = (first(n, "simple_identifier"), cx.row_at(cx.start(n))) {
                let in_class = cx.facts.scopes[scope as usize].kind == ScopeKind::Class;
                let returns_self = expr.kind() == "this_expression"
                    || (expr.kind() == "call_expression"
                        && expr.named_child(0).is_some_and(|c| c.kind() == "simple_identifier" && matches!(cx.text(c), "apply" | "also")));
                let (decl_type, init_ctor) = if returns_self && (!recv_ty.is_empty() || in_class) {
                    (if !recv_ty.is_empty() { recv_ty.clone() } else { "this".to_owned() }, String::new())
                } else if expr.kind() == "object_literal" {
                    (object_literal_super(cx, expr), String::new())
                } else if let Some(path) = expr_path(cx, expr, 0) {
                    (String::new(), path)
                } else {
                    (String::new(), String::new())
                };
                if !decl_type.is_empty() || !init_ctor.is_empty() {
                    cx.facts.bindings.push(BindingFact {
                        scope: sc,
                        name: cx.text(id).to_owned(),
                        kind: BindKind::ReturnType,
                        decl_type,
                        init_ctor,
                        line: cx.line(n),
                        node_row: row,
                    });
                }
            }
        }
    }
    for ch in kids(n) {
        if ch.kind() != "function_value_parameters" {
            walk(cx, ch, sc);
        }
    }
}

fn lambda(cx: &mut Cx, n: Node, scope: u32, param_init: Option<&str>, this_init: Option<&str>) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    let line = cx.line(n);
    if let Some(this_expr) = this_init {
        cx.add_binding(sc, "this", BindKind::Param, "", this_expr, line);
    }
    if let Some(params) = first(n, "lambda_parameters") {
        let mut first_param = true;
        for vd in kids(params).filter(|c| c.kind() == "variable_declaration") {
            let ty = find_type(cx, vd);
            if let Some(id) = first(vd, "simple_identifier") {
                let t = cx.text(id).to_owned();
                let pline = cx.line(id);
                let init = if first_param { param_init.unwrap_or("") } else { "" };
                first_param = false;
                cx.add_binding(sc, &t, BindKind::Param, &ty, init, pline);
            }
        }
    } else {
        // implicit `it`
        cx.add_binding(sc, "it", BindKind::Param, "", param_init.unwrap_or(""), line);
    }
    walk_children(cx, n, sc);
}
