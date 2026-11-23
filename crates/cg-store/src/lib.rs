//! cg-store: the immutable snapshot format and everything that reads or writes it.

pub mod build;
pub mod compact;
pub mod delta;
pub mod dump;
pub mod edges;
pub mod finish;
pub mod index;
pub mod intern;
pub mod part;
pub mod records;
pub mod seg;
pub mod store;
pub mod strpool;
pub mod tokenize;
pub mod view;

pub use build::{build_draft, build_draft_at, node_id_string, parse_node_id, tail_of, BuildBase, Draft, EdgeBuild, ProjectMeta};
pub use delta::{assemble_delta, base_offsets, DeltaParts, EdgesOnlyFile};
pub use finish::{finish, FinishInput, RefUpdate};
pub use part::Part;
pub use records::*;
pub use seg::{Seg, SegMeta, SegParts};
pub use store::{FsyncMode, Manifest, Snapshot, Store, WatchGuard, WriteGuard};
pub use strpool::StrPool;
pub use view::{DeltaIdx, DeltaState, EdgeRef, FileRef, NodeRef, OutEdges, Posting, UnresolvedRefSite, UnresolvedRefSummary, UnresolvedScope, View};
