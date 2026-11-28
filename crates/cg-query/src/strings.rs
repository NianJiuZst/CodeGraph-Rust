//! User-visible strings copied from the upstream project's `src/mcp/tools.ts` and
//! `server-instructions.ts` (design doc 8.3 step 8, 10.1). The agent-facing behaviour those
//! strings produced was validated upstream, so wording changes only through an A/B run. The
//! only edits are the product's own names (tool prefix, data directory, binary).

use cg_core::brand;

/// Output surface driving user-facing command and flag hints in query responses.
/// Defaults to [`OutputMode::Mcp`] so direct `Graph::explore` / `ToolEnv` callers keep
/// MCP wording unless explicitly running through the CLI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    Mcp,
    Cli,
}

impl OutputMode {
    pub fn flag(self, mcp: &'static str, cli: &'static str) -> &'static str {
        match self {
            OutputMode::Mcp => mcp,
            OutputMode::Cli => cli,
        }
    }
}

pub fn explore_tool() -> String {
    format!("{}explore", brand::MCP_TOOL_PREFIX)
}

pub fn tool(short: &str) -> String {
    format!("{}{short}", brand::MCP_TOOL_PREFIX)
}

pub fn explore_tool_for(mode: OutputMode) -> String {
    tool_for(mode, "explore")
}

pub fn tool_for(mode: OutputMode, short: &str) -> String {
    match mode {
        OutputMode::Mcp => tool(short),
        OutputMode::Cli => format!("{} {short}", brand::BIN_NAME),
    }
}

fn explore_inline(mode: OutputMode) -> String {
    match mode {
        OutputMode::Mcp => explore_tool(),
        OutputMode::Cli => format!("`{} explore`", brand::BIN_NAME),
    }
}

pub fn dedup_reuse_hint(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Mcp => "reuse or pass `noDedup: true`.",
        OutputMode::Cli => "reuse or pass `--no-dedup`.",
    }
}

pub fn dedup_copy_hint(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Mcp => "use that copy or pass `noDedup: true`.",
        OutputMode::Cli => "use that copy or pass `--no-dedup`.",
    }
}

pub fn compact_explore_note(mode: OutputMode) -> String {
    match mode {
        OutputMode::Mcp => {
            format!("> Compact mode omits source bodies. Drop `compact` or call `{}` on a symbol for verbatim code.\n\n", tool("node"))
        }
        OutputMode::Cli => {
            format!("> Compact mode omits source bodies. Drop `--compact` or run `{} node <symbol> --code` for verbatim code.\n\n", brand::BIN_NAME)
        }
    }
}

pub fn compact_one_symbol_note(mode: OutputMode) -> String {
    match mode {
        OutputMode::Mcp => "> Compact mode omits source bodies. Drop `compact` or call `node` for verbatim code.".to_owned(),
        OutputMode::Cli => {
            format!("> Compact mode omits source bodies. Drop `--compact` or run `{} node <symbol> --code` for verbatim code.", brand::BIN_NAME)
        }
    }
}

pub fn one_symbol_body_cut_note(mode: OutputMode, start: u32, shown_to: u32, end: u32, path: &str) -> String {
    let next = shown_to + 1;
    match mode {
        OutputMode::Mcp => {
            format!("(lines {start}–{shown_to} of {start}–{end} shown — pass `node` with `file: \"{path}\"` and `offset: {next}` for the rest)")
        }
        OutputMode::Cli => {
            format!("(lines {start}–{shown_to} of {start}–{end} shown — run `{} node {path} --offset {next}` for the rest)", brand::BIN_NAME)
        }
    }
}

pub fn container_outline_note(mode: OutputMode, path: &str) -> String {
    match mode {
        OutputMode::Mcp => format!("> Structural outline only. Read `{path}` or call {} on a specific member for its body.", tool("node")),
        OutputMode::Cli => {
            format!("> Structural outline only. Read `{path}` or run `{} node` with `--code` on a specific member for its body.", brand::BIN_NAME)
        }
    }
}

pub fn node_trail_header(mode: OutputMode) -> String {
    match mode {
        OutputMode::Mcp => format!("**Trail — {} any of these to follow it (no Read needed)**", tool("node")),
        OutputMode::Cli => format!("**Trail — run `{} node` on any of these to follow it (no Read needed)**", brand::BIN_NAME),
    }
}

pub fn symbols_only_footer(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Mcp => "> Drop `symbolsOnly` (or pass `offset`/`limit`) to read the source, like Read.",
        OutputMode::Cli => "> Drop `--symbols-only` (or pass `--offset`/`--limit`) to read the source, like Read.",
    }
}

pub fn file_view_range_footer(mode: OutputMode, offset: usize, shown_end: usize, total: usize, line_clipped: bool) -> String {
    let clipped = if line_clipped { ", partial line" } else { "" };
    match mode {
        OutputMode::Mcp => format!(
            "(lines {offset}–{shown_end} of {total}{clipped} — pass `offset`/`limit` for another range, or `{} <symbol>` for one symbol in full)",
            tool("node")
        ),
        OutputMode::Cli => format!(
            "(lines {offset}–{shown_end} of {total}{clipped} — pass `--offset`/`--limit` for another range, or `{} node <symbol> --code` for one symbol in full)",
            brand::BIN_NAME
        ),
    }
}

pub fn impact_truncated_note(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Mcp => "> Result truncated to protect the MCP process on a high-fanout graph. Narrow with `file` or reduce `depth`.",
        OutputMode::Cli => "> Result truncated on a high-fanout graph. Narrow with `--file` or reduce `--depth`.",
    }
}

pub fn brandify(s: &str) -> String {
    s.replace("codegraph_explore", &explore_tool())
        .replace("codegraph_node", &tool("node"))
        .replace(".codegraph/", &format!("{}/", brand::DATA_DIR))
        .replace("`codegraph init`", &format!("`{} init`", brand::BIN_NAME))
        .replace("`codegraph status`", &format!("`{} status`", brand::BIN_NAME))
        .replace("CODEGRAPH_EXPLORE_DEDUP", "CGR_EXPLORE_DEDUP")
        .replace("CodeGraph auto-sync", "codegraph-rust auto-sync")
}

/// `# Codegraph — ...` playbook sent in the MCP `initialize` response.
pub fn server_instructions() -> String {
    brandify(SERVER_INSTRUCTIONS)
}

/// Variant sent when the server's own root has no index (tools still listed, design doc 10.1).
pub fn server_instructions_no_root_index() -> String {
    brandify(SERVER_INSTRUCTIONS_NO_ROOT_INDEX)
}

const SERVER_INSTRUCTIONS: &str = r#"# Codegraph — code intelligence over an indexed knowledge graph

Codegraph is a memory-mapped knowledge graph of every symbol, edge, and file in
the workspace — pre-computed structure you would otherwise re-derive by
reading files (cached intelligence: thousands of parse/trace decisions you
don't pay to re-reason each run). This build indexes TypeScript/JavaScript
(including TSX/JSX), Vue, Svelte, Python, Go, Rust, Java, Kotlin, C/C++, C#, PHP, Ruby,
and Swift. Reads are
sub-millisecond; the index lags writes by ~1s through the file watcher. Reach for it BEFORE *and* while
writing or editing code — not just for questions: one call returns the
verbatim source PLUS who calls it and what it affects, so you edit with the
blast radius in view. More accurate context, in far fewer tokens and
round-trips than reading files yourself.

## One tool: codegraph_explore — use it instead of reading files

There is a single tool, `codegraph_explore`, and it is Read-equivalent. It
takes either a natural-language question or a bag of symbol/file names and
returns the **verbatim, line-numbered source** of the relevant symbols
grouped by file — the same `<n>\t<line>` shape `Read` gives you, safe to
`Edit` from — PLUS the call path among them (including dynamic-dispatch hops
like callbacks, React re-render, and JSX children that grep can't follow) and
a blast-radius summary of what depends on them.

Whether you're answering "how does X work" or implementing a change (fixing a
bug, adding a feature), call `codegraph_explore` before you Read. ONE call
usually answers the whole question. Codegraph IS the pre-built search index —
so running your own grep + read loop, or delegating the lookup to a separate
file-reading sub-task/agent, repeats work codegraph already did and costs more
for the same answer. A direct codegraph answer is typically one to a few
calls; a grep/read exploration is dozens.

## How to query

- **Almost any question — "how does X work", architecture, a bug, "what/where is X", or surveying an area** → `codegraph_explore` with a natural-language question or the relevant names. ONE capped call returns the verbatim source grouped by file; most often the ONLY call you need.
- **"How does X reach/become Y? / the flow / the path from X to Y"** → `codegraph_explore`, naming the symbols that span the flow (e.g. `mutateElement renderScene`) — it surfaces the call path among them, riding dynamic-dispatch hops, and returns their source.
- **Reading or editing a file/symbol you can name** → put its name or file path in the `codegraph_explore` query — it returns that current line-numbered source (safe to `Edit` from) with the call path and blast radius attached, so you don't Read it separately. For an overloaded name it returns every matching definition's body in one call.
- **Need more?** Call `codegraph_explore` again with more specific names — treat the source it returns as already Read. Suggested call counts are advisory only, NOT a quota; extra calls are never rejected or rate-limited.
- Qualified symbol names accept dots, `::`, or slashes, including containers whose names contain dots (for example, `AppWeb.Format.group`).
- Named-symbol call paths require exact matches; partial or mistyped names are never silently substituted as flow endpoints. If a graph query reports a missing symbol with did-you-mean suggestions, query the suggested name explicitly.
- Explore matches names and indexed code words lexically, not by meaning; an empty result reports word matches and may suggest indexed candidate names to retry with `codegraph_explore`.

## Anti-patterns

- **Trust codegraph's results — don't re-verify them with grep.** They come from a full AST parse; re-checking with grep is slower, less accurate, and wastes context.
- **Don't grep or Read first** to find or understand indexed code — ONE `codegraph_explore` returns the relevant symbols' source together in a single round-trip. Reach for raw `Read`/`Grep` only to confirm a specific detail codegraph didn't cover, or for what codegraph doesn't index (configs, docs).
- **Don't reconstruct a flow by hand** — name the endpoints in one `codegraph_explore` and it surfaces the path between them, dynamic-dispatch hops included.
- **After editing, check the staleness banner.** When a tool response starts with "⚠️ Some files referenced below were edited since the last index sync…", the listed files are pending re-index — Read those specific files for accurate content. Every file NOT in that banner is fresh, so still trust codegraph. A different, rarer banner — "⚠️ CodeGraph auto-sync is DISABLED…" or "⚠️ CodeGraph auto-sync is RECOVERING…" — means the whole index may be stale; until the full catch-up completes, Read files directly to confirm anything that may have changed. If a response refuses to answer from changed files or says freshness could not be verified within its budget, retry after sync or narrow the query; that refusal is not an error in the tool.
- **A file flagged "⚠ changed on disk after the last index sync" drifted from its index** (most common on projects queried via `projectPath`, which have no live watcher). Codegraph never serves a possibly-mis-sliced body from such a file — it either shows the file's full CURRENT source (trust it as a Read) or omits the source with this flag. When the source was omitted, Read that specific file; line numbers referencing it elsewhere in the response may be shifted until that project's next sync. All unflagged files remain trustworthy.

- **Source is re-served on every call by default**, including for fresh subagents and after context compaction. Cross-call dedup requires `CODEGRAPH_EXPLORE_DEDUP=1` and is only suitable for hosts that guarantee one durable context per connection. With that opt-in, **"Already sent earlier in this conversation"** points to exact, unchanged source returned by an earlier `codegraph_explore` in that context. Use that copy; don't re-fetch it and don't Read the file. The bytes it freed went into source you have not seen yet, elsewhere in the same response.

## Limitations

- If a tool reports a project isn't indexed (no `.codegraph/`), stop calling codegraph tools for that project for the rest of the session and use your built-in tools there instead. Indexing is the user's decision — mention they can run `codegraph init` if it comes up, but don't run it yourself.
- Index lags file writes by ~1 second.
- Cross-file resolution is best-effort name matching; ambiguous calls may return multiple candidates.
- No live correctness validation — that's still the TypeScript compiler / test suite / linter's job. Codegraph supplements those with structural context they don't have.
"#;

const SERVER_INSTRUCTIONS_NO_ROOT_INDEX: &str = r#"# Codegraph — available (per-project; pass projectPath)

Codegraph is a knowledge graph of a codebase's symbols, edges, and
files (TypeScript/JavaScript including TSX/JSX, Vue, Svelte, Python, Go, Rust, Java, Kotlin,
C/C++, C#, PHP, Ruby, and Swift): one `codegraph_explore` call returns the verbatim, line-numbered source
of the relevant symbols PLUS the call paths between them and a blast-radius
summary — replacing a grep + Read loop with one round-trip.

This server started somewhere with no `.codegraph/` of its own, so there is no
default project — but the tools are available and work **per project**:

- To query a project that HAS a `.codegraph/` index (e.g. a service inside a
  monorepo, or a second repo), pass its path as `projectPath` to
  `codegraph_explore` (and any other codegraph tool). Codegraph resolves the
  nearest `.codegraph/` at or above that path and answers from it — for as many
  projects as you like in one session.
- For a project with no `.codegraph/`, use your built-in tools (Read/Grep/Glob)
  for that project. Indexing is the user's decision — don't run it yourself, but
  if it comes up they can run `codegraph init` in a project to enable codegraph
  there (a new index is picked up live, no restart).
"#;

pub const VERBATIM_NOTE: &str = "> The code below is the **verbatim, current on-disk source** of these files — re-read from disk on this call and line-numbered, byte-for-byte identical to what the Read tool returns. It is NOT a summary, outline, or stale cache. Treat each block as a Read you have already performed: do not Read a file shown here.";

pub const FLOW_FOOTER: &str = "> Full source for these symbols is below — the call flow among them, followed by their bodies.";

pub const POINTER_HEADER: &str = "**Not shown above — explore these names for their source**";

pub fn lost_note() -> String {
    lost_note_for(OutputMode::Mcp)
}

pub fn lost_note_for(mode: OutputMode) -> String {
    format!("> (Trailing pointer list omitted for size. The source above is complete and verbatim — treat it as already Read. For anything this call did not cover, run another {} with the specific names rather than reading those files.)", explore_inline(mode))
}

pub fn cut_note(trimmed: bool) -> String {
    cut_note_for(OutputMode::Mcp, trimmed)
}

pub fn cut_note_for(mode: OutputMode, trimmed: bool) -> String {
    let ex = explore_inline(mode);
    if trimmed {
        format!("\n\n> (Trailing notes omitted for size. The source above is verbatim — treat it as already Read. For symbols its gap markers name, and anything else not covered, run another {ex} with those names rather than reading.)")
    } else {
        format!("\n\n> (Trailing notes omitted for size. The source above is complete and verbatim — treat it as already Read. For anything this call did not cover, run another {ex} with the specific names rather than reading those files.)")
    }
}

pub fn truncated_note(trimmed: bool) -> String {
    truncated_note_for(OutputMode::Mcp, trimmed)
}

pub fn truncated_note_for(mode: OutputMode, trimmed: bool) -> String {
    let ex = explore_inline(mode);
    if trimmed {
        format!("\n\n... (output truncated to budget; the source above is verbatim — treat it as already Read. For names its gap markers list, or any area not covered, run another {ex} — do NOT Read these files.)")
    } else {
        format!("\n\n... (output truncated to budget; the source above is complete and verbatim — treat it as already Read. For any area not covered, run another {ex} with the specific names — do NOT Read these files.)")
    }
}

pub fn trimmed_sections_note() -> String {
    trimmed_sections_note_for(OutputMode::Mcp)
}

pub fn trimmed_sections_note_for(mode: OutputMode) -> String {
    format!("> Some file sections were trimmed for size. Elided symbols are named inside gap markers as `name (file:line)` and preferred in the file header — run another `{}` with those exact names for their source.", explore_tool_for(mode))
}

pub fn budget_note(file_count: usize, calls: u32) -> String {
    budget_note_for(OutputMode::Mcp, file_count, calls)
}

pub fn budget_note_for(mode: OutputMode, file_count: usize, calls: u32) -> String {
    format!(
        "> **Exploration guidance — advisory only, NOT a quota: this project (~{} files indexed) is usually covered in ≈{calls} focused explore calls, and extra calls are never rejected or rate-limited.** If the response above does not fully cover your question, run another {} on the uncovered symbols — it is cheaper and more complete than Read. Only stop exploring when the response actually covers the flow you asked about.",
        thousands(file_count),
        explore_inline(mode)
    )
}

pub fn not_in_index_note(names: &[String], with_results: bool) -> String {
    let one = names.len() == 1;
    let list = names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
    let effect =
        if with_results { format!(", so the code below matches the query's other words, not {}.", if one { "it" } else { "them" }) } else { ".".to_owned() };
    format!(
        "> **Not in the index:** {list}. No indexed symbol, file, signature or comment uses {}{effect} If {} defined in this project, the file{} defining {} {} not indexed: `{} status` shows what is. If {} in a folder skipped by default, like `build/` or `dist/`, the user can include that folder by adding a re-include such as `!/build/` to `.gitignore`.",
        if one { "this name" } else { "these names" },
        if one { "it is" } else { "they are" },
        if one { "" } else { "s" },
        if one { "it" } else { "them" },
        if one { "is" } else { "are" },
        brand::BIN_NAME,
        if one { "it lives" } else { "they live" },
    )
}

pub fn stale_banner(paths: &[String]) -> String {
    let lines: Vec<String> = paths.iter().map(|p| format!("  - {p} (modified after the last index sync)")).collect();
    format!(
        "⚠️ Some files referenced below were edited since the last index sync — their codegraph entries may be stale:\n{}\nFor accurate content of those specific files, Read them directly. The rest of this response is fresh.",
        lines.join("\n")
    )
}

pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub const BARE_GAP_MARKER: &str = "\n\n... (gap) ...\n\n";

pub fn blast_radius_header() -> &'static str {
    "**Blast radius — what depends on these (update/verify before editing)**"
}

pub fn dynamic_links_header() -> &'static str {
    "**Dynamic-dispatch links among your symbols**\n(synthesized — the indirect hops grep/Read would reconstruct; the `@file:line` is the wiring site)"
}

pub fn flow_header() -> &'static str {
    "**Flow (call path among the symbols you queried)**"
}
