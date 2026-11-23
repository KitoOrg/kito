//! Client of the daemon, used by the CLI and by the MCP server.
//!
//! A repository is either here, with its daemon on a Unix socket, or kept
//! by a server (`kito-server`): then the directory holds
//! `.kito/remote.toml`, which names the server among the person's (see
//! [`crate::servers`]) or, connected before 0.40, says how the server is
//! reached and with what token; and a daemon
//! of its own on the same kind of socket, which asks the server over TCP
//! or through a command such as `ssh` and keeps here the lanes that are
//! worked on from here.

use crate::rpc::{self, Request, Response, RpcError};
use kito_core::paths::Canonical as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
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
    /// `host:port` of a server reached over TCP. In what a directory that
    /// names its server keeps, the name.
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
    /// The fingerprint of the server's certificate (`sha256:…`): what says
    /// that the server reached is the one meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The server has a certificate from a public authority, and is known
    /// by its name instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub authority: bool,
    /// The server does not encrypt its connections (`serve --plain`):
    /// accepted for a server on this machine only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plain: bool,
    /// The way this person signs in to the server (`github`): the token
    /// is then a session's, and `kito sign-in` gets another this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<String>,
    /// The entry of the person's `servers.toml` this connection is, when
    /// the directory names its server (ADR 0023): what a new session or
    /// token is kept in.
    #[serde(skip)]
    pub name: Option<String>,
}

/// What a directory that names its server keeps: the name and the
/// repository, nothing secret.
#[derive(Serialize)]
struct Named<'a> {
    server: &'a str,
    repo: &'a str,
}

impl Remote {
    /// The remote a directory is connected to, if it is: the whole
    /// connection, from the person's `servers.toml` when the directory
    /// names its server.
    pub fn load(root: &Path) -> Option<Remote> {
        Some(Remote::read(root)?.filled(&crate::servers::Servers::load()))
    }

    /// What the directory's `remote.toml` says, as it says it.
    pub fn read(root: &Path) -> Option<Remote> {
        let text = std::fs::read_to_string(root.join(kito_store::STORE_DIR).join(REMOTE_FILE)).ok()?;
        toml::from_str(&text).ok()
    }

    /// Whether this is what a directory that names its server keeps: a
    /// name and a repository. One connected before 0.40 keeps how the
    /// server is reached and known, and a token or a command.
    pub fn names_its_server(&self) -> bool {
        self.server.is_some()
            && self.command.is_empty()
            && self.token.is_none()
            && self.fingerprint.is_none()
            && !self.authority
            && !self.plain
            && self.sign_in.is_none()
    }

    /// The whole connection, from the entry of `servers` a directory names.
    pub fn filled(self, servers: &crate::servers::Servers) -> Remote {
        let Some(name) = self.server.clone().filter(|_| self.names_its_server()) else {
            return self;
        };
        match servers.servers.get(&name) {
            Some(entry) => entry.remote(&name, &self.repo),
            // A server the person is not connected to: reached nowhere.
            None => Remote { server: None, name: Some(name), repo: self.repo, ..Remote::default() },
        }
    }

    /// Makes `root` a client of the repository `repo` of the person's
    /// server `name`, written whole. Nothing secret is in it.
    pub fn save_named(root: &Path, name: &str, repo: &str) -> std::io::Result<()> {
        let dir = root.join(kito_store::STORE_DIR);
        std::fs::create_dir_all(&dir)?;
        let text = toml::to_string(&Named { server: name, repo }).map_err(std::io::Error::other)?;
        crate::servers::write_whole(&dir.join(REMOTE_FILE), text.as_bytes())
    }

    /// Makes `root` a client of this remote. The file holds the token: it
    /// is readable by its owner only.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let dir = root.join(kito_store::STORE_DIR);
        std::fs::create_dir_all(&dir)?;
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // On Windows the user profile's permissions keep it from other users (ADR 0002).
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(dir.join(REMOTE_FILE))?;
        file.write_all(text.as_bytes())
    }

    /// The address of the server, resolved, and its host name.
    pub fn address(&self) -> Result<(std::net::SocketAddr, String), ClientError> {
        use std::net::ToSocketAddrs;
        let server = self.server.as_deref().unwrap_or_default();
        let unreachable = |e: std::io::Error| ClientError::Server(format!("{server}: {e}"));
        let address = server
            .to_socket_addrs()
            .map_err(unreachable)?
            .next()
            .ok_or_else(|| ClientError::Server(format!("{server}: no such address")))?;
        let host = server.rsplit_once(':').map_or(server, |(host, _)| host);
        Ok((address, host.trim_matches(['[', ']']).to_owned()))
    }

    /// How the server is known to be the one meant; `None`: the
    /// connection is not encrypted. An error for a server on another
    /// machine that nothing vouches for.
    pub fn trust(&self, address: &std::net::SocketAddr) -> Result<Option<crate::tls::Trust>, ClientError> {
        use crate::tls::Trust;
        let here = address.ip().is_loopback();
        match (self.plain, &self.fingerprint, self.authority) {
            (true, ..) if here => Ok(None),
            (true, ..) => Err(ClientError::Server(format!(
                "{address} is not on this machine: a connection that is not encrypted is refused"
            ))),
            (_, Some(pin), _) => Ok(Some(Trust::Fingerprint(pin.clone()))),
            (_, None, true) => Ok(Some(Trust::Authority)),
            (_, None, false) if here => Ok(Some(Trust::Anything)),
            _ => Err(ClientError::Server(format!(
                "{address} is not on this machine, and nothing says it is the server you mean: connect \
                 with --fingerprint <what `kito-server fingerprint` prints there> (or --authority if \
                 its certificate comes from a public authority)"
            ))),
        }
    }

    /// Where the server is, in words.
    pub fn place(&self) -> String {
        match (&self.server, &self.name) {
            (Some(server), _) => server.clone(),
            (None, Some(name)) if self.command.is_empty() => name.clone(),
            (None, _) => self.command.join(" "),
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
    let start = start.canonical().ok()?;
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
    /// The version of Kito the server said it is when it was greeted;
    /// none: a daemon, or a server that does not say.
    server: Option<String>,
    /// Who the server said is asking, when it was greeted.
    user: Option<String>,
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
        let stream = crate::ipc::connect(root)
            .map_err(|e| ClientError::Connect(format!("{}: {e}", socket.display())))?;
        let mut writer = stream.try_clone()?;
        // The key of the socket first: it says this client can read the
        // repository's own data. A daemon that has none ignores nothing:
        // none is sent.
        // A plugin says its own, which Kito gave it when it started it
        // (ADR 0005): the daemon answers it what the repository gave it.
        let own = std::env::var(crate::plugins::ENV_KEY).ok().filter(|key| !key.is_empty());
        if let Some(key) = own.or_else(|| rpc::socket_key(root)) {
            let mut line = serde_json::to_vec(&json!({ "key": key }))
                .map_err(|e| ClientError::Protocol(e.to_string()))?;
            line.push(b'\n');
            writer.write_all(&line)?;
        }
        Ok(Client {
            reader: Box::new(BufReader::new(stream)),
            writer: Box::new(writer),
            child: None,
            next_id: 1,
            server: None,
            user: None,
        })
    }

    /// Reaches a server, and says nothing to it yet.
    fn reach(remote: &Remote) -> Result<Client, ClientError> {
        Ok(match (&remote.server, remote.command.as_slice()) {
            (Some(server), _) => {
                let unreachable = |e: std::io::Error| ClientError::Server(format!("{server}: {e}"));
                let (address, host) = remote.address()?;
                let trust = remote.trust(&address)?;
                let stream =
                    std::net::TcpStream::connect_timeout(&address, CONNECT_WITHIN).map_err(unreachable)?;
                let _ = stream.set_nodelay(true);
                // A server that went away without a word is not waited for forever.
                let _ = stream.set_read_timeout(Some(ANSWER_WITHIN));
                let (reader, writer): crate::tls::Ends = match trust {
                    Some(trust) => crate::tls::connect(stream, &host, &trust)
                        .map_err(|e| ClientError::Server(format!("{server}: {e}")))?,
                    None => {
                        let writer = stream.try_clone()?;
                        (Box::new(BufReader::new(stream)), Box::new(writer))
                    }
                };
                Client { reader, writer, child: None, next_id: 1, server: None, user: None }
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
                    server: None,
                    user: None,
                }
            }
            (None, []) => {
                return Err(ClientError::Server(match &remote.name {
                    Some(name) => format!(
                        "{name} is not one of your servers (`kito server list`): `kito connect` to it first, \
                         as `--as {name}` if you gave it that name"
                    ),
                    None => "the remote names neither a server nor a command".into(),
                }))
            }
        })
    }

    /// Says one thing to a server before it knows who asks, and returns
    /// what it answers; what it refuses is an error.
    fn say(&mut self, remote: &Remote, message: &Value) -> Result<Value, ClientError> {
        let mut line = serde_json::to_vec(message).map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        self.writer.flush()?;
        let mut answer = Vec::new();
        if self.reader.read_until(b'\n', &mut answer)? == 0 {
            return Err(ClientError::Server(format!("{}: it closed the connection", remote.place())));
        }
        // A server that encrypts answers a plain hello with something
        // that is not text.
        let answer: Value = serde_json::from_slice(&answer).map_err(|_| {
            ClientError::Server(format!(
                "{}: it does not answer as a server that does not encrypt would: connect again without \
                 --plain",
                remote.place()
            ))
        })?;
        match answer.get("error").and_then(Value::as_str) {
            Some(reason) => Err(ClientError::Refused(reason.to_owned())),
            None => Ok(answer),
        }
    }

    /// Connects to a server and says who is asking. With `repo`, the
    /// requests that follow are about the remote's repository; without,
    /// only the server's own (`server.repos`) are answered.
    pub fn remote(remote: &Remote, repo: bool) -> Result<Client, ClientError> {
        let mut client = Client::reach(remote)?;
        // Who is asking, and about which repository.
        let mut hello = json!({ "version": PROTOCOL, "token": remote.token });
        if repo {
            hello["repo"] = json!(remote.repo);
        }
        let answer = client.say(remote, &json!({ "hello": hello }))?;
        match answer.get("ok") {
            Some(ok) => {
                client.server = ok["server"].as_str().map(str::to_owned);
                client.user = ok["user"].as_str().map(str::to_owned);
                Ok(client)
            }
            None => Err(ClientError::Protocol(format!("unexpected greeting: {answer}"))),
        }
    }

    /// The version of Kito the server said it is, when it was greeted.
    pub fn server_version(&self) -> Option<&str> {
        self.server.as_deref()
    }

    /// Who the server said is asking, when it was greeted.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Signs in to a server in one of the ways its plugins offer (ADR
    /// 0006). `tell` is given, once, where the person goes and what they
    /// enter there (`open`, `code`); this waits while they do. The answer
    /// is the token of a session, who it is for and the days it lasts.
    /// The ways to sign in that the remote's server offers: each a method
    /// and what it says of itself.
    pub fn sign_in_methods(remote: &Remote) -> Result<Vec<(String, String)>, ClientError> {
        let mut client = Client::reach(remote)?;
        let said = client.say(remote, &json!({ "sign_in": { "version": PROTOCOL } }))?;
        Ok(said["methods"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|way| {
                let method = way["method"].as_str()?.to_owned();
                Some((method, way["about"].as_str().unwrap_or_default().to_owned()))
            })
            .collect())
    }

    pub fn sign_in(
        remote: &Remote,
        method: &str,
        tell: &mut dyn FnMut(&Value),
    ) -> Result<Value, ClientError> {
        let mut client = Client::reach(remote)?;
        let begun = client.say(remote, &json!({ "sign_in": { "version": PROTOCOL, "method": method } }))?;
        let Some(how) = begun.get("sign_in") else {
            return Err(ClientError::Protocol(format!("unexpected answer to a sign-in: {begun}")));
        };
        tell(how);
        let interval = how["interval"].as_u64().unwrap_or(5);
        let mut wait = interval;
        loop {
            // Never in a tight circle, whatever the server says.
            std::thread::sleep(Duration::from_millis((wait * 1000).clamp(200, 60_000)));
            let answer = client.say(remote, &json!({ "poll": true }))?;
            if let Some(ok) = answer.get("ok") {
                return Ok(ok.clone());
            }
            match answer.get("wait").and_then(Value::as_u64) {
                Some(longer) => wait = longer.max(interval),
                None => {
                    return Err(ClientError::Protocol(format!("unexpected answer to a sign-in: {answer}")))
                }
            }
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
        let sibling = exe.with_file_name(format!("kitod{}", std::env::consts::EXE_SUFFIX));
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
    // The daemon outlives this process: it must not hold its pipes.
    crate::detach::keep_standard_handles();
    let mut command = Command::new(&binary);
    command.arg("--repo").arg(root).stdin(Stdio::null()).stdout(log).stderr(err);
    // A process group of its own: the daemon outlives the CLI's terminal.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // On Windows: detached from the console, in a group of its own (ADR 0002).
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(
        &mut command,
        DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
    );
    command.spawn().map_err(|e| ClientError::Spawn(format!("{}: {e}", binary.display())))
}

#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
