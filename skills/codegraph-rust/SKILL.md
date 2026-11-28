---
name: codegraph-rust
description: >-
  Fast local code graph CLI (`cgr`) for locating symbols, tracing cross-file
  call chains, checking callers/implementations/blast radius, and finding
  affected tests across 15 languages. Use for unfamiliar codebases, cross-file
  navigation, bug localization, or refactors; skip for trivial single-file edits
  at a known path.
---

# CodeGraph Rust (`cgr`)

Pass `-p <dir>` only when outside the target repo. Do **not** run `cgr status`
first — run your query directly; if it fails with `no .cgr index found`, run
`cgr init --local-only` once and retry.

## 1. Pick One Entry Query (Turn 1)

| Starting point | Command (pick ONE) |
|---|---|
| Unfamiliar area / vague bug (no exact symbol yet) | `cgr explore "<terms>" --compact --max-chars 5000 --max-files 6` |
| One known symbol (body + prod/test callers + callees) | `cgr explore "<Sym>" --max-chars 6000` *(omit `--max-files`!)* |
| Known traceback `file:line` | `cgr explore "<path/file.ext:line>" --max-chars 6000` |
| Flow across 2–4 known symbols | `cgr explore "<Class.methodA> <Class.methodB>" --compact --max-chars 5000 --max-files 6` |
| Exact file known & purely local tweak / non-code file | Skip `cgr`; use `read_file` or `rg` directly. |

Query tips:
- Prefer exact `camelCase`/`snake_case` or qualified names (`Class.method`,
  `mod::fn`) over bare lowercase/single-word names (`parse`, `Router`) so
  call-flow and blast-radius matching trigger.
- Treat any `<n>\t<line>` source block returned by `cgr` as verbatim on-disk
  source (edit-ready) — **never** re-read those same lines with `read_file`.

## 2. Targeted Follow-up Commands (Batch Symbols, Always Cap Size)

| Need | Command |
|---|---|
| Signature + 1-hop callers & callees (cheapest lookup) | `cgr node <s1> [s2 ...] [--file <suffix>] --max-chars 4000` |
| Symbol source + caller/callee trail | `cgr node <s1> [s2 ...] --code [--file <suffix>] --max-chars 6000` |
| File symbol map (cheap file overview) | `cgr node <file-path> --symbols-only --max-chars 4000` |
| Callers / callees (batch multiple symbols) | `cgr callers\|callees <s1> [s2 ...] [--file <suffix>] --limit 20 --max-chars 4000` |
| Refactor blast radius / implementations | `cgr impact <s1> [s2 ...] --depth 2 --max-chars 4000` / `cgr implementations <s1> [s2 ...] --max-chars 4000` |
| Symbol name search (plain text, with typo suggestions) | `cgr query "<text>" [--kind function\|method\|class\|interface\|type\|variable\|route\|component] --limit 8 --max-chars 2500` |
| Scoped directory layout | `cgr files <dir> --format flat --no-metadata --max-chars 3000` *(for repo root only: `cgr files --max-depth 2 --no-metadata --max-chars 3000`)* |
| Candidate impacted test files (compact list) | `cgr affected <f1> [f2 ...] [--cross-tier] --quiet` |

## 3. Strict Token & Accuracy Rules

1. **Stop exploring early & keep reads narrow**: Aim for **1–2 `cgr` calls**
   to locate target symbols/lines and blast radius, then read **only ±40 lines**
   around matched line numbers via `read_file(StartLine, EndLine)` — avoid
   dumping >300-line whole files into context unless refactoring the entire file.
2. **Never dump or micro-slice files via `cgr node`**: Never run unfiltered
   `cgr node <file>` or repeated 40–60 line `--offset --limit` loops; use
   `--symbols-only` or bounded `read_file` ranges instead.
3. **Inline sync & session dedup**: After edits, pass `--sync` directly on the
   next query (`cgr --sync callers ...`) instead of a separate `cgr sync` turn
   (skip if `CGR_AUTO_SYNC=1` is already set). Pass `--session <id>` across
   consecutive `explore`/`node`/`callers`/`callees` calls (unless `CGR_SESSION`
   is set) so repeated bodies and caller lists collapse to 1-line back-references.
4. **Trust `tail_idx` fallback & pair with `rg`**: Static graphs miss dynamic
   dispatch, reflection, and non-symbol literals. Whenever `cgr` prints an
   `unresolved / ambiguous references` or `Dynamic-dispatch note (tail_idx)`
   hint—or when searching error strings/config keys/macros—follow up immediately
   with targeted `rg -n`.
5. **Narrow verification first**: Run the narrowest relevant test file/case
   from the modified module (or top candidate from `cgr affected --quiet`)
   instead of blindly running the whole transitive test closure.
