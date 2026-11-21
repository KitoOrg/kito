//! `kitod`: the daemon of a Kito repository.
//!
//! It owns the store and is the only one that writes to it. The CLI and the
//! MCP server are thin clients that talk to it over a Unix socket (JSON-RPC).

pub mod client;
pub mod comments;
pub mod config;
pub mod connected;
pub mod detach;
pub mod error;
pub mod files;
pub mod fsinfo;
pub mod guard;
pub mod identity;
pub mod ignore;
pub mod integrate;
pub mod ipc;
pub mod lanes;
pub mod mirror;
pub mod notes;
pub mod plugins;
pub mod pull;
pub mod refs;
pub mod registry;
pub mod rename;
pub mod repo;
pub mod review;
pub mod rpc;
pub mod sandbox;
pub mod scope;
pub mod semantic;
pub mod server;
pub mod simulate;
pub mod tls;
pub mod verdicts;
pub mod why;

pub use error::{Error, Result};
pub use repo::Repo;
