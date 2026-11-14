//! Client del daemon, usato dalla CLI e dal server MCP.

use crate::rpc::{self, Request, Response, RpcError};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("nessun repository Kito qui o nelle cartelle superiori (esegui `kito init`)")]
    NoRepository,
    #[error("impossibile contattare il daemon: {0}")]
    Connect(String),
    #[error("impossibile avviare il daemon: {0}")]
    Spawn(String),
    #[error("comunicazione con il daemon: {0}")]
    Io(#[from] std::io::Error),
    #[error("risposta del daemon non valida: {0}")]
    Protocol(String),
    #[error("{0}")]
    Rpc(#[from] RpcError),
}

/// Risale dalle cartelle di `start` fino a trovare quella che contiene `.kito/`.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let start = start.canonicalize().ok()?;
    start.ancestors().find(|dir| dir.join(kito_store::STORE_DIR).join("format").is_file()).map(Path::to_owned)
}

pub struct Client {
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    /// Si collega al daemon del repository, se è in esecuzione.
    pub fn connect(root: &Path) -> Result<Client, ClientError> {
        let socket = rpc::socket_path(root);
        let stream = UnixStream::connect(&socket)
            .map_err(|e| ClientError::Connect(format!("{}: {e}", socket.display())))?;
        Ok(Client { reader: BufReader::new(stream), next_id: 1 })
    }

    /// Si collega e verifica che il daemon risponda davvero: un socket può
    /// accettare la connessione anche mentre il suo processo sta morendo.
    fn connect_alive(root: &Path) -> Result<Client, ClientError> {
        let mut client = Client::connect(root)?;
        client.call_value("status", Value::Null)?;
        Ok(client)
    }

    /// Si collega al daemon, avviandolo se non è già attivo.
    pub fn connect_or_start(root: &Path) -> Result<Client, ClientError> {
        if let Ok(client) = Client::connect_alive(root) {
            return Ok(client);
        }
        let mut child = spawn_daemon(root)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // Se il daemon appena avviato è già uscito (per esempio perché
            // quello precedente stava ancora chiudendo lo store), si riprova.
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
                        "{e}\nultime righe di {}:\n{}",
                        log.display(),
                        tail.join("\n")
                    )));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// Chiama un metodo e deserializza il risultato.
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
        self.reader.get_mut().write_all(&line)?;
        let mut answer = String::new();
        if self.reader.read_line(&mut answer)? == 0 {
            return Err(ClientError::Protocol("il daemon ha chiuso la connessione".into()));
        }
        let response: Response =
            serde_json::from_str(&answer).map_err(|e| ClientError::Protocol(e.to_string()))?;
        match (response.result, response.error) {
            (_, Some(error)) => Err(ClientError::Rpc(error)),
            (Some(result), None) => Ok(result),
            (None, None) => Err(ClientError::Protocol("risposta senza risultato".into())),
        }
    }
}

/// Eseguibile del daemon: `$KITOD_BIN`, poi `kitod` accanto all'eseguibile
/// corrente, poi `kitod` nel PATH.
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
        // Gruppo di processi proprio: il daemon sopravvive al terminale della CLI.
        .process_group(0)
        .spawn()
        .map_err(|e| ClientError::Spawn(format!("{}: {e}", binary.display())))
}
