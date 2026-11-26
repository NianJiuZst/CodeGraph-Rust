# Architecture

`cgr` is a Cargo workspace that produces one binary. This page is the map; the module headers in
the source carry the details. Section numbers like "design doc 5.3" refer to the technical design
document the project was built from.

## Pipeline

```
files ──scan──▶ extract (tree-sitter kernel, parallel) ──▶ merge ──▶ resolve (rounds) ──▶ synthesise
                                                                                            │
 MCP / CLI ◀── query ◀── View{base, delta} ◀── mmap'd segments ◀── assemble & write ◀──────┘
```

| crate | role |
|---|---|
| `cg-core` | ids, kinds, config files (`cgr.json`), memory/thread budget, branding constants (`brand.rs` is the only place that spells the product name, binary name, data dir and tool prefix) |
| `cg-kernel` | vendored upstream extraction kernel + patches P1 (no napi, MVP languages), P2 (keep error trees), P4 (parse hook with timeout) |
| `cg-scan` | which files are indexed: ignore rules, language by extension/shebang, generated/test/deprioritised flags |
| `cg-extract` | runs the kernel per file with panic and timeout isolation; produces node/edge rows plus *facts* (imports, exports, scopes, bindings, references) |
| `cg-store` | the snapshot format, builders, `View` (base + delta), the `.cgr/` directory (manifest, locks, publishing, GC) |
| `cg-resolve` | module tables, export closure, resolution rounds, framework route hooks, synthesised edges, the full-index driver |
| `cg-query` | search (routes, BM25, trigram, FST), traversals, the `explore` pipeline, the tool handlers shared by MCP and the CLI |
| `cg-sync` | change detection, the incremental algorithm, delta layers, merge, the file watcher |
| `cg-mcp` | JSON-RPC core, stdio and HTTP transports |
| `cg-install` | agent configuration editors (Claude Code, Cursor, Codex) |
| `cgr` | the CLI, `export --sqlite`, `bench`, prompt hook |

## Snapshot format (`cg-store`)

A segment is written once, in one sequential pass, and never modified: a 64-byte header, a
partition directory (id, element size, offset, byte length, count, xxh3 checksum) and 8-byte
aligned partitions that are cast in place from the memory map. Partition ids are part of the
format. Besides node and file records, the segment holds:

* string pools (names, paths, free text) with offset tables, and posting lists by name/qualified name;
* edges as structure-of-arrays CSR in both directions with a packed `out_kind` byte (edge kind,
  provenance, class: extracted / resolved / synthesised / framework);
* the resolver's *facts* (imports, exports, scopes, bindings, per-file reference ranges and their
  outcomes) so incremental sync can re-resolve without re-parsing unchanged files;
* search indexes: FSTs for name / lower-cased name / path segments, a trigram index, a BM25 token
  index with delta-varint posting lists, document frequencies and persisted global document length.

The current format is version 2. Version-1 segments require `cgr index` after upgrading.
Large edge sets use an adaptive radix/comparison sorter; duplicate identities keep the original
class and synthesis-pass precedence.

`.cgr/manifest` names the current base and delta; a new generation is published by writing the
segment, fsyncing, then switching the manifest (`manifest.prev` is kept). Readers take a shared
lock on the files of the snapshot they use, so garbage collection never pulls a file from under a
reader. `Store::open_snapshot` verifies header and partition directory; `cgr status --verify`
checks every checksum.

## Resolution (`cg-resolve`)

References (calls, imports, extends, type uses, …) are extracted unresolved. Resolution works in
two rounds — round 1 for imports, supertypes and type-level references, round 2 for calls and
the rest, which may use the hierarchy produced by round 1 — over a frozen *engine* (module tables,
export closure, per-package indexes). Per reference the strategies run in a fixed order
(framework, lexical scope, import binding, receiver type, qualified name, same package, global
name) and the first answer wins; ambiguity produces no edge ("better unresolved than wrong").
Full builds write outcomes directly into preallocated, disjoint per-file slots. Optional worker
caches have byte limits derived from the memory budget, and table construction uses the same
configured pool as resolution. Each outcome records status, strategy and confidence; unresolved references stay queryable
(`cgr debug unresolved`). Results do not depend on thread count or scheduling.

Route extraction (Express, Flask, FastAPI, Django, Gin, Spring, Axum, Next.js) adds `route`
nodes with handler references. Synthesis passes add edges that only exist at run time —
interface → implementation, Go structural interfaces, event emitter `emit` → `on` handlers, React
`setState` → `render`, JSX parent → child component — each tagged with the pass that made it,
capped per pass so a hub cannot explode, and computed from per-file *sites* persisted in the
snapshot so the link step needs no source. One shared Aho-Corasick trigger scan selects extract
hooks. Event keys link in parallel in bounded chunks; incremental non-structural edits reconnect
changed event keys and changed React/JSX files. Structural changes conservatively run all passes.

## Incremental sync (`cg-sync`)

`delta = f(base, tree)`: a delta layer never stacks on another delta. A sync compares with
the snapshot (mtime/size first, then content hash), re-extracts the changed files, and works out
which base references an edit can change:

* references whose tail name matches a definition that appeared, disappeared or changed;
* references to nodes of replaced files that no longer exist (otherwise nodes are remapped);
* files whose imports now resolve to a different file, and the consumers of files whose exports
  changed, through re-exporting barrels;
* all round-2 references when the class hierarchy changed (compared by position-independent keys,
  so a line inserted above a class is not a hierarchy change);
* files whose synthesised edges changed.

Unchanged files whose references are affected are rewritten as *edges-only* entries. Broad
reference/file invalidation or changed configuration/engine versions can fall back to a full
rebuild. Compaction is a separate pure data rewrite of `View(base + delta)`, preserving facts,
reference outcomes, provenance and metadata without parsing source. After at least 30 seconds
of quiet, >5% affected files (bases of at least 40 files), 64 MiB, 200 syncs or 10 idle minutes
make a delta eligible for compaction. The watcher performs maintenance without a source scan
and yields to an active writer. Readers keep the previous complete snapshot.

`cargo test -p cg-sync` generates random edit sequences over a multi-language
fixture and requires `canonical(incremental) == canonical(full rebuild)` after every step;
disabling any rule makes a specific scenario fail (the rules were sabotage-checked).
`cgr debug diff` runs the same comparison on a real project, `cgr bench sync --verify` does it
after a random edit run.

One watcher per project is elected through `watch.lock` (released by the OS when a process dies,
so another session takes over). A bounded queue feeds debounced paths to sync, avoiding a whole
filesystem walk for normal events. Config/directory changes, rescan flags, errors or lost/burst
events fall back to a full scan. Periodic scanning recovers missing events. The inventory is
still assembled in memory and earlier delta files may be re-extracted against the same base.
The event filter retains ignored sources already present in the published inventory;
`.git/index` and `.git/info/exclude` events request a full scan so tracked-file membership and
local ignore changes are visible. Built-in skips and explicit `exclude` patterns still apply.

Kernel output buffers accumulated during extraction can spill to shared immutable temporary
chunks, then be read through the same borrowed ABI views. Source text is not retained there.
Facts and graph/resolution tables still grow with repository size: `CGR_MEMORY_MB` is a tuning
budget, not a process RSS limit.

## Query and MCP (`cg-query`, `cg-mcp`)

`explore` ranks symbols (name/qualified/path routes fused with BM25), expands along the call
graph, orders a *flow* section first when the query names endpoints, lists the blast radius, and
renders line-numbered source windows under a character budget (13k / 18k / 24k by project size,
hard cap min(1.5×, 25k)). Strings, section titles and notices are the upstream ones with the
product name substituted. Source windows use a bounded owned text buffer and lazy line offsets;
mutable working-tree source is reread for each call rather than mmap'd or cached across calls.
`cg-mcp` is a small hand-written JSON-RPC server: a fixed 4-thread pool with a 16-job admission
queue, a 30 s deadline with cooperative cancellation, project resolution by client roots
→ `--path` → `projectPath`, a per-session history for optional de-duplication, and the "index
lags writes" staleness banner based on `stat` of the files in the answer. Project LRU eviction
also releases watcher resources, and repeated old-format/corrupt-index requests share one
background rebuild. EOF drains short calls briefly, cancels remaining work and stops watchers
against a shared shutdown deadline. Filesystem blocking calls are not forcibly interruptible.

See [performance implementation](PERFORMANCE-IMPLEMENTATION.md) for the design mapping,
validation evidence, benchmark preflight result and work that remains.

## Installer (`cg-install`)

Each agent is an `AgentTarget` that plans a set of `Change`s against the real files: JSONC edits
use the `jsonc-parser` CST so comments and formatting survive, TOML edits use `toml_edit`, and
instruction blocks are delimited by `<!-- CGR_START -->` / `<!-- CGR_END -->`. Existing entries
only get missing keys added; user-pinned `command`/`cwd` are never replaced. Everything is
idempotent (a second run reports *unchanged* and leaves bytes alone) and `uninstall` restores the
original files. Tests run in a temporary `HOME`.

## Deviations from the design document

* HTTP transport is hand-written on `std::net` instead of pulling in an async runtime; stdio is
  hand-written JSON-RPC instead of `rmcp` (the protocol surface used is small and the output is
  byte-controlled).
* Names follow the decision recorded in the design doc (§15.2): product `codegraph-rust`, binary
  `cgr`, data directory `.cgr/`, MCP tools `codegraph-rust_*`.
* `cgr export --sqlite` is a runtime feature (cargo feature `sqlite`, on by default); the design
  listed SQLite as a dev-only dependency.
* Releases use a hand-written GitHub Actions workflow instead of `cargo-dist`.
