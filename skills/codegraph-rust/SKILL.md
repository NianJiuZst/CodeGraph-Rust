---
name: codegraph-rust
description: >-
  Fast local code knowledge graph CLI (`cgr`) for exploring codebases, tracing
  cross-file call chains, finding callers/callees/implementations, assessing
  refactor impact, locating affected tests, and detecting import cycles across
  15 languages (TypeScript/JavaScript, Vue/Svelte SFC, Python, Go, Rust, Java,
  Kotlin, C/C++, C#, PHP, Ruby, Swift). Use this skill when navigating an
  unfamiliar or medium-to-large codebase, tracing how symbols connect across
  files, checking blast radius before a refactor, or when the user mentions
  `cgr` or CodeGraph.
---

# CodeGraph Rust (`cgr`)

`cgr` indexes a repository locally into `.cgr/` (mmap segments + bidirectional
CSR call graph + FST/BM25 search) and answers structural queries in
sub-millisecond latency without external services, APIs, or Node.js.

## 1. Check or Initialize the Index

Always pass `-p <project_dir>` when the target repository is not the current
working directory (`cgr` also locates the nearest `.cgr/` upward).

```sh
cgr status --json
```

- **Not indexed yet** (command fails with no `.cgr/` found):
  Initialize without touching tracked git files:
  ```sh
  cgr init --local-only
  ```
  (`--local-only` ignores `.cgr/` via `.git/info/exclude`, keeping `git status` clean.)
- **Pending edits detected** (`added` / `modified` / `removed` > 0) or **after you edit files**:
  Pass `--sync` directly on your next query command instead of spending a separate turn on `cgr sync` (or export `CGR_AUTO_SYNC=1` for the shell session):
  ```sh
  cgr --sync callers mySymbol
  cgr --sync affected src/changed.ts --json
  ```
  (Use `cgr sync` standalone only when you want an explicit sync report without querying.)

## 2. Primary Entry Point: `cgr explore`

Run `cgr explore` **first** instead of multi-round `grep` + `read_file` loops.
It returns verbatim line-numbered source windows (`<n>\t<line>`) plus cross-file
call paths in one bounded response. Treat returned code windows as already read.

- **Single symbol** (focused mode — definition source + grouped prod/test callers + callees):
  ```sh
  cgr explore "resolve_call"
  ```
- **Flow between symbols or architecture question**:
  ```sh
  cgr explore "AuthService loginUser SessionStore"
  cgr explore "how does HTTP request reach database" --max-files 6
  ```
- **Low-context triage & cross-call deduplication**:
  - Pass `--compact` to inspect call flow, blast radius, and symbol outlines without dumping full bodies, or `--max-chars 6000` to cap output size.
  - Pass `--session <id>` (or set `CGR_SESSION=<id>`) across consecutive `explore`, `node`, `callers`, and `callees` calls in the same task so unchanged code ranges and graph results already sent earlier in the conversation are replaced with 1-line back-references reporting bytes saved (use `--no-dedup` to force full verbatim output).

## 3. Targeted Graph Commands

| Goal | Command |
|---|---|
| Read one or more symbols' source + caller/callee trail | `cgr node <sym1> [sym2 ...] --code [--file <path-or-suffix>]` |
| Read a file + dependent files (use large blocks) | `cgr node <file-path> [--offset N --limit 300] [--max-chars N]` |
| Structural symbol map of a file (cheap overview) | `cgr node <file-path> --symbols-only` |
| Incoming callers (supports multiple symbols + `fnRef`) | `cgr callers <sym1> [sym2 ...] [--file <path>] [--limit 20]` |
| Outgoing callees (supports multiple symbols) | `cgr callees <sym1> [sym2 ...] [--file <path>] [--limit 20]` |
| Refactor blast radius (supports multiple symbols) | `cgr impact <sym1> [sym2 ...] --depth 2 [--file <path>]` |
| Trait / interface / subclass implementations | `cgr implementations <sym1> [sym2 ...] --depth 10` |
| Tests impacted by modified files | `cgr affected <file1> <file2> [--cross-tier] --json` |
| Fast symbol search (locations only, no code) | `cgr query <text> [--kind function\|method\|class\|interface\|type\|component\|route] --json` |
| Indexed file layout / package breakdown | `cgr files [--format tree\|flat\|grouped\|package] [--tests\|--no-tests]` |
| Detect circular imports (exit 1 if cycles found) | `cgr cycles --json` |

Tip: When a symbol name is ambiguous across modules, pass `--file <path-or-suffix>`
to `node`, `callers`, `callees`, or `impact`. When `callers`, `callees`, `impact`,
or one-symbol `explore` finds 0 resolved edges, check any appended
`unresolved / ambiguous references` note for dynamic-dispatch candidate sites
(and check `cgr status --json`'s `refs.resolvedRate` to gauge static coverage in dynamic repos).

## 4. Efficiency Rules & Pitfalls to Avoid

- **Never micro-paginate files with `cgr node <file>`**: Do **not** read large
  files in repeated 40–60 line `--offset --limit` slices. First lock onto the
  relevant symbols with `explore` / `callers` / `impact` and read them via
  `cgr node <symbol> --code`; when you need full-file or wide context (e.g.
  test fixtures or editing a large module), read in large contiguous chunks
  (250–500 lines via `read_file` or one wide `--limit` call) instead of many
  tiny shell round-trips.
- **Batch independent graph queries**: When you already know the symbols to
  inspect, combine `callers`, `impact`, `implementations`, and `node <sym> --code`
  into a single shell command (or parallel calls) to minimize LLM turns.
- **Right tool for the query (`cgr` vs `rg`/`grep`)**: Use `cgr` for symbols,
  call chains, implementations, blast radius, and affected tests. Use `rg` /
  `grep` directly for non-symbol literals (protocol/command strings, error
  messages, config keys, comments).
- **Skip cold `cgr init` for trivial known-path tweaks**: If the exact file is
  already known and the edit is purely local (copy/config/single-file tweak),
  use `read_file` directly without paying cold-start turns.
- **Fast iteration + full verification**: Use `cgr affected <files...> --json`
  for fast targeted test loops while iterating, then finish cross-module
  changes with the repo's typecheck/lint/full test suite (when practical) to
  cover dynamic dispatch and macro expansions outside the static graph.

## 5. Verification & Troubleshooting

- Verify index integrity and freshness: `cgr status --verify --strict`
- Inspect unresolved call sites in a repo: `cgr debug unresolved`
- Force a full rebuild (e.g. after major structural changes or `cgr.json` edits): `cgr index`
- Clean up `.cgr/` if the user wants it removed: `cgr uninit --yes`

## 6. Scope & Boundaries

- **Extracted languages (15)**: TypeScript/TSX, JavaScript/JSX, Vue SFC (`.vue`),
  Svelte SFC (`.svelte`), Python, Go, Rust, Java, Kotlin, C, C++, C#, PHP, Ruby, Swift.
- **Route hooks (13)**: Express, Flask, FastAPI, Django, Gin, Spring, Axum,
  Next.js, Nuxt, SvelteKit, NestJS, React Router, TanStack Router.
- **Static analysis limit**: `cgr` is a static graph, not a full compiler/LSP.
  Highly dynamic dispatch or ambiguous overloads may be omitted; verify critical
  call paths against source and tests before making high-risk edits.
