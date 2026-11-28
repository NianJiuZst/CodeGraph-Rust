//! ASP.NET Core route extraction: Controller attribute routing (`[Route]`, `[HttpGet]`,
//! `[AcceptVerbs]`, `[Area]`, `[controller]`/`[action]`/`[area]` tokens) and Minimal API
//! endpoints (`MapGet`/`MapPost`/`MapPut`/`MapDelete`/`MapPatch`/`MapMethods` with `MapGroup`).

use super::routes::{call_args, ident_path_re, Handler, Lines, Route};
use regex::Regex;
use std::sync::OnceLock;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

re!(csharp_class_re, r"\bclass\s+(@?[A-Za-z_]\w*)");
re!(minimal_group_re, r"\b([A-Za-z_]\w*)\s*=\s*([A-Za-z_]\w*)\s*\.\s*MapGroup\s*\(");
re!(minimal_map_re, r"\.\s*(MapGet|MapPost|MapPut|MapDelete|MapPatch|MapMethods)\s*\(");

const HTTP_VERBS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

/// Produce two same-length, newline-preserving views of C# source:
/// - `clean`: `//` (including `///`) and `/* */` comments blanked with spaces; strings intact.
/// - `code`: comments AND string/char literal interiors blanked with spaces (delimiters kept),
///   so structural scans for `class`, `[...]`, `{...}`, and `.MapGet(` never match inside strings.
fn prepare_csharp(src: &str) -> (String, String) {
    let b = src.as_bytes();
    let mut clean = b.to_vec();
    let mut code = b.to_vec();
    let mut i = 0;

    let blank_byte = |buf: &mut Vec<u8>, idx: usize| {
        if buf[idx] != b'\n' && buf[idx] != b'\r' {
            buf[idx] = b' ';
        }
    };

    while i < b.len() {
        // Line comment (`//` or `///`)
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                blank_byte(&mut clean, i);
                blank_byte(&mut code, i);
                i += 1;
            }
            continue;
        }
        // Block comment (`/* ... */`)
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            clean[i] = b' ';
            clean[i + 1] = b' ';
            code[i] = b' ';
            code[i + 1] = b' ';
            i += 2;
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                blank_byte(&mut clean, i);
                blank_byte(&mut code, i);
                i += 1;
            }
            if i < b.len() {
                blank_byte(&mut clean, i);
                blank_byte(&mut code, i);
                if i + 1 < b.len() {
                    blank_byte(&mut clean, i + 1);
                    blank_byte(&mut code, i + 1);
                }
                i += 2;
            }
            continue;
        }
        // C# 11 raw string literal (`"""..."""`, 3 or more quotes)
        if b[i] == b'"' && b.get(i + 1) == Some(&b'"') && b.get(i + 2) == Some(&b'"') {
            let mut q = 0usize;
            while i + q < b.len() && b[i + q] == b'"' {
                q += 1;
            }
            i += q;
            while i < b.len() {
                if b[i] == b'"' {
                    let mut close = 0usize;
                    while i + close < b.len() && b[i + close] == b'"' {
                        close += 1;
                    }
                    if close >= q {
                        i += close;
                        break;
                    }
                    for k in 0..close {
                        blank_byte(&mut code, i + k);
                    }
                    i += close;
                    continue;
                }
                blank_byte(&mut code, i);
                i += 1;
            }
            continue;
        }
        // Verbatim string (`@"..."` or `@$"..."`; `$@"..."` hits `@"` after skipping `$`)
        if b[i] == b'@' && (b.get(i + 1) == Some(&b'"') || (b.get(i + 1) == Some(&b'$') && b.get(i + 2) == Some(&b'"'))) {
            i += if b.get(i + 1) == Some(&b'$') { 3 } else { 2 };
            while i < b.len() {
                if b[i] == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        blank_byte(&mut code, i);
                        blank_byte(&mut code, i + 1);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                blank_byte(&mut code, i);
                i += 1;
            }
            continue;
        }
        // Normal string (`"..."`) or char literal (`'...'`)
        if b[i] == b'"' || b[i] == b'\'' {
            let q = b[i];
            i += 1;
            while i < b.len() && b[i] != q && b[i] != b'\n' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    blank_byte(&mut code, i);
                    blank_byte(&mut code, i + 1);
                    i += 2;
                } else {
                    blank_byte(&mut code, i);
                    i += 1;
                }
            }
            if i < b.len() && b[i] == q {
                i += 1;
            }
            continue;
        }
        i += 1;
    }

    let clean = String::from_utf8(clean).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    let code = String::from_utf8(code).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    (clean, code)
}

/// Parse a single-line non-interpolated C# string literal (`"..."` or `@"..."`).
fn parse_csharp_string(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.starts_with('$') || s.starts_with("@$") {
        return None;
    }
    if let Some(rest) = s.strip_prefix("@\"") {
        let inner = rest.strip_suffix('"')?;
        if inner.contains('\n') || inner.contains('\r') {
            return None;
        }
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '"' {
                if chars.next() != Some('"') {
                    return None;
                }
                out.push('"');
            } else {
                out.push(c);
            }
        }
        return Some(out);
    }
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('\n') || inner.contains('\r') {
        return None;
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            '"' => return None,
            other => out.push(other),
        }
    }
    Some(out)
}

fn find_balanced(code: &str, open: usize, open_ch: u8, close_ch: u8, max_len: usize) -> Option<usize> {
    let b = code.as_bytes();
    if b.get(open) != Some(&open_ch) {
        return None;
    }
    let mut depth = 0i32;
    let limit = open.saturating_add(max_len).min(b.len());
    let mut i = open;
    while i < limit {
        if b[i] == open_ch {
            depth += 1;
        } else if b[i] == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

fn split_top_level(code: &str, start: usize, end: usize, sep: u8) -> Vec<(usize, usize)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = start;
    let mut i = start;
    while i < end {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = (depth - 1).max(0),
            c if c == sep && depth == 0 => {
                out.push((cur, i));
                cur = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if cur < end {
        out.push((cur, end));
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_csharp_keyword(s: &str) -> bool {
    matches!(
        s,
        "abstract"
            | "as"
            | "async"
            | "await"
            | "base"
            | "bool"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "checked"
            | "class"
            | "const"
            | "continue"
            | "decimal"
            | "default"
            | "delegate"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "event"
            | "explicit"
            | "extern"
            | "false"
            | "file"
            | "finally"
            | "fixed"
            | "float"
            | "for"
            | "foreach"
            | "goto"
            | "if"
            | "implicit"
            | "in"
            | "int"
            | "interface"
            | "internal"
            | "is"
            | "lock"
            | "long"
            | "namespace"
            | "new"
            | "null"
            | "object"
            | "operator"
            | "out"
            | "override"
            | "params"
            | "partial"
            | "private"
            | "protected"
            | "public"
            | "readonly"
            | "record"
            | "ref"
            | "return"
            | "sbyte"
            | "sealed"
            | "short"
            | "sizeof"
            | "stackalloc"
            | "static"
            | "string"
            | "struct"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "uint"
            | "ulong"
            | "unchecked"
            | "unsafe"
            | "ushort"
            | "using"
            | "virtual"
            | "void"
            | "volatile"
            | "where"
            | "while"
    )
}

#[derive(Debug)]
enum AttrKind {
    Verb(&'static str, Option<String>),
    AcceptVerbs(Vec<String>, Option<String>),
    Route(String),
    Area(String),
}

#[derive(Debug)]
struct ParsedAttr {
    offset: usize,
    kind: AttrKind,
}

fn parse_attr_item(clean: &str, code: &str, start: usize, end: usize) -> Option<ParsedAttr> {
    let slice_code = &code[start..end];
    let lead = slice_code.len() - slice_code.trim_start().len();
    let item_start = start + lead;
    let trimmed_code = code[item_start..end].trim_end();
    if trimmed_code.is_empty() {
        return None;
    }
    let item_end = item_start + trimmed_code.len();

    // Strip optional attribute target specifier (`method: HttpGet(...)`, `type: Route(...)`).
    let mut name_start = item_start;
    if let Some(colon) = trimmed_code.find(':') {
        let prefix = trimmed_code[..colon].trim();
        if !prefix.is_empty() && prefix.bytes().all(is_ident_byte) && !trimmed_code[..colon].contains('(') {
            let after_colon = item_start + colon + 1;
            let skip = code[after_colon..item_end].len() - code[after_colon..item_end].trim_start().len();
            name_start = after_colon + skip;
        }
    }

    let (raw_name, arg_span) = match code[name_start..item_end].find('(') {
        Some(rel_open) => {
            let open = name_start + rel_open;
            let close = find_balanced(code, open, b'(', b')', 2000)?;
            (&code[name_start..open], Some((open + 1, close - 1)))
        }
        None => (&code[name_start..item_end], None),
    };

    let short = raw_name.trim().rsplit('.').next()?.trim();
    let short = short.strip_suffix("Attribute").unwrap_or(short);

    let mut positional: Vec<String> = Vec::new();
    let mut named: Vec<(&str, String)> = Vec::new();
    if let Some((a_start, a_end)) = arg_span {
        for (s, e) in split_top_level(code, a_start, a_end, b',') {
            let seg_code = &code[s..e];
            if let Some(eq) = seg_code.find('=') {
                let key = seg_code[..eq].trim();
                let val = &clean[s + eq + 1..e];
                if let Some(lit) = parse_csharp_string(val) {
                    named.push((key, lit));
                }
            } else if let Some(colon) = seg_code.find(':') {
                let key = seg_code[..colon].trim();
                let val = &clean[s + colon + 1..e];
                if !key.is_empty() && key.bytes().all(is_ident_byte) {
                    if let Some(lit) = parse_csharp_string(val) {
                        named.push((key, lit));
                    }
                }
            } else if let Some(lit) = parse_csharp_string(&clean[s..e]) {
                positional.push(lit);
            }
        }
    }

    let template = named
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("template") || k.eq_ignore_ascii_case("route"))
        .map(|(_, v)| v.clone())
        .or_else(|| positional.first().cloned());

    let kind = match short {
        "HttpGet" => AttrKind::Verb("GET", template),
        "HttpPost" => AttrKind::Verb("POST", template),
        "HttpPut" => AttrKind::Verb("PUT", template),
        "HttpDelete" => AttrKind::Verb("DELETE", template),
        "HttpPatch" => AttrKind::Verb("PATCH", template),
        "HttpHead" => AttrKind::Verb("HEAD", template),
        "HttpOptions" => AttrKind::Verb("OPTIONS", template),
        "AcceptVerbs" => {
            let verbs: Vec<String> = positional.into_iter().map(|v| v.to_ascii_uppercase()).filter(|v| HTTP_VERBS.contains(&v.as_str())).collect();
            if verbs.is_empty() {
                return None;
            }
            let route_tpl = named.iter().find(|(k, _)| *k == "Route" || *k == "Template").map(|(_, v)| v.clone());
            AttrKind::AcceptVerbs(verbs, route_tpl)
        }
        "Route" => AttrKind::Route(template?),
        "Area" => AttrKind::Area(positional.into_iter().next()?),
        _ => return None,
    };

    Some(ParsedAttr { offset: name_start, kind })
}

fn parse_attr_block(clean: &str, code: &str, open_bracket: usize, close_bracket: usize, out: &mut Vec<ParsedAttr>) {
    for (s, e) in split_top_level(code, open_bracket + 1, close_bracket - 1, b',') {
        if let Some(attr) = parse_attr_item(clean, code, s, e) {
            out.push(attr);
        }
    }
}

struct CSharpClass {
    name: String,
    body_start: usize,
    body_end: usize,
    routes: Vec<String>,
    area: Option<String>,
}

/// Walk backwards from a `class` keyword over C# class modifiers and contiguous `[...]` attribute blocks.
fn preceding_class_attrs(clean: &str, code: &str, class_kw: usize) -> Vec<ParsedAttr> {
    let b = code.as_bytes();
    let mut pos = class_kw;

    // Skip class modifiers immediately before `class`.
    loop {
        while pos > 0 && b[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        let end = pos;
        while pos > 0 && is_ident_byte(b[pos - 1]) {
            pos -= 1;
        }
        let word = &code[pos..end];
        if matches!(word, "public" | "private" | "protected" | "internal" | "static" | "sealed" | "abstract" | "partial" | "new" | "file" | "unsafe" | "record")
        {
            continue;
        }
        pos = end;
        break;
    }

    // Collect contiguous `[...]` attribute lists going backwards.
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    loop {
        while pos > 0 && b[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        if pos == 0 || b[pos - 1] != b']' {
            break;
        }
        let close = pos;
        let mut depth = 0i32;
        let mut i = pos;
        let mut found = None;
        while i > 0 {
            i -= 1;
            if b[i] == b']' {
                depth += 1;
            } else if b[i] == b'[' {
                depth -= 1;
                if depth == 0 {
                    found = Some(i);
                    break;
                }
            }
        }
        let Some(open) = found else { break };
        blocks.push((open, close));
        pos = open;
    }
    blocks.reverse();

    let mut attrs = Vec::new();
    for (open, close) in blocks {
        parse_attr_block(clean, code, open, close, &mut attrs);
    }
    attrs
}

fn find_classes(clean: &str, code: &str) -> Vec<CSharpClass> {
    let b = code.as_bytes();
    let mut classes = Vec::new();

    for m in csharp_class_re().captures_iter(code) {
        let whole = m.get(0).unwrap();
        let name_m = m.get(1).unwrap();
        let raw_name = name_m.as_str().trim_start_matches('@');
        if is_csharp_keyword(raw_name) {
            continue;
        }
        // Reject generic type constraints (`where T : class`).
        let mut prev = whole.start();
        while prev > 0 && b[prev - 1].is_ascii_whitespace() {
            prev -= 1;
        }
        if prev > 0 && matches!(b[prev - 1], b':' | b',' | b'.') {
            continue;
        }

        // Advance past generic params, primary constructor, base list, and `where` clauses to `{`.
        let mut i = name_m.end();
        let mut paren = 0i32;
        let mut angle = 0i32;
        let mut open_brace = None;
        while i < b.len() {
            match b[i] {
                b'(' => paren += 1,
                b')' => paren = (paren - 1).max(0),
                b'<' if paren == 0 => angle += 1,
                b'>' if paren == 0 => angle = (angle - 1).max(0),
                b'{' if paren == 0 && angle == 0 => {
                    open_brace = Some(i);
                    break;
                }
                b';' | b'}' if paren == 0 && angle == 0 => break,
                _ => {}
            }
            i += 1;
        }
        let Some(body_start) = open_brace else { continue };
        let Some(body_end) = find_balanced(code, body_start, b'{', b'}', usize::MAX) else { continue };

        let attrs = preceding_class_attrs(clean, code, whole.start());
        let mut routes = Vec::new();
        let mut area = None;
        for a in attrs {
            match a.kind {
                AttrKind::Route(tpl) => routes.push(tpl),
                AttrKind::Area(val) => area = Some(val),
                _ => {}
            }
        }
        classes.push(CSharpClass { name: raw_name.to_owned(), body_start, body_end, routes, area });
    }
    classes
}

/// Find the method name identifier of a C# method declaration starting at `from`.
/// Returns `None` for properties, fields, constructors, events, or nested type declarations.
fn csharp_method_after(code: &str, from: usize, class_name: &str) -> Option<(usize, usize)> {
    let b = code.as_bytes();
    let mut i = from;
    let mut last_ident: Option<(usize, usize)> = None;
    let mut candidate: Option<(usize, usize)> = None;
    let mut tokens_before_candidate = 0usize;
    let mut token_count = 0usize;

    while i < b.len() {
        let ch = b[i];
        if ch.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if ch == b'{' || ch == b';' || ch == b'}' || (ch == b'=' && b.get(i + 1) == Some(&b'>')) {
            break;
        }
        if ch == b'<' {
            i = find_balanced(code, i, b'<', b'>', 1000)?;
            token_count += 1;
            continue;
        }
        if ch == b'[' {
            i = find_balanced(code, i, b'[', b']', 1000)?;
            continue;
        }
        if ch == b'(' {
            let close = find_balanced(code, i, b'(', b')', usize::MAX)?;
            if let Some(id) = last_ident {
                // Record `ident (...)` as the method name; if this was actually a tuple return
                // type `(int A, string B) Get(...)`, a later `Get(...)` overwrites it.
                candidate = Some(id);
                tokens_before_candidate = token_count.saturating_sub(1);
                last_ident = None;
            } else {
                // Tuple return type like `(int Id, string Name) GetPair(...)`.
                token_count += 1;
            }
            i = close;
            continue;
        }
        if ch == b'@' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') {
            let start = i + 1;
            let mut end = start + 1;
            while end < b.len() && is_ident_byte(b[end]) {
                end += 1;
            }
            last_ident = Some((start, end));
            token_count += 1;
            i = end;
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == b'_' {
            let start = i;
            let mut end = i + 1;
            while end < b.len() && is_ident_byte(b[end]) {
                end += 1;
            }
            let word = &code[start..end];
            if candidate.is_none() && matches!(word, "class" | "struct" | "interface" | "record" | "enum" | "namespace" | "delegate" | "event") {
                return None;
            }
            if word == "where" && candidate.is_some() {
                break;
            }
            if !is_csharp_keyword(word) {
                last_ident = Some((start, end));
            }
            if !matches!(
                word,
                "public"
                    | "private"
                    | "protected"
                    | "internal"
                    | "static"
                    | "async"
                    | "virtual"
                    | "override"
                    | "sealed"
                    | "abstract"
                    | "extern"
                    | "unsafe"
                    | "new"
                    | "partial"
                    | "readonly"
            ) {
                token_count += 1;
            }
            i = end;
            continue;
        }
        // `.`, `?`, `,`, `*`, etc. (part of qualified/nullable/pointer return types)
        i += 1;
    }

    let (ms, me) = candidate?;
    let name = &code[ms..me];
    // Reject constructors (same name as enclosing class) and bare calls without a return type.
    if name == class_name || tokens_before_candidate == 0 {
        return None;
    }
    Some((ms, me))
}

fn controller_token(class_name: &str) -> String {
    let stripped = class_name.strip_suffix("Controller").filter(|s| !s.is_empty()).unwrap_or(class_name);
    stripped.to_ascii_lowercase()
}

fn action_token(method_name: &str) -> String {
    let stripped = method_name.strip_suffix("Async").filter(|s| !s.is_empty()).unwrap_or(method_name);
    stripped.to_ascii_lowercase()
}

fn replace_token_ci(input: &str, token: &str, replacement: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cur = 0;
    while let Some(rel) = lower[cur..].find(token) {
        out.push_str(&input[cur..cur + rel]);
        out.push_str(replacement);
        cur += rel + token.len();
    }
    out.push_str(&input[cur..]);
    out
}

fn expand_aspnet_tokens(template: &str, class_name: &str, area: Option<&str>, method_name: &str) -> String {
    let s = replace_token_ci(template, "[controller]", &controller_token(class_name));
    let s = replace_token_ci(&s, "[action]", &action_token(method_name));
    match area {
        Some(a) => replace_token_ci(&s, "[area]", &a.to_ascii_lowercase()),
        None => s,
    }
}

fn normalize_route_path(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix("~/").unwrap_or(trimmed);
    let mut out = String::with_capacity(stripped.len() + 1);
    for seg in stripped.split('/') {
        if seg.is_empty() {
            continue;
        }
        out.push('/');
        out.push_str(seg);
    }
    if out.is_empty() {
        "/".to_owned()
    } else {
        out
    }
}

/// Join a Minimal API `MapGroup` prefix with a sub-route pattern. Unlike Controller `[Http*]`
/// attributes, a leading `/` in `group.MapGet("/items", ...)` appends to the group prefix.
fn join_group_path(prefix: &str, sub: &str) -> String {
    let sub = sub.trim();
    if sub.starts_with("~/") {
        return normalize_route_path(sub);
    }
    let p = prefix.trim().strip_prefix("~/").unwrap_or(prefix.trim()).trim_end_matches('/');
    let s = sub.trim_start_matches('/');
    if p.is_empty() {
        normalize_route_path(s)
    } else if s.is_empty() {
        normalize_route_path(p)
    } else {
        normalize_route_path(&format!("{p}/{s}"))
    }
}

fn join_route_paths(prefix: &str, sub: &str) -> String {
    let sub = sub.trim();
    if sub.starts_with('/') || sub.starts_with("~/") {
        return normalize_route_path(sub);
    }
    let p = prefix.trim().strip_prefix("~/").unwrap_or(prefix.trim()).trim_end_matches('/');
    let s = sub.trim_start_matches('/');
    if p.is_empty() {
        normalize_route_path(s)
    } else if s.is_empty() {
        normalize_route_path(p)
    } else {
        normalize_route_path(&format!("{p}/{s}"))
    }
}

fn resolve_controller_paths(class_prefixes: &[String], tpl: Option<&str>, class_name: &str, area: Option<&str>, method_name: &str) -> Vec<String> {
    if let Some(sub) = tpl {
        let sub_trim = sub.trim();
        if sub_trim.starts_with('/') || sub_trim.starts_with("~/") {
            let expanded = expand_aspnet_tokens(sub_trim, class_name, area, method_name);
            return vec![normalize_route_path(&expanded)];
        }
        if !class_prefixes.is_empty() {
            return class_prefixes
                .iter()
                .map(|p| {
                    let combined = join_route_paths(p, sub_trim);
                    let expanded = expand_aspnet_tokens(&combined, class_name, area, method_name);
                    normalize_route_path(&expanded)
                })
                .collect();
        }
        let expanded = expand_aspnet_tokens(sub_trim, class_name, area, method_name);
        return vec![normalize_route_path(&expanded)];
    }
    if !class_prefixes.is_empty() {
        return class_prefixes
            .iter()
            .map(|p| {
                let expanded = expand_aspnet_tokens(p, class_name, area, method_name);
                normalize_route_path(&expanded)
            })
            .collect();
    }
    // No class `[Route]` and no method template (conventional MVC action): do not fabricate `/`.
    Vec::new()
}

fn scan_controller_routes(clean: &str, code: &str, lines: &Lines, classes: &[CSharpClass], out: &mut Vec<Route>) {
    let b = code.as_bytes();
    for cls in classes {
        let mut i = cls.body_start + 1;
        let end = cls.body_end.saturating_sub(1);
        while i < end {
            match b[i] {
                b'{' => {
                    // Skip method, property, or nested type bodies (nested classes are scanned on their own pass).
                    i = find_balanced(code, i, b'{', b'}', usize::MAX).unwrap_or(end);
                }
                b'(' => {
                    i = find_balanced(code, i, b'(', b')', usize::MAX).unwrap_or(i + 1);
                }
                b'[' => {
                    // Collect a contiguous group of `[...]` member attributes at direct class depth.
                    let mut attrs = Vec::new();
                    let mut pos = i;
                    while pos < end && b[pos] == b'[' {
                        let Some(close) = find_balanced(code, pos, b'[', b']', 2000) else {
                            pos += 1;
                            break;
                        };
                        parse_attr_block(clean, code, pos, close, &mut attrs);
                        pos = close;
                        while pos < end && b[pos].is_ascii_whitespace() {
                            pos += 1;
                        }
                    }
                    i = pos;

                    let mut verb_attrs: Vec<(String, Option<String>, usize)> = Vec::new();
                    let mut method_routes: Vec<(String, usize)> = Vec::new();
                    for a in attrs {
                        match a.kind {
                            AttrKind::Verb(v, tpl) => verb_attrs.push((v.to_owned(), tpl, a.offset)),
                            AttrKind::AcceptVerbs(vs, tpl) => {
                                for v in vs {
                                    verb_attrs.push((v, tpl.clone(), a.offset));
                                }
                            }
                            AttrKind::Route(tpl) => method_routes.push((tpl, a.offset)),
                            AttrKind::Area(_) => {}
                        }
                    }
                    if verb_attrs.is_empty() && method_routes.is_empty() {
                        continue;
                    }
                    let Some((ms, me)) = csharp_method_after(code, pos, &cls.name) else { continue };
                    let method_name = &code[ms..me];
                    let (hline, hcol) = lines.at(clean, ms);

                    if !verb_attrs.is_empty() {
                        for (verb, tpl, off) in verb_attrs {
                            let (line, _) = lines.at(clean, off);
                            if let Some(ref t) = tpl {
                                for path in resolve_controller_paths(&cls.routes, Some(t), &cls.name, cls.area.as_deref(), method_name) {
                                    out.push(Route {
                                        method: verb.clone(),
                                        path,
                                        line,
                                        handlers: vec![Handler { name: method_name.to_owned(), line: hline, col: hcol }],
                                        ..Default::default()
                                    });
                                }
                            } else if !method_routes.is_empty() {
                                for (rtpl, _) in &method_routes {
                                    for path in resolve_controller_paths(&cls.routes, Some(rtpl), &cls.name, cls.area.as_deref(), method_name) {
                                        out.push(Route {
                                            method: verb.clone(),
                                            path,
                                            line,
                                            handlers: vec![Handler { name: method_name.to_owned(), line: hline, col: hcol }],
                                            ..Default::default()
                                        });
                                    }
                                }
                            } else {
                                for path in resolve_controller_paths(&cls.routes, None, &cls.name, cls.area.as_deref(), method_name) {
                                    out.push(Route {
                                        method: verb.clone(),
                                        path,
                                        line,
                                        handlers: vec![Handler { name: method_name.to_owned(), line: hline, col: hcol }],
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                    } else {
                        for (rtpl, off) in method_routes {
                            let (line, _) = lines.at(clean, off);
                            for path in resolve_controller_paths(&cls.routes, Some(&rtpl), &cls.name, cls.area.as_deref(), method_name) {
                                out.push(Route {
                                    method: "ANY".to_owned(),
                                    path,
                                    line,
                                    handlers: vec![Handler { name: method_name.to_owned(), line: hline, col: hcol }],
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
                _ => i += 1,
            }
        }
    }
}

struct GroupBinding {
    var: String,
    prefix: String,
    scope_start: usize,
    scope_end: usize,
}

/// Find the innermost `{ ... }` block enclosing `pos` in `code` (or the whole file for
/// top-level statements).
fn enclosing_brace_span(code: &str, pos: usize) -> (usize, usize) {
    let b = code.as_bytes();
    let mut depth = 0i32;
    let mut i = pos;
    while i > 0 {
        i -= 1;
        if b[i] == b'}' {
            depth += 1;
        } else if b[i] == b'{' {
            if depth == 0 {
                let end = find_balanced(code, i, b'{', b'}', usize::MAX).unwrap_or(b.len());
                return (i, end);
            }
            depth -= 1;
        }
    }
    (0, b.len())
}

fn lookup_group<'a>(groups: &'a [GroupBinding], recv: &str, pos: usize) -> Option<&'a str> {
    groups.iter().rev().find(|g| g.var == recv && g.scope_start <= pos && pos < g.scope_end).map(|g| g.prefix.as_str())
}

/// Resolve the `MapGroup` prefix chain immediately preceding `.MapGet(...)` at `dot_pos`.
fn receiver_group_prefix(clean: &str, code: &str, dot_pos: usize, groups: &[GroupBinding]) -> Option<String> {
    let b = code.as_bytes();
    let mut pos = dot_pos;
    let mut chained_prefixes: Vec<String> = Vec::new();

    for _ in 0..8 {
        while pos > 0 && b[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        if pos == 0 {
            break;
        }
        if b[pos - 1] == b')' {
            // Walk back across balanced `(...)` of a chained call (`MapGroup("/api").WithTags("x")`).
            let close = pos;
            let mut depth = 0i32;
            let mut i = close;
            let mut open = None;
            while i > 0 {
                i -= 1;
                if b[i] == b')' {
                    depth += 1;
                } else if b[i] == b'(' {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
            }
            let Some(open) = open else { break };
            let mut name_end = open;
            while name_end > 0 && b[name_end - 1].is_ascii_whitespace() {
                name_end -= 1;
            }
            let mut name_start = name_end;
            while name_start > 0 && is_ident_byte(b[name_start - 1]) {
                name_start -= 1;
            }
            let callee = &code[name_start..name_end];
            if callee == "MapGroup" {
                if let Some((args, _)) = call_args(code, open) {
                    if let Some((a0_off, a0_txt)) = args.first() {
                        if let Some(p) = parse_csharp_string(&clean[*a0_off..*a0_off + a0_txt.len()]) {
                            chained_prefixes.push(p);
                        }
                    }
                }
            }
            let mut before = name_start;
            while before > 0 && b[before - 1].is_ascii_whitespace() {
                before -= 1;
            }
            if before > 0 && b[before - 1] == b'.' {
                pos = before - 1;
                continue;
            }
            break;
        }
        if is_ident_byte(b[pos - 1]) {
            let end = pos;
            let mut start = end;
            while start > 0 && is_ident_byte(b[start - 1]) {
                start -= 1;
            }
            let recv = &code[start..end];
            if let Some(base) = lookup_group(groups, recv, dot_pos) {
                chained_prefixes.push(base.to_owned());
            }
            break;
        }
        break;
    }

    if chained_prefixes.is_empty() {
        return None;
    }
    chained_prefixes.reverse();
    let mut acc = String::new();
    for p in chained_prefixes {
        acc = join_group_path(&acc, &p);
    }
    Some(acc)
}

fn minimal_handler(clean: &str, lines: &Lines, off: usize, raw: &str) -> Vec<Handler> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !ident_path_re().is_match(trimmed) || is_csharp_keyword(trimmed) {
        return Vec::new();
    }
    let lead = raw.len() - raw.trim_start().len();
    let (line, col) = lines.at(clean, off + lead);
    vec![Handler { name: trimmed.to_owned(), line, col }]
}

fn parse_http_method_token(raw: &str) -> Option<String> {
    if let Some(lit) = parse_csharp_string(raw) {
        let upper = lit.to_ascii_uppercase();
        return HTTP_VERBS.contains(&upper.as_str()).then_some(upper);
    }
    // `HttpMethods.Get`, `Microsoft.AspNetCore.Http.HttpMethods.Options`, etc.
    let trimmed = raw.trim();
    let (prefix, member) = trimmed.rsplit_once('.')?;
    if prefix.rsplit('.').next() == Some("HttpMethods") {
        let upper = member.trim().to_ascii_uppercase();
        if HTTP_VERBS.contains(&upper.as_str()) {
            return Some(upper);
        }
    }
    None
}

fn extract_method_verbs(clean: &str, code: &str, start: usize, end: usize) -> Vec<String> {
    let mut verbs = Vec::new();
    let slice = &clean[start..end];
    // Prefer `{ ... }` (`new[] { "GET", "HEAD" }`, `new string[] { ... }`) over `[ ... ]`
    // (C# 12 collection expressions `["GET", "HEAD"]`) so `new[]` brackets don't steal the span.
    let (inner_start, inner_end) = if let (Some(lo), Some(hi)) = (slice.find('{'), slice.rfind('}')) {
        (start + lo + 1, start + hi)
    } else if let (Some(lo), Some(hi)) = (slice.find('['), slice.rfind(']')) {
        (start + lo + 1, start + hi)
    } else {
        (start, end)
    };
    if inner_start >= inner_end {
        return verbs;
    }
    for (s, e) in split_top_level(code, inner_start, inner_end, b',') {
        if let Some(verb) = parse_http_method_token(&clean[s..e]) {
            verbs.push(verb);
        }
    }
    verbs
}

fn scan_minimal_api_routes(clean: &str, code: &str, lines: &Lines, out: &mut Vec<Route>) {
    enum Event {
        Group { pos: usize, var: String, recv: String, open: usize },
        Map { pos: usize, kind: &'static str, open: usize },
    }

    let mut events: Vec<Event> = Vec::new();
    for m in minimal_group_re().captures_iter(code) {
        let whole = m.get(0).unwrap();
        let open = whole.end() - 1;
        events.push(Event::Group { pos: whole.start(), var: m[1].to_owned(), recv: m[2].to_owned(), open });
    }
    for m in minimal_map_re().captures_iter(code) {
        let whole = m.get(0).unwrap();
        let open = whole.end() - 1;
        let kind = match &m[1] {
            "MapGet" => "GET",
            "MapPost" => "POST",
            "MapPut" => "PUT",
            "MapDelete" => "DELETE",
            "MapPatch" => "PATCH",
            "MapMethods" => "METHODS",
            _ => continue,
        };
        events.push(Event::Map { pos: whole.start(), kind, open });
    }
    events.sort_by_key(|e| match e {
        Event::Group { pos, .. } | Event::Map { pos, .. } => *pos,
    });

    let mut groups: Vec<GroupBinding> = Vec::new();
    let b = code.as_bytes();
    for ev in events {
        match ev {
            Event::Group { pos, var, recv, open } => {
                let Some((args, mut after)) = call_args(code, open) else { continue };
                let Some((a0_off, a0_txt)) = args.first() else { continue };
                let Some(prefix) = parse_csharp_string(&clean[*a0_off..*a0_off + a0_txt.len()]) else { continue };
                let mut full = match lookup_group(&groups, &recv, pos) {
                    Some(parent) => join_group_path(parent, &prefix),
                    None => normalize_route_path(&prefix),
                };
                // Follow chained `.WithTags(...).MapGroup("/sub")` on the same assignment.
                for _ in 0..8 {
                    while after < b.len() && b[after].is_ascii_whitespace() {
                        after += 1;
                    }
                    if b.get(after) != Some(&b'.') {
                        break;
                    }
                    after += 1;
                    while after < b.len() && b[after].is_ascii_whitespace() {
                        after += 1;
                    }
                    let id_start = after;
                    while after < b.len() && is_ident_byte(b[after]) {
                        after += 1;
                    }
                    let callee = &code[id_start..after];
                    while after < b.len() && b[after].is_ascii_whitespace() {
                        after += 1;
                    }
                    if b.get(after) != Some(&b'(') {
                        break;
                    }
                    let Some((c_args, next_after)) = call_args(code, after) else { break };
                    if callee == "MapGroup" {
                        if let Some((ca0_off, ca0_txt)) = c_args.first() {
                            if let Some(sub) = parse_csharp_string(&clean[*ca0_off..*ca0_off + ca0_txt.len()]) {
                                full = join_group_path(&full, &sub);
                            }
                        }
                    }
                    after = next_after;
                }
                let (scope_start, scope_end) = enclosing_brace_span(code, pos);
                groups.push(GroupBinding { var, prefix: full, scope_start, scope_end });
            }
            Event::Map { pos, kind, open } => {
                let Some((args, _)) = call_args(code, open) else { continue };
                let Some((a0_off, a0_txt)) = args.first() else { continue };
                let Some(tpl) = parse_csharp_string(&clean[*a0_off..*a0_off + a0_txt.len()]) else { continue };
                let prefix = receiver_group_prefix(clean, code, pos, &groups);
                let path = match prefix.as_deref() {
                    Some(p) => join_group_path(p, &tpl),
                    None => normalize_route_path(&tpl),
                };
                let (line, _) = lines.at(clean, pos);

                if kind == "METHODS" {
                    if args.len() < 3 {
                        continue;
                    }
                    let (m_off, m_txt) = args[1];
                    let verbs = extract_method_verbs(clean, code, m_off, m_off + m_txt.len());
                    let (h_off, h_txt) = args[2];
                    let handlers = minimal_handler(clean, lines, h_off, &clean[h_off..h_off + h_txt.len()]);
                    for verb in verbs {
                        out.push(Route {
                            method: verb,
                            path: path.clone(),
                            line,
                            handlers: handlers.iter().map(|h| Handler { name: h.name.clone(), line: h.line, col: h.col }).collect(),
                            ..Default::default()
                        });
                    }
                } else {
                    if args.len() < 2 {
                        continue;
                    }
                    let (h_off, h_txt) = args[1];
                    let handlers = minimal_handler(clean, lines, h_off, &clean[h_off..h_off + h_txt.len()]);
                    out.push(Route { method: kind.to_owned(), path, line, handlers, ..Default::default() });
                }
            }
        }
    }
}

pub fn scan_aspnet_core(src: &str) -> Vec<Route> {
    let (clean, code) = prepare_csharp(src);
    let lines = Lines::new(&clean);
    let classes = find_classes(&clean, &code);
    let mut out = Vec::new();
    scan_controller_routes(&clean, &code, &lines, &classes, &mut out);
    scan_minimal_api_routes(&clean, &code, &lines, &mut out);
    out.sort_by_key(|r| r.line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(r: &[Route]) -> Vec<String> {
        r.iter().map(|r| format!("{} {} -> {}", r.method, r.path, r.handlers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join("|"))).collect()
    }

    #[test]
    fn controller_attributes_tokens_overrides_and_named_args() {
        let src = r#"
[ApiController]
[Route("api/[controller]")]
public class OrdersController : ControllerBase
{
    [HttpGet(Name = "ListOrders")]
    public Task<List<Order>> ListAsync() => default!;

    [Authorize]
    [HttpGet("{id:int}", Name = "GetOrder")]
    [ProducesResponseType(200)]
    public async Task<ActionResult<Order>> GetById(int id) => default!;

    [HttpPost("/v2/orders")]
    public IActionResult Create([FromBody] CreateOrderDto dto) => Ok();

    [HttpPut("~/legacy/[action]/{id}")]
    public Task UpdateOrderAsync(int id) => Task.CompletedTask;

    [AcceptVerbs("GET", "HEAD", Route = "status")]
    public IActionResult Status() => Ok();

    [HttpDelete, Route("remove/{id}")]
    public IActionResult Remove(int id) => Ok();

    [Route("fallback/{id}")]
    public IActionResult Fallback(int id) => Ok();
}
"#;
        assert_eq!(
            names(&scan_aspnet_core(src)),
            vec![
                "GET /api/orders -> ListAsync",
                "GET /api/orders/{id:int} -> GetById",
                "POST /v2/orders -> Create",
                "PUT /legacy/updateorder/{id} -> UpdateOrderAsync",
                "GET /api/orders/status -> Status",
                "HEAD /api/orders/status -> Status",
                "DELETE /api/orders/remove/{id} -> Remove",
                "ANY /api/orders/fallback/{id} -> Fallback",
            ]
        );
    }

    #[test]
    fn multi_class_area_comma_attributes_and_negative_cases() {
        let src = r#"
/// <example>
/// [HttpGet("/from-xml-doc")]
/// public void DocFake() {}
/// </example>
// [HttpPost("/from-comment")]
/* [HttpDelete("/from-block")] */
[Area("Admin")]
[Route("[area]/v1/[controller]")]
[Route("[area]/v2/[controller]")]
public class UsersController : ControllerBase
{
    const string Doc = """
        [HttpGet("/from-raw-string")]
        public void RawFake() {}
        """;
    const string Verbatim = @"[HttpGet(""/from-verbatim"")]
        public void VerbatimFake() {}";

    [Authorize, HttpGet("me")]
    public User Current() => new();

    [HttpGet("not-a-prop")]
    public string Prop { get; set; } = "";
}

// Conventional MVC controller without [Route]: bare [HttpPost] must not fabricate POST /
public class LegacyController : Controller
{
    [HttpPost]
    public IActionResult Submit() => Ok();

    [HttpGet("explicit")]
    public IActionResult Explicit() => Ok();
}
"#;
        assert_eq!(names(&scan_aspnet_core(src)), vec!["GET /admin/v1/users/me -> Current", "GET /admin/v2/users/me -> Current", "GET /explicit -> Explicit"]);
    }

    #[test]
    fn minimal_api_groups_methods_and_lambdas() {
        let src = r#"
public static class OrderEndpoints
{
    public static void Map(WebApplication app)
    {
        var api = app.MapGroup("/api");
        var orders = api.MapGroup("/orders").RequireAuthorization();
        var nested = app.MapGroup("/v2").WithTags("V2").MapGroup("/items");
        orders.MapGet("/{id:int}", OrderHandlers.GetById);
        orders.MapPost("", CreateOrder);
        nested.MapGet("/{id}", GetItem);
        app.MapGroup("/admin").WithTags("Admin").MapDelete("/cache", ClearCache);
        app.MapGet("/health", () => "ok");
        app.MapMethods("/ping", new[] { "GET", "HEAD" }, Ping);
        app.MapMethods("/ping2", ["GET", "OPTIONS"], Ping);
        app.MapMethods("/check", [HttpMethods.Options, Microsoft.AspNetCore.Http.HttpMethods.Head], Check);
        app.MapGet($"/dynamic/{prefix}", ShouldIgnoreInterpolated);
    }

    // Separate method reusing parameter name `orders` must NOT inherit the group prefix from `Map`.
    public static void MapOther(IEndpointRouteBuilder orders)
    {
        orders.MapGet("/standalone", Standalone);
    }

    public static void MapGet(string pattern, Delegate handler) {}
}
"#;
        assert_eq!(
            names(&scan_aspnet_core(src)),
            vec![
                "GET /api/orders/{id:int} -> OrderHandlers.GetById",
                "POST /api/orders -> CreateOrder",
                "GET /v2/items/{id} -> GetItem",
                "DELETE /admin/cache -> ClearCache",
                "GET /health -> ",
                "GET /ping -> Ping",
                "HEAD /ping -> Ping",
                "GET /ping2 -> Ping",
                "OPTIONS /ping2 -> Ping",
                "OPTIONS /check -> Check",
                "HEAD /check -> Check",
                "GET /standalone -> Standalone",
            ]
        );
    }

    #[test]
    fn handles_large_parameter_lists_over_4kb() {
        let params: String = (0..120).map(|i| format!("        [FromQuery] string? param{i}")).collect::<Vec<_>>().join(",\n");
        let src = format!(
            "[Route(\"api/[controller]\")]\npublic class ItemsController : ControllerBase\n{{\n    [HttpGet(\"search\")]\n    public Task<string> Search(\n{params}\n    ) => Task.FromResult(\"\");\n}}\n"
        );
        assert!(src.len() > 4500);
        assert_eq!(names(&scan_aspnet_core(&src)), vec!["GET /api/items/search -> Search"]);
    }

    #[test]
    fn primary_ctor_named_ctor_arg_qualified_attr_tuples_generics_and_ctor_rejection() {
        let src = r#"
[Route(template: "api/[controller]")]
public class ReportsController(IReportService svc) : ControllerBase
{
    [HttpGet("should-not-match-ctor")]
    public ReportsController() : this(null!) {}

    [Microsoft.AspNetCore.Mvc.HttpGetAttribute(template: "by-key/{key}")]
    public (int Id, string Name) GetPair(string key) => (1, key);

    [HttpPost("generic/{id}")]
    public Task<T> Fetch<T>(int id) where T : class => default!;
}
"#;
        assert_eq!(names(&scan_aspnet_core(src)), vec!["GET /api/reports/by-key/{key} -> GetPair", "POST /api/reports/generic/{id} -> Fetch"]);
    }
}
