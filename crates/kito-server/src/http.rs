//! The server's HTTP address (ADR 0006).
//!
//! The server serves no pages of its own. Under `/-/` it answers what
//! only it can: who is asking, and a repository's daemon asked as that
//! person. Everything else is passed on to the plugin whose pages are
//! mounted there, a program of its own that the server starts and keeps
//! running.
//!
//! Who asks is known by a token (`Authorization: Bearer`, as the command
//! line has one) or by the cookie of a session the server opened.

use crate::state::{Server, ENV_SERVER_DIR};
use axum::body::Body;
use axum::extract::{Path as Segment, Request, State};
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
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The cookie of a session.
const COOKIE: &str = "kito_session";
/// How long a session lasts.
const SESSION_DAYS: u64 = 30;
/// In the environment of a plugin that serves pages: where to listen, and
/// what the server says with every request it passes on.
pub const ENV_PAGES_LISTEN: &str = "KITO_PAGES_LISTEN";
pub const ENV_PAGES_SECRET: &str = "KITO_PAGES_SECRET";

/// A plugin that serves pages, as the server reaches it.
struct Pages {
    plugin: String,
    /// The path its pages are under, without a `/` at its end.
    mount: String,
    /// Where it listens, on this machine.
    listen: String,
    secret: String,
}

struct Http {
    dir: PathBuf,
    /// Encrypted: cookies are for encrypted connections only.
    secure: bool,
    /// Sessions, by what the cookie holds: who, and until when (ms).
    sessions: Mutex<HashMap<String, (String, u64)>>,
    pages: Vec<Pages>,
    /// What pages may ask of a repository: what the plugins that serve
    /// them were given.
    pages_may: Vec<Permission>,
    client: Client<HttpConnector, Body>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
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
            return Some(Who { name, user, by_cookie: false });
        }
        let id = cookie(headers, COOKIE)?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let (name, until) = sessions.get(id).cloned()?;
        // Ended, or of somebody who was removed since.
        let user = server.config.users.get(&name).cloned().filter(|_| until > now_ms());
        match user {
            Some(user) => Some(Who { name, user, by_cookie: true }),
            None => {
                sessions.remove(id);
                None
            }
        }
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
        .map(|repo| json!({ "name": repo, "role": who.user.role_in(&repo).word() }))
        .collect();
    answered(json!({ "user": who.name, "email": who.user.email, "repos": repos }))
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
        return said(StatusCode::UNAUTHORIZED, "this token is not one the server knows");
    };
    let id = match random_hex() {
        Ok(id) => id,
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let seconds = SESSION_DAYS * 24 * 3600;
    let mut sessions = http.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    sessions.retain(|_, (_, until)| *until > now);
    sessions.insert(id.clone(), (name.clone(), now + seconds * 1000));
    drop(sessions);
    let headers =
        [(header::SET_COOKIE, http.cookie_line(&id, seconds)), (header::CACHE_CONTROL, "no-store".into())];
    (headers, Json(json!({ "user": name }))).into_response()
}

/// `DELETE /-/session`: the session ends.
async fn close_session(State(http): State<Arc<Http>>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie(&headers, COOKIE) {
        http.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }
    ([(header::SET_COOKIE, http.cookie_line("", 0))], Json(json!({}))).into_response()
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
    let within = who.by_cookie.then(|| http.pages_may.clone());
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
    let Some(page) =
        http.pages.iter().filter(|page| under(&path, &page.mount)).max_by_key(|page| page.mount.len())
    else {
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

/// Starts the plugins that serve pages, each listening on this machine,
/// and keeps them running. The server holds their standard input open:
/// when it closes, the server is gone, and a plugin stops.
fn start_pages(server: &Server) -> Result<(Vec<Pages>, Vec<Permission>), String> {
    let (mut pages, mut may) = (Vec::new(), Vec::new());
    for (found, config) in server.running() {
        let Some(spec) =
            found.manifest.pages.as_ref().filter(|_| config.permissions.contains(&Permission::Pages))
        else {
            continue;
        };
        let name = found.manifest.name.clone();
        // A port nobody has, on this machine only.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .map_err(|e| format!("a port for the pages of `{name}`: {e}"))?
            .port();
        let (listen, secret) = (format!("127.0.0.1:{port}"), random_hex()?);
        let program = kitod::plugins::program(&found.dir, &name);
        let (dir, told) = (server.dir.clone(), (listen.clone(), secret.clone(), name.clone()));
        std::thread::spawn(move || loop {
            let child = std::process::Command::new(&program)
                .arg("serve-pages")
                .current_dir(&dir)
                .env(ENV_SERVER_DIR, &dir)
                .env(kitod::plugins::ENV_API, rpc::API.to_string())
                .env(ENV_PAGES_LISTEN, &told.0)
                .env(ENV_PAGES_SECRET, &told.1)
                .stdin(std::process::Stdio::piped())
                .spawn();
            match child {
                Ok(mut child) => {
                    // Held open while it runs: `wait` would close it, and
                    // a closed input tells the plugin the server is gone.
                    let held = child.stdin.take();
                    let status = child.wait();
                    drop(held);
                    eprintln!("kito-server: the plugin `{}`, which serves pages, stopped ({status:?}): started again", told.2);
                }
                Err(e) => eprintln!("kito-server: the plugin `{}`: {}: {e}", told.2, program.display()),
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        may.extend(config.permissions.iter().copied().filter(|permission| permission.of_a_repository()));
        pages.push(Pages {
            plugin: name,
            mount: spec.mount.trim_end_matches('/').to_owned(),
            listen,
            secret,
        });
    }
    may.sort();
    may.dedup();
    Ok((pages, may))
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
    let (pages, pages_may) = start_pages(server)?;
    let mounted: Vec<String> =
        pages.iter().map(|page| format!("{} at {}/", page.plugin, page.mount)).collect();
    let http = Arc::new(Http {
        dir: server.dir.clone(),
        secure: !plain,
        sessions: Mutex::default(),
        pages,
        pages_may,
        client: Client::builder(TokioExecutor::new()).build_http(),
    });
    let router = Router::new()
        .route("/-/me", get(me))
        .route("/-/session", post(open_session).delete(close_session))
        .route("/-/api/{repo}", post(api))
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
