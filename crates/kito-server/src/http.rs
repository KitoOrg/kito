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

use crate::state::{identity_of, Role, Server, ENV_SERVER_DIR, SESSION_PREFIX};
use axum::body::Body;
use axum::extract::{Path as Segment, RawQuery, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
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
/// The cookie of somebody on their way to sign in and back: what the
/// plugin handed out when they left, for ten minutes.
const SIGNING_IN: &str = "kito_sign_in";
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
    (status, [(header::CACHE_CONTROL, "no-store")], Json(json!({ "error": message }))).into_response()
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
}

impl Http {
    /// The server as it is now: people and repositories added since it
    /// started count for a request made now.
    fn server(&self) -> Result<Server, String> {
        Server::open(&self.dir)
    }

    fn who(&self, server: &Server, headers: &HeaderMap) -> Option<Who> {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if let Some(token) = bearer {
            let (name, user) = server.user_of(token.trim())?;
            server.saw(&name);
            return Some(Who { name, user, by_cookie: false });
        }
        // A session and nothing else: the token an administrator handed
        // out is not something a browser keeps.
        let id = cookie(headers, COOKIE).filter(|id| id.starts_with(SESSION_PREFIX))?;
        let (name, user) = server.user_of(id)?;
        server.saw(&name);
        Some(Who { name, user, by_cookie: true })
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

/// `/-/me`: who is asking, and what they may use.
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
/// (ADR 0014): `server.people`, `server.people_add`, `_remove`, `_role`.
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
        crate::session::people(&server, &who.name, &who.user, None, None, &call.method, &call.params)
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
async fn open_session(State(http): State<Arc<Http>>, Json(opening): Json<Opening>) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    // The same answer for no token and for a wrong one.
    let Some((name, _)) = server.user_of(opening.token.trim()) else {
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
    let id = match server.open_session(&name) {
        Ok(id) => id,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let seconds = server.config.sign_in.days * 24 * 3600;
    let headers =
        [(header::SET_COOKIE, http.cookie_line(&id, seconds)), (header::CACHE_CONTROL, "no-store".into())];
    (headers, Json(json!({ "user": name }))).into_response()
}

/// `DELETE /-/session`: the session ends.
async fn close_session(State(http): State<Arc<Http>>, headers: HeaderMap) -> Response {
    if let (Some(id), Ok(server)) = (cookie(&headers, COOKIE).map(str::to_owned), http.server()) {
        // The plugins that listen are told, which may take them a moment.
        let _ = tokio::task::spawn_blocking(move || server.end_session(&id)).await;
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

/// `GET /-/sign-in/<method>`: sends the browser where the plugin says the
/// person signs in. What the plugin handed out for the way back is kept
/// in a cookie of this browser, so that the one who comes back is the
/// one who left.
async fn begin_sign_in(
    State(http): State<Arc<Http>>,
    Segment(method): Segment<String>,
    headers: HeaderMap,
) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let Some(redirect) = http.callback(&headers, &method) else {
        return said(StatusCode::BAD_REQUEST, "the request does not say which host it is for");
    };
    let told = json!({ "kind": "browser", "redirect": redirect });
    let begun = tokio::task::spawn_blocking(move || server.sign_in_step(&method, &["begin"], &told)).await;
    let begun = match begun {
        Ok(Ok(begun)) => begun,
        Ok(Err(e)) => return said(StatusCode::NOT_FOUND, &e),
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if let Some(why) = begun["refused"].as_str() {
        return said(StatusCode::FORBIDDEN, why);
    }
    // What goes in a cookie as it is, and nothing else.
    let plain = |state: &&str| {
        !state.is_empty()
            && state.len() <= 512
            && state.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
    };
    let (Some(open), Some(state)) = (begun["open"].as_str(), begun["state"].as_str().filter(plain)) else {
        return said(StatusCode::BAD_GATEWAY, "the plugin did not say where to sign in");
    };
    let secure = if http.secure { "; Secure" } else { "" };
    // Lax: the way back starts on the provider's site.
    let kept = format!("{SIGNING_IN}={state}; Path=/-/sign-in; HttpOnly; SameSite=Lax; Max-Age=600{secure}");
    let headers = [
        (header::LOCATION, open.to_owned()),
        (header::SET_COOKIE, kept),
        (header::CACHE_CONTROL, "no-store".to_owned()),
    ];
    (StatusCode::FOUND, headers).into_response()
}

/// `GET /-/sign-in/<method>/callback`: the browser is back from the
/// provider. The plugin says who that is; if the server has such a
/// person, a session opens for them.
async fn finish_sign_in(
    State(http): State<Arc<Http>>,
    Segment(method): Segment<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let server = match http.server() {
        Ok(server) => server,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let query = pairs(query.as_deref().unwrap_or_default());
    let left_with = cookie(&headers, SIGNING_IN).filter(|state| !state.is_empty());
    let Some(state) = left_with.filter(|state| query.get("state").map(String::as_str) == Some(state)) else {
        let again = format!("this sign-in did not start in this browser: start again at /-/sign-in/{method}");
        return said(StatusCode::FORBIDDEN, &again);
    };
    let Some(redirect) = http.callback(&headers, &method) else {
        return said(StatusCode::BAD_REQUEST, "the request does not say which host it is for");
    };
    let told = json!({ "kind": "browser", "state": state, "query": query, "redirect": redirect });
    let opened =
        tokio::task::spawn_blocking(move || -> Result<(String, String, u64), (StatusCode, String)> {
            let answer =
                server.sign_in_step(&method, &["finish"], &told).map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            let identity = identity_of(&method, &answer).map_err(|why| (StatusCode::FORBIDDEN, why))?;
            let name = server.signed_in(&identity).map_err(|why| (StatusCode::FORBIDDEN, why))?;
            let token = server.open_session(&name).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            Ok((token, name, server.config.sign_in.days * 24 * 3600))
        })
        .await;
    let secure = if http.secure { "; Secure" } else { "" };
    let forget = format!("{SIGNING_IN}=; Path=/-/sign-in; HttpOnly; SameSite=Lax; Max-Age=0{secure}");
    let mut response = match opened {
        Ok(Ok((token, name, seconds))) => {
            // A page, and not a redirect: the session's cookie is for
            // requests that start on this site, and one that follows a
            // redirect from the provider's does not. The page is where
            // the next one starts.
            let name = name.replace('&', "&amp;").replace('<', "&lt;");
            let page = format!(
                "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0;url=/\">\
                 <title>Signed in</title><p>Signed in as {name}. <a href=\"/\">Go on</a>.</p>"
            );
            let mut response = axum::response::Html(page).into_response();
            if let Ok(value) = HeaderValue::from_str(&http.cookie_line(&token, seconds)) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            response
        }
        Ok(Err((status, why))) => said(status, &why),
        Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
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
            crate::session::ask(&server, &name, &user, &repo, &call.method, call.params, Some(&given))
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
        crate::session::ask(
            &server,
            &who.name,
            &who.user,
            &repo,
            &call.method,
            call.params,
            within.as_deref(),
        )
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
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some(COOKIE))
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
            // A plugin sets no session of the server.
            let theirs: Vec<HeaderValue> = parts
                .headers
                .get_all(header::SET_COOKIE)
                .iter()
                .filter(|value| {
                    !value.to_str().is_ok_and(|line| line.trim_start().starts_with(&format!("{COOKIE}=")))
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
                        let Ok((stream, _)) = local.accept().await else { continue };
                        let router = router.clone();
                        tokio::spawn(async move {
                            let service = hyper_util::service::TowerToHyperService::new(router);
                            let serving = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                            let _ = serving.serve_connection(TokioIo::new(stream), service).await;
                        });
                    }
                });
            }
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let (router, tls) = (router.clone(), tls.clone());
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
