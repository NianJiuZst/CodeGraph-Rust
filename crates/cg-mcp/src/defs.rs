//! Tool definitions: names, descriptions, input schemas and annotations copied from the upstream
//! `src/mcp/tools.ts` (design doc 10.1). Only the product's own names differ.

use cg_core::brand;
use cg_query::strings::brandify;
use serde_json::{json, Value};

pub const ALL_TOOLS: &[&str] = &["search", "callers", "callees", "impact", "node", "explore", "status", "files"];
/// The default surface is `explore` alone, like upstream.
pub const DEFAULT_TOOLS: &[&str] = &["explore"];

fn project_path_prop() -> Value {
    json!({
        "type": "string",
        "description": brandify("Absolute path to the project to query (or any directory inside it) — codegraph uses the nearest .codegraph/ index at or above that path. Omit to use this session's default project. Pass it to query a second codebase, or when the server root has no index of its own (e.g. a monorepo where only sub-projects are indexed, so there is no default project)."),
    })
}

fn read_only() -> Value {
    json!({"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false})
}

/// Allowlist from `CGR_MCP_TOOLS` (falling back to `CODEGRAPH_MCP_TOOLS`), short names.
pub fn allowlist_from_env() -> Option<Vec<String>> {
    let raw = brand::env::get(brand::env::MCP_TOOLS, Some(brand::env::MCP_TOOLS_UPSTREAM))?;
    let list: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().trim_start_matches(brand::MCP_TOOL_PREFIX).trim_start_matches("codegraph_").to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

pub fn is_enabled(short: &str, allow: &Option<Vec<String>>) -> bool {
    match allow {
        Some(list) => list.iter().any(|s| s == short),
        None => DEFAULT_TOOLS.contains(&short),
    }
}

pub fn definitions(allow: &Option<Vec<String>>) -> Vec<Value> {
    ALL_TOOLS.iter().filter(|t| is_enabled(t, allow)).map(|t| definition(t)).collect()
}

pub fn definition(short: &str) -> Value {
    let name = format!("{}{short}", brand::MCP_TOOL_PREFIX);
    let pp = project_path_prop();
    match short {
        "search" => json!({
            "name": name,
            "description": brandify("Quick symbol search by name. Returns locations only (no code). Use codegraph_explore instead to get the actual source / understand an area in one call."),
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "Symbol name or partial name (e.g., \"auth\", \"signIn\", \"UserService\")"},
                "kind": {"type": "string", "description": "Filter by node kind", "enum": ["function", "method", "class", "interface", "type", "variable", "route", "component"]},
                "limit": {"type": "number", "description": "Maximum results (default: 10)", "default": 10},
                "projectPath": pp,
            }, "required": ["query"]},
            "annotations": read_only(),
        }),
        "callers" => json!({
            "name": name,
            "description": brandify("List functions that call <symbol>. For the full flow, use codegraph_explore."),
            "inputSchema": {"type": "object", "properties": {
                "symbol": {"type": "string", "description": "Name of the function, method, or class to find callers for"},
                "file": {"type": "string", "description": "Narrow to the definition in this file (path or suffix) when several same-named symbols exist (e.g. one UserService per app in a monorepo)"},
                "limit": {"type": "number", "description": "Maximum number of callers to return (default: 20)", "default": 20},
                "projectPath": pp,
            }, "required": ["symbol"]},
            "annotations": read_only(),
        }),
        "callees" => json!({
            "name": name,
            "description": brandify("List functions that <symbol> calls. For the full flow, use codegraph_explore."),
            "inputSchema": {"type": "object", "properties": {
                "symbol": {"type": "string", "description": "Name of the function, method, or class to find callees for"},
                "file": {"type": "string", "description": "Narrow to the definition in this file (path or suffix) when several same-named symbols exist"},
                "limit": {"type": "number", "description": "Maximum number of callees to return (default: 20)", "default": 20},
                "projectPath": pp,
            }, "required": ["symbol"]},
            "annotations": read_only(),
        }),
        "impact" => json!({
            "name": name,
            "description": "List symbols affected by changing <symbol>. Use before a refactor.",
            "inputSchema": {"type": "object", "properties": {
                "symbol": {"type": "string", "description": "Name of the symbol to analyze impact for"},
                "file": {"type": "string", "description": "Narrow to the definition in this file (path or suffix) when several same-named symbols exist"},
                "depth": {"type": "number", "description": "How many levels of dependencies to traverse (default: 2)", "default": 2},
                "projectPath": pp,
            }, "required": ["symbol"]},
            "annotations": read_only(),
        }),
        "node" => json!({
            "name": name,
            "description": brandify("Two modes. (1) READ A FILE — use INSTEAD of the Read tool: pass `file` (a path or basename) with no `symbol` and it returns that file's current on-disk source with line numbers, exactly the shape Read gives you (`<n>\\t<line>`, safe to Edit from), narrowable with `offset`/`limit` just like Read — PLUS a one-line note of which files depend on it. Same bytes as Read, faster (served from the index), with the blast radius attached. Use it whenever you would Read a source file. (2) ONE SYMBOL you can name — its location, signature, verbatim source (includeCode=true) and caller/callee trail in one call, so before changing it you see what calls it and what your edit would break. For an AMBIGUOUS name it returns EVERY matching definition's body in one call (so you never Read a file to find the right overload); pass `file`/`line` to pin one. Use codegraph_explore for several related symbols or the full flow."),
            "inputSchema": {"type": "object", "properties": {
                "symbol": {"type": "string", "description": "Name of the symbol to read (symbol mode). Omit it and pass `file` alone to read a whole file like Read."},
                "includeCode": {"type": "boolean", "description": "Symbol mode: include the symbol's full body (default: false). Ignored in file mode, which always returns source unless `symbolsOnly` is set.", "default": false},
                "file": {"type": "string", "description": "A file path or basename (e.g. \"harness.rs\", \"src/auth/session.ts\"). Pass it ALONE (no symbol) to READ the file like the Read tool — its full source with line numbers + which files depend on it. Or pass it WITH a symbol to disambiguate an overloaded name to the definition in this file."},
                "offset": {"type": "number", "description": "File mode: 1-based line to start reading from, exactly like Read's offset. Defaults to the start of the file."},
                "limit": {"type": "number", "description": "File mode: maximum number of lines to return, exactly like Read's limit. Defaults to the whole file (capped at 2000 lines, like Read)."},
                "symbolsOnly": {"type": "boolean", "description": "File mode: return just the file's symbol map + dependents (a cheap structural overview) instead of its source.", "default": false},
                "maxChars": {"type": "number", "description": "File mode: optional character budget cap (1000–100000, default: 38000)."},
                "line": {"type": "number", "description": "Symbol mode only: disambiguate to the definition at/around this line (use with the file:line a trail showed you)."},
                "projectPath": pp,
            }, "required": []},
            "annotations": read_only(),
        }),
        "explore" => json!({
            "name": name,
            "description": brandify("PRIMARY TOOL — call FIRST for almost any question OR before an edit: how does X work, architecture, a bug, where/what is X, surveying an area, or the symbols you are about to change. Returns the verbatim source of the relevant symbols grouped by file in ONE capped call (Read-equivalent — treat the shown source as already Read; do NOT re-open those files), plus the call path among them. Query can be a natural-language question OR a bag of symbol/file names. Usually the ONLY call you need — more accurate context, in far fewer tokens and round-trips than a search/Read/Grep loop."),
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "Symbol names, file names, or short code terms to explore (e.g., \"AuthService loginUser session-manager\", \"GraphTraverser BFS impact traversal.ts\"). For a flow question, name the symbols spanning the flow (e.g. \"mutateElement renderScene\"). A natural-language question works too."},
                "maxFiles": {"type": "number", "description": "Maximum number of files to return source from, 1–20. Omit it to use a default sized to the project (4 files below 150 indexed files, 5 below 500, 8 otherwise). Total output is also capped by characters (about 13K–24K by project size), so a higher maxFiles spreads that budget over more files rather than returning more text."},
                "maxChars": {"type": "number", "description": "Optional hard character budget cap (1000–50000) to tighten or widen response size."},
                "compact": {"type": "boolean", "description": "Return call flow, blast radius, and symbol outlines without verbatim source bodies (low-context triage mode).", "default": false},
                "projectPath": pp,
            }, "required": ["query"]},
            "annotations": read_only(),
            "_meta": {"anthropic/alwaysLoad": true},
        }),
        "status" => json!({
            "name": name,
            "description": "Index health check: files, nodes, edges, last indexed time, and added/modified/removed counts. Skip unless debugging.",
            "inputSchema": {"type": "object", "properties": {"projectPath": pp}},
            "annotations": read_only(),
        }),
        "files" => json!({
            "name": name,
            "description": "Indexed file tree with language + symbol counts. Faster than Glob for project layout.",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Filter to files under this directory path (e.g., \"src/components\"). Returns all files if not specified."},
                "pattern": {"type": "string", "description": "Filter files matching this glob pattern (e.g., \"*.tsx\", \"**/*.test.ts\")"},
                "format": {"type": "string", "description": "Output format: \"tree\" (hierarchical, default), \"flat\" (simple list), \"grouped\" (by language), \"package\" (by nearest manifest: package.json, composer.json, Cargo.toml, go.mod, pyproject.toml, pom.xml, build.gradle)", "enum": ["tree", "flat", "grouped", "package"], "default": "tree"},
                "includeMetadata": {"type": "boolean", "description": "Include file metadata like language and symbol count (default: true)", "default": true},
                "testsOnly": {"type": "boolean", "description": "Only return test files (default: false)", "default": false},
                "excludeTests": {"type": "boolean", "description": "Exclude test files (default: false)", "default": false},
                "maxDepth": {"type": "number", "description": "Maximum directory depth to show (default: unlimited)"},
                "projectPath": pp,
            }},
            "annotations": read_only(),
        }),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_surface_is_explore_only_with_always_load() {
        let defs = definitions(&None);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["name"], "codegraph-rust_explore");
        assert_eq!(defs[0]["_meta"]["anthropic/alwaysLoad"], true);
        assert_eq!(defs[0]["annotations"]["readOnlyHint"], true);
        assert!(defs[0]["inputSchema"]["properties"]["projectPath"]["description"].as_str().unwrap().contains(".cgr/"));
    }

    #[test]
    fn allowlist_enables_more_tools() {
        let allow = Some(vec!["explore".to_string(), "node".to_string(), "callers".to_string()]);
        let names: Vec<String> = definitions(&allow).iter().map(|d| d["name"].as_str().unwrap().to_owned()).collect();
        assert_eq!(names, ["codegraph-rust_callers", "codegraph-rust_node", "codegraph-rust_explore"]);
    }
}
