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
Start with **`--compact --max-chars 6000 --max-files 6`** to get exact file
paths, line numbers, symbol outlines, call paths, and blast radius in ~1–6 KB
without dumping redundant raw file bodies into context (omit `--compact` only
for a focused single-symbol lookup).

- **Architecture, flow, or multi-symbol triage (recommended default — compact map first)**:
  ```sh
  cgr explore "AuthService loginUser SessionStore" --compact --max-chars 6000 --max-files 6
  cgr explore "how does HTTP request reach database" --compact --max-chars 6000 --max-files 6
  ```
- **Single symbol** (focused mode — definition source + grouped prod/test callers + callees):
  ```sh
  cgr explore "resolve_call" --max-chars 8000
  ```
- **Cross-call session deduplication**:
  - Pass `--session <id>` (or set `CGR_SESSION=<id>`) across consecutive `explore`, `node`, `callers`, and `callees` calls in the same task so unchanged code ranges and graph results already sent earlier in the conversation are replaced with 1-line back-references reporting bytes saved (use `--no-dedup` to force full verbatim output).

## 3. Targeted Graph Commands

| Goal | Command |
|---|---|
| Read one or more symbols' source + caller/callee trail (bounded) | `cgr node <sym1> [sym2 ...] --code --max-chars 8000 [--file <path-or-suffix>]` |
| Structural symbol map of a file (cheap overview) | `cgr node <file-path> --symbols-only` |
| Incoming callers (supports multiple symbols + `fnRef`) | `cgr callers <sym1> [sym2 ...] [--file <path>] [--limit 20]` |
| Outgoing callees (supports multiple symbols) | `cgr callees <sym1> [sym2 ...] [--file <path>] [--limit 20]` |
| Refactor blast radius (supports multiple symbols) | `cgr impact <sym1> [sym2 ...] --depth 2 [--file <path>]` |
| Trait / interface / subclass implementations | `cgr implementations <sym1> [sym2 ...] --depth 10` |
| Tests impacted by modified files (narrow candidate list) | `cgr affected <file1> <file2> [--cross-tier] --json` |
| Fast symbol search (locations only, no code) | `cgr query <text> [--kind function\|method\|class\|interface\|type\|component\|route] --json` |
| Indexed file layout / package breakdown | `cgr files [--format tree\|flat\|grouped\|package] [--tests\|--no-tests]` |
| Detect circular imports (exit 1 if cycles found) | `cgr cycles --json` |

Tip: When a symbol name is ambiguous across modules, pass `--file <path-or-suffix>`
to `node`, `callers`, `callees`, or `impact`. When `callers`, `callees`, `impact`,
or one-symbol `explore` finds 0 resolved edges, check any appended
`unresolved / ambiguous references` (`tail_idx`) note for dynamic-dispatch
candidate sites and follow up with targeted `rg` (check `cgr status --json`'s
`refs.resolvedRate` to gauge static coverage in dynamic repos).

## 4. Efficiency Rules & Pitfalls to Avoid

- **Compact structural map first, targeted `read_file` for edits**: Use
  `cgr explore "<query>" --compact --max-chars 6000 --max-files 6` to pinpoint
  files, symbols, and line numbers first, then jump directly to targeted
  `read_file(StartLine, EndLine)` only for the lines you need to inspect or edit.
  This prevents double-reading raw file bodies across graph output and `read_file`.
- **Always bound `cgr node --code` and never dump whole files**: Always pass
  `--max-chars 8000` when calling `cgr node <s1> [s2 ...] --code`, and **never**
  pass whole file paths to `cgr node <file> --code` (use `--symbols-only` for
  file overviews or targeted `read_file` for large ranges). Do **not**
  micro-paginate files in repeated 40–60 line `--offset --limit` slices.
- **Batch multi-symbol queries & use inline `--sync`**: Combine related symbols
  in a single call (`cgr callers s1 s2`, `cgr callees s1 s2`,
  `cgr impact s1 s2 --depth 2`, `cgr implementations s1 s2`) instead of
  one-symbol-per-turn loops. After edits, pass `--sync` directly on your next
  query (`cgr --sync callers ...`) rather than spending an extra turn on `cgr sync`.
- **Right tool for the query (`cgr` vs `rg`/`grep`)**: Use `cgr` for symbols,
  call chains, implementations, and blast radius. Follow up with targeted `rg`
  when `cgr` reports `unresolved / ambiguous references` (`tail_idx`), and use
  `rg` / `grep` directly for non-symbol literals (protocol/command strings,
  error messages, config keys, comments).
- **Skip cold `cgr init` for trivial known-path tweaks**: If the exact file is
  already known and the edit is purely local (copy/config/single-file tweak),
  use `read_file` directly without paying cold-start turns.
- **Disciplined narrow verification**: Run the narrowest relevant test target
  for the modified module first and finish once passing — do **not** blindly
  execute the entire transitive test closure returned by `cgr affected` on
  scoped fixes (reserve full-suite runs for broad cross-module refactors).

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
