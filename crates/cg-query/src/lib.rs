//! cg-query: everything an agent can ask of a snapshot (design doc 8). Every operation reads a
//! [`cg_store::View`] and does bounded lookups or traversals over indexes built at index time.

pub mod explore;
pub mod graph;
pub mod project;
pub mod search;
pub mod source;
pub mod strings;
pub mod telemetry;
pub mod tools;
pub mod traverse;

pub use graph::Graph;
pub use project::Project;
pub use search::{Hit, SearchOptions};
