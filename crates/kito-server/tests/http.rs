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
        .env("KITO_CONFIG_DIR", dir.join(".personal-config"))
        // A registry of files, next to the server: empty unless a test fills it.
        .env("KITO_REGISTRY", format!("file://{}", dir.join("registry").display()))
        .env("KITO_HELLO_CLEANED", dir.join("cleaned"));
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
    install_as(dir, "hello", manifest);
}

/// The plugin of the tests under a name of its own: the same program, and
/// the manifest given, which says that name.
fn install_as(dir: &Path, plugin: &str, manifest: &str) {
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
    let installed = dir.join(".personal-config/plugins").join(plugin);
    fs::create_dir_all(&installed).unwrap();
    fs::copy(built, installed.join(format!("kito-{plugin}{}", std::env::consts::EXE_SUFFIX))).unwrap();
    fs::write(installed.join("kito-plugin.toml"), manifest).unwrap();
}

/// Puts the plugin of the tests behind a script that waits `seconds`
/// before it is the plugin, when it is run for `slow` (its first
/// argument, or its first two): a plugin that takes its time.
#[cfg(unix)]
fn slow_hello(dir: &Path, slow: &str, seconds: u32) {
    use std::os::unix::fs::PermissionsExt;
    let installed = dir.join(".personal-config/plugins/hello");
    fs::rename(installed.join("kito-hello"), installed.join("hello-itself")).unwrap();
    let script = format!(
        "#!/bin/sh\ncase \"$1 $2\" in \"{slow}\"*) sleep {seconds};; esac\nexec \"$(dirname \"$0\")/hello-itself\" \"$@\"\n"
    );
    fs::write(installed.join("kito-hello"), script).unwrap();
    fs::set_permissions(installed.join("kito-hello"), fs::Permissions::from_mode(0o755)).unwrap();
}

/// A server with a repository, ada who may integrate and carol who may
/// read, serving in the clear on this machine.
struct Served {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    child: Child,
    http: String,
    /// Where it listens for clients.
    clients: String,
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
        // Added with nothing: its README.md is what a lane starts from.
        ok(&dir, &["repo", "add", "shop"]);
        let ada = token_in(&ok(&dir, &["user", "add", "ada", "--repo", "shop:integrate"]));
        let carol = token_in(&ok(&dir, &["user", "add", "carol", "--repo", "shop:read"]));
        setup(&dir);
        let mut child =
            server(&dir, &["serve", "--plain", "--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
        let (mut line, mut second) = (String::new(), String::new());
        let mut said = BufReader::new(child.stdout.take().unwrap());
        said.read_line(&mut line).unwrap();
        // kito-server: http://127.0.0.1:<port>, <pages>
        let http = line
            .trim()
            .strip_prefix("kito-server: http://")
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_else(|| panic!("{line:?}"))
            .to_owned();
        // kito-server: listening on 127.0.0.1:<port>, ...
        said.read_line(&mut second).unwrap();
        let clients = second
            .trim()
            .strip_prefix("kito-server: listening on ")
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_else(|| panic!("{second:?}"))
            .to_owned();
        Served { _tmp: tmp, dir, child, http, clients, ada, carol }
    }

    /// A session opened in a browser with a token: its cookie.
    fn session(&self, token: &str) -> String {
        let opened = self.request("POST", "/-/session", &[], Some(&json!({ "token": token })));
        let value = opened
            .head
            .lines()
            .find_map(|line| line.strip_prefix("set-cookie: kito_session="))
            .unwrap_or_else(|| panic!("{}{}", opened.head, opened.body));
        format!("kito_session={}", value.split(';').next().unwrap())
    }

    /// One of `/-/people`, asked as a page would with these headers, or
    /// with a token.
    fn people(&self, headers: &[(&str, &str)], method: &str, params: Value) -> Answer {
        let call = json!({ "method": method, "params": params });
        self.request("POST", "/-/people", headers, Some(&call))
    }

    /// Who the server says asks with these headers.
    fn me(&self, headers: &[(&str, &str)]) -> Answer {
        self.request("GET", "/-/me", headers, None)
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

/// A client's connection as it travels, in the clear: a line of JSON
/// each way. What the server says when it ends one is read as it is.
struct Wire {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    asked: u64,
}

impl Wire {
    /// Says hello with a token, about no repository: the connection, and
    /// what the server answered.
    fn open(address: &str, token: &str) -> (Wire, Value) {
        let stream = TcpStream::connect(address).unwrap();
        let mut wire = Wire { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream, asked: 0 };
        let greeting = wire.say(&json!({ "hello": { "version": 1, "token": token } }));
        (wire, greeting)
    }

    /// Sends a line and reads one; `Null`: the server closed the connection.
    fn say(&mut self, message: &Value) -> Value {
        if writeln!(self.writer, "{message}").is_err() {
            return Value::Null;
        }
        let mut line = String::new();
        let _ = self.reader.read_line(&mut line);
        serde_json::from_str(&line).unwrap_or(Value::Null)
    }

    /// Asks one thing: the line that came back, a result, an error, or
    /// the end of the connection with why.
    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.asked += 1;
        self.say(&json!({ "jsonrpc": "2.0", "id": self.asked, "method": method, "params": params }))
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

#[test]
fn an_admin_adds_a_repository_with_its_first_files_from_a_page() {
    let served = Served::start(|dir| {
        ok(dir, &["user", "role", "ada", "admin"]);
    });
    let session = |token: &str| {
        let opened = served.request("POST", "/-/session", &[], Some(&json!({ "token": token })));
        let value =
            opened.head.lines().find_map(|line| line.strip_prefix("set-cookie: kito_session=")).unwrap();
        format!("kito_session={}", value.split(';').next().unwrap())
    };
    let (ada, carol) = (session(&served.ada), session(&served.carol));
    let add = |cookie: &str, params: Value| {
        let call = json!({ "method": "server.repo_add", "params": params });
        served.request("POST", "/-/people", &[("Cookie", cookie), ("X-Kito", "1")], Some(&call))
    };

    // The files reach the server as they were given: they are the first
    // version of the repository, and a lane opens in it right away.
    let files = json!([
        { "path": "kitchen.toml", "text": "name = \"Shopworks\"\n" },
        { "path": "recipes/review.md", "text": "# Review\n" },
    ]);
    let added = add(&ada, json!({ "name": "kitchen", "files": files }));
    assert_eq!((added.status, added.json()), (200, json!({ "name": "kitchen" })), "{}", added.body);
    let kitchen = served.dir.join("repos/kitchen");
    assert_eq!(fs::read_to_string(kitchen.join("recipes/review.md")).unwrap(), "# Review\n");
    assert!(!kitchen.join("README.md").exists());
    let lane = json!({ "agent": "cook", "intent": "a recipe for releases", "scope": ["recipes/"] });
    let opened = served.ask(&served.ada, "kitchen", "open_lane", lane);
    assert_eq!((opened.status, opened.json()["id"].as_str()), (200, Some("lane/1")), "{}", opened.body);

    // What cannot be given is a request that is wrong, said with the path.
    let out = add(&ada, json!({ "name": "out", "files": [{ "path": "../out.md", "text": "" }] }));
    assert_eq!(out.status, 400, "{}", out.body);
    assert!(out.body.contains(r#"\"../out.md\" is not a path inside the repository"#), "{}", out.body);
    let both = add(&ada, json!({ "name": "out", "from": "https://example.com/x.git", "files": files }));
    assert_eq!(both.status, 400, "{}", both.body);
    assert!(both.body.contains("from a git address or from the files given, not both"), "{}", both.body);
    assert!(!served.dir.join("repos/out").exists());
    // And it is for the admins of the server, as adding one with nothing is.
    let refused = add(&carol, json!({ "name": "out", "files": files }));
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(!served.dir.join("repos/out").exists());

    if let Ok(mut daemon) = kitod::client::Client::connect(&kitchen) {
        let _ = daemon.call_value("shutdown", json!({}));
    }
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

    // The plugin asks the server as itself, with what it was given (ADR
    // 0020): it reads; it does not write; a wrong secret is nobody.
    let asked = page("/ask/shop/lanes", &[]);
    assert!(asked.body.starts_with("asked=HTTP/1.1 200"), "{}", asked.body);
    let wrote = page("/ask/shop/checkpoint", &[]);
    assert!(
        wrote.body.contains("may not ask `checkpoint`") || wrote.body.contains("not something a client"),
        "{}",
        wrote.body
    );
    // Of the server itself it is given nothing: its people, its
    // repositories and its log are a person's to ask. So is how a person
    // signs in: asked with nobody named, it would be the token of
    // whoever is called as the plugin is.
    let namesake = token_in(&ok(&served.dir, &["user", "add", "hello"]));
    for method in [
        "server.people",
        "server.repos_all",
        "server.repo_add",
        "server.audit",
        "server.grant",
        "server.people_link",
        "server.people_unlink",
        "server.people_token",
    ] {
        let asked = page(&format!("/ask/shop/{method}"), &[]);
        assert!(
            asked.body.starts_with("asked=HTTP/1.1 403") && asked.body.contains("is for a person"),
            "{method}: {}",
            asked.body
        );
        assert!(!asked.body.contains("kito_"), "{method}: {}", asked.body);
    }
    let theirs = format!("Bearer {namesake}");
    assert_eq!(served.me(&[("Authorization", &theirs)]).json()["user"], "hello");
    // Who uses the repository is for a plugin that was given `admin`.
    let members = page("/ask/shop/server.members", &[]);
    assert!(
        members.body.starts_with("asked=HTTP/1.1 403")
            && members.body.contains("only when it was given `admin`"),
        "{}",
        members.body
    );
    let call = json!({ "method": "lanes" });
    let nobody = served.request("POST", "/-/api/shop", &[("x-kito-plugin", "guess")], Some(&call));
    assert_eq!(nobody.status, 401, "{}", nobody.body);
}

#[test]
fn pages_come_and_go_while_the_server_runs() {
    let served = Served::start(|dir| install_hello(dir, PAGES));
    let until = |wanted: u16| {
        let mut last = 0;
        for _ in 0..400 {
            last = served.request("GET", "/", &[], None).status;
            if last == wanted {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("the page answered {last}, not {wanted}");
    };
    // Nothing serves pages yet.
    until(404);
    // Enabled on the server's machine: served without a restart.
    ok(&served.dir, &["plugin", "enable", "hello"]);
    until(200);
    // Disabled: gone, and its program stopped.
    ok(&served.dir, &["plugin", "disable", "hello"]);
    until(404);
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
    // How a client connects here is said with it: the fingerprint the
    // server is known by, which `kito-server fingerprint` prints.
    let print = ok(&dir, &["fingerprint"]).trim().to_owned();
    assert!(known.contains(&format!("\"fingerprint\":\"{print}\"")), "{known}");
    assert!(known.contains("\"plain\":false") && known.contains("\"port\":\""), "{known}");
    let cookie = opened
        .lines()
        .find(|line| line.to_lowercase().starts_with("set-cookie: kito_session="))
        .expect(&opened);
    assert!(cookie.contains("; Secure"), "{cookie}");
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("nobody is signed in"));
}

const SIGNS: &str = r#"name = "hello"
version = "0.6.0"
api = 1
permissions = ["sign_in", "server_events"]

[sign_in]
method = "hello"
about = "Sign in with hello"

[[server_event]]
kinds = ["signed_in", "sign_in_refused", "session_ended"]
"#;

#[test]
fn somebody_signs_in_in_a_browser_as_who_a_plugin_says_they_are() {
    let served = Served::start(|dir| {
        install_hello(dir, SIGNS);
        ok(dir, &["plugin", "enable", "hello"]);
        // Who the plugin says `bob` is, is bob here: no token is made.
        let said = ok(dir, &["user", "add", "bob", "--identity", "hello:bob", "--repo", "shop:write"]);
        assert!(said.contains("whoever signs in as hello:bob is bob. No token."), "{said}");
        assert!(said.contains("--sign-in hello") && !said.contains("kito_"), "{said}");
        assert!(
            ok(dir, &["user", "list"]).contains("bob <bob@hello.example> [hello:bob] (member): shop (write)")
        );
        // The plugin is asked who that is, and an identity is one person's.
        let unknown = server(dir, &["user", "add", "ghost", "--identity", "hello:nobody"]).output().unwrap();
        assert!(String::from_utf8_lossy(&unknown.stderr)
            .contains("hello:nobody: hello knows nobody by that name"));
        let twice = server(dir, &["user", "add", "bobby", "--identity", "hello:bob"]).output().unwrap();
        assert!(String::from_utf8_lossy(&twice.stderr).contains("hello:id-bob is bob already"));
        let none = server(dir, &["user", "add", "dan", "--identity", "github:dan"]).output().unwrap();
        let none = String::from_utf8_lossy(&none.stderr).into_owned();
        assert!(none.contains("no way to sign in with `github`: it has hello"), "{none}");
    });
    // What the server keeps is what does not change, not the login.
    let config = fs::read_to_string(served.dir.join("server.toml")).unwrap();
    assert!(config.contains("identities = [\"hello:id-bob\"]"), "{config}");

    let ways = served.request("GET", "/-/sign-in", &[], None).json();
    assert_eq!(
        ways["methods"],
        json!([{ "method": "hello", "about": "Sign in with hello", "path": "/-/sign-in/hello", "ready": true }])
    );
    // A way its plugin says is not set up is listed with why: a client does not offer it.
    let not_ready = served.dir.join("plugins/hello/not-ready");
    fs::create_dir_all(not_ready.parent().unwrap()).unwrap();
    fs::write(&not_ready, "an admin gives it the provider's application first\n").unwrap();
    let ways = served.request("GET", "/-/sign-in", &[], None).json();
    assert_eq!(ways["methods"][0]["ready"], false, "{ways}");
    assert_eq!(ways["methods"][0]["why"], "an admin gives it the provider's application first", "{ways}");
    fs::remove_file(&not_ready).unwrap();
    let nowhere = served.request("GET", "/-/sign-in/github", &[], None);
    assert_eq!(nowhere.status, 404, "{}", nowhere.body);

    // The browser is sent to the provider, with where to come back to.
    let left = served.request("GET", "/-/sign-in/hello", &[], None);
    assert_eq!(left.status, 302, "{}", left.body);
    let to = format!(
        "location: https://hello.example/authorize?state=left-1&redirect_uri=http://{}/-/sign-in/hello/callback",
        served.http
    );
    assert!(left.head.contains(&to), "{}", left.head);
    assert!(
        left.head.contains("set-cookie: kito_sign_in=left-1; path=/-/sign-in; httponly; samesite=lax"),
        "{}",
        left.head
    );

    // Whoever comes back is the one who left, or nobody.
    let back = "/-/sign-in/hello/callback?code=bob&state=left-1";
    assert_eq!(served.request("GET", back, &[], None).status, 403);
    assert_eq!(served.request("GET", back, &[("Cookie", "kito_sign_in=another")], None).status, 403);
    let leaving = [("Cookie", "kito_sign_in=left-1")];
    let signed = served.request("GET", back, &leaving, None);
    assert_eq!(signed.status, 200, "{}", signed.body);
    assert!(signed.body.contains("Signed in as bob."), "{}", signed.body);
    let session = signed
        .head
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: kito_session="))
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| panic!("{}", signed.head))
        .to_owned();
    assert!(session.starts_with("kitos_"), "{session}");
    assert!(signed.head.contains("set-cookie: kito_sign_in=; "), "{}", signed.head);

    // The session is bob, with what bob may do; what is kept is not the token.
    let cookie = format!("kito_session={session}");
    let me = served.request("GET", "/-/me", &[("Cookie", &cookie)], None).json();
    assert_eq!((me["user"].as_str(), me["email"].as_str()), (Some("bob"), Some("bob@hello.example")));
    assert_eq!(me["repos"], json!([{ "name": "shop", "role": "write" }]));
    let bearer = format!("Bearer {session}");
    assert_eq!(served.request("GET", "/-/me", &[("Authorization", &bearer)], None).json()["user"], "bob");
    let kept = fs::read_to_string(served.dir.join("sessions.toml")).unwrap();
    assert!(kept.contains("user = \"bob\"") && !kept.contains(&session), "{kept}");
    // A token an administrator handed out is not a session of a browser.
    let as_cookie = format!("kito_session={}", served.ada);
    assert_eq!(served.request("GET", "/-/me", &[("Cookie", &as_cookie)], None).status, 401);

    // Somebody the plugin knows and the server does not is nobody here;
    // so is one who said no at the provider.
    let eve = served.request("GET", "/-/sign-in/hello/callback?code=eve&state=left-1", &leaving, None);
    assert_eq!(eve.status, 403, "{}", eve.body);
    assert!(eve.body.contains("hello:eve is nobody on this server"), "{}", eve.body);
    assert!(!eve.head.contains("kito_session="), "{}", eve.head);
    let no = served.request("GET", "/-/sign-in/hello/callback?code=refuse&state=left-1", &leaving, None);
    assert_eq!(no.status, 403, "{}", no.body);
    assert!(no.body.contains("they said no at hello"), "{}", no.body);

    // The plugins that listen are told who signed in, and who was refused.
    let told = fs::read_to_string(served.dir.join("plugins/hello/events.log")).unwrap_or_default();
    assert_eq!(told.lines().collect::<Vec<_>>(), ["signed_in bob (hello:bob)", "sign_in_refused hello:eve"]);

    // A session ends when it is closed, and with the person.
    let closed = served.request("DELETE", "/-/session", &[("Cookie", &cookie)], None);
    assert_eq!(closed.status, 200, "{}", closed.body);
    assert_eq!(served.request("GET", "/-/me", &[("Cookie", &cookie)], None).status, 401);
    let told = fs::read_to_string(served.dir.join("plugins/hello/events.log")).unwrap_or_default();
    assert_eq!(told.lines().last(), Some("session_ended bob"), "{told}");
    let again = served.request("GET", back, &leaving, None);
    let again = again.head.lines().find_map(|line| line.strip_prefix("set-cookie: kito_session=")).unwrap();
    let cookie = format!("kito_session={}", again.split(';').next().unwrap());
    assert_eq!(served.request("GET", "/-/me", &[("Cookie", &cookie)], None).status, 200);
    ok(&served.dir, &["user", "remove", "bob"]);
    assert_eq!(served.request("GET", "/-/me", &[("Cookie", &cookie)], None).status, 401);
    assert!(!fs::read_to_string(served.dir.join("sessions.toml")).unwrap().contains("bob"));
}

const LINKS: &str = r#"name = "hello"
version = "0.6.1"
api = 1
permissions = ["sign_in", "server_events"]

[sign_in]
method = "hello"
about = "Sign in with hello"

[[server_event]]
kinds = ["session_ended", "identity_linked", "identity_unlinked", "token_changed", "token_removed"]
"#;

/// The browser back from hello, where `login` signed in: what the server
/// answers it, with the cookies it brings.
fn back_from_hello(served: &Served, login: &str, cookies: &str) -> Answer {
    let back = format!("/-/sign-in/hello/callback?code={login}&state=left-1");
    served.request("GET", &back, &[("Cookie", cookies)], None)
}

/// Somebody signs in with hello in a browser: the id of their session.
fn signed_in_with_hello(served: &Served, login: &str) -> String {
    let signed = back_from_hello(served, login, "kito_sign_in=left-1");
    let id = signed
        .head
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: kito_session="))
        .unwrap_or_else(|| panic!("{}{}", signed.head, signed.body));
    id.split(';').next().unwrap().to_owned()
}

/// What the audit log keeps, each line as `<who> <kind> <what>`; and
/// everything under `audit/`, as it is on the disk.
fn audited(dir: &Path) -> (Vec<String>, String) {
    let lines = ok(dir, &["audit", "--limit", "200"])
        .lines()
        .map(|line| line.split_whitespace().skip(1).collect::<Vec<_>>().join(" "))
        .collect();
    let kept = fs::read_dir(dir.join("audit"))
        .unwrap()
        .map(|file| fs::read_to_string(file.unwrap().path()).unwrap_or_default())
        .collect();
    (lines, kept)
}

/// What the plugin of the tests was told so far, a line for each event.
/// The plugins are told once whoever asked has their answer: waited for,
/// until there are `at_least`.
fn told(served: &Served, at_least: usize) -> Vec<String> {
    let log = served.dir.join("plugins/hello/events.log");
    let mut lines = Vec::new();
    for _ in 0..800 {
        lines = fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_owned).collect();
        if lines.len() >= at_least {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    lines
}

/// Every run of 64 hex digits in a text: what a file keeps of tokens, of
/// sessions and of links.
fn hashes_in(text: &str) -> Vec<String> {
    let words = text.split(|c: char| !c.is_ascii_hexdigit());
    words.filter(|word| word.len() == 64).map(str::to_owned).collect()
}

#[test]
fn a_change_of_how_somebody_signs_in_ends_what_was_opened_before_it() {
    let served = Served::start(|dir| {
        install_hello(dir, LINKS);
        ok(dir, &["plugin", "enable", "hello"]);
        // carol has a token; from now on she signs in with hello too.
        let said = ok(dir, &["user", "link", "carol", "hello:carol"]);
        assert!(said.contains("Whoever signs in as hello:carol is carol from now on."), "{said}");
        assert!(said.contains("carol signs in with a token or as hello:carol."), "{said}");
        let listed = ok(dir, &["user", "list"]);
        assert!(listed.contains("carol [token, hello:carol] (member): shop (read)"), "{listed}");
    });
    let changed = "carol's token was changed: connect again with the new one";
    let unknown = "this token is not one the server knows";

    // Three sessions of hers: a browser's opened with her token, another
    // browser's opened by signing in, and a command line's.
    let tab = served.session(&served.carol);
    let other = format!("kito_session={}", signed_in_with_hello(&served, "carol"));
    let command_line = signed_in_with_hello(&served, "carol");
    // And connections of clients: two opened with her token, one with a session.
    let (mut with_token, greeting) = Wire::open(&served.clients, &served.carol);
    assert_eq!(greeting["ok"]["user"], "carol", "{greeting}");
    let (mut with_session, greeting) = Wire::open(&served.clients, &command_line);
    assert_eq!(greeting["ok"]["user"], "carol", "{greeting}");
    let remote = kitod::client::Remote {
        server: Some(served.clients.clone()),
        repo: "shop".to_owned(),
        token: Some(served.carol.clone()),
        plain: true,
        ..Default::default()
    };
    let mut client = kitod::client::Client::remote(&remote, true).unwrap();
    assert_eq!(with_token.ask("server.whoami", json!({}))["result"]["user"], "carol");
    assert_eq!(with_session.ask("server.whoami", json!({}))["result"]["user"], "carol");
    assert_eq!(client.call_value("server.whoami", json!({})).unwrap()["user"], "carol");

    // She asks for a new token from the first browser: said this once.
    let page = [("Cookie", tab.as_str()), ("X-Kito", "1")];
    let renewed = served.people(&page, "server.people_token", json!({}));
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    let new = renewed.json()["token"].as_str().unwrap_or_else(|| panic!("{}", renewed.body)).to_owned();
    assert_eq!(renewed.json(), json!({ "user": "carol", "token": new }));
    assert!(new.starts_with("kito_") && new.len() == 5 + 64 && new != served.carol, "{new}");

    // At once: the browser she asked from goes on, her other sessions ended.
    assert_eq!(served.me(&[("Cookie", &tab)]).json()["user"], "carol");
    assert_eq!(served.me(&[("Cookie", &other)]).status, 401);
    let as_command_line = format!("Bearer {command_line}");
    assert_eq!(served.me(&[("Authorization", &as_command_line)]).status, 401);
    // What was opened with the token she had is refused at its next
    // request and told why, and so is what was opened with a session
    // that ended. Nothing more is answered there.
    assert_eq!(with_token.ask("server.whoami", json!({})), json!({ "error": changed }));
    assert_eq!(with_token.ask("server.whoami", json!({})), Value::Null);
    let ended = json!({ "error": "this session ended: sign in again (`kito sign-in`)" });
    assert_eq!(with_session.ask("server.whoami", json!({})), ended);
    assert!(client.call_value("server.whoami", json!({})).is_err());
    // The token she had opens nothing more; the new one does.
    assert_eq!(Wire::open(&served.clients, &served.carol).1, json!({ "error": unknown }));
    let as_before = format!("Bearer {}", served.carol);
    assert_eq!(served.me(&[("Authorization", &as_before)]).status, 401);
    let opening = json!({ "token": served.carol });
    assert_eq!(served.request("POST", "/-/session", &[], Some(&opening)).status, 401);
    let (mut mine, greeting) = Wire::open(&served.clients, &new);
    assert_eq!(greeting["ok"]["user"], "carol", "{greeting}");
    let (mut beside, _) = Wire::open(&served.clients, &new);

    // The connection that asks for its own new token is told it, and
    // goes on; one opened beside it with the same token does not. It was
    // asked through no session: the browser's ended with the others.
    let again = mine.ask("server.people_token", json!({ "user": "carol" }));
    let newer = again["result"]["token"].as_str().unwrap_or_else(|| panic!("{again}")).to_owned();
    assert!(newer.starts_with("kito_") && newer != new && newer != served.carol, "{again}");
    assert_eq!(mine.ask("server.whoami", json!({}))["result"]["user"], "carol");
    assert_eq!(beside.ask("server.whoami", json!({})), json!({ "error": changed }));
    assert_eq!(served.me(&[("Cookie", &tab)]).status, 401);

    // An identity unlinked ends her sessions too: one of them may have
    // been opened with it. Her token is hers still, and what it opened.
    let signed = format!("kito_session={}", signed_in_with_hello(&served, "carol"));
    let how = served.me(&[("Cookie", &signed)]).json()["signs_in"].clone();
    assert_eq!(how, json!({ "token": true, "logins": ["hello:carol"] }));
    let unlinked = mine.ask("server.people_unlink", json!({ "method": "hello" }));
    assert_eq!(unlinked["result"], json!({ "user": "carol", "token": true, "logins": [] }), "{unlinked}");
    assert_eq!(served.me(&[("Cookie", &signed)]).status, 401);
    assert_eq!(mine.ask("server.whoami", json!({}))["result"]["user"], "carol");
    let nobody = back_from_hello(&served, "carol", "kito_sign_in=left-1");
    assert_eq!(nobody.status, 403, "{}", nobody.body);
    assert!(nobody.body.contains("hello:carol is nobody on this server"), "{}", nobody.body);

    // Never nothing: her token is all she signs in with now.
    let no = mine.ask("server.people_token", json!({ "remove": true }));
    let why = no["error"]["message"].as_str().unwrap_or_else(|| panic!("{no}"));
    assert_eq!(why, "carol would have no way to sign in: link an identity to them first");
    assert_eq!(mine.ask("server.whoami", json!({}))["result"]["user"], "carol");

    // With an identity again, she takes her token away from a browser:
    // the session she asks through goes on, and what the token opened is
    // refused at its next request. (The plugins are told of a change
    // once it is answered: what the server's machine does now comes
    // after what they were to hear before.)
    assert_eq!(told(&served, 8).len(), 8);
    ok(&served.dir, &["user", "link", "carol", "hello:carol"]);
    let last = format!("kito_session={}", signed_in_with_hello(&served, "carol"));
    let page = [("Cookie", last.as_str()), ("X-Kito", "1")];
    let removed = served.people(&page, "server.people_token", json!({ "remove": true }));
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert_eq!(removed.json(), json!({ "user": "carol", "token": null }));
    let how = served.me(&[("Cookie", &last)]).json()["signs_in"].clone();
    assert_eq!(how, json!({ "token": false, "logins": ["hello:carol"] }));
    let gone = "carol has no token any more: sign in instead (`kito sign-in <method>`)";
    assert_eq!(mine.ask("server.whoami", json!({})), json!({ "error": gone }));
    assert_eq!(Wire::open(&served.clients, &newer).1, json!({ "error": unknown }));
    let listed = ok(&served.dir, &["user", "list"]);
    assert!(listed.contains("carol [hello:carol] (member): shop (read)"), "{listed}");
    // Her identity is all she signs in with now: it stays.
    let no = served.people(&page, "server.people_unlink", json!({ "method": "hello" }));
    assert_eq!(no.status, 403, "{}", no.body);
    assert!(no.body.contains("carol would have no way to sign in: give them a token first"), "{}", no.body);

    // A first token ends nothing: there was none to open anything with.
    // Asked through `/-/api` too, the session it comes through is kept.
    let second = format!("kito_session={}", signed_in_with_hello(&served, "carol"));
    let call = json!({ "method": "server.people_token" });
    let first = served.request("POST", "/-/api/shop", &page, Some(&call));
    assert_eq!(first.status, 200, "{}", first.body);
    let first = first.json()["token"].as_str().unwrap().to_owned();
    assert_eq!(served.me(&[("Cookie", &last)]).json()["user"], "carol");
    assert_eq!(served.me(&[("Cookie", &second)]).json()["user"], "carol");
    assert_eq!(Wire::open(&served.clients, &first).1["ok"]["user"], "carol");

    // The plugins that listen were told each change, then each session
    // it ended.
    assert_eq!(
        told(&served, 11),
        [
            "identity_linked carol: hello:carol",
            "token_changed carol",
            "session_ended carol",
            "session_ended carol",
            "token_changed carol",
            "session_ended carol",
            "identity_unlinked carol: hello:carol",
            "session_ended carol",
            "identity_linked carol: hello:carol",
            "token_removed carol",
            "token_changed carol, who had none",
        ]
    );
    // And the log keeps who did what to whom, never a token or a session.
    let (lines, kept) = audited(&served.dir);
    for line in [
        "the server's machine identity_linked carol: hello:carol",
        "carol token_changed carol",
        "carol identity_unlinked carol: hello:carol",
        "carol token_removed carol",
        "carol token_changed carol, who had none",
    ] {
        assert!(lines.iter().any(|kept| kept == line), "no `{line}` in {lines:#?}");
    }
    for secret in [&served.carol, &new, &newer, &first, &command_line] {
        assert!(!kept.contains(secret.as_str()), "a token in the audit log");
    }
    assert!(!kept.contains("kito_") && !kept.contains("kitos_"), "{kept}");
}

#[test]
fn somebody_links_an_account_of_their_own_by_signing_in_to_it() {
    let served = Served::start(|dir| {
        install_hello(dir, LINKS);
        ok(dir, &["plugin", "enable", "hello"]);
        ok(dir, &["user", "add", "bob", "--identity", "hello:bob"]);
    });
    // How somebody signs in is said to them, never the token itself; and
    // how a client connects here, to whoever is signed in: it is no secret.
    let as_carol = format!("Bearer {}", served.carol);
    let me = served.me(&[("Authorization", &as_carol)]).json();
    assert_eq!(me["signs_in"], json!({ "token": true, "logins": [] }));
    let port = served.clients.rsplit(':').next().unwrap();
    assert_eq!(me["clients"], json!({ "port": port, "plain": true, "fingerprint": null }));
    assert_eq!(me["admin"], false);

    // An account is linked by somebody signed in in this browser: not by
    // nobody, and not by whoever holds a token.
    let holder = [("Authorization", as_carol.as_str())];
    for headers in [&[][..], &holder[..]] {
        let refused = served.request("GET", "/-/sign-in/hello?link=1", headers, None);
        assert_eq!(refused.status, 401, "{}", refused.body);
        assert!(refused.body.contains("nobody is signed in: sign in first"), "{}", refused.body);
        assert!(!refused.head.contains("set-cookie"), "{}", refused.head);
    }

    // carol, signed in, is sent to the provider as for signing in. What
    // is kept for the way back says it is a link, with a value made for
    // this link and no other: the server keeps what it hashes to, with
    // the session the link was asked from.
    let tab = served.session(&served.carol);
    let sessions = || fs::read_to_string(served.dir.join("sessions.toml")).unwrap();
    let before = sessions();
    let leave = |session: &str| -> String {
        let left = served.request("GET", "/-/sign-in/hello?link=1", &[("Cookie", session)], None);
        assert_eq!(left.status, 302, "{}", left.body);
        let to = "location: https://hello.example/authorize?state=left-1&redirect_uri=http://";
        assert!(left.head.contains(to), "{}", left.head);
        let kept = left
            .head
            .lines()
            .find_map(|line| line.strip_prefix("set-cookie: "))
            .unwrap_or_else(|| panic!("{}", left.head));
        assert!(kept.contains("; path=/-/sign-in; httponly; samesite=lax; max-age=600"), "{kept}");
        kept.split(';').next().unwrap().to_owned()
    };
    let value_of = |kept: &str| -> String {
        let value = kept.strip_prefix("kito_sign_in=left-1:").unwrap_or_else(|| panic!("{kept}"));
        assert!(value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()), "{kept}");
        value.to_owned()
    };
    let signs_in = |bearer: &str| served.me(&[("Authorization", bearer)]).json()["signs_in"].clone();
    let waits_no_more =
        "Nothing was linked: no link waits for this browser: it was used already, it is more \
                         than ten minutes old, or whoever asked for it is no longer signed in here.";
    let refused = |no: Answer, why: &str| {
        assert_eq!(no.status, 403, "{}", no.body);
        assert!(no.head.contains("content-type: text/html"), "{}", no.head);
        assert!(no.body.contains(why) && !no.body.contains("http-equiv"), "{}", no.body);
        assert!(!no.head.contains("kito_session=") && no.head.contains("set-cookie: kito_sign_in=; "));
    };
    // A link asked again is the one that waits: the one before is forgotten.
    let (forgotten, kept) = (leave(&tab), leave(&tab));
    let mut values = vec![value_of(&forgotten), value_of(&kept)];
    assert_ne!(values[0], values[1]);
    refused(back_from_hello(&served, "carol", &forgotten), waits_no_more);

    // What the server keeps brings nobody back: not what a session's
    // token hashes to, which is in `sessions.toml` and in every backup,
    // and not what the link's value does. Whoever reads the disk and
    // signs in to an account of their own is nobody's link.
    let waiting = sessions();
    assert!(waiting.contains(".link]") && !waiting.contains(&values[1]), "{waiting}");
    let on_disk = hashes_in(&waiting);
    assert!(on_disk.len() >= 2 && !tab.contains(&values[1]), "{waiting}");
    for kept in on_disk {
        refused(back_from_hello(&served, "mallory", &format!("kito_sign_in=left-1:{kept}")), waits_no_more);
    }
    assert_eq!(signs_in(&as_carol), json!({ "token": true, "logins": [] }));
    assert_eq!(sessions(), waiting, "a value nobody waits for changed what waits");

    // An account that is another person's stays theirs: a page says so
    // in words, with a way back, and nothing changed. The value was used
    // all the same: it brings a browser back once, whatever follows.
    let said = "<p>Nothing was linked: hello:bob is bob already. <a href=\"/\">Go back</a>.</p>";
    refused(back_from_hello(&served, "bob", &kept), said);
    assert_eq!(signs_in(&as_carol), json!({ "token": true, "logins": [] }));
    assert_eq!(sessions(), before);
    refused(back_from_hello(&served, "carol", &kept), waits_no_more);
    // So does one whose owner said no at the provider.
    let kept = leave(&tab);
    values.push(value_of(&kept));
    refused(back_from_hello(&served, "refuse", &kept), "Nothing was linked: they said no at hello.");

    // Her own is attached to who she is. The way back starts on the
    // provider's site, so the session's cookie is not with it; no session
    // is opened, and the page goes on to the server's.
    let kept = leave(&tab);
    values.push(value_of(&kept));
    let linked = back_from_hello(&served, "carol", &kept);
    assert_eq!(linked.status, 200, "{}", linked.body);
    assert!(linked.body.contains("<meta http-equiv=\"refresh\" content=\"0;url=/\">"), "{}", linked.body);
    let said = "<p>Whoever signs in as hello:carol is carol from now on. <a href=\"/\">Go on</a>.</p>";
    assert!(linked.body.contains(said), "{}", linked.body);
    assert!(!linked.head.contains("kito_session="), "{}", linked.head);
    assert!(linked.head.contains("set-cookie: kito_sign_in=; "), "{}", linked.head);
    assert_eq!(sessions(), before);
    assert_eq!(signs_in(&as_carol), json!({ "token": true, "logins": ["hello:carol"] }));
    let config = fs::read_to_string(served.dir.join("server.toml")).unwrap();
    assert!(config.contains("identities = [\"hello:id-carol\"]"), "{config}");
    // Once, and not twice.
    refused(back_from_hello(&served, "carol", &kept), waits_no_more);
    // The plugins that listen hear of it once she has her answer.
    assert_eq!(told(&served, 1), ["identity_linked carol: hello:carol"]);
    // From then on whoever signs in with it is her.
    let hers = format!("Bearer {}", signed_in_with_hello(&served, "carol"));
    assert_eq!(served.me(&[("Authorization", &hers)]).json()["user"], "carol");

    // Linked again, it is hers already: nothing changes, and she goes
    // on. Another account of the same way is one too many.
    let kept = leave(&tab);
    let again = back_from_hello(&served, "carol", &format!("{kept}; {tab}"));
    assert_eq!(again.status, 200, "{}", again.body);
    assert!(again.body.contains("hello:carol is carol already. <a href=\"/\">Go on</a>."), "{}", again.body);
    let said = "Nothing was linked: carol signs in with hello:carol already, and a person has one identity \
                of each way: unlink it first.";
    refused(back_from_hello(&served, "carla", &leave(&tab)), said);
    assert_eq!(signs_in(&as_carol), json!({ "token": true, "logins": ["hello:carol"] }));

    // A link is for whoever asked for it, while they are signed in: not
    // for another session of the browser, and not once theirs has ended.
    let as_ada = format!("Bearer {}", served.ada);
    let ada = served.session(&served.ada);
    let said = "Nothing was linked: whoever asked for this link is no longer signed in here.";
    refused(back_from_hello(&served, "ada", &format!("{}; {tab}", leave(&ada))), said);
    let kept = leave(&ada);
    values.push(value_of(&kept));
    assert_eq!(served.request("DELETE", "/-/session", &[("Cookie", &ada)], None).status, 200);
    refused(back_from_hello(&served, "ada", &kept), waits_no_more);
    assert_eq!(signs_in(&as_ada), json!({ "token": true, "logins": [] }));
    // And none begins for a session that has ended.
    let nobody = served.request("GET", "/-/sign-in/hello?link=1", &[("Cookie", &ada)], None);
    assert_eq!(nobody.status, 401, "{}", nobody.body);

    // Who comes back is who left, for a link as for signing in; and
    // without `link=1` the flow is what it was.
    let stranger = back_from_hello(&served, "carol", "kito_sign_in=another:0000");
    assert_eq!(stranger.status, 403, "{}", stranger.body);
    assert!(stranger.body.contains("this sign-in did not start in this browser"), "{}", stranger.body);
    for path in ["/-/sign-in/hello", "/-/sign-in/hello?link=0", "/-/sign-in/hello?other=1"] {
        let left = served.request("GET", path, &[("Cookie", &tab)], None);
        assert!(left.head.contains("set-cookie: kito_sign_in=left-1; path=/-/sign-in"), "{}", left.head);
    }

    // The admins see how each person signs in; the plugins that listen
    // were told of the link, and the log keeps who linked what.
    ok(&served.dir, &["user", "role", "ada", "admin"]);
    let people = served.people(&[("Authorization", &as_ada)], "server.people", json!({})).json();
    let how = |name: &str| {
        let them = people.as_array().unwrap().iter().find(|them| them["user"] == name).unwrap();
        (them["token"].clone(), them["logins"].clone())
    };
    assert_eq!(how("carol"), (json!(true), json!(["hello:carol"])));
    assert_eq!(how("bob"), (json!(false), json!(["hello:bob"])));
    assert_eq!(how("ada"), (json!(true), json!([])));
    assert_eq!(told(&served, 2), ["identity_linked carol: hello:carol", "session_ended ada"]);
    let (lines, audit) = audited(&served.dir);
    let line = "carol identity_linked carol: hello:carol";
    assert!(lines.iter().any(|kept| kept == line), "no `{line}` in {lines:#?}");
    // No value a link was given is anywhere on the server: not with the
    // sessions, not in its configuration, not in its log.
    let config = fs::read_to_string(served.dir.join("server.toml")).unwrap();
    for value in &values {
        assert!(!sessions().contains(value) && !config.contains(value) && !audit.contains(value));
    }
}

#[test]
fn admins_change_how_a_member_signs_in_and_owners_how_anybody_does() {
    let served = Served::start(|dir| {
        install_hello(dir, LINKS);
        ok(dir, &["plugin", "enable", "hello"]);
        ok(dir, &["user", "role", "ada", "admin"]);
        ok(dir, &["user", "add", "bob", "--identity", "hello:bob"]);
    });
    let rose = token_in(&ok(&served.dir, &["user", "add", "rose", "--role", "admin"]));
    let (as_ada, as_rose) = (format!("Bearer {}", served.ada), format!("Bearer {rose}"));
    let ask = |who: &str, method: &str, params: Value| {
        served.people(&[("Authorization", who)], &format!("server.people_{method}"), params)
    };
    let refused = |answer: Answer, status: u16, why: &str| {
        assert_eq!(answer.status, status, "{}", answer.body);
        assert!(answer.body.contains(why), "{}", answer.body);
    };

    // An admin attaches an identity to a member, who keeps their token.
    let linked = ask(&as_ada, "link", json!({ "user": "carol", "identity": "hello:carol" }));
    assert_eq!(linked.status, 200, "{}", linked.body);
    assert_eq!(linked.json(), json!({ "user": "carol", "token": true, "logins": ["hello:carol"] }));
    // Theirs already: nothing changes.
    let again = ask(&as_ada, "link", json!({ "user": "carol", "identity": "hello:carol" }));
    assert_eq!(again.json(), json!({ "user": "carol", "token": true, "logins": ["hello:carol"] }));
    // One identity is one person's, a person has one of each way, and
    // the plugin is asked who a login is.
    let link = |identity: &str| ask(&as_ada, "link", json!({ "user": "carol", "identity": identity }));
    refused(link("hello:bob"), 403, "hello:bob is bob already");
    refused(link("hello:carla"), 403, "carol signs in with hello:carol already");
    refused(link("hello:nobody"), 403, "hello:nobody: hello knows nobody by that name");
    refused(link("github:carol"), 403, "no way to sign in with `github`: it has hello");
    refused(ask(&as_ada, "link", json!({ "user": "carol" })), 400, "missing parameter `identity`");
    refused(
        ask(&as_ada, "link", json!({ "user": "dan", "identity": "hello:dan" })),
        403,
        "dan is not a user of this server",
    );

    // Never nothing: bob signs in with hello, and with nothing else.
    let unlink_bob = || ask(&as_ada, "unlink", json!({ "user": "bob", "method": "hello" }));
    refused(unlink_bob(), 403, "bob would have no way to sign in: give them a token first");
    refused(
        ask(&as_ada, "token", json!({ "user": "bob", "remove": true })),
        403,
        "bob has no token: there is nothing to take away",
    );
    // A first token ends nothing: the session he has goes on.
    let session = format!("kito_session={}", signed_in_with_hello(&served, "bob"));
    let given = ask(&as_ada, "token", json!({ "user": "bob" }));
    assert_eq!(given.status, 200, "{}", given.body);
    let bob = given.json()["token"].as_str().unwrap().to_owned();
    assert_eq!(given.json(), json!({ "user": "bob", "token": bob }));
    assert_eq!(served.me(&[("Cookie", &session)]).json()["user"], "bob");
    let as_bob = format!("Bearer {bob}");
    let how = served.me(&[("Authorization", &as_bob)]).json()["signs_in"].clone();
    assert_eq!(how, json!({ "token": true, "logins": ["hello:bob"] }));
    // Now his identity can go: the session, which it may have opened,
    // ends; the token is his still.
    let unlinked = unlink_bob();
    assert_eq!(unlinked.status, 200, "{}", unlinked.body);
    assert_eq!(unlinked.json(), json!({ "user": "bob", "token": true, "logins": [] }));
    assert_eq!(served.me(&[("Cookie", &session)]).status, 401);
    assert_eq!(served.me(&[("Authorization", &as_bob)]).json()["user"], "bob");
    refused(unlink_bob(), 403, "bob has no identity of `hello`: there is nothing to unlink");
    refused(
        ask(&as_ada, "token", json!({ "user": "bob", "remove": true })),
        403,
        "bob would have no way to sign in: link an identity to them first",
    );
    // carol has both: her token is taken away, and stops at once.
    let removed = ask(&as_ada, "token", json!({ "user": "carol", "remove": true }));
    assert_eq!(removed.json(), json!({ "user": "carol", "token": null }), "{}", removed.body);
    let as_carol = format!("Bearer {}", served.carol);
    assert_eq!(served.me(&[("Authorization", &as_carol)]).status, 401);

    // A member changes nobody else, and is told no more than that; their
    // own token and identities are theirs, but naming a login proves
    // nothing: nobody links themselves by it.
    let members = "only an admin of this server may manage its people: bob is a member";
    refused(ask(&as_bob, "token", json!({ "user": "carol" })), 403, members);
    refused(ask(&as_bob, "token", json!({ "user": "nobody", "remove": true })), 403, members);
    refused(ask(&as_bob, "unlink", json!({ "user": "carol", "method": "hello" })), 403, members);
    refused(ask(&as_bob, "link", json!({ "user": "carol", "identity": "hello:bob" })), 403, members);
    let by_name = "naming a login proves nothing: bob links an account of their own by signing in to it";
    refused(ask(&as_bob, "link", json!({ "identity": "hello:bob" })), 403, by_name);
    refused(ask(&as_bob, "link", json!({ "user": "bob", "identity": "hello:bob" })), 403, by_name);
    refused(
        ask(&as_bob, "link", json!({ "user": "bob", "identity": "hello:bob" })),
        403,
        "or an admin links it",
    );
    let how = served.me(&[("Authorization", &as_bob)]).json()["signs_in"].clone();
    assert_eq!(how, json!({ "token": true, "logins": [] }));

    // An admin does not change an admin or an owner, as for their role;
    // what is her own she does change, but not by naming a login.
    let owners = "rose is an admin: only an owner changes them";
    refused(ask(&as_ada, "token", json!({ "user": "rose" })), 403, owners);
    refused(ask(&as_ada, "token", json!({ "user": "rose", "remove": true })), 403, owners);
    refused(ask(&as_ada, "unlink", json!({ "user": "rose", "method": "hello" })), 403, owners);
    refused(ask(&as_ada, "link", json!({ "user": "rose", "identity": "hello:rose" })), 403, owners);
    refused(
        ask(&as_ada, "link", json!({ "user": "ada", "identity": "hello:ada" })),
        403,
        "or an owner links it",
    );
    assert_eq!(served.me(&[("Authorization", &as_rose)]).json()["user"], "rose");
    let own = ask(&as_ada, "token", json!({ "user": "ada" }));
    assert_eq!(own.status, 200, "{}", own.body);
    let as_ada = format!("Bearer {}", own.json()["token"].as_str().unwrap());
    // An owner changes anybody, themselves by name too.
    ok(&served.dir, &["user", "role", "ada", "owner"]);
    let ask = |method: &str, params: Value| {
        served.people(&[("Authorization", &as_ada)], &format!("server.people_{method}"), params)
    };
    let linked = ask("link", json!({ "user": "rose", "identity": "hello:rose" }));
    assert_eq!(linked.json(), json!({ "user": "rose", "token": true, "logins": ["hello:rose"] }));
    let linked = ask("link", json!({ "identity": "hello:ada" }));
    assert_eq!(linked.json(), json!({ "user": "ada", "token": true, "logins": ["hello:ada"] }));
    let taken = ask("token", json!({ "user": "rose", "remove": true }));
    assert_eq!(taken.json(), json!({ "user": "rose", "token": null }), "{}", taken.body);
    assert_eq!(served.me(&[("Authorization", &as_rose)]).status, 401);

    // What is not a name, or not a yes or a no, is a request that is wrong.
    refused(ask("token", json!({ "user": 5 })), 400, "`user` is the name of a person");
    refused(ask("token", json!({ "user": "bob", "remove": "yes" })), 400, "`remove` is true or false");
    refused(ask("unlink", json!({ "user": "bob" })), 400, "missing parameter `method`");
    assert_eq!(served.me(&[("Authorization", &as_bob)]).json()["user"], "bob");

    // Who did what to whom is kept, and never a token.
    let (lines, kept) = audited(&served.dir);
    for line in [
        "ada identity_linked carol: hello:carol",
        "ada token_changed bob, who had none",
        "ada identity_unlinked bob: hello:bob",
        "ada token_removed carol",
        "ada token_changed ada",
        "ada identity_linked rose: hello:rose",
        "ada identity_linked ada: hello:ada",
        "ada token_removed rose",
    ] {
        assert!(lines.iter().any(|kept| kept == line), "no `{line}` in {lines:#?}");
    }
    assert!(!kept.contains("kito_") && !kept.contains("kitos_"), "{kept}");
}

const FROM_REGISTRY: &str = r#"name = "hello"
version = "0.9.0"
description = "Says what it is told"
api = 2
permissions = ["read", "server_events"]

[[server_event]]
kinds = ["plugin_installed", "plugin_enabled", "plugin_disabled", "plugin_removed"]

[cleanup]
about = "Takes out what it set up"
"#;

/// A registry of files that lists the plugin of the tests, in `dir`.
fn registry_with_hello(dir: &Path) {
    let name = format!("kito-hello{}", std::env::consts::EXE_SUFFIX);
    let built = Path::new(env!("CARGO_BIN_EXE_kito-server")).with_file_name("examples").join(&name);
    let packed = dir.join("pack/kito-hello");
    fs::create_dir_all(&packed).unwrap();
    fs::copy(built, packed.join(&name)).unwrap();
    fs::write(packed.join("kito-plugin.toml"), FROM_REGISTRY).unwrap();
    let registry = dir.join("registry");
    fs::create_dir_all(registry.join("plugins")).unwrap();
    let archive = registry.join("hello.tar.gz");
    let done = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(dir.join("pack"))
        .arg("kito-hello")
        .status();
    assert!(done.unwrap().success());
    let sum = kitod::registry::sha256_hex(&fs::read(&archive).unwrap());
    let index = json!({ "registry": 1, "plugins": { "hello": { "about": "Says what it is told", "latest": "0.9.0" } } });
    fs::write(registry.join("index.json"), index.to_string()).unwrap();
    let plugin = json!({ "name": "hello", "about": "Says what it is told", "versions": { "0.9.0": {
        "api": 2, "files": { kitod::registry::platform(): { "url": "hello.tar.gz", "sha256": sum } },
    }}});
    fs::write(registry.join("plugins/hello.json"), plugin.to_string()).unwrap();
}

#[test]
fn an_administrator_installs_and_takes_out_the_server_s_plugins_from_a_browser() {
    let served = Served::start(|dir| {
        install_hello(dir, PAGES);
        ok(dir, &["plugin", "enable", "hello"]);
        // The plugin that serves the pages is installed as `hello`: the one
        // of the registry replaces it later, so it is kept under its own name.
        registry_with_hello(dir);
        let said = ok(dir, &["user", "role", "ada", "admin"]);
        assert!(said.contains("ada is an admin"), "{said}");
        assert!(ok(dir, &["user", "list"]).contains("ada [token] (admin): every repository"));
    });
    let session = |token: &str| {
        let opened = served.request("POST", "/-/session", &[], Some(&json!({ "token": token })));
        let value =
            opened.head.lines().find_map(|line| line.strip_prefix("set-cookie: kito_session=")).unwrap();
        format!("kito_session={}", value.split(';').next().unwrap())
    };
    let (ada, carol) = (session(&served.ada), session(&served.carol));
    let page = [("Cookie", ada.as_str()), ("X-Kito", "1")];

    // Who is an administrator is said to the page; nobody else is let in,
    // and the answer is the same whoever they are.
    assert_eq!(served.request("GET", "/-/me", &[("Cookie", &ada)], None).json()["admin"], true);
    assert_eq!(served.request("GET", "/-/me", &[("Cookie", &carol)], None).json()["admin"], false);
    assert_eq!(served.request("GET", "/-/admin/plugins", &[], None).status, 401);
    let refused = served.request("GET", "/-/admin/plugins", &[("Cookie", &carol), ("X-Kito", "1")], None);
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(refused.body.contains("for its admins"), "{}", refused.body);
    let change = json!({ "action": "disable", "name": "hello" });
    assert_eq!(
        served
            .request("POST", "/-/admin/plugins", &[("Cookie", &carol), ("X-Kito", "1")], Some(&change))
            .status,
        403
    );
    // A page of another site can send the cookie along, and nothing more.
    assert_eq!(served.request("GET", "/-/admin/plugins", &[("Cookie", &ada)], None).status, 403);
    assert_eq!(served.request("POST", "/-/admin/plugins", &[("Cookie", &ada)], Some(&change)).status, 403);

    // What is there, and what the registry has.
    let listed = served.request("GET", "/-/admin/plugins", &page, None);
    assert_eq!(listed.status, 200, "{}", listed.body);
    let listed = listed.json();
    assert_eq!(
        listed["listed"],
        json!([{ "name": "hello", "about": "Says what it is told", "latest": "0.9.0" }])
    );
    let hello = &listed["installed"][0];
    assert_eq!(
        (hello["name"].as_str(), hello["enabled"].as_bool(), hello["serves_pages"].as_bool()),
        (Some("hello"), Some(true), Some(true))
    );
    let ask = |body: Value| served.request("POST", "/-/admin/plugins", &page, Some(&body));

    // The plugin that serves the pages is not taken out from the pages.
    let no = ask(json!({ "action": "disable", "name": "hello" }));
    assert!(no.status == 400 && no.body.contains("serves the pages of this server"), "{}", no.body);
    let no = ask(json!({ "action": "remove", "name": "hello" }));
    assert!(no.status == 400 && no.body.contains("is enabled on this server"), "{}", no.body);
    // Only by name, and only what the registry has.
    for name in ["../hello", "https://evil.example/x", "nope"] {
        let no = ask(json!({ "action": "install", "name": name }));
        assert_eq!(no.status, 400, "{name}: {}", no.body);
    }
    assert!(ask(json!({ "action": "upload", "name": "hello" }))
        .body
        .contains("installed, enabled, disabled or removed"));

    // On the server's machine it is disabled; from then on the browser
    // installs the one the registry lists, enables it with what it is
    // given, and takes it out.
    ok(&served.dir, &["plugin", "disable", "hello", "--no-cleanup"]);
    let installed = ask(json!({ "action": "install", "name": "hello" }));
    assert_eq!(installed.status, 200, "{}", installed.body);
    let installed = installed.json();
    assert!(installed["done"]
        .as_str()
        .unwrap()
        .contains("hello 0.9.0 is installed. It runs nowhere until it is enabled."));
    assert_eq!(installed["asks"], json!(["read", "server_events"]));
    assert_eq!(installed["from"]["sha256"].as_str().unwrap().len(), 64);
    let more = ask(json!({ "action": "enable", "name": "hello", "permissions": ["server_events", "pages"] }));
    assert!(more.status == 400 && more.body.contains("does not ask for `pages`"), "{}", more.body);
    let enabled = ask(
        json!({ "action": "enable", "name": "hello", "permissions": ["server_events"], "for_clients": true }),
    );
    assert_eq!(enabled.status, 200, "{}", enabled.body);
    assert!(enabled.json()["done"].as_str().unwrap().contains("It may: server_events."));
    let now = served.request("GET", "/-/admin/plugins", &page, None).json();
    let hello = &now["installed"][0];
    assert_eq!(hello["version"], "0.9.0");
    assert_eq!(
        (hello["permissions"].clone(), hello["for_clients"].as_bool()),
        (json!(["server_events"]), Some(true))
    );

    let disabled = ask(json!({ "action": "disable", "name": "hello", "purge": true }));
    assert_eq!(disabled.status, 200, "{}", disabled.body);
    // It was run one last time, as the server's.
    assert_eq!(fs::read_to_string(served.dir.join("cleaned")).unwrap(), "server server=true\n");
    let removed = ask(json!({ "action": "remove", "name": "hello" }));
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert!(removed.json()["done"].as_str().unwrap().contains("removed from the server's machine"));
    assert_eq!(served.request("GET", "/-/admin/plugins", &page, None).json()["installed"], json!([]));
    assert!(!served.dir.join(".personal-config/plugins/hello").exists());
    assert!(ask(json!({ "action": "remove", "name": "hello" })).body.contains("is not installed"));

    // An administrator who is one no longer is refused at once.
    ok(&served.dir, &["user", "role", "ada", "member"]);
    assert_eq!(served.request("GET", "/-/admin/plugins", &page, None).status, 403);
}

/// A git repository served as plain files over HTTP on this machine, as
/// a host that knows nothing of git serves one. It answers nothing until
/// it is opened: a clone from it takes as long as a test wants.
struct SlowGit {
    address: String,
    asked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    open: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SlowGit {
    /// A repository with one commit, made under `dir`, and served.
    fn start(dir: &Path) -> SlowGit {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let git = |at: &Path, args: &[&str]| {
            let done = Command::new("git")
                .arg("-C")
                .arg(at)
                .args(["-c", "user.name=ada", "-c", "user.email=ada@example.com"])
                .args(args)
                .output()
                .expect("these tests use git");
            assert!(done.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&done.stderr));
        };
        let (work, bare) = (dir.join("elsewhere"), dir.join("elsewhere.git"));
        fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "--quiet"]);
        fs::write(work.join("README.md"), "# elsewhere\n").unwrap();
        git(&work, &["add", "README.md"]);
        git(&work, &["commit", "--quiet", "-m", "first"]);
        git(dir, &["clone", "--quiet", "--bare", "elsewhere", "elsewhere.git"]);
        // What a host that serves it as files needs to be there.
        git(&bare, &["update-server-info"]);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (asked, open) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (was_asked, opened) = (asked.clone(), open.clone());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let (bare, was_asked, opened) = (bare.clone(), was_asked.clone(), opened.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let (mut first, mut line) = (String::new(), String::new());
                    let _ = reader.read_line(&mut first);
                    while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                        line.clear();
                    }
                    was_asked.store(true, Ordering::SeqCst);
                    while !opened.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    // The file asked for, under the repository and nowhere else.
                    let path = first.split(' ').nth(1).unwrap_or("/").split('?').next().unwrap_or("/");
                    let file = bare.join(path.trim_start_matches('/'));
                    let bytes = Some(file)
                        .filter(|file| !path.contains("..") && file.is_file())
                        .and_then(|file| fs::read(file).ok());
                    let head = match &bytes {
                        Some(bytes) => format!("200 OK\r\nContent-Length: {}", bytes.len()),
                        None => "404 Not Found\r\nContent-Length: 0".to_owned(),
                    };
                    let _ = write!(stream, "HTTP/1.1 {head}\r\nConnection: close\r\n\r\n");
                    let _ = stream.write_all(&bytes.unwrap_or_default());
                });
            }
        });
        SlowGit { address, asked, open }
    }

    /// Waits until somebody clones it: they asked, and are not answered yet.
    fn asked(&self) {
        for _ in 0..3000 {
            if self.asked.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("nobody cloned it");
    }

    /// From here on it answers.
    fn open(&self) {
        self.open.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn a_change_that_takes_its_time_does_not_bring_back_a_token_taken_away_meanwhile() {
    let served = Served::start(|dir| {
        install_hello(dir, LINKS);
        ok(dir, &["plugin", "enable", "hello"]);
        ok(dir, &["user", "role", "ada", "admin"]);
        // bob signs in with hello, and has a token too: the one that leaked.
        ok(dir, &["user", "add", "bob", "--identity", "hello:bob"]);
    });
    let olga = token_in(&ok(&served.dir, &["user", "add", "olga", "--role", "owner"]));
    let leaked = token_in(&ok(&served.dir, &["user", "token", "bob"]));
    let (as_ada, as_olga) = (format!("Bearer {}", served.ada), format!("Bearer {olga}"));
    let as_bob = format!("Bearer {leaked}");
    assert_eq!(served.me(&[("Authorization", &as_bob)]).json()["user"], "bob");

    let elsewhere = SlowGit::start(served.dir.parent().unwrap());
    let from = format!("http://{}/", elsewhere.address);
    let added = std::thread::scope(|scope| {
        // ada adds a repository from a git address: a clone, which takes its time.
        let adding = scope.spawn(|| {
            let slow = json!({ "name": "slow", "from": from });
            served.people(&[("Authorization", &as_ada)], "server.repo_add", slow)
        });
        elsewhere.asked();
        // Meanwhile the owner takes bob's token away, and is told it is gone;
        // and makes carol an admin.
        let taken = json!({ "user": "bob", "remove": true });
        let taken = served.people(&[("Authorization", &as_olga)], "server.people_token", taken);
        assert_eq!(taken.json(), json!({ "user": "bob", "token": null }), "{}", taken.body);
        assert_eq!(served.me(&[("Authorization", &as_bob)]).status, 401);
        let made = json!({ "user": "carol", "role": "admin" });
        assert_eq!(served.people(&[("Authorization", &as_olga)], "server.people_role", made).status, 200);
        // The clone ends, and ada is given what it made.
        elsewhere.open();
        adding.join().unwrap()
    });
    assert_eq!((added.status, added.json()), (200, json!({ "name": "slow" })), "{}", added.body);
    assert!(served.dir.join("repos/slow/README.md").is_file());

    // What she is given is written on the server as it is by then: the
    // token that was taken away is nobody's still, and carol is an admin.
    assert_eq!(served.me(&[("Authorization", &as_bob)]).status, 401, "the token taken away works again");
    let unknown = json!({ "error": "this token is not one the server knows" });
    assert_eq!(Wire::open(&served.clients, &leaked).1, unknown);
    let people = served.people(&[("Authorization", &as_olga)], "server.people", json!({})).json();
    let of =
        |name: &str| people.as_array().unwrap().iter().find(|them| them["user"] == name).unwrap().clone();
    assert_eq!(
        (of("bob")["token"].clone(), of("bob")["logins"].clone()),
        (json!(false), json!(["hello:bob"]))
    );
    assert_eq!(of("carol")["role"], "admin");
    assert_eq!(of("ada")["access"]["slow"], "maintain");
    if let Ok(mut daemon) = kitod::client::Client::connect(&served.dir.join("repos/slow")) {
        let _ = daemon.call_value("shutdown", json!({}));
    }
}

#[test]
fn people_who_change_their_tokens_at_once_each_end_with_the_token_they_were_answered() {
    let served = Served::start(|_| {});
    // Each on a connection of their own, which goes on with each new token.
    let changing = |first: &str| -> Vec<String> {
        let (mut wire, greeting) = Wire::open(&served.clients, first);
        assert!(greeting["ok"].is_object(), "{greeting}");
        let mut answered = Vec::new();
        for _ in 0..30 {
            let answer = wire.ask("server.people_token", json!({}));
            answered
                .push(answer["result"]["token"].as_str().unwrap_or_else(|| panic!("{answer}")).to_owned());
        }
        answered
    };
    let (ada, carol) = std::thread::scope(|scope| {
        let (ada, carol) = (scope.spawn(|| changing(&served.ada)), scope.spawn(|| changing(&served.carol)));
        (ada.join().unwrap(), carol.join().unwrap())
    });
    for (name, first, answered) in [("ada", &served.ada, &ada), ("carol", &served.carol, &carol)] {
        // The last each was answered is theirs; nothing they replaced is anybody's.
        let (last, replaced) = answered.split_last().unwrap();
        let bearer = format!("Bearer {last}");
        assert_eq!(served.me(&[("Authorization", &bearer)]).json()["user"], name);
        assert_eq!(Wire::open(&served.clients, last).1["ok"]["user"], name);
        for old in replaced.iter().chain([first]) {
            let bearer = format!("Bearer {old}");
            assert_eq!(served.me(&[("Authorization", &bearer)]).status, 401, "a token {name} replaced works");
        }
    }
}

#[test]
fn what_cannot_be_checked_is_refused_on_the_clients_port() {
    let aged = |path: &Path| {
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        fs::File::options().write(true).open(path).unwrap().set_modified(then).unwrap();
    };
    let served = Served::start(|dir| {
        // What a server stopped in the middle of a write left behind, a
        // while ago; and what somebody is writing just now.
        for left in ["server.toml.4242-7.new", "server.toml.4242-8.new"] {
            fs::write(dir.join(left), "left behind\n").unwrap();
        }
        aged(&dir.join("server.toml.4242-7.new"));
    });
    // Swept when the server starts, once it is old.
    assert!(!served.dir.join("server.toml.4242-7.new").exists());
    assert!(served.dir.join("server.toml.4242-8.new").exists());

    let (mut open, greeting) = Wire::open(&served.clients, &served.carol);
    assert_eq!(greeting["ok"]["user"], "carol", "{greeting}");
    assert_eq!(open.ask("server.whoami", json!({}))["result"]["user"], "carol");
    // The configuration cannot be read: nobody is held to what it was.
    let (config, away) = (served.dir.join("server.toml"), served.dir.join("server.toml.away"));
    fs::rename(&config, &away).unwrap();
    let unreadable = json!({ "error": "the server's configuration cannot be read just now: try again" });
    // An open connection is not answered on what it knew...
    assert_eq!(open.ask("server.whoami", json!({})), unreadable);
    assert_eq!(open.ask("server.whoami", json!({})), Value::Null);
    // ...and a new one is not let in on what was read when the server
    // started, with a token or to sign in.
    assert_eq!(Wire::open(&served.clients, &served.carol).1, unreadable);
    let stream = TcpStream::connect(&served.clients).unwrap();
    let mut wire = Wire { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream, asked: 0 };
    assert_eq!(wire.say(&json!({ "sign_in": { "version": 1 } })), unreadable);
    // The same for one that is there and is no configuration.
    fs::write(&config, "users = 5\n").unwrap();
    assert_eq!(Wire::open(&served.clients, &served.carol).1, unreadable);
    let bearer = format!("Bearer {}", served.carol);
    assert_eq!(served.me(&[("Authorization", &bearer)]).status, 500);
    // Readable again, it is what everybody is held to.
    fs::rename(&away, &config).unwrap();
    assert_eq!(Wire::open(&served.clients, &served.carol).1["ok"]["user"], "carol");
    assert_eq!(served.me(&[("Authorization", &bearer)]).json()["user"], "carol");
}

#[test]
fn a_sign_out_that_could_not_be_made_is_said() {
    let served = Served::start(|_| {});
    let tab = served.session(&served.carol);
    // Something holds what the server keeps, and does not let go: a lock
    // written again before each try, for it not to be old enough to be
    // taken over.
    let lock = served.dir.join(".lock");
    let held = || fs::write(&lock, "somebody else").unwrap();
    held();
    let ended = served.request("DELETE", "/-/session", &[("Cookie", &tab)], None);
    assert_eq!(ended.status, 500, "{}", ended.body);
    assert!(ended.body.contains("held by something that does not finish"), "{}", ended.body);
    // The browser keeps its cookie: the session goes on, and it is told so.
    assert!(!ended.head.contains("set-cookie"), "{}", ended.head);
    // Nobody is signed in or removed meanwhile, and that is said too.
    held();
    let opening = json!({ "token": served.ada });
    let refused = served.request("POST", "/-/session", &[], Some(&opening));
    assert_eq!(refused.status, 500, "{}", refused.body);
    held();
    let gone = server(&served.dir, &["user", "remove", "carol"]).output().unwrap();
    assert!(!gone.status.success());
    let said = String::from_utf8_lossy(&gone.stderr).into_owned();
    assert!(said.contains("held by something that does not finish: nothing was changed"), "{said}");
    fs::remove_file(&lock).unwrap();
    assert_eq!(served.me(&[("Cookie", &tab)]).json()["user"], "carol");

    // Let go, the session ends, and the browser forgets it.
    let ended = served.request("DELETE", "/-/session", &[("Cookie", &tab)], None);
    assert_eq!(ended.status, 200, "{}", ended.body);
    assert!(ended.head.contains("set-cookie: kito_session=;"), "{}", ended.head);
    assert_eq!(served.me(&[("Cookie", &tab)]).status, 401);
}

const HALLO: &str = r#"name = "hallo"
version = "0.1.0"
api = 1
permissions = ["sign_in"]

[sign_in]
method = "hallo"
about = "Sign in with hallo"
"#;

#[test]
fn an_identity_of_a_way_the_server_offers_no_more_is_no_way_to_sign_in() {
    let served = Served::start(|dir| {
        install_hello(dir, LINKS);
        install_as(dir, "hallo", HALLO);
        ok(dir, &["plugin", "enable", "hello"]);
        ok(dir, &["plugin", "enable", "hallo"]);
        ok(dir, &["user", "role", "ada", "admin"]);
        // carol has a token and signs in with hallo; bob signs in with
        // hello or with hallo, and has no token.
        ok(dir, &["user", "link", "carol", "hallo:carol"]);
        ok(dir, &["user", "add", "bob", "--identity", "hello:bob"]);
        ok(dir, &["user", "link", "bob", "hallo:bob"]);
    });
    let (as_ada, as_carol) = (format!("Bearer {}", served.ada), format!("Bearer {}", served.carol));
    let ask = |who: &str, method: &str, params: Value| {
        served.people(&[("Authorization", who)], &format!("server.people_{method}"), params)
    };
    // The plugin that signs people in with hallo is disabled: whoever
    // has an identity of that way does not sign in with it.
    ok(&served.dir, &["plugin", "disable", "hallo", "--no-cleanup"]);
    let no_more = "and this server offers that way no more (no plugin enabled here signs people in with it)";

    // carol's token is all she signs in with: it stays, whoever asks.
    let hers = format!(
        "carol would have no way to sign in: hallo:carol is all they would have, {no_more}: link \
                        an identity to them first"
    );
    for (who, params) in
        [(&as_carol, json!({ "remove": true })), (&as_ada, json!({ "user": "carol", "remove": true }))]
    {
        let no = ask(who, "token", params);
        assert_eq!(no.status, 403, "{}", no.body);
        assert!(no.body.contains(&hers), "{}", no.body);
    }
    let no = server(&served.dir, &["user", "token", "carol", "--remove"]).output().unwrap();
    assert!(!no.status.success());
    assert!(String::from_utf8_lossy(&no.stderr).contains(&hers), "{}", String::from_utf8_lossy(&no.stderr));
    assert_eq!(served.me(&[("Authorization", &as_carol)]).json()["user"], "carol");

    // bob's identity of hello is all he signs in with: it stays. The one
    // of hallo may go: it signs nobody in.
    let his = format!(
        "bob would have no way to sign in: hallo:bob is all they would have, {no_more}: give them \
                       a token first"
    );
    let no = ask(&as_ada, "unlink", json!({ "user": "bob", "method": "hello" }));
    assert_eq!(no.status, 403, "{}", no.body);
    assert!(no.body.contains(&his), "{}", no.body);
    let no = server(&served.dir, &["user", "unlink", "bob", "hello"]).output().unwrap();
    assert!(String::from_utf8_lossy(&no.stderr).contains(&his), "{}", String::from_utf8_lossy(&no.stderr));
    let gone = ask(&as_ada, "unlink", json!({ "user": "bob", "method": "hallo" }));
    assert_eq!(
        gone.json(),
        json!({ "user": "bob", "token": false, "logins": ["hello:bob"] }),
        "{}",
        gone.body
    );

    // Enabled again, hallo is a way to sign in: carol's token can go.
    ok(&served.dir, &["plugin", "enable", "hallo"]);
    let removed = ask(&as_carol, "token", json!({ "remove": true }));
    assert_eq!(removed.json(), json!({ "user": "carol", "token": null }), "{}", removed.body);
}

#[cfg(unix)]
#[test]
fn who_may_link_by_name_is_asked_again_once_the_plugin_has_answered() {
    let served = Served::start(|dir| {
        install_hello(dir, LINKS);
        ok(dir, &["plugin", "enable", "hello"]);
        ok(dir, &["user", "role", "ada", "admin"]);
        // The plugin takes its time to say who a login is.
        slow_hello(dir, "sign-in lookup", 3);
    });
    let olga = token_in(&ok(&served.dir, &["user", "add", "olga", "--role", "owner"]));
    let (as_ada, as_olga, as_carol) =
        (format!("Bearer {}", served.ada), format!("Bearer {olga}"), format!("Bearer {}", served.carol));
    let refused = std::thread::scope(|scope| {
        // ada, an admin, links an account to carol, a member...
        let linking = scope.spawn(|| {
            let link = json!({ "user": "carol", "identity": "hello:carol" });
            served.people(&[("Authorization", &as_ada)], "server.people_link", link)
        });
        // ...who is made an owner while the plugin is asked who that is.
        std::thread::sleep(std::time::Duration::from_millis(600));
        let made = json!({ "user": "carol", "role": "owner" });
        let made = served.people(&[("Authorization", &as_olga)], "server.people_role", made);
        assert_eq!(made.status, 200, "{}", made.body);
        linking.join().unwrap()
    });
    // An admin attaches nothing to an owner: asked where the link is made.
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(refused.body.contains("carol is an owner: only an owner changes them"), "{}", refused.body);
    let how = served.me(&[("Authorization", &as_carol)]).json()["signs_in"].clone();
    assert_eq!(how, json!({ "token": true, "logins": [] }));
}

#[cfg(unix)]
#[test]
fn a_plugin_that_is_slow_holds_no_token_back() {
    let served = Served::start(|dir| {
        install_hello(dir, LINKS);
        ok(dir, &["plugin", "enable", "hello"]);
        // The plugin takes its time to hear what happened.
        slow_hello(dir, "server-event", 5);
    });
    let log = served.dir.join("plugins/hello/events.log");
    let tab = served.session(&served.carol);
    let _other = served.session(&served.carol);
    let asked = std::time::Instant::now();
    let renewed = served.people(&[("Cookie", &tab), ("X-Kito", "1")], "server.people_token", json!({}));
    let took = asked.elapsed();
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    let new = renewed.json()["token"].as_str().unwrap().to_owned();
    // The token is told before a plugin has heard of it; what happened is
    // kept by then, and the token works.
    assert!(took < std::time::Duration::from_secs(4), "the token waited {took:?} for a plugin");
    assert!(!log.exists(), "a plugin was told before the answer");
    let (lines, _) = audited(&served.dir);
    for line in ["carol token_changed carol", "carol session_ended carol"] {
        assert!(lines.iter().any(|kept| kept == line), "no `{line}` in {lines:#?}");
    }
    let bearer = format!("Bearer {new}");
    assert_eq!(served.me(&[("Authorization", &bearer)]).json()["user"], "carol");
    // The plugin hears of it all the same: the change, then the session it ended.
    assert_eq!(told(&served, 2), ["token_changed carol", "session_ended carol"]);
}

#[cfg(unix)]
#[test]
fn a_server_that_serves_one_client_tells_the_plugins_before_it_leaves() {
    let served = Served::start(|dir| {
        install_hello(dir, LINKS);
        ok(dir, &["plugin", "enable", "hello"]);
        slow_hello(dir, "server-event", 1);
    });
    // As SSH runs it: one client on its standard input, known by the name
    // it came as, and gone when the client is.
    let mut child = server(&served.dir, &["stdio", "--as", "carol"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut say = |message: Value| -> Value {
        writeln!(input, "{message}").unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or(Value::Null)
    };
    assert_eq!(say(json!({ "hello": { "version": 1 } }))["ok"]["user"], "carol");
    let asked = json!({ "jsonrpc": "2.0", "id": 1, "method": "server.people_token", "params": {} });
    let answer = say(asked);
    assert!(answer["result"]["token"].as_str().is_some_and(|token| token.starts_with("kito_")), "{answer}");
    // She has her token while the plugin has heard nothing yet.
    let log = served.dir.join("plugins/hello/events.log");
    assert!(!log.exists(), "a plugin was told before the answer");
    drop(input);
    assert!(child.wait().unwrap().success());
    // The server left once the plugin had: nothing is waited for here.
    assert_eq!(fs::read_to_string(&log).unwrap_or_default(), "token_changed carol\n");
}

#[test]
fn served_securely_the_sign_in_cookie_is_one_only_this_host_sets() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("server");
    ok(tmp.path(), &["init", dir.to_str().unwrap()]);
    install_hello(&dir, SIGNS);
    ok(&dir, &["plugin", "enable", "hello"]);
    ok(&dir, &["user", "add", "bob", "--identity", "hello:bob"]);
    let mut child = server(&dir, &["serve", "--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let address =
        line.trim().strip_prefix("kito-server: ").and_then(|rest| rest.split(",").next()).unwrap().to_owned();
    assert!(address.starts_with("https://127.0.0.1:"), "{line}");
    let curl = |args: &[&str]| {
        let out = Command::new("curl")
            .args(["--silent", "--insecure", "--include", "--max-time", "20"])
            .args(args)
            .output();
        let out = out.expect("these tests ask over HTTPS with curl");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let set = |answer: &str, name: &str| -> Option<String> {
        let line =
            answer.lines().find(|line| line.to_lowercase().starts_with(&format!("set-cookie: {name}=")));
        line.and_then(|line| line.split_once(": ")).map(|(_, value)| value.trim().to_owned())
    };
    let left = curl(&[&format!("{address}/-/sign-in/hello")]);
    let back = format!("{address}/-/sign-in/hello/callback?code=bob&state=left-1");
    // Brought back under the name it has in the clear, it is not the one that left.
    let planted = curl(&["--cookie", "kito_sign_in=left-1", &back]);
    let signed = curl(&["--cookie", "__Host-kito_sign_in=left-1", &back]);
    let _ = child.kill();
    let _ = child.wait();
    // `__Host-`: with `Secure`, for every path and with no `Domain`, as a browser wants it.
    assert_eq!(
        set(&left, "__host-kito_sign_in").as_deref(),
        Some("__Host-kito_sign_in=left-1; Path=/; HttpOnly; SameSite=Lax; Max-Age=600; Secure"),
        "{left}"
    );
    assert!(set(&left, "kito_sign_in").is_none(), "{left}");
    assert!(planted.contains("this sign-in did not start in this browser"), "{planted}");
    assert!(signed.contains("Signed in as bob."), "{signed}");
    // Forgotten under the name it was set with.
    assert_eq!(
        set(&signed, "__host-kito_sign_in").as_deref(),
        Some("__Host-kito_sign_in=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; Secure"),
        "{signed}"
    );
}
