//! cg-core: types shared by every codegraph-rust crate.
//!
//! Dependency direction (design doc 3.1): everything depends on `cg-core`,
//! `cg-core` depends on nothing in the workspace.

pub mod brand;
pub mod budget;
pub mod cancel;
pub mod config;
pub mod error;
pub mod fileflags;
pub mod hash;
pub mod ids;
pub mod kinds;
pub mod path;
pub mod text;

pub use error::{CgError, Result};
pub use ids::{FileId, ListId, NodeIdx, ScopeId, StrId, NONE};
pub use kinds::{EdgeKind, Lang, LangFamily, NodeKind, Provenance, RefKind, Visibility};
