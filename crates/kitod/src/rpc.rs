//! Protocollo tra daemon e client: JSON-RPC 2.0, un messaggio per riga, su
//! socket Unix.

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

/// Percorso del socket del daemon di un repository.
///
/// Di norma `.kito/kitod.sock`; se il percorso supera il limite dei socket
/// Unix (circa 100 byte) si ripiega su un nome nella cartella temporanea,
/// derivato dalla radice del repository.
pub fn socket_path(root: &Path) -> PathBuf {
    let preferred = root.join(kito_store::STORE_DIR).join("kitod.sock");
    if preferred.as_os_str().len() <= 100 {
        return preferred;
    }
    let digest = kito_core::Hash::of(root.as_os_str().as_encoded_bytes());
    std::env::temp_dir().join(format!("kito-{}.sock", digest.short()))
}

/// Statistiche del watcher restituite da `status`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WatcherStatus {
    /// Eventi ricevuti dal sistema operativo (esclusi `.kito` e `.git`).
    pub events: u64,
    /// Gruppi di eventi elaborati dopo il debounce.
    pub batches: u64,
    /// Scansioni complete di riconciliazione.
    pub rescans: u64,
    /// Tempo di elaborazione di un gruppo (dopo il debounce), in millisecondi.
    pub capture_p50_ms: Option<f64>,
    pub capture_p95_ms: Option<f64>,
    /// Tempo dal primo evento del gruppo all'op confermata, debounce compreso.
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
