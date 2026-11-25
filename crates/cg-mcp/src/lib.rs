//! cg-mcp: the MCP server (design doc 10). Transport-independent core in [`server`], stdio and
//! HTTP transports on top.

pub mod defs;
pub mod http;
pub mod server;
pub mod stdio;

pub use http::{serve_http, HttpConfig};
pub use server::{Config, Reply, Server, SyncHookResult};
pub use stdio::serve_stdio;
