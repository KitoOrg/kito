//! `kitod`: the daemon of a Kito repository.
//!
//! It owns the store and is the only one that writes to it. The CLI and the
//! MCP server are thin clients that talk to it over a Unix socket (JSON-RPC).

pub mod client;
pub mod config;
pub mod error;
pub mod fsinfo;
pub mod identity;
pub mod ignore;
pub mod integrate;
pub mod lanes;
pub mod rename;
pub mod repo;
pub mod rpc;
pub mod semantic;
pub mod server;

pub use error::{Error, Result};
pub use repo::Repo;
