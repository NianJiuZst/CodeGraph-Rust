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
        "class_declaration" | "object_declaration" | "companion_object" => class(cx, n, scope),
        "function_declaration" | "secondary_constructor" | "anonymous_function" => function(cx, n, scope),
        "lambda_literal" => lambda(cx, n, scope),
        "call_expression" => {
            super::args::kotlin_call(cx, n);
            walk_children(cx, n, scope);
        }
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
        "this_expression" => Some("this".to_owned()),
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
        "navigation_expression" => {
            let obj = n.named_child(0)?;
            let suffix = kids(n).find(|c| c.kind() == "navigation_suffix")?;
            let id = first(suffix, "simple_identifier")?;
            let base = expr_path(cx, obj, depth + 1)?;
            Some(format!("{base}.{}", cx.text(id)))
        }
        "call_expression" => {
            let callee = n.named_child(0)?;
            let path = expr_path(cx, callee, depth + 1)?;
            Some(format!("{path}()"))
        }
        _ => None,
    }
}

fn init_of(cx: &Cx, n: Node) -> String {
    // the initialiser is the last expression child of a property declaration
    for c in kids(n).collect::<Vec<_>>().into_iter().rev() {
        match c.kind() {
            "call_expression" | "navigation_expression" | "parenthesized_expression" | "postfix_expression" => {
                return expr_path(cx, c, 0).unwrap_or_default();
            }
            "binding_pattern_kind" | "variable_declaration" | "modifiers" | "type_constraints" | "getter" | "setter" => break,
            _ => {}
        }
    }
    String::new()
}

fn property(cx: &mut Cx, n: Node, scope: u32) {
    let Some(vd) = first(n, "variable_declaration") else { return };
    let Some(id) = first(vd, "simple_identifier") else { return };
    let ty = find_type(cx, vd);
    let init = init_of(cx, n);
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
                if is_prop {
                    cx.add_binding(sc, &t, BindKind::Field, &ty, "", line);
                }
            }
        }
    }
    walk_children(cx, n, sc);
}

fn function(cx: &mut Cx, n: Node, scope: u32) {
    if cx.facts.scopes[scope as usize].kind == ScopeKind::Function && n.kind() == "function_declaration" {
        if let Some(id) = first(n, "simple_identifier") {
            let t = cx.text(id).to_owned();
            cx.add_binding(scope, &t, BindKind::LocalFn, "", "", 0);
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
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
    for ch in kids(n) {
        if ch.kind() != "function_value_parameters" {
            walk(cx, ch, sc);
        }
    }
}

fn lambda(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(params) = first(n, "lambda_parameters") {
        for vd in kids(params).filter(|c| c.kind() == "variable_declaration") {
            let ty = find_type(cx, vd);
            if let Some(id) = first(vd, "simple_identifier") {
                let t = cx.text(id).to_owned();
                let line = cx.line(id);
                cx.add_binding(sc, &t, BindKind::Param, &ty, "", line);
            }
        }
    } else {
        // implicit `it`
        let line = cx.line(n);
        cx.add_binding(sc, "it", BindKind::Param, "", "", line);
    }
    walk_children(cx, n, sc);
}
