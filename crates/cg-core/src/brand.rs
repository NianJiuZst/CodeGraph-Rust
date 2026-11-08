//! Names that appear on disk and on the wire. The project was called `cgr` while
//! the design was being written; the official name is `codegraph-rust` (design doc
//! 15.2 D1). Everything user-visible is defined here, so a rename is a one-file change.

/// Official project name.
pub const PROJECT_NAME: &str = "codegraph-rust";
/// Name of the executable (short form); `codegraph-rust` is installed as an alias.
pub const BIN_NAME: &str = "cgr";
/// Per-project data directory.
pub const DATA_DIR: &str = ".cgr";
/// Key under which the MCP server is registered in agent configs.
pub const MCP_SERVER_NAME: &str = "codegraph-rust";
/// Prefix of every MCP tool name: `codegraph-rust_explore`, ...
pub const MCP_TOOL_PREFIX: &str = "codegraph-rust_";
/// Names of the upstream project's files we stay compatible with.
pub const UPSTREAM_CONFIG_FILE: &str = "codegraph.json";
pub const CONFIG_FILE: &str = "cgr.json";
/// Version of the resolution rules: changing resolver behaviour bumps this, which
/// marks older indexes as "built by an older engine" (design doc 5.8).
pub const ENGINE_VERSION: u32 = 12;
/// Version of the on-disk segment format; a mismatch forces a full rebuild.
pub const FORMAT_VERSION: u32 = 2;
/// The upstream commit whose kernel is vendored (see crates/cg-kernel/UPSTREAM_COMMIT).
pub const UPSTREAM_KERNEL_COMMIT: &str = include_str!("../../cg-kernel/UPSTREAM_COMMIT");

/// Environment variables (design doc 16.4). The `CODEGRAPH_*` spellings are read as
/// fallbacks where the upstream project has an equivalent.
pub mod env {
    pub const MEMORY_MB: &str = "CGR_MEMORY_MB";
    pub const THREADS: &str = "CGR_THREADS";
    pub const MCP_TOOLS: &str = "CGR_MCP_TOOLS";
    pub const MCP_TOOLS_UPSTREAM: &str = "CODEGRAPH_MCP_TOOLS";
    pub const NO_PROMPT_HOOK: &str = "CGR_NO_PROMPT_HOOK";
    pub const NO_PROMPT_HOOK_UPSTREAM: &str = "CODEGRAPH_NO_PROMPT_HOOK";
    pub const FSYNC: &str = "CGR_FSYNC";
    pub const LOG: &str = "CGR_LOG";
    pub const TIMINGS: &str = "CGR_TIMINGS";

    /// `name` if set and non-empty, else `fallback`.
    pub fn get(name: &str, fallback: Option<&str>) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty()).or_else(|| fallback.and_then(|f| std::env::var(f).ok()).filter(|v| !v.is_empty()))
    }
}
