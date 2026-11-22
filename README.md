# CodeGraph Rust (`cgr`)

**A local code knowledge graph for AI coding agents, built in Rust.**

[English](README.md) · [简体中文](README.zh-CN.md) · [Architecture](docs/ARCHITECTURE.md) · [Benchmarks](docs/BENCHMARKS.md) · [Changelog](CHANGELOG.md)

CodeGraph Rust indexes a repository into a persistent graph of symbols, references and
relationships. It gives developers and coding agents a way to retrieve relevant source,
follow calls across files, inspect dependencies and estimate the impact of a change through
one command-line executable or an MCP connection.

The engine runs locally and builds its graph from source syntax and deterministic analysis.
Indexing and querying require no hosted service, embedding API, model credentials, Node.js
runtime or database server. The normal index is stored in the project's `.cgr/` directory;
query results include source windows from the working tree.

This is an independent Rust re-implementation of the engine behind
[colbymchenry/codegraph](https://github.com/colbymchenry/codegraph). It reuses the upstream
Tree-sitter extraction kernel and adapts the agent-facing tool schemas and guidance, with
`codegraph-rust_` as the MCP tool prefix. Storage, resolution, incremental synchronization,
queries and service integration are implemented in this Rust workspace. Current coverage
differs from upstream; see [current limitations](#current-limitations).

## Contents

- [Capabilities](#capabilities)
- [Measured performance](#measured-performance)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Language and framework support](#language-and-framework-support)
- [Command-line reference](#command-line-reference)
- [Agent and MCP integration](#agent-and-mcp-integration)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Index lifecycle](#index-lifecycle)
- [Development and verification](#development-and-verification)
- [Current limitations](#current-limitations)
- [Contributing](#contributing)
- [Acknowledgements and license](#acknowledgements-and-license)

## Capabilities

- **Source-backed exploration.** `explore` combines symbol search, call paths, related source
  and change impact in one bounded response. Queries can contain natural-language questions,
  symbol names, qualified names or file names; specific names help identify a flow.
- **Cross-file code navigation.** Inspect callers, callees, implementations, imports and
  dependencies, including supported framework routes and heuristic dynamic-dispatch links.
- **Incremental indexing.** Re-extract changed files and re-resolve affected references.
  Larger changes and configuration changes can trigger a full rebuild.
- **Efficient persistent storage.** Immutable memory-mapped segments, dense integer IDs,
  string pools, bidirectional CSR edges, and prebuilt FST, trigram and BM25 search indexes.
- **CLI and MCP access.** Shared query handlers serve terminal workflows and agent clients.
  Built-in installers support Claude Code, Cursor and Codex.
- **Updates during agent sessions.** The MCP server can watch indexed projects, debounce
  file changes and publish new snapshots. Run `cgr sync` manually when no watcher is running.
- **Inspectable results.** Index health and checksums, unresolved references, incremental/full
  graph comparisons and optional SQLite exports are available from the CLI.

## Measured performance

Measured on **2026-11-22** with **`v0.2.0`** (engine version 14, built from `bf986bd` + `0.2.0` version bump) against upstream
[colbymchenry/codegraph](https://github.com/colbymchenry/codegraph) **v1.6.4** (**`461a5d8`**) with its native
`darwin-arm64` kernel loaded, on twelve repositories covering JavaScript, Go, Rust, Python, TypeScript,
Kotlin/Java, C, C++, C#, PHP, Ruby and Swift. Every row comes from the same machine and
configuration (`2` workers per side). Equal-weight geometric-mean speedups are **6.42×** for full indexing,
**15.49× / 14.56× / 10.82×** for incremental CLI sync (no changes / comment edit /
single-file rename), **55.73×** for warm in-process `explore` P50 and **17.27×** for a new-process CLI `explore`.
Median peak RSS during full indexing is **75.7%–88.0%** lower. Every cell in all twelve repositories
is faster than upstream, and **11 of 12** single-file renames re-resolve incrementally (only Zod's
`_parse` edit affects 86.7% of repository references and falls back to a full rebuild, still 2.98× faster).
With default auto-threads (`CGR_THREADS` unset on 10 cores), `cgr` indexes all twelve repositories combined in **2.37 s**.
Strict graph agreement with upstream meets the 97% precision / 95% recall gate in **4 of 12** repositories;
upstream is a reference rather than semantic ground truth, and across the larger repositories `cgr` now resolves
thousands of additional real call sites while maintaining **91.4%–100.0%** shared-site call precision (`CallP*`)
in 11 of 12 repositories. All samples, queries, sync reports and per-kind graph counts are in the
[structured evidence](docs/benchmarks/2026-11-22-twelve-repo.json)
(previous 2026-11-02 WSL2/Ryzen measurement archived in [2026-11-02-twelve-repo.json](docs/benchmarks/2026-11-02-twelve-repo.json)).

| Setting | Value |
|---|---|
| Machine | Apple M5 (10 cores), 32 GiB RAM, macOS 26.6 (Build 25G72), arm64, on AC power |
| `cgr` | `v0.2.0` (engine version 14, `bf986bd` source), `cargo build --release --locked -p cgr` (fat LTO, one codegen unit), rustc 1.99.1 |
| Upstream | v1.6.4 (`461a5d8`) production `dist/` with the `darwin-arm64` native kernel loaded; Node v26.13.1 launched with `CODEGRAPH_ALLOW_UNSAFE_NODE=1 node --liftoff-only --disable-warning=ExperimentalWarning` |
| Workers | `CGR_THREADS=2`, `CGR_MEMORY_MB=2048`; `CODEGRAPH_PARSE_WORKERS=2`, `CODEGRAPH_RESOLVE_WORKERS=2`; telemetry off (separate `cgr` auto-thread column recorded with `CGR_THREADS` unset) |
| Timing | one command at a time in alternating order, warm OS caches, `caffeinate -i`; wall time includes process start and exit, peak RSS comes from `/usr/bin/time -l` |
| Corpus | the pinned commits below; both sides index the exact same files with identical SHA-256 source digests (files only one side would index are excluded on both through `codegraph.json`) |

### Full indexing

One untimed warmup and three timed complete CLI rebuilds per side, in alternating order.
Upstream's database is initialized before warmup; its `index` command recreates the full database,
and `cgr index` rebuilds the complete snapshot. The `cgr (auto)` column records a separate
warmup + 3-run median with `CGR_THREADS` unset (default auto-threading on 10 cores).

| Repository | Language | Commit | Files | Upstream full index (2w) | `cgr` full index (2w) | Speedup (2w) | `cgr` (auto) | Peak RSS: upstream → `cgr` |
|---|---|---|---:|---:|---:|---:|---:|---:|
| Express | JavaScript | [`1c6f1b0`](https://github.com/expressjs/express/tree/1c6f1b0fe4000977b9a7b4f34097e4d527dd9824) | 142 | 667.8 ms | 105.6 ms | **6.32×** | 67.5 ms | 217.0 → 50.2 MiB |
| Gin | Go | [`0f09c3a`](https://github.com/gin-gonic/gin/tree/0f09c3a9b4626d9fe9979cf4d0521d6ae32ab646) | 99 | 678.7 ms | 91.3 ms | **7.43×** | 60.2 ms | 230.2 → 37.9 MiB |
| Tokio | Rust | [`b499257`](https://github.com/tokio-rs/tokio/tree/b499257ef62bd631b83a988d371f4330f545670c) | 814 | 3,123.7 ms | 532.7 ms | **5.86×** | 287.5 ms | 654.9 → 85.6 MiB |
| Django | Python | [`a05e16a`](https://github.com/django/django/tree/a05e16a7230fe33d3f43f4a3aaed103cebb42efc) | 2,984 | 12,065.7 ms | 1,617.3 ms | **7.46×** | 840.2 ms | 1,622.7 → 257.9 MiB |
| Zod | TypeScript | [`2554b70`](https://github.com/colinhacks/zod/tree/2554b70c3d4a80273381e61d527d820720626440) | 522 | 3,269.3 ms | 405.8 ms | **8.06×** | 229.5 ms | 619.7 → 105.5 MiB |
| OkHttp | Kotlin/Java | [`16fb6ae`](https://github.com/square/okhttp/tree/16fb6aef3aee1c4645fc8d24af01f8a4dc128ebf) | 690 | 3,272.1 ms | 565.4 ms | **5.79×** | 327.3 ms | 727.6 → 121.4 MiB |
| cJSON | C | [`6d9f244`](https://github.com/DaveGamble/cJSON/tree/6d9f2443ab071f86e5d9b43025a40929ec41c46c) | 68 | 515.1 ms | 63.0 ms | **8.18×** | 46.5 ms | 296.5 → 35.5 MiB |
| fmt | C++ | [`4ed3535`](https://github.com/fmtlib/fmt/tree/4ed3535092a96ac9f56fab80df83416d51bcc78e) | 80 | 2,333.3 ms | 315.5 ms | **7.40×** | 177.6 ms | 505.9 → 81.6 MiB |
| MediatR | C# | [`62010ba`](https://github.com/jbogard/MediatR/tree/62010baf12851770553f275ce4047e93f4471e79) | 159 | 554.8 ms | 94.3 ms | **5.88×** | 65.7 ms | 209.0 → 50.8 MiB |
| Slim | PHP | [`3675bf6`](https://github.com/slimphp/Slim/tree/3675bf6baac66b07032575b7bef4200b60b7974b) | 125 | 457.8 ms | 69.6 ms | **6.58×** | 51.0 ms | 179.0 → 36.2 MiB |
| Sinatra | Ruby | [`cb22afd`](https://github.com/sinatra/sinatra/tree/cb22afd7902b566b6eaba6c4ea89739494a65d12) | 147 | 452.0 ms | 92.3 ms | **4.90×** | 61.1 ms | 172.6 → 36.6 MiB |
| Alamofire | Swift | [`6c2892e`](https://github.com/Alamofire/Alamofire/tree/6c2892ea5adb5a03fa81c510933dd782c5ddd9cb) | 111 | 1,255.7 ms | 281.7 ms | **4.46×** | 160.5 ms | 415.8 → 76.7 MiB |

### Incremental CLI synchronization

Medians of three timed `sync` commands per category and side, upstream → `cgr` (speedup). Both
copies receive the same edit to one central source file: comment edits append a distinct
comment, and rename edits alternate a symbol and its references within that file. The original
bytes are restored and synced after each category. After restoration both sides return exactly
to their initial strict edge sets, and `cgr status --verify --strict` and `cgr debug diff`
(checked in both renamed and restored states) pass in all twelve repositories. These CLI runs
include the filesystem scan; they measure neither watcher latency nor repository-wide refactoring.

| Repository | No changes | Comment edit | Single-file symbol rename |
|---|---:|---:|---:|
| Express | 198.8 → 13.3 ms (**14.98×**) | 386.8 → 29.5 ms (**13.12×**) | 393.3 → 30.6 ms (**12.87×**) |
| Gin | 190.7 → 13.3 ms (**14.33×**) | 366.3 → 25.1 ms (**14.57×**) | 373.3 → 28.4 ms (**13.14×**) |
| Tokio | 252.8 → 14.9 ms (**16.96×**) | 551.4 → 41.5 ms (**13.30×**) | 566.7 → 55.3 ms (**10.25×**) |
| Django | 861.4 → 55.2 ms (**15.59×**) | 2,560.9 → 86.8 ms (**29.50×**) | 2,569.5 → 108.4 ms (**23.70×**) |
| Zod | 279.8 → 16.5 ms (**16.97×**) | 734.6 → 33.5 ms (**21.94×**) | 1,244.5 → 418.1 ms (**2.98×**)† |
| OkHttp | 333.4 → 19.4 ms (**17.15×**) | 724.7 → 56.6 ms (**12.80×**) | 815.5 → 131.7 ms (**6.19×**) |
| cJSON | 202.6 → 13.9 ms (**14.55×**) | 361.2 → 29.9 ms (**12.08×**) | 363.7 → 28.8 ms (**12.62×**) |
| fmt | 197.9 → 13.9 ms (**14.26×**) | 1,016.9 → 61.9 ms (**16.44×**) | 1,018.8 → 63.0 ms (**16.17×**) |
| MediatR | 207.3 → 14.7 ms (**14.14×**) | 331.3 → 25.6 ms (**12.95×**) | 341.8 → 28.2 ms (**12.12×**) |
| Slim | 193.4 → 14.0 ms (**13.81×**) | 266.0 → 20.3 ms (**13.07×**) | 271.4 → 23.9 ms (**11.38×**) |
| Sinatra | 201.2 → 13.2 ms (**15.26×**) | 355.0 → 39.9 ms (**8.90×**) | 351.9 → 39.3 ms (**8.96×**) |
| Alamofire | 287.7 → 15.4 ms (**18.65×**) | 514.7 → 35.6 ms (**14.47×**) | 520.6 → 38.5 ms (**13.53×**) |

† Only Zod's `_parse` rename rebuilt the full index (`58,840` of `67,845` references affected = 86.7%,
exceeding the incremental bailout threshold). All twelve comment edits re-resolved 1 file, and all
other eleven renames re-resolved incrementally (1–173 files: e.g. Express 4, Gin 9, Tokio 106,
Django 24, OkHttp 173, cJSON 4, fmt 2, MediatR 21, Slim 22, Sinatra 1, Alamofire 3).

<details>
<summary>Edited file and renamed symbol per repository</summary>

| Repository | File | Symbol |
|---|---|---|
| Express | `lib/application.js` | `handle` |
| Gin | `gin.go` | `Default` |
| Tokio | `tokio/src/runtime/builder.rs` | `new_current_thread` |
| Django | `django/core/handlers/base.py` | `get_response` |
| Zod | `packages/zod/src/v4/core/parse.ts` | `_parse` |
| OkHttp | `okhttp/src/commonJvmAndroid/kotlin/okhttp3/OkHttpClient.kt` | `newCall` |
| cJSON | `cJSON.c` | `cJSON_ParseWithOpts` |
| fmt | `include/fmt/format.h` | `format_decimal` |
| MediatR | `src/MediatR/Mediator.cs` | `Send` |
| Slim | `Slim/App.php` | `handle` |
| Sinatra | `lib/sinatra/base.rb` | `invoke` |
| Alamofire | `Source/Core/Session.swift` | `perform` |

</details>

### Query latency

Warm latency calls the production in-process explore handlers (`cgr bench explore`; upstream
`ToolHandler.executeReadTool("codegraph_explore")`) with the same 20 fixed queries on both
sides: eight warm-up queries, then 20 queries × 3 passes, giving 60 observations with
nearest-rank percentiles. MCP transport and queueing are excluded. The new-process column runs
the CLI `explore` for the first query three times per side in alternating order, including startup.
Returned content differs, and the sizes are not directly comparable: across the twelve repositories
upstream answers average 12,808 UTF-16 code units and `cgr` answers average 11,424 UTF-8 bytes.

| Repository | Warm P50: upstream → `cgr` | P50 speedup | Warm P95: upstream → `cgr` | New-process CLI median |
|---|---:|---:|---:|---:|
| Express | 5.58 → 0.08 ms | **72.94×** | 11.88 → 0.16 ms | 108.3 → 6.3 ms (**17.15×**) |
| Gin | 9.92 → 0.15 ms | **65.36×** | 14.51 → 0.25 ms | 115.7 → 6.1 ms (**19.09×**) |
| Tokio | 20.91 → 0.43 ms | **49.05×** | 38.20 → 0.75 ms | 115.1 → 6.4 ms (**18.07×**) |
| Django | 36.59 → 0.60 ms | **61.29×** | 51.51 → 1.28 ms | 113.2 → 6.2 ms (**18.17×**) |
| Zod | 22.62 → 0.44 ms | **51.06×** | 30.60 → 1.06 ms | 122.8 → 6.6 ms (**18.57×**) |
| OkHttp | 17.44 → 0.40 ms | **43.69×** | 31.39 → 0.78 ms | 111.9 → 7.2 ms (**15.55×**) |
| cJSON | 9.22 → 0.18 ms | **51.56×** | 11.70 → 0.24 ms | 113.9 → 6.7 ms (**16.97×**) |
| fmt | 15.29 → 0.30 ms | **50.74×** | 21.66 → 0.56 ms | 117.1 → 7.1 ms (**16.41×**) |
| MediatR | 9.72 → 0.18 ms | **54.37×** | 12.57 → 0.37 ms | 114.7 → 6.9 ms (**16.64×**) |
| Slim | 9.79 → 0.16 ms | **61.95×** | 13.14 → 0.30 ms | 116.7 → 6.8 ms (**17.04×**) |
| Sinatra | 8.51 → 0.10 ms | **82.62×** | 10.78 → 0.16 ms | 108.5 → 6.6 ms (**16.49×**) |
| Alamofire | 12.35 → 0.32 ms | **38.53×** | 17.14 → 0.49 ms | 113.7 → 6.5 ms (**17.38×**) |

### Graph agreement with upstream

Edges are compared by `(source natural key, target natural key, kind, line, UTF-16 column)` where
each node key is `(file_path, kind, name, start_line, start_col)`, with NULL distinct from zero;
precision = shared / `cgr` edges and recall = shared / upstream edges.
`CallP*` measures target agreement restricted to call sites resolved by both engines, and
`Site-match` measures the share of upstream call sites where `cgr` also resolves a call.
All edge kinds, route nodes and files with syntax errors stay in the denominators.

| Repository | Language | All P / R | Call P / R | CallP\* | Call site-match | Edges: `cgr` / upstream / shared | 97% / 95% gate |
|---|---|---:|---:|---:|---:|---:|---|
| Express | JavaScript | 65.13% / 92.23% | 57.16% / 87.40% | **100.00%** | 87.40% | 3,315 / 2,341 / 2,159 | fail |
| Gin | Go | 93.37% / 97.35% | 99.13% / 95.85% | **99.70%** | **97.46%** | 8,417 / 8,073 / 7,859 | fail |
| Tokio | Rust | 79.74% / 93.90% | 80.57% / **87.86%** | **92.86%** | **91.46%** | 49,603 / 42,124 / 39,554 | fail |
| Django | Python | 87.45% / 95.37% | 71.96% / **88.62%** | **97.45%** | **90.77%** | 211,778 / 194,186 / 185,192 | fail |
| Zod | TypeScript | 91.38% / 97.25% | 87.59% / **97.98%** | **91.39%** | **98.03%** | 38,643 / 36,310 / 35,312 | fail |
| OkHttp | Kotlin/Java | 79.24% / 88.42% | 65.90% / **77.95%** | 83.38% | **90.82%** | 59,154 / 53,018 / 46,876 | fail |
| cJSON | C | 99.68% / 99.44% | 99.82% / **99.47%** | **99.88%** | **99.59%** | 3,757 / 3,766 / 3,745 | **pass** |
| fmt | C++ | 94.01% / 91.69% | 90.22% / 85.31% | **92.30%** | **94.05%** | 20,654 / 21,177 / 19,417 | fail |
| MediatR | C# | 97.03% / 97.18% | 91.96% / **91.07%** | **92.57%** | **94.07%** | 5,753 / 5,744 / 5,582 | **pass** |
| Slim | PHP | 98.21% / 95.41% | 92.99% / 83.39% | **93.47%** | 88.74% | 4,529 / 4,662 / 4,448 | **pass** |
| Sinatra | Ruby | 98.58% / 96.41% | 98.09% / **89.84%** | **99.47%** | **90.32%** | 2,612 / 2,671 / 2,575 | **pass** |
| Alamofire | Swift | 95.67% / 93.37% | 89.86% / 85.65% | **94.09%** | 89.95% | 12,184 / 12,485 / 11,657 | fail |

Upstream is a comparison oracle rather than semantic ground truth ([method notes](docs/BENCHMARKS.md)),
so raw precision drops whenever `cgr` resolves valid calls that upstream leaves unresolved or fixes
an upstream mis-guess: on Django `cgr` resolves 17.6k more edges than upstream (`211,778` vs `194,186`)
mostly from typed `QuerySet`/`BaseCache`/`RequestFactory`/`Template` receivers while maintaining **97.45% `CallP*`**;
on Tokio `cgr` extracts `cfg_*!` and `pin_project!` items and resolves `7.5k` more edges with **92.86% `CallP*`**;
on OkHttp `cgr` reaches **90.82%** call-site match and resolves real builder/extension targets (`ResponseBody::string`,
`Call::execute`, `OkHttpClient::Builder::sslSocketFactory`) where upstream guesses test/build methods;
and on Express **100.00% `CallP*`** confirms every shared call site agrees while extra edges come from
file-level `app.use`/`app.set`/`app.listen` calls and route containment. Details:
[second-phase languages](docs/LANGUAGES-P5.md) and the
[JavaScript/TypeScript diagnostics](docs/GRAPH-DIAGNOSTICS-2026-11-01.md).

### Performance engineering update

Main includes event-path sync, source-free snapshot compaction after a quiet period, extraction
buffer spill, preallocated resolution outcomes, fused hook scanning, selective/parallel event
synthesis, compressed BM25 postings, adaptive radix sorting and bounded source windows. MCP has
bounded admission, cooperative cancellation and project/watcher cleanup.

These changes use **snapshot format 2**; run `cgr index /path/to/project` to rebuild an old index.
The new full-index retest is revision-pinned above. The earlier **6.40×** result used different
worker settings and an older upstream revision; it is [archived separately](docs/BENCHMARKS-HISTORICAL.md),
and is not a before/after estimate for this optimization. Implementation boundaries are recorded
in the bilingual [engineering record](docs/PERFORMANCE-IMPLEMENTATION.md).

## Installation

### Build and install from source

Building from source requires **Rust 1.89 or newer**,
Cargo, Git and a C compiler toolchain for the Tree-sitter grammars.

```sh
git clone https://github.com/NianJiuZst/CodeGraph-Rust.git
cd CodeGraph-Rust
cargo install --path crates/cgr --locked
cgr version --verbose
```

Cargo installs `cgr` into its binary directory, normally `~/.cargo/bin`. Make sure that directory
is on your `PATH`. Rust and the C compiler are build-time requirements; the resulting executable
runs without a Rust toolchain or Node.js.

For a local build without installing:

```sh
cargo build --release --locked -p cgr
./target/release/cgr --help
```

On Windows, the executable is `target\release\cgr.exe`.

### Release installers

Download scripts verify SHA-256 checksums for the archives on the
[Releases page](https://github.com/NianJiuZst/CodeGraph-Rust/releases).

```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.sh | sh
```

```powershell
# Windows, PowerShell 7+
irm https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.ps1 | iex
```

Release automation defines these targets:

| Platform | Architectures | Packaging |
|---|---|---|
| Linux | x86_64, ARM64 | Static musl executable, `.tar.gz` |
| macOS | Intel x86_64, Apple Silicon ARM64 | Executable, `.tar.gz` |
| Windows | x86_64, ARM64 | MSVC executable, `.zip` |

The scripts accept `CGR_VERSION` to select a version and `CGR_INSTALL_DIR` to choose the install
location. Defaults are `~/.local/bin` on Linux/macOS and `%LOCALAPPDATA%\Programs\cgr` on Windows.

## Quick start

Run these commands in the repository you want to analyze:

```sh
cd /path/to/your-project
cgr init
cgr status --verify
```

Explore an area using names from that project's source:

```sh
cgr explore "AuthService loginUser SessionStore"
cgr query loginUser --json
cgr callers loginUser
cgr callees loginUser
cgr impact loginUser --depth 2
```

`AuthService`, `loginUser` and `SessionStore` are illustrative names; replace them with your
own symbols. You can also ask a question such as `cgr explore "how does login reach the database"`.
The response uses the indexed graph and relevant source, with line numbers and an output budget.

Register the MCP server with supported agents:

```sh
cgr install --dry-run
cgr install
```

The installer connects the agent; `cgr init` builds each project's index. When you use only
CLI commands, update the graph after edits with `cgr sync`. While an MCP server is running
with watching enabled, it performs that synchronization automatically after a debounce period.

## Language and framework support

### Source languages

| Language | Recognized supported extensions |
|---|---|
| TypeScript / TSX | `.ts`, `.mts`, `.cts`, `.tsx` |
| JavaScript / JSX | `.js`, `.mjs`, `.cjs`, `.jsx`, `.xsjs`, `.xsjslib` |
| Vue SFC | `.vue` |
| Svelte SFC | `.svelte` |
| Python | `.py`, `.pyw` |
| Go | `.go` |
| Java | `.java` |
| Rust | `.rs` |
| Kotlin | `.kt`, `.kts` |
| C | `.c`, `.h` (a `.h` with C++ constructs is parsed as C++) |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx`, `.metal`, `.cu`, `.cuh` |
| C# | `.cs` |
| PHP | `.php`, `.module`, `.install`, `.theme`, `.inc` |
| Ruby | `.rb`, `.rake` |
| Swift | `.swift` |

Extraction records source structure, symbols and references. Resolution uses available scope,
import, receiver-type, module and package information. Coverage depends on language constructs
and project configuration; this is a static code graph rather than a full compiler type checker.

TypeScript/TSX factory calls propagate explicit named return types from function declarations,
variable-initialized arrow/function expressions and methods, using the factory's enclosing
declarations and imports. Types declared inside the factory body do not shadow its return
annotation. Union/intersection types, generic substitution, arrays and async/generator wrappers
remain unresolved; an unsupported annotation does not justify guessing a same-named method.
Block-level type shadowing is not modeled. These return rules use engine version 10, so syncing
an older index refreshes its facts and call edges.

### Framework and dynamic-dispatch rules

Route hooks are implemented for **Express, Flask, FastAPI, Django, Gin, Spring, Axum, Next.js,
Nuxt, SvelteKit, NestJS, React Router and TanStack Router**.
Framework detection uses project manifests, so route coverage can differ when dependencies are
absent or when indexing a framework's own repository. The benchmark documents observed gaps.

Supported synthesis passes include interface/implementation dispatch, Go structural interfaces,
event-emitter handlers, React re-render links and JSX parent/child links. These are bounded
heuristics with recorded provenance; they cover specific patterns rather than arbitrary
runtime behavior.

For NestJS, literal `RouterModule.register` paths and a literal `setGlobalPrefix` on a
recognizable `NestFactory.create` application are composed for HTTP-client matching, including
statically readable exclusions. Route node names remain controller-local. For React Router,
exported route-object arrays can be consumed across imports and re-exports, including literal
parent routes and `children` arrays. Candidate route nodes are retained for exported arrays;
only arrays reached from a recognized router call participate in navigation matching. Dynamic
route configuration remains outside this cross-file rule. Go interface dispatch includes unambiguous promoted methods;
method-set inference remains a bounded static approximation.

Engine rule version 7 adds these resolution rules and synthesis passes, all bounded static rules
that link nothing when a name is ambiguous:

- **Go**: embedded standard-library types take part in interface satisfaction (the `io.Reader`,
  `*os.File`, `error`, ... method table is ported from upstream) and other outside embeds are
  opaque; a project embedding the kernel writes without its package binds through the written
  qualifier; func-field dispatch (`d.refreshf()` reaches the functions put in the field, through one
  field-to-field copy); protoc `Unimplemented*Server` stubs reach hand-written servers; Gin's
  `Context.Next` reaches registered middleware.
- **TS/JS**: calls through `const local = other` and `export const alias = realImpl` reach the
  real function, as do object-literal members (`api.run()`); conditional / `??` / `||` arms of
  module-level values (`const-alias`); ES5 constructors with `prototype` methods and
  `util.inherits` / `Object.create` inheritance; ambient modules declared with `declare module 'x'`
  in `.d.ts` files; Redux thunk `dispatch` chains and RTK Query generated hooks.
- **Rust**: generic parameters resolve through their trait bounds (an unbounded parameter no longer
  matches a project type of the same name); `impl Deref` targets, `type X = Y` aliases and
  `Mutex` / `RwLock` lock guards are looked through; `self.field` call chains are evaluated.
- **Python**: factories returning different classes in different branches are typed by the nearest
  common base; `self.x = self.make()` types a field across methods; pytest fixture parameters (same
  file or a `conftest.py` above) take the fixture's return type.
- **Cross-language / JVM**: Spring test requests (MockMvc, WebTestClient, TestRestTemplate,
  RestAssured) reach routes; Kotlin Multiplatform `expect` reaches each platform's `actual`; a Tauri
  frontend's `invoke('cmd')` reaches the registered `#[tauri::command]`; React Native
  `NativeModules.X.m()` reaches the Java / Kotlin module's `@ReactMethod`.

Engine rule version 14 adds Vue/Svelte SFC extraction and deepens static inference for Rust,
Kotlin and Python:

- **Vue / Svelte SFC**: `.vue` and `.svelte` files extract `<script>`/`<script setup>`/`<script module>`
  blocks with exact source line/column mapping (reusing the existing TS/TSX/JS/JSX parsers without
  new C grammars), Vue Options API members, template component tags and call expressions, Svelte
  `$store` reactive references, `@/` / `~/` / `$lib/` aliases (honoring `tsconfig.json` `paths` first),
  Vue 3 macros, Nuxt auto-imports and virtual modules (`#imports`, `#app`, ...), Svelte 5 runes
  (including `.svelte.ts`/`.svelte.js`), and Nuxt/SvelteKit file-based routes.
- **Rust**: `pin_project!` wrapper masking, tuple struct fields, `TypeParam::method` and call-site
  generic bound substitution, project-defined `Mutex`/`RwLock` guards, return-type-verified generic
  `Deref` wrappers, transparent `.project()`/`.as_mut()`/`.as_ref()`/`.clone()`, method-call tuple
  destructuring, sibling inline `mod` scoping, and receiver-verified blanket extension traits.
- **Kotlin / JVM**: callable-over-field disambiguation for builder APIs, pre-order chained call
  resolution, expression-body returns (`= apply { ... }`, `= this`, `= object : T() {}`), getter
  properties, chained field access on call results (`execute().body.string()`), scope-function
  lambdas (`.use`/`.let`/`.also`/`.apply`/`.run`/`with`), DSL receiver lambdas
  (`Receiver.() -> Unit`), and receiver-narrowed extension imports.
- **Python**: default-parameter values and imported singleton aliases, transparent stdlib decorator
  return inference (`@property`, `@cached_property`, `@classmethod`, `@lru_cache`, ...), general
  decorator-injected `self.<attr>` fields, subscript indexing (`__getitem__`), and Django runtime
  proxies (`connection`, `cache`, `connections[...]`, `caches[...]`, `schema_editor()`, `cursor()`,
  and implicit `Model.objects` -> `QuerySet`).

Not covered: MyBatis XML, Angular templates, Pinia/Vuex store internals, Vue Router / dynamic
programmatic Nuxt/SvelteKit route tables, React Native events and the Fabric / Expo native side,
Tauri events, GoFrame, and general call-site-driven Python parameter-type flow (upstream implements
it for Ruby only).

C/C++, C#, PHP, Ruby and Swift use the upstream extraction walkers with this project's own
resolution rules (includes, namespaces, `require`, SwiftPM modules, implicit `this` members).
Against upstream v1.6.4 (`461a5d8`) on strict edge keys, cJSON, MediatR, Slim and Sinatra meet the
97% / 95% gate; fmt (94.0% / 91.7%) and Alamofire (95.7% / 93.4%) do not; much of the gap is
call sites where the two pick different overloads or scopes (see [docs/LANGUAGES-P5.md](docs/LANGUAGES-P5.md)). These
languages have no framework route rules yet. Dart, Scala, Lua, Luau and R are detected but not extracted.

## Command-line reference

Use `cgr --help` or `cgr <command> --help` for the full option list. `--path` / `-p` selects a
project explicitly, for example `cgr --path /path/to/project explore "UserService save"`.
Query commands can locate an index in the current directory or an ancestor.

| Command | Purpose |
|---|---|
| `cgr init [path]` | Create `.cgr/` and build a full index |
| `cgr index [path] --timings` | Rebuild the full graph and report phase timings |
| `cgr sync [path]` | Reconcile additions, edits and deletions with the index |
| `cgr status --json --verify` | Report counts, changes and snapshot checksum verification |
| `cgr status --strict` | Return a nonzero status when the index has pending changes |
| `cgr explore <query…>` | Retrieve related source, call paths and impact |
| `cgr query <text> --kind function --json` | Search symbols, optionally filtering by kind |
| `cgr node <symbol-or-file> --code` | Inspect a symbol or file and its source |
| `cgr files [--tests\|--no-tests] [--format package]` | List indexed files by tree, language, or package |
| `cgr callers <symbol>` / `cgr callees <symbol>` | Follow incoming or outgoing calls |
| `cgr impact <symbol> --depth 2` | Inspect the graph neighborhood affected by a change |
| `cgr implementations <symbol> --depth 10` | Find implementations or subtypes |
| `cgr affected <files…> [--cross-tier] --json` | Find test files connected to changed files |
| `cgr cycles --json` | Report import cycles; exit 1 when cycles are found |
| `cgr export --sqlite graph.sqlite` | Export a queryable SQLite graph |
| `cgr serve --mcp` / `cgr serve --http ADDR` | Start an MCP service |
| `cgr install` / `cgr uninstall` | Add or remove agent integration |
| `cgr uninit [path] --yes` | Remove the project's index |
| `cgr version --verbose` | Show build, format and kernel information |

Normal success returns 0; operational failures and conditions such as detected cycles return 1;
invalid command-line usage returns 2. Source windows are read from the current working tree,
so keeping the graph synchronized matters after edits.

## Agent and MCP integration

### Automatic configuration

`cgr install` detects supported agents and edits their existing configuration while preserving
comments and unrelated entries. Existing user-pinned commands and settings are retained;
missing integration keys are added. `uninstall` removes the owned registration and marked
instruction blocks.

```sh
cgr install --agent claude-code
cgr install --agent cursor
cgr install --agent codex
cgr install --local --agent claude-code
cgr install --no-hook
cgr uninstall --agent cursor
```

| Agent | Global registration | Project-local behavior |
|---|---|---|
| Claude Code | MCP registration, instructions, optional prompt hook and explore permission | `.mcp.json`, `CLAUDE.md`, `.claude/settings.json` |
| Cursor | `~/.cursor/mcp.json`, with workspace path passed to the server | `.cursor/mcp.json` with the project path |
| Codex | MCP registration in `~/.codex/config.toml` and instructions | `--local` edits project `AGENTS.md`; register the server globally separately |

Claude Code honors `CLAUDE_CONFIG_DIR`; Codex honors `CODEX_HOME`. Use `--dry-run` to inspect
changes and `--bin` to specify a different executable path. `--no-hook` disables registration
of the optional Claude Code prompt hook; `--no-permissions` skips the explore permission entry.

### Manual stdio configuration

For an MCP client with an `mcpServers` JSON configuration, use absolute paths appropriate to
that machine:

```json
{
  "mcpServers": {
    "codegraph-rust": {
      "command": "/absolute/path/to/cgr",
      "args": ["serve", "--mcp", "--path", "/absolute/path/to/project"]
    }
  }
}
```

Initialize that project with `cgr init` first. Clients can supply MCP roots; tools also accept
`projectPath` for selecting another indexed project. Use `--path` for clients that do not
provide a project root. Add `--no-watch` when the server should serve snapshots without
watching source changes.

By default, the tool list exposes **`codegraph-rust_explore`**. Set `CGR_MCP_TOOLS` in the server's
environment to enable additional tools:

```sh
CGR_MCP_TOOLS=explore,node,callers,callees,impact,files,search,status cgr serve --mcp
```

| Tool | Purpose |
|---|---|
| `codegraph-rust_explore` | Relevant source, named-symbol flows and impact |
| `codegraph-rust_node` | Symbol or file details and source |
| `codegraph-rust_callers` / `codegraph-rust_callees` | Call relationships |
| `codegraph-rust_impact` | Change impact neighborhood |
| `codegraph-rust_files` | Indexed file tree |
| `codegraph-rust_search` | Symbol locations |
| `codegraph-rust_status` | Index health and pending changes |

### HTTP transport

HTTP-capable clients can connect to `/mcp`:

```sh
export CGR_HTTP_TOKEN="replace-with-your-token"
cgr serve --http 127.0.0.1:7878 --path /absolute/path/to/project
```

This listens at `http://127.0.0.1:7878/mcp` and requires `Authorization: Bearer <token>` when a
token is configured. The token can also be supplied with `--token`. Keep the binding and token
appropriate for the clients that should access the project's source.

## Configuration

### Project settings

Place `cgr.json` at the project root. The upstream filename `codegraph.json` is also read;
`cgr.json` takes precedence for fields it supplies.

```json
{
  "exclude": ["**/generated/**", "**/fixtures/**"],
  "include": ["packages/shared/src/**"],
  "deprioritize": ["third_party/**"],
  "includeIgnored": false,
  "extensions": {
    ".customts": "typescript"
  }
}
```

| Field | Meaning |
|---|---|
| `exclude` | Gitignore-style patterns excluded from indexing, including tracked files |
| `include` | Re-include user-ignored paths; this is not a source-file whitelist |
| `deprioritize` | Keep matching paths indexed but rank them below first-party code |
| `includeIgnored` | Include embedded Git repositories during discovery |
| `extensions` | Map extra extensions to existing language parsers |
| `webRoot` | Explicit document root for PHP `$_SERVER['DOCUMENT_ROOT']` includes (auto-inferred when omitted) |

Explicit `exclude` rules take precedence over `include`; built-in directory skips still apply
when re-including paths. Invalid configuration produces warnings. Extension mappings select
an existing parser and do not add support for a new language.

Discovery applies ignore rules and skips common dependency/build directories. Source files
larger than 1 MiB are excluded from extraction. Generated files and test paths are recorded as
flags for downstream ranking and analysis.

### Environment variables

| Variable | Behavior |
|---|---|
| `CGR_THREADS` | Indexing worker count; the detected memory budget can reduce it |
| `CGR_MEMORY_MB` | Tuning budget for workers, batches, kernel spill and resolver caches; not an RSS cap |
| `CGR_MCP_TOOLS` | Comma-separated list of exposed MCP tools; `CODEGRAPH_MCP_TOOLS` is a fallback |
| `CGR_EXPLORE_DEDUP=1` | Skip source already returned in the same durable MCP conversation |
| `CGR_NO_PROMPT_HOOK=1` | Disable the optional Claude Code prompt hook at runtime |
| `CGR_HTTP_TOKEN` | Require a bearer token on HTTP requests |
| `CGR_LOG` | Service log filter, default `warn`; logs go to stderr |
| `CGR_TIMINGS` | Enable CLI indexing phase timing output |
| `CGR_FSYNC` | Default platform syncing; `full` selects strongest syncing, `off` skips syncing |

Defaults derive indexing resources from the host's available memory and CPU parallelism.
For example, `CGR_THREADS=4 CGR_MEMORY_MB=2048 cgr index --timings` requests a constrained
indexing configuration. Actual memory use also depends on graph size and intermediate data.

## Architecture

```text
source files → scan → parallel extraction + facts → merge → resolve + synthesize
                                                                    ↓
CLI / MCP ← query handlers ← base + delta view ← mmap segments ← publish snapshot
```

The snapshot format stores hot/cold node records, string pools, bidirectional CSR edges,
reference outcomes and search indexes. Publishing writes new immutable files and switches
the manifest; readers retain a stable snapshot, with shared locks protecting files from GC.

Incremental updates compare timestamps/sizes and content hashes, extract changed files, then
re-resolve references affected by changed definitions, imports, exports or hierarchy. One delta
is computed against the base; deltas do not accumulate as an unbounded stack. Quiet-period
compaction rewrites the live snapshot without re-extraction; broad reference changes or config
changes can still rebuild the graph. The correctness target is that the
incremental view equals a fresh full build, checked by canonical graph comparisons.

| Crate | Responsibility |
|---|---|
| `cg-core` | IDs, node/edge kinds, configuration, resource tuning and shared errors |
| `cg-kernel` | Vendored upstream extraction kernel and adaptation patches |
| `cg-scan` | File discovery, language detection and ignore rules |
| `cg-extract` | Parallel extraction, facts, error isolation and hooks |
| `cg-store` | Segment format, indexes, snapshots, publishing and garbage collection |
| `cg-resolve` | Module/export tables, reference resolution, route hooks and synthesis |
| `cg-query` | Retrieval, traversal, source windows and shared tool handlers |
| `cg-sync` | Change detection, incremental updates and watcher coordination |
| `cg-mcp` | JSON-RPC, stdio and HTTP service transports |
| `cg-install` | Agent configuration and instruction-block editing |
| `cgr` | CLI, prompt hook, SQLite export and benchmark commands |

See [Architecture](docs/ARCHITECTURE.md) for storage and algorithm details.

## Index lifecycle

- `cgr init` creates `.cgr/` and indexes the project. Its internal `.gitignore` keeps index
  contents out of version control. `init --local-only` additionally uses `.git/info/exclude`
  when available.
- CLI queries open existing snapshots directly. The MCP process stays alive for its client
  and can watch source changes; it does not require a separate database service.
- `cgr sync` updates the graph after edits. `cgr index` performs a full rebuild, including after
  changes that require refreshed engine analysis. `status` reports index version and changes.
- `cgr export --sqlite graph.sqlite` writes a separate SQLite file for analysis; it does not
  replace the native snapshot backend. Add `--force` to replace an existing export.
- `cgr uninstall` removes agent integration. `cgr uninit --yes` separately removes the project's
  index. The source tree is retained.

The index and exported graphs contain code-derived information. Keep them with the access and
retention policy appropriate to the repository.

## Development and verification

```sh
cargo test --workspace --locked
cargo clippy --workspace --exclude cg-kernel --all-targets -- -D warnings
cargo build --release --locked -p cgr
```

For faster optimized development builds, use `cargo build --profile fastrel -p cgr`. The
release profile uses fat LTO; `fastrel` reduces link work and is a separate performance profile.

Focused correctness and measurement commands include:

```sh
cgr status --verify --strict
cgr debug unresolved
cgr debug diff
cgr bench index /path/to/project --runs 3 --json
cgr bench sync /path/to/project --edits 30 --seed 1 --verify --json
cgr bench explore /path/to/project --count 100 --runs 3 --json
scripts/bench-local.sh --help
```

`bench sync` edits a temporary copy by default; `--in-place` changes that behavior. `debug diff`
compares the current graph with a fresh full build. The benchmark wrapper adds memory, disk
and swap preflight checks and process-tree monitoring; its prerequisites may require closing
other applications before a run.

The `v0.2.0` (`bf986bd`) twelve-repository retest passed restoration, `cgr status --verify --strict`
and `cgr debug diff` (in both renamed and restored states) across all twelve repositories; see
[Measured performance](#measured-performance) and the
[structured evidence](docs/benchmarks/2026-11-22-twelve-repo.json).
Local measurements and hosted CI results are separate evidence.

Code revision `bf986bd` passed all [hosted CI checks](https://github.com/NianJiuZst/CodeGraph-Rust/actions/runs/45000315060),
including Linux/macOS/Windows tests and end-to-end smoke, rustfmt/Clippy, kernel reproducibility
against upstream v1.6.4 (`461a5d8`) and the static Linux build. Hosted correctness results are not
used as performance measurements.

The CI configuration includes formatting, Clippy, kernel reproducibility, multi-platform tests
and release-build checks. The smoke helper covers the CLI, MCP handshake and prompt hook.
To refresh the kernel, use `scripts/sync-kernel.sh <upstream-checkout> <commit>` and verify the
resulting patches and extraction tests. Vendored formatting is kept separate from workspace
formatting checks.

## Current limitations

- **Coverage parity:** supported language families, framework routes, synthesized edges and
  explore responses differ from upstream. The benchmark has a per-repository coverage table.
- **Static-analysis limits:** ambiguity can leave references unresolved; dynamic behavior is
  covered only by implemented patterns. Inspect important call paths before relying on them.
- **Memory tuning:** kernel-buffer spill and resolver cache limits release heap memory, but
  facts, graph builders and shared tables still scale with repository size. The tuning budget
  does not enforce a process RSS cap.
- **Incremental scope:** filesystem event probing is local, but inventory assembly and the
  base-relative delta still do broader in-memory work. Structural changes can trigger wider
  resolution/synthesis; background compaction with delta rebasing remains M6 work.
- **Execution deadlines:** query/hook cancellation is cooperative; a blocking filesystem call
  cannot be forcibly interrupted by the worker deadline.
- **Performance scope:** current timings cover twelve restricted-tier repositories. Standard-tier
  workloads, concurrent-reader latency, large-repository RSS and agent-task outcomes remain
  outside this measurement.

## Contributing

Start with [Architecture](docs/ARCHITECTURE.md) and the owning crate. Keep graph changes focused,
include a regression case for changed resolution or synchronization behavior, and compare
incremental output with a full rebuild. Document supported constructs and observed coverage
when extending a language or framework rule. Run the relevant tests and formatting/Clippy
checks before submitting changes; kernel updates should remain reproducible from their pinned
upstream commit and patches.

## Acknowledgements and license

CodeGraph Rust builds on [Colby Mchenry's CodeGraph](https://github.com/colbymchenry/codegraph)
and its Tree-sitter extraction work. The vendored upstream kernel is MIT-licensed; its pinned
revision is recorded in `crates/cg-kernel/UPSTREAM_COMMIT`.

This project is distributed under the [MIT License](LICENSE). See
[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) for upstream attribution and vendored components.
