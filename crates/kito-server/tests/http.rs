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
        let ada = token_in(&ok(&dir, &["user", "add", "ada", "--repo", "shop:integrate"]));
        let carol = token_in(&ok(&dir, &["user", "add", "carol", "--repo", "shop:read"]));
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
        assert!(ok(dir, &["user", "list"]).contains("ada (admin): every repository"));
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
