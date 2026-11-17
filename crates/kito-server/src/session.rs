//! A client's session: who it is, which repository it is about, and its
//! requests passed on to that repository's daemon.

use crate::state::{Role, Server, User};
use kitod::client::{Client, ClientError, PROTOCOL};
use kitod::rpc::{self, Request, Response};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// What a client may ask of a repository through the server. What stops
/// the daemon, rewrites the main lane or reads files of the server by path
/// stays with whoever runs the server.
const ALLOWED: &[&str] = &[
    "status",
    "log",
    "why",
    "lanes",
    "lane_status",
    "board",
    "who_touches",
    "events",
    "notices",
    "conflicts",
    "open_lane",
    "close_lane",
    "extend_scope",
    "set_intent",
    "rename_symbol",
    "sync",
    "request_integration",
    "integration_result",
    "resolve_conflict",
    "lane_files",
    "get_blobs",
    "put_file",
    "remove_file",
    "claim_writer",
];

/// Of those, what somebody who may only read can ask.
const READ: &[&str] = &[
    "status",
    "log",
    "why",
    "lanes",
    "lane_status",
    "board",
    "who_touches",
    "events",
    "conflicts",
    "integration_result",
    "lane_files",
    "get_blobs",
    // Asks that what is already saved be recorded: no change of theirs.
    "sync",
];

fn send(writer: &mut dyn Write, message: &impl serde::Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

/// Serves one client. `known` is who the client is when something else has
/// already established it (SSH); otherwise its first line must carry a token.
pub fn run(server: &Server, reader: &mut dyn BufRead, writer: &mut dyn Write, known: Option<(String, User)>) {
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let refuse = |writer: &mut dyn Write, reason: &str| {
        let _ = send(writer, &json!({ "error": reason }));
    };
    let hello: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
    let Some(hello) = hello.get("hello") else {
        return refuse(
            writer,
            "say hello first: {\"hello\": {\"version\": 1, \"token\": ..., \"repo\": ...}}",
        );
    };
    if hello.get("version").and_then(Value::as_u64) != Some(PROTOCOL) {
        return refuse(writer, &format!("this server speaks version {PROTOCOL} of the protocol"));
    }
    let Some((name, user)) =
        known.or_else(|| hello.get("token").and_then(Value::as_str).and_then(|token| server.user_of(token)))
    else {
        // The same answer for no token and for a wrong one.
        return refuse(writer, "this token is not one the server knows");
    };
    let repo = match hello.get("repo").and_then(Value::as_str) {
        Some(repo) => match server.repo(repo).filter(|_| user.may_use(repo)) {
            Some(root) => Some((repo.to_owned(), root)),
            // The same answer for one that is not there and one that is not theirs.
            None => {
                return refuse(writer, &format!("there is no repository `{repo}` for {name} on this server"))
            }
        },
        None => None,
    };
    let greeting = json!({ "ok": {
        "user": name,
        "repo": repo.as_ref().map(|(repo, _)| repo),
        "server": env!("CARGO_PKG_VERSION"),
    }});
    if send(writer, &greeting).is_err() {
        return;
    }

    // What the person may do in the repository goes with every request:
    // the repository's daemon applies it.
    let role = repo.as_ref().map(|(repo, _)| user.role_in(repo));
    let person = json!({ "name": name, "email": user.email, "role": role.map(Role::word) });
    // The connection to the repository's daemon, made when first needed.
    let mut daemon: Option<Client> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::err(0, rpc::PARSE_ERROR, format!("invalid request: {e}")),
            Ok(request) => match request.method.as_str() {
                "server.repos" => {
                    let mine: Vec<String> =
                        server.repos().into_iter().filter(|repo| user.may_use(repo)).collect();
                    Response::ok(request.id, json!(mine))
                }
                "server.whoami" => Response::ok(
                    request.id,
                    json!({ "user": name, "email": user.email, "role": role.map(Role::word) }),
                ),
                method if !ALLOWED.contains(&method) => Response::err(
                    request.id,
                    rpc::METHOD_NOT_FOUND,
                    format!("`{method}` is not something a client can ask through the server"),
                ),
                method if role == Some(Role::Read) && !READ.contains(&method) => Response::err(
                    request.id,
                    rpc::REFUSED,
                    format!("{name} may read this repository, and `{method}` changes it"),
                ),
                method => match &repo {
                    None => Response::err(
                        request.id,
                        rpc::INVALID_PARAMS,
                        "this session is about no repository: name one when connecting",
                    ),
                    Some((_, root)) => {
                        // Who asks is what the server says, whatever the client sent.
                        let mut params = match request.params {
                            Value::Object(params) => Value::Object(params),
                            _ => json!({}),
                        };
                        params["on_behalf_of"] = person.clone();
                        forward(&mut daemon, root, request.id, method, params)
                    }
                },
            },
        };
        if send(writer, &response).is_err() {
            return;
        }
    }
}

/// Passes a request on to the repository's daemon, starting it if needed
/// and connecting again once if the connection was lost.
fn forward(
    daemon: &mut Option<Client>,
    root: &std::path::Path,
    id: u64,
    method: &str,
    params: Value,
) -> Response {
    for attempt in 0..2 {
        if daemon.is_none() {
            match Client::connect_or_start(root) {
                Ok(client) => *daemon = Some(client),
                Err(e) => {
                    return Response::err(id, rpc::INTERNAL_ERROR, format!("the repository's daemon: {e}"))
                }
            }
        }
        let Some(client) = daemon.as_mut() else { continue };
        match client.call_value(method, params.clone()) {
            Ok(value) => return Response::ok(id, value),
            Err(ClientError::Rpc(error)) => return Response::err(id, error.code, error.message),
            Err(e) => {
                *daemon = None;
                if attempt == 1 {
                    return Response::err(id, rpc::INTERNAL_ERROR, format!("the repository's daemon: {e}"));
                }
            }
        }
    }
    Response::err(id, rpc::INTERNAL_ERROR, "the repository's daemon did not answer")
}

fn serve_client(server: &Server, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let Ok(read) = stream.try_clone() else { return };
    let mut writer = stream;
    run(server, &mut BufReader::new(read), &mut writer, None);
}

/// Listens for clients, each served on a thread of its own.
pub fn serve(server: Server, listen: Option<&str>) -> Result<(), String> {
    let address = listen.unwrap_or(&server.config.listen).to_owned();
    let listener = TcpListener::bind(&address).map_err(|e| format!("{address}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    // Said on standard output, flushed: whoever started the server reads
    // the port from it when it asked for any free one.
    println!("kito-server: listening on {bound}, {} repositories", server.repos().len());
    let _ = std::io::stdout().flush();
    let server = Arc::new(server);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let server = server.clone();
        std::thread::spawn(move || {
            // People and repositories added since the server started
            // count for a connection made now.
            match Server::open(&server.dir) {
                Ok(now) => serve_client(&now, stream),
                Err(_) => serve_client(&server, stream),
            }
        });
    }
    Ok(())
}
