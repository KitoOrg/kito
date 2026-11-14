//! The provider that asks a language server.
//!
//! One server per language runs in the main working directory. A snapshot
//! that differs from what is on disk there (a lane) is shown to the server
//! as open documents (`didOpen`), which a server takes as the truth in
//! place of the files; when the question is answered they are closed again
//! and the server is back to the disk. No copy of the repository, no second
//! server.
//!
//! The server is driven by a thread of its own with blocking I/O, so the
//! provider does not depend on which async runtime calls it. It is started
//! at the first question. A server that fails, or does not answer in time,
//! is stopped; the next question starts it again.

use crate::text::{offset_at, position_at};
use crate::{Error, Result, SemanticProvider, Snapshot, Usage};
use kito_core::{Lang, Location, RefSource, Reference, SymbolId};
use kito_parse::lang_for_path;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How a language server is started and what it is for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspConfig {
    /// Program and arguments: `["typescript-language-server", "--stdio"]`.
    pub command: Vec<String>,
    pub languages: Vec<Lang>,
    /// Seconds a question may take, the start of the server included.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Passed to the server as its `initializationOptions`, for the servers
    /// that need to be told something to start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

fn default_timeout() -> u64 {
    60
}

/// How the failure of a server whose program does not exist starts.
const NOT_INSTALLED: &str = "its program is not installed";

/// More names than this in a file are not all asked about by
/// [`LspProvider::references`]: one question each.
const MAX_NAMES_ASKED: usize = 2000;

/// A place as the server gives it.
#[derive(Clone, Debug)]
struct Place {
    /// Relative to the root; `None` for a file outside it.
    file: Option<PathBuf>,
    start: (u32, u32),
    end: (u32, u32),
}

struct Question {
    method: &'static str,
    file: PathBuf,
    position: (u32, u32),
}

enum Job {
    Ask {
        /// Documents to show the server in place of the files on disk.
        open: Vec<(PathBuf, String)>,
        questions: Vec<Question>,
        reply: tokio::sync::oneshot::Sender<Result<Vec<Vec<Place>>>>,
    },
    Stop,
}

/// A language server for some languages of a repository.
pub struct LspProvider {
    name: String,
    config: LspConfig,
    root: PathBuf,
    worker: Mutex<Option<Sender<Job>>>,
    /// The program is not installed: found out at the first question, and
    /// not tried again.
    missing: std::sync::atomic::AtomicBool,
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "go" => "go",
        "py" => "python",
        "rs" => "rust",
        "java" => "java",
        "cs" => "csharp",
        "c" | "h" => "c",
        "php" => "php",
        _ => "cpp",
    }
}

fn uri_of(root: &Path, file: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in root.join(file).to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri
}

fn path_of(root: &Path, uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?.as_bytes();
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut at = 0;
    while at < encoded.len() {
        match encoded[at] {
            b'%' => {
                let hex = std::str::from_utf8(encoded.get(at + 1..at + 3)?).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
                at += 3;
            }
            byte => {
                bytes.push(byte);
                at += 1;
            }
        }
    }
    let path = PathBuf::from(String::from_utf8(bytes).ok()?);
    path.strip_prefix(root).ok().map(Path::to_owned)
}

impl LspProvider {
    /// A server for the repository at `root`. Nothing is started yet.
    pub fn new(name: &str, config: LspConfig, root: &Path) -> LspProvider {
        // Servers answer with real paths: links in the root are resolved.
        let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
        LspProvider {
            name: name.to_owned(),
            config,
            root,
            worker: Mutex::new(None),
            missing: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `false` once the server's program turned out not to be installed.
    pub fn available(&self) -> bool {
        !self.missing.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn fail(&self, message: impl Into<String>) -> Error {
        Error::Server { server: self.name.clone(), message: message.into() }
    }

    async fn ask(&self, open: Vec<(PathBuf, String)>, questions: Vec<Question>) -> Result<Vec<Vec<Place>>> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let mut job = Some(Job::Ask { open, questions, reply });
        {
            let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
            // A worker that is gone (it was stopped) is replaced, once.
            for _ in 0..2 {
                let Some(waiting) = job.take() else { break };
                let sender = worker.get_or_insert_with(|| {
                    let (sender, jobs) = mpsc::channel();
                    let (name, config, root) = (self.name.clone(), self.config.clone(), self.root.clone());
                    std::thread::spawn(move || serve(&name, &config, &root, jobs));
                    sender
                });
                if let Err(mpsc::SendError(back)) = sender.send(waiting) {
                    job = Some(back);
                    *worker = None;
                }
            }
        }
        if job.is_some() {
            return Err(self.fail("its thread cannot be started"));
        }
        let answer = answer.await.map_err(|_| self.fail("stopped while a question was open"))?;
        if matches!(&answer, Err(Error::Server { message, .. }) if message.starts_with(NOT_INSTALLED)) {
            self.missing.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        answer
    }

    /// The documents that make the server see `snap`: what differs from the
    /// disk, and the files the questions are about (a server answers only
    /// about files it was told are open).
    fn documents(&self, snap: &Snapshot<'_>, about: &[&Path]) -> Vec<(PathBuf, String)> {
        let mine = |path: &Path| self.config.languages.contains(&lang_for_path(path));
        let mut open: Vec<(PathBuf, String)> = snap
            .overlay
            .iter()
            .filter(|(path, _)| mine(path))
            .map(|(path, content)| {
                // A file the snapshot does not have is shown empty.
                let text =
                    content.as_deref().map(|c| String::from_utf8_lossy(c).into_owned()).unwrap_or_default();
                (path.clone(), text)
            })
            .collect();
        for path in about {
            if !open.iter().any(|(known, _)| known == path) {
                if let Some(content) = snap.files.read(path) {
                    open.push(((*path).to_owned(), String::from_utf8_lossy(&content).into_owned()));
                }
            }
        }
        open
    }

    /// Stops the server, if it runs. The next question starts it again.
    pub async fn shutdown(&self) {
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.send(Job::Stop);
        }
    }
}

impl Drop for LspProvider {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = worker.send(Job::Stop);
        }
    }
}

impl SemanticProvider for LspProvider {
    fn languages(&self) -> &[Lang] {
        &self.config.languages
    }

    async fn usages(&self, snap: &Snapshot<'_>, sym: SymbolId) -> Result<Vec<Usage>> {
        let def = snap.definition(sym)?;
        let name = def.name().to_vec();
        let at = def.name_range().start;
        let question = Question {
            method: "textDocument/references",
            file: def.file.clone(),
            position: position_at(def.parsed.source(), at),
        };
        let places = self.ask(self.documents(snap, &[&def.file]), vec![question]).await?;

        let mut contents: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();
        let mut usages: Vec<Usage> = Vec::new();
        for place in places.into_iter().flatten() {
            let Some(file) = place.file else { continue };
            let Some(content) = contents.entry(file.clone()).or_insert_with(|| snap.files.read(&file)) else {
                continue;
            };
            let (Some(start), Some(end)) = (
                offset_at(content, place.start.0, place.start.1),
                offset_at(content, place.end.0, place.end.1),
            ) else {
                continue;
            };
            // Only the places where the name is written: a server may also
            // point at what refers to the symbol under another name.
            let written = content.get(start..end) == Some(name.as_slice());
            let definition = file == def.file && start == at;
            if written && !definition {
                usages.push(Usage {
                    file,
                    range: start..end,
                    confidence: 1.0,
                    source: RefSource::Lsp(self.name.clone()),
                });
            }
        }
        usages.sort_by(|a, b| (&a.file, a.range.start).cmp(&(&b.file, b.range.start)));
        usages.dedup_by(|a, b| a.file == b.file && a.range == b.range);
        Ok(usages)
    }

    async fn references(&self, snap: &Snapshot<'_>, file: &Path) -> Result<Vec<Reference>> {
        let Some(parsed) = snap.parse(file) else { return Ok(Vec::new()) };
        let source = parsed.source();
        let symbols = &parsed.file.symbols;
        let ids = kito_diff::ast::identify(snap.symbols, file, &parsed.file);
        let defined_at: Vec<usize> = symbols.iter().map(|s| s.name_range.start).collect();
        let names: Vec<_> = parsed
            .identifiers()
            .into_iter()
            .filter(|id| !defined_at.contains(&id.range.start))
            .take(MAX_NAMES_ASKED)
            .collect();
        let questions = names
            .iter()
            .map(|id| Question {
                method: "textDocument/definition",
                file: file.to_owned(),
                position: position_at(source, id.range.start),
            })
            .collect();
        let answers = self.ask(self.documents(snap, &[file]), questions).await?;

        // The definitions of each file an answer points to.
        let mut targets: HashMap<PathBuf, Vec<(std::ops::Range<usize>, SymbolId)>> = HashMap::new();
        let mut references = Vec::new();
        for (id, places) in names.iter().zip(answers) {
            let from = symbols
                .iter()
                .zip(&ids)
                .filter(|(sym, _)| sym.range.start <= id.range.start && id.range.end <= sym.range.end)
                .filter_map(|(_, id)| *id)
                .next_back();
            let Some(from) = from else { continue };
            for place in places {
                let Some(target) = place.file else { continue };
                let known = targets.entry(target.clone()).or_insert_with(|| {
                    let Some(parsed) = snap.parse(&target) else { return Vec::new() };
                    let ids = kito_diff::ast::identify(snap.symbols, &target, &parsed.file);
                    let symbols = parsed.file.symbols.iter().zip(ids);
                    symbols.filter_map(|(sym, id)| Some((sym.name_range.clone(), id?))).collect()
                });
                let Some(content) = snap.files.read(&target) else { continue };
                let Some(start) = offset_at(&content, place.start.0, place.start.1) else { continue };
                let to = known.iter().find(|(name, _)| name.contains(&start)).map(|(_, id)| *id);
                let Some(to) = to.filter(|to| *to != from) else { continue };
                let line = 1 + source[..id.range.start].iter().filter(|&&b| b == b'\n').count() as u32;
                references.push(Reference {
                    from,
                    to,
                    site: Location {
                        file: file.to_owned(),
                        line,
                        start: id.range.start as u32,
                        end: id.range.end as u32,
                    },
                    confidence: 1.0,
                    source: RefSource::Lsp(self.name.clone()),
                });
                break;
            }
        }
        Ok(references)
    }
}

/// The thread that owns a server: one job at a time.
fn serve(name: &str, config: &LspConfig, root: &Path, jobs: Receiver<Job>) {
    let mut running: Option<Running> = None;
    for job in jobs {
        match job {
            Job::Stop => break,
            Job::Ask { open, questions, reply } => {
                let deadline = Instant::now() + Duration::from_secs(config.timeout_secs.max(1));
                let answer = match &mut running {
                    Some(server) => server.ask(&open, &questions, deadline),
                    None => Running::start(name, config, root, deadline)
                        .and_then(|server| running.insert(server).ask(&open, &questions, deadline)),
                };
                if answer.is_err() {
                    // Whatever state it is in, it is not one to build on.
                    if let Some(server) = running.take() {
                        server.kill();
                    }
                }
                let _ = reply.send(answer);
            }
        }
    }
    if let Some(server) = running.take() {
        server.stop();
    }
}

struct Running {
    name: String,
    root: PathBuf,
    timeout: u64,
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Option<Value>>,
    next_id: u64,
}

/// Reads one message: headers, an empty line, a JSON body.
fn read_message(input: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; length?];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

impl Running {
    fn fail(&self, message: impl Into<String>) -> Error {
        Error::Server { server: self.name.clone(), message: message.into() }
    }

    fn start(name: &str, config: &LspConfig, root: &Path, deadline: Instant) -> Result<Running> {
        let fail = |message: String| Error::Server { server: name.to_owned(), message };
        let (program, arguments) =
            config.command.split_first().ok_or_else(|| fail("no command configured".to_owned()))?;
        let mut child = Command::new(program)
            .args(arguments)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => fail(format!("{NOT_INSTALLED} (`{program}`)")),
                _ => fail(format!("cannot start `{program}`: {e}")),
            })?;
        let stdin = child.stdin.take().ok_or_else(|| fail("no standard input".to_owned()))?;
        let stdout = child.stdout.take().ok_or_else(|| fail("no standard output".to_owned()))?;
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut input = BufReader::new(stdout);
            loop {
                let message = read_message(&mut input);
                let ended = message.is_none();
                if sender.send(message).is_err() || ended {
                    break;
                }
            }
        });
        let mut server = Running {
            name: name.to_owned(),
            root: root.to_owned(),
            timeout: config.timeout_secs,
            child,
            stdin,
            messages,
            next_id: 0,
        };
        let root_uri = uri_of(root, Path::new(""));
        let root_uri = root_uri.trim_end_matches('/');
        let started = server.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "rootPath": root,
                "workspaceFolders": [{ "uri": root_uri, "name": "kito" }],
                "initializationOptions": config.options.clone().unwrap_or(Value::Null),
                "capabilities": {
                    "general": { "positionEncodings": ["utf-16"] },
                    "textDocument": {
                        "synchronization": { "dynamicRegistration": false },
                        "references": { "dynamicRegistration": false },
                        "definition": { "dynamicRegistration": false, "linkSupport": false }
                    },
                    "workspace": { "workspaceFolders": true, "configuration": true }
                }
            }),
            deadline,
        );
        match started {
            Ok(_) => {
                server.notify("initialized", json!({}))?;
                Ok(server)
            }
            Err(e) => {
                server.kill();
                Err(e)
            }
        }
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let body = message.to_string();
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        self.stdin
            .write_all(framed.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| self.fail(format!("it stopped listening: {e}")))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Sends a request and waits for its answer, answering meanwhile what
    /// the server asks in turn.
    fn request(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = match self.messages.recv_timeout(left) {
                Ok(Some(message)) => message,
                Ok(None) | Err(RecvTimeoutError::Disconnected) => return Err(self.fail("it ended")),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::Timeout { server: self.name.clone(), seconds: self.timeout })
                }
            };
            match (message.get("id"), message.get("method").and_then(Value::as_str)) {
                // A request of the server's: answered with what little a
                // client that only asks questions has to say.
                (Some(their), Some(asked)) => {
                    let result = match asked {
                        "workspace/configuration" => {
                            let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                            Value::Array(vec![Value::Null; items])
                        }
                        "workspace/workspaceFolders" => {
                            json!([{ "uri": uri_of(&self.root, Path::new("")), "name": "kito" }])
                        }
                        _ => Value::Null,
                    };
                    self.send(&json!({ "jsonrpc": "2.0", "id": their, "result": result }))?;
                }
                (Some(theirs), None) if theirs.as_u64() == Some(id) => {
                    if let Some(error) = message.get("error") {
                        let text = error["message"].as_str().unwrap_or("request refused").to_owned();
                        return Err(self.fail(format!("{method}: {text}")));
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                // Notifications, and answers to nothing we are waiting for.
                _ => {}
            }
        }
    }

    fn ask(
        &mut self,
        open: &[(PathBuf, String)],
        questions: &[Question],
        deadline: Instant,
    ) -> Result<Vec<Vec<Place>>> {
        for (file, text) in open {
            self.notify(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri_of(&self.root, file), "languageId": language_id(file), "version": 1, "text": text
                } }),
            )?;
        }
        let mut answers = Vec::with_capacity(questions.len());
        let mut failure = None;
        for question in questions {
            let mut params = json!({
                "textDocument": { "uri": uri_of(&self.root, &question.file) },
                "position": { "line": question.position.0, "character": question.position.1 },
            });
            if question.method == "textDocument/references" {
                params["context"] = json!({ "includeDeclaration": false });
            }
            match self.request(question.method, params, deadline) {
                Ok(result) => answers.push(self.places(&result)),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        // Back to the files on disk, whatever happened.
        for (file, _) in open {
            let closed = self.notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri_of(&self.root, file) } }),
            );
            if failure.is_none() {
                closed?;
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(answers),
        }
    }

    /// The places in an answer: `Location`, `Location[]` or `LocationLink[]`.
    fn places(&self, result: &Value) -> Vec<Place> {
        let one = |value: &Value| -> Option<Place> {
            let uri = value.get("uri").or_else(|| value.get("targetUri"))?.as_str()?;
            let range = value.get("range").or_else(|| value.get("targetSelectionRange"))?;
            let point = |end: &str| -> Option<(u32, u32)> {
                Some((range[end]["line"].as_u64()? as u32, range[end]["character"].as_u64()? as u32))
            };
            Some(Place { file: path_of(&self.root, uri), start: point("start")?, end: point("end")? })
        };
        match result {
            Value::Array(items) => items.iter().filter_map(one).collect(),
            Value::Object(_) => one(result).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    /// Asks the server to end, and makes sure it does.
    fn stop(mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        if self.request("shutdown", Value::Null, deadline).is_ok() {
            let _ = self.notify("exit", Value::Null);
            std::thread::sleep(Duration::from_millis(50));
        }
        self.kill();
    }

    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_uris() {
        let root = Path::new("/work/my repo");
        let uri = uri_of(root, Path::new("src/città/a+b.ts"));
        assert_eq!(uri, "file:///work/my%20repo/src/citt%C3%A0/a%2Bb.ts");
        assert_eq!(path_of(root, &uri), Some(PathBuf::from("src/città/a+b.ts")));
        assert_eq!(path_of(root, "file:///elsewhere/x.ts"), None);
        assert_eq!(path_of(root, "untitled:x"), None);
        assert_eq!(path_of(root, "file:///work/my%2"), None);
    }

    #[test]
    fn messages_are_framed_by_their_length() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let framed = format!("Content-Length: {}\r\nContent-Type: x\r\n\r\n{body}{{", body.len());
        let mut input = BufReader::new(framed.as_bytes());
        assert_eq!(read_message(&mut input).unwrap()["id"], 1);
        assert!(read_message(&mut input).is_none());
    }
}
