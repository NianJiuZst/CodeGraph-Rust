//! Route extraction for web frameworks (design doc 6.7): Express, Flask, FastAPI, Django, Gin,
//! Spring, Axum and Next.js file routes. A route becomes a `route` node (`GET /users/:id`) that
//! `references` its handler functions, so a request path can be followed to the code that
//! serves it. Scanners are pure functions over the source text.

use cg_core::{Lang, LangFamily, NodeKind, RefKind};
use cg_extract::post::node_digest;
use cg_extract::{ExtraNode, ExtraRef, FileHook, HookCtx, HookOut};
use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framework {
    Express,
    Flask,
    FastApi,
    Django,
    Gin,
    Spring,
    AspNetCore,
    Axum,
    Next,
    Nuxt,
    SvelteKit,
    Nest,
    ReactRouter,
    TanStack,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Handler {
    pub name: String,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Route {
    /// HTTP method, transport verb (`EVENT`, `WS`), or empty for a page named by its path.
    pub method: String,
    pub path: String,
    pub line: u32,
    pub handlers: Vec<Handler>,
    /// How the route reaches its handlers: a function value (`references`), or the page
    /// component it renders (`calls`).
    pub ref_kind: RefKind,
    /// The router that declared a page (`next`, `react-router`, `tanstack-router`), stored as
    /// the node's signature so the navigation pass finds its own routes.
    pub router: Option<&'static str>,
}

impl Default for Route {
    fn default() -> Self {
        Route { method: String::new(), path: String::new(), line: 0, handlers: Vec::new(), ref_kind: RefKind::FunctionRef, router: None }
    }
}

pub(super) struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    pub(super) fn new(src: &str) -> Lines {
        let mut starts = vec![0];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Lines { starts }
    }
    /// 1-based line and UTF-16 column of a byte offset.
    pub(super) fn at(&self, src: &str, off: usize) -> (u32, u32) {
        let line = self.starts.partition_point(|s| *s <= off) - 1;
        let start = self.starts[line];
        let end = src[start..].find('\n').map_or(src.len(), |e| start + e);
        (line as u32 + 1, cg_core::text::col16(&src[start..end], off - start))
    }
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

re!(express_chain_re, r#"\b([A-Za-z_$][\w$]*)\s*\.\s*route\s*\(\s*(?:['"`])(/[^'"`\n]*)['"`]\s*\)"#);
re!(express_link_re, r"^\s*\.\s*(get|post|put|patch|delete|all)\s*\(");
re!(express_mount_re, r#"\b[A-Za-z_$][\w$]*\s*\.\s*use\s*\(\s*(['"`])(/[^'"`\n]*)['"`]\s*,"#);
re!(express_re, r#"(?m)\b([A-Za-z_$][\w$]*)\s*\.\s*(get|post|put|delete|patch|options|head|all|use)\s*\(\s*(['"`])(/[^'"`\n]*)['"`]\s*,"#);
re!(
    py_decorator_re,
    r#"(?m)^[ \t]*@[ \t]*([A-Za-z_]\w*)[ \t]*\.[ \t]*(route|get|post|put|delete|patch|head|options|websocket)[ \t]*\(\s*(?:[rbuf]*)(['"])([^'"\n]*)['"]"#
);
re!(py_methods_re, r#"methods\s*=\s*[\[\(]([^\]\)]*)[\]\)]"#);
re!(py_def_re, r"(?m)^[ \t]*(?:async[ \t]+)?def[ \t]+(\w+)");
re!(django_re, r#"(?m)\b(?:path|re_path|url)\s*\(\s*[rbuf]*(['"])([^'"\n]*)['"]\s*,\s*([A-Za-z_][\w.]*)"#);
re!(gin_re, r#"(?m)\b([A-Za-z_]\w*)\s*\.\s*(GET|POST|PUT|DELETE|PATCH|OPTIONS|HEAD|Any)\s*\(\s*"([^"\n]*)"\s*,"#);
re!(spring_re, r#"(?m)^[ \t]*@(Get|Post|Put|Delete|Patch|Request)Mapping\b(?:\s*\(([^)]*)\))?"#);
re!(spring_path_re, r#"(?:(?:value|path)\s*=\s*)?\{?\s*"([^"]*)""#);
re!(spring_http_re, r"RequestMethod\.(\w+)");
re!(
    java_method_re,
    r"(?m)^[ \t]*(?:@\w+(?:\([^)]*\))?\s*)*(?:public|private|protected)?[ \t]*(?:static[ \t]+)?(?:final[ \t]+)?[\w<>\[\],.? ]+?[ \t]+(\w+)[ \t]*\("
);
re!(kotlin_fun_re, r"(?m)^[ \t]*(?:(?:public|private|protected|internal|override|suspend)[ \t]+)*fun[ \t]+(?:<[^>]*>[ \t]*)?(\w+)[ \t]*\(");
re!(spring_class_re, r"(?m)^[ \t]*(?:public[ \t]+|open[ \t]+|abstract[ \t]+)*(?:class|interface)[ \t]+\w+");
re!(axum_route_re, r#"\.route\(\s*"([^"\n]*)"\s*,\s*"#);
re!(axum_method_re, r"(get|post|put|delete|patch|head|options|any)\s*\(\s*([\w:]+)\s*\)");
pub(super) fn ident_path_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*$").expect("static regex"))
}

/// Argument list of the call whose `(` is at/after `from`: top-level arguments as
/// `(offset, text)`, plus the byte offset after the closing parenthesis. Strings and comments
/// are skipped when matching parentheses.
pub(super) fn call_args(src: &str, open: usize) -> Option<(Vec<(usize, &str)>, usize)> {
    let b = src.as_bytes();
    if b.get(open) != Some(&b'(') {
        return None;
    }
    let (mut depth, mut i) = (0i32, open);
    let mut args: Vec<(usize, &str)> = Vec::new();
    let mut arg_start = open + 1;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    args.push((arg_start, &src[arg_start..i]));
                    return Some((args, i + 1));
                }
            }
            b',' if depth == 1 => {
                args.push((arg_start, &src[arg_start..i]));
                arg_start = i + 1;
            }
            _ => {}
        }
        i += 1;
        if i - open > 4000 {
            return None; // not a plain route registration
        }
    }
    None
}

/// Handler names among call arguments: bare identifiers (`list`, `ctrl.list`) and the same
/// inside an array of middleware; function literals have no name.
fn handlers_in(src: &str, lines: &Lines, args: &[(usize, &str)]) -> Vec<Handler> {
    let mut out = Vec::new();
    for (off, text) in args {
        let t = text.trim();
        let inner: Vec<(usize, &str)> = if t.starts_with('[') && t.ends_with(']') {
            let base = off + text.find('[').unwrap_or(0) + 1;
            t[1..t.len() - 1]
                .split(',')
                .scan(base, |pos, s| {
                    let o = *pos;
                    *pos += s.len() + 1;
                    Some((o, s))
                })
                .collect()
        } else {
            vec![(*off, *text)]
        };
        for (o, s) in inner {
            let name = s.trim();
            if ident_path_re().is_match(name) && !matches!(name, "true" | "false" | "null" | "undefined" | "nil" | "this") {
                let lead = s.len() - s.trim_start().len();
                let (line, col) = lines.at(src, o + lead);
                out.push(Handler { name: name.to_owned(), line, col });
            }
        }
    }
    out
}

fn looks_like_router(recv: &str) -> bool {
    matches!(recv, "app" | "router" | "server" | "api" | "route" | "routes" | "r")
        || recv.ends_with("Router")
        || recv.ends_with("App")
        || recv.ends_with("Api")
        || recv.ends_with("Routes")
}

pub fn scan_express(src: &str) -> Vec<Route> {
    let lines = Lines::new(src);
    let mut out = Vec::new();
    for m in express_re().captures_iter(src) {
        if !looks_like_router(&m[1]) {
            continue;
        }
        let method = m[2].to_ascii_uppercase();
        let path = m[4].to_owned();
        let whole = m.get(0).unwrap();
        let open = src[whole.start()..].find('(').map(|i| whole.start() + i).unwrap_or(whole.start());
        let (line, _) = lines.at(src, whole.start());
        let Some((args, _)) = call_args(src, open) else { continue };
        let handlers = handlers_in(src, &lines, &args[1..]);
        out.push(Route { method, path, line, handlers, ..Default::default() });
    }
    // the chained form: `router.route('/:id').get(getOne).put(auth, update)`, one route per
    // method at the line of its `.method(`
    for m in express_chain_re().captures_iter(src) {
        if !looks_like_router(&m[1]) {
            continue;
        }
        let path = m[2].to_owned();
        let mut at = m.get(0).unwrap().end();
        for _ in 0..8 {
            let Some(link) = express_link_re().captures(&src[at..(at + 64).min(src.len())]) else { break };
            let open = at + link.get(0).unwrap().end() - 1;
            let Some((args, end)) = call_args(src, open) else { break };
            let (line, _) = lines.at(src, open);
            let handlers = handlers_in(src, &lines, &args);
            out.push(Route { method: link[1].to_ascii_uppercase(), path: path.clone(), line, handlers, ..Default::default() });
            at = end;
        }
    }
    out.sort_by_key(|r| r.line);
    out
}

/// `x.use('/prefix', …, target)`: where a router file is mounted, as (prefix, target text,
/// line). The target is the last argument: an identifier or an inline `require('./x')`.
pub fn express_mounts(src: &str) -> Vec<(String, String, u32)> {
    let lines = Lines::new(src);
    let mut out = Vec::new();
    for m in express_mount_re().captures_iter(src) {
        let whole = m.get(0).unwrap();
        let Some(open) = whole.as_str().find('(').map(|i| whole.start() + i) else { continue };
        let Some((args, _)) = call_args(src, open) else { continue };
        let Some(last) = args.last().map(|a| a.1.trim()).filter(|a| !a.is_empty() && args.len() > 1) else { continue };
        let (line, _) = lines.at(src, whole.start());
        out.push((m[2].to_owned(), last.to_owned(), line));
    }
    out
}

/// Blank out comments and triple-quoted strings (docstrings are full of `@app.route` examples),
/// keeping every offset and newline.
pub fn strip_python_noise(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for k in from..to.min(out.len()) {
            if out[k] != b'\n' {
                out[k] = b' ';
            }
        }
    };
    while i < b.len() {
        match b[i] {
            b'#' => {
                let end = src[i..].find('\n').map_or(b.len(), |e| i + e);
                blank(&mut out, i, end);
                i = end;
            }
            b'"' | b'\'' => {
                let q = b[i];
                if b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q) {
                    let close = [q, q, q];
                    let end = src[i + 3..].find(std::str::from_utf8(&close).unwrap_or("\"\"\"")).map_or(b.len(), |e| i + 3 + e + 3);
                    blank(&mut out, i, end);
                    i = end;
                } else {
                    // ordinary string: skip it unchanged
                    i += 1;
                    while i < b.len() && b[i] != q && b[i] != b'\n' {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| src.to_owned())
}

pub fn scan_python_decorators(src: &str) -> Vec<Route> {
    let clean = strip_python_noise(src);
    let src = clean.as_str();
    let lines = Lines::new(src);
    let mut out = Vec::new();
    for m in py_decorator_re().captures_iter(src) {
        let whole = m.get(0).unwrap();
        let kind = &m[2];
        let path = m[4].to_owned();
        // the decorator call may span lines: look for `methods=[...]` up to the closing paren
        let open = src[whole.start()..].find('(').map(|i| whole.start() + i).unwrap_or(whole.start());
        let call_end = call_args(src, open).map(|x| x.1).unwrap_or(whole.end());
        let call_text = &src[open..call_end];
        let methods: Vec<String> = if kind == "route" {
            match py_methods_re().captures(call_text) {
                Some(mm) => mm[1].split(',').map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_uppercase()).filter(|s| !s.is_empty()).collect(),
                None => vec!["GET".to_owned()],
            }
        } else if kind == "websocket" {
            vec!["WS".to_owned()]
        } else {
            vec![kind.to_ascii_uppercase()]
        };
        // the decorated function: the first `def` after the decorator
        let Some(d) = py_def_re().captures(&src[call_end..]) else { continue };
        let dm = d.get(1).unwrap();
        let (hline, hcol) = lines.at(src, call_end + dm.start());
        let (line, _) = lines.at(src, whole.start());
        for method in methods {
            out.push(Route {
                method,
                path: path.clone(),
                line,
                handlers: vec![Handler { name: dm.as_str().to_owned(), line: hline, col: hcol }],
                ..Default::default()
            });
        }
    }
    out
}

pub fn scan_django(src: &str) -> Vec<Route> {
    let clean = strip_python_noise(src);
    let src = clean.as_str();
    let lines = Lines::new(src);
    django_re()
        .captures_iter(src)
        .filter(|m| !matches!(&m[3], "include" | "admin") && !m[3].ends_with(".urls") && !m[3].starts_with("admin."))
        .map(|m| {
            let whole = m.get(0).unwrap();
            let (line, _) = lines.at(src, whole.start());
            let h = m.get(3).unwrap();
            let (hl, hc) = lines.at(src, h.start());
            let mut name = h.as_str().to_owned();
            name = name.trim_end_matches(".as_view").to_owned();
            let path = if m[2].starts_with('/') { m[2].to_owned() } else { format!("/{}", &m[2]) };
            Route { method: "ANY".into(), path, line, handlers: vec![Handler { name, line: hl, col: hc }], ..Default::default() }
        })
        .collect()
}

pub fn scan_gin(src: &str) -> Vec<Route> {
    let lines = Lines::new(src);
    let mut out = Vec::new();
    for m in gin_re().captures_iter(src) {
        let whole = m.get(0).unwrap();
        let open = src[whole.start()..].find('(').map(|i| whole.start() + i).unwrap_or(whole.start());
        let Some((args, _)) = call_args(src, open) else { continue };
        let handlers = handlers_in(src, &lines, &args[1..]);
        let (line, _) = lines.at(src, whole.start());
        out.push(Route { method: m[2].to_ascii_uppercase(), path: m[3].to_owned(), line, handlers, ..Default::default() });
    }
    out
}

pub fn scan_spring(src: &str, kotlin: bool) -> Vec<Route> {
    let lines = Lines::new(src);
    // class-level @RequestMapping("/prefix") before the first class declaration
    let class_at = spring_class_re().find(src).map_or(usize::MAX, |m| m.start());
    let mut prefix = String::new();
    let mut out = Vec::new();
    for m in spring_re().captures_iter(src) {
        let whole = m.get(0).unwrap();
        let args = m.get(2).map_or("", |a| a.as_str());
        let path = spring_path_re().captures(args).map(|p| p[1].to_owned()).unwrap_or_default();
        if &m[1] == "Request" && whole.start() < class_at {
            prefix = path;
            continue;
        }
        let method = match &m[1] {
            "Request" => spring_http_re().captures(args).map(|h| h[1].to_ascii_uppercase()).unwrap_or_else(|| "ANY".into()),
            other => other.to_ascii_uppercase(),
        };
        // the handler method that follows (other annotations in between are skipped)
        let after = whole.end();
        let tail = &src[after..];
        let hm = if kotlin { kotlin_fun_re().captures(tail).and_then(|c| c.get(1)) } else { java_method_re().captures(tail).and_then(|c| c.get(1)) };
        let Some(hm) = hm else { continue };
        let (hl, hc) = lines.at(src, after + hm.start());
        let (line, _) = lines.at(src, whole.start());
        let full = format!("{}{}{}", prefix, if path.starts_with('/') || path.is_empty() || prefix.ends_with('/') { "" } else { "/" }, path);
        out.push(Route {
            method,
            path: if full.is_empty() { "/".into() } else { full },
            line,
            handlers: vec![Handler { name: hm.as_str().to_owned(), line: hl, col: hc }],
            ..Default::default()
        });
    }
    out
}

pub fn scan_axum(src: &str) -> Vec<Route> {
    let lines = Lines::new(src);
    let mut out = Vec::new();
    for m in axum_route_re().captures_iter(src) {
        let whole = m.get(0).unwrap();
        let (line, _) = lines.at(src, whole.start());
        let open = whole.start() + whole.as_str().find('(').unwrap_or(0);
        let Some((_, end)) = call_args(src, open) else { continue };
        let body = &src[whole.end()..end.saturating_sub(1)];
        for mm in axum_method_re().captures_iter(body) {
            let h = mm.get(2).unwrap();
            let (hl, hc) = lines.at(src, whole.end() + h.start());
            out.push(Route {
                method: mm[1].to_ascii_uppercase(),
                path: m[1].to_owned(),
                line,
                handlers: vec![Handler { name: h.as_str().to_owned(), line: hl, col: hc }],
                ..Default::default()
            });
        }
    }
    out
}

/// Convert a Nuxt `pages/**/*.vue` file path to its route path (e.g. `pages/blog/[slug].vue` -> `/blog/:slug`).
pub fn nuxt_page_route(rel: &str) -> Option<String> {
    if !rel.ends_with(".vue") {
        return None;
    }
    let norm = format!("/{rel}");
    let idx = norm.find("/pages/")?;
    let after = &norm[idx + "/pages/".len()..];
    let stem = after.strip_suffix(".vue")?;
    let mut segs: Vec<String> = Vec::new();
    for seg in stem.split('/') {
        if seg.is_empty() || seg == "index" {
            continue;
        }
        let s = if let Some(inner) = seg.strip_prefix("[...").and_then(|x| x.strip_suffix(']')) {
            format!("*{inner}")
        } else if let Some(inner) = seg.strip_prefix("[[").and_then(|x| x.strip_suffix("]]")) {
            format!(":{inner}?")
        } else if let Some(inner) = seg.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
            format!(":{inner}")
        } else {
            seg.to_owned()
        };
        segs.push(s);
    }
    Some(format!("/{}", segs.join("/")))
}

/// Convert a SvelteKit `routes/**/+page.svelte` file path to its route path (e.g. `src/routes/blog/[slug]/+page.svelte` -> `/blog/:slug`).
pub fn sveltekit_page_route(rel: &str) -> Option<String> {
    if rel.rsplit('/').next() != Some("+page.svelte") {
        return None;
    }
    let norm = format!("/{rel}");
    let idx = norm.find("/routes/")?;
    let after = &norm[idx + "/routes/".len()..];
    let dir = after.rsplit_once('/').map_or("", |(d, _)| d);
    let mut segs: Vec<String> = Vec::new();
    for seg in dir.split('/') {
        if seg.is_empty() || (seg.starts_with('(') && seg.ends_with(')')) {
            continue;
        }
        fn strip_matcher(x: &str) -> &str {
            x.split('=').next().unwrap_or(x)
        }
        let s = if let Some(inner) = seg.strip_prefix("[...").and_then(|x| x.strip_suffix(']')) {
            format!("*{}", strip_matcher(inner))
        } else if let Some(inner) = seg.strip_prefix("[[").and_then(|x| x.strip_suffix("]]")) {
            format!(":{}?", strip_matcher(inner))
        } else if let Some(inner) = seg.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
            format!(":{}", strip_matcher(inner))
        } else {
            seg.to_owned()
        };
        segs.push(s);
    }
    Some(format!("/{}", segs.join("/")))
}

fn scan_sfc_page(rel: &str, path: String, router: &'static str, ext: &str) -> Vec<Route> {
    let comp = rel.rsplit('/').next().unwrap_or(rel).strip_suffix(ext).unwrap_or(rel);
    vec![Route {
        method: String::new(),
        path,
        line: 1,
        handlers: vec![Handler { name: comp.to_owned(), line: 1, col: 0 }],
        ref_kind: RefKind::Edge(cg_core::EdgeKind::Calls),
        router: Some(router),
    }]
}

pub fn scan(fw: Framework, lang: Lang, rel: &str, src: &str) -> Vec<Route> {
    // Framework repositories contain route examples in API comments. Those
    // snippets are documentation, not registrations in the program's graph.
    let code;
    let src = if matches!(fw, Framework::Express | Framework::Gin | Framework::Next | Framework::Nest | Framework::ReactRouter | Framework::TanStack) {
        code = cg_extract::post::strip_js_comments(src);
        code.as_str()
    } else {
        src
    };
    match fw {
        Framework::Express => scan_express(src),
        Framework::Flask | Framework::FastApi => scan_python_decorators(src),
        Framework::Django => {
            if rel.ends_with("urls.py") || src.contains("urlpatterns") {
                scan_django(src)
            } else {
                Vec::new()
            }
        }
        Framework::Gin => scan_gin(src),
        Framework::Spring => scan_spring(src, lang == Lang::Kotlin),
        Framework::AspNetCore => super::aspnet::scan_aspnet_core(src),
        Framework::Axum => scan_axum(src),
        Framework::Next => super::jsroutes::scan_next(rel, src),
        Framework::Nuxt => nuxt_page_route(rel).map_or_else(Vec::new, |p| scan_sfc_page(rel, p, "nuxt", ".vue")),
        Framework::SvelteKit => sveltekit_page_route(rel).map_or_else(Vec::new, |p| scan_sfc_page(rel, p, "sveltekit", ".svelte")),
        Framework::Nest => super::jsroutes::scan_nest(src),
        Framework::ReactRouter => super::jsroutes::scan_react_router(rel, src),
        Framework::TanStack => super::jsroutes::scan_tanstack(rel, src),
    }
}

pub struct RouteHook {
    pub fw: Framework,
}

impl FileHook for RouteHook {
    fn name(&self) -> &'static str {
        match self.fw {
            Framework::Express => "express-routes",
            Framework::Flask => "flask-routes",
            Framework::FastApi => "fastapi-routes",
            Framework::Django => "django-routes",
            Framework::Gin => "gin-routes",
            Framework::Spring => "spring-routes",
            Framework::AspNetCore => "aspnet-core-routes",
            Framework::Axum => "axum-routes",
            Framework::Next => "next-routes",
            Framework::Nuxt => "nuxt-routes",
            Framework::SvelteKit => "sveltekit-routes",
            Framework::Nest => "nest-routes",
            Framework::ReactRouter => "react-router-routes",
            Framework::TanStack => "tanstack-router-routes",
        }
    }
    fn applies(&self, lang: Lang, rel: &str) -> bool {
        match self.fw {
            Framework::Next => lang.family() == LangFamily::Js && super::jsroutes::next_route_of(rel).is_some(),
            Framework::Nuxt => lang == Lang::Vue && nuxt_page_route(rel).is_some(),
            Framework::SvelteKit => lang == Lang::Svelte && sveltekit_page_route(rel).is_some(),
            Framework::Express | Framework::Nest | Framework::ReactRouter | Framework::TanStack => lang.family() == LangFamily::Js,
            Framework::Flask | Framework::FastApi | Framework::Django => lang == Lang::Python,
            Framework::Gin => lang == Lang::Go,
            Framework::Spring => matches!(lang, Lang::Java | Lang::Kotlin),
            Framework::AspNetCore => lang == Lang::CSharp,
            Framework::Axum => lang == Lang::Rust,
        }
    }
    fn triggers(&self) -> &'static [&'static str] {
        match self.fw {
            Framework::Express => &[".get(", ".post(", ".put(", ".delete(", ".patch(", ".all(", ".use(", ".options(", ".head("],
            Framework::Flask | Framework::FastApi => &["@"],
            Framework::Django => &["path(", "url("],
            Framework::Gin => &[".GET(", ".POST(", ".PUT(", ".DELETE(", ".PATCH(", ".Any(", ".OPTIONS(", ".HEAD("],
            Framework::Spring => &["Mapping"],
            Framework::AspNetCore => &[
                "HttpGet",
                "HttpPost",
                "HttpPut",
                "HttpDelete",
                "HttpPatch",
                "HttpHead",
                "HttpOptions",
                "AcceptVerbs",
                "Route(",
                "RouteAttribute(",
                "MapGet",
                "MapPost",
                "MapPut",
                "MapDelete",
                "MapPatch",
                "MapMethods",
            ],
            Framework::Axum => &[".route("],
            Framework::Next | Framework::Nuxt | Framework::SvelteKit => &[],
            Framework::Nest => &[
                "@Get",
                "@Post",
                "@Put",
                "@Patch",
                "@Delete",
                "@Head",
                "@Options",
                "@All",
                "@Query",
                "@Mutation",
                "@Subscription",
                "@MessagePattern",
                "@EventPattern",
                "@SubscribeMessage",
            ],
            Framework::ReactRouter => &[
                "<Route",
                "createBrowserRouter",
                "createHashRouter",
                "createMemoryRouter",
                "createRoutesFromElements",
                "useRoutes",
                "const ",
                "let ",
                "var ",
                "export default",
            ],
            Framework::TanStack => &["createFileRoute", "createLazyFileRoute", "createRoute", "createRootRoute"],
        }
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let mut routes: Vec<(Option<(String, bool)>, Route)> = scan(self.fw, ctx.lang, ctx.rel, ctx.src).into_iter().map(|r| (None, r)).collect();
        if self.fw == Framework::ReactRouter {
            let code = cg_extract::post::strip_js_comments(ctx.src);
            let candidates = super::jsroutes::scan_react_tables(ctx.rel, &code);
            // The ordinary scanner sees every object when the file also creates a router.
            // Keep array declarations as candidates so only the consumed table is active.
            routes.retain(|(_, r)| !candidates.iter().any(|(_, c, _)| c.line == r.line && c.path == r.path));
            routes.extend(candidates.into_iter().map(|(name, r, absolute)| (Some((name, absolute)), r)));
            for usage in super::jsroutes::react_table_uses(&code) {
                out.sites.push(cg_extract::SiteFact {
                    pass: crate::synth::sites::PASS_REACT_TABLE,
                    role: 1,
                    key_kind: 0,
                    key_a: usage.name,
                    key_b: usage.owner,
                    node_row: cg_core::NONE,
                    line: 0,
                    payload: format!("{}{}", if usage.absolute { "a" } else { "r" }, usage.prefix),
                });
            }
        }
        if self.fw == Framework::Express && ctx.src.contains(".use(") {
            let code = cg_extract::post::strip_js_comments(ctx.src);
            for (prefix, target, line) in express_mounts(&code) {
                out.sites.push(cg_extract::SiteFact {
                    pass: crate::synth::sites::PASS_HTTP,
                    role: crate::synth::dynamic::ROLE_HTTP_MOUNT,
                    key_kind: 0,
                    key_a: prefix,
                    key_b: String::new(),
                    node_row: cg_core::NONE,
                    line,
                    payload: target,
                });
            }
        }
        let kcount = ctx.rows.node_count() as u32;
        for (table, mut r) in routes {
            if self.fw == Framework::Nest {
                r.router = Some("nestjs");
            }
            // a page is named by its path alone; a server route by its method and path
            let (name, qname) = if r.method.is_empty() {
                (r.path.clone(), format!("{}::route:{}", ctx.rel, r.path))
            } else {
                (format!("{} {}", r.method, r.path), format!("{}::{}:{}", ctx.rel, r.method, r.path))
            };
            if out.nodes.iter().any(|n| n.qname == qname && n.start_line == r.line) {
                continue;
            }
            let row = kcount + out.nodes.len() as u32;
            out.nodes.push(ExtraNode {
                kind: NodeKind::Route,
                qname,
                digest: node_digest(ctx.rel, NodeKind::Route, &name, r.line),
                name,
                start_line: r.line,
                end_line: r.line,
                start_col: 0,
                end_col: 0,
                signature: r.router.map(str::to_owned),
                parent_row: 0,
                exported: None,
            });
            if let Some((table, absolute)) = table {
                out.sites.push(cg_extract::SiteFact {
                    pass: crate::synth::sites::PASS_REACT_TABLE,
                    role: 0,
                    key_kind: 0,
                    key_a: table,
                    key_b: String::new(),
                    node_row: row,
                    line: r.line,
                    payload: if absolute { "a" } else { "r" }.to_owned(),
                });
            }
            for h in r.handlers {
                out.refs.push(ExtraRef { from_row: row, kind: r.ref_kind, name: h.name, line: h.line, col: h.col });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(r: &[Route]) -> Vec<String> {
        r.iter().map(|r| format!("{} {} -> {}", r.method, r.path, r.handlers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join("|"))).collect()
    }

    #[test]
    fn express() {
        let src = "const app = express();\napp.get('/users/:id', auth, ctrl.show);\nrouter.post(\"/users\", [validate, rate], create);\napp.use('/api', apiRouter);\napp.get('/inline', (req, res) => res.send('x'));\nfoo.get('/nope', h);\napp.get(`/tpl`, tplHandler)";
        assert_eq!(
            names(&scan_express(src)),
            vec![
                "GET /users/:id -> auth|ctrl.show",
                "POST /users -> validate|rate|create",
                "USE /api -> apiRouter",
                "GET /inline -> ",
                "GET /tpl -> tplHandler"
            ]
        );
    }

    #[test]
    fn express_chains_and_mounts() {
        let src = "router.route('/').get(getProducts).post(protect, admin, createProduct)\nrouter\n  .route('/:id')\n  .get(getProductById)\n  .delete(protect, deleteProduct)\napp.use('/api/products', productRoutes)\napp.use('/api/users', require('./routes/users'))\napp.use(notFound)\n";
        assert_eq!(
            names(&scan_express(src)),
            vec![
                "GET / -> getProducts",
                "POST / -> protect|admin|createProduct",
                "GET /:id -> getProductById",
                "DELETE /:id -> protect|deleteProduct",
                "USE /api/products -> productRoutes",
                "USE /api/users -> "
            ]
        );
        let mounts: Vec<_> = express_mounts(src).into_iter().map(|m| format!("{} {}", m.0, m.1)).collect();
        assert_eq!(mounts, vec!["/api/products productRoutes", "/api/users require('./routes/users')"]);
    }

    #[test]
    fn flask_and_fastapi() {
        let src = "@app.route('/a', methods=['GET', 'POST'])\n@login_required\ndef a():\n    pass\n\n@router.get(\"/items/{id}\")\nasync def read_item(id: int):\n    pass\n\n@app.route('/plain')\ndef plain(): pass\n";
        assert_eq!(names(&scan_python_decorators(src)), vec!["GET /a -> a", "POST /a -> a", "GET /items/{id} -> read_item", "GET /plain -> plain"]);
    }

    #[test]
    fn docstring_examples_are_not_routes() {
        let src = "def f():\n    \"\"\"\n    @app.route('/example')\n    def hello(): pass\n    \"\"\"\n# @app.route('/commented')\n@app.route('/real')\ndef real(): pass\n";
        assert_eq!(names(&scan_python_decorators(src)), vec!["GET /real -> real"]);
    }

    #[test]
    fn django() {
        let src = "urlpatterns = [\n    path('users/', views.user_list, name='users'),\n    path('x/', XView.as_view()),\n    path('admin/', admin.site.urls),\n    path('api/', include('api.urls')),\n]";
        assert_eq!(names(&scan_django(src)), vec!["ANY /users/ -> views.user_list", "ANY /x/ -> XView"]);
    }

    #[test]
    fn gin() {
        let src = "r.GET(\"/ping\", ping)\nv1.POST(\"/users\", auth(), handlers.Create)\nr.Any(\"/any\", func(c *gin.Context) {})";
        assert_eq!(names(&scan_gin(src)), vec!["GET /ping -> ping", "POST /users -> handlers.Create", "ANY /any -> "]);
    }

    #[test]
    fn spring() {
        let src = "@RestController\n@RequestMapping(\"/api\")\npublic class UserController {\n    @GetMapping(\"/users/{id}\")\n    public User get(@PathVariable Long id) { return null; }\n\n    @RequestMapping(value = \"/save\", method = RequestMethod.POST)\n    public void save() {}\n}\n";
        assert_eq!(names(&scan_spring(src, false)), vec!["GET /api/users/{id} -> get", "POST /api/save -> save"]);
        let kt = "@GetMapping(\"/k\")\nfun listAll(): List<X> = emptyList()\n";
        assert_eq!(names(&scan_spring(kt, true)), vec!["GET /k -> listAll"]);
    }

    #[test]
    fn axum() {
        let src = "let app = Router::new()\n    .route(\"/\", get(root))\n    .route(\"/users\", get(list).post(handlers::create));";
        assert_eq!(names(&scan_axum(src)), vec!["GET / -> root", "GET /users -> list", "POST /users -> handlers::create"]);
    }
}
