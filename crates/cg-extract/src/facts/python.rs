//! Facts for Python: imports, function / class scopes, parameters with annotations, assignments
//! (module variables, locals, class attributes, `self.x = ...` fields).

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
        "import_statement" => import_stmt(cx, n, scope),
        "import_from_statement" => from_import(cx, n, scope),
        "function_definition" => function(cx, n, scope),
        "lambda" => lambda(cx, n, scope),
        "class_definition" => class(cx, n, scope),
        "assignment" => {
            assignment(cx, n, scope);
            walk_children(cx, n, scope);
        }
        "for_statement" | "for_in_clause" => {
            if let Some(l) = n.child_by_field_name("left") {
                bind_targets(cx, l, scope);
            }
            walk_children(cx, n, scope);
        }
        "as_pattern" => {
            let target =
                n.child_by_field_name("alias").or_else(|| n.named_child(n.named_child_count().saturating_sub(1)).filter(|c| c.kind() == "as_pattern_target"));
            if let Some(a) = target {
                let single_id = if a.kind() == "identifier" {
                    Some(a)
                } else if a.kind() == "as_pattern_target" && a.named_child_count() == 1 {
                    a.named_child(0).filter(|c| c.kind() == "identifier")
                } else {
                    None
                };
                if let (Some(id), Some(expr)) = (single_id, n.named_child(0).filter(|c| c.id() != a.id())) {
                    let (vt, init) = value_info(cx, expr, scope);
                    let nm = cx.text(id).to_owned();
                    let line = cx.line(id);
                    cx.add_binding(scope, &nm, BindKind::Local, &vt, &init, line);
                } else {
                    bind_targets(cx, a, scope);
                }
            }
            walk_children(cx, n, scope);
        }
        "named_expression" => {
            if let Some(nm) = n.child_by_field_name("name") {
                if nm.kind() == "identifier" {
                    let (vt, init) = n.child_by_field_name("value").map(|v| value_info(cx, v, scope)).unwrap_or_default();
                    let text = cx.text(nm).to_owned();
                    let line = cx.line(nm);
                    cx.add_binding(scope, &text, BindKind::Local, &vt, &init, line);
                } else {
                    bind_targets(cx, nm, scope);
                }
            }
            walk_children(cx, n, scope);
        }
        _ => walk_children(cx, n, scope),
    }
}

fn bind_targets(cx: &mut Cx, n: Node, scope: u32) {
    match n.kind() {
        "identifier" => {
            let nm = cx.text(n).to_owned();
            let line = cx.line(n);
            cx.add_binding(scope, &nm, BindKind::Local, "", "", line);
        }
        "as_pattern_target" | "pattern_list" | "tuple_pattern" | "list_pattern" | "list_splat_pattern" | "parenthesized_expression" => {
            for ch in kids(n) {
                bind_targets(cx, ch, scope);
            }
        }
        _ => {}
    }
}

/// `Optional[Foo]`, `Foo | None`, `"Foo"`, `list[Foo]` -> `Foo` / "" for builtins and containers.
pub fn py_type_name(text: &str) -> String {
    let mut t = text.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    for wrapper in ["Optional[", "Final[", "ClassVar[", "Annotated[", "Type[", "type["] {
        if let Some(inner) = t.strip_prefix(wrapper) {
            t = inner.trim_end_matches(']').split(',').next().unwrap_or("").trim();
        }
    }
    if let Some((a, b)) = t.split_once('|') {
        let (a, b) = (a.trim(), b.trim());
        t = if a == "None" {
            b
        } else if b == "None" {
            a
        } else {
            return String::new();
        };
    }
    let name = simple_type_name(t);
    match name.as_str() {
        "int" | "str" | "float" | "bool" | "bytes" | "list" | "dict" | "set" | "tuple" | "None" | "Any" | "object" | "List" | "Dict" | "Set" | "Tuple"
        | "Callable" | "Iterable" | "Iterator" | "Sequence" | "Mapping" => String::new(),
        _ => name,
    }
}

fn py_expr_path(cx: &Cx, n: Node, depth: u8) -> Option<String> {
    if depth >= 32 {
        return None;
    }
    match n.kind() {
        "identifier" => Some(cx.text(n).to_owned()),
        "attribute" => {
            let obj = n.child_by_field_name("object")?;
            let attr = n.child_by_field_name("attribute")?;
            let base = py_expr_path(cx, obj, depth + 1)?;
            Some(format!("{base}.{}", cx.text(attr)))
        }
        "call" => {
            let f = n.child_by_field_name("function")?;
            let path = py_expr_path(cx, f, depth + 1)?;
            Some(format!("{path}()"))
        }
        "await" | "parenthesized_expression" => n.named_child(0).and_then(|c| py_expr_path(cx, c, depth + 1)),
        _ => None,
    }
}

fn value_info(cx: &Cx, v: Node, scope: u32) -> (String, String) {
    match v.kind() {
        "call" => (String::new(), py_expr_path(cx, v, 0).unwrap_or_default()),
        "await" | "parenthesized_expression" => v.named_child(0).map(|c| value_info(cx, c, scope)).unwrap_or_default(),
        "identifier" => {
            let text = cx.text(v);
            // alias of another binding: copy what we know about it
            match cx.lookup_binding(scope, text) {
                Some((t, i)) => (t, i),
                // Class reference (`client_class = Client`, `except MyError as e:`):
                // kept in `init_ctor` so `cls.client_class()` or `e` resolves to the class.
                None if text.starts_with(|c: char| c.is_ascii_uppercase()) => (String::new(), text.to_owned()),
                None => Default::default(),
            }
        }
        "attribute" => {
            // `mod.Client` or `self.client`
            if let Some(p) = py_expr_path(cx, v, 0) {
                let tail = p.rsplit('.').next().unwrap_or("");
                if tail.starts_with(|c: char| c.is_ascii_uppercase()) || p.starts_with("self.") || p.starts_with("cls.") {
                    return (String::new(), p);
                }
            }
            Default::default()
        }
        _ => Default::default(),
    }
}

fn import_stmt(cx: &mut Cx, n: Node, scope: u32) {
    let line = cx.line(n);
    for nm in kids(n) {
        match nm.kind() {
            "dotted_name" => {
                let full = cx.text(nm).to_owned();
                let first = full.split('.').next().unwrap_or("").to_owned();
                cx.add_import(ImportFact { local: first, imported: "*".into(), spec: full, kind: ImportKind::Default, type_only: false, scope, line });
            }
            "aliased_import" => {
                let full = nm.child_by_field_name("name").map(|c| cx.text(c).to_owned()).unwrap_or_default();
                let alias = nm.child_by_field_name("alias").map(|c| cx.text(c).to_owned()).unwrap_or_default();
                cx.add_import(ImportFact { local: alias, imported: "*".into(), spec: full, kind: ImportKind::Default, type_only: false, scope, line });
            }
            _ => {}
        }
    }
}

fn from_import(cx: &mut Cx, n: Node, scope: u32) {
    let line = cx.line(n);
    let Some(m) = n.child_by_field_name("module_name") else { return };
    let spec = cx.text(m).split_whitespace().collect::<String>();
    let mut any = false;
    for nm in kids(n) {
        if nm.id() == m.id() {
            continue;
        }
        match nm.kind() {
            "dotted_name" => {
                any = true;
                let name = cx.text(nm).to_owned();
                cx.add_import(ImportFact { local: name.clone(), imported: name, spec: spec.clone(), kind: ImportKind::Named, type_only: false, scope, line });
            }
            "aliased_import" => {
                any = true;
                let name = nm.child_by_field_name("name").map(|c| cx.text(c).to_owned()).unwrap_or_default();
                let alias = nm.child_by_field_name("alias").map(|c| cx.text(c).to_owned()).unwrap_or_else(|| name.clone());
                cx.add_import(ImportFact { local: alias, imported: name, spec: spec.clone(), kind: ImportKind::Named, type_only: false, scope, line });
            }
            "wildcard_import" => {
                any = true;
                cx.add_import(ImportFact {
                    local: "*".into(),
                    imported: "*".into(),
                    spec: spec.clone(),
                    kind: ImportKind::Namespace,
                    type_only: false,
                    scope,
                    line,
                });
            }
            _ => {}
        }
    }
    let _ = any;
}

fn param_names(cx: &mut Cx, params: Node, sc: u32) {
    for p in kids(params) {
        match p.kind() {
            "identifier" => {
                let nm = cx.text(p).to_owned();
                let line = cx.line(p);
                cx.add_binding(sc, &nm, BindKind::Param, "", "", line);
            }
            "typed_parameter" | "typed_default_parameter" | "default_parameter" => {
                let ty = p.child_by_field_name("type").map(|t| py_type_name(cx.text(t))).unwrap_or_default();
                let name_node = p.child_by_field_name("name").or_else(|| p.named_child(0));
                if let Some(nn) = name_node {
                    let inner = if matches!(nn.kind(), "list_splat_pattern" | "dictionary_splat_pattern") { nn.named_child(0) } else { Some(nn) };
                    if let Some(i) = inner.filter(|i| i.kind() == "identifier") {
                        let nm = cx.text(i).to_owned();
                        let line = cx.line(p);
                        cx.add_binding(sc, &nm, BindKind::Param, &ty, "", line);
                    }
                }
            }
            "list_splat_pattern" | "dictionary_splat_pattern" => {
                if let Some(i) = p.named_child(0) {
                    let nm = cx.text(i).to_owned();
                    let line = cx.line(p);
                    cx.add_binding(sc, &nm, BindKind::Param, "", "", line);
                }
            }
            _ => {}
        }
    }
}

fn function(cx: &mut Cx, n: Node, scope: u32) {
    let row = cx.row_at(cx.start(n));
    if cx.facts.scopes[scope as usize].kind != ScopeKind::Class {
        if let Some(name) = n.child_by_field_name("name") {
            let nm = cx.text(name).to_owned();
            cx.add_binding(scope, &nm, BindKind::LocalFn, "", "", 0);
            cx.facts.bindings.last_mut().unwrap().node_row = row.unwrap_or(NONE);
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(params) = n.child_by_field_name("parameters") {
        param_names(cx, params, sc);
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
        if let (Some(row), Some(name)) = (row, n.child_by_field_name("name")) {
            let fixture = n
                .parent()
                .filter(|p| p.kind() == "decorated_definition")
                .is_some_and(|p| kids(p).any(|d| d.kind() == "decorator" && cx.text(d).contains("fixture")));
            let (decl_type, init_ctor) = if let Some(ret) = n.child_by_field_name("return_type") {
                let raw = cx.text(ret);
                let ty = py_type_name(raw);
                (if ty.is_empty() { raw.to_owned() } else { ty }, String::new())
            } else {
                inferred_return(cx, n, body, fixture).unwrap_or_default()
            };
            if fixture {
                // pytest hands a test what the fixture returns: the decorator changes nothing there
                cx.facts.bindings.push(BindingFact {
                    scope,
                    name: cx.text(name).to_owned(),
                    kind: BindKind::Fixture,
                    decl_type: decl_type.clone(),
                    init_ctor: init_ctor.clone(),
                    line: cx.line(n),
                    node_row: row,
                });
            }
            if !decl_type.is_empty() || !init_ctor.is_empty() {
                cx.facts.bindings.push(BindingFact {
                    scope: sc,
                    name: cx.text(name).to_owned(),
                    kind: BindKind::ReturnType,
                    decl_type,
                    init_ctor,
                    line: cx.line(n),
                    node_row: row,
                });
            }
        }
    }
    if let Some(dec) = n.child_by_field_name("decorators") {
        walk(cx, dec, scope);
    }
}

/// Infer only explicit return expressions, each a call. A final return rules out implicit
/// None; nested definitions, generators and arbitrary decorators must not supply evidence for
/// the enclosing function. Different calls in different branches (`return A()` / `return B()`)
/// are kept as `A()|B()`, for the resolver to type by their common base class.
fn inferred_return(cx: &Cx, function: Node, body: Node, fixture: bool) -> Option<(String, String)> {
    if kids(function).any(|n| n.kind() == "async") || (!fixture && function.parent().is_some_and(|p| p.kind() == "decorated_definition")) {
        return None;
    }
    if kids(body).filter(|n| n.kind() != "comment").last()?.kind() != "return_statement" {
        return None;
    }
    fn collect(cx: &Cx, n: Node, result: &mut Option<(String, String)>, depth: u8) -> Option<()> {
        if depth >= 64 {
            return None;
        }
        match n.kind() {
            "function_definition" | "class_definition" | "lambda" => return Some(()),
            "yield" => return None,
            "return_statement" => {
                let value = n.named_child(0)?;
                let inferred = match value.kind() {
                    "call" => {
                        let callee = value.child_by_field_name("function")?;
                        if !matches!(callee.kind(), "identifier" | "attribute") || cx.text(callee).len() > 4096 {
                            return None;
                        }
                        (String::new(), format!("{}()", cx.text(callee)))
                    }
                    _ => return None,
                };
                match result.as_mut() {
                    Some((_, previous)) if previous.split('|').any(|p| *p == inferred.1) => {}
                    Some((_, previous)) if previous.split('|').count() < 4 => {
                        previous.push('|');
                        previous.push_str(&inferred.1);
                    }
                    Some(_) => return None,
                    None => *result = Some(inferred),
                }
                return Some(());
            }
            _ => {}
        }
        for child in kids(n) {
            collect(cx, child, result, depth + 1)?;
        }
        Some(())
    }
    let mut result = None;
    collect(cx, body, &mut result, 0)?;
    result
}

fn lambda(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(params) = n.child_by_field_name("parameters") {
        param_names(cx, params, sc);
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
    }
}

fn class(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
    }
}

fn assignment(cx: &mut Cx, n: Node, scope: u32) {
    let Some(left) = n.child_by_field_name("left") else { return };
    let ty = n.child_by_field_name("type").map(|t| py_type_name(cx.text(t))).unwrap_or_default();
    let (vt, init) = n.child_by_field_name("right").map(|r| value_info(cx, r, scope)).unwrap_or_default();
    let decl = if ty.is_empty() { vt } else { ty };
    let line = cx.line(n);
    match left.kind() {
        "identifier" => {
            let nm = cx.text(left).to_owned();
            let kind = if cx.facts.scopes[scope as usize].kind == ScopeKind::Class { BindKind::Field } else { BindKind::Local };
            cx.add_binding(scope, &nm, kind, &decl, &init, line);
        }
        "attribute" => {
            let obj = left.child_by_field_name("object");
            let attr = left.child_by_field_name("attribute");
            if let (Some(obj), Some(attr)) = (obj, attr) {
                if obj.kind() == "identifier" && matches!(cx.text(obj), "self" | "cls") {
                    if let Some(cs) = cx.class_scope_of(scope) {
                        let nm = cx.text(attr).to_owned();
                        // keep the first typed assignment of a field
                        let already = cx
                            .facts
                            .bindings
                            .iter()
                            .any(|b| b.scope == cs && b.name == nm && b.kind == BindKind::Field && (!b.decl_type.is_empty() || !b.init_ctor.is_empty()));
                        if !(already && decl.is_empty() && init.is_empty()) {
                            cx.add_binding(cs, &nm, BindKind::Field, &decl, &init, line);
                        }
                    }
                }
            }
        }
        "pattern_list" | "tuple_pattern" | "list_pattern" => bind_targets(cx, left, scope),
        _ => {}
    }
}
