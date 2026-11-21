//! Protocol between daemon and clients: JSON-RPC 2.0, one message per line,
//! over a Unix socket.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The version of the daemon's methods, as plugins and other programs
/// rely on them (ADR 0005, `docs/api.md`). Adding a method or an optional
/// field of an answer does not change it; removing one, or changing what
/// it means, does.
pub const API: u32 = 2;

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const PARSE_ERROR: i64 = -32700;
/// What was asked is not done, and asking again will not help.
pub const REFUSED: i64 = -32020;
/// The lane is written from another machine now.
pub const NOT_THE_WRITER: i64 = -32021;
/// The lane does not exist, or is closed.
pub const NO_SUCH_LANE: i64 = -32022;

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

/// Something that happened in a repository, as the clients that watch it
/// are told.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Events are numbered from 1, in the order they happened.
    pub seq: u64,
    /// Wall-clock time, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// `lane_opened`, `lane_closed`, `scope_extended`, `intent`, `saved`,
    /// `renamed`, `integration_requested`, `integrated`, `conflicts`,
    /// `verification_failed`, `out_of_scope`, `nothing_to_integrate`,
    /// `failed`, `conflict_resolved`, `checkpoint`.
    pub kind: String,
    /// The lane it happened in (`lane/1`, `main`).
    pub lane: Option<String>,
    /// Who did it: the person, or the agent.
    pub by: Option<String>,
    /// What, in a few words.
    pub what: String,
    /// For `saved`: the files that changed. Empty when there were too
    /// many to list: then everything may have.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The bytes of a file as text a message can carry: base64, with padding.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            match i <= chunk.len() {
                true => out.push(char::from(*BASE64.get(((n >> shift) & 63) as usize).unwrap_or(&b'A'))),
                false => out.push('='),
            }
        }
    }
    out
}

/// The bytes a base64 text stands for; `None` if it is not base64.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    for chunk in text.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            let value = BASE64.iter().position(|known| known == c)? as u32;
            n |= value << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(bytes.get(..chunk.len() - 1)?);
    }
    Some(out)
}

/// Path of the socket of a repository's daemon.
///
/// Normally `.kito/kitod.sock`; if the path exceeds the limit of Unix sockets
/// (about 100 bytes) it falls back to a name in the temporary directory,
/// derived from the repository root.
pub fn socket_path(root: &Path) -> PathBuf {
    // On Windows, the file that stands for the daemon's pipe (crate::ipc).
    if cfg!(windows) {
        return root.join(kito_store::STORE_DIR).join("kitod.pipe");
    }
    let preferred = root.join(kito_store::STORE_DIR).join("kitod.sock");
    if preferred.as_os_str().len() <= 100 {
        return preferred;
    }
    let digest = kito_core::Hash::of(root.as_os_str().as_encoded_bytes());
    std::env::temp_dir().join(format!("kito-{}.sock", digest.short()))
}

/// The file that holds the key of a daemon's socket.
pub const SOCKET_KEY: &str = "socket.key";

/// The key a client says first on the daemon's socket. Whoever can read
/// `.kito/` can command the daemon; a command that runs confined (the
/// checks of an integration, what an agent starts in its lane) cannot read
/// it, and so cannot.
pub fn socket_key(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(kito_store::STORE_DIR).join(SOCKET_KEY)).ok()?;
    Some(text.trim().to_owned()).filter(|key| !key.is_empty())
}

/// The key of the socket, made if there is none yet: read by its owner only.
pub fn ensure_socket_key(root: &Path) -> std::io::Result<String> {
    use std::io::Write;
    if let Some(key) = socket_key(root) {
        return Ok(key);
    }
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    let key: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = root.join(kito_store::STORE_DIR).join(SOCKET_KEY);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // On Windows the user profile's permissions keep it from other users (ADR 0002).
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(key.as_bytes())?;
    Ok(key)
}

/// What a client says first: `{"key": "<the key>"}`. `Some(true)` if the
/// line is that and the key is right, `Some(false)` if it is wrong, `None`
/// if the line is something else (a request sent without a key).
pub fn key_said(line: &str, key: &str) -> Option<bool> {
    let said: serde_json::Value = serde_json::from_str(line).ok()?;
    let said = said.as_object().filter(|object| object.len() == 1)?.get("key")?.as_str()?;
    Some(said == key)
}

/// What a connection that did not say the key is told.
pub const NO_KEY: &str =
    "this connection did not say the key of the repository's daemon (.kito/socket.key): \
                          a command that runs confined cannot command the daemon";

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
