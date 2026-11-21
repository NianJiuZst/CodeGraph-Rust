//! cg-extract: turns files into per-file node/edge/reference tables plus facts.

pub mod cpp_preparse;
pub mod facts;
pub mod macro_mask;
pub mod pipeline;
pub mod post;
pub mod rows;

pub use facts::{BindKind, BindingFact, ExportFact, Facts, ImportFact, ImportKind, ScopeFact, ScopeKind};
pub use pipeline::{
    extract_all, extract_bytes, extract_one, extract_stream, file_node_only, fileflags, try_extract_stream, try_extract_stream_in_pool, ExtractConfig,
    FileBatch,
};
pub use post::{node_digest, ExtraNode, ExtraRef, FileHook, HookCtx, HookOut, SiteFact};
pub use rows::{EdgeView, FileRows, NodeView, RefView};
mod hook_plan;
pub mod spill;

/// Wire-format version of the vendored kernel (recorded in every segment).
pub const KERNEL_ABI_VERSION: u8 = cg_kernel::KERNEL_ABI_VERSION;
