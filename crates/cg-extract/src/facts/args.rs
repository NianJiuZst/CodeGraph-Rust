//! Argument shapes of call sites, for overload selection (upstream `javaOverloadsFitting`):
//! a call's arguments decide which of several same-named methods it reaches. Each argument
//! is read from syntax alone and stored as one short string:
//!
//! * `T:<type>` - a literal, `new T(..)` or a cast: the static type as written
//! * `V:<name>` - a local, parameter or field, typed at resolution from its binding
//! * `F:<name>` - `this.<name>`: a field of the enclosing type
//! * `C:<name>` - a call inside `satisfies`, typed from an unambiguous return annotation
//! * `#this`    - `this`
//! * `N`        - `null` / `undefined`
//! * `L<k>`     - a lambda / function literal taking `k` parameters; `L` alone: a method
//!   reference, whose parameter count the text does not say
//! * `S` / `W`  - C++ only: a narrow / wide string literal
//! * Swift: each argument's label, `_` for none
//! * `?`        - anything else (a call, an operator, ...): fits every parameter
//!
//! A spread argument (`...xs`) or a named one (Kotlin `f(n = 1)`) makes the positional
//! reading unreliable, and such a call records nothing.

use super::util::{kids, Cx};
use super::CallArgsFact;
use cg_core::text::unpack_pos;
use tree_sitter::Node;

/// First item of a reference's candidate list that carries argument shapes.
pub const ARGS_MARK: &str = "#args";

/// More arguments than this are not worth a list (and no overload set differs that late).
const MAX_ARGS: usize = 24;

fn push(cx: &mut Cx, at: Node, tail: &str, args: Vec<String>) {
    if tail.is_empty() || args.len() > MAX_ARGS {
        return;
    }
    let (line, col) = unpack_pos(cx.start(at));
    cx.facts.call_args.push(CallArgsFact { line, col, tail: tail.to_owned(), args });
}

fn lambda(params: usize) -> String {
    format!("L{params}")
}

// ------------------------------------------------------------------- Java

/// `foo(a, "x")`, `obj.foo(1)`, `this.foo()`, `super.foo()`: the kernel's reference sits at
/// the start of the `method_invocation`.
pub fn java_call(cx: &mut Cx, call: Node) {
    let Some(name) = call.child_by_field_name("name") else { return };
    let Some(list) = call.child_by_field_name("arguments") else { return };
    let tail = cx.text(name).to_owned();
    let mut args = Vec::new();
    for a in kids(list) {
        if !a.is_named() || a.kind().ends_with("comment") {
            continue;
        }
        args.push(java_shape(cx, a));
    }
    push(cx, call, &tail, args);
}

fn java_type(cx: &Cx, t: Node) -> String {
    match t.kind() {
        "generic_type" => t.named_child(0).map(|c| java_type(cx, c)).unwrap_or_default(),
        "annotated_type" => kids(t).find(|c| !matches!(c.kind(), "annotation" | "marker_annotation")).map(|c| java_type(cx, c)).unwrap_or_default(),
        "array_type" => {
            let el = t.child_by_field_name("element").map(|c| java_type(cx, c)).unwrap_or_default();
            if el.is_empty() {
                String::new()
            } else {
                format!("{el}[]")
            }
        }
        "type_identifier" if cx.text(t) == "var" => String::new(),
        "type_identifier" | "scoped_type_identifier" | "integral_type" | "floating_point_type" | "boolean_type" => cx.text(t).to_owned(),
        _ => String::new(),
    }
}

fn java_shape(cx: &Cx, a: Node) -> String {
    let text = cx.text(a);
    let typed = |t: &str| format!("T:{t}");
    match a.kind() {
        "string_literal" | "text_block" => typed("String"),
        "character_literal" => typed("char"),
        "decimal_integer_literal" | "hex_integer_literal" | "octal_integer_literal" | "binary_integer_literal" => {
            typed(if text.ends_with(['l', 'L']) { "long" } else { "int" })
        }
        "decimal_floating_point_literal" | "hex_floating_point_literal" => typed(if text.ends_with(['f', 'F']) { "float" } else { "double" }),
        "true" | "false" => typed("boolean"),
        "null_literal" => "N".into(),
        "this" => "#this".into(),
        "identifier" => format!("V:{text}"),
        "object_creation_expression" => match a.child_by_field_name("type").map(|t| java_type(cx, t)) {
            Some(t) if !t.is_empty() => typed(&t),
            _ => "?".into(),
        },
        "array_creation_expression" => match a.child_by_field_name("type").map(|t| java_type(cx, t)) {
            Some(t) if !t.is_empty() => typed(&format!("{t}[]")),
            _ => "?".into(),
        },
        "cast_expression" => match a.child_by_field_name("type").map(|t| java_type(cx, t)) {
            Some(t) if !t.is_empty() => typed(&t),
            _ => "?".into(),
        },
        "field_access" => match (a.child_by_field_name("object"), a.child_by_field_name("field")) {
            (Some(o), Some(f)) if o.kind() == "this" => format!("F:{}", cx.text(f)),
            _ => "?".into(),
        },
        "lambda_expression" => {
            let n = match a.child_by_field_name("parameters") {
                Some(p) if p.kind() == "identifier" => 1,
                Some(p) => kids(p).filter(|c| c.is_named() && !c.kind().ends_with("comment")).count(),
                None => 0,
            };
            lambda(n)
        }
        "method_reference" => "L".into(),
        "parenthesized_expression" => a.named_child(0).map(|c| java_shape(cx, c)).unwrap_or_else(|| "?".into()),
        "unary_expression" => match a.child_by_field_name("operand") {
            // `-1`, `-2.5`: the operand's numeric type
            Some(o) if text.starts_with(['-', '+']) && o.kind().contains("literal") => java_shape(cx, o),
            _ => "?".into(),
        },
        _ => "?".into(),
    }
}

// ------------------------------------------------------------------- Kotlin

/// `f(a, b)`, `obj.f(1)`, `f(1) { .. }`: the kernel's reference sits at the start of the
/// `call_expression` (the inner one, for a trailing lambda).
pub fn kotlin_call(cx: &mut Cx, call: Node) {
    let mut c = call.walk();
    let parts: Vec<Node> = call.named_children(&mut c).collect();
    let (Some(callee), Some(suffix)) = (parts.first().copied(), parts.get(1).copied()) else { return };
    if suffix.kind() != "call_suffix" {
        return;
    }
    let tail = match callee.kind() {
        "simple_identifier" => cx.text(callee).to_owned(),
        "navigation_expression" => match kids(callee).filter(|k| k.kind() == "navigation_suffix").last() {
            Some(s) => kids(s).find(|k| k.kind() == "simple_identifier").map(|k| cx.text(k).to_owned()).unwrap_or_default(),
            None => String::new(),
        },
        _ => String::new(),
    };
    let mut args = Vec::new();
    for part in kids(suffix) {
        match part.kind() {
            "value_arguments" => {
                for va in kids(part).filter(|k| k.kind() == "value_argument") {
                    let mut vc = va.walk();
                    let inner: Vec<Node> = va.named_children(&mut vc).collect();
                    // `name = value` and `*spread` are not positional
                    if inner.len() != 1 || kids(va).any(|k| !k.is_named() && matches!(cx.text(k), "*" | "=")) {
                        return;
                    }
                    args.push(kotlin_shape(cx, inner[0]));
                }
            }
            "annotated_lambda" => args.push(kotlin_lambda(cx, part)),
            _ => {}
        }
    }
    // `f(1) { .. }` parses as a call of the call `f(1)` with the lambda: it is one more
    // argument of the inner call.
    if let Some(parent) = call.parent() {
        if parent.kind() == "call_expression" && parent.named_child(0) == Some(call) {
            if let Some(outer) = parent.named_child(1).filter(|s| s.kind() == "call_suffix") {
                if kids(outer).all(|k| !k.is_named() || k.kind() == "annotated_lambda") {
                    for l in kids(outer).filter(|k| k.kind() == "annotated_lambda") {
                        args.push(kotlin_lambda(cx, l));
                    }
                }
            }
        }
    }
    push(cx, call, &tail, args);
}

fn kotlin_lambda(cx: &Cx, n: Node) -> String {
    let lit = if n.kind() == "lambda_literal" { Some(n) } else { kids(n).find(|k| k.kind() == "lambda_literal") };
    let Some(lit) = lit else { return "?".into() };
    let params = kids(lit).find(|k| k.kind() == "lambda_parameters").map_or(0, |p| kids(p).filter(|k| k.is_named()).count());
    let _ = cx;
    // `{ it }` takes one implicit parameter or none: the count is unknown
    if params == 0 {
        "L".into()
    } else {
        lambda(params)
    }
}

fn kotlin_shape(cx: &Cx, a: Node) -> String {
    let text = cx.text(a);
    let typed = |t: &str| format!("T:{t}");
    match a.kind() {
        "string_literal" | "line_string_literal" | "multi_line_string_literal" => typed("String"),
        "character_literal" => typed("Char"),
        "integer_literal" | "hex_literal" | "bin_literal" => typed("Int"),
        "long_literal" => typed("Long"),
        "real_literal" => typed(if text.ends_with(['f', 'F']) { "Float" } else { "Double" }),
        "boolean_literal" => typed("Boolean"),
        "null_literal" => "N".into(),
        "this_expression" => "#this".into(),
        "simple_identifier" if text == "null" => "N".into(),
        "simple_identifier" => format!("V:{text}"),
        "lambda_literal" | "annotated_lambda" => kotlin_lambda(cx, a),
        "anonymous_function" => "L".into(),
        "callable_reference" => "L".into(),
        "parenthesized_expression" => a.named_child(0).map(|c| kotlin_shape(cx, c)).unwrap_or_else(|| "?".into()),
        "as_expression" => match a.named_child(1) {
            Some(t) => typed(cx.text(t).trim_end_matches('?')),
            None => "?".into(),
        },
        "object_literal" => {
            let sup = super::kotlin::object_literal_super(cx, a);
            if sup.is_empty() {
                "?".into()
            } else {
                typed(&sup)
            }
        }
        "call_expression" => match a.named_child(0) {
            Some(c) if c.kind() == "simple_identifier" && cx.text(c).starts_with(|ch: char| ch.is_ascii_uppercase()) => typed(cx.text(c)),
            Some(c) if c.kind() == "navigation_expression" => {
                let tail = kids(c)
                    .filter(|k| k.kind() == "navigation_suffix")
                    .last()
                    .and_then(|s| kids(s).find(|k| k.kind() == "simple_identifier"))
                    .map(|k| cx.text(k))
                    .unwrap_or("");
                if tail.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                    typed(tail)
                } else {
                    "?".into()
                }
            }
            _ => "?".into(),
        },
        "prefix_expression" if text.starts_with(['-', '+']) => match a.named_child(a.named_child_count().saturating_sub(1)) {
            Some(o) if o.kind().ends_with("literal") => kotlin_shape(cx, o),
            _ => "?".into(),
        },
        _ => "?".into(),
    }
}

// ------------------------------------------------------------------- TypeScript / JavaScript

/// `f(a)`, `obj.f(1)`, `this.f()`: the kernel's reference sits at the call, or at its callee
/// when an `await` sits inside the callee; both positions are recorded.
pub fn js_call(cx: &mut Cx, call: Node) {
    let Some(func) = call.child_by_field_name("function") else { return };
    let Some(list) = call.child_by_field_name("arguments") else { return };
    if list.kind() != "arguments" {
        return; // a tagged template
    }
    let tail = match func.kind() {
        "identifier" => cx.text(func).to_owned(),
        "member_expression" => func.child_by_field_name("property").map(|p| cx.text(p).to_owned()).unwrap_or_default(),
        _ => return,
    };
    let mut args = Vec::new();
    for a in kids(list) {
        if !a.is_named() || a.kind() == "comment" {
            continue;
        }
        if a.kind() == "spread_element" {
            return;
        }
        args.push(js_shape(cx, a));
    }
    if cx.start(func) != cx.start(call) {
        push(cx, func, &tail, args.clone());
    }
    push(cx, call, &tail, args);
}

fn js_type(cx: &Cx, t: Node) -> String {
    match t.kind() {
        "type_annotation" | "parenthesized_type" => t.named_child(0).map(|c| js_type(cx, c)).unwrap_or_default(),
        "predefined_type" | "type_identifier" | "nested_type_identifier" => cx.text(t).to_owned(),
        "generic_type" => t.child_by_field_name("name").map(|c| cx.text(c).to_owned()).unwrap_or_default(),
        _ => String::new(),
    }
}

fn js_shape(cx: &Cx, a: Node) -> String {
    js_shape_inner(cx, a, false)
}

fn js_shape_inner(cx: &Cx, a: Node, in_satisfies: bool) -> String {
    let typed = |t: &str| format!("T:{t}");
    match a.kind() {
        "string" | "template_string" => typed("string"),
        "number" => typed(if cx.text(a).ends_with('n') { "bigint" } else { "number" }),
        "true" | "false" => typed("boolean"),
        "null" | "undefined" => "N".into(),
        "this" => "#this".into(),
        "identifier" if cx.text(a) == "undefined" => "N".into(),
        "identifier" => format!("V:{}", cx.text(a)),
        "member_expression" => match (a.child_by_field_name("object"), a.child_by_field_name("property")) {
            (Some(o), Some(p)) if o.kind() == "this" && p.kind() == "property_identifier" => format!("F:{}", cx.text(p)),
            _ => "?".into(),
        },
        "new_expression" => match a.child_by_field_name("constructor") {
            Some(c) if matches!(c.kind(), "identifier" | "member_expression") => typed(cx.text(c)),
            _ => "?".into(),
        },
        "arrow_function" | "function_expression" | "function" => {
            let n = if a.child_by_field_name("parameter").is_some() {
                1
            } else {
                a.child_by_field_name("parameters").map_or(0, |p| kids(p).filter(|c| c.is_named() && c.kind() != "comment").count())
            };
            lambda(n)
        }
        "parenthesized_expression" | "non_null_expression" => a.named_child(0).map(|c| js_shape_inner(cx, c, in_satisfies)).unwrap_or_else(|| "?".into()),
        "satisfies_expression" => a.named_child(0).map(|c| js_shape_inner(cx, c, true)).unwrap_or_else(|| "?".into()),
        "call_expression" if in_satisfies => match a.child_by_field_name("function") {
            Some(f) if f.kind() == "identifier" => format!("C:{}", cx.text(f)),
            _ => "?".into(),
        },
        "as_expression" => match a.named_child(1).map(|t| js_type(cx, t)) {
            Some(t) if !t.is_empty() => typed(&t),
            _ => "?".into(),
        },
        "unary_expression" => match a.child_by_field_name("argument") {
            Some(o) if o.kind() == "number" && matches!(cx.text(a).as_bytes().first(), Some(b'-' | b'+')) => {
                typed(if cx.text(o).ends_with('n') { "bigint" } else { "number" })
            }
            _ => "?".into(),
        },
        _ => "?".into(),
    }
}

// ------------------------------------------------------------------- C++

/// Every call in the file. C++ reads no types here: the count and the string literals are what
/// `overload::cpp_fit` weighs.
/// Swift `f(a, label: b)`, `x.f(..)`: the labels, when the call writes its `( .. )` (a call
/// with a trailing closure only says nothing of its labels).
pub fn swift_calls(cx: &mut Cx, root: Node) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        stack.extend(kids(n));
        if n.kind() != "call_expression" {
            continue;
        }
        let Some(callee) = n.named_child(0) else { continue };
        let name = match callee.kind() {
            "simple_identifier" => Some(callee),
            "navigation_expression" => match callee.named_child(1) {
                Some(s) if s.kind() == "navigation_suffix" => kids(s).find(|g| g.kind() == "simple_identifier"),
                other => other.filter(|s| s.kind() == "simple_identifier"),
            },
            _ => None,
        };
        let Some(name) = name else { continue };
        let Some(list) = kids(n).find(|c| c.kind() == "call_suffix").and_then(|s| kids(s).find(|c| c.kind() == "value_arguments")) else { continue };
        if !cx.text(list).starts_with('(') {
            continue;
        }
        let labels = kids(list).filter(|a| a.kind() == "value_argument").map(|a| swift_label(cx.text(a)).to_owned()).collect();
        let tail = cx.text(name).to_owned();
        push(cx, n, &tail, labels);
    }
}

fn swift_label(arg: &str) -> &str {
    let t = arg.trim_start();
    let end = t.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(t.len());
    let rest = t[end..].trim_start();
    if end > 0 && rest.starts_with(':') && !rest.starts_with("::") {
        &t[..end]
    } else {
        "_"
    }
}

pub fn cpp_calls(cx: &mut Cx, root: Node) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "call_expression" {
            cpp_call(cx, n);
        }
        stack.extend(kids(n));
    }
}

/// `f(a)`, `ns::f<T>(a)`, `x.f(a)`: the kernel's reference sits at the start of the call.
fn cpp_call(cx: &mut Cx, call: Node) {
    let (Some(mut name), Some(list)) = (call.child_by_field_name("function"), call.child_by_field_name("arguments")) else { return };
    while let Some(inner) = match name.kind() {
        "qualified_identifier" | "template_function" | "template_method" => name.child_by_field_name("name"),
        "field_expression" => name.child_by_field_name("field"),
        _ => None,
    } {
        name = inner;
    }
    let tail = cx.text(name).to_owned();
    let args = kids(list).filter(|a| a.is_named() && a.kind() != "comment").map(|a| cpp_shape(cx.text(a)).to_owned()).collect();
    push(cx, call, &tail, args);
}

fn cpp_shape(arg: &str) -> &'static str {
    let body = ["u8", "u", "U", "L"].iter().find_map(|p| arg.strip_prefix(p)).unwrap_or(arg);
    if body.strip_prefix('R').unwrap_or(body).starts_with('"') {
        if arg.starts_with('L') {
            "W"
        } else {
            "S"
        }
    } else if arg.strip_prefix("FMT_STRING").is_some_and(|r| r.trim_start().starts_with('(')) {
        "S"
    } else {
        "?"
    }
}
