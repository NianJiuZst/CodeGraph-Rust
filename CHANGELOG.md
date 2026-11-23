# Changelog

## Unreleased

## 0.3.0 — 2026-11-23

### Context efficiency, minified-bundle skipping, transparent sync, and dynamic-dispatch hints (`ENGINE_VERSION = 15`)

- **Minified / compiled JS bundle fast-skip (`cg-scan`, `cg-extract`)**:
  - Skip Tree-sitter AST extraction for minified and compiled JS bundles (`*.min.[cm]?js`,
    webpack runtimes, Closure/Emscripten bundles such as `draco_encoder.js` and `ammo.wasm.js`),
    retaining them as `GENERATED` file nodes so `status`, `files`, and `node <file>` remain intact.
  - String/template/comment-aware JS syntax scanner in `cg_scan::generated::is_minified_content`
    prevents false positives on hand-written modules embedding large numeric lookup tables
    (e.g. `RectAreaLightUniformsLib.js`), base64 data URIs, or minified CSS/HTML template literals.
  - Real-world impact on `mrdoob/three.js` (1,291 files, `--release`): full indexing drops from
    `1,273 ms` to `405 ms` (**3.1× faster**; extraction `1,124 ms → 295 ms`), peak RSS drops from
    `631 MB` to `245 MB` (**-61%**), and index size shrinks from `23.0 MB` to `15.4 MB` (**-33%**).
- **Session-persistent deduplication & compact/budgeted output (`cg-query`, `cg-mcp`, `cgr`)**:
  - Upgrade `explore::History` to a serializable 32-call sliding window with interval coalescing,
    exact interval subtraction for merged source windows, rendered-range dedup in focused
    one-symbol `explore`, and section-end-offset tracking so truncated bodies are never cached.
  - Add CLI `--session <ID>` / `CGR_SESSION` persistence under `.cgr/sessions/` with per-session
    advisory locking + reload-merge (`History::save_merged`) for parallel CLI calls, plus
    `--no-dedup` opt-out.
  - Add `--compact` (`compact` in MCP `explore`) for low-context call-flow + blast-radius +
    symbol outlines without dumping source bodies (never poisons dedup history), and `--max-chars`
    (`maxChars` in MCP `explore` and `node`) with strict cap enforcement including oversized single lines.
- **Transparent auto-sync, multi-symbol queries, and CLI cleanups (`cg-sync`, `cgr`)**:
  - Add global `--sync` / `--no-sync` flags and `CGR_AUTO_SYNC=1` across all query commands with
    bounded `SyncOptions::lock_timeout` graceful fallback so edits no longer require a separate sync turn.
  - Support multiple symbols/targets in a single mmap open on `cgr callers`, `cgr callees`,
    `cgr impact`, `cgr implementations`, and `cgr node`.
  - Expose reference resolution counts and `resolvedRate` under `refs` in `cgr status --json`,
    and remove duplicate `measure_changes` in `status` and redundant `Project::open` in `node`.
- **Calibrated dynamic-dispatch / ambiguity hints via on-disk `tail_idx` (`cg-store`, `cg-query`)**:
  - Add $O(\log R)$ `View::unresolved_refs_by_tail` and `View::unresolved_refs_from` with
    `UnresolvedScope` (`CallLike` vs `Dependency`) and order-independent top-K base+delta parity.
  - Surface concise unresolved / ambiguous / dynamic-receiver candidate sites on zero-result
    `callers`, `callees`, `impact`, and focused one-symbol `explore` responses without adding
    false-positive graph edges.

## 0.2.0 — 2026-11-22

### Vue/Svelte SFC support and deep Rust/Kotlin/Python inference (`ENGINE_VERSION = 14`)

- **Vue (`.vue`) and Svelte (`.svelte`) Single-File Component support** (zero new Tree-sitter C
  grammars; reuses existing TS/TSX/JS/JSX parsers):
  - `cg-extract/src/sfc.rs`: emit a `Component` node per SFC, fold `<script>`, `<script setup>`,
    and `<script module>` blocks with exact 1-based line and UTF-16 column preservation (skipping
    raw-text `<script>`/`<style>` bodies when masking HTML comments), extract Vue Options API
    `methods`/`computed`/`watch`/lifecycle hooks, template component tags (`PascalCase` and
    `kebab-case`), interpolations, directives, bare handlers, and Svelte `$store` reactive refs.
  - `cg-resolve`: add `@/` / `~/` / `$lib/` path aliases (after explicit `tsconfig.json` `paths`),
    Nuxt (`#imports`, `#components`, `#app`, `#build`, `#head`) and SvelteKit (`$app/*`, `$env/*`)
    virtual modules, Vue 3 compiler macros, Nuxt auto-imports, Svelte 5 runes (including
    `.svelte.ts` / `.svelte.js`), and Nuxt/SvelteKit file-based route hooks.
  - `cg-sync` & `cg-query`: invalidate Svelte `$store` consumers on store edits, and recognize
    `.vue` / `.svelte` in file-path explore queries, `cgr version --verbose`, and MCP instructions.
- **Deepened Rust / Kotlin / Python semantic inference**:
  - **Rust**: mask `pin_project!` / `pin_project_lite::pin_project!` wrappers alongside `cfg_*!`;
    infer tuple struct fields, `TypeParam` trait bounds and `TypeParam::method` (`U::initialize`),
    `array_type` elements, hoisted `static`/`const` bindings, `reference_expression`,
    `if let` conditions, `self` / `mut self: Pin<&mut Self>` receivers, `::pin` constructors,
    transparent `.clone()`/`.as_mut()`/`.as_ref()`/`.project()`/`.as_pin_mut()`/`.as_pin_ref()`,
    project-defined `Mutex`/`RwLock` guards and re-exported `Arc`, return-type-verified generic
    `Deref` wrappers, method-call tuple destructuring, sibling inline `mod` scoping (`use super::`
    and line-span disambiguation), and receiver-verified blanket extension traits.
  - **Kotlin / JVM**: prefer callables/types over same-named fields/properties in `direct_member`
    (builder property/method shadowing), preserve AST pre-order for chained same-name calls at one
    `(line, col)`, infer expression-body returns (`= apply { ... }`, `= also { ... }`, `= this`,
    `= object : T() {}`), getter-only properties, `as` casts, `object` literals, companion-object
    enclosing-class scope, chained field access on call results (`execute().body.string()`),
    scope-function lambdas (`.use`/`.let`/`.also`/`.apply`/`.run`/`with`), general Kotlin DSL
    receiver lambdas (`block: Receiver.() -> Unit`), companion/top-level extension imports with
    deterministic path/line/col tie-breaking, and receiver-type-narrowed extension resolution.
  - **Python**: infer default-parameter values and imported singleton aliases, enable return-type
    inference through transparent stdlib decorators (`@property`, `@cached_property`, `@classmethod`,
    `@staticmethod`, `@lru_cache`, `@cache`, `@override`, `@abstractmethod`, `@async_unsafe`) while
    preserving `self` subclass types, handle `return self`/`cls(...)`/typed locals/`with` blocks,
    resolve general decorator-injected `self.<attr>` fields, resolve subscript indexing (`__getitem__`),
    and model Django runtime proxies (`connection`, `cache`, `connections[...]`, `caches[...]`,
    `schema_editor()`, `cursor()`, and implicit `Model.objects` -> `QuerySet`).
- **Incremental sync parity (`cg-sync`)**:
  - Track file-scope `BindKind::Local` across Go/Rust/Python/Kotlin, Rust `BindKind::TypeParam`,
    and blanket `BindKind::TraitImpl` in `resolver_fact_sig`; invalidate consumers when exported
    inference providers swap imports, when wildcard imports or `qname` owner members change, and
    for Python subscript receiver expressions.

- **Benchmarks (`docs/benchmarks/2026-11-22-twelve-repo.json`)**:
  - Refresh the paired 12-repository benchmark in `README.md` / `README.zh-CN.md` against
    upstream v1.6.4 (`461a5d8`): 6.42× full-index speedup (2w vs 2w; 2.37 s total across all 12
    repos with auto-threads), 75.7%–88.0% lower peak RSS, 15.49× / 14.56× / 10.82× incremental
    sync speedups (11 of 12 single-file renames now surgical incremental), 55.73× warm `explore`
    P50 speedup, and 17.27× CLI `explore` speedup.

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
