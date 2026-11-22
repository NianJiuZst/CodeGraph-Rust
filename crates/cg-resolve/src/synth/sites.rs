//! Synthesis sites (design doc 7.2): observations recorded while the source is in memory, keyed
//! for the later, source-free "link" step. One scanner covers the JS/TS passes: event emitters,
//! React `setState` and JSX child components.

use cg_core::{Lang, LangFamily, NodeKind};
use cg_extract::{FileHook, HookCtx, HookOut, SiteFact};
use regex::Regex;
use std::sync::OnceLock;

pub const PASS_EMITTER: u16 = 1;
pub const PASS_REACT_RENDER: u16 = 2;
pub const PASS_JSX: u16 = 3;
/// Go type names that stand for other types (`go_implements`).
pub const PASS_GO_TYPES: u16 = 4;
/// Navigation calls and link markup (`frameworks::nav`).
pub const PASS_NAV: u16 = 5;
/// A Next.js proxy's entry and matcher (`frameworks::nav`).
pub const PASS_PROXY: u16 = 6;
/// Field-backed callback channels (`dynamic::callbacks`).
pub const PASS_CALLBACK: u16 = 7;
/// Object-literal registries and their dispatch (`dynamic::registries`).
pub const PASS_REGISTRY: u16 = 8;
/// HTTP client calls and client instances (`dynamic::cross_tier`).
pub const PASS_HTTP: u16 = 9;
/// Queue bindings, producers and consumers (`dynamic::cross_tier`).
pub const PASS_QUEUE: u16 = 10;
/// Event-bus / socket dispatches and handlers (`dynamic::cross_tier`).
pub const PASS_BUS: u16 = 11;
/// Celery tasks and their dispatch (`dynamic::celery`).
pub const PASS_CELERY: u16 = 12;
/// Spring application events (`dynamic::spring_events`).
pub const PASS_SPRING: u16 = 13;
/// React Router arrays and the router calls that consume them.
pub const PASS_REACT_TABLE: u16 = 14;
/// Nest modules, router registrations and application bootstrap configuration.
pub const PASS_NEST: u16 = 15;
/// Go types a struct or interface embeds, with the import path they are written through.
pub const PASS_GO_EMBEDS: u16 = 16;
/// Go func-typed struct fields, func types and the values put in struct fields.
pub const PASS_GO_FIELDS: u16 = 17;
/// Gin's handler-chain dispatcher and the handlers registration calls pass (`gin`).
pub const PASS_GIN: u16 = 18;
/// What a Redux thunk constant's body dispatches (`redux`).
pub const PASS_THUNK: u16 = 19;
/// Requests Spring tests make (`jvm`).
pub const PASS_TEST_REQUEST: u16 = 20;
/// Tauri commands and React Native native modules across the language bridge (`bridges`).
pub const PASS_BRIDGE: u16 = 21;
/// C# MediatR: a `Handle` method by the type it handles, a `.Send` / `.Publish` by the type it sends.
pub const PASS_MEDIATR: u16 = 22;

pub const ROLE_EMIT: u8 = 0;
pub const ROLE_ON: u8 = 1;
pub const ROLE_GO_ALIAS: u8 = 0;
pub const ROLE_GO_GENERIC: u8 = 1;
/// `PASS_GO_FIELDS` roles.
pub const ROLE_GO_FUNC_FIELD: u8 = 0;
pub const ROLE_GO_FIELD_LITERAL: u8 = 1;
pub const ROLE_GO_FIELD_ASSIGN: u8 = 2;
pub const ROLE_GO_FUNC_TYPE: u8 = 3;

/// Go true aliases and generic type parameters, embedded types and the facts the
/// func-field pass reads, from the facts of the file.
pub struct GoTypeSites;

impl FileHook for GoTypeSites {
    fn name(&self) -> &'static str {
        "go-type-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang == Lang::Go
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let site = |pass: u16, role: u8, key_a: &str, key_b: &str, node_row: u32, line: u32, payload: String| SiteFact {
            pass,
            role,
            key_kind: 0,
            key_a: key_a.to_owned(),
            key_b: key_b.to_owned(),
            node_row,
            line,
            payload,
        };
        for t in &ctx.facts.go_types {
            if t.alias || !t.type_params.is_empty() {
                let role = if t.alias { ROLE_GO_ALIAS } else { ROLE_GO_GENERIC };
                out.sites.push(site(PASS_GO_TYPES, role, &t.name, "", cg_core::NONE, 0, t.type_params.join(",")));
            }
            if t.func_type {
                out.sites.push(site(PASS_GO_FIELDS, ROLE_GO_FUNC_TYPE, &t.name, "", cg_core::NONE, 0, String::new()));
            }
        }
        // the import path an embedded type is written through (`io.Reader` -> `io`)
        for e in &ctx.facts.go_embeds {
            let path = if e.qualifier.is_empty() {
                String::new()
            } else {
                match ctx.facts.imports.iter().find(|i| i.local == e.qualifier) {
                    Some(i) => i.spec.clone(),
                    None => continue,
                }
            };
            out.sites.push(site(PASS_GO_EMBEDS, 0, &e.qualifier, &e.name, e.row, 0, path));
        }
        for f in &ctx.facts.go_fields {
            match f {
                cg_extract::facts::GoFieldFact::FuncField { row, field } => {
                    out.sites.push(site(PASS_GO_FIELDS, ROLE_GO_FUNC_FIELD, field, "", *row, 0, String::new()));
                }
                cg_extract::facts::GoFieldFact::Literal { ty, field, value, line, col } => {
                    let row = super::scan::enclosing_row(ctx, *line, super::scan::FN_KINDS).unwrap_or(cg_core::NONE);
                    out.sites.push(site(PASS_GO_FIELDS, ROLE_GO_FIELD_LITERAL, ty, field, row, *line, format!("{col}|{value}")));
                }
                cg_extract::facts::GoFieldFact::Assign { receiver, field, value, line, col } => {
                    let row = super::scan::enclosing_row(ctx, *line, super::scan::FN_KINDS).unwrap_or(cg_core::NONE);
                    out.sites.push(site(PASS_GO_FIELDS, ROLE_GO_FIELD_ASSIGN, receiver, field, row, *line, format!("{col}|{value}")));
                }
            }
        }
    }
}

/// Event names too generic to link on (design doc 7.3).
const GENERIC_EVENTS: &[&str] = &[
    "error",
    "data",
    "end",
    "close",
    "open",
    "message",
    "connect",
    "disconnect",
    "ready",
    "finish",
    "drain",
    "timeout",
    "abort",
    "exit",
    "load",
    "resize",
    "click",
    "change",
];

fn event_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"\.\s*(emit|on|once|addListener|prependListener)\s*\(\s*(['"`])([^'"`\n]{1,80})['"`](?:\s*,\s*((?:this\.)?[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*)\s*[,)])?"#).expect("static regex")
    })
}

fn component_class_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bclass\s+[A-Za-z_$][\w$]*\s+extends\s+(?:React\s*\.\s*)?(?:Pure)?Component\b").expect("static regex"))
}

fn set_state_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bthis\s*\.\s*setState\s*\(").expect("static regex"))
}

fn jsx_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"<([A-Z][A-Za-z0-9_$]*)[\s/>]").expect("static regex"))
}

pub struct SiteScanner;

/// Row of the innermost node of one of `kinds` that contains `line`.
fn enclosing(ctx: &HookCtx<'_>, line: u32, kinds: &[NodeKind]) -> Option<u32> {
    let mut best: Option<(u32, u32)> = None; // (span, row)
    for (row, n) in ctx.rows.nodes().enumerate() {
        if !kinds.contains(&n.kind()) {
            continue;
        }
        let (s, e) = (n.start_line(), n.end_line());
        if s <= line && line <= e {
            let span = e - s;
            if best.is_none_or(|(b, _)| span < b) {
                best = Some((span, row as u32));
            }
        }
    }
    best.map(|b| b.1)
}

fn line_of(src: &str, off: usize) -> u32 {
    src[..off].bytes().filter(|b| *b == b'\n').count() as u32 + 1
}

impl FileHook for SiteScanner {
    fn name(&self) -> &'static str {
        "synth-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang.family() == LangFamily::Js
    }
    fn triggers(&self) -> &'static [&'static str] {
        &[".emit(", ".on(", ".once(", ".addListener(", ".prependListener(", "setState(", "</", "/>"]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let src = ctx.src;
        let code_kinds = [NodeKind::Method, NodeKind::Function, NodeKind::Component, NodeKind::Class];
        if src.contains(".emit(") || src.contains(".on(") || src.contains(".once(") || src.contains(".addListener(") || src.contains(".prependListener(") {
            for m in event_re().captures_iter(src) {
                let event = &m[3];
                if GENERIC_EVENTS.contains(&event) {
                    continue;
                }
                let whole = m.get(0).unwrap();
                let line = line_of(src, whole.start());
                let is_emit = &m[1] == "emit";
                let row = enclosing(ctx, line, &code_kinds);
                if is_emit {
                    // an emit must happen inside a method or function
                    let Some(row) = row else { continue };
                    out.sites.push(SiteFact {
                        pass: PASS_EMITTER,
                        role: ROLE_EMIT,
                        key_kind: 0,
                        key_a: event.to_owned(),
                        key_b: String::new(),
                        node_row: row,
                        line,
                        payload: String::new(),
                    });
                } else if let Some(h) = m.get(4) {
                    // a registration may sit at the top level of the file
                    out.sites.push(SiteFact {
                        pass: PASS_EMITTER,
                        role: ROLE_ON,
                        key_kind: 0,
                        key_a: event.to_owned(),
                        key_b: String::new(),
                        node_row: row.unwrap_or(cg_core::NONE),
                        line,
                        payload: h.as_str().to_owned(),
                    });
                }
            }
        }
        if src.contains("setState(") {
            // role 1: the class extends React's Component (the kernel keeps no `extends` reference
            // for a qualified base such as `React.Component`)
            for m in component_class_re().find_iter(src) {
                let line = line_of(src, m.start());
                if let Some(row) = enclosing(ctx, line, &[NodeKind::Class]) {
                    out.sites.push(SiteFact {
                        pass: PASS_REACT_RENDER,
                        role: 1,
                        key_kind: 2,
                        key_a: String::new(),
                        key_b: String::new(),
                        node_row: row,
                        line,
                        payload: String::new(),
                    });
                }
            }
            for m in set_state_re().find_iter(src) {
                let line = line_of(src, m.start());
                if let Some(row) = enclosing(ctx, line, &[NodeKind::Method]) {
                    out.sites.push(SiteFact {
                        pass: PASS_REACT_RENDER,
                        role: 0,
                        key_kind: 2,
                        key_a: String::new(),
                        key_b: String::new(),
                        node_row: row,
                        line,
                        payload: String::new(),
                    });
                }
            }
        }
        if !matches!(ctx.lang, Lang::Vue | Lang::Svelte) && (matches!(ctx.lang, Lang::Tsx | Lang::Jsx) || src.contains("</") || src.contains("/>")) {
            let mut seen: std::collections::HashSet<(u32, String)> = std::collections::HashSet::new();
            let jsx_kinds = [NodeKind::Function, NodeKind::Method, NodeKind::Component, NodeKind::Class, NodeKind::Variable, NodeKind::Constant];
            for m in jsx_re().captures_iter(src) {
                let name = m[1].to_owned();
                if name == "Fragment" {
                    continue;
                }
                let line = line_of(src, m.get(0).unwrap().start());
                let Some(row) = enclosing(ctx, line, &jsx_kinds) else { continue };
                if seen.insert((row, name.clone())) {
                    out.sites.push(SiteFact {
                        pass: PASS_JSX,
                        role: 0,
                        key_kind: 0,
                        key_a: name,
                        key_b: String::new(),
                        node_row: row,
                        line,
                        payload: String::new(),
                    });
                }
            }
        }
    }
}
