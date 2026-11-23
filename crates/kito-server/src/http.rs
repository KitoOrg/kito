//! The server's HTTP address (ADR 0006).
//!
//! The server serves no pages of its own. Under `/-/` it answers what
//! only it can: who is asking, and a repository's daemon asked as that
//! person. Everything else is passed on to the plugin whose pages are
//! mounted there, a program of its own that the server starts and keeps
//! running.
//!
//! Who asks is known by a token (`Authorization: Bearer`, as the command
//! line has one) or by the cookie of a session the server opened: for a
//! token, or for whoever a plugin that signs people in says they are
//! (`/-/sign-in/<method>`).

use crate::session::Came;
use crate::state::{hash_of, identity_of, Role, Server, ENV_SERVER_DIR, LINK_SECONDS, SESSION_PREFIX};
use axum::body::Body;
use axum::extract::{Path as Segment, RawQuery, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use kitod::plugins::Permission;
use kitod::rpc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The cookie of a session.
const COOKIE: &str = "kito_session";

/// The address a request came from, as the connection says it: the
/// browser's, or that of whatever passes requests on to this server.
#[derive(Clone)]
struct Peer(String);
/// The cookie of somebody on their way to sign in and back: what the
/// plugin handed out when they left, for ten minutes. Served securely it
/// has the `__Host-` prefix, which a browser takes only from this host
/// itself, with `Secure`, for every path and with no `Domain`: a host
/// beside this one under the same domain cannot plant one.
const SIGNING_IN: &str = "kito_sign_in";
const SIGNING_IN_SECURELY: &str = "__Host-kito_sign_in";

/// The name of the sign-in cookie, as it is set, read and forgotten.
fn signing_in(secure: bool) -> &'static str {
    match secure {
        true => SIGNING_IN_SECURELY,
        false => SIGNING_IN,
    }
}

/// The line that sets the sign-in cookie, or forgets it (`seconds`: 0).
/// Lax: the way back starts on the provider's site.
fn signing_in_line(secure: bool, value: &str, seconds: u64) -> String {
    let name = signing_in(secure);
    match secure {
        true => format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={seconds}; Secure"),
        false => format!("{name}={value}; Path=/-/sign-in; HttpOnly; SameSite=Lax; Max-Age={seconds}"),
    }
}

/// `true` for a cookie that is the server's own: its session's, and what
/// it keeps for a sign-in. A plugin is not passed them on, and sets none.
fn own_cookie(name: &str) -> bool {
    [COOKIE, SIGNING_IN, SIGNING_IN_SECURELY].contains(&name.trim())
}
/// In the environment of a plugin that serves pages: where to listen, and
/// what the server says with every request it passes on.
pub const ENV_PAGES_LISTEN: &str = "KITO_PAGES_LISTEN";
pub const ENV_PAGES_SECRET: &str = "KITO_PAGES_SECRET";
/// Where a plugin that serves pages reaches the server's `/-/api` from
/// this machine, with its secret in `x-kito-plugin` (ADR 0020).
pub const ENV_SERVER_API: &str = "KITO_SERVER_API";

/// A plugin that serves pages, as the server reaches it.
#[derive(Clone)]
struct Pages {
    plugin: String,
    /// The version that runs: another one installed is started in its place.
    version: String,
    /// What it was given, as the server's configuration says.
    given: Vec<Permission>,
    /// The path its pages are under, without a `/` at its end.
    mount: String,
    /// Where it listens, on this machine.
    listen: String,
    secret: String,
    /// Set when it is disabled or replaced: its program is stopped.
    stop: Arc<std::sync::atomic::AtomicBool>,
}

struct Http {
    dir: PathBuf,
    /// Encrypted: cookies are for encrypted connections only.
    secure: bool,
    /// The plugins that serve pages now: started and stopped as they are
    /// enabled and disabled, while the server runs.
    pages: std::sync::RwLock<Vec<Pages>>,
    client: Client<HttpConnector, Body>,
    /// The server's HTTP on this machine, in the clear: for its plugins.
    local: String,
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The pairs of a query, decoded: `code=abc&state=x%2Fy`.
fn pairs(query: &str) -> BTreeMap<String, String> {
    fn decoded(text: &str) -> String {
        let bytes = text.as_bytes();
        let (mut out, mut at) = (Vec::with_capacity(bytes.len()), 0);
        while at < bytes.len() {
            let hex = bytes.get(at + 1..at + 3).and_then(|hex| std::str::from_utf8(hex).ok());
            match (bytes[at], hex.and_then(|hex| u8::from_str_radix(hex, 16).ok())) {
                (b'%', Some(byte)) => {
                    out.push(byte);
                    at += 2;
                }
                (b'+', _) => out.push(b' '),
                (byte, _) => out.push(byte),
            }
            at += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    let pairs = query.split('&').filter(|pair| !pair.is_empty());
    pairs
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .map(|(key, value)| (decoded(key), decoded(value)))
        .collect()
}

fn said(status: StatusCode, message: &str) -> Response {
    let message = match status {
        StatusCode::INTERNAL_SERVER_ERROR => went_wrong(message),
        _ => message.to_owned(),
    };
    (status, [(header::CACHE_CONTROL, "no-store")], Json(json!({ "error": message }))).into_response()
}

/// What a browser is told when the server failed it: a text of its own
/// that names nothing of where it is. Why is said on its standard error,
/// where whoever runs it reads (`its log says why`).
fn went_wrong(why: &str) -> String {
    match why == crate::state::UNREADABLE || why == crate::state::COULD_NOT {
        true => why.to_owned(),
        false => crate::state::could_not(format_args!("{why}")),
    }
}

fn answered(value: Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}

/// The value of a cookie of the request.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let lines = headers.get_all(header::COOKIE).iter().filter_map(|value| value.to_str().ok());
    lines.flat_map(|line| line.split(';')).find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

/// Who asks, and whether a cookie says so (a browser) or a token.
struct Who {
    name: String,
    user: crate::state::User,
    by_cookie: bool,
    /// What they came with: a token of their own, or a session's.
    came: Came,
}

impl Who {
    fn asker(&self) -> crate::session::Asker<'_> {
        crate::session::Asker { name: &self.name, user: &self.user, came: &self.came }
    }
}

impl Http {
    /// The server as it is now: people and repositories added since it
    /// started count for a request made now.
    ///
    /// When it cannot be read, why is said on this machine and the answer
    /// says only that: what the server says of itself names its directory.
    fn server(&self) -> Result<Server, String> {
        Server::open_for_a_request(&self.dir)
    }

    fn who(&self, server: &Server, headers: &HeaderMap) -> Option<Who> {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if let Some(token) = bearer {
            let token = token.trim();
            let (name, user) = server.user_of(token)?;
            server.saw(&name);
            return Some(Who { name, user, by_cookie: false, came: Came::with(token) });
        }
        // A session and nothing else: the token an administrator handed
        // out is not something a browser keeps.
        let id = cookie(headers, COOKIE).filter(|id| id.starts_with(SESSION_PREFIX))?;
        let (name, user) = server.user_of(id)?;
        server.saw(&name);
        Some(Who { name, user, by_cookie: true, came: Came::with(id) })
    }

    /// Where the provider sends the browser back to: this server, as the
    /// browser named it.
    fn callback(&self, headers: &HeaderMap, method: &str) -> Option<String> {
        let host = headers.get(header::HOST)?.to_str().ok().filter(|host| !host.is_empty())?;
        let scheme = if self.secure { "https" } else { "http" };
        Some(format!("{scheme}://{host}/-/sign-in/{method}/callback"))
    }

    fn cookie_line(&self, value: &str, seconds: u64) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={seconds}{secure}")
    }
}

/// `/-/me`: who is asking, what they may use, and how they sign in.
async fn me(State(http): State<Arc<Http>>, headers: HeaderMap) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let Some(who) = http.who(&server, &headers) else {
        return said(StatusCode::UNAUTHORIZED, "nobody is signed in: give a token, or open a session");
    };
    let repos: Vec<Value> = server
        .repos()
        .into_iter()
        .filter(|repo| who.user.may_use(repo))
        .map(|repo| json!({ "name": repo, "role": who.user.role_in(&repo).map(Role::word) }))
        .collect();
    answered(json!({
        "user": who.name,
        "email": who.user.email,
        "repos": repos,
        "admin": who.user.is_admin(),
        "role": who.user.server_role.word(),
        // Whether they have a token, never the token; and who they sign in as.
        "signs_in": { "token": who.user.has_token(), "logins": who.user.logins },
        // How a client connects here, for a `kito connect` line: no secret,
        // and so for anybody signed in, not for admins alone.
        "clients": crate::session::clients(&server),
        // What runs on this server: pages show what belongs to each.
        "plugins": server.running().iter().map(|(found, _)| found.manifest.name.clone()).collect::<Vec<_>>(),
    }))
}

/// Who asks, if they are an administrator of the server and the request
/// is theirs to make: `Err` is the answer otherwise, the same for
/// whoever is not one.
fn administrator(
    http: &Http,
    server: &Server,
    headers: &HeaderMap,
) -> Result<Who, (StatusCode, &'static str)> {
    let Some(who) = http.who(server, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "nobody is signed in: give a token, or open a session"));
    };
    // A page of another site can send the cookie along, and cannot set this.
    if who.by_cookie && headers.get("x-kito").is_none() {
        return Err((StatusCode::FORBIDDEN, "not a request of a page of this server"));
    }
    match who.user.is_admin() {
        true => Ok(who),
        false => Err((
            StatusCode::FORBIDDEN,
            "the plugins of this server are for its admins (`kito-server user role <name> admin`, on its machine)",
        )),
    }
}

/// `GET /-/admin/plugins`: what is installed and enabled, and what the
/// registry lists (ADR 0008).
async fn admin_plugins(State(http): State<Arc<Http>>, headers: HeaderMap) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    if let Err((status, why)) = administrator(&http, &server, &headers) {
        return said(status, why);
    }
    // The registry is asked over the network: not on the thread that serves.
    match tokio::task::spawn_blocking(move || crate::admin::listed(&server)).await {
        Ok(listed) => answered(listed),
        Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `POST /-/admin/plugins`: install, enable, disable or remove one.
async fn admin_change(
    State(http): State<Arc<Http>>,
    headers: HeaderMap,
    Json(ask): Json<crate::admin::Ask>,
) -> Response {
    let mut server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let who = match administrator(&http, &server, &headers) {
        Ok(who) => who,
        Err((status, why)) => return said(status, why),
    };
    // An admin still when the change is made: installing takes a while.
    server.only_while(crate::session::still(&who.name, &who.came, crate::state::ServerRole::Admin));
    let settle = http.clone();
    let done = tokio::task::spawn_blocking(move || {
        let done = crate::admin::change(&mut server, &who.name, &ask);
        settle.settle_pages();
        done
    })
    .await;
    match done {
        Ok(Ok(done)) => answered(done),
        Ok(Err(why)) => said(StatusCode::BAD_REQUEST, &why),
        Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `POST /-/people`: the server's people, asked as whoever is signed in
/// (ADR 0014): `server.people`, `server.people_add`, `_remove`, `_role`;
/// and how each signs in (ADR 0006, amended): `_link`, `_unlink`, `_token`.
async fn people(State(http): State<Arc<Http>>, headers: HeaderMap, Json(call): Json<Call>) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let Some(who) = http.who(&server, &headers) else {
        return said(StatusCode::UNAUTHORIZED, "nobody is signed in: give a token, or open a session");
    };
    if who.by_cookie && headers.get("x-kito").is_none() {
        return said(StatusCode::FORBIDDEN, "not a request of a page of this server");
    }
    let theirs = ["server.people", "server.audit", "server.backup", "server.repo"];
    if !theirs.iter().any(|prefix| call.method.starts_with(prefix)) {
        return said(
            StatusCode::NOT_FOUND,
            "`/-/people` answers the server's people, its audit log and its backups",
        );
    }
    let asked = tokio::task::spawn_blocking(move || {
        crate::session::people(&server, &who.asker(), None, None, &call.method, &call.params)
    })
    .await;
    match asked {
        Ok(Ok(value)) => answered(value),
        Ok(Err((code, message))) => {
            let status = match code {
                rpc::REFUSED => StatusCode::FORBIDDEN,
                rpc::METHOD_NOT_FOUND => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_REQUEST,
            };
            (status, [(header::CACHE_CONTROL, "no-store")], Json(json!({ "error": message, "code": code })))
                .into_response()
        }
        Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `GET /-/admin/backups/<name>`: an archive as it is kept, for an admin to download.
async fn download_backup(
    State(http): State<Arc<Http>>,
    Segment(name): Segment<String>,
    headers: HeaderMap,
) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    // A link the browser follows: it sends the session, and nothing a
    // page adds. Reading changes nothing, and another site cannot read
    // the answer.
    let Some(who) = http.who(&server, &headers).filter(|who| who.user.is_admin()) else {
        return said(StatusCode::FORBIDDEN, "the backups of this server are for its admins");
    };
    let plain = name.starts_with("kito-backup-") && name.ends_with(".kbak") && !name.contains(['/', '\\']);
    let Some(path) = crate::backup::folder(&server.dir, &server.config.backup)
        .map(|dir| dir.join(&name))
        .filter(|path| plain && path.is_file())
    else {
        return said(StatusCode::NOT_FOUND, "no such backup in this server's folder");
    };
    crate::audit::record(&server.dir, "backup_downloaded", Some(&who.name), None, &name);
    match tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .map_err(std::io::Error::other)
        .and_then(|read| read)
    {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct Opening {
    token: String,
}

/// `POST /-/session`: a session for whoever holds a token, kept in a
/// cookie from then on.
async fn open_session(
    State(http): State<Arc<Http>>,
    Extension(peer): Extension<Peer>,
    Json(opening): Json<Opening>,
) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    // The same answer for no token and for a wrong one; and for one that
    // was theirs when it was looked at and is not when the session is
    // written (changed meanwhile). The lock is waited for off the thread
    // that serves, as everywhere else.
    let opened =
        tokio::task::spawn_blocking(move || -> Result<(Server, Option<(String, String)>), String> {
            let token = opening.token.trim();
            let theirs = |now: &Server| now.user_of(token).map(|(name, _)| name);
            let Some(name) = theirs(&server) else { return Ok((server, None)) };
            let still = |now: &Server| theirs(now).is_some_and(|still| still == name);
            let id = server.open_session(&name, ("a token", Some(&peer.0)), still)?;
            Ok((server, id.map(|id| (name, id))))
        })
        .await;
    let (server, opened) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let Some((name, id)) = opened else {
        crate::audit::record(
            &server.dir,
            "token_refused",
            Some("somebody"),
            None,
            "a browser with a token the server does not know",
        );
        return said(StatusCode::UNAUTHORIZED, "this token is not one the server knows");
    };
    crate::audit::record(
        &server.dir,
        "session_opened",
        Some(&name),
        None,
        &format!("{name}, in a browser, with a token"),
    );
    let seconds = server.config.sign_in.days * 24 * 3600;
    let headers =
        [(header::SET_COOKIE, http.cookie_line(&id, seconds)), (header::CACHE_CONTROL, "no-store".into())];
    (headers, Json(json!({ "user": name }))).into_response()
}

/// `DELETE /-/session`: the session ends. When it could not be ended that
/// is said, and the browser keeps its cookie: a session that goes on is
/// not one its browser has forgotten.
async fn close_session(State(http): State<Arc<Http>>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie(&headers, COOKIE).map(str::to_owned) {
        let dir = http.dir.clone();
        // The lock is waited for off the thread that serves.
        let ended =
            tokio::task::spawn_blocking(move || Server::open_for_a_request(&dir)?.end_session(&id)).await;
        match ended {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return said(StatusCode::INTERNAL_SERVER_ERROR, &why),
            Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }
    ([(header::SET_COOKIE, http.cookie_line("", 0))], Json(json!({}))).into_response()
}

/// `GET /-/sign-in`: the ways to sign in here, each a plugin's, and
/// whether each can be used now: one that is not set up is listed with
/// why, for a client not to offer it.
async fn ways(State(http): State<Arc<Http>>) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let asked = tokio::task::spawn_blocking(move || {
        let ways = server.sign_in_methods().into_iter();
        ways.map(|(method, about)| {
            let not_ready = server.sign_in_not_ready(&method);
            let mut way = json!({
                "path": format!("/-/sign-in/{method}"), "method": method, "about": about,
                "ready": not_ready.is_none(),
            });
            if let Some(why) = not_ready {
                way["why"] = json!(why);
            }
            way
        })
        .collect::<Vec<Value>>()
    })
    .await;
    match asked {
        Ok(methods) => answered(json!({ "methods": methods })),
        Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Text as a page shows it: nothing of it is read as markup.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A small page for the person who comes back from a provider: what
/// happened, and a link to the pages of the server. `at_once`: the
/// browser goes there by itself.
fn page(title: &str, happened: &str, go: &str, at_once: bool) -> axum::response::Html<String> {
    let refresh = if at_once { "<meta http-equiv=\"refresh\" content=\"0;url=/\">" } else { "" };
    axum::response::Html(format!(
        "<!doctype html><meta charset=\"utf-8\">{refresh}<title>{title}</title><p>{} <a href=\"/\">{go}</a>.</p>",
        escaped(happened)
    ))
}

/// `GET /-/sign-in/<method>`: sends the browser where the plugin says the
/// person signs in. What the plugin handed out for the way back is kept
/// in a cookie of this browser, so that the one who comes back is the
/// one who left.
///
/// With `?link=1`, from somebody who is signed in in this browser: the
/// account they sign in to is attached to who they are here, and no
/// session opens (ADR 0006, amended). For the way back the same cookie
/// carries, after the state, a value made for this link and no other.
/// The server keeps what it hashes to, with the session the link was
/// asked from: what is on its disk brings nobody back, and no page reads
/// or writes the cookie.
async fn begin_sign_in(
    State(http): State<Arc<Http>>,
    Segment(method): Segment<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let nobody = "nobody is signed in: sign in first, then link an account to who you are";
    let linking = pairs(query.as_deref().unwrap_or_default()).get("link").is_some_and(|link| link == "1");
    let session = match linking {
        false => None,
        // The session of this browser and nothing else, as for a page.
        true => {
            let session = cookie(&headers, COOKIE)
                .filter(|id| id.starts_with(SESSION_PREFIX))
                .filter(|id| server.user_of(id).is_some());
            match session {
                Some(id) => Some(id.to_owned()),
                None => return said(StatusCode::UNAUTHORIZED, nobody),
            }
        }
    };
    let Some(redirect) = http.callback(&headers, &method) else {
        return said(StatusCode::BAD_REQUEST, "the request does not say which host it is for");
    };
    let told = json!({ "kind": "browser", "redirect": redirect });
    let begun = tokio::task::spawn_blocking(move || -> Result<(String, String), (StatusCode, String)> {
        let begun =
            server.sign_in_step(&method, &["begin"], &told).map_err(|e| (StatusCode::NOT_FOUND, e))?;
        if let Some(why) = begun["refused"].as_str() {
            return Err((StatusCode::FORBIDDEN, why.to_owned()));
        }
        // What goes in a cookie as it is, and nothing else.
        let plain = |state: &&str| {
            !state.is_empty()
                && state.len() <= 512
                && state.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
        };
        let (Some(open), Some(state)) = (begun["open"].as_str(), begun["state"].as_str().filter(plain))
        else {
            return Err((StatusCode::BAD_GATEWAY, "the plugin did not say where to sign in".to_owned()));
        };
        // The link begins once the plugin has said where to go: nothing
        // waits for a browser that was sent nowhere.
        let kept = match session {
            None => state.to_owned(),
            Some(session) => {
                let value =
                    server.begin_link(&session).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
                let value = value.ok_or_else(|| (StatusCode::UNAUTHORIZED, nobody.to_owned()))?;
                format!("{state}:{value}")
            }
        };
        Ok((open.to_owned(), kept))
    })
    .await;
    let (open, kept) = match begun {
        Ok(Ok(begun)) => begun,
        Ok(Err((status, why))) => return said(status, &why),
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let headers = [
        (header::LOCATION, open),
        (header::SET_COOKIE, signing_in_line(http.secure, &kept, LINK_SECONDS)),
        (header::CACHE_CONTROL, "no-store".to_owned()),
    ];
    (StatusCode::FOUND, headers).into_response()
}

/// Attaches the account somebody just signed in to, to who they are here:
/// the person whose session the link was asked from. `value` is what the
/// browser brought back for the link, and `sent` what the session it
/// sent, if it sent one, hashes to. Answers what to tell them; `Err`: why
/// nothing was linked.
///
/// What is kept of it says through which session it came, how that session
/// was opened and from where, and where the browser came back from: the
/// one who holds a person's token can attach an account to them, and it
/// must be possible to tell afterwards that they did (ADR 0006, amended).
fn link_account(
    server: &Server,
    method: &str,
    told: &Value,
    value: &str,
    (sent, peer): (Option<String>, &Peer),
) -> Result<String, (StatusCode, String)> {
    let refused = |why: String| (StatusCode::FORBIDDEN, why);
    let failed = |why: String| (StatusCode::INTERNAL_SERVER_ERROR, why);
    let gone = "whoever asked for this link is no longer signed in here";
    // Looked up before anything else, and taken out as it is: a value
    // brings somebody back once, whatever follows.
    let (name, session) = server.take_link(value).map_err(failed)?.ok_or_else(|| {
        refused(
            "no link waits for this browser: it was used already, it is more than ten minutes old, or \
             whoever asked for it is no longer signed in here"
                .to_owned(),
        )
    })?;
    // The way back starts on the provider's site, and the session's
    // cookie may not come with it: when it does, it is that session.
    if sent.is_some_and(|sent| sent != session) {
        return Err(refused(gone.to_owned()));
    }
    let answer = server.sign_in_step(method, &["finish"], told).map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    let identity = identity_of(method, &answer).map_err(refused)?;
    // The plugin took its time: whether they are signed in still is asked
    // where the link is made, on the server as it is then.
    let mut now = Server::open_for_a_request(&server.dir).map_err(failed)?;
    let opened = now.session_opened(&session);
    let written = identity.written();
    let (kept, told) = link_said(&name, &identity, opened, &peer.0);
    let linked = now
        .change(|change| match change.session_user(&session).as_deref() == Some(name.as_str()) {
            // Kept and queued with the change: the plugins hear what was
            // linked, the log keeps how too.
            true => {
                let linked = change.link(&name, &identity)?;
                if linked {
                    change.then_tell_kept("identity_linked", kept, told);
                }
                Ok(linked)
            }
            false => Err(gone.to_owned()),
        })
        .map_err(refused)?;
    match linked {
        true => Ok(format!("Whoever signs in as {written} is {name} from now on.")),
        // Theirs already: nothing to change, and nothing to refuse.
        false => Ok(format!("{written} is {name} already.")),
    }
}

/// What is kept of a link made in a browser, and what the plugins are
/// told of it: the log has the addresses it came from, the plugins do
/// not. `opened` is how the session it came through was opened, and
/// from where (`Server::session_opened`); `peer` where the browser came
/// back from.
fn link_said(
    name: &str,
    identity: &crate::state::Identity,
    opened: Option<(String, Option<String>)>,
    peer: &str,
) -> (Value, Value) {
    let what = format!("{name}: {}", identity.written());
    let (through, from) = opened.unwrap_or_else(|| ("opened before sessions said how".to_owned(), None));
    let how = format!("in a browser, through a session of {name} {through}");
    let from = from.map(|from| format!(" from {from}")).unwrap_or_default();
    let kept = format!("{what}, in a browser at {peer}, through a session of {name} {through}{from}");
    let kept = json!({ "user": name, "by": name, "what": kept });
    let told = json!({ "user": name, "identity": identity.key(), "by": name, "what": what, "how": how });
    (kept, told)
}

/// `GET /-/sign-in/<method>/callback`: the browser is back from the
/// provider. The plugin says who that is; if the server has such a
/// person, a session opens for them. Back from a link (`?link=1` when
/// they left), the account is attached to who asked instead, and they
/// are told so, or why not, in a page.
async fn finish_sign_in(
    State(http): State<Arc<Http>>,
    Segment(method): Segment<String>,
    RawQuery(query): RawQuery,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let query = pairs(query.as_deref().unwrap_or_default());
    // What was kept when they left: the plugin's state and, for a link,
    // the value that was made for it.
    let left_with = cookie(&headers, signing_in(http.secure)).map(|kept| match kept.split_once(':') {
        Some((state, value)) => (state, Some(value.to_owned())),
        None => (kept, None),
    });
    let came_back = left_with
        .filter(|(state, _)| !state.is_empty() && query.get("state").map(String::as_str) == Some(*state));
    let Some((state, link)) = came_back else {
        let again = format!("this sign-in did not start in this browser: start again at /-/sign-in/{method}");
        return said(StatusCode::FORBIDDEN, &again);
    };
    let Some(redirect) = http.callback(&headers, &method) else {
        return said(StatusCode::BAD_REQUEST, "the request does not say which host it is for");
    };
    let told = json!({ "kind": "browser", "state": state, "query": query, "redirect": redirect });
    let forget = signing_in_line(http.secure, "", 0);
    let mut response = match link {
        Some(value) => {
            let sent = cookie(&headers, COOKIE).map(hash_of);
            let linked = tokio::task::spawn_blocking(move || {
                link_account(&server, &method, &told, &value, (sent, &peer))
            })
            .await;
            match linked {
                Ok(Ok(happened)) => page("Linked", &happened, "Go on", true).into_response(),
                // A page, not JSON: a person reads it.
                Ok(Err((status, why))) => {
                    let why = match status {
                        StatusCode::INTERNAL_SERVER_ERROR => went_wrong(&why),
                        _ => why,
                    };
                    let happened = format!("Nothing was linked: {why}.");
                    (status, page("Not linked", &happened, "Go back", false)).into_response()
                }
                Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        None => {
            let opened = tokio::task::spawn_blocking(
                move || -> Result<(String, String, u64), (StatusCode, String)> {
                    let answer = server
                        .sign_in_step(&method, &["finish"], &told)
                        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
                    let identity =
                        identity_of(&method, &answer).map_err(|why| (StatusCode::FORBIDDEN, why))?;
                    let name = server.signed_in(&identity).map_err(|why| (StatusCode::FORBIDDEN, why))?;
                    // Theirs still when the session is written: not
                    // unlinked while the provider was asked.
                    let with = identity.written();
                    let token = server
                        .open_session(&name, (&with, Some(&peer.0)), |now| now.signs_in_as(&identity, &name))
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
                        .ok_or_else(|| (StatusCode::FORBIDDEN, Server::nobody(&identity)))?;
                    Ok((token, name, server.config.sign_in.days * 24 * 3600))
                },
            )
            .await;
            match opened {
                Ok(Ok((token, name, seconds))) => {
                    // A page, and not a redirect: the session's cookie is for
                    // requests that start on this site, and one that follows a
                    // redirect from the provider's does not. The page is where
                    // the next one starts.
                    let mut response =
                        page("Signed in", &format!("Signed in as {name}."), "Go on", true).into_response();
                    if let Ok(value) = HeaderValue::from_str(&http.cookie_line(&token, seconds)) {
                        response.headers_mut().append(header::SET_COOKIE, value);
                    }
                    response
                }
                Ok(Err((status, why))) => said(status, &why),
                Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
    };
    if let Ok(value) = HeaderValue::from_str(&forget) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
struct Call {
    method: String,
    #[serde(default)]
    params: Value,
}

/// `POST /-/api/<repository>`: a method of the daemon's API, answered as
/// the daemon would answer the person who asks.
async fn api(
    State(http): State<Arc<Http>>,
    Segment(repo): Segment<String>,
    headers: HeaderMap,
    Json(call): Json<Call>,
) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    // A plugin that serves pages asks as itself, with the secret the
    // server gave it, and with what it was given (ADR 0020).
    if let Some(plugin) = headers.get("x-kito-plugin").and_then(|value| value.to_str().ok()) {
        let Some((name, given)) = http.plugin_of(plugin) else {
            return said(StatusCode::UNAUTHORIZED, "this is not the secret of a plugin that runs here");
        };
        let who = format!("plugin:{name}");
        // It may use every repository, as what it was given there; of the
        // server itself it is given nothing.
        if let Some(why) = crate::session::not_for_a_plugin(&call.method, &given) {
            crate::audit::request(&server.dir, &who, Some(&repo), &call.method, false);
            return said(StatusCode::FORBIDDEN, &why);
        }
        let user = crate::state::User { server_role: crate::state::ServerRole::Admin, ..Default::default() };
        let (dir, method, place) = (server.dir.clone(), call.method.clone(), repo.clone());
        let asked = tokio::task::spawn_blocking(move || {
            let itself = crate::session::Asker { name: &name, user: &user, came: &Came::Otherwise };
            crate::session::ask(&server, &itself, &repo, &call.method, call.params, Some(&given))
        })
        .await;
        crate::audit::request(&dir, &who, Some(&place), &method, matches!(asked, Ok(Ok(_))));
        return match asked {
            Ok(Ok(value)) => answered(value),
            Ok(Err((code, message))) => (
                StatusCode::BAD_REQUEST,
                [(header::CACHE_CONTROL, "no-store")],
                Json(json!({ "error": message, "code": code })),
            )
                .into_response(),
            Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
    }
    let Some(who) = http.who(&server, &headers) else {
        return said(StatusCode::UNAUTHORIZED, "nobody is signed in: give a token, or open a session");
    };
    // A page of another site can send the cookie along, and cannot set
    // this: it marks the requests of the pages served here.
    if who.by_cookie && headers.get("x-kito").is_none() {
        return said(StatusCode::FORBIDDEN, "not a request of a page of this server");
    }
    // A browser asks through pages: of what the person may do, what the
    // plugins that serve them were given.
    let within = who.by_cookie.then(|| http.pages_may());
    let (dir, person, method, place) =
        (server.dir.clone(), who.name.clone(), call.method.clone(), repo.clone());
    let asked = tokio::task::spawn_blocking(move || {
        crate::session::ask(&server, &who.asker(), &repo, &call.method, call.params, within.as_deref())
    })
    .await;
    crate::audit::request(&dir, &person, Some(&place), &method, matches!(asked, Ok(Ok(_))));
    match asked {
        Ok(Ok(value)) => answered(value),
        Ok(Err((code, message))) => {
            let status = match code {
                rpc::REFUSED | rpc::METHOD_NOT_FOUND => StatusCode::FORBIDDEN,
                rpc::NO_SUCH_LANE | crate::session::NO_SUCH_REPO => StatusCode::NOT_FOUND,
                // The daemon could not be reached, as against: it said no.
                rpc::INTERNAL_ERROR if message.starts_with("the repository's daemon") => {
                    StatusCode::BAD_GATEWAY
                }
                _ => StatusCode::BAD_REQUEST,
            };
            (status, [(header::CACHE_CONTROL, "no-store")], Json(json!({ "error": message, "code": code })))
                .into_response()
        }
        Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `true` if `path` is `mount` or under it.
fn under(path: &str, mount: &str) -> bool {
    mount.is_empty() || path == mount || path.strip_prefix(mount).is_some_and(|rest| rest.starts_with('/'))
}

/// Everything that is not the server's own: passed on to the plugin whose
/// pages are mounted there.
async fn pass_on(State(http): State<Arc<Http>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if path == "/-" || path.starts_with("/-/") {
        return said(StatusCode::NOT_FOUND, "the server has nothing at this address");
    }
    let found = {
        let pages = http.pages.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        pages.iter().filter(|page| under(&path, &page.mount)).max_by_key(|page| page.mount.len()).cloned()
    };
    let Some(page) = found else {
        let text =
            "kito-server: no plugin serves pages here. `/-/me` says who you are; `/-/api/<repository>` \
                    answers for a repository.\n";
        return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text)
            .into_response();
    };
    let who = http.server().ok().and_then(|server| http.who(&server, request.headers()));
    let (mut parts, body) = request.into_parts();
    let rest = parts.uri.path_and_query().map_or("/", |rest| rest.as_str());
    let Ok(uri) = format!("http://{}{rest}", page.listen).parse() else {
        return said(StatusCode::BAD_REQUEST, "this address cannot be passed on");
    };
    parts.uri = uri;
    // What the plugin is told is the server's word: nothing the browser
    // sent in its place, and nothing that lets the plugin be the person.
    let ours: Vec<header::HeaderName> =
        parts.headers.keys().filter(|name| name.as_str().starts_with("x-kito-")).cloned().collect();
    for name in ours {
        parts.headers.remove(name);
    }
    parts.headers.remove(header::AUTHORIZATION);
    let kept: Vec<String> = parts
        .headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .filter(|pair| !pair.is_empty() && !own_cookie(pair.split('=').next().unwrap_or_default()))
        .map(str::to_owned)
        .collect();
    parts.headers.remove(header::COOKIE);
    if let Ok(value) = HeaderValue::from_str(&kept.join("; ")) {
        if !kept.is_empty() {
            parts.headers.insert(header::COOKIE, value);
        }
    }
    let mut tell = |name: &'static str, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            parts.headers.insert(name, value);
        }
    };
    tell("x-kito-secret", &page.secret);
    tell("x-kito-mount", if page.mount.is_empty() { "/" } else { &page.mount });
    if let Some(who) = &who {
        tell("x-kito-user", &who.name);
        tell("x-kito-role", who.user.server_role.word());
    }
    match http.client.request(Request::from_parts(parts, body)).await {
        Ok(response) => {
            let (mut parts, body) = response.into_parts();
            // A plugin sets no session of the server, and nothing the
            // server keeps for a sign-in.
            let theirs: Vec<HeaderValue> = parts
                .headers
                .get_all(header::SET_COOKIE)
                .iter()
                .filter(|value| {
                    !value.to_str().is_ok_and(|line| own_cookie(line.split('=').next().unwrap_or_default()))
                })
                .cloned()
                .collect();
            parts.headers.remove(header::SET_COOKIE);
            for value in theirs {
                parts.headers.append(header::SET_COOKIE, value);
            }
            Response::from_parts(parts, Body::new(body))
        }
        Err(_) => said(
            StatusCode::BAD_GATEWAY,
            &format!("the plugin `{}`, which serves this page, is not answering", page.plugin),
        ),
    }
}

impl Http {
    /// The plugin whose secret this is, and what it was given in a
    /// repository (ADR 0020).
    fn plugin_of(&self, said: &str) -> Option<(String, Vec<Permission>)> {
        let pages = self.pages.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        pages
            .iter()
            .find(|page| {
                page.secret.len() == said.len()
                    && page.secret.bytes().zip(said.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
            })
            .map(|page| {
                let given =
                    page.given.iter().copied().filter(|permission| permission.of_a_repository()).collect();
                (page.plugin.clone(), given)
            })
    }

    /// What pages may ask of a repository: what the plugins that serve
    /// them were given.
    fn pages_may(&self) -> Vec<Permission> {
        let pages = self.pages.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut may: Vec<Permission> = pages
            .iter()
            .flat_map(|page| page.given.iter().copied())
            .filter(|permission| permission.of_a_repository())
            .collect();
        may.sort();
        may.dedup();
        may
    }

    /// Brings the plugins that serve pages in line with what the server
    /// says now: one enabled is started, one disabled is stopped, one
    /// updated or given other permissions is started again. Run when the
    /// server starts, after a change from the browser, and every two
    /// seconds for a change made on the server's machine.
    fn settle_pages(&self) {
        let Ok(server) = Server::open(&self.dir) else { return };
        let wanted: Vec<(kitod::plugins::Installed, String, Vec<Permission>)> = server
            .running()
            .into_iter()
            .filter_map(|(found, config)| {
                let spec = found
                    .manifest
                    .pages
                    .as_ref()
                    .filter(|_| config.permissions.contains(&Permission::Pages))?;
                let mount = spec.mount.trim_end_matches('/').to_owned();
                let given = config.permissions.clone();
                Some((found, mount, given))
            })
            .collect();
        let mut pages = self.pages.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        pages.retain(|page| {
            let same = wanted.iter().any(|(found, mount, given)| {
                found.manifest.name == page.plugin
                    && found.manifest.version == page.version
                    && *mount == page.mount
                    && *given == page.given
            });
            if !same {
                page.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                eprintln!("kito-server: the pages of `{}` are no longer served", page.plugin);
            }
            same
        });
        for (found, mount, given) in wanted {
            if pages.iter().any(|page| page.plugin == found.manifest.name) {
                continue;
            }
            match start_pages(&server.dir, &found, mount, given, &self.local) {
                Ok(page) => {
                    eprintln!("kito-server: pages: {} at {}/", page.plugin, page.mount);
                    pages.push(page);
                }
                Err(e) => eprintln!("kito-server: {e}"),
            }
        }
    }
}

/// Starts a plugin that serves pages, listening on this machine, and keeps
/// it running until it is told to stop. The server holds its standard
/// input open: when it closes, the server is gone or the plugin was
/// disabled, and the plugin stops.
fn start_pages(
    dir: &std::path::Path,
    found: &kitod::plugins::Installed,
    mount: String,
    given: Vec<Permission>,
    local: &str,
) -> Result<Pages, String> {
    use std::sync::atomic::Ordering;
    let name = found.manifest.name.clone();
    // A port nobody has, on this machine only.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map_err(|e| format!("a port for the pages of `{name}`: {e}"))?
        .port();
    let (listen, secret) = (format!("127.0.0.1:{port}"), random_hex()?);
    let program = kitod::plugins::program(&found.dir, &name);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (dir, told, stopped) =
        (dir.to_path_buf(), (listen.clone(), secret.clone(), name.clone()), stop.clone());
    let local = local.to_owned();
    std::thread::spawn(move || {
        let pause = std::time::Duration::from_millis(200);
        while !stopped.load(Ordering::SeqCst) {
            let child = std::process::Command::new(&program)
                .arg("serve-pages")
                .current_dir(&dir)
                .env(ENV_SERVER_DIR, &dir)
                .env(kitod::plugins::ENV_API, rpc::API.to_string())
                .env(ENV_PAGES_LISTEN, &told.0)
                .env(ENV_PAGES_SECRET, &told.1)
                .env(ENV_SERVER_API, &local)
                .stdin(std::process::Stdio::piped())
                .spawn();
            match child {
                Ok(mut child) => {
                    // Held open while it runs: a closed input tells the
                    // plugin the server is gone.
                    let held = child.stdin.take();
                    loop {
                        if stopped.load(Ordering::SeqCst) {
                            drop(held);
                            // A moment to finish what it was answering.
                            for _ in 0..10 {
                                if matches!(child.try_wait(), Ok(Some(_))) {
                                    return;
                                }
                                std::thread::sleep(pause);
                            }
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                        match child.try_wait() {
                            Ok(None) => std::thread::sleep(pause),
                            Ok(Some(status)) => {
                                eprintln!(
                                    "kito-server: the plugin `{}`, which serves pages, stopped ({status}): started again",
                                    told.2
                                );
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(e) => eprintln!("kito-server: the plugin `{}`: {}: {e}", told.2, program.display()),
            }
            for _ in 0..10 {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(pause);
            }
        }
    });
    Ok(Pages { plugin: name, version: found.manifest.version.clone(), given, mount, listen, secret, stop })
}

/// Serves HTTP on `address` until the process ends: encrypted with the
/// server's certificate, or (`plain`) in the clear on this machine only.
/// Says where it listens in one line on standard output.
pub fn serve(server: &Server, address: &str, plain: bool) -> Result<(), String> {
    let listener = std::net::TcpListener::bind(address).map_err(|e| format!("{address}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    if plain && !bound.ip().is_loopback() {
        return Err(format!(
            "{bound} is reachable from other machines: HTTP that is not encrypted (--plain) is for this \
             machine only"
        ));
    }
    let tls = match plain {
        true => None,
        false => Some(tokio_rustls::TlsAcceptor::from(crate::tls::config(&server.dir)?)),
    };
    // On this machine, in the clear, for the server's own plugins: the
    // encrypted address is for the server's name, not for 127.0.0.1.
    let local_listener = match plain {
        true => None,
        false => Some(std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("127.0.0.1: {e}"))?),
    };
    let local = match &local_listener {
        Some(listener) => format!("http://{}", listener.local_addr().map_err(|e| e.to_string())?),
        None => format!("http://{bound}"),
    };
    let http = Arc::new(Http {
        dir: server.dir.clone(),
        secure: !plain,
        pages: std::sync::RwLock::new(Vec::new()),
        client: Client::builder(TokioExecutor::new()).build_http(),
        local,
    });
    http.settle_pages();
    let mounted: Vec<String> = {
        let pages = http.pages.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        pages.iter().map(|page| format!("{} at {}/", page.plugin, page.mount)).collect()
    };
    // A plugin enabled or disabled on the server's machine counts at once.
    let settling = http.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        settling.settle_pages();
    });
    let router = Router::new()
        .route("/-/me", get(me))
        .route("/-/session", post(open_session).delete(close_session))
        .route("/-/api/{repo}", post(api))
        .route("/-/admin/plugins", get(admin_plugins).post(admin_change))
        .route("/-/people", post(people))
        .route("/-/admin/backups/{name}", get(download_backup))
        .route("/-/sign-in", get(ways))
        .route("/-/sign-in/{method}", get(begin_sign_in))
        .route("/-/sign-in/{method}/callback", get(finish_sign_in))
        .fallback(pass_on)
        .with_state(http);

    use std::io::Write as _;
    let scheme = if plain { "http" } else { "https" };
    let serves =
        if mounted.is_empty() { "no pages".to_owned() } else { format!("pages: {}", mounted.join(", ")) };
    let mut out = std::io::stdout();
    let _ = writeln!(out, "kito-server: {scheme}://{bound}, {serves}");
    let _ = out.flush();

    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    if let Some(local) = &local_listener {
        local.set_nonblocking(true).map_err(|e| e.to_string())?;
    }
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => return eprintln!("kito-server: HTTP is not served: {e}"),
        };
        runtime.block_on(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(e) => return eprintln!("kito-server: HTTP is not served: {e}"),
            };
            if let Some(local) =
                local_listener.and_then(|local| tokio::net::TcpListener::from_std(local).ok())
            {
                let router = router.clone();
                tokio::spawn(async move {
                    loop {
                        let Ok((stream, peer)) = local.accept().await else { continue };
                        let router = router.clone().layer(Extension(Peer(peer.ip().to_string())));
                        tokio::spawn(async move {
                            let service = hyper_util::service::TowerToHyperService::new(router);
                            let serving = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                            let _ = serving.serve_connection(TokioIo::new(stream), service).await;
                        });
                    }
                });
            }
            loop {
                let Ok((stream, peer)) = listener.accept().await else { continue };
                // Where each request of the connection came from, for what is kept of it.
                let router = router.clone().layer(Extension(Peer(peer.ip().to_string())));
                let tls = tls.clone();
                tokio::spawn(async move {
                    let service = hyper_util::service::TowerToHyperService::new(router);
                    let serving = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                    match tls {
                        Some(tls) => {
                            if let Ok(stream) = tls.accept(stream).await {
                                let _ = serving.serve_connection(TokioIo::new(stream), service).await;
                            }
                        }
                        None => {
                            let _ = serving.serve_connection(TokioIo::new(stream), service).await;
                        }
                    }
                });
            }
        });
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugins_hear_of_a_link_without_the_addresses_the_log_keeps() {
        let identity = crate::state::Identity {
            provider: "hello".to_owned(),
            subject: "id-mallory".to_owned(),
            login: "mallory".to_owned(),
            name: None,
            email: None,
        };
        let opened =
            Some(("opened with a token on 2026-11-23 10:12:05 UTC".to_owned(), Some("203.0.113.9".into())));
        let (kept, told) = link_said("bob", &identity, opened, "198.51.100.7");
        assert_eq!(
            kept["what"],
            "bob: hello:mallory, in a browser at 198.51.100.7, through a session of bob opened with a token \
             on 2026-11-23 10:12:05 UTC from 203.0.113.9"
        );
        assert_eq!(told["what"], "bob: hello:mallory");
        assert_eq!(
            told["how"],
            "in a browser, through a session of bob opened with a token on 2026-11-23 10:12:05 UTC"
        );
        let told = told.to_string();
        assert!(!told.contains("198.51.100.7") && !told.contains("203.0.113.9"), "{told}");
    }

    #[test]
    fn the_sign_in_cookie_is_named_for_how_the_server_is_served() {
        // In the clear: the name it always had, for the paths of signing in.
        assert_eq!(
            signing_in_line(false, "left-1", 600),
            "kito_sign_in=left-1; Path=/-/sign-in; HttpOnly; SameSite=Lax; Max-Age=600"
        );
        // Served securely: `__Host-`, which a browser takes only with
        // `Secure`, for every path, and with no `Domain`.
        let line = signing_in_line(true, "left-1:5f0e", 600);
        assert_eq!(
            line,
            "__Host-kito_sign_in=left-1:5f0e; Path=/; HttpOnly; SameSite=Lax; Max-Age=600; Secure"
        );
        assert!(!line.to_lowercase().contains("domain"), "{line}");
        // Forgotten under the name it was set with, and with what it was set with.
        assert_eq!(
            signing_in_line(true, "", 0),
            "__Host-kito_sign_in=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; Secure"
        );
        assert_eq!(
            signing_in_line(false, "", 0),
            "kito_sign_in=; Path=/-/sign-in; HttpOnly; SameSite=Lax; Max-Age=0"
        );
        // And read under it: one planted under the other name is not it.
        let mut headers = HeaderMap::new();
        let sent = "kito_sign_in=planted; __Host-kito_sign_in=left-1:5f0e; theme=dark";
        headers.insert(header::COOKIE, HeaderValue::from_static(sent));
        assert_eq!(cookie(&headers, signing_in(true)), Some("left-1:5f0e"));
        assert_eq!(cookie(&headers, signing_in(false)), Some("planted"));
        // Neither is a plugin's to be sent or to set, nor is the session's.
        for own in ["kito_sign_in", "__Host-kito_sign_in", " kito_session"] {
            assert!(own_cookie(own), "{own}");
        }
        assert!(!own_cookie("theme") && !own_cookie("kito_sign_in_2"));
    }
}
