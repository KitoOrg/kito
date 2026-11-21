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
    "why_graph",
    "reasons",
    "before_edit",
    "reasons_behind",
    "after_shell",
    // What the session of an assistant runs on (ADR 0010).
    "assistant_session",
    // What agents are told (ADR 0012).
    "instructions",
    "instructions_set",
    "instructions_remove",
    // Notes on code (ADR 0003).
    "add_note",
    "notes",
    "remove_note",
    // Review of a lane (ADR 0004).
    "lane_diff",
    "comment",
    "comments",
    "resolve_comment",
    "verdict",
    "review",
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
    "why_graph",
    "reasons",
    "notes",
    "lane_diff",
    "comments",
    "review",
    // Asked before an edit and after a command: the answer changes nothing
    // of the repository.
    "before_edit",
    "after_shell",
    "assistant_session",
    "instructions",
    // Asks that what is already saved be recorded: no change of theirs.
    "sync",
];

/// What only a maintainer of the repository may ask (ADR 0014).
const MAINTAIN: &[&str] = &["protect", "configure"];

/// The code of "there is no such repository for this person".
pub const NO_SUCH_REPO: i64 = -32023;

/// Asks a repository's daemon one thing as a person: what their role
/// allows, and, with `within`, of that only what needs one of these
/// permissions (the pages of a server ask within what their plugin was
/// given). The answer is the daemon's.
pub fn ask(
    server: &Server,
    name: &str,
    user: &User,
    repo: &str,
    method: &str,
    params: Value,
    within: Option<&[kitod::plugins::Permission]>,
) -> Result<Value, (i64, String)> {
    // The same answer for one that is not there and one that is not theirs.
    let root = server.repo(repo).filter(|_| user.may_use(repo)).ok_or_else(|| {
        (NO_SUCH_REPO, format!("there is no repository `{repo}` for {name} on this server"))
    })?;
    let role = user.role_in(repo).unwrap_or(Role::Read);
    if MAINTAIN.contains(&method) && role < Role::Maintain {
        return Err((
            rpc::REFUSED,
            format!("only a maintainer of this repository may `{method}`: {name} may {}", role.word()),
        ));
    }
    if !ALLOWED.contains(&method) && !MAINTAIN.contains(&method) {
        return Err((
            rpc::METHOD_NOT_FOUND,
            format!("`{method}` is not something a client can ask through the server"),
        ));
    }
    if role == Role::Read && !READ.contains(&method) {
        return Err((rpc::REFUSED, format!("{name} may read this repository, and `{method}` changes it")));
    }
    if let (Some(given), Some(needed)) = (within, kitod::plugins::permission_of(method)) {
        if !given.contains(&needed) {
            return Err((
                rpc::REFUSED,
                format!(
                    "the pages of this server may not ask `{method}`: no plugin that serves them was given `{}`",
                    needed.word()
                ),
            ));
        }
    }
    let mut params = match params {
        Value::Object(params) => Value::Object(params),
        _ => json!({}),
    };
    // Who asks is what the server says, whatever the client sent.
    params["on_behalf_of"] = json!({ "name": name, "email": user.email, "role": role.word() });
    let mut daemon = None;
    let answer = forward(&mut daemon, &root, 0, method, params);
    match (answer.result, answer.error) {
        (_, Some(error)) => Err((error.code, error.message)),
        (Some(value), None) => Ok(value),
        (None, None) => Ok(Value::Null),
    }
}

fn send(writer: &mut dyn Write, message: &impl serde::Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

/// How long somebody has to sign in, from when they started.
const SIGN_IN_WITHIN: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Signs somebody in for a command line (ADR 0006): the plugin says where
/// to go and what to enter there, the client asks (`{"poll": true}`)
/// until the person has done it, and the answer is the token of a
/// session. Who they are is the plugin's word; whether that is anybody
/// here is the server's. What the plugin keeps between the two steps
/// stays on this side.
fn sign_in(server: &Server, asked: &Value, reader: &mut dyn BufRead, writer: &mut dyn Write) {
    use std::time::{Duration, Instant};
    let refuse = |writer: &mut dyn Write, reason: &str| {
        let _ = send(writer, &json!({ "error": reason }));
    };
    if asked.get("version").and_then(Value::as_u64) != Some(PROTOCOL) {
        return refuse(writer, &format!("this server speaks version {PROTOCOL} of the protocol"));
    }
    let Some(method) = asked.get("method").and_then(Value::as_str) else {
        // No way named: the ways there are.
        let methods: Vec<Value> = server
            .sign_in_methods()
            .into_iter()
            .map(|(method, about)| json!({ "method": method, "about": about }))
            .collect();
        let _ = send(writer, &json!({ "methods": methods }));
        return;
    };
    let begun = match server.sign_in_step(method, &["begin"], &json!({ "kind": "device" })) {
        Ok(begun) => begun,
        Err(e) => return refuse(writer, &e),
    };
    if let Some(why) = begun["refused"].as_str() {
        return refuse(writer, why);
    }
    let interval = begun["interval"].as_u64().unwrap_or(5).min(60);
    let told = json!({ "sign_in": {
        "method": method,
        "open": begun["open"],
        "code": begun["code"],
        "interval": interval,
    }});
    if send(writer, &told).is_err() {
        return;
    }
    let started = Instant::now();
    let mut asked_at: Option<Instant> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if started.elapsed() > SIGN_IN_WITHIN {
            return refuse(writer, "signing in took too long: start again");
        }
        // The plugin is asked no oftener than it said, whatever the client does.
        if let Some(left) = asked_at.and_then(|at| Duration::from_secs(interval).checked_sub(at.elapsed())) {
            std::thread::sleep(left);
        }
        asked_at = Some(Instant::now());
        let finish = json!({ "kind": "device", "state": begun["state"] });
        let answer = match server.sign_in_step(method, &["finish"], &finish) {
            Ok(answer) => answer,
            Err(e) => return refuse(writer, &e),
        };
        if let Some(wait) = answer["wait"].as_u64() {
            if send(writer, &json!({ "wait": wait.min(60) })).is_err() {
                return;
            }
            continue;
        }
        let identity = match crate::state::identity_of(method, &answer) {
            Ok(identity) => identity,
            Err(why) => return refuse(writer, &why),
        };
        // People added while this one was signing in count.
        let now = Server::open(&server.dir);
        let server = now.as_ref().unwrap_or(server);
        let opened = server.signed_in(&identity).and_then(|name| Ok((server.open_session(&name)?, name)));
        return match opened {
            Ok((token, name)) => {
                let days = server.config.sign_in.days;
                let _ = send(writer, &json!({ "ok": { "token": token, "user": name, "days": days } }));
            }
            Err(why) => refuse(writer, &why),
        };
    }
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
    if let Some(asked) = hello.get("sign_in") {
        return sign_in(server, asked, reader, writer);
    }
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
        // The same answer for no token and for a wrong one; a session
        // says what it is by how it starts, and how to get another.
        let session = hello.get("token").and_then(Value::as_str);
        return match session.is_some_and(|token| token.starts_with(crate::state::SESSION_PREFIX)) {
            true => refuse(
                writer,
                "this session ended, or is not one the server knows: sign in again (`kito sign-in`)",
            ),
            false => refuse(writer, "this token is not one the server knows"),
        };
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
    let role = repo.as_ref().and_then(|(repo, _)| user.role_in(repo));
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
                // The plugins that run in the directories connected here (ADR 0007).
                "server.client_plugins" => Response::ok(request.id, json!(server.client_plugins())),
                // A plugin on a client's machine asks the plugin of the same
                // name here: answered by the server, not by a repository.
                "plugin_call" => {
                    let text = |key: &str| request.params.get(key).and_then(Value::as_str);
                    let who =
                        (name.as_str(), repo.as_ref().map(|(repo, _)| repo.as_str()), role.map(Role::word));
                    let answer = match (text("plugin"), text("ask"), who) {
                        (Some(plugin), Some(ask), (user, Some(repo), Some(role))) => {
                            // People and plugins changed since this client connected count.
                            let now = Server::open(&server.dir);
                            let server = now.as_ref().unwrap_or(server);
                            let params = request.params.get("params").cloned().unwrap_or(Value::Null);
                            server
                                .answer(plugin, ask, &params, (user, repo, role))
                                .map_err(|why| (rpc::REFUSED, why))
                        }
                        (_, _, (_, None, _)) | (_, _, (_, _, None)) => {
                            Err((rpc::INVALID_PARAMS, "this session is about no repository".to_owned()))
                        }
                        _ => Err((rpc::INVALID_PARAMS, "`plugin_call` wants `plugin` and `ask`".to_owned())),
                    };
                    match answer {
                        Ok(value) => Response::ok(request.id, value),
                        Err((code, why)) => Response::err(request.id, code, why),
                    }
                }
                // Who may use the server's repositories, and as what (ADR 0014).
                method if method.starts_with("server.") && PEOPLE.contains(&method) => {
                    let here = repo.as_ref().map(|(repo, _)| repo.as_str());
                    match people(server, &name, &user, here, role, method, &request.params) {
                        Ok(value) => Response::ok(request.id, value),
                        Err((code, why)) => Response::err(request.id, code, why),
                    }
                }
                method if MAINTAIN.contains(&method) && role.is_some_and(|role| role < Role::Maintain) => {
                    Response::err(
                        request.id,
                        rpc::REFUSED,
                        format!(
                            "only a maintainer of this repository may `{method}`: {name} may {}",
                            role.map(Role::word).unwrap_or("read")
                        ),
                    )
                }
                method if !ALLOWED.contains(&method) && !MAINTAIN.contains(&method) => Response::err(
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

/// The server's people, asked from a connected directory.
const PEOPLE: &[&str] = &[
    "server.members",
    "server.grant",
    "server.people",
    "server.people_add",
    "server.people_remove",
    "server.people_role",
];

/// Answers one of `PEOPLE`, as `name` with what they may do: `here` the
/// repository of the session, `role` theirs in it.
fn people(
    server: &Server,
    name: &str,
    user: &User,
    here: Option<&str>,
    role: Option<Role>,
    method: &str,
    params: &Value,
) -> Result<Value, (i64, String)> {
    use crate::state::ServerRole;
    let refused = |why: String| (rpc::REFUSED, why);
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((rpc::INVALID_PARAMS, format!("missing parameter `{key}`")))
    };
    // What changed since this client connected counts.
    let mut now = Server::open(&server.dir).map_err(|e| (rpc::INTERNAL_ERROR, e))?;
    let admin = user.is_admin();
    let owner = user.server_role == ServerRole::Owner;
    match method {
        "server.members" => {
            let repo = here.ok_or((rpc::INVALID_PARAMS, "this session is about no repository".to_owned()))?;
            let members: Vec<Value> = now
                .members(repo)
                .into_iter()
                .map(|(person, role, by_server)| json!({ "user": person, "role": role.word(), "admin": by_server }))
                .collect();
            Ok(json!({ "repo": repo, "members": members }))
        }
        "server.grant" => {
            let repo = here.ok_or((rpc::INVALID_PARAMS, "this session is about no repository".to_owned()))?;
            if role < Some(Role::Maintain) {
                return Err(refused(format!(
                    "only a maintainer of `{repo}` may say who uses it: {name} may {}",
                    role.map(Role::word).unwrap_or("nothing")
                )));
            }
            let person = text("user")?;
            let given = match params.get("role").and_then(Value::as_str) {
                Some(word) => Some(Role::of_word(word).map_err(|why| (rpc::INVALID_PARAMS, why))?),
                None => None,
            };
            let changed = now.grant(person, repo, given).map_err(refused)?;
            let what = match given {
                Some(given) => format!("{person} may {} in {repo}", given.word()),
                None => format!("{person} may no longer use {repo}"),
            };
            if changed {
                now.tell("role_changed", json!({ "user": person, "repo": repo, "role": given.map(Role::word), "by": name, "what": what }));
            }
            Ok(json!({ "user": person, "repo": repo, "role": given.map(Role::word), "changed": changed }))
        }
        _ if !admin => {
            Err(refused(format!("only an admin of this server may manage its people: {name} is a member")))
        }
        "server.people" => {
            let people: Vec<Value> = now
                .config
                .users
                .iter()
                .map(|(person, them)| {
                    let access: serde_json::Map<String, Value> =
                        them.access.iter().map(|(repo, role)| (repo.clone(), json!(role.word()))).collect();
                    json!({ "user": person, "email": them.email, "role": them.server_role.word(), "access": access, "logins": them.logins })
                })
                .collect();
            Ok(json!(people))
        }
        "server.people_add" => {
            let person = text("user")?;
            let given = match params.get("role").and_then(Value::as_str) {
                Some(word) => ServerRole::of_word(word).map_err(|why| (rpc::INVALID_PARAMS, why))?,
                None => ServerRole::Member,
            };
            if given >= ServerRole::Admin && !owner {
                return Err(refused("only an owner of this server names its admins and owners".to_owned()));
            }
            let identity = match params.get("identity").and_then(Value::as_str) {
                Some(identity) => Some(now.look_up(identity).map_err(refused)?),
                None => None,
            };
            let email = params.get("email").and_then(Value::as_str).map(str::to_owned);
            let token = now.add_user(person, email, &[], given, identity.as_ref()).map_err(refused)?;
            now.tell("user_added", json!({ "user": person, "role": given.word(), "by": name, "what": format!("{person} ({})", given.word()) }));
            Ok(
                json!({ "user": person, "role": given.word(), "token": token, "fingerprint": crate::tls::fingerprint(&now.dir).ok() }),
            )
        }
        "server.people_remove" | "server.people_role" => {
            let person = text("user")?;
            let theirs = now.config.users.get(person).map(|them| them.server_role);
            let theirs = theirs.ok_or_else(|| refused(format!("{person} is not a user of this server")))?;
            if theirs >= ServerRole::Admin && !owner {
                return Err(refused(format!("{person} is an {}: only an owner changes them", theirs.word())));
            }
            if method == "server.people_remove" {
                now.remove_user(person).map_err(refused)?;
                now.tell("user_removed", json!({ "user": person, "by": name, "what": person }));
                return Ok(json!({ "user": person, "removed": true }));
            }
            let given = ServerRole::of_word(text("role")?).map_err(|why| (rpc::INVALID_PARAMS, why))?;
            if given >= ServerRole::Admin && !owner {
                return Err(refused("only an owner of this server names its admins and owners".to_owned()));
            }
            now.set_server_role(person, given).map_err(refused)?;
            now.tell("role_changed", json!({ "user": person, "server_role": given.word(), "by": name, "what": format!("{person}: {}", given.word()) }));
            Ok(json!({ "user": person, "role": given.word() }))
        }
        _ => Err((
            rpc::METHOD_NOT_FOUND,
            format!("`{method}` is not something a client can ask through the server"),
        )),
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

fn serve_client(server: &Server, stream: TcpStream, tls: Option<Arc<rustls::ServerConfig>>) {
    let _ = stream.set_nodelay(true);
    match tls {
        // Encrypted: the handshake is made with the first bytes read.
        Some(config) => {
            let Ok(encrypted) = crate::tls::Encrypted::new(config, stream) else { return };
            let mut writer = encrypted.clone();
            run(server, &mut BufReader::new(encrypted), &mut writer, None);
        }
        None => {
            let Ok(read) = stream.try_clone() else { return };
            let mut writer = stream;
            run(server, &mut BufReader::new(read), &mut writer, None);
        }
    }
}

/// Listens for clients, each served on a thread of its own. Connections
/// are encrypted with the server's certificate; `plain`: they are not,
/// and the server listens on this machine only.
pub fn serve(server: Server, listen: Option<&str>, plain: bool) -> Result<(), String> {
    let address = listen.unwrap_or(&server.config.listen).to_owned();
    let listener = TcpListener::bind(&address).map_err(|e| format!("{address}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    if plain && !bound.ip().is_loopback() {
        return Err(format!(
            "{bound} is reachable from other machines: a server that does not encrypt (--plain) \
             listens on this machine only"
        ));
    }
    let tls = match plain {
        true => None,
        false => Some(crate::tls::config(&server.dir)?),
    };
    // Said on standard output in one line, flushed: whoever started the
    // server reads the port from it when it asked for any free one, and
    // may stop listening right after.
    let how = match plain {
        true => "NOT encrypted (--plain)".to_owned(),
        false => format!("encrypted (certificate {})", crate::tls::fingerprint(&server.dir)?),
    };
    let mut out = std::io::stdout();
    let _ = writeln!(out, "kito-server: listening on {bound}, {} repositories, {how}", server.repos().len());
    let _ = out.flush();
    let server = Arc::new(server);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (server, tls) = (server.clone(), tls.clone());
        std::thread::spawn(move || {
            // People and repositories added since the server started
            // count for a connection made now.
            match Server::open(&server.dir) {
                Ok(now) => serve_client(&now, stream, tls),
                Err(_) => serve_client(&server, stream, tls),
            }
        });
    }
    Ok(())
}
