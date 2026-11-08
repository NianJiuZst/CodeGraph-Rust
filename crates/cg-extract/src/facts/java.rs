//! Facts for Java: package, imports (single, wildcard, static), class / method / lambda scopes,
//! parameters, locals and fields with declared types.

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

fn walk(cx: &mut Cx, n: Node, scope: u32) {
    match n.kind() {
        "package_declaration" => {
            let name = kids(n).find(|c| matches!(c.kind(), "scoped_identifier" | "identifier")).map(|c| cx.text(c).to_owned());
            if let Some(p) = name {
                cx.add_binding(0, &p, BindKind::Package, "", "", 1);
            }
        }
        "import_declaration" => import(cx, n, scope),
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" | "annotation_type_declaration" => class(cx, n, scope),
        "method_declaration" | "constructor_declaration" | "compact_constructor_declaration" => function(cx, n, scope),
        "lambda_expression" => lambda(cx, n, scope),
        "method_invocation" => {
            super::args::java_call(cx, n);
            super_receiver(cx, n);
            walk_children(cx, n, scope);
        }
        "local_variable_declaration" => {
            declare(cx, n, scope, BindKind::Local);
            walk_children(cx, n, scope);
        }
        "enhanced_for_statement" => {
            let ty = n.child_by_field_name("type").map(|t| java_type_name(cx, t)).unwrap_or_default();
            if let Some(nm) = n.child_by_field_name("name") {
                let t = cx.text(nm).to_owned();
                let line = cx.line(nm);
                cx.add_binding(scope, &t, BindKind::Local, &ty, "", line);
            }
            walk_children(cx, n, scope);
        }
        "catch_formal_parameter" | "resource" => {
            let ty = n.child_by_field_name("type").map(|t| java_type_name(cx, t)).unwrap_or_default();
            if let Some(nm) = n.child_by_field_name("name") {
                let t = cx.text(nm).to_owned();
                let line = cx.line(nm);
                cx.add_binding(scope, &t, BindKind::Local, &ty, "", line);
            }
            walk_children(cx, n, scope);
        }
        "type_pattern" | "record_pattern" => {
            // `x instanceof Foo f`
            let ty = kids(n)
                .find(|c| matches!(c.kind(), "type_identifier" | "scoped_type_identifier" | "generic_type"))
                .map(|t| java_type_name(cx, t))
                .unwrap_or_default();
            if let Some(nm) = kids(n).find(|c| c.kind() == "identifier") {
                let t = cx.text(nm).to_owned();
                let line = cx.line(nm);
                cx.add_binding(scope, &t, BindKind::Local, &ty, "", line);
            }
            walk_children(cx, n, scope);
        }
        _ => walk_children(cx, n, scope),
    }
}

fn java_type_name(cx: &Cx, n: Node) -> String {
    match n.kind() {
        "type_identifier" if cx.text(n) == "var" => String::new(),
        "type_identifier" | "scoped_type_identifier" => cx.text(n).to_owned(),
        // primitives type their arguments for overload selection (`args`); they name no member
        "integral_type" | "floating_point_type" | "boolean_type" => cx.text(n).to_owned(),
        "generic_type" => n.named_child(0).map(|c| java_type_name(cx, c)).unwrap_or_default(),
        "annotated_type" => kids(n).find(|c| !matches!(c.kind(), "annotation" | "marker_annotation")).map(|c| java_type_name(cx, c)).unwrap_or_default(),
        _ => String::new(), // arrays, `var`, void
    }
}

fn value_info(cx: &Cx, v: Node) -> String {
    match v.kind() {
        "object_creation_expression" => v.child_by_field_name("type").map(|t| java_type_name(cx, t)).unwrap_or_default(),
        "method_invocation" => {
            let name = v.child_by_field_name("name").map(|c| cx.text(c)).unwrap_or("");
            match v.child_by_field_name("object") {
                Some(o) if matches!(o.kind(), "identifier" | "field_access" | "this") => format!("{}.{}()", cx.text(o), name),
                Some(_) => String::new(),
                None => format!("{name}()"),
            }
        }
        "parenthesized_expression" | "cast_expression" => {
            if v.kind() == "cast_expression" {
                return String::new();
            }
            v.named_child(0).map(|c| value_info(cx, c)).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// `Foo x = new Foo();` / `var x = new Foo();` / fields.
fn declare(cx: &mut Cx, n: Node, scope: u32, kind: BindKind) {
    let ty = n.child_by_field_name("type").map(|t| java_type_name(cx, t)).unwrap_or_default();
    for d in kids(n) {
        if d.kind() != "variable_declarator" {
            continue;
        }
        let Some(nm) = d.child_by_field_name("name") else { continue };
        let mut init = d.child_by_field_name("value").map(|v| value_info(cx, v)).unwrap_or_default();
        // `var x = new Foo()` names its type through the initialiser; typed declarations keep theirs
        let (decl, init_ctor) = if ty.is_empty() && !init.ends_with("()") { (std::mem::take(&mut init), String::new()) } else { (ty.clone(), init) };
        let _ = &decl;
        let t = cx.text(nm).to_owned();
        let line = cx.line(nm);
        cx.add_binding(scope, &t, kind, &decl, &init_ctor, line);
    }
}

fn import(cx: &mut Cx, n: Node, scope: u32) {
    let line = cx.line(n);
    let text = cx.text(n);
    let is_static = kids(n).any(|c| c.kind() == "static");
    let wildcard = kids(n).any(|c| c.kind() == "asterisk");
    let path: String = text
        .trim_start_matches("import")
        .trim()
        .trim_start_matches("static")
        .trim()
        .trim_end_matches(';')
        .trim()
        .trim_end_matches(".*")
        .split_whitespace()
        .collect();
    if path.is_empty() {
        return;
    }
    let last = path.rsplit('.').next().unwrap_or("").to_owned();
    let fact = match (is_static, wildcard) {
        (true, true) => ImportFact { local: "*".into(), imported: "*".into(), spec: path, kind: ImportKind::Static, type_only: false, scope, line },
        (true, false) => ImportFact { local: last.clone(), imported: last, spec: path, kind: ImportKind::Static, type_only: false, scope, line },
        (false, true) => ImportFact { local: "*".into(), imported: "*".into(), spec: path, kind: ImportKind::Namespace, type_only: false, scope, line },
        (false, false) => ImportFact { local: last.clone(), imported: last, spec: path, kind: ImportKind::Named, type_only: false, scope, line },
    };
    cx.add_import(fact);
}

/// The kernel names `super.m()` as a bare `m`, which would bind to the caller's own
/// override; restore the `super.` receiver so the call resolves in the ancestors.
fn super_receiver(cx: &mut Cx, n: Node) {
    let Some(name) = n.child_by_field_name("name") else { return };
    if !n.child_by_field_name("object").is_some_and(|o| o.kind() == "super") {
        return;
    }
    let pos = cx.start(n);
    let method = cx.text(name).to_owned();
    cx.facts.call_names.push(CallNameFact {
        line: (pos >> 32) as u32,
        col: pos as u32,
        tail: method.clone(),
        kernel_name: method.clone(),
        name: format!("super.{method}"),
    });
}

/// `<T extends Foo, U>` of a class or method: `T` names no project declaration.
fn type_params(cx: &mut Cx, n: Node, scope: u32) {
    let Some(tp) = n.child_by_field_name("type_parameters") else { return };
    for p in kids(tp).filter(|p| p.kind() == "type_parameter") {
        if let Some(nm) = kids(p).find(|c| matches!(c.kind(), "type_identifier" | "identifier")) {
            let t = cx.text(nm).to_owned();
            let line = cx.line(nm);
            cx.add_binding(scope, &t, BindKind::TypeParam, "", "", line);
        }
    }
}

fn class(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    type_params(cx, n, sc);
    // record components `record R(Foo a)`
    if let Some(params) = n.child_by_field_name("parameters") {
        for p in kids(params) {
            if p.kind() == "formal_parameter" {
                let ty = p.child_by_field_name("type").map(|t| java_type_name(cx, t)).unwrap_or_default();
                if let Some(nm) = p.child_by_field_name("name") {
                    let t = cx.text(nm).to_owned();
                    let line = cx.line(nm);
                    cx.add_binding(sc, &t, BindKind::Field, &ty, "", line);
                }
            }
        }
    }
    let Some(body) = n.child_by_field_name("body") else { return };
    for m in kids(body) {
        match m.kind() {
            "field_declaration" => {
                declare(cx, m, sc, BindKind::Field);
                walk_children(cx, m, sc);
            }
            "enum_constant" => {}
            _ => walk(cx, m, sc),
        }
    }
}

fn function(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    type_params(cx, n, sc);
    if let Some(params) = n.child_by_field_name("parameters") {
        for p in kids(params) {
            match p.kind() {
                "formal_parameter" | "receiver_parameter" => {
                    let ty = p.child_by_field_name("type").map(|t| java_type_name(cx, t)).unwrap_or_default();
                    if let Some(nm) = p.child_by_field_name("name") {
                        let t = cx.text(nm).to_owned();
                        let line = cx.line(nm);
                        cx.add_binding(sc, &t, BindKind::Param, &ty, "", line);
                    }
                }
                "spread_parameter" => {
                    if let Some(vd) = kids(p).find(|c| c.kind() == "variable_declarator") {
                        if let Some(nm) = vd.child_by_field_name("name") {
                            let t = cx.text(nm).to_owned();
                            let line = cx.line(nm);
                            cx.add_binding(sc, &t, BindKind::Param, "", "", line);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
    }
}

fn lambda(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(p) = n.child_by_field_name("parameters") {
        match p.kind() {
            "identifier" => {
                let t = cx.text(p).to_owned();
                let line = cx.line(p);
                cx.add_binding(sc, &t, BindKind::Param, "", "", line);
            }
            _ => {
                for i in kids(p) {
                    let nm = match i.kind() {
                        "identifier" => Some(i),
                        "formal_parameter" => i.child_by_field_name("name"),
                        _ => None,
                    };
                    if let Some(nm) = nm {
                        let ty = if i.kind() == "formal_parameter" {
                            i.child_by_field_name("type").map(|t| java_type_name(cx, t)).unwrap_or_default()
                        } else {
                            String::new()
                        };
                        let t = cx.text(nm).to_owned();
                        let line = cx.line(nm);
                        cx.add_binding(sc, &t, BindKind::Param, &ty, "", line);
                    }
                }
            }
        }
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
    }
}
