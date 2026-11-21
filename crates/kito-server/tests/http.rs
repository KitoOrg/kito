//! The server's HTTP address (ADR 0006): who asks, a repository asked as
//! that person, and pages passed on to a plugin.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn kitod() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let path = Path::new(env!("CARGO_BIN_EXE_kito-server"))
        .with_file_name(format!("kitod{}", std::env::consts::EXE_SUFFIX));
    BUILD.call_once(|| {
        if !path.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "kitod", "--bin", "kitod"])
                .status()
                .unwrap();
            assert!(status.success(), "building kitod");
        }
    });
    path
}

fn server(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kito-server"));
    command
        .arg("--dir")
        .arg(dir)
        .args(args)
        .env("KITOD_BIN", kitod())
        .env("KITO_AUTHOR", "the server")
        .env_remove("KITO_EMAIL")
        .env("KITO_CONFIG_DIR", dir.join(".personal-config"));
    command
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = server(dir, args).output().unwrap();
    let (said, errors) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "kito-server {args:?}: {said}{errors}");
    said.into_owned()
}

fn token_in(printed: &str) -> String {
    printed.lines().find(|line| line.starts_with("kito_")).expect(printed).trim().to_owned()
}

/// The plugin of the tests, installed for the user the server runs as.
fn install_hello(dir: &Path, manifest: &str) {
    let name = format!("kito-hello{}", std::env::consts::EXE_SUFFIX);
    let built = Path::new(env!("CARGO_BIN_EXE_kito-server")).with_file_name("examples").join(&name);
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        if !built.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "kito-cli", "--example", "kito-hello"])
                .status()
                .unwrap();
            assert!(status.success(), "building kito-hello");
        }
    });
    let plugin = dir.join(".personal-config/plugins/hello");
    fs::create_dir_all(&plugin).unwrap();
    fs::copy(built, plugin.join(&name)).unwrap();
    fs::write(plugin.join("kito-plugin.toml"), manifest).unwrap();
}

/// A server with a repository, ada who may integrate and carol who may
/// read, serving in the clear on this machine.
struct Served {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    child: Child,
    http: String,
    ada: String,
    carol: String,
}

struct Answer {
    status: u16,
    head: String,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

impl Served {
    fn start(setup: impl FnOnce(&Path)) -> Served {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap().join("server");
        ok(tmp.path(), &["init", dir.to_str().unwrap()]);
        ok(&dir, &["repo", "add", "shop"]);
        // Something for a lane to start from.
        let shop = dir.join("repos/shop");
        fs::write(
            shop.join(".gitignore"),
            ".kito/
",
        )
        .unwrap();
        fs::write(
            shop.join("shop.ts"),
            "export const price = 1;
",
        )
        .unwrap();
        let mut repo = kitod::Repo::open(&shop).unwrap();
        repo.checkpoint("first version").unwrap();
        repo.close().unwrap();
        let ada = token_in(&ok(&dir, &["user", "add", "ada"]));
        let carol = token_in(&ok(&dir, &["user", "add", "carol", "--role", "read"]));
        setup(&dir);
        let mut child =
            server(&dir, &["serve", "--plain", "--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        // kito-server: http://127.0.0.1:<port>, <pages>
        let http = line
            .trim()
            .strip_prefix("kito-server: http://")
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_else(|| panic!("{line:?}"))
            .to_owned();
        Served { _tmp: tmp, dir, child, http, ada, carol }
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>) -> Answer {
        let mut stream = TcpStream::connect(&self.http).unwrap();
        let body = body.map(Value::to_string).unwrap_or_default();
        let mut text = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", self.http);
        for (name, value) in headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            text.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
        }
        text.push_str("\r\n");
        stream.write_all(text.as_bytes()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).unwrap();
        let answer = String::from_utf8_lossy(&answer).into_owned();
        let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
        let status = head.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
        Answer { status, head: head.to_lowercase(), body: body.to_owned() }
    }

    /// A method of a repository's daemon, asked with a token.
    fn ask(&self, token: &str, repo: &str, method: &str, params: Value) -> Answer {
        let bearer = format!("Bearer {token}");
        let call = json!({ "method": method, "params": params });
        self.request("POST", &format!("/-/api/{repo}"), &[("Authorization", &bearer)], Some(&call))
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(mut daemon) = kitod::client::Client::connect(&self.dir.join("repos/shop")) {
            let _ = daemon.call_value("shutdown", json!({}));
        }
    }
}

#[test]
fn a_person_is_known_by_a_token_or_by_a_session_and_asks_as_themselves() {
    let served = Served::start(|_| {});

    // Nobody: nothing of the server is told.
    let nobody = served.request("GET", "/-/me", &[], None);
    assert_eq!(nobody.status, 401, "{}", nobody.body);
    let call = json!({ "method": "lanes" });
    assert_eq!(served.request("POST", "/-/api/shop", &[], Some(&call)).status, 401);
    assert_eq!(served.request("GET", "/-/me", &[("Authorization", "Bearer kito_wrong")], None).status, 401);

    // By a token, as the command line has one.
    let bearer = format!("Bearer {}", served.ada);
    let me = served.request("GET", "/-/me", &[("Authorization", &bearer)], None).json();
    assert_eq!(me["user"], "ada");
    assert_eq!(me["repos"], json!([{ "name": "shop", "role": "integrate" }]));

    // A repository's daemon, asked as that person: what their role allows.
    assert_eq!(served.ask(&served.ada, "shop", "lanes", json!({})).json(), json!([]));
    let opened = served.ask(
        &served.ada,
        "shop",
        "open_lane",
        json!({ "agent": "alice", "intent": "a cheaper price", "scope": ["./"] }),
    );
    assert_eq!(opened.status, 200, "{}", opened.body);
    assert_eq!(
        (opened.json()["id"].as_str(), opened.json()["owner"].as_str()),
        (Some("lane/1"), Some("ada"))
    );
    assert_eq!(served.ask(&served.carol, "shop", "lanes", json!({})).json()[0]["id"], "lane/1");
    let refused = served.ask(&served.carol, "shop", "close_lane", json!({ "lane": "1" }));
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(refused.body.contains("carol may read this repository"), "{}", refused.body);
    // What a client may not ask through the server, it may not ask here.
    assert_eq!(served.ask(&served.ada, "shop", "shutdown", json!({})).status, 403);
    // The same answer for a repository that is not there and one that is not theirs.
    assert_eq!(served.ask(&served.ada, "nothing", "lanes", json!({})).status, 404);
    assert_eq!(served.ask(&served.ada, "shop", "lane_diff", json!({ "lane": "9" })).status, 404);

    // A session, for a browser: opened with a token, kept in a cookie
    // that scripts cannot read and other sites cannot send.
    let wrong = served.request("POST", "/-/session", &[], Some(&json!({ "token": "kito_wrong" })));
    assert_eq!(wrong.status, 401, "{}", wrong.body);
    let opened = served.request("POST", "/-/session", &[], Some(&json!({ "token": served.carol })));
    assert_eq!(opened.status, 200, "{}", opened.body);
    let line =
        opened.head.lines().find(|line| line.starts_with("set-cookie: kito_session=")).expect(&opened.head);
    assert!(
        line.contains("httponly") && line.contains("samesite=strict") && line.contains("path=/"),
        "{line}"
    );
    let cookie = line.trim_start_matches("set-cookie: ").split(';').next().unwrap().to_owned();
    let me = served.request("GET", "/-/me", &[("Cookie", &cookie)], None).json();
    assert_eq!((me["user"].as_str(), me["repos"][0]["role"].as_str()), (Some("carol"), Some("read")));

    // With a cookie, only the requests of a page of this server; and with
    // no plugin serving pages, pages may ask nothing of a repository.
    let lanes = json!({ "method": "lanes" });
    let foreign = served.request("POST", "/-/api/shop", &[("Cookie", &cookie)], Some(&lanes));
    assert_eq!(foreign.status, 403, "{}", foreign.body);
    let page = served.request("POST", "/-/api/shop", &[("Cookie", &cookie), ("X-Kito", "1")], Some(&lanes));
    assert_eq!(page.status, 403, "{}", page.body);
    assert!(page.body.contains("no plugin that serves them was given `read`"), "{}", page.body);

    // Removed, a person's session is nobody's; ended, it is gone.
    ok(&served.dir, &["user", "remove", "carol"]);
    assert_eq!(served.request("GET", "/-/me", &[("Cookie", &cookie)], None).status, 401);
    let ended = served.request("DELETE", "/-/session", &[("Cookie", &cookie)], None);
    assert!(ended.head.contains("set-cookie: kito_session=;"), "{}", ended.head);

    // No plugin serves pages: the address says so, in a line.
    let none = served.request("GET", "/", &[], None);
    assert_eq!(none.status, 404);
    assert!(none.body.contains("no plugin serves pages here"), "{}", none.body);
    assert_eq!(served.request("GET", "/-/nothing", &[], None).status, 404);
}

const PAGES: &str = r#"name = "hello"
version = "0.5.0"
api = 1
permissions = ["pages", "read"]

[pages]
mount = "/"
"#;

#[test]
fn pages_are_passed_on_to_the_plugin_that_serves_them() {
    let served = Served::start(|dir| {
        install_hello(dir, PAGES);
        let said = ok(dir, &["plugin", "enable", "hello"]);
        assert!(said.contains("It may: read, pages.") && said.contains("pages under /"), "{said}");
    });
    // The plugin is a program of its own, started by the server: it may
    // take a moment to listen.
    let page = |path: &str, headers: &[(&str, &str)]| {
        for _ in 0..200 {
            let answer = served.request("GET", path, headers, None);
            if answer.status != 502 {
                return answer;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("the plugin never answered");
    };

    // To nobody, the page is the plugin's to show or not: it is told
    // nobody is signed in, and that it is the server asking.
    let anonymous = page("/lanes?open=1", &[]);
    assert_eq!(anonymous.status, 200, "{}", anonymous.body);
    assert!(
        anonymous.body.contains("hello pages\npath=/lanes?open=1\nuser=-\nsecret=ok\nmount=/\n"),
        "{}",
        anonymous.body
    );

    // Signed in, it is told who; never with what would let it be them,
    // and never believed about who it is.
    let opened = served.request("POST", "/-/session", &[], Some(&json!({ "token": served.ada })));
    let line =
        opened.head.lines().find(|line| line.starts_with("set-cookie: kito_session=")).expect(&opened.head);
    let session = line.trim_start_matches("set-cookie: ").split(';').next().unwrap().to_owned();
    let cookies = format!("{session}; theme=dark");
    let bearer = format!("Bearer {}", served.ada);
    let told = page(
        "/",
        &[
            ("Cookie", &cookies),
            ("Authorization", &bearer),
            ("X-Kito-User", "root"),
            ("X-Kito-Secret", "guess"),
        ],
    );
    assert!(told.body.contains("user=ada\nsecret=ok\n"), "{}", told.body);
    assert!(told.body.contains("cookie=theme=dark\nauthorization=-\n"), "{}", told.body);
    // What it sets is its own: not a session of the server.
    assert!(
        told.head.contains("set-cookie: hello=1") && !told.head.contains("kito_session=theirs"),
        "{}",
        told.head
    );

    // Its pages ask of a repository what it was given, as who looks at them.
    let lanes = json!({ "method": "lanes" });
    let read = served.request("POST", "/-/api/shop", &[("Cookie", &session), ("X-Kito", "1")], Some(&lanes));
    assert_eq!((read.status, read.json()), (200, json!([])), "{}", read.body);
    let open = json!({ "method": "open_lane", "params": { "agent": "a", "intent": "a cheaper price", "scope": ["./"] } });
    let write = served.request("POST", "/-/api/shop", &[("Cookie", &session), ("X-Kito", "1")], Some(&open));
    assert_eq!(write.status, 403, "{}", write.body);
    assert!(write.body.contains("no plugin that serves them was given `write`"), "{}", write.body);
    // With her own token, ada is not held to what a plugin was given.
    assert_eq!(served.ask(&served.ada, "shop", "open_lane", open["params"].clone()).status, 200);

    // What is the server's own is never the plugin's.
    assert_eq!(served.request("GET", "/-/me", &[], None).status, 401);
}

#[test]
fn http_that_is_not_encrypted_is_for_this_machine_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("server");
    ok(tmp.path(), &["init", dir.to_str().unwrap()]);
    let out = server(&dir, &["serve", "--plain", "--listen", "127.0.0.1:0", "--http", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("HTTP that is not encrypted (--plain) is for this machine only"), "{said}");
}

#[test]
fn http_is_encrypted_with_the_server_s_certificate() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("server");
    ok(tmp.path(), &["init", dir.to_str().unwrap()]);
    let ada = token_in(&ok(&dir, &["user", "add", "ada"]));
    let mut child = server(&dir, &["serve", "--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let address =
        line.trim().strip_prefix("kito-server: ").and_then(|rest| rest.split(",").next()).unwrap().to_owned();
    assert!(address.starts_with("https://127.0.0.1:"), "{line}");

    // Asked with curl, which is told to take the certificate as it is:
    // nobody vouches for one a server made for itself.
    let curl = |args: &[&str]| {
        let out =
            Command::new("curl").args(["--silent", "--insecure", "--max-time", "20"]).args(args).output();
        let out = out.expect("these tests ask over HTTPS with curl");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let me = format!("{address}/-/me");
    let nobody = curl(&[&me]);
    let bearer = format!("Authorization: Bearer {ada}");
    let known = curl(&["--header", &bearer, &me]);
    // A session over an encrypted connection is for encrypted connections only.
    let opening = format!("{{\"token\": \"{ada}\"}}");
    let session = format!("{address}/-/session");
    let opened =
        curl(&["--include", "--header", "Content-Type: application/json", "--data", &opening, &session]);
    // In the clear, the same address answers nothing.
    let plain = Command::new("curl")
        .args(["--silent", "--max-time", "10", &me.replace("https://", "http://")])
        .output()
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(nobody.contains("nobody is signed in"), "{nobody}");
    assert!(known.contains("\"user\":\"ada\""), "{known}");
    let cookie = opened
        .lines()
        .find(|line| line.to_lowercase().starts_with("set-cookie: kito_session="))
        .expect(&opened);
    assert!(cookie.contains("; Secure"), "{cookie}");
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("nobody is signed in"));
}
