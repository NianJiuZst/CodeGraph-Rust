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
        "subscript" => {
            let val = n.child_by_field_name("value")?;
            let base = py_expr_path(cx, val, depth + 1)?;
            Some(format!("{base}[]"))
        }
        "await" | "parenthesized_expression" => n.named_child(0).and_then(|c| py_expr_path(cx, c, depth + 1)),
        _ => None,
    }
}

fn value_info(cx: &Cx, v: Node, scope: u32) -> (String, String) {
    match v.kind() {
        "call" => {
            let path = py_expr_path(cx, v, 0).unwrap_or_default();
            if matches!(path.as_str(), "self.__class__()" | "cls()") {
                return ("self".to_owned(), String::new());
            }
            (String::new(), path)
        }
        "subscript" => (String::new(), py_expr_path(cx, v, 0).unwrap_or_default()),
        "await" | "parenthesized_expression" => v.named_child(0).map(|c| value_info(cx, c, scope)).unwrap_or_default(),
        "identifier" => {
            let text = cx.text(v);
            if text == "self" {
                return ("self".to_owned(), String::new());
            }
            // alias of another binding: copy what we know about it
            match cx.lookup_binding(scope, text) {
                Some((t, i)) => (t, i),
                // Imported class or module singleton (`client_class = Client`, `apps = global_apps`,
                // `except MyError as e:`): kept in `init_ctor` for resolver lookup.
                None if !matches!(text, "None" | "True" | "False") => (String::new(), text.to_owned()),
                None => Default::default(),
            }
        }
        "attribute" => {
            // `mod.Client`, `self.client`, or `state.apps`
            if let Some(p) = py_expr_path(cx, v, 0) {
                let tail = p.rsplit('.').next().unwrap_or("");
                if tail.starts_with(|c: char| c.is_ascii_uppercase())
                    || p.starts_with("self.")
                    || p.starts_with("cls.")
                    || cx.lookup_binding(scope, p.split('.').next().unwrap_or("")).is_some()
                {
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

fn param_names(cx: &mut Cx, params: Node, sc: u32, outer_scope: u32) {
    for p in kids(params) {
        match p.kind() {
            "identifier" => {
                let nm = cx.text(p).to_owned();
                let line = cx.line(p);
                cx.add_binding(sc, &nm, BindKind::Param, "", "", line);
            }
            "typed_parameter" | "typed_default_parameter" | "default_parameter" => {
                let mut ty = p.child_by_field_name("type").map(|t| py_type_name(cx.text(t))).unwrap_or_default();
                let mut init = String::new();
                if ty.is_empty() {
                    if let Some(val) = p.child_by_field_name("value") {
                        let (vt, vi) = value_info(cx, val, outer_scope);
                        ty = vt;
                        init = vi;
                    }
                }
                let name_node = p.child_by_field_name("name").or_else(|| p.named_child(0));
                if let Some(nn) = name_node {
                    let inner = if matches!(nn.kind(), "list_splat_pattern" | "dictionary_splat_pattern") { nn.named_child(0) } else { Some(nn) };
                    if let Some(i) = inner.filter(|i| i.kind() == "identifier") {
                        let nm = cx.text(i).to_owned();
                        let line = cx.line(p);
                        cx.add_binding(sc, &nm, BindKind::Param, &ty, &init, line);
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

fn decorator_callee(cx: &Cx, dec: Node) -> Option<String> {
    let expr = dec.named_child(0)?;
    let target = if expr.kind() == "call" { expr.child_by_field_name("function")? } else { expr };
    py_expr_path(cx, target, 0)
}

fn is_property_decorator(path: &str) -> bool {
    matches!(path.rsplit('.').next(), Some("property" | "cached_property"))
}

fn is_transparent_decorator(path: &str) -> bool {
    matches!(
        path.rsplit('.').next(),
        Some("property" | "cached_property" | "classmethod" | "staticmethod" | "lru_cache" | "cache" | "override" | "abstractmethod" | "async_unsafe")
    )
}

fn function(cx: &mut Cx, n: Node, scope: u32) {
    let row = cx.row_at(cx.start(n));
    let in_class = cx.facts.scopes[scope as usize].kind == ScopeKind::Class;
    if !in_class {
        if let Some(name) = n.child_by_field_name("name") {
            let nm = cx.text(name).to_owned();
            cx.add_binding(scope, &nm, BindKind::LocalFn, "", "", 0);
            cx.facts.bindings.last_mut().unwrap().node_row = row.unwrap_or(NONE);
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    let mut dec_paths: Vec<String> = Vec::new();
    let mut fixture = false;
    if let Some(parent) = n.parent().filter(|p| p.kind() == "decorated_definition") {
        for d in kids(parent).filter(|c| c.kind() == "decorator") {
            if cx.text(d).contains("fixture") {
                fixture = true;
            }
            if let Some(path) = decorator_callee(cx, d) {
                if in_class && !is_transparent_decorator(&path) {
                    cx.add_binding(sc, "\x02dec", BindKind::Local, "", &path, 0);
                }
                dec_paths.push(path);
            }
        }
    }
    let is_prop = dec_paths.iter().any(|p| is_property_decorator(p));
    let transparent_decs = fixture || (!dec_paths.is_empty() && dec_paths.iter().all(|p| is_transparent_decorator(p)));

    if let Some(params) = n.child_by_field_name("parameters") {
        param_names(cx, params, sc, scope);
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
        if let (Some(row), Some(name)) = (row, n.child_by_field_name("name")) {
            let (decl_type, init_ctor) = if let Some(ret) = n.child_by_field_name("return_type") {
                let raw = cx.text(ret);
                let ty = py_type_name(raw);
                (if ty.is_empty() { raw.to_owned() } else { ty }, String::new())
            } else {
                inferred_return(cx, n, body, sc, transparent_decs).unwrap_or_default()
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
            if in_class && is_prop && (!decl_type.is_empty() || !init_ctor.is_empty()) {
                let nm = cx.text(name).to_owned();
                let already = cx
                    .facts
                    .bindings
                    .iter()
                    .any(|b| b.scope == scope && b.name == nm && b.kind == BindKind::Field && (!b.decl_type.is_empty() || !b.init_ctor.is_empty()));
                if !already {
                    cx.add_binding(scope, &nm, BindKind::Field, &decl_type, &init_ctor, cx.line(n));
                }
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

fn ends_with_return(body: Node) -> bool {
    let Some(last) = kids(body).filter(|n| n.kind() != "comment").last() else { return false };
    match last.kind() {
        "return_statement" => true,
        "with_statement" => last.child_by_field_name("body").is_some_and(ends_with_return),
        _ => false,
    }
}

/// Infer explicit return expressions (calls, `self`, `self.__class__(...)`, `self.attr`, or typed
/// locals). A final return rules out implicit None; nested definitions, generators and opaque
/// decorators must not supply evidence for the enclosing function. Different calls in different
/// branches (`return A()` / `return B()`) are kept as `A()|B()`, for the resolver to type by
/// their common base class.
fn inferred_return(cx: &Cx, function: Node, body: Node, sc: u32, transparent_decs: bool) -> Option<(String, String)> {
    if kids(function).any(|n| n.kind() == "async") || (!transparent_decs && function.parent().is_some_and(|p| p.kind() == "decorated_definition")) {
        return None;
    }
    if !ends_with_return(body) {
        return None;
    }
    fn collect(cx: &Cx, n: Node, sc: u32, result: &mut Option<(String, String)>, depth: u8) -> Option<()> {
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
                        let path = py_expr_path(cx, value, 0)?;
                        if path.len() > 4096 {
                            return None;
                        }
                        if matches!(path.as_str(), "self.__class__()" | "cls()") {
                            ("self".to_owned(), String::new())
                        } else {
                            (String::new(), path)
                        }
                    }
                    "identifier" => {
                        let t = cx.text(value);
                        if t == "self" {
                            ("self".to_owned(), String::new())
                        } else {
                            let (dt, ic) = cx.lookup_binding(sc, t)?;
                            if dt.is_empty() && ic.is_empty() {
                                return None;
                            }
                            (dt, ic)
                        }
                    }
                    "attribute" => {
                        let path = py_expr_path(cx, value, 0)?;
                        if path.len() > 4096 {
                            return None;
                        }
                        (String::new(), path)
                    }
                    _ => return None,
                };
                match result.as_mut() {
                    Some((prev_dt, prev_ic)) => {
                        if !prev_dt.is_empty() && prev_dt == &inferred.0 && inferred.1.is_empty() {
                            // same decl_type
                        } else if prev_dt.is_empty() && inferred.0.is_empty() && !inferred.1.is_empty() {
                            if prev_ic.split('|').any(|p| p == inferred.1) {
                                // already seen
                            } else if prev_ic.split('|').count() < 4 {
                                prev_ic.push('|');
                                prev_ic.push_str(&inferred.1);
                            } else {
                                return None;
                            }
                        } else {
                            return None;
                        }
                    }
                    None => *result = Some(inferred),
                }
                return Some(());
            }
            _ => {}
        }
        for child in kids(n) {
            collect(cx, child, sc, result, depth + 1)?;
        }
        Some(())
    }
    let mut result = None;
    collect(cx, body, sc, &mut result, 0)?;
    result
}

fn lambda(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(params) = n.child_by_field_name("parameters") {
        param_names(cx, params, sc, scope);
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

fn top_function_scope(cx: &Cx, mut scope: u32) -> Option<u32> {
    let mut top = None;
    while scope != 0 {
        let sc = &cx.facts.scopes[scope as usize];
        if sc.kind == ScopeKind::Function {
            top = Some(scope);
        }
        if sc.parent == scope {
            break;
        }
        scope = sc.parent;
    }
    top
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
                    } else if !decl.is_empty() || !init.is_empty() {
                        // Decorator wrapper injecting `self.<attr> = Expr` outside any class:
                        // attach to the outermost top-level decorator function so decorated methods
                        // can resolve `self.<attr>`.
                        if let Some(top_fn) = top_function_scope(cx, scope) {
                            let start = cx.facts.scopes[top_fn as usize].start;
                            if let Some(row) = cx.row_at(start) {
                                let nm = cx.text(attr).to_owned();
                                let already = cx.facts.bindings.iter().any(|b| b.node_row == row && b.name == nm && b.kind == BindKind::Field);
                                if !already {
                                    cx.facts.bindings.push(BindingFact {
                                        scope: top_fn,
                                        name: nm,
                                        kind: BindKind::Field,
                                        decl_type: decl,
                                        init_ctor: init,
                                        line,
                                        node_row: row,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        "pattern_list" | "tuple_pattern" | "list_pattern" => bind_targets(cx, left, scope),
        _ => {}
    }
}
