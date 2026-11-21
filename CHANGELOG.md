# Changelog

## Unreleased

### Upstream v1.6.4 kernel, multi-language precision/recall, and query parity

- **Re-vendor upstream v1.6.4 (`461a5d8`)** in `crates/cg-kernel` (preserving all 4 reproducible
  patches via `scripts/sync-kernel.sh`), and cache one `tree_sitter::Parser` per language per
  worker thread (`cg-kernel/src/hooks.rs`, `cg-extract/src/facts/rust.rs`,
  `cg-extract/src/macro_mask.rs`) with tight progress-callback deadline checks.
- **Multi-language precision & recall (`ENGINE_VERSION = 13`)**:
  - **Rust**: fix `use crate_name::{self as alias}` empty-spec binding, wildcard submodule
    resolution (`use pkg::*; submod::fn`), inline submodule imports (`mod a { pub mod b; }`),
    `std::pin::Pin` / `core::pin::Pin` smart-pointer deref, and imported module/package receivers.
  - **Python**: uppercase class-valued attributes (`client_class = Client`), `with ... as` and
    walrus (`:=`) bindings, structural chained calls, and class-valued attribute instantiation
    (`cls.client = cls.client_class()`).
  - **Kotlin / Java**: structural chained fluent builder calls (`OkHttpClient.Builder().dns().build()`),
    Java casts, `LangFamily::Jvm` implicit `this` and enclosing class field fallbacks, and
    dispatch-receiver-scoped Kotlin member extension functions (`private fun Editor.setString()`).
  - **Go**: prefer concrete struct composite-literal initializers over interface annotations on
    package/local `var`s (`var JSON Binding = jsonBinding{}`), and support calling function-valued
    top-level `var`s.
  - **C#**: extract generic DI getters (`GetRequiredService<T>`, `GetService<T>`,
    `GetRequiredKeyedService<T>`, `GetKeyedService<T>`, `GetInstance<T>`, `Resolve<T>`), chained
    calls in `call_path`, and reject non-callable `Property`/`Field`/`EnumMember` targets for
    `RefKind::FunctionRef`.
  - **PHP**: ASCII case-insensitive method lookup (`#3297`), relative `__DIR__`/`dirname(__FILE__)`
    includes, `$_SERVER['DOCUMENT_ROOT']` includes via configured `webRoot` (`cgr.json` /
    `codegraph.json`) or auto-inferred web roots with incremental sync invalidation.
  - **C++**: detect declaration-only C++ `.h` headers (`#3397`, `looks_like_cpp_decl`).
- **Query, CLI, and MCP improvements**:
  - Focused one-symbol `explore` mode (`#3407`): single-symbol queries return verbatim `<n>\t<line>`
    definition source, grouped production/test dependent sites, and callees, with automatic
    fallback on multi-symbol queries, explicit `maxFiles`, or stale files.
  - `cgr affected`: include cross-file same-package symbol dependencies (Go/Java/Kotlin/C#/Ruby/Swift)
    and gate process-boundary links (HTTP, queues, sockets, Celery) behind `--cross-tier` (`#3320`)
    while preserving local in-process `@OnEvent` links.
  - `cgr node <file>`: use fast single-file `Graph::file_dependents` instead of building a full
    repository `file_graph()` on every call.
  - `cgr files` & `codegraph-rust_files`: fix `--format package` via cached on-disk manifest
    detection (`package.json`, `composer.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`,
    `pom.xml`, `build.gradle`), and add `--tests` / `--no-tests` (`testsOnly` / `excludeTests`).
  - `cgr implementations -d/--depth`: make inheritance traversal depth configurable.
  - `cgr callers` / `callees` / `impact`: wire `fnRef` (`callback registration`) edge labels and
    surface stale-file warnings when referenced files drifted on disk.
  - Update MCP server instructions to accurately reflect all 13 extracted languages.

### Engine performance and surgical incremental sync

- **Scanner (`cg-scan`)**: pure-Rust global gitconfig `core.excludesFile` fast-path (with Windows
  `USERPROFILE` fallback and `[include` fallback to `git`), `RwLock<FxHashMap<String, _>>` nested
  `.gitignore` cache, `O(1)` per-directory ignore checks, pre-stat language/config filtering,
  unified single-walk config discovery, and overlapped `git ls-files`.
- **Extraction & Resolution (`cg-extract`, `cg-resolve`)**: single shared 64 MiB-stack Rayon pool
  for foreground CLI runs (low-priority retained for background MCP rebuilds/watchers/compaction),
  `ProjectInfo::hash_known` / `load_known`, dynamic LPT extraction scheduling, Rust `cfg_*!`
  candidate pre-scan, bit-reversed LPT resolution scheduling, and `WorkerCx::file_cx` cache
  widening (`6 -> 64`).
- **Storage (`cg-store`)**: parallelize `finish()` across edge assembly, tail index, 4-way
  `build_name_indexes`, and chunked `build_token_index` with byte-identical output; cap radix
  edge-sort scratch; honor `light` delta fsync in `sync_dir` on macOS.
- **Surgical incremental sync (`cg-sync`)**: diff resolver facts via `symmetric_difference`, split
  broad vs symbol-level export invalidation through barrels/aliases and JS value forwarders,
  generalize `BindKind::Field`, Go file-scope `BindKind::Local`, and cross-file receiver
  invalidation (`changed_receivers`), add PHP case-insensitive method & added-file include
  invalidation, and raise delta bailout thresholds (`25/75/50%`) with early pre-resolution bailout.
- **Query (`cg-query`)**: skip Levenshtein-2 DFA for already-matched tokens, cap fuzzy DFA lengths,
  fuse whole-query and per-word explore search into a single pass, use `select_nth_unstable_by`,
  and eliminate per-line formatting allocations in `window.rs`.

## 0.1.0 — first release

### Language coverage and Rust macro items

- Vendor the upstream C/C++, C#, PHP, Ruby and Swift walkers and the Swift grammar through
  kernel patch P5 (`scripts/sync-kernel.sh` re-vendors them; each language has a cargo feature).
  A `.h` header with C++ constructs is parsed with the C++ grammar, as upstream does.
- Facts extractors for the five languages (scopes, typed parameters/locals/fields, imports,
  namespaces), and resolution rules: `#include` by relative path and path suffix, C# namespaces
  including the enclosing ones, PHP namespaced `use` names, Ruby `require`/`require_relative`,
  Swift modules per SwiftPM target; implicit-`this` member calls, call-name normalisation
  (PHP `->`/`parent::`/`self::`, C# `?.`/`base.`, Ruby `@ivar`, C++ arity suffixes), nested type
  paths, qualified callables, and members declared in other files (partial classes, extensions,
  reopened classes, out-of-line C++ definitions). Per-family builtin and common-method lists
  keep untyped receivers from linking to same-named project methods.
- Rust: items inside conditional wrapper macros (`cfg_rt! { pub struct X; impl X { .. } }`,
  Tokio's `cfg_*!` family) are extracted from a position-preserving view with only the wrapper
  tokens blanked. Node identities and positions of everything else are unchanged; the original
  text is used again if blanking introduces a syntax error. On Tokio this adds ~740 nodes and
  ~2,200 resolved call references.
- Self-review fixes: depth bounds (iterative helpers) in the five extractors; deterministic
  tie-breaking for cross-file members and qualified names; C++ `::a::f` / `::f()` resolved in the
  global namespace only (no self-links from `test::open` wrappers), `::std::..` as builtin, and
  unqualified `get`/`swap`/`move`/.. in C++ files as standard functions; PHP `parent::__construct()`
  and namespaced names; Ruby single-segment `require` limited to load-path roots; an explicit Swift
  standard-type list with nested-type shadowing; Objective-C headers recorded as file nodes; the
  `cfg_*!` pre-pass only blanks closed wrappers and only when it does not add syntax errors.
- Fix `file_node_only` building a node row 8 bytes larger than the kernel ABI row (a debug
  assertion panic on any kernel failure in debug/test builds).
- **Engine rules version 5**: indexes built by an older engine are rebuilt on the next sync.

### Engine performance follow-through

- Add event-path sync, bounded watcher events and source-free snapshot compaction after a
  quiet period. Keep full scanning/rebuild fallbacks for broad or structural changes.
- Spill accumulated kernel buffers to immutable temporary mappings; cap resolver caches and
  indexing workers by the memory tuning budget. Use the configured pool for table construction
  and lower indexing-worker priority on Linux/macOS/Windows.
- Preallocate resolution outcomes, fuse hook trigger scans, and link event keys in bounded,
  deterministic parallel chunks with selective incremental reconnection.
- Keep package/module representatives ordered by source path and track package/declaration
  import dependencies, preserving incremental/full equality when package membership changes.
  Engine revision 2 refreshes older dependency records on rebuild.
- Compress BM25 postings, persist global document lengths, and add adaptive radix edge sorting.
  **Snapshot format 2 requires rebuilding old indexes with `cgr index`.**
- Use bounded source text/line-offset views; add MCP backpressure and cooperative cancellation,
  coalesce background rebuilds, release watcher resources on LRU eviction, and bound shutdown.
- Bound prompt-hook discovery; repair smoke-helper stdin and timer cleanup on macOS Bash 3.2.
- Document implementation, correctness validation and remaining boundaries in
  [English and Chinese](docs/PERFORMANCE-IMPLEMENTATION.md). The historical 6.40× comparison is
  unchanged; a new benchmark was refused by the resource preflight, so no additional speedup
  is claimed.

### Initial milestones M0–M4

**Engine**
- Static single binary `cgr`; vendored upstream tree-sitter kernel (TypeScript/JavaScript, Python,
  Go, Java, Rust, Kotlin, C/C++, C#, PHP, Ruby, Swift), re-vendorable with `scripts/sync-kernel.sh`.
- Immutable, memory-mapped snapshot format (header, checksummed partitions, CSR edges, FST name
  indexes, trigram and BM25 indexes), published atomically; readers never block the writer.
- Reference resolution in rounds (module tables, export closure, scope/import/receiver/qualified
  strategies, "better unresolved than wrong"); route extraction for Express, Flask, FastAPI,
  Django, Gin, Spring, Axum, Next.js; synthesised dynamic-dispatch edges for interfaces and
  implementations, Go structural interfaces, event emitters, React render and JSX children.
- Incremental sync: delta layer over a base segment, re-resolution of only the affected
  references, thresholds that fall back to a full rebuild, automatic merge; the result equals a
  full rebuild (property test with random edit sequences, plus sabotage-checked rule coverage).
- File watcher with a lock-based election (one watcher per project across sessions).

**Agent surface**
- MCP server over stdio and HTTP: upstream tool schemas/descriptions/instructions with the
  `codegraph-rust_` prefix, explore-only default surface, `roots`/`--path`/`projectPath`
  precedence, success-shaped errors, 30 s call budget, in-flight calls answered on EOF.
- `cgr install` / `uninstall` for Claude Code, Cursor and Codex (comment-preserving JSONC/TOML
  edits, marker blocks, idempotent and reversible), Claude Code `UserPromptSubmit` hook.
- CLI: init, index, sync, status, query, explore, node, files, callers, callees, impact,
  implementations, affected, cycles, serve, install, uninstall, uninit, export --sqlite, version.
- Development tools: `debug dump|unresolved|diff`, `bench index|sync|explore|compare`,
  `scripts/bench-local.sh` (memory-guarded), `scripts/smoke.sh`.

**Release engineering**
- CI: rustfmt, clippy `-D warnings`, kernel re-vendoring check, tests on Linux/macOS/Windows,
  smoke test, static musl build. Release workflow for six targets with checksums; install scripts.

Not in this release: other upstream languages; Vue/Svelte;
code signing and notarisation; npm/PyPI wrappers; Homebrew tap. See the README's current limitations.
