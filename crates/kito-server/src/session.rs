//! A client's session: who it is, which repository it is about, and its
//! requests passed on to that repository's daemon.

use crate::state::{hash_of, FirstFiles, Role, Server, User};
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
    // Search and numbers (ADR 0016).
    "search",
    "stats",
    // Tasks (ADR 0015).
    "tasks",
    "task",
    "task_add",
    "task_update",
    "task_claim",
    "task_comment",
    "task_done",
    "task_reopen",
    "task_drop",
    // What agents are told (ADR 0012).
    "instructions",
    "instructions_text",
    "instructions_history",
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
    "submit_review",
    "apply_suggestion",
    "changes_since_review",
    // The git remote now (ADR 0020).
    "git_sync",
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
    "changes_since_review",
    // Asked before an edit and after a command: the answer changes nothing
    // of the repository.
    "before_edit",
    "after_shell",
    "assistant_session",
    "instructions",
    "instructions_text",
    "instructions_history",
    "tasks",
    "task",
    "search",
    "stats",
    // Asks that what is already saved be recorded: no change of theirs.
    "sync",
];

/// What only a maintainer of the repository may ask (ADR 0014).
const MAINTAIN: &[&str] = &["protect", "configure"];

/// The code of "there is no such repository for this person".
pub const NO_SUCH_REPO: i64 = -32023;

/// What a connection was opened with, or a request came with, kept as
/// what it hashes to. It is looked at again before each request, and
/// where a change is made: a token that is no longer the person's, and a
/// session that ended, stop there (ADR 0006, amended).
#[derive(Clone)]
pub enum Came {
    /// With a person's token.
    Token(String),
    /// With the token of a session.
    Session(String),
    /// With neither: something else said who they are (SSH, or the
    /// secret of a plugin that asks as itself).
    Otherwise,
}

impl Came {
    /// What somebody came with, by the token they gave.
    pub fn with(token: &str) -> Came {
        match token.starts_with(crate::state::SESSION_PREFIX) {
            true => Came::Session(hash_of(token)),
            false => Came::Token(hash_of(token)),
        }
    }

    /// Why what they came with is no longer `name`'s, by the server as it
    /// is; `None`: it is theirs still.
    pub fn no_longer(&self, now: &Server, name: &str) -> Option<String> {
        let Some(them) = now.config.users.get(name) else {
            return Some(format!("{name} is no longer a user of this server"));
        };
        match self {
            Came::Token(hash) if them.token != *hash => Some(match them.has_token() {
                true => format!("{name}'s token was changed: connect again with the new one"),
                false => format!("{name} has no token any more: sign in instead (`kito sign-in <method>`)"),
            }),
            Came::Session(hash) if now.session_user(hash).as_deref() != Some(name) => {
                Some("this session ended: sign in again (`kito sign-in`)".to_owned())
            }
            _ => None,
        }
    }
}

/// Whether somebody may still ask for a change, by the server as it is
/// when the change is made (`Server::only_while`): what they came with is
/// theirs still, and they are at least `least` on the server. A request
/// that took its time, cloning or asking a plugin, changes nothing for
/// somebody who was removed or made less meanwhile.
pub fn still(
    name: &str,
    came: &Came,
    least: crate::state::ServerRole,
) -> impl Fn(&Server) -> Result<(), String> + Send + Sync + 'static {
    let (name, came) = (name.to_owned(), came.clone());
    move |now: &Server| {
        if let Some(why) = came.no_longer(now, &name) {
            return Err(why);
        }
        let role = now.config.users.get(&name).map(|them| them.server_role).unwrap_or_default();
        match role >= least {
            true => Ok(()),
            false => {
                Err(format!("{name} is no longer an {} of this server: nothing was changed", least.word()))
            }
        }
    }
}

/// Who asks: their name, what the server had of them when the request
/// began, and what the request came with.
pub struct Asker<'a> {
    pub name: &'a str,
    pub user: &'a User,
    pub came: &'a Came,
}

impl Asker<'_> {
    /// The session the request came through, as what its token hashes to;
    /// none: it came with a person's token, or through SSH.
    fn session(&self) -> Option<&str> {
        match self.came {
            Came::Session(hash) => Some(hash),
            _ => None,
        }
    }

    /// What whoever asks is on the server by the configuration as it was
    /// just read: an admin, an owner. Asked where a change is made, with
    /// the lock held: what the request came with may have been taken away
    /// since it began, and what they are may have changed.
    fn standing(&self, now: &Server) -> Result<(bool, bool), No> {
        use crate::state::ServerRole;
        if let Some(why) = self.came.no_longer(now, self.name) {
            return Err(No(rpc::REFUSED, why));
        }
        let them = now.config.users.get(self.name);
        Ok((them.is_some_and(User::is_admin), them.is_some_and(|them| them.server_role == ServerRole::Owner)))
    }
}

/// Why a request is not answered: its code, and what is wrong. What the
/// server's state refuses (a `String`) is a refusal.
pub struct No(i64, String);

impl From<String> for No {
    fn from(why: String) -> No {
        No(rpc::REFUSED, why)
    }
}

impl From<No> for (i64, String) {
    fn from(no: No) -> (i64, String) {
        (no.0, no.1)
    }
}

/// Asks a repository's daemon one thing as a person: what their role
/// allows, and, with `within`, of that only what needs one of these
/// permissions (the pages of a server ask within what their plugin was
/// given). The answer is the daemon's.
pub fn ask(
    server: &Server,
    who: &Asker,
    repo: &str,
    method: &str,
    params: Value,
    within: Option<&[kitod::plugins::Permission]>,
) -> Result<Value, (i64, String)> {
    let (name, user) = (who.name, who.user);
    // The same answer for one that is not there and one that is not theirs.
    let resolved = server.resolve(repo);
    let repo = resolved.as_str();
    let root = server.repo(repo).filter(|_| user.may_use(repo)).ok_or_else(|| {
        (NO_SUCH_REPO, format!("there is no repository `{repo}` for {name} on this server"))
    })?;
    let role = user.role_in(repo).unwrap_or(Role::Read);
    if PEOPLE.contains(&method) {
        return people(server, who, Some(repo), Some(role), method, &params);
    }
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
    settings_allowed(server, user, repo, method, &params)?;
    let asked = params.clone();
    // Who asks is what the server says, whatever the client sent.
    params["on_behalf_of"] = json!({ "name": name, "email": user.email, "role": role.word() });
    let mut daemon = None;
    let answer = forward(&mut daemon, &root, 0, method, params);
    if answer.error.is_none() {
        note_settings(server, name, repo, method, &asked);
    }
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
        // People added while this one was signing in count; and nobody
        // is signed in on what was read before, when it cannot be read.
        let Ok(now) = Server::open(&server.dir) else { return refuse(writer, crate::state::UNREADABLE) };
        let server = &now;
        let opened = server.signed_in(&identity).and_then(|name| {
            // Theirs still when the session is written: not unlinked meanwhile.
            let with = (identity.written(), None);
            let token =
                server.open_session(&name, (&with.0, with.1), |now| now.signs_in_as(&identity, &name))?;
            token.map(|token| (token, name)).ok_or_else(|| Server::nobody(&identity))
        });
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
    // The server as it is now: people and repositories added since it
    // started count for a connection made now. When what is on the disk
    // cannot be read, nobody is let in on what was read before.
    let Ok(now) = Server::open(&server.dir) else { return refuse(writer, crate::state::UNREADABLE) };
    let server = &now;
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
    // The token counts when nothing else said who they are.
    let token = hello.get("token").and_then(Value::as_str).filter(|_| known.is_none());
    let mut came = token.map_or(Came::Otherwise, Came::with);
    let Some((name, mut user)) = known.or_else(|| token.and_then(|token| server.user_of(token))) else {
        // The same answer for no token and for a wrong one; a session
        // says what it is by how it starts, and how to get another.
        let session = hello.get("token").and_then(Value::as_str);
        return match session.is_some_and(|token| token.starts_with(crate::state::SESSION_PREFIX)) {
            true => refuse(
                writer,
                "this session ended, or is not one the server knows: sign in again (`kito sign-in`)",
            ),
            false => {
                crate::audit::record(
                    &server.dir,
                    "token_refused",
                    Some("somebody"),
                    None,
                    "a client with a token the server does not know",
                );
                refuse(writer, "this token is not one the server knows")
            }
        };
    };
    let asked_repo = hello.get("repo").and_then(Value::as_str).map(|repo| server.resolve(repo));
    let repo = match asked_repo.as_deref() {
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
    server.saw(&name);
    let mut role = repo.as_ref().and_then(|(repo, _)| user.role_in(repo));
    let mut person = json!({ "name": name, "email": user.email, "role": role.map(Role::word) });
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
        // What the person may do now, not when they connected: one
        // removed, or whose repository was taken away, is stopped at once,
        // and so is a connection opened with what is no longer theirs.
        // What cannot be checked is refused: no request is answered on
        // what was read before.
        let Ok(now) = Server::open(&server.dir) else { return refuse(writer, crate::state::UNREADABLE) };
        let server = &now;
        if let Some(why) = came.no_longer(server, &name) {
            return refuse(writer, &why);
        }
        if let Some(them) = server.config.users.get(&name) {
            user = them.clone();
        }
        if let Some((repo, _)) = &repo {
            role = user.role_in(repo);
            if role.is_none() {
                return refuse(writer, &format!("{name} may no longer use `{repo}`"));
            }
        }
        person = json!({ "name": name, "email": user.email, "role": role.map(Role::word) });
        server.saw(&name);
        let parsed = serde_json::from_str::<Request>(&line);
        let asked = parsed.as_ref().map(|request| request.method.clone()).unwrap_or_default();
        let response = match parsed {
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
                    let who = Asker { name: &name, user: &user, came: &came };
                    match people(server, &who, here, role, method, &request.params) {
                        Ok(value) => {
                            // Their own new token: the connection that
                            // asked for it goes on, opened with it now.
                            let own = method == "server.people_token" && value["user"] == name.as_str();
                            if let (Came::Token(hash), true, Some(new)) =
                                (&mut came, own, value["token"].as_str())
                            {
                                *hash = hash_of(new);
                            }
                            Response::ok(request.id, value)
                        }
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
                    Some((repo, root)) => {
                        // Who asks is what the server says, whatever the client sent.
                        let mut params = match request.params {
                            Value::Object(params) => Value::Object(params),
                            _ => json!({}),
                        };
                        match settings_allowed(server, &user, repo, method, &params) {
                            Err((code, why)) => Response::err(request.id, code, why),
                            Ok(()) => {
                                let asked = params.clone();
                                params["on_behalf_of"] = person.clone();
                                let answer = forward(&mut daemon, root, request.id, method, params);
                                if answer.error.is_none() {
                                    note_settings(server, &name, repo, method, &asked);
                                }
                                answer
                            }
                        }
                    }
                },
            },
        };
        // Every request is kept, for 90 days (ADR 0017).
        crate::audit::request(
            &server.dir,
            &name,
            repo.as_ref().map(|(repo, _)| repo.as_str()),
            &asked,
            response.error.is_none(),
        );
        if send(writer, &response).is_err() {
            return;
        }
    }
}

/// What a maintainer may not change from a client, and where a kitchen
/// may be (ADR 0014, amended): checks run commands on the server's
/// machine, for its admins to say; a kitchen is another repository here.
fn settings_allowed(
    server: &Server,
    user: &User,
    repo: &str,
    method: &str,
    params: &Value,
) -> Result<(), (i64, String)> {
    if method != "configure" {
        return Ok(());
    }
    if params.get("checks").is_some() && !user.is_admin() {
        return Err((
            rpc::REFUSED,
            "the checks run commands on the server's machine: only its admins change them".to_owned(),
        ));
    }
    match params.get("kitchen") {
        None | Some(Value::Null) => Ok(()),
        Some(kitchen) => {
            let name = kitchen.as_str().and_then(|path| path.strip_prefix("../")).unwrap_or_default();
            match !name.is_empty() && name != repo && server.repo(name).is_some() {
                true => Ok(()),
                false => Err((
                    rpc::REFUSED,
                    format!("a kitchen is another repository of this server, given as `../<name>`: {kitchen} is not"),
                )),
            }
        }
    }
}

/// Keeps who changed which setting of a repository, once it is changed.
fn note_settings(server: &Server, name: &str, repo: &str, method: &str, params: &Value) {
    match method {
        "configure" => {
            for setting in ["checks", "push", "approvals", "kitchen"] {
                if let Some(value) = params.get(setting) {
                    server.note_setting(repo, name, setting, value);
                }
            }
        }
        "protect" => {
            let on = params.get("on").and_then(Value::as_bool).unwrap_or(true);
            server.note_setting(repo, name, "protected", &json!(on));
        }
        _ => {}
    }
}

/// Why a plugin that asks as itself (ADR 0020) may not ask `method`, when
/// it may not. What is asked of the server itself (its people, its
/// repositories, its log, its backups) is a person's to ask: a plugin is
/// given things in a repository, and is nobody on the server. Who uses the
/// repository, and what was changed in its settings, it may know when it
/// was given `admin` there.
pub fn not_for_a_plugin(method: &str, given: &[kitod::plugins::Permission]) -> Option<String> {
    if !PEOPLE.contains(&method) {
        return None;
    }
    let of_the_repository = ["server.members", "server.settings_history"].contains(&method);
    if of_the_repository && given.contains(&kitod::plugins::Permission::Admin) {
        return None;
    }
    Some(match of_the_repository {
        true => format!("a plugin may ask `{method}` only when it was given `admin`"),
        false => format!("`{method}` is for a person: a plugin asks of a repository what it was given there"),
    })
}

const PEOPLE: &[&str] = &[
    "server.repos_all",
    "server.repo_add",
    "server.repo_upload_begin",
    "server.repo_upload_chunk",
    "server.repo_upload_end",
    "server.repo_archive",
    "server.repo_unarchive",
    "server.repo_delete",
    "server.repo_rename",
    "server.audit",
    "server.audit_verify",
    "server.backups",
    "server.backup_now",
    "server.settings_history",
    "server.members",
    "server.grant",
    "server.people",
    "server.people_add",
    "server.people_remove",
    "server.people_role",
    // How a person signs in (ADR 0006, amended).
    "server.people_link",
    "server.people_unlink",
    "server.people_token",
];

/// How a client connects to this server: the port it listens on, whether
/// it encrypts, and the fingerprint its certificate is known by (none
/// when it does not encrypt, or when an authority vouches for it). None
/// of it is a secret: it is what a `kito connect` line is written with.
pub fn clients(server: &Server) -> Value {
    let (port, plain) = match SERVING.get() {
        Some((bound, plain)) => (Some(bound.port().to_string()), *plain),
        None => (server.config.listen.rsplit_once(':').map(|(_, port)| port.to_owned()), false),
    };
    let by_fingerprint = !plain && !crate::tls::from_authority(&server.dir);
    let fingerprint = by_fingerprint.then(|| crate::tls::fingerprint(&server.dir).ok()).flatten();
    json!({ "port": port, "plain": plain, "fingerprint": fingerprint })
}

/// The server's people and repositories, asked from a connected
/// directory or from a page: answers one of `PEOPLE`, as whoever asks
/// with what they may do. `here` is the repository of the session, `role`
/// theirs in it.
///
/// What changes the configuration is made as one change (`Server::change`):
/// who asks, what they are and what they ask about are checked there, on
/// the configuration as it is on the disk then.
pub fn people(
    server: &Server,
    who: &Asker,
    here: Option<&str>,
    role: Option<Role>,
    method: &str,
    params: &Value,
) -> Result<Value, (i64, String)> {
    use crate::state::ServerRole;
    let (name, user) = (who.name, who.user);
    let refused = |why: String| (rpc::REFUSED, why);
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((rpc::INVALID_PARAMS, format!("missing parameter `{key}`")))
    };
    // What changed since this client connected counts.
    let mut now = Server::open_for_a_request(&server.dir).map_err(|e| (rpc::INTERNAL_ERROR, e))?;
    let admin = user.is_admin();
    let owner = user.server_role == ServerRole::Owner;
    // And what changes while this is answered: nothing is changed for
    // somebody who, when the change is made, may no longer ask for it.
    let least = match method {
        "server.repo_unarchive" | "server.repo_delete" => ServerRole::Owner,
        // Theirs to ask whatever they are on the server: what else they
        // must be is checked where each of these is made.
        "server.settings_history" | "server.members" | "server.grant" => ServerRole::Member,
        "server.people_link" | "server.people_unlink" | "server.people_token" => ServerRole::Member,
        _ => ServerRole::Admin,
    };
    now.only_while(still(name, who.came, least));
    let admins_manage = format!("only an admin of this server may manage its people: {name} is a member");
    let owners_name = "only an owner of this server names its admins and owners";
    match method {
        "server.settings_history" => {
            let repo = here.ok_or((rpc::INVALID_PARAMS, "this session is about no repository".to_owned()))?;
            if role < Some(Role::Maintain) && !admin {
                return Err(refused(format!("only a maintainer of `{repo}` sees its settings")));
            }
            Ok(json!(now.settings_history(repo)))
        }
        "server.members" => {
            let repo = here.ok_or((rpc::INVALID_PARAMS, "this session is about no repository".to_owned()))?;
            if role < Some(Role::Maintain) && !admin {
                return Err(refused(format!(
                    "only a maintainer of `{repo}` sees who uses it: {name} may {}",
                    role.map(Role::word).unwrap_or("nothing")
                )));
            }
            let members: Vec<Value> = now
                .members(repo)
                .into_iter()
                .map(|(person, role, by_server)| {
                    let them = now.config.users.get(&person);
                    json!({
                        "user": person,
                        "role": role.word(),
                        "admin": by_server,
                        "email": them.and_then(|them| them.email.clone()),
                        "logins": them.map(|them| them.logins.clone()).unwrap_or_default(),
                        "seen_ms": now.seen(&person),
                    })
                })
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
            let what = match given {
                Some(given) => format!("{person} may {} in {repo}", given.word()),
                None => format!("{person} may no longer use {repo}"),
            };
            let changed = now.change(|change| -> Result<bool, No> {
                // Theirs to say still, by what they may do here now.
                who.standing(change)?;
                let theirs = change.config.users.get(name).and_then(|them| them.role_in(repo));
                if theirs < Some(Role::Maintain) {
                    return Err(No(
                        rpc::REFUSED,
                        format!(
                            "only a maintainer of `{repo}` may say who uses it: {name} may {}",
                            theirs.map(Role::word).unwrap_or("nothing")
                        ),
                    ));
                }
                let changed = change.grant(person, repo, given)?;
                if changed {
                    change.then_tell("role_changed", json!({ "user": person, "repo": repo, "role": given.map(Role::word), "by": name, "what": what }));
                }
                Ok(changed)
            })?;
            Ok(json!({ "user": person, "repo": repo, "role": given.map(Role::word), "changed": changed }))
        }
        // How a person signs in: their own to change, whatever their role.
        "server.people_link" | "server.people_unlink" | "server.people_token" => {
            change_sign_in(&mut now, who, method, params)
        }
        _ if !admin => Err(refused(admins_manage)),
        // The server's repositories (ADR 0018), for admins; restoring and
        // deleting for owners.
        "server.repos_all" => {
            let live: Vec<Value> = now
                .repos()
                .into_iter()
                .map(|repo| {
                    let members =
                        now.config.users.values().filter(|them| them.access.contains_key(&repo)).count();
                    let size =
                        now.repo(&repo).map(|root| dir_size(&root.join(kito_store::STORE_DIR))).unwrap_or(0);
                    json!({ "name": repo, "members": members, "store_bytes": size })
                })
                .collect();
            Ok(
                json!({ "repos": live, "archived": now.archived(), "renamed": now.config.renamed, "keep_archived_days": crate::state::KEEP_ARCHIVED_DAYS }),
            )
        }
        "server.repo_add" => {
            let repo = text("name")?.to_owned();
            let from = params.get("from").and_then(Value::as_str).filter(|from| !from.trim().is_empty());
            let token = params.get("token").and_then(Value::as_str);
            // One way to start a repository at a time.
            if from.is_some() && gives_files(params) {
                return Err((
                    rpc::INVALID_PARAMS,
                    "a repository comes from a git address or from the files given, not both".to_owned(),
                ));
            }
            // Told as it takes its name, with the lock that gives it held.
            let added = |with: String| {
                vec![("repo_added", json!({ "repo": repo, "by": name, "what": format!("{repo}{with}") }))]
            };
            match from {
                // An address elsewhere: not a directory of the server's own machine.
                Some(url) if !is_remote_git(url.trim()) => {
                    return Err(refused(format!(
                        "`{}` is not a git address: one is https://, ssh:// or user@host:path",
                        url.trim()
                    )))
                }
                Some(url) => {
                    let told = added(format!(" from {url}"));
                    now.add_repo_from_git(&repo, url.trim(), token, told)
                        .map_err(|why| refused(now.without_dir(why)))?;
                }
                None => {
                    let files = first_files(params)?;
                    let with = match params.get("files").and_then(Value::as_array).map_or(0, Vec::len) {
                        0 => String::new(),
                        1 => ", with 1 file given".to_owned(),
                        given => format!(", with {given} files given"),
                    };
                    now.add_repo_told(&repo, None, files.as_ref(), false, added(with))
                        .map_err(|why| refused(now.without_dir(why)))?;
                }
            };
            // Whoever adds it maintains it: given once it is there, to
            // whoever is an admin still (`only_while`), however long the
            // clone took.
            let _ = now.grant(name, &repo, Some(Role::Maintain));
            Ok(json!({ "name": repo }))
        }
        "server.repo_upload_begin" => {
            let mut bytes = [0u8; 16];
            getrandom::getrandom(&mut bytes).map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
            let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let path = now.upload_path(&id).map_err(refused)?;
            std::fs::create_dir_all(path.parent().unwrap_or(&path))
                .map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
            std::fs::write(&path, b"").map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
            Ok(json!({ "id": id }))
        }
        "server.repo_upload_chunk" => {
            use std::io::Write as _;
            let path = now.upload_path(text("id")?).map_err(refused)?;
            let bytes =
                rpc::decode(text("data")?).ok_or((rpc::INVALID_PARAMS, "`data` is not base64".to_owned()))?;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|_| refused("no such upload".to_owned()))?;
            file.write_all(&bytes).map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
            Ok(json!({ "size": file.metadata().map(|meta| meta.len()).unwrap_or(0) }))
        }
        "server.repo_upload_end" => {
            let path = now.upload_path(text("id")?).map_err(refused)?;
            let repo = text("name")?.to_owned();
            let kind = text("kind")?;
            // What was sent is the repository: it starts with nothing else.
            if gives_files(params) {
                return Err((
                    rpc::INVALID_PARAMS,
                    "a repository comes from what was sent or from the files given, not both".to_owned(),
                ));
            }
            let what = format!("{repo}, sent from {name}'s machine ({kind})");
            let told = vec![("repo_added", json!({ "repo": repo, "by": name, "what": what }))];
            // Made aside, and given its name once it is whole (ADR 0018,
            // amended): what fails takes away what was made here, and
            // nothing that has the name.
            let result = (|| -> Result<(), String> {
                now.name_free(&repo)?;
                match kind {
                    // A Kito repository as it was on the machine it came from.
                    "kito" => {
                        let key = kitod::rpc::decode(text("key").map_err(|(_, why)| why)?)
                            .filter(|key| key.len() == 32)
                            .ok_or("the key of the upload is not one")?;
                        let mut fixed = [0u8; 32];
                        fixed.copy_from_slice(&key);
                        let aside = now.adding_dir(&repo)?;
                        let read = std::fs::File::open(&path)
                            .map_err(|e| e.to_string())
                            .and_then(|file| kitod::archive::read(file, &fixed, &aside).map(|_| ()))
                            .and_then(|()| {
                                match aside.join(kito_store::STORE_DIR).join("format").is_file() {
                                    true => Ok(()),
                                    false => Err("what was sent is not a Kito repository".to_owned()),
                                }
                            });
                        if let Err(why) = read {
                            let _ = std::fs::remove_dir_all(&aside);
                            return Err(why);
                        }
                        now.take_name(&repo, &aside, told).map(|_| ())
                    }
                    // A git repository, as a bundle of its history.
                    "git" => {
                        now.add_repo_told(&repo, Some(&path.to_string_lossy()), None, true, told).map(|_| ())
                    }
                    other => Err(format!("`{other}`: an upload is `kito` or `git`")),
                }
            })();
            let _ = std::fs::remove_file(&path);
            // Where the server keeps what was sent is not said to whoever sent it.
            result.map_err(|why| refused(now.without_dir(why)))?;
            let _ = now.grant(name, &repo, Some(Role::Maintain));
            Ok(json!({ "name": repo }))
        }
        "server.repo_archive" => {
            let repo = text("name")?.to_owned();
            now.archive_repo(&repo, Some(name)).map_err(refused)?;
            Ok(json!({ "name": repo }))
        }
        "server.repo_unarchive" | "server.repo_delete" if !owner => {
            Err(refused("only an owner of this server brings back or deletes a repository".to_owned()))
        }
        "server.repo_unarchive" => {
            let repo = text("name")?.to_owned();
            now.unarchive_repo(&repo, Some(name)).map_err(refused)?;
            Ok(json!({ "name": repo }))
        }
        "server.repo_delete" => {
            let repo = text("name")?.to_owned();
            if params.get("confirm").and_then(Value::as_str) != Some(repo.as_str()) {
                return Err((
                    rpc::INVALID_PARAMS,
                    format!("to delete `{repo}`, give its name again as `confirm`"),
                ));
            }
            // Which of the name: the archived one, or the one in use.
            use crate::state::Which;
            let asked = match params.get("archived") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(true)) => Some(Which::Archived),
                Some(Value::Bool(false)) => Some(Which::Live),
                Some(_) => return Err((rpc::INVALID_PARAMS, "`archived` is true or false".to_owned())),
            };
            let archived = now.delete_repo(&repo, asked, Some(name)).map_err(refused)? == Which::Archived;
            Ok(json!({ "name": repo, "archived": archived }))
        }
        "server.repo_rename" => {
            let (from, to) = (text("from")?.to_owned(), text("to")?.to_owned());
            now.rename_repo(&from, &to, Some(name)).map_err(refused)?;
            Ok(json!({ "from": from, "name": to }))
        }
        // The audit log and the backups (ADR 0017), for admins.
        "server.audit" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
            let filter = crate::audit::Filter {
                requests: params.get("requests").and_then(Value::as_bool).unwrap_or(false),
                who: text("who"),
                kind: text("kind"),
                repo: text("repo"),
                since_ms: params.get("since_ms").and_then(Value::as_u64),
                limit: params.get("limit").and_then(Value::as_u64).unwrap_or(100).min(2000) as usize,
            };
            Ok(json!(crate::audit::read(&now.dir, &filter)))
        }
        "server.audit_verify" => match crate::audit::verify(&now.dir) {
            Ok(lines) => Ok(json!({ "whole": true, "lines": lines })),
            Err(why) => Ok(json!({ "whole": false, "why": why })),
        },
        "server.backups" => {
            let listed = crate::backup::list(&now.dir, &now.config.backup).map_err(refused)?;
            Ok(json!({
                "backups": listed,
                "every_hours": now.config.backup.every_hours,
                "keep": now.config.backup.keep,
                "folder": crate::backup::folder(&now.dir, &now.config.backup),
                "s3": now.config.backup.s3.as_ref().map(|s3| format!("{}/{}", s3.endpoint, s3.bucket)),
            }))
        }
        "server.backup_now" => {
            let mut done = crate::backup::run(&now.dir, &now.config.backup).map_err(refused)?;
            // The key is said on the server's machine, not over the network.
            if done["new_key"].is_string() {
                done["new_key"] =
                    json!("made now: `kito-server backup key` on the server's machine prints it");
            }
            now.tell("backup_requested", json!({ "by": name, "what": done["name"] }));
            Ok(done)
        }
        "server.people" => {
            let people: Vec<Value> = now
                .config
                .users
                .iter()
                .map(|(person, them)| {
                    let access: serde_json::Map<String, Value> =
                        them.access.iter().map(|(repo, role)| (repo.clone(), json!(role.word()))).collect();
                    json!({ "user": person, "email": them.email, "role": them.server_role.word(), "access": access, "token": them.has_token(), "logins": them.logins, "seen_ms": now.seen(person) })
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
                return Err(refused(owners_name.to_owned()));
            }
            // The plugin is asked first: it may take its time, and nothing
            // waits for it with the configuration held.
            let identity = match params.get("identity").and_then(Value::as_str) {
                Some(identity) => Some(now.look_up(identity).map_err(refused)?),
                None => None,
            };
            let email = params.get("email").and_then(Value::as_str).map(str::to_owned);
            let token = now.change(|change| -> Result<Option<String>, No> {
                let (admin, owner) = who.standing(change)?;
                if !admin {
                    return Err(No(rpc::REFUSED, admins_manage.clone()));
                }
                if given >= ServerRole::Admin && !owner {
                    return Err(No(rpc::REFUSED, owners_name.to_owned()));
                }
                let token = change.add_user(person, email, &[], given, identity.as_ref())?;
                // Kept and queued in the order the changes were made; the
                // plugins hear no token.
                let what = format!("{person} ({})", given.word());
                change.then_tell(
                    "user_added",
                    json!({ "user": person, "role": given.word(), "by": name, "what": what }),
                );
                Ok(token)
            })?;
            // What they connect with: the port the server listens on, its certificate.
            let how = clients(&now);
            Ok(
                json!({ "user": person, "role": given.word(), "token": token, "fingerprint": how["fingerprint"], "plain": how["plain"], "port": how["port"] }),
            )
        }
        "server.people_remove" | "server.people_role" => {
            let person = text("user")?;
            let given = match method {
                "server.people_remove" => None,
                _ => Some(ServerRole::of_word(text("role")?).map_err(|why| (rpc::INVALID_PARAMS, why))?),
            };
            now.change(|change| -> Result<(), No> {
                // Who asks, and who they change, as both are now.
                let (admin, owner) = who.standing(change)?;
                if !admin {
                    return Err(No(rpc::REFUSED, admins_manage.clone()));
                }
                let theirs = change.config.users.get(person).map(|them| them.server_role);
                let theirs = theirs.ok_or_else(|| format!("{person} is not a user of this server"))?;
                if theirs >= ServerRole::Admin && !owner {
                    let why = format!("{person} is an {}: only an owner changes them", theirs.word());
                    return Err(No(rpc::REFUSED, why));
                }
                match given {
                    None => {
                        change.remove_user(person)?;
                        change.then_tell("user_removed", json!({ "user": person, "by": name, "what": person }));
                        Ok(())
                    }
                    Some(given) if given >= ServerRole::Admin && !owner => {
                        Err(No(rpc::REFUSED, owners_name.to_owned()))
                    }
                    Some(given) => {
                        change.set_server_role(person, given)?;
                        change.then_tell("role_changed", json!({ "user": person, "server_role": given.word(), "by": name, "what": format!("{person}: {}", given.word()) }));
                        Ok(())
                    }
                }
            })?;
            let Some(given) = given else {
                return Ok(json!({ "user": person, "removed": true }));
            };
            Ok(json!({ "user": person, "role": given.word() }))
        }
        _ => Err((
            rpc::METHOD_NOT_FOUND,
            format!("`{method}` is not something a client can ask through the server"),
        )),
    }
}

/// Changes how a person signs in (ADR 0006, amended): an identity attached
/// (`server.people_link`) or taken away (`server.people_unlink`), a new
/// token or none (`server.people_token`). Nobody named: whoever asks.
///
/// A new token of one's own, taking one's own away and unlinking one's own
/// identity are anybody's to ask. The rest is for whoever may change the
/// person, as for their role: an admin for a member, an owner for
/// anybody. That holds for linking oneself too: a login named proves
/// nothing, and a person attaches an account of their own by signing in
/// to it (`/-/sign-in/<method>?link=1`).
fn change_sign_in(
    now: &mut Server,
    who: &Asker,
    method: &str,
    params: &Value,
) -> Result<Value, (i64, String)> {
    use crate::state::ServerRole;
    let name = who.name;
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((rpc::INVALID_PARAMS, format!("missing parameter `{key}`")))
    };
    let person = match params.get("user") {
        None | Some(Value::Null) => name,
        Some(Value::String(person)) => person.as_str(),
        Some(_) => return Err((rpc::INVALID_PARAMS, "`user` is the name of a person".to_owned())),
    };
    let own = person == name;
    let by_signing_in = |other: &str| {
        No(
            rpc::REFUSED,
            format!(
                "naming a login proves nothing: {name} links an account of their own by signing in to it (in \
                 a browser, `/-/sign-in/<method>?link=1` at the server's web address), or {other} links it for \
                 them"
            ),
        )
    };
    // Whether they may, by the server as it is: what they came with is
    // theirs still, and they may change that person. Asked where each
    // change is made, on what was just read; and once before anything
    // else, so that whoever may not is told that and no more, and no
    // plugin is asked for them.
    let may = |now: &Server| -> Result<(), No> {
        let (admin, owner) = who.standing(now)?;
        if own && method != "server.people_link" {
            return Ok(());
        }
        if !admin {
            return Err(match own {
                true => by_signing_in("an admin"),
                false => No(
                    rpc::REFUSED,
                    format!("only an admin of this server may manage its people: {name} is a member"),
                ),
            });
        }
        let theirs = now.config.users.get(person).map(|them| them.server_role);
        let theirs = theirs.ok_or_else(|| format!("{person} is not a user of this server"))?;
        match theirs >= ServerRole::Admin && !owner {
            true if own => Err(by_signing_in("an owner")),
            true => {
                Err(No(rpc::REFUSED, format!("{person} is an {}: only an owner changes them", theirs.word())))
            }
            false => Ok(()),
        }
    };
    may(now)?;
    // What happened is kept and queued for the plugins in the change that
    // made it (`then_tell`), in the order the changes were made: the
    // change, then each session it ended. They are told on a thread of
    // their own: a plugin that is slow holds no token back.
    match method {
        "server.people_link" => {
            let identity = now.look_up(text("identity")?).map_err(|why| (rpc::REFUSED, why))?;
            // The plugin took its time: who may is asked again, where the
            // link is made.
            now.change(|change| -> Result<(), No> {
                may(change)?;
                if change.link(person, &identity)? {
                    let what = format!("{person}: {}", identity.written());
                    let event =
                        json!({ "user": person, "identity": identity.key(), "by": name, "what": what });
                    change.then_tell("identity_linked", event);
                }
                Ok(())
            })?;
        }
        "server.people_unlink" => {
            let way = text("method")?;
            now.change(|change| -> Result<(), No> {
                may(change)?;
                let (login, ended) = change.unlink(person, way, who.session())?;
                let event = json!({ "user": person, "by": name, "what": format!("{person}: {login}") });
                change.then_tell_all(Server::with_sessions_ended(
                    ("identity_unlinked", event),
                    person,
                    ended,
                ));
                Ok(())
            })?;
        }
        _ => {
            let yes_or_no = |key: &str| match params.get(key) {
                None | Some(Value::Null) => Ok(false),
                Some(Value::Bool(yes)) => Ok(*yes),
                Some(_) => Err((rpc::INVALID_PARAMS, format!("`{key}` is true or false"))),
            };
            let (remove, alone) = (yes_or_no("remove")?, yes_or_no("alone")?);
            if alone && remove {
                return Err((
                    rpc::INVALID_PARAMS,
                    "`alone` is a new token and nothing else, and `remove` takes the token away: one or the \
                     other"
                        .to_owned(),
                ));
            }
            if alone {
                return token_alone(now, who, person, &may);
            }
            if !remove {
                let token = now.change(|change| -> Result<String, No> {
                    may(change)?;
                    let (token, replaced, ended) = change.change_token(person, who.session())?;
                    let what = match replaced {
                        true => person.to_owned(),
                        false => format!("{person}, who had none"),
                    };
                    let event = json!({ "user": person, "replaced": replaced, "by": name, "what": what });
                    change.then_tell_all(Server::with_sessions_ended(
                        ("token_changed", event),
                        person,
                        ended,
                    ));
                    Ok(token)
                })?;
                // Said this once, to whoever asked: the server keeps what it hashes to. And
                // how they sign in besides: a token replaced does not take away an identity
                // whoever held it may have attached.
                let logins = now.config.users.get(person).map(|them| them.logins.clone()).unwrap_or_default();
                return Ok(json!({ "user": person, "token": token, "logins": logins }));
            }
            now.change(|change| -> Result<(), No> {
                may(change)?;
                let ended = change.remove_token(person, who.session())?;
                let event = json!({ "user": person, "by": name, "what": person });
                change.then_tell_all(Server::with_sessions_ended(("token_removed", event), person, ended));
                Ok(())
            })?;
            return Ok(json!({ "user": person, "token": null }));
        }
    }
    let them = now.config.users.get(person);
    Ok(json!({
        "user": person,
        "token": them.is_some_and(User::has_token),
        "logins": them.map(|them| them.logins.clone()).unwrap_or_default(),
    }))
}

/// `server.people_token { user, alone: true }`: for a token that leaked, a
/// new token and nothing else to sign in with (`Change::token_alone`), for
/// whoever may give the person a new token. The answer and the audit log
/// name the identities taken away.
fn token_alone(
    now: &mut Server,
    who: &Asker,
    person: &str,
    may: &dyn Fn(&Server) -> Result<(), No>,
) -> Result<Value, (i64, String)> {
    let (token, unlinked) = now.change(|change| -> Result<(String, Vec<String>), No> {
        may(change)?;
        let (token, unlinked, replaced, ended) = change.token_alone(person, who.session())?;
        change.then_tell_all(Server::alone_events(person, Some(who.name), (&unlinked, replaced), ended));
        Ok((token, unlinked))
    })?;
    Ok(json!({ "user": person, "token": token, "logins": [], "unlinked": unlinked }))
}

/// Whether a request gives a new repository files to start with.
fn gives_files(params: &Value) -> bool {
    match params.get("files") {
        None | Some(Value::Null) => false,
        Some(Value::Array(given)) => !given.is_empty(),
        Some(_) => true,
    }
}

/// The files a request gives a new repository to start with (ADR 0018,
/// amended): `files`, each a `path` in the repository and its `text`.
/// `None`: it gives none.
fn first_files(params: &Value) -> Result<Option<FirstFiles>, (i64, String)> {
    const EACH: &str = "{ \"path\": …, \"text\": … }";
    let wrong = |why: String| (rpc::INVALID_PARAMS, why);
    if !gives_files(params) {
        return Ok(None);
    }
    let given = params["files"].as_array().ok_or_else(|| wrong(format!("`files` is a list of {EACH}")))?;
    let mut files = Vec::new();
    for (at, file) in given.iter().enumerate() {
        let path = file
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| wrong(format!("file {} of `files` has no `path`: each is {EACH}", at + 1)))?;
        let text = file
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| wrong(format!("{path:?} has no `text`: each of `files` is {EACH}")))?;
        files.push((path, text));
    }
    FirstFiles::new(&files).map(Some).map_err(wrong)
}

/// A git address of another machine, the only kind a client may have the
/// server clone (ADR 0018): never a path on the server's own disk.
fn is_remote_git(url: &str) -> bool {
    match url.split_once("://") {
        Some((scheme, rest)) => matches!(scheme, "https" | "http" | "ssh" | "git") && !rest.is_empty(),
        // user@host:path, as ssh is written for git
        None => url
            .split_once(':')
            .is_some_and(|(host, path)| host.contains('@') && !host.contains('/') && !path.is_empty()),
    }
}

/// The bytes of the files under a directory.
fn dir_size(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
            _ => entry.metadata().map(|meta| meta.len()).unwrap_or(0),
        })
        .sum()
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

/// Where this process serves clients, and whether it encrypts: what a
/// person added from a page connects to.
static SERVING: std::sync::OnceLock<(std::net::SocketAddr, bool)> = std::sync::OnceLock::new();

/// Listens for clients, each served on a thread of its own. Connections
/// are encrypted with the server's certificate; `plain`: they are not,
/// and the server listens on this machine only.
pub fn serve(server: Server, listen: Option<&str>, plain: bool) -> Result<(), String> {
    let address = listen.unwrap_or(&server.config.listen).to_owned();
    let listener = TcpListener::bind(&address).map_err(|e| format!("{address}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    let _ = SERVING.set((bound, plain));
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
        // Each connection reads the server as it is when it says hello
        // (`run`): what was read when this started is only where it is.
        std::thread::spawn(move || serve_client(&server, stream, tls));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ServerRole;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// One of `PEOPLE`, asked as a request of whoever holds `token` is
    /// answered: who that is by the server as it is, then what they ask.
    fn asks(dir: &Path, token: &str, method: &str, params: Value) -> Result<Value, String> {
        let server = Server::open(dir)?;
        let (name, user) = server.user_of(token).ok_or("this token is not one the server knows")?;
        let came = Came::with(token);
        let who = Asker { name: &name, user: &user, came: &came };
        people(&server, &who, None, None, method, &params).map_err(|(_, why)| why)
    }

    fn token_in(answer: &Value) -> String {
        answer["token"].as_str().unwrap_or_else(|| panic!("no token in {answer}")).to_owned()
    }

    #[test]
    fn what_an_admin_took_away_is_not_brought_back_by_whoever_asks_for_tokens_in_a_loop() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let ada = server.add_user("ada", None, &[], ServerRole::Owner, None).unwrap().unwrap();
        // bob asks for a new token over and over: each request reads the
        // server, and writes it. The token he holds is the last he was told.
        let (stop, asked) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let (holds, given) = (Arc::new(Mutex::new(String::new())), Arc::new(AtomicUsize::new(0)));
        let looping = {
            let (dir, stop, asked) = (dir.path().to_owned(), stop.clone(), asked.clone());
            let (holds, given) = (holds.clone(), given.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let with = holds.lock().unwrap().clone();
                    if let Ok(answer) = asks(&dir, &with, "server.people_token", json!({})) {
                        given.fetch_add(1, Ordering::SeqCst);
                        let mut holds = holds.lock().unwrap();
                        // Unless he was added again meanwhile, with another.
                        if *holds == with {
                            *holds = token_in(&answer);
                        }
                    }
                    asked.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        // Whatever he had asked when a change was made has been answered by then.
        let later = || {
            let at = asked.load(Ordering::SeqCst);
            while asked.load(Ordering::SeqCst) < at + 2 {
                std::thread::yield_now();
            }
        };
        let role = || {
            let now = Server::open(dir.path()).unwrap();
            now.config.users.get("bob").map(|bob| bob.server_role)
        };
        let as_ada = |method: &str, params: Value| asks(dir.path(), &ada, method, params).unwrap();
        for round in 0..300 {
            let added = as_ada("server.people_add", json!({ "user": "bob", "role": "admin" }));
            *holds.lock().unwrap() = token_in(&added);
            later();
            assert_eq!(role(), Some(ServerRole::Admin), "round {round}");
            // Made a member, he stays one, however he writes.
            as_ada("server.people_role", json!({ "user": "bob", "role": "member" }));
            later();
            assert_eq!(role(), Some(ServerRole::Member), "round {round}: he is an admin again");
            // Removed, he stays removed.
            as_ada("server.people_remove", json!({ "user": "bob" }));
            later();
            assert_eq!(role(), None, "round {round}: he is back");
        }
        stop.store(true, Ordering::SeqCst);
        looping.join().unwrap();
        // He did write all along: a new token in every round, at the least.
        assert!(given.load(Ordering::SeqCst) >= 300, "{} tokens", given.load(Ordering::SeqCst));
        crate::state::told();
    }

    #[test]
    fn a_request_changes_nothing_once_what_it_came_with_is_no_longer_theirs() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let bob = server.add_user("bob", None, &[], ServerRole::Admin, None).unwrap().unwrap();
        let carol = server.add_user("carol", None, &[], ServerRole::Member, None).unwrap().unwrap();
        // A request of bob's has begun: who he is, and what he may, were read.
        let began = Server::open(dir.path()).unwrap();
        let (name, user) = began.user_of(&bob).unwrap();
        let came = Came::with(&bob);
        let who = Asker { name: &name, user: &user, came: &came };
        let asks = |method: &str, params: Value| people(&began, &who, None, None, method, &params);
        let everything = [
            ("server.people_token", json!({})),
            ("server.people_token", json!({ "remove": true })),
            ("server.people_unlink", json!({ "method": "hello" })),
            ("server.people_token", json!({ "user": "carol" })),
            ("server.people_role", json!({ "user": "carol", "role": "admin" })),
            ("server.people_remove", json!({ "user": "carol" })),
            ("server.people_add", json!({ "user": "dave" })),
        ];

        // Meanwhile he is made a member: what is an admin's to ask is no longer his.
        server.set_server_role("bob", ServerRole::Member).unwrap();
        let members = "only an admin of this server may manage its people: bob is a member";
        let no_longer = "bob is no longer an admin of this server: nothing was changed";
        for (method, params) in &everything[3..] {
            let (code, why) = asks(method, params.clone()).unwrap_err();
            assert_eq!(code, rpc::REFUSED, "{method}: {why}");
            assert!(why == members || why == no_longer, "{method}: {why}");
        }

        // And his token is replaced, as one that leaked is: the request
        // that came with it gets no token of its own, and changes nothing.
        let (new, _, _) = server.change_token("bob", None).unwrap();
        let changed = "bob's token was changed: connect again with the new one";
        for (method, params) in &everything {
            let refused = asks(method, params.clone()).unwrap_err();
            assert_eq!(refused, (rpc::REFUSED, changed.to_owned()), "{method}");
        }
        let after = Server::open(dir.path()).unwrap();
        assert_eq!(after.user_of(&new).map(|(name, _)| name).as_deref(), Some("bob"));
        assert_eq!(after.user_of(&carol).map(|(name, _)| name).as_deref(), Some("carol"));
        assert_eq!(after.config.users.len(), 2);
        assert_eq!(after.config.users["carol"].server_role, ServerRole::Member);

        // Removed, he is nobody: so is what he had asked.
        server.remove_user("bob").unwrap();
        let (_, why) = asks("server.people_token", json!({})).unwrap_err();
        assert_eq!(why, "bob is no longer a user of this server");
        assert!(!Server::open(dir.path()).unwrap().config.users.contains_key("bob"));

        // What is an owner's to ask is for whoever is an owner still when
        // it is done: carol was one when she asked.
        server.set_server_role("carol", ServerRole::Owner).unwrap();
        let began = Server::open(dir.path()).unwrap();
        let (name, user) = began.user_of(&carol).unwrap();
        let came = Came::with(&carol);
        let who = Asker { name: &name, user: &user, came: &came };
        server.set_server_role("carol", ServerRole::Admin).unwrap();
        let gone = json!({ "name": "shop", "confirm": "shop" });
        let (_, why) = people(&began, &who, None, None, "server.repo_delete", &gone).unwrap_err();
        assert_eq!(why, "carol is no longer an owner of this server: nothing was changed");
        crate::state::told();
    }

    #[test]
    fn what_people_change_at_once_is_told_in_the_order_it_was_made() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let ada = server.add_user("ada", None, &[], ServerRole::Owner, None).unwrap().unwrap();
        let rose = server.add_user("rose", None, &[], ServerRole::Owner, None).unwrap().unwrap();
        // For each person, one owner adds them while another removes them
        // as soon as they are there; and each change takes its time
        // between being written and what it tells.
        std::fs::write(dir.path().join("pause-telling-in-the-tests"), "").unwrap();
        let people: Vec<String> = (0..24).map(|n| format!("p{n}")).collect();
        let asking = |token: String, method: &'static str, person: String| {
            let dir = dir.path().to_owned();
            std::thread::spawn(move || {
                for _ in 0..4000 {
                    if asks(&dir, &token, method, json!({ "user": person })).is_ok() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                panic!("{method} {person} was never made");
            })
        };
        let threads: Vec<_> = people
            .iter()
            .flat_map(|person| {
                [
                    asking(ada.clone(), "server.people_add", person.clone()),
                    asking(rose.clone(), "server.people_remove", person.clone()),
                ]
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        // The plugins are given each person's addition before their
        // removal, and so does the audit log keep them.
        let here = server.dir.clone();
        let queued: Vec<(String, String)> = crate::state::tests::queued()
            .lock()
            .unwrap()
            .iter()
            .filter(|(dir, _, _)| *dir == here)
            .map(|(_, kind, event)| (kind.clone(), event["user"].as_str().unwrap_or_default().to_owned()))
            .collect();
        let kept = crate::audit::read(&here, &crate::audit::Filter { limit: 1000, ..Default::default() });
        let mut kept: Vec<(String, String)> = kept
            .iter()
            .filter(|line| line["kind"] == "user_added" || line["kind"] == "user_removed")
            .map(|line| {
                let what = line["what"].as_str().unwrap_or_default();
                (
                    line["kind"].as_str().unwrap_or_default().to_owned(),
                    what.split(' ').next().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        kept.reverse();
        for person in &people {
            let of = |list: &[(String, String)]| -> Vec<String> {
                list.iter().filter(|(_, who)| who == person).map(|(kind, _)| kind.clone()).collect()
            };
            assert_eq!(of(&queued), ["user_added", "user_removed"], "{person}");
            assert_eq!(of(&kept), ["user_added", "user_removed"], "{person}");
        }
        crate::state::told();
    }
}
