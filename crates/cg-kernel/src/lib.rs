//! cg-kernel - the upstream CodeGraph extraction kernel with the napi boundary removed.
//!
//! Origin: `colbymchenry/codegraph` `codegraph-kernel/` (MIT, see THIRD_PARTY_NOTICES at
//! the repository root and `UPSTREAM_COMMIT` next to this crate's manifest).
//!
//! Patch P1 replaces the napi entry points with the pure-Rust API below and puts
//! each walker behind a cargo feature.
//! Input is `(file path, source text, language)`; output is the five flat tables
//! described in `buffers.rs` (ABI v2), one call per file.

// Vendored code: keep upstream's style, silence lints that newer toolchains add.
#![allow(clippy::all)]
#![allow(dead_code, unused_imports, unused_variables, unused_mut)]

/// First statement of every recursive walker function (see stack.rs, #1581):
/// once the stack pointer is inside the red zone, stop descending - the
/// latched flag makes `stack::run_guarded` discard the walk and defer the
/// file. `Default::default()` covers every walker return type in use
/// (`()`, `bool`, `Option<_>`, `String`); a hook returning `false` just sends
/// its caller down the generic child walk, whose own guard returns at once.
macro_rules! stack_guard {
    () => {
        if $crate::stack::exhausted() {
            return ::core::default::Default::default();
        }
    };
}

mod buffers;
mod docstring;
pub mod hooks;
mod ids;
mod langs;
mod stack;
mod textutil;

#[cfg(feature = "lang-c")]
mod ccpp;
#[cfg(feature = "lang-csharp")]
mod csharp;
#[cfg(feature = "lang-go")]
mod go;
#[cfg(feature = "lang-java")]
mod java;
#[cfg(feature = "lang-kotlin")]
mod kotlin;
#[cfg(feature = "lang-php")]
mod php;
#[cfg(feature = "lang-python")]
mod python;
#[cfg(feature = "lang-ruby")]
mod ruby;
#[cfg(feature = "lang-rust")]
mod rustlang;
#[cfg(feature = "lang-swift")]
mod swift;
#[cfg(feature = "lang-ts")]
mod tsjs;

use std::cell::Cell;
use std::fmt;

pub use buffers::{
    EmitOut, EDGE_KINDS, EDGE_ROW_SIZE, FUNCTION_REF_CODE, KERNEL_ABI_VERSION, META_SIZE,
    NODE_KINDS, NODE_ROW_SIZE, NONE, REF_FLAG_FILE_PATH, REF_ROW_SIZE,
};

/// Options for one extraction.
#[derive(Debug, Clone, Copy)]
pub struct ExtractOptions {
    /// Keep walking a syntax tree that contains ERROR / MISSING nodes instead of
    /// returning `defer:` (upstream hands such files to a wasm fallback that
    /// cgr does not have). cgr defaults to `true`.
    pub allow_error_trees: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        ExtractOptions { allow_error_trees: true }
    }
}

/// Why an extraction produced no tables.
#[derive(Debug, Clone)]
pub enum KernelError {
    /// Expected routing signal: the walker declined this file (error tree with
    /// `allow_error_trees = false`, nesting too deep for the stack guard, ...).
    Defer(String),
    /// A real failure (unknown language, grammar load failure, ...).
    Failed(String),
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::Defer(m) | KernelError::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for KernelError {}

/// Wire-contract description, checked at startup against `cg-core`'s enums.
#[derive(Debug, Clone)]
pub struct ContractInfo {
    pub abi_version: u32,
    pub kernel_version: &'static str,
    pub node_kinds: &'static [&'static str],
    pub edge_kinds: &'static [&'static str],
    pub languages: &'static [&'static str],
}

pub fn contract_info() -> ContractInfo {
    ContractInfo {
        abi_version: buffers::KERNEL_ABI_VERSION as u32,
        kernel_version: env!("CARGO_PKG_VERSION"),
        node_kinds: &buffers::NODE_KINDS,
        edge_kinds: &buffers::EDGE_KINDS,
        languages: langs::LANGUAGES,
    }
}

/// Whether this build can extract `language`.
pub fn supports(language: &str) -> bool {
    langs::LANGUAGES.contains(&language)
}

thread_local! {
    static ALLOW_ERROR_TREES: Cell<bool> = const { Cell::new(false) };
}

/// Read by the walkers (patch P2) at their `has_error()` checkpoint.
pub(crate) fn allow_error_trees() -> bool {
    ALLOW_ERROR_TREES.with(|c| c.get())
}

/// Extract one file. The whole walk runs under the stack guard (stack.rs, #1581):
/// a file nested deeply enough to overflow this thread's stack comes back as
/// `KernelError::Defer` instead of a SIGSEGV.
pub fn extract_file(
    path: &str,
    src: &str,
    language: &str,
    opts: &ExtractOptions,
) -> Result<EmitOut, KernelError> {
    ALLOW_ERROR_TREES.with(|c| c.set(opts.allow_error_trees));
    let out = stack::run_guarded(|| match language {
        #[cfg(feature = "lang-java")]
        "java" => java::extract(path, src),
        #[cfg(feature = "lang-python")]
        "python" => python::extract(path, src),
        #[cfg(feature = "lang-go")]
        "go" => go::extract(path, src),
        #[cfg(feature = "lang-rust")]
        "rust" => rustlang::extract(path, src),
        #[cfg(feature = "lang-kotlin")]
        "kotlin" => kotlin::extract(path, src),
        #[cfg(feature = "lang-ts")]
        "typescript" | "tsx" | "javascript" | "jsx" => tsjs::extract(path, src, language),
        #[cfg(feature = "lang-c")]
        "c" | "cpp" => ccpp::extract(path, src, language),
        #[cfg(feature = "lang-csharp")]
        "csharp" => csharp::extract(path, src),
        #[cfg(feature = "lang-php")]
        "php" => php::extract(path, src),
        #[cfg(feature = "lang-ruby")]
        "ruby" => ruby::extract(path, src),
        #[cfg(feature = "lang-swift")]
        "swift" => swift::extract(path, src),
        other => Err(format!("unsupported language: {other}")),
    });
    out.map_err(|e| {
        if e.starts_with("defer:") {
            KernelError::Defer(e)
        } else {
            KernelError::Failed(e)
        }
    })
}
