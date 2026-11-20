//! A language server for the tests: it knows nothing of any language. A
//! "reference" is the same word written somewhere in the files it can see
//! (the open documents first, then the `.ts` files under the root), a
//! "definition" is the first `function <word>`.
//!
//! What it does on a question is read from `.fake-lsp-mode` under the root:
//! `hang` (it never answers) or `crash` (it exits). Every message it gets is
//! appended to `.fake-lsp-log`.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

fn send(message: &Value) {
    let body = message.to_string();
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = out.flush();
}

fn read(input: &mut impl BufRead) -> Option<Value> {
    let mut length = 0;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn path_of(uri: &str) -> PathBuf {
    let mut bytes = Vec::new();
    let raw = uri.trim_start_matches("file://").as_bytes();
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' && at + 2 < raw.len() + 1 {
            let hex = std::str::from_utf8(&raw[at + 1..at + 3]).unwrap_or("20");
            bytes.push(u8::from_str_radix(hex, 16).unwrap_or(b' '));
            at += 3;
        } else {
            bytes.push(raw[at]);
            at += 1;
        }
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // On Windows a URI's path is `/C:/repo/a.ts`: the path is `C:\repo\a.ts`.
    if cfg!(windows) && text.as_bytes().get(2) == Some(&b':') {
        return PathBuf::from(text[1..].replace('/', "\\"));
    }
    PathBuf::from(text)
}

/// The URI of a path, as `path_of` reads it back.
fn uri_of(path: &Path) -> String {
    let text = path.display().to_string();
    match cfg!(windows) {
        true => format!("file:///{}", text.strip_prefix(r"\\?\").unwrap_or(&text).replace('\\', "/")),
        false => format!("file://{text}"),
    }
}

fn on_disk(dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            on_disk(&path, out);
        } else if path.extension().is_some_and(|e| e == "ts") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                out.entry(path).or_insert(text);
            }
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every place `word` is written as a whole word: (line, column).
fn occurrences(text: &str, word: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (line, content) in text.lines().enumerate() {
        let mut from = 0;
        while let Some(at) = content[from..].find(word) {
            let (start, end) = (from + at, from + at + word.len());
            let before = content[..start].chars().next_back().is_some_and(is_word);
            let after = content[end..].chars().next().is_some_and(is_word);
            if !before && !after {
                found.push((line, content[..start].encode_utf16().count()));
            }
            from = end;
        }
    }
    found
}

pub fn main() {
    let mut input = std::io::stdin().lock();
    let mut open: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut root = PathBuf::new();
    while let Some(message) = read(&mut input) {
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let id = message.get("id").cloned();
        let params = &message["params"];
        if !root.as_os_str().is_empty() {
            if let Ok(mut log) =
                std::fs::OpenOptions::new().create(true).append(true).open(root.join(".fake-lsp-log"))
            {
                let about = params["textDocument"]["uri"].as_str().map(path_of);
                let about = about
                    .as_ref()
                    .and_then(|p| p.strip_prefix(&root).ok())
                    .map(|p| p.display().to_string().replace('\\', "/"));
                let _ = writeln!(log, "{method} {}", about.unwrap_or_default());
            }
        }
        let mode = std::fs::read_to_string(root.join(".fake-lsp-mode")).unwrap_or_default();
        match method.as_str() {
            "initialize" => {
                root = path_of(params["rootUri"].as_str().unwrap_or_default());
                // A server asks things too: the client must answer and go on.
                send(
                    &json!({ "jsonrpc": "2.0", "id": "ask-1", "method": "workspace/configuration", "params": { "items": [{}, {}] } }),
                );
                send(
                    &json!({ "jsonrpc": "2.0", "method": "window/logMessage", "params": { "type": 3, "message": "hello" } }),
                );
                send(&json!({ "jsonrpc": "2.0", "id": id, "result": { "capabilities": {} } }));
            }
            "textDocument/didOpen" => {
                let doc = &params["textDocument"];
                open.insert(
                    path_of(doc["uri"].as_str().unwrap_or_default()),
                    doc["text"].as_str().unwrap_or_default().to_owned(),
                );
            }
            "textDocument/didClose" => {
                open.remove(&path_of(params["textDocument"]["uri"].as_str().unwrap_or_default()));
            }
            "textDocument/references" | "textDocument/definition" => {
                match mode.trim() {
                    "hang" => continue,
                    "crash" => std::process::exit(3),
                    _ => {}
                }
                let file = path_of(params["textDocument"]["uri"].as_str().unwrap_or_default());
                let mut files = open.clone();
                on_disk(&root, &mut files);
                let (line, column) = (
                    params["position"]["line"].as_u64().unwrap_or(0) as usize,
                    params["position"]["character"].as_u64().unwrap_or(0) as usize,
                );
                let text = files.get(&file).cloned().unwrap_or_default();
                let on_line: Vec<char> = text.lines().nth(line).unwrap_or_default().chars().collect();
                let word: String = on_line.iter().skip(column).take_while(|c| is_word(**c)).collect();
                let mut places = Vec::new();
                for (path, text) in &files {
                    for (at_line, at_column) in occurrences(text, &word) {
                        let declaration = *path == file && (at_line, at_column) == (line, column);
                        let wanted = match method.as_str() {
                            "textDocument/references" => !declaration,
                            _ => text
                                .lines()
                                .nth(at_line)
                                .is_some_and(|l| l.contains(&format!("function {word}"))),
                        };
                        if wanted && !word.is_empty() {
                            places.push(json!({
                                "uri": uri_of(path),
                                "range": {
                                    "start": { "line": at_line, "character": at_column },
                                    "end": { "line": at_line, "character": at_column + word.encode_utf16().count() }
                                }
                            }));
                        }
                    }
                }
                send(&json!({ "jsonrpc": "2.0", "id": id, "result": places }));
            }
            "shutdown" => send(&json!({ "jsonrpc": "2.0", "id": id, "result": null })),
            "exit" => return,
            _ => {
                if let Some(id) = id.filter(|_| !method.is_empty()) {
                    send(&json!({ "jsonrpc": "2.0", "id": id, "result": null }));
                }
            }
        }
    }
}
