//! Protocol between daemon and clients: JSON-RPC 2.0, one message per line,
//! over a Unix socket.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const PARSE_ERROR: i64 = -32700;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: &str, params: Value) -> Request {
        Request { jsonrpc: "2.0".into(), id, method: method.into(), params }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Response {
        Response { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
    }

    pub fn err(id: u64, code: i64, message: impl Into<String>) -> Response {
        Response {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError { code, message: message.into() }),
        }
    }
}

/// Path of the socket of a repository's daemon.
///
/// Normally `.kito/kitod.sock`; if the path exceeds the limit of Unix sockets
/// (about 100 bytes) it falls back to a name in the temporary directory,
/// derived from the repository root.
pub fn socket_path(root: &Path) -> PathBuf {
    let preferred = root.join(kito_store::STORE_DIR).join("kitod.sock");
    if preferred.as_os_str().len() <= 100 {
        return preferred;
    }
    let digest = kito_core::Hash::of(root.as_os_str().as_encoded_bytes());
    std::env::temp_dir().join(format!("kito-{}.sock", digest.short()))
}

/// Watcher statistics returned by `status`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WatcherStatus {
    /// Events received from the operating system (excluding `.kito` and `.git`).
    pub events: u64,
    /// Batches of events processed after the debounce.
    pub batches: u64,
    /// Full reconciliation scans.
    pub rescans: u64,
    /// `false` if the watcher is disabled by the configuration.
    #[serde(default)]
    pub watching: bool,
    /// Type of the repository's file system, if known.
    #[serde(default)]
    pub filesystem: Option<String>,
    /// Events are unreliable here: the daemon scans often.
    #[serde(default)]
    pub events_unreliable: bool,
    /// Changes found by the scan without the watcher having reported them.
    #[serde(default)]
    pub missed_by_watcher: u64,
    /// Current interval between two reconciliation scans.
    #[serde(default)]
    pub rescan_interval_ms: u64,
    /// Processing time of a batch (after the debounce), in milliseconds.
    pub capture_p50_ms: Option<f64>,
    pub capture_p95_ms: Option<f64>,
    /// Time from the first event of the batch to the confirmed op, debounce included.
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub version: String,
    pub uptime_secs: u64,
    pub socket: PathBuf,
    pub debounce_ms: u64,
    pub watcher: WatcherStatus,
    pub repo: crate::repo::Status,
}
