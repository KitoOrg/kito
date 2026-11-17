//! Client of the daemon, used by the CLI and by the MCP server.
//!
//! A repository is either here, with its daemon on a Unix socket, or kept
//! by a server (`kito-server`): then the directory holds
//! `.kito/remote.toml`, which says how the server is reached, and a daemon
//! of its own on the same kind of socket, which asks the server over TCP
//! or through a command such as `ssh` and keeps here the lanes that are
//! worked on from here.

use crate::rpc::{self, Request, Response, RpcError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The file that makes a directory a client of a repository on a server.
pub const REMOTE_FILE: &str = "remote.toml";

/// Version of the protocol between clients and `kito-server`.
pub const PROTOCOL: u64 = 1;

/// How long a server is given to accept a connection.
const CONNECT_WITHIN: Duration = Duration::from_secs(10);

/// How long a server is given to answer: longer than the longest wait a
/// request can ask for.
const ANSWER_WITHIN: Duration = Duration::from_secs(120);

/// How a directory reaches a repository kept by a server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    /// `host:port` of a server reached over TCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// A command whose standard input and output speak the protocol:
    /// `ssh user@host kito-server stdio --dir /srv/kito`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The repository on the server.
    pub repo: String,
    /// The token the server gave this person, for a server reached over TCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl Remote {
    /// The remote a directory is connected to, if it is.
    pub fn load(root: &Path) -> Option<Remote> {
        let text = std::fs::read_to_string(root.join(kito_store::STORE_DIR).join(REMOTE_FILE)).ok()?;
        toml::from_str(&text).ok()
    }

    /// Makes `root` a client of this remote. The file holds the token: it
    /// is readable by its owner only.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = root.join(kito_store::STORE_DIR);
        std::fs::create_dir_all(&dir)?;
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dir.join(REMOTE_FILE))?;
        file.write_all(text.as_bytes())
    }

    /// Where the server is, in words.
    pub fn place(&self) -> String {
        match &self.server {
            Some(server) => server.clone(),
            None => self.command.join(" "),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("no Kito repository here or in any parent directory (run `kito init`)")]
    NoRepository,
    #[error("cannot reach the daemon: {0}")]
    Connect(String),
    #[error("cannot reach the server: {0}")]
    Server(String),
    #[error("cannot start the daemon: {0}")]
    Spawn(String),
    #[error("communication with the daemon: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid response from the daemon: {0}")]
    Protocol(String),
    #[error("{0}")]
    Rpc(#[from] RpcError),
    #[error("the server refused the connection: {0}")]
    Refused(String),
}

/// Walks up from `start` until it finds the directory that contains
/// `.kito/`: a repository, or a directory connected to one on a server.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let start = start.canonicalize().ok()?;
    start
        .ancestors()
        .find(|dir| {
            let kito = dir.join(kito_store::STORE_DIR);
            kito.join("format").is_file() || kito.join(REMOTE_FILE).is_file()
        })
        .map(Path::to_owned)
}

pub struct Client {
    reader: Box<dyn BufRead + Send>,
    writer: Box<dyn Write + Send>,
    /// The process a connection made through a command goes through.
    child: Option<std::process::Child>,
    next_id: u64,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Client {
    /// Connects to the directory's daemon, if it is running.
    pub fn connect(root: &Path) -> Result<Client, ClientError> {
        let socket = rpc::socket_path(root);
        let stream = UnixStream::connect(&socket)
            .map_err(|e| ClientError::Connect(format!("{}: {e}", socket.display())))?;
        let writer = stream.try_clone()?;
        Ok(Client {
            reader: Box::new(BufReader::new(stream)),
            writer: Box::new(writer),
            child: None,
            next_id: 1,
        })
    }

    /// Connects to a server and says who is asking. With `repo`, the
    /// requests that follow are about the remote's repository; without,
    /// only the server's own (`server.repos`) are answered.
    pub fn remote(remote: &Remote, repo: bool) -> Result<Client, ClientError> {
        let mut client = match (&remote.server, remote.command.as_slice()) {
            (Some(server), _) => {
                use std::net::ToSocketAddrs;
                let unreachable = |e: std::io::Error| ClientError::Server(format!("{server}: {e}"));
                let address = server
                    .to_socket_addrs()
                    .map_err(unreachable)?
                    .next()
                    .ok_or_else(|| ClientError::Server(format!("{server}: no such address")))?;
                let stream =
                    std::net::TcpStream::connect_timeout(&address, CONNECT_WITHIN).map_err(unreachable)?;
                let _ = stream.set_nodelay(true);
                // A server that went away without a word is not waited for forever.
                let _ = stream.set_read_timeout(Some(ANSWER_WITHIN));
                let writer = stream.try_clone()?;
                Client {
                    reader: Box::new(BufReader::new(stream)),
                    writer: Box::new(writer),
                    child: None,
                    next_id: 1,
                }
            }
            (None, [program, args @ ..]) => {
                let mut child = Command::new(program)
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .map_err(|e| ClientError::Server(format!("{program}: {e}")))?;
                let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                    return Err(ClientError::Server(format!("{program}: no pipes")));
                };
                Client {
                    reader: Box::new(BufReader::new(stdout)),
                    writer: Box::new(stdin),
                    child: Some(child),
                    next_id: 1,
                }
            }
            (None, []) => {
                return Err(ClientError::Server("the remote names neither a server nor a command".into()))
            }
        };
        // Who is asking, and about which repository.
        let mut hello = json!({ "version": PROTOCOL, "token": remote.token });
        if repo {
            hello["repo"] = json!(remote.repo);
        }
        let mut line = serde_json::to_vec(&json!({ "hello": hello }))
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push(b'\n');
        client.writer.write_all(&line)?;
        client.writer.flush()?;
        let mut answer = String::new();
        if client.reader.read_line(&mut answer)? == 0 {
            return Err(ClientError::Server(format!("{}: it closed the connection", remote.place())));
        }
        let answer: Value =
            serde_json::from_str(&answer).map_err(|e| ClientError::Protocol(e.to_string()))?;
        match answer.get("error").and_then(Value::as_str) {
            Some(reason) => Err(ClientError::Refused(reason.to_owned())),
            None if answer.get("ok").is_some() => Ok(client),
            None => Err(ClientError::Protocol(format!("unexpected greeting: {answer}"))),
        }
    }

    /// Connects and checks that the daemon really answers: a socket can
    /// accept the connection even while its process is dying.
    fn connect_alive(root: &Path) -> Result<Client, ClientError> {
        let mut client = Client::connect(root)?;
        match client.call_value("ping", Value::Null) {
            // A daemon of an earlier version: it answers, which is all
            // that was asked.
            Err(ClientError::Rpc(error)) if error.code == rpc::METHOD_NOT_FOUND => Ok(client),
            Err(e) => Err(e),
            Ok(_) => Ok(client),
        }
    }

    /// Connects to the daemon, starting it if it is not already running.
    pub fn connect_or_start(root: &Path) -> Result<Client, ClientError> {
        if let Ok(client) = Client::connect_alive(root) {
            return Ok(client);
        }
        let mut child = spawn_daemon(root)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // If the daemon just started has already exited (for example
            // because the previous one was still closing the store), try again.
            if matches!(child.try_wait(), Ok(Some(_))) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
                child = spawn_daemon(root)?;
            }
            match Client::connect_alive(root) {
                Ok(client) => return Ok(client),
                Err(e) if Instant::now() >= deadline => {
                    let log = root.join(kito_store::STORE_DIR).join("kitod.log");
                    let tail = std::fs::read_to_string(&log).unwrap_or_default();
                    let tail: Vec<&str> = tail.lines().rev().take(5).collect();
                    let tail: Vec<&str> = tail.into_iter().rev().collect();
                    return Err(ClientError::Spawn(format!(
                        "{e}\nlast lines of {}:\n{}",
                        log.display(),
                        tail.join("\n")
                    )));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// Calls a method and deserializes the result.
    pub fn call<T: DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<T, ClientError> {
        let value = self.call_value(method, params)?;
        serde_json::from_value(value).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    pub fn call_value(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&Request::new(id, method, params))
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        self.writer.flush()?;
        let mut answer = String::new();
        if self.reader.read_line(&mut answer)? == 0 {
            return Err(ClientError::Protocol("the daemon closed the connection".into()));
        }
        let response: Response =
            serde_json::from_str(&answer).map_err(|e| ClientError::Protocol(e.to_string()))?;
        match (response.result, response.error) {
            (_, Some(error)) => Err(ClientError::Rpc(error)),
            (Some(result), None) => Ok(result),
            (None, None) => Err(ClientError::Protocol("response without a result".into())),
        }
    }
}

/// Daemon executable: `$KITOD_BIN`, then `kitod` next to the current
/// executable, then `kitod` on the PATH.
fn daemon_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("KITOD_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("kitod");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("kitod")
}

fn spawn_daemon(root: &Path) -> Result<std::process::Child, ClientError> {
    let log_path = root.join(kito_store::STORE_DIR).join("kitod.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| ClientError::Spawn(e.to_string()))?;
    let err = log.try_clone().map_err(|e| ClientError::Spawn(e.to_string()))?;
    let binary = daemon_binary();
    Command::new(&binary)
        .arg("--repo")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        // A process group of its own: the daemon outlives the CLI's terminal.
        .process_group(0)
        .spawn()
        .map_err(|e| ClientError::Spawn(format!("{}: {e}", binary.display())))
}
