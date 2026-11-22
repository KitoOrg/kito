//! The real server, with the real daemons it starts for its repositories.

use kito_core::paths::Canonical as _;
use kitod::client::{Client, ClientError, Remote};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The daemon binary sits next to the server's. If the tests run for this
/// package alone it may not have been built.
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
        .env("KITO_CONFIG_DIR", dir.join(".absent-personal-config"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    command
}

/// Runs a server command that must succeed and returns what it printed.
fn ok(dir: &Path, args: &[&str]) -> String {
    let out = server(dir, args).output().unwrap();
    assert!(
        out.status.success(),
        "kito-server {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg_attr(not(unix), allow(dead_code))]
fn refused(dir: &Path, args: &[&str]) -> String {
    let out = server(dir, args).output().unwrap();
    assert!(!out.status.success(), "kito-server {args:?} should have failed");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

const SHOP: &str =
    "export function total(price: number, quantity: number): number {\n  return price * quantity;\n}\n";

/// A git repository with one commit, to give the server something to host.
fn source(dir: &Path, name: &str, file: &str, content: &str) -> PathBuf {
    let root = dir.join(name);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join(file), content).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "start"]);
    root
}

/// A server directory with two repositories and two people: ada may use
/// both, bob only the shop. Returns their tokens.
struct World {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    ada: String,
    bob: String,
    serving: Option<Child>,
    /// What the server said when it last started.
    said: String,
}

fn token_in(printed: &str) -> String {
    printed.lines().find(|line| line.starts_with("kito_")).expect(printed).trim().to_owned()
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonical().unwrap();
        let dir = base.join("server");
        assert!(ok(&base, &["init", dir.to_str().unwrap()]).contains("Kito server created"));
        let shop = source(&base, "shop-src", "src/shop.ts", SHOP);
        let site = source(&base, "site-src", "src/index.ts", "export const title = \"site\";\n");
        ok(&dir, &["repo", "add", "shop", "--from", shop.to_str().unwrap()]);
        ok(&dir, &["repo", "add", "site", "--from", site.to_str().unwrap()]);
        let ada = token_in(&ok(
            &dir,
            &[
                "user",
                "add",
                "ada",
                "--email",
                "ada@example.com",
                "--repo",
                "shop:integrate",
                "--repo",
                "site:integrate",
            ],
        ));
        let bob = token_in(&ok(&dir, &["user", "add", "bob", "--repo", "shop:integrate"]));
        World { _tmp: tmp, dir, ada, bob, serving: None, said: String::new() }
    }

    /// Starts `serve` on a free port and returns its address.
    fn serve(&mut self) -> String {
        self.serve_with(&[])
    }

    /// Starts the server; returns the address it listens on. What it
    /// said when it started is kept in `said`.
    fn serve_with(&mut self, more: &[&str]) -> String {
        let args: Vec<&str> = ["serve", "--listen", "127.0.0.1:0"].iter().chain(more).copied().collect();
        let mut child =
            server(&self.dir, &args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        self.serving = Some(child);
        self.said = line.trim().to_owned();
        let address = line.trim().strip_prefix("kito-server: listening on ").expect(&line);
        address.split(',').next().unwrap().to_owned()
    }

    fn repo(&self, name: &str) -> PathBuf {
        self.dir.join("repos").join(name)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(child) = &mut self.serving {
            let _ = child.kill();
            let _ = child.wait();
        }
        // The daemons the server started for its repositories.
        for name in ["shop", "site"] {
            let root = self.repo(name);
            if let Ok(mut client) = Client::connect(&root) {
                let _ = client.call_value("shutdown", json!({}));
                let socket = kitod::rpc::socket_path(&root);
                for _ in 0..1000 {
                    if !socket.exists() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

fn remote(address: &str, token: &str, repo: &str) -> Remote {
    Remote {
        server: Some(address.to_owned()),
        command: Vec::new(),
        repo: repo.to_owned(),
        token: Some(token.to_owned()),
        ..Default::default()
    }
}

fn connect(address: &str, token: &str, repo: &str) -> Client {
    Client::remote(&remote(address, token, repo), true).unwrap()
}

fn refusal(result: Result<Client, ClientError>) -> String {
    match result {
        Err(ClientError::Refused(reason)) => reason,
        Err(other) => panic!("refused in another way: {other}"),
        Ok(_) => panic!("the server let it in"),
    }
}

#[test]
#[cfg(unix)] // Unix permissions or links: not on Windows (ADR 0002)
fn people_repositories_and_tokens() {
    let world = World::new();
    // A repository that came with a history is protected from the start.
    assert_eq!(
        ok(&world.dir, &["repo", "list"]),
        "shop  [main protected, no checks]\nsite  [main protected, no checks]\n"
    );
    let users = ok(&world.dir, &["user", "list"]);
    assert!(
        users.contains("ada <ada@example.com> (member): shop (integrate), site (integrate)")
            && users.contains("bob (member): shop (integrate)"),
        "{users}"
    );
    // The history of the git repository came along.
    assert!(world.repo("shop").join("src/shop.ts").is_file());

    // A token is shown once and kept only as what it hashes to, in a file
    // nobody else can read.
    assert!(world.ada.starts_with("kito_") && world.ada.len() == 5 + 64 && world.ada != world.bob);
    let config = fs::read_to_string(world.dir.join("server.toml")).unwrap();
    assert!(!config.contains(&world.ada) && !config.contains(&world.bob), "{config}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(world.dir.join("server.toml")).unwrap().permissions().mode() & 0o777, 0o600);

    // What cannot be: a second server, names that are paths, things twice.
    assert!(refused(&world.dir, &["init"]).contains("is a server already"));
    assert!(refused(&world.dir, &["repo", "add", "../out"]).contains("not a name a repository can have"));
    assert!(refused(&world.dir, &["repo", "add", "shop"]).contains("already has something called"));
    assert!(refused(&world.dir, &["user", "add", "ada"]).contains("is a user already"));
    assert!(refused(&world.dir, &["user", "add", "carol", "--repo", "nowhere"])
        .contains("no repository `nowhere`"));
    assert!(refused(&world.dir.join("repos"), &["repo", "list"]).contains("is not a Kito server"));

    // Removed, a person's token is gone with them.
    ok(&world.dir, &["user", "remove", "bob"]);
    assert!(!ok(&world.dir, &["user", "list"]).contains("bob"));
    assert!(refused(&world.dir, &["user", "remove", "bob"]).contains("not a user"));
}

#[test]
fn every_session_is_authenticated_and_bound_to_what_its_person_may_use() {
    let mut world = World::new();
    let address = world.serve();

    // No token, a wrong one, a repository that is not theirs or not there:
    // refused, and told no more than that.
    let unknown = "this token is not one the server knows";
    assert_eq!(refusal(Client::remote(&remote(&address, "kito_wrong", "shop"), true)), unknown);
    let none = Remote { token: None, ..remote(&address, "", "shop") };
    assert_eq!(refusal(Client::remote(&none, true)), unknown);
    let not_bobs = refusal(Client::remote(&remote(&address, &world.bob, "site"), true));
    let not_there = refusal(Client::remote(&remote(&address, &world.bob, "nowhere"), true));
    assert_eq!(not_bobs, "there is no repository `site` for bob on this server");
    assert_eq!(not_there, "there is no repository `nowhere` for bob on this server");

    // A client that does not encrypt gets nothing done, and nothing to read.
    let mut raw = std::net::TcpStream::connect(&address).unwrap();
    raw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"lanes\",\"params\":{}}\n").unwrap();
    let mut answer = Vec::new();
    let _ = std::io::Read::read_to_end(&mut raw, &mut answer);
    let answer = String::from_utf8_lossy(&answer);
    assert!(!answer.contains("result") && !answer.contains("lane"), "{answer}");

    // Each sees the repositories they may use.
    let mut ada = Client::remote(&remote(&address, &world.ada, "shop"), false).unwrap();
    let mut bob = Client::remote(&remote(&address, &world.bob, "shop"), false).unwrap();
    assert_eq!(ada.call_value("server.repos", json!({})).unwrap(), json!(["shop", "site"]));
    assert_eq!(bob.call_value("server.repos", json!({})).unwrap(), json!(["shop"]));
    assert_eq!(bob.call_value("server.whoami", json!({})).unwrap()["user"], "bob");
    // A session about no repository can ask nothing of one.
    let error = ada.call_value("lanes", json!({})).unwrap_err().to_string();
    assert!(error.contains("about no repository"), "{error}");

    // What stops a daemon or rewrites the main lane is not for clients.
    let mut ada = connect(&address, &world.ada, "shop");
    for method in ["shutdown", "checkpoint", "rollback", "reload", "export_git", "import_scip"] {
        let error = ada.call_value(method, json!({})).unwrap_err().to_string();
        assert!(error.contains("not something a client can ask through the server"), "{method}: {error}");
    }
    assert!(ada.call_value("status", json!({})).unwrap()["repo"]["files"].as_u64().unwrap() >= 1);
    // The daemon's own errors come back as they are.
    let error = ada.call_value("lane_status", json!({ "lane": "lane/9" })).unwrap_err().to_string();
    assert!(error.contains("no such open lane"), "{error}");
}

#[test]
fn a_lane_opened_by_one_client_is_seen_by_another_within_a_second() {
    let mut world = World::new();
    let address = world.serve();
    let mut ada = connect(&address, &world.ada, "shop");
    let mut bob = connect(&address, &world.bob, "shop");

    // Bob watches: a request that waits for what happens next.
    let from = bob.call_value("events", json!({})).unwrap()["next"].as_u64().unwrap();
    let watching = std::thread::spawn(move || {
        let heard = bob.call_value("events", json!({ "after": from, "wait_ms": 10_000 })).unwrap();
        (Instant::now(), heard, bob)
    });
    std::thread::sleep(Duration::from_millis(100));

    // Ada opens a lane. She cannot say she is somebody else.
    let asked = Instant::now();
    let lane = ada
        .call_value(
            "open_lane",
            json!({
                "agent": "builder",
                "intent": "prices in cents",
                "scope": ["src/shop.ts:total"],
                "on_behalf_of": { "name": "mallory" },
            }),
        )
        .unwrap();
    assert_eq!((lane["id"].as_str(), lane["owner"].as_str()), (Some("lane/1"), Some("ada")));

    let (heard_at, heard, mut bob) = watching.join().unwrap();
    let took = heard_at.saturating_duration_since(asked);
    assert!(took < Duration::from_secs(1), "bob heard after {took:?}");
    let event = &heard["events"][0];
    assert_eq!(
        (event["kind"].as_str(), event["lane"].as_str(), event["by"].as_str()),
        (Some("lane_opened"), Some("lane/1"), Some("ada"))
    );
    assert!(event["what"].as_str().unwrap().contains("builder: prices in cents [src/shop.ts:total]"));

    // Bob sees the lane, whose it is, and the board; and opening one on the
    // same function he is told of hers.
    let lanes = bob.call_value("lanes", json!({})).unwrap();
    assert_eq!((lanes[0]["agent"].as_str(), lanes[0]["owner"].as_str()), (Some("builder"), Some("ada")));
    let his = bob
        .call_value(
            "open_lane",
            json!({ "agent": "fixer", "intent": "a discount", "scope": ["src/shop.ts:total"] }),
        )
        .unwrap();
    assert_eq!(his["owner"], "bob");
    assert_eq!(his["overlaps"][0]["lane"], "lane/1");
    let board = bob.call_value("board", json!({})).unwrap();
    assert_eq!(board.as_array().unwrap().len(), 2);
    assert_eq!(board[0]["owner"], "ada");

    // What is saved in a lane, and how its integration ends, is heard too.
    let from = bob.call_value("events", json!({})).unwrap()["next"].as_u64().unwrap();
    let workdir = PathBuf::from(lane["workdir"].as_str().unwrap());
    fs::write(workdir.join("src/shop.ts"), SHOP.replace("price * quantity", "(price * quantity) / 100"))
        .unwrap();
    let job = ada.call_value("request_integration", json!({ "lane": "lane/1" })).unwrap()["job"].clone();
    let done = ada.call_value("integration_result", json!({ "job": job, "wait_ms": 20_000 })).unwrap();
    assert_eq!(done["result"]["result"], "integrated", "{done}");
    let mut kinds: Vec<String> = Vec::new();
    let mut after = from;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !kinds.iter().any(|kind| kind == "integrated") && Instant::now() < deadline {
        let heard = bob.call_value("events", json!({ "after": after, "wait_ms": 2000 })).unwrap();
        after = heard["next"].as_u64().unwrap();
        kinds.extend(
            heard["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_owned()),
        );
    }
    for kind in ["saved", "integration_requested", "integrated"] {
        assert!(kinds.iter().any(|heard| heard == kind), "no {kind} in {kinds:?}");
    }

    // The other repository knows nothing of all this.
    let mut elsewhere = connect(&address, &world.ada, "site");
    assert_eq!(elsewhere.call_value("lanes", json!({})).unwrap(), json!([]));
    assert_eq!(elsewhere.call_value("events", json!({})).unwrap()["next"], 0);
    assert!(std::path::Path::new(
        elsewhere.call_value("status", json!({})).unwrap()["repo"]["root"].as_str().unwrap()
    )
    .ends_with("site"));
}

#[test]
fn the_same_protocol_over_a_command() {
    let world = World::new();
    // What SSH would run on the server, run here: the person is who the
    // command says.
    let through = |user: &str, repo: &str| Remote {
        server: None,
        command: [
            env!("CARGO_BIN_EXE_kito-server"),
            "--dir",
            world.dir.to_str().unwrap(),
            "stdio",
            "--as",
            user,
        ]
        .iter()
        .map(|part| (*part).to_owned())
        .collect(),
        repo: repo.to_owned(),
        token: None,
        ..Default::default()
    };
    std::env::set_var("KITOD_BIN", kitod());
    let mut bob = Client::remote(&through("bob", "shop"), true).unwrap();
    let lane = bob
        .call_value(
            "open_lane",
            json!({ "agent": "fixer", "intent": "a fix of the totals", "scope": ["src/shop.ts"] }),
        )
        .unwrap();
    assert_eq!(lane["owner"], "bob");
    // The same limits hold: his repositories only, and people the server knows.
    assert_eq!(
        refusal(Client::remote(&through("bob", "site"), true)),
        "there is no repository `site` for bob on this server"
    );
    match Client::remote(&through("nobody", "shop"), true) {
        Err(ClientError::Server(reason)) => assert!(reason.contains("closed the connection"), "{reason}"),
        other => panic!("nobody got in: {:?}", other.err().map(|e| e.to_string())),
    }
    let reply: Value = bob.call_value("server.whoami", json!({})).unwrap();
    assert_eq!(reply["user"], "bob");
}

#[test]
fn a_person_added_while_the_server_runs_is_known_at_once() {
    let mut world = World::new();
    let address = world.serve();
    assert!(refusal(Client::remote(&remote(&address, "kito_not_yet", "shop"), true))
        .contains("not one the server knows"));
    let carol = token_in(&ok(&world.dir, &["user", "add", "carol", "--repo", "site:integrate"]));
    let mut client = connect(&address, &carol, "site");
    let me: Value = client.call("server.whoami", json!({})).unwrap();
    assert_eq!(me["user"], "carol");
    // And one removed is gone for the connections that follow.
    ok(&world.dir, &["user", "remove", "carol"]);
    assert!(
        refusal(Client::remote(&remote(&address, &carol, "site"), true)).contains("not one the server knows")
    );
}

#[test]
#[cfg(unix)] // Unix permissions or links: not on Windows (ADR 0002)
fn connections_are_encrypted_and_the_server_is_known_by_its_fingerprint() {
    let mut world = World::new();
    // The server has a certificate of its own from the start, and says
    // which: with the token of every person it adds, and when it is asked.
    let print = ok(&world.dir, &["fingerprint"]).trim().to_owned();
    assert!(print.starts_with("sha256:") && print.len() == 71, "{print}");
    let added = ok(&world.dir, &["user", "add", "carol"]);
    assert!(added.contains(&format!("--fingerprint {print}")), "{added}");
    let key = world.dir.join("tls/key.pem");
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
    }

    let address = world.serve();
    assert!(world.said.ends_with(&format!("encrypted (certificate {print})")), "{}", world.said);

    // With the right fingerprint, however it is typed; not with another.
    let pinned =
        |print: &str| Remote { fingerprint: Some(print.to_owned()), ..remote(&address, &world.ada, "shop") };
    let mut ada = Client::remote(&pinned(&print), true).unwrap();
    assert_eq!(ada.call_value("server.whoami", json!({})).unwrap()["user"], "ada");
    let shouted = print.strip_prefix("sha256:").unwrap().to_uppercase();
    assert!(Client::remote(&pinned(&shouted), true).is_ok());
    let other = format!("sha256:{}", "ab".repeat(32));
    let error = Client::remote(&pinned(&other), true).err().unwrap().to_string();
    assert!(error.contains("not the one this directory was connected with"), "{error}");
    assert!(error.contains(&print), "it says what the server presents: {error}");

    // What travels is not readable: the token is not in what a listener
    // between the two would see. (A relay that copies the bytes it carries.)
    let relay = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let through = relay.local_addr().unwrap().to_string();
    let (to, seen) = (address.clone(), std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new())));
    let copy = seen.clone();
    std::thread::spawn(move || {
        let (mut client, _) = relay.accept().unwrap();
        let mut server = std::net::TcpStream::connect(to).unwrap();
        let (mut back, mut forth) = (server.try_clone().unwrap(), client.try_clone().unwrap());
        std::thread::spawn(move || std::io::copy(&mut back, &mut forth));
        let mut buffer = [0u8; 4096];
        while let Ok(read) = std::io::Read::read(&mut client, &mut buffer) {
            if read == 0 || server.write_all(&buffer[..read]).is_err() {
                break;
            }
            copy.lock().unwrap().extend_from_slice(&buffer[..read]);
        }
    });
    let mut ada = Client::remote(&Remote { server: Some(through), ..pinned(&print) }, true).unwrap();
    assert!(ada.call_value("status", json!({})).is_ok());
    let seen = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
    assert!(seen.len() > 200, "the relay saw the conversation");
    assert!(
        !seen.contains(&world.ada) && !seen.contains("hello") && !seen.contains("status"),
        "in the clear"
    );

    // A server on another machine is not trusted on its word: without a
    // fingerprint (or an authority) nothing is sent to it.
    let far = Remote { server: Some("192.0.2.7:7421".to_owned()), ..remote(&address, &world.ada, "shop") };
    let error = Client::remote(&far, true).err().unwrap().to_string();
    assert!(error.contains("nothing says it is the server you mean"), "{error}");
    let plain_far = Remote { plain: true, ..far };
    let error = Client::remote(&plain_far, true).err().unwrap().to_string();
    assert!(error.contains("a connection that is not encrypted is refused"), "{error}");
}

#[test]
fn a_server_may_be_served_in_the_clear_on_its_own_machine() {
    let mut world = World::new();
    // Not on an address other machines reach.
    let refused = server(&world.dir, &["serve", "--plain", "--listen", "0.0.0.0:0"]).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("listens on this machine only"));

    let address = world.serve_with(&["--plain"]);
    assert!(world.said.ends_with("NOT encrypted (--plain)"), "{}", world.said);
    let plain = Remote { plain: true, ..remote(&address, &world.ada, "shop") };
    let mut ada = Client::remote(&plain, true).unwrap();
    assert_eq!(ada.call_value("server.whoami", json!({})).unwrap()["user"], "ada");
    // A client that does not say hello gets nothing done.
    let mut raw = std::net::TcpStream::connect(&address).unwrap();
    raw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"lanes\",\"params\":{}}\n").unwrap();
    let mut answer = String::new();
    BufReader::new(raw.try_clone().unwrap()).read_line(&mut answer).unwrap();
    assert!(answer.contains("say hello first"), "{answer}");

    // A client that expects encryption is told what the matter is.
    let error = Client::remote(&remote(&address, &world.ada, "shop"), true).err().unwrap().to_string();
    assert!(error.contains("does not seem to encrypt") && error.contains("--plain"), "{error}");
}

#[test]
fn what_agents_are_told_is_the_server_s_and_its_kitchen_comes_first() {
    let mut world = World::new();
    // The kitchen is another repository of the server: what its main lane has.
    ok(&world.dir, &["repo", "set", "site", "--unprotect"]);
    let style = world.repo("site").join("instructions/all/style.md");
    fs::create_dir_all(style.parent().unwrap()).unwrap();
    fs::write(&style, "Two spaces, never tabs.\n").unwrap();
    fs::create_dir_all(world.repo("site").join("instructions/claude")).unwrap();
    fs::write(world.repo("site").join("instructions/claude/brief.md"), "Answer briefly.\n").unwrap();
    let set = ok(&world.dir, &["repo", "set", "shop", "--kitchen", "site"]);
    assert!(set.contains("its agents get the instructions of the kitchen site"), "{set}");
    assert!(
        server(&world.dir, &["repo", "set", "shop", "--kitchen", "nowhere"]).output().unwrap().status.code()
            != Some(0)
    );
    let carol = token_in(&ok(&world.dir, &["user", "add", "carol", "--repo", "shop:read"]));

    let address = world.serve();
    let mut ada = connect(&address, &world.ada, "shop");
    let mut bob = connect(&address, &world.bob, "shop");
    let mut carol = connect(&address, &carol, "shop");
    let told = ada.call_value("instructions", json!({})).unwrap();
    assert_eq!(told["kitchen"], "../site");
    let names = |told: &Value| -> Vec<(String, String)> {
        told["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (item["name"].as_str().unwrap().to_owned(), item["from"].as_str().unwrap().to_owned())
            })
            .collect()
    };
    assert_eq!(
        names(&told),
        [
            ("all/style.md".to_owned(), "kitchen".to_owned()),
            ("claude/brief.md".to_owned(), "kitchen".to_owned())
        ]
    );

    // The repository's own word wins, for everyone connected; one who may
    // only read is told, and changes nothing.
    ada.call_value("instructions_set", json!({ "name": "style", "text": "Tabs here." })).unwrap();
    ada.call_value("instructions_set", json!({ "name": "claude/brief", "text": "" })).unwrap();
    let told = bob.call_value("instructions", json!({})).unwrap();
    assert_eq!(names(&told), [("all/style.md".to_owned(), "repository".to_owned())]);
    assert_eq!(told["items"][0]["text"], "Tabs here.\n");
    assert_eq!(told["items"][0]["overrides"], true);
    assert_eq!(told["removed"], json!(["claude/brief.md"]));
    let told = carol.call_value("instructions", json!({})).unwrap();
    assert_eq!(told["items"][0]["text"], "Tabs here.\n");
    let refused = carol.call_value("instructions_set", json!({ "name": "style", "text": "Mine." }));
    assert!(refused.unwrap_err().to_string().contains("may read this repository"));
    // Kept by Kito, not in the project's files.
    assert!(world.repo("shop").join(".kito/instructions/all/style.md").is_file());
    assert!(!world.repo("shop").join("instructions").exists());
    let bad = bob.call_value("instructions_set", json!({ "name": "../../etc/x", "text": "no" }));
    assert!(bad.unwrap_err().to_string().contains("is not the name of an instruction"));
}

#[test]
fn a_person_has_a_role_in_each_repository_and_one_on_the_server() {
    let mut world = World::new();
    let dave = token_in(&ok(&world.dir, &["user", "add", "dave"]));
    let added = ok(&world.dir, &["user", "add", "erin", "--repo", "shop:maintain"]);
    let erin = token_in(&added);
    assert!(added.contains("kito connect <host>:<port>/shop --token"), "{added}");
    let wrong = server(&world.dir, &["user", "add", "frank", "--role", "write"]).output().unwrap();
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("`write` is a role in a repository"));
    let address = world.serve();

    // A member given nothing may use nothing.
    let none = refusal(Client::remote(&remote(&address, &dave, "shop"), true));
    assert!(none.contains("there is no repository `shop` for dave"), "{none}");

    // A maintainer says who uses the repository, and changes its rules.
    let mut maintainer = connect(&address, &erin, "shop");
    let members = maintainer.call_value("server.members", json!({})).unwrap();
    let roles: Vec<(String, String)> = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["user"].as_str().unwrap().to_owned(), m["role"].as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(
        roles,
        [
            ("ada".into(), "integrate".into()),
            ("bob".into(), "integrate".into()),
            ("erin".into(), "maintain".into())
        ]
    );
    let granted = maintainer.call_value("server.grant", json!({ "user": "dave", "role": "read" })).unwrap();
    assert_eq!(granted["changed"], true);
    let mut reader = connect(&address, &dave, "shop");
    let no = reader.call_value("server.grant", json!({ "user": "dave", "role": "maintain" })).unwrap_err();
    assert!(no.to_string().contains("only a maintainer of `shop`"), "{no}");
    let rules = maintainer.call_value("configure", json!({ "approvals": 1 })).unwrap();
    assert_eq!(rules["approvals"], 1);
    let mut integrator = connect(&address, &world.bob, "shop");
    let no = integrator.call_value("configure", json!({ "approvals": 0 })).unwrap_err();
    assert!(no
        .to_string()
        .contains("only a maintainer of this repository may `configure`: bob may integrate"));
    // Given in one repository, nothing in another.
    assert!(refusal(Client::remote(&remote(&address, &erin, "site"), true)).contains("no repository `site`"));
    maintainer.call_value("server.grant", json!({ "user": "dave" })).unwrap();
    assert!(refusal(Client::remote(&remote(&address, &dave, "shop"), true)).contains("no repository"));

    // The server's people are for its admins; admins and owners for its owners.
    let no = maintainer.call_value("server.people", json!({})).unwrap_err();
    assert!(no.to_string().contains("only an admin of this server"), "{no}");
    ok(&world.dir, &["user", "role", "erin", "admin"]);
    let mut admin = connect(&address, &erin, "site");
    let people = admin.call_value("server.people", json!({})).unwrap();
    assert_eq!(people.as_array().unwrap().len(), 4);
    let frank = admin.call_value("server.people_add", json!({ "user": "frank" })).unwrap();
    assert!(frank["token"].as_str().unwrap().starts_with("kito_"));
    let no = admin.call_value("server.people_add", json!({ "user": "gus", "role": "admin" })).unwrap_err();
    assert!(no.to_string().contains("only an owner"), "{no}");
    let no = admin.call_value("server.people_role", json!({ "user": "ada", "role": "owner" })).unwrap_err();
    assert!(no.to_string().contains("only an owner"), "{no}");
    admin.call_value("server.people_remove", json!({ "user": "frank" })).unwrap();
    ok(&world.dir, &["user", "role", "erin", "owner"]);
    let mut owner = connect(&address, &erin, "site");
    owner.call_value("server.people_role", json!({ "user": "ada", "role": "admin" })).unwrap();
    let list = ok(&world.dir, &["user", "list"]);
    assert!(
        list.contains(
            "ada <ada@example.com> (admin): every repository; given shop (integrate), site (integrate)"
        ),
        "{list}"
    );
    assert!(list.contains("dave (member): no repository"), "{list}");
}

#[test]
fn a_server_written_before_roles_per_repository_is_read_as_it_was_meant() {
    let world = World::new();
    let path = world.dir.join("server.toml");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("\n[users.old]\ntoken = \"x\"\nrepos = [\"*\"]\nrole = \"write\"\nroles = { site = \"read\" }\nadmin = true\n");
    text.push_str("\n[users.few]\ntoken = \"y\"\nrepos = [\"shop\"]\n");
    fs::write(&path, text).unwrap();
    let list = ok(&world.dir, &["user", "list"]);
    assert!(list.contains("old (admin): every repository; given shop (write), site (read)"), "{list}");
    assert!(list.contains("few (member): shop (integrate)"), "{list}");
    let kept = fs::read_to_string(&path).unwrap();
    assert!(!kept.contains("repos =") && kept.contains("server_role = \"admin\""), "{kept}");
}

#[test]
fn a_role_changed_counts_at_once_and_who_was_here_is_known() {
    let mut world = World::new();
    let address = world.serve();
    let mut bob = connect(&address, &world.bob, "shop");
    assert!(bob.call_value("status", json!({})).is_ok());
    // Read only from now on, in the connection that is open.
    ok(&world.dir, &["grant", "bob", "shop", "read"]);
    let no = bob
        .call_value("open_lane", json!({ "agent": "x", "intent": "y", "scope": ["README.md"] }))
        .unwrap_err();
    assert!(no.to_string().contains("may read this repository"), "{no}");
    // And nothing once it is taken away.
    ok(&world.dir, &["revoke", "bob", "shop"]);
    assert!(bob.call_value("status", json!({})).is_err());
    // Who was here, and when.
    let mut ada = connect(&address, &world.ada, "shop");
    let members = ada.call_value("server.members", json!({})).unwrap_err();
    assert!(members.to_string().contains("only"), "ada integrates, and may not list: {members}");
    ok(&world.dir, &["grant", "ada", "shop", "maintain"]);
    let members = ada.call_value("server.members", json!({})).unwrap();
    let ada_seen = members["members"].as_array().unwrap().iter().find(|m| m["user"] == "ada").unwrap()
        ["seen_ms"]
        .clone();
    assert!(ada_seen.as_u64().is_some_and(|ms| ms > 0), "{members}");
}
