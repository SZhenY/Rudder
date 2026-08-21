//! Model Context Protocol feature.
//!
//! MCP transport, tools, resources, and protocol adapters belong in this module.

#[path = "impls/server.rs"]
mod server;
#[path = "impls/tools.rs"]
mod tools;

pub(crate) use server::run_stdio;
