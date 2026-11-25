//! `cgr`: CLI entry point (design doc 11.1).

mod bench;
mod cmds;
#[cfg(feature = "sqlite")]
mod export;
mod hook;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "cgr", version, about = "codegraph-rust: fast code graph for AI agents")]
struct Cli {
    /// Project directory (default: the current directory; the nearest `.cgr/` above it is used)
    #[arg(short = 'p', long = "path", global = true)]
    project: Option<PathBuf>,
    /// Incrementally sync index changes before querying (also enabled by CGR_AUTO_SYNC=1)
    #[arg(long = "sync", global = true, conflicts_with = "no_sync")]
    auto_sync: bool,
    /// Skip automatic sync even when CGR_AUTO_SYNC=1 is set
    #[arg(long = "no-sync", global = true)]
    no_sync: bool,
    /// Session ID for cross-call session deduplication (also read from CGR_SESSION)
    #[arg(long = "session", global = true, value_name = "ID")]
    session: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create `.cgr/` and build the index
    Init {
        path: Option<PathBuf>,
        /// Keep `.cgr/` out of git through `.git/info/exclude` instead of its own `.gitignore`
        #[arg(long)]
        local_only: bool,
    },
    /// Build the index from scratch
    Index {
        path: Option<PathBuf>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        quiet: bool,
        /// Print per-phase timings and peak memory
        #[arg(long)]
        timings: bool,
        /// Print a machine-readable report (used by `bench index`)
        #[arg(long, hide = true)]
        report_json: bool,
    },
    /// Incrementally update the index from the files that changed
    Sync {
        path: Option<PathBuf>,
        #[arg(long)]
        quiet: bool,
    },
    /// Show index statistics
    Status {
        path: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Exit 1 when files changed since the last sync
        #[arg(long)]
        strict: bool,
        /// Verify every partition checksum
        #[arg(long)]
        verify: bool,
    },
    /// Search for symbols
    Query {
        search: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        max_chars: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Explore: source, call paths and blast radius for a question or a bag of names
    Explore {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long)]
        max_files: Option<usize>,
        /// Hard character budget cap (1000–50000)
        #[arg(long)]
        max_chars: Option<usize>,
        /// Return flow, blast radius, and symbol outlines without full source bodies
        #[arg(long)]
        compact: bool,
        /// Ignore session deduplication history for this call
        #[arg(long)]
        no_dedup: bool,
    },
    /// Read one or more symbols (with trail) or files
    Node {
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
        /// Include the symbol's source
        #[arg(long)]
        code: bool,
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        offset: Option<usize>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        symbols_only: bool,
        /// Omit verbatim source bodies (signature + trail in symbol mode, symbol map in file mode)
        #[arg(long)]
        compact: bool,
        /// Hard character budget cap (500–50000 symbol mode; 1000–100000 file mode)
        #[arg(long)]
        max_chars: Option<usize>,
        /// Ignore session deduplication history for this call
        #[arg(long)]
        no_dedup: bool,
    },
    /// Indexed file tree
    Files {
        dir: Option<String>,
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long, default_value = "tree")]
        format: String,
        #[arg(long)]
        no_metadata: bool,
        /// Show only test files
        #[arg(long, conflicts_with = "no_tests")]
        tests: bool,
        /// Exclude test files
        #[arg(long)]
        no_tests: bool,
        #[arg(long)]
        max_depth: Option<usize>,
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// Functions that call one or more symbols
    Callers {
        #[arg(required = true, num_args = 1..)]
        symbols: Vec<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        max_chars: Option<usize>,
        /// Ignore session deduplication history for this call
        #[arg(long)]
        no_dedup: bool,
    },
    /// Functions one or more symbols call
    Callees {
        #[arg(required = true, num_args = 1..)]
        symbols: Vec<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        max_chars: Option<usize>,
        /// Ignore session deduplication history for this call
        #[arg(long)]
        no_dedup: bool,
    },
    /// Symbols affected by changing one or more symbols
    Impact {
        #[arg(required = true, num_args = 1..)]
        symbols: Vec<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(short, long, default_value_t = 2)]
        depth: u32,
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// Implementations / subtypes of one or more interfaces, abstract classes, or methods
    Implementations {
        #[arg(required = true, num_args = 1..)]
        symbols: Vec<String>,
        #[arg(long)]
        file: Option<String>,
        /// Maximum inheritance depth to traverse
        #[arg(short, long, default_value_t = 10)]
        depth: u32,
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// Test files affected by changes to the given files
    Affected {
        files: Vec<String>,
        /// Read file names from stdin, one per line
        #[arg(long)]
        stdin: bool,
        #[arg(short, long, default_value_t = 5)]
        depth: u32,
        /// Only report files matching this glob
        #[arg(short, long)]
        filter: Option<String>,
        /// Also follow links across a process boundary (HTTP client, queue job, event bus/socket, Celery)
        #[arg(long)]
        cross_tier: bool,
        #[arg(short = 'j', long)]
        json: bool,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Import cycles (exit 1 when any exist)
    Cycles {
        path: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Serve the MCP protocol (stdio by default; `--http` for HTTP)
    Serve {
        /// MCP over stdio (the default transport; accepted for compatibility with `serve --mcp`)
        #[arg(long)]
        mcp: bool,
        /// Listen for MCP over HTTP on this address instead of stdio, e.g. 127.0.0.1:7878
        #[arg(long, value_name = "ADDR", num_args = 0..=1, default_missing_value = "127.0.0.1:7878")]
        http: Option<String>,
        /// Bearer token required by the HTTP transport (or set CGR_HTTP_TOKEN)
        #[arg(long)]
        token: Option<String>,
        /// Do not watch files for changes
        #[arg(long)]
        no_watch: bool,
        /// Enable session-level deduplication across explore/node/callers/callees (also CGR_DEDUP=1 or CGR_EXPLORE_DEDUP=1)
        #[arg(long)]
        dedup: bool,
        /// Disable pre-query synchronous incremental sync (also CGR_AUTO_SYNC=0)
        #[arg(long)]
        no_auto_sync: bool,
    },
    /// Register the MCP server (and hook) with Claude Code, Cursor and Codex
    Install {
        /// Agents to configure: claude-code, cursor, codex (default: every one found on this machine)
        #[arg(long = "agent", value_name = "AGENT")]
        agents: Vec<String>,
        /// Configure the current project instead of the user's home
        #[arg(long)]
        local: bool,
        /// Claude Code: do not add the prompt hook
        #[arg(long)]
        no_hook: bool,
        /// Claude Code: do not pre-approve the explore tool
        #[arg(long)]
        no_permissions: bool,
        #[arg(long)]
        dry_run: bool,
        /// Path of the executable to register (default: this one)
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// Undo `install`
    Uninstall {
        #[arg(long = "agent", value_name = "AGENT")]
        agents: Vec<String>,
        #[arg(long)]
        local: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// Remove the index of a project (`.cgr/`)
    Uninit {
        path: Option<PathBuf>,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Hidden: Claude Code UserPromptSubmit hook (reads JSON on stdin)
    #[command(hide = true)]
    PromptHook,
    /// Hidden: nothing to unlock (locks are released by the OS when a process exits)
    #[command(hide = true)]
    Unlock,
    /// Hidden: there is no daemon in this version
    #[command(hide = true)]
    Daemon,
    /// Export the graph to a SQLite file for ad-hoc SQL queries
    Export {
        /// Output file
        #[arg(long, value_name = "FILE", required = true)]
        sqlite: PathBuf,
        /// Replace the output file if it exists
        #[arg(long)]
        force: bool,
    },
    /// Print version information
    Version {
        #[arg(long)]
        verbose: bool,
    },
    /// Development helpers
    #[command(subcommand)]
    Debug(DebugCmd),
    /// Development benchmarks (run them through scripts/bench-local.sh)
    #[command(subcommand)]
    Bench(BenchCmd),
}

#[derive(Subcommand)]
enum BenchCmd {
    /// Full index: median time, phases and peak memory over several runs
    Index {
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 3)]
        runs: usize,
        /// Worker threads for the child processes (default: automatic)
        #[arg(long)]
        threads: Option<usize>,
        /// Run this many explore readers against the previous snapshot while the next index builds
        #[arg(long, default_value_t = 0)]
        with_readers: usize,
        /// Queries for the readers (one per line)
        #[arg(long)]
        queries: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Random single-file edits, each followed by a sync (works on a copy of the tree)
    Sync {
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 200)]
        edits: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Edit the tree itself instead of a copy
        #[arg(long)]
        in_place: bool,
        /// Compare the final index with a full rebuild
        #[arg(long)]
        verify: bool,
        /// Print every edit with its sync result
        #[arg(short, long)]
        verbose: bool,
        #[arg(long)]
        json: bool,
    },
    /// Summarise a directory of `bench ... --json` results and compare it with the last baseline
    Compare {
        /// Directory with `<repo>.<index|sync|explore>.json` files
        #[arg(long)]
        results: PathBuf,
        /// Directory of saved summaries (bench/history)
        #[arg(long, default_value = "bench/history")]
        history: PathBuf,
        /// Benchmark tier the numbers belong to (standard, restricted)
        #[arg(long, default_value = "standard")]
        tier: String,
        /// Fail when a timing is more than this many percent slower than the baseline
        #[arg(long, default_value_t = 10.0)]
        threshold: f64,
        /// Do not write the new summary into the history directory
        #[arg(long)]
        no_save: bool,
        /// Save the summary as the new baseline even when it regressed
        #[arg(long)]
        force_save: bool,
    },
    /// explore latency distribution
    Explore {
        path: Option<PathBuf>,
        /// One query per line (default: generated from symbol names)
        #[arg(long)]
        queries: Option<PathBuf>,
        /// Number of generated queries
        #[arg(long, default_value_t = 200)]
        count: usize,
        #[arg(long, default_value_t = 3)]
        runs: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Print edges (except `contains`) as sorted lines
    Dump,
    /// Compare the index on disk with a fresh full rebuild (exit 1 when they differ)
    Diff {
        path: Option<PathBuf>,
        /// How many differing lines to print
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print unresolved references
    Unresolved,
}

/// Restore the default SIGPIPE action so `cgr ... | head` ends quietly instead of panicking.
fn reset_sigpipe() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn main() {
    reset_sigpipe();
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    let base = cli.project;
    let sync = if cli.auto_sync {
        cmds::SyncMode::Explicit
    } else if cli.no_sync {
        cmds::SyncMode::Off
    } else if cg_core::brand::env::get(cg_core::brand::env::AUTO_SYNC, None).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        cmds::SyncMode::Auto
    } else {
        cmds::SyncMode::Off
    };
    let session = cli.session.or_else(|| cg_core::brand::env::get(cg_core::brand::env::SESSION, None));
    let qctx = cmds::QueryCtx { base: base.clone(), sync, session };
    match cli.cmd {
        Cmd::Init { path, local_only } => cmds::init(path.or(base), local_only),
        Cmd::Index { path, force, quiet, timings, report_json } => cmds::index(path.or(base), force, quiet, timings, report_json),
        Cmd::Sync { path, quiet } => cmds::sync(path.or(base), quiet),
        Cmd::Status { path, json, strict, verify } => cmds::status(path.or(base), json, strict, verify),
        Cmd::Query { search, kind, limit, max_chars, json } => cmds::query(&qctx, &search, kind, limit, max_chars, json),
        Cmd::Explore { query, max_files, max_chars, compact, no_dedup } => cmds::explore(&qctx, &query.join(" "), max_files, max_chars, compact, no_dedup),
        Cmd::Node { targets, code, file, offset, limit, symbols_only, compact, max_chars, no_dedup } => {
            cmds::node(&qctx, &targets, cmds::NodeArgs { code, file, offset, limit, symbols_only, compact, max_chars, no_dedup })
        }
        Cmd::Files { dir, pattern, format, no_metadata, tests, no_tests, max_depth, max_chars } => {
            cmds::files(&qctx, dir, pattern, &format, !no_metadata, tests, no_tests, max_depth, max_chars)
        }
        Cmd::Callers { symbols, file, limit, max_chars, no_dedup } => cmds::callers(&qctx, &symbols, file, limit, max_chars, no_dedup, true),
        Cmd::Callees { symbols, file, limit, max_chars, no_dedup } => cmds::callers(&qctx, &symbols, file, limit, max_chars, no_dedup, false),
        Cmd::Impact { symbols, file, depth, max_chars } => cmds::impact(&qctx, &symbols, file, depth, max_chars),
        Cmd::Implementations { symbols, file, depth, max_chars } => cmds::implementations(&qctx, &symbols, file, depth, max_chars),
        Cmd::Affected { files, stdin, depth, filter, cross_tier, json, quiet } => cmds::affected(&qctx, files, stdin, depth, filter, cross_tier, json, quiet),
        Cmd::Cycles { path, json } => cmds::cycles(path.or(base), sync, json),
        Cmd::Serve { mcp: _, http, token, no_watch, dedup, no_auto_sync } => cmds::serve(base, http, token, no_watch, dedup, no_auto_sync),
        Cmd::Install { agents, local, no_hook, no_permissions, dry_run, bin } => {
            cmds::install(base, agents, local, !no_hook, !no_permissions, dry_run, bin, true)
        }
        Cmd::Uninstall { agents, local, dry_run, bin } => cmds::install(base, agents, local, true, true, dry_run, bin, false),
        Cmd::Uninit { path, yes } => cmds::uninit(path.or(base), yes),
        Cmd::PromptHook => Ok(hook::run()),
        Cmd::Unlock => {
            println!("Nothing to unlock: locks are held by the operating system and released when a process exits.");
            Ok(0)
        }
        Cmd::Daemon => {
            println!("There is no daemon: each agent session runs its own `cgr serve --mcp` process.");
            Ok(0)
        }
        Cmd::Export { sqlite, force } => cmds::export_sqlite(base, sqlite, force),
        Cmd::Version { verbose } => cmds::version(verbose),
        Cmd::Debug(DebugCmd::Dump) => cmds::debug_dump(base),
        Cmd::Debug(DebugCmd::Unresolved) => cmds::debug_unresolved(base),
        Cmd::Debug(DebugCmd::Diff { path, limit }) => cmds::debug_diff(path.or(base), limit),
        Cmd::Bench(BenchCmd::Index { path, runs, threads, with_readers, queries, json }) => {
            bench::index(path.or(base), runs, threads, with_readers, queries, json)
        }
        Cmd::Bench(BenchCmd::Sync { path, edits, seed, in_place, verify, verbose, json }) => {
            bench::sync(path.or(base), edits, seed, in_place, verify, verbose, json)
        }
        Cmd::Bench(BenchCmd::Compare { results, history, tier, threshold, no_save, force_save }) => {
            bench::compare(results, history, tier, threshold, !no_save, force_save)
        }
        Cmd::Bench(BenchCmd::Explore { path, queries, count, runs, seed, json }) => bench::explore(path.or(base), queries, count, runs, seed, json),
    }
}
