//! `kitod`: il daemon di un repository Kito.
//!
//! Possiede lo store ed è l'unico a scriverci. La CLI e il server MCP sono
//! client sottili che gli parlano tramite socket Unix (JSON-RPC).

pub mod client;
pub mod config;
pub mod error;
pub mod fsinfo;
pub mod ignore;
pub mod repo;
pub mod rpc;
pub mod server;

pub use error::{Error, Result};
pub use repo::Repo;
