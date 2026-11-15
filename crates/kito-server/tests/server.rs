//! The real server, with the real daemons it starts for its repositories.

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
    let path = Path::new(env!("CARGO_BIN_EXE_kito-server")).with_file_name("kitod");
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
}

fn token_in(printed: &str) -> String {
    printed.lines().find(|line| line.starts_with("kito_")).expect(printed).trim().to_owned()
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let dir = base.join("server");
        assert!(ok(&base, &["init", dir.to_str().unwrap()]).contains("Kito server created"));
        let shop = source(&base, "shop-src", "src/shop.ts", SHOP);
        let site = source(&base, "site-src", "src/index.ts", "export const title = \"site\";\n");
        ok(&dir, &["repo", "add", "shop", "--from", shop.to_str().unwrap()]);
        ok(&dir, &["repo", "add", "site", "--from", site.to_str().unwrap()]);
        let ada = token_in(&ok(&dir, &["user", "add", "ada", "--email", "ada@example.com"]));
        let bob = token_in(&ok(&dir, &["user", "add", "bob", "--repo", "shop"]));
        World { _tmp: tmp, dir, ada, bob, serving: None }
    }

    /// Starts `serve` on a free port and returns its address.
    fn serve(&mut self) -> String {
        let mut child = server(&self.dir, &["serve", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        self.serving = Some(child);
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
fn people_repositories_and_tokens() {
    let world = World::new();
    assert_eq!(ok(&world.dir, &["repo", "list"]), "shop\nsite\n");
    let users = ok(&world.dir, &["user", "list"]);
    assert!(users.contains("ada <ada@example.com>: *") && users.contains("bob: shop"), "{users}");
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

    // A client that does not say hello gets nothing done.
    let mut raw = std::net::TcpStream::connect(&address).unwrap();
    raw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"lanes\",\"params\":{}}\n").unwrap();
    let mut answer = String::new();
    BufReader::new(raw.try_clone().unwrap()).read_line(&mut answer).unwrap();
    assert!(answer.contains("say hello first"), "{answer}");

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
    assert!(elsewhere.call_value("status", json!({})).unwrap()["repo"]["root"]
        .as_str()
        .unwrap()
        .ends_with("/site"));
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
    };
    std::env::set_var("KITOD_BIN", kitod());
    let mut bob = Client::remote(&through("bob", "shop"), true).unwrap();
    let lane = bob
        .call_value("open_lane", json!({ "agent": "fixer", "intent": "a fix", "scope": ["src/shop.ts"] }))
        .unwrap();
    assert_eq!(lane["owner"], "bob");
    // The same limits hold: his repositories only, and people the server knows.
    assert_eq!(
        refusal(Client::remote(&through("bob", "site"), true)),
        "there is no repository `site` for bob on this server"
    );
    match Client::remote(&through("nobody", "shop"), true) {
        Err(ClientError::Connect(reason)) => assert!(reason.contains("closed the connection"), "{reason}"),
        other => panic!("nobody got in: {:?}", other.err().map(|e| e.to_string())),
    }
    let reply: Value = bob.call_value("server.whoami", json!({})).unwrap();
    assert_eq!(reply["user"], "bob");
}
