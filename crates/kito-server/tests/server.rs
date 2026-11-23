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
        users.contains("ada <ada@example.com> [token] (member): shop (integrate), site (integrate)")
            && users.contains("bob [token] (member): shop (integrate)"),
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
    // What Claude Code reads, as the server has it; and who changed what.
    let claude = bob.call_value("instructions_text", json!({ "assistant": "claude" })).unwrap();
    let text = claude["text"].as_str().unwrap();
    assert!(
        text.contains("## Version control: Kito")
            && text.contains("Tabs here.")
            && !text.contains("Answer briefly.")
    );
    let history = carol.call_value("instructions_history", json!({})).unwrap();
    assert_eq!(history[0]["by"], "ada");
    assert_eq!(history[0]["name"], "claude/brief.md");
    assert_eq!(history[1]["after"], "Tabs here.\n");
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
            "ada <ada@example.com> [token] (admin): every repository; given shop (integrate), site (integrate)"
        ),
        "{list}"
    );
    assert!(list.contains("dave [token] (member): no repository"), "{list}");
}

#[test]
fn a_token_is_changed_by_its_person_and_by_whoever_may_change_them() {
    let mut world = World::new();
    let erin = token_in(&ok(&world.dir, &["user", "add", "erin", "--role", "admin"]));
    let rose = token_in(&ok(&world.dir, &["user", "add", "rose", "--role", "admin"]));
    let olga = token_in(&ok(&world.dir, &["user", "add", "olga", "--role", "owner"]));
    let address = world.serve();
    let unknown = "this token is not one the server knows";
    let said = |asked: Result<Value, ClientError>| asked.unwrap_err().to_string();
    let token_of = |answer: &Value| answer["token"].as_str().unwrap_or_else(|| panic!("{answer}")).to_owned();

    // Everybody was added with a token: the admins see who has one, never which.
    let mut admin = connect(&address, &erin, "shop");
    let people = admin.call_value("server.people", json!({})).unwrap();
    let all = people.as_array().unwrap();
    assert!(all.iter().all(|them| them["token"] == true && them["logins"] == json!([])), "{people}");
    assert!(!people.to_string().contains("kito_"), "{people}");

    // An admin gives a member a new token, said this once: the one bob
    // had stops at once, for the connection he has open and for the next.
    let mut open = connect(&address, &world.bob, "shop");
    assert!(open.call_value("status", json!({})).is_ok());
    let given = admin.call_value("server.people_token", json!({ "user": "bob" })).unwrap();
    let new = token_of(&given);
    assert_eq!(given, json!({ "user": "bob", "token": new, "logins": [] }));
    assert!(new.starts_with("kito_") && new.len() == 5 + 64 && new != world.bob, "{new}");
    assert!(open.call_value("status", json!({})).is_err());
    assert_eq!(refusal(Client::remote(&remote(&address, &world.bob, "shop"), true)), unknown);
    let mut bob = connect(&address, &new, "shop");
    let mut beside = connect(&address, &new, "shop");
    assert_eq!(bob.call_value("server.whoami", json!({})).unwrap()["user"], "bob");
    // What the server keeps is what it hashes to, as for the first one.
    let config = fs::read_to_string(world.dir.join("server.toml")).unwrap();
    assert!(!config.contains(&new), "{config}");

    // Bob gets another by himself, whatever his role: the connection he
    // asks through is told it and goes on, the one beside it does not.
    let own = bob.call_value("server.people_token", json!({})).unwrap();
    let newer = token_of(&own);
    assert!(own["user"] == "bob" && newer != new, "{own}");
    assert!(bob.call_value("status", json!({})).is_ok());
    assert!(beside.call_value("status", json!({})).is_err());
    assert_eq!(refusal(Client::remote(&remote(&address, &new, "shop"), true)), unknown);
    // Named, he is himself still.
    let newest = token_of(&bob.call_value("server.people_token", json!({ "user": "bob" })).unwrap());
    assert!(bob.call_value("status", json!({})).is_ok());
    assert_eq!(refusal(Client::remote(&remote(&address, &newer, "shop"), true)), unknown);

    // Never nothing: a token is all he signs in with.
    let no = said(bob.call_value("server.people_token", json!({ "remove": true })));
    assert!(no.contains("bob would have no way to sign in: link an identity to them first"), "{no}");
    let no = said(bob.call_value("server.people_unlink", json!({ "method": "hello" })));
    assert!(no.contains("bob has no identity of `hello`: there is nothing to unlink"), "{no}");
    // Naming a login proves nothing: he attaches none to himself by it.
    for params in [json!({ "identity": "hello:bob" }), json!({ "user": "bob", "identity": "hello:bob" })] {
        let no = said(bob.call_value("server.people_link", params));
        let by_name = "naming a login proves nothing: bob links an account of their own by signing in to it";
        assert!(no.contains(by_name) && no.contains("or an admin links it for them"), "{no}");
    }
    // Nobody else's is his to change, and he is told no more than that.
    for person in ["ada", "erin", "nobody"] {
        for method in ["server.people_token", "server.people_unlink", "server.people_link"] {
            let no = said(bob.call_value(method, json!({ "user": person, "remove": true })));
            assert!(
                no.contains("only an admin of this server may manage its people: bob is a member"),
                "{no}"
            );
        }
    }
    assert!(bob.call_value("status", json!({})).is_ok());

    // An admin does not change an admin or an owner, as for their role:
    // an owner does. What is her own, she changes.
    for (person, role) in [("rose", "admin"), ("olga", "owner")] {
        let no = said(admin.call_value("server.people_token", json!({ "user": person })));
        assert!(no.contains(&format!("{person} is an {role}: only an owner changes them")), "{no}");
    }
    assert!(Client::remote(&remote(&address, &rose, "shop"), true).is_ok());
    let mut owner = connect(&address, &olga, "shop");
    let given = owner.call_value("server.people_token", json!({ "user": "rose" })).unwrap();
    assert_eq!(refusal(Client::remote(&remote(&address, &rose, "shop"), true)), unknown);
    assert!(Client::remote(&remote(&address, &token_of(&given), "shop"), true).is_ok());
    let own = admin.call_value("server.people_token", json!({ "user": "erin" })).unwrap();
    assert_eq!(own["user"], "erin");
    assert!(admin.call_value("server.people", json!({})).is_ok());
    assert_eq!(refusal(Client::remote(&remote(&address, &erin, "shop"), true)), unknown);
    // What is not a name, or not a yes or a no, is a request that is wrong.
    let no = said(owner.call_value("server.people_token", json!({ "user": ["bob"] })));
    assert!(no.contains("`user` is the name of a person"), "{no}");
    let no = said(owner.call_value("server.people_token", json!({ "user": "bob", "remove": 1 })));
    assert!(no.contains("`remove` is true or false"), "{no}");
    assert!(bob.call_value("status", json!({})).is_ok());

    // On the server's machine: a new token printed once, and the one
    // before stops at once. A connection that SSH opened was not opened
    // with a token: it goes on.
    std::env::set_var("KITOD_BIN", kitod());
    let command =
        [env!("CARGO_BIN_EXE_kito-server"), "--dir", world.dir.to_str().unwrap(), "stdio", "--as", "bob"];
    let through_ssh = Remote {
        server: None,
        command: command.iter().map(|part| (*part).to_owned()).collect(),
        repo: "shop".to_owned(),
        token: None,
        ..Default::default()
    };
    let mut by_ssh = Client::remote(&through_ssh, true).unwrap();
    let printed = ok(&world.dir, &["user", "token", "bob"]);
    assert!(printed.contains("bob's new token, shown this once:"), "{printed}");
    assert!(printed.contains("The one they had stops working now"), "{printed}");
    let last = token_in(&printed);
    assert!(bob.call_value("status", json!({})).is_err());
    assert_eq!(refusal(Client::remote(&remote(&address, &newest, "shop"), true)), unknown);
    assert!(connect(&address, &last, "shop").call_value("status", json!({})).is_ok());
    assert_eq!(by_ssh.call_value("server.whoami", json!({})).unwrap()["user"], "bob");
    // The same rules there: never nothing, and a plugin says who a login is.
    let no = refused(&world.dir, &["user", "token", "bob", "--remove"]);
    assert!(no.contains("bob would have no way to sign in: link an identity to them first"), "{no}");
    let no = refused(&world.dir, &["user", "unlink", "bob", "hello"]);
    assert!(no.contains("bob has no identity of `hello`: there is nothing to unlink"), "{no}");
    let no = refused(&world.dir, &["user", "link", "bob", "hello:bob"]);
    assert!(no.contains("no way to sign in with `hello`: no plugin enabled here signs people in"), "{no}");
    assert!(refused(&world.dir, &["user", "token", "dan"]).contains("dan is not a user of this server"));
    assert!(ok(&world.dir, &["user", "list"]).contains("bob [token] (member): shop (integrate)"));
    assert!(connect(&address, &last, "shop").call_value("status", json!({})).is_ok());

    // Who changed whose token is kept, and no token is.
    let audit = ok(&world.dir, &["audit"]);
    let lines: Vec<String> =
        audit.lines().map(|line| line.split_whitespace().skip(1).collect::<Vec<_>>().join(" ")).collect();
    for line in [
        "erin token_changed bob",
        "bob token_changed bob",
        "olga token_changed rose",
        "erin token_changed erin",
        "the server's machine token_changed bob",
    ] {
        assert!(lines.iter().any(|kept| kept == line), "no `{line}` in {lines:#?}");
    }
    let kept: String = fs::read_dir(world.dir.join("audit"))
        .unwrap()
        .map(|file| fs::read_to_string(file.unwrap().path()).unwrap_or_default())
        .collect();
    assert!(kept.contains("server.people_token"), "the requests are kept: {kept}");
    for token in [&world.bob, &new, &newer, &newest, &last, &erin, &rose] {
        assert!(!kept.contains(token.as_str()), "a token in the audit log");
    }
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
    assert!(
        list.contains("old [token] (admin): every repository; given shop (write), site (read)"),
        "{list}"
    );
    assert!(list.contains("few [token] (member): shop (integrate)"), "{list}");
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

#[test]
fn a_maintainer_changes_the_rules_but_not_the_checks_and_every_change_is_kept() {
    let mut world = World::new();
    let erin = token_in(&ok(&world.dir, &["user", "add", "erin", "--repo", "shop:maintain"]));
    let root = token_in(&ok(&world.dir, &["user", "add", "rose", "--role", "admin"]));
    ok(&world.dir, &["repo", "set", "shop", "--approvals", "2"]);
    let address = world.serve();
    let mut maintainer = connect(&address, &erin, "shop");

    // The checks run commands on the server's machine: its admins say them.
    let no = maintainer.call_value("configure", json!({ "checks": ["rm -rf /"] })).unwrap_err();
    assert!(no.to_string().contains("only its admins change them"), "{no}");
    let mut admin = connect(&address, &root, "shop");
    let set = admin.call_value("configure", json!({ "checks": ["true"] })).unwrap();
    assert_eq!(set["checks"], json!(["true"]));

    // A kitchen is another repository of this server, and nothing else.
    for wrong in [json!("/etc"), json!("../shop"), json!("../nowhere"), json!("../../x")] {
        let no = maintainer.call_value("configure", json!({ "kitchen": wrong })).unwrap_err();
        assert!(no.to_string().contains("a kitchen is another repository of this server"), "{no}");
    }
    let set = maintainer.call_value("configure", json!({ "kitchen": "../site", "approvals": 1 })).unwrap();
    assert_eq!((set["kitchen"].as_str(), set["approvals"].as_u64()), (Some("../site"), Some(1)));
    maintainer.call_value("protect", json!({ "on": false })).unwrap();

    // Who changed what, the latest first; for maintainers and admins.
    let history = maintainer.call_value("server.settings_history", json!({})).unwrap();
    let seen: Vec<(String, String, Value)> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["by"].as_str().unwrap().to_owned(),
                c["setting"].as_str().unwrap().to_owned(),
                c["value"].clone(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("erin".to_owned(), "protected".to_owned(), json!(false)),
            ("erin".to_owned(), "kitchen".to_owned(), json!("../site")),
            ("erin".to_owned(), "approvals".to_owned(), json!(1)),
            ("rose".to_owned(), "checks".to_owned(), json!(["true"])),
            ("the server's machine".to_owned(), "approvals".to_owned(), json!(2)),
        ]
    );
    let mut bob = connect(&address, &world.bob, "shop");
    assert!(bob.call_value("server.settings_history", json!({})).is_err());
}

#[test]
fn tasks_are_the_server_s_and_their_comments_reach_the_lanes_on_them() {
    let mut world = World::new();
    let carol = token_in(&ok(&world.dir, &["user", "add", "carol", "--repo", "shop:read"]));
    let address = world.serve();
    let mut ada = connect(&address, &world.ada, "shop");
    let mut bob = connect(&address, &world.bob, "shop");
    let mut reader = connect(&address, &carol, "shop");

    let task = ada.call_value("task_add", json!({ "title": "Prices in cents", "for": "bob" })).unwrap();
    assert_eq!((task["id"].as_str(), task["by"].as_str()), (Some("task#1"), Some("ada")));
    assert_eq!(reader.call_value("tasks", json!({})).unwrap().as_array().unwrap().len(), 1);
    assert!(reader.call_value("task_add", json!({ "title": "Mine now" })).is_err());

    // An agent of bob takes it up, and another agent cannot.
    bob.call_value("task_claim", json!({ "task": "task#1", "agent": "dana" })).unwrap();
    let no = ada.call_value("task_claim", json!({ "task": "task#1", "agent": "erin" })).unwrap_err();
    assert!(no.to_string().contains("taken up by dana already"), "{no}");
    let lane = bob
        .call_value("open_lane", json!({ "agent": "dana", "intent": "prices in cents", "scope": ["src/shop.ts"], "task": "task#1" }))
        .unwrap();
    let lane = lane["id"].as_str().unwrap().to_owned();
    assert_eq!(reader.call_value("task", json!({ "task": "task#1" })).unwrap()["state"], "in_lane");

    // What ada says on the task reaches the agent in its lane, once.
    ada.call_value("task_comment", json!({ "task": "task#1", "text": "Round half to even." })).unwrap();
    let told = bob.call_value("notices", json!({ "lane": lane })).unwrap();
    let said: Vec<&Value> = told.as_array().unwrap().iter().filter(|n| n["kind"] == "task_comment").collect();
    assert_eq!(said.len(), 1, "{told}");
    assert!(said[0]["detail"]
        .as_str()
        .unwrap()
        .contains("ada said on task#1, which your lane is for: Round half to even."));
    let again = bob.call_value("notices", json!({ "lane": lane })).unwrap();
    assert!(!again.as_array().unwrap().iter().any(|n| n["kind"] == "task_comment"), "{again}");

    // The agent thinks it is done; a person says it is.
    let thinks = bob.call_value("task_done", json!({ "task": "task#1", "agent": "dana" })).unwrap();
    assert_eq!((thinks["thinks_done"].as_str(), thinks["state"].as_str()), (Some("dana"), Some("in_lane")));
    let done = ada.call_value("task_done", json!({ "task": "task#1" })).unwrap();
    assert_eq!(done["state"], "done");
    assert!(world.repo("shop").join(".kito/tasks.json").is_file());
}

#[test]
fn what_changes_the_server_is_kept_in_a_chain_and_the_server_is_backed_up_whole() {
    let mut world = World::new();
    let root = token_in(&ok(&world.dir, &["user", "add", "rose", "--role", "admin"]));
    ok(&world.dir, &["grant", "bob", "site", "read"]);

    // On the server's machine: what changed it, by whom, and the chain whole.
    let audit = ok(&world.dir, &["audit"]);
    assert!(audit.contains("role_changed") && audit.contains("bob may read in site"), "{audit}");
    assert!(audit.contains("user_added") && audit.contains("rose (admin)"), "{audit}");
    assert!(ok(&world.dir, &["audit", "--verify"]).contains("The audit log is whole"));

    // A backup of everything, encrypted with a key said once.
    let made = ok(&world.dir, &["backup"]);
    assert!(made.contains("kept here") && made.contains("shown this once"), "{made}");
    let key: String = made.lines().last().unwrap().trim().to_owned();
    assert_eq!(key.len(), 64, "{made}");
    assert!(!ok(&world.dir, &["backup"]).contains("shown this once"));
    let listed = ok(&world.dir, &["backup", "list"]);
    assert_eq!(listed.lines().count(), 2, "{listed}");
    let first =
        world.dir.join("backups").join(listed.lines().last().unwrap().split_whitespace().next().unwrap());

    // Put back elsewhere, with the key alone: the repositories and the people are there.
    let key_file = world.dir.join("..").join("the-key");
    fs::write(&key_file, format!("{key}\n")).unwrap();
    let to = world.dir.join("..").join("restored");
    let back = ok(
        &world.dir,
        &[
            "restore",
            first.to_str().unwrap(),
            "--to",
            to.to_str().unwrap(),
            "--key",
            key_file.to_str().unwrap(),
        ],
    );
    assert!(back.contains("files put back"), "{back}");
    assert!(to.join("server.toml").is_file() && to.join("repos/shop/.kito").is_dir());
    assert!(!to.join("backup.key").exists() && !to.join("backups").exists());
    let people = ok(&to, &["user", "list"]);
    assert!(people.contains("rose") && people.contains("bob [token] (member)"), "{people}");
    assert!(fs::read_to_string(to.join("repos/shop/src/shop.ts")).unwrap().contains("total"));
    // Its repositories open: the indexes left out are made again.
    let set = ok(&to, &["repo", "set", "shop", "--approvals", "1"]);
    assert!(set.contains("1 approval"), "{set}");
    if let Ok(mut daemon) = Client::connect(&to.join("repos/shop")) {
        let _ = daemon.call_value("shutdown", json!({}));
    }
    let again = server(&world.dir, &["restore", first.to_str().unwrap(), "--to", to.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&again.stderr).contains("not empty"));

    // Through the server: every request kept; the log and the backups for admins only.
    let address = world.serve();
    let mut bob = connect(&address, &world.bob, "shop");
    bob.call_value("status", json!({})).unwrap();
    assert!(bob.call_value("server.audit", json!({})).unwrap_err().to_string().contains("only an admin"));
    let mut admin = connect(&address, &root, "shop");
    let asked = admin.call_value("server.audit", json!({ "requests": true, "who": "bob" })).unwrap();
    assert!(
        asked.as_array().unwrap().iter().any(|entry| entry["what"] == "status" && entry["repo"] == "shop"),
        "{asked}"
    );
    assert_eq!(admin.call_value("server.audit_verify", json!({})).unwrap()["whole"], true);
    let now = admin.call_value("server.backup_now", json!({})).unwrap();
    assert!(now["name"].as_str().unwrap().starts_with("kito-backup-"));
    assert_eq!(
        admin.call_value("server.backups", json!({})).unwrap()["backups"].as_array().unwrap().len(),
        3
    );

    // A line changed by hand is found.
    let log = world.dir.join("audit/log.jsonl");
    let text = fs::read_to_string(&log).unwrap().replacen("bob may read in site", "bob may maintain site", 1);
    fs::write(&log, text).unwrap();
    let broken = server(&world.dir, &["audit", "--verify"]).output().unwrap();
    assert!(
        !broken.status.success()
            && String::from_utf8_lossy(&broken.stderr).contains("it was changed after it was written")
    );
}

/// Whether any file under `dir` says `text`.
fn says(dir: &Path, text: &str) -> bool {
    fs::read_dir(dir).into_iter().flatten().flatten().any(|entry| match entry.path().is_dir() {
        true => says(&entry.path(), text),
        false => fs::read(entry.path())
            .is_ok_and(|bytes| bytes.windows(text.len()).any(|seen| seen == text.as_bytes())),
    })
}

#[test]
fn admins_add_archive_rename_and_owners_delete_repositories() {
    let mut world = World::new();
    // A repository cloned and imported where it was made aside keeps
    // nothing of that place: not git, not Kito.
    assert!(!says(&world.repo("shop"), ".adding-"), "the repository names where it was made");
    let rose = token_in(&ok(&world.dir, &["user", "add", "rose", "--role", "admin"]));
    let olga = token_in(&ok(&world.dir, &["user", "add", "olga", "--role", "owner"]));
    let address = world.serve();
    let mut bob = connect(&address, &world.bob, "shop");
    let said = bob.call_value("server.repo_add", json!({ "name": "notes" })).unwrap_err().to_string();
    assert!(said.contains("only an admin"), "{said}");

    // Given nothing, it starts with a README.md; the admin who adds it
    // maintains it.
    let mut admin = connect(&address, &rose, "shop");
    assert_eq!(admin.call_value("server.repo_add", json!({ "name": "notes" })).unwrap()["name"], "notes");
    assert!(world.repo("notes").join(".kito").is_dir());
    assert_eq!(fs::read_to_string(world.repo("notes").join("README.md")).unwrap(), "# notes\n");
    let mut notes = connect(&address, &rose, "notes");
    notes.call_value("status", json!({})).unwrap();
    // Never a directory of the server's machine; a token only over https.
    let here = world.dir.join("..").join("shop-src");
    let said = admin
        .call_value("server.repo_add", json!({ "name": "copy", "from": here.to_str().unwrap() }))
        .unwrap_err()
        .to_string();
    assert!(said.contains("not a git address"), "{said}");
    let said = admin
        .call_value(
            "server.repo_add",
            json!({ "name": "copy", "from": "http://example.com/x.git", "token": "t0ken" }),
        )
        .unwrap_err()
        .to_string();
    assert!(said.contains("https://") && !said.contains("t0ken"), "{said}");

    // Sent from a machine: a git repository as a bundle of its history ...
    let bundle = world.dir.join("..").join("shop.bundle");
    git(&here, &["bundle", "create", bundle.to_str().unwrap(), "--all"]);
    let upload = |admin: &mut Client, bytes: &[u8], name: &str, kind: &str, key: Option<&[u8; 32]>| {
        let id = admin.call_value("server.repo_upload_begin", json!({})).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        for piece in bytes.chunks(1000) {
            admin
                .call_value(
                    "server.repo_upload_chunk",
                    json!({ "id": id, "data": kitod::rpc::encode(piece) }),
                )
                .unwrap();
        }
        let mut ended = json!({ "id": id, "name": name, "kind": kind });
        if let Some(key) = key {
            ended["key"] = json!(kitod::rpc::encode(key));
        }
        admin.call_value("server.repo_upload_end", ended)
    };
    upload(&mut admin, &fs::read(&bundle).unwrap(), "shop2", "git", None).unwrap();
    assert!(fs::read_to_string(world.repo("shop2").join("src/shop.ts")).unwrap().contains("total"));
    // ... and a Kito one whole, its history with it.
    let site = world.repo("site");
    let files = kitod::archive::collect(&site, &|path| {
        let text = path.to_string_lossy().replace('\\', "/");
        text.starts_with(".kito/index") || text.contains("sock") || text.ends_with("lock")
    });
    let key = kitod::archive::random_key().unwrap();
    let packed = world.dir.join("..").join("site.kitoarchive");
    kitod::archive::write(&site, &files, &packed, &key).unwrap();
    upload(&mut admin, &fs::read(&packed).unwrap(), "site2", "kito", Some(&key)).unwrap();
    let mut site2 = connect(&address, &rose, "site2");
    site2.call_value("status", json!({})).unwrap();
    let said = upload(&mut admin, b"not an archive", "site3", "kito", Some(&key)).unwrap_err().to_string();
    assert!(!world.repo("site3").exists(), "{said}");
    assert!(fs::read_dir(world.dir.join("uploads")).unwrap().next().is_none());
    // Under a name somebody is still making a repository with (its
    // directory there, not yet whole), an upload is refused, and takes
    // nothing of theirs away.
    fs::create_dir_all(world.repo("site4")).unwrap();
    fs::write(world.repo("site4").join("being-made"), "kept").unwrap();
    let said = upload(&mut admin, &fs::read(&packed).unwrap(), "site4", "kito", Some(&key)).unwrap_err();
    assert!(said.to_string().contains("already has something called `site4`"), "{said}");
    assert_eq!(fs::read_to_string(world.repo("site4").join("being-made")).unwrap(), "kept");
    fs::remove_dir_all(world.repo("site4")).unwrap();
    // What went wrong with what was sent is said without where the server is.
    let said = upload(&mut admin, b"not a bundle", "site5", "git", None).unwrap_err().to_string();
    assert!(said.contains("git clone") && !said.contains(world.dir.to_str().unwrap()), "{said}");
    assert!(!world.repo("site5").exists());

    // Renamed: the old name still reaches it.
    admin.call_value("server.repo_rename", json!({ "from": "shop2", "to": "shop3" })).unwrap();
    assert!(!world.repo("shop2").exists() && world.repo("shop3").is_dir());
    let mut old = connect(&address, &rose, "shop2");
    old.call_value("status", json!({})).unwrap();
    drop(old);

    // Archived: out of reach, kept 30 days; owners bring it back or delete it.
    admin.call_value("server.repo_archive", json!({ "name": "shop3" })).unwrap();
    refusal(Client::remote(&remote(&address, &rose, "shop3"), true));
    let all = admin.call_value("server.repos_all", json!({})).unwrap();
    assert!(
        all["archived"]
            .as_array()
            .unwrap()
            .iter()
            .any(|repo| repo["name"] == "shop3" && repo["by"] == "rose"),
        "{all}"
    );
    assert!(
        all["repos"].as_array().unwrap().iter().any(|repo| repo["name"] == "notes" && repo["members"] == 1),
        "{all}"
    );
    assert_eq!(all["renamed"]["shop2"], "shop3");
    let said = admin.call_value("server.repo_unarchive", json!({ "name": "shop3" })).unwrap_err().to_string();
    assert!(said.contains("only an owner"), "{said}");
    let mut owner = connect(&address, &olga, "shop");
    owner.call_value("server.repo_unarchive", json!({ "name": "shop3" })).unwrap();
    assert!(world.repo("shop3").is_dir());
    owner.call_value("server.repo_archive", json!({ "name": "shop3" })).unwrap();
    assert!(owner.call_value("server.repo_delete", json!({ "name": "shop3" })).is_err());
    owner.call_value("server.repo_delete", json!({ "name": "shop3", "confirm": "shop3" })).unwrap();
    assert!(!world.dir.join("archived/shop3").exists());
    let all = owner.call_value("server.repos_all", json!({})).unwrap();
    assert!(all["archived"].as_array().unwrap().is_empty() && all["renamed"].get("shop2").is_none(), "{all}");

    // On the server's machine too, and every change kept.
    ok(&world.dir, &["repo", "rename", "notes", "journal"]);
    ok(&world.dir, &["repo", "archive", "journal"]);
    assert!(refused(&world.dir, &["repo", "delete", "journal", "--confirm", "notes"])
        .contains("--confirm journal"));
    // Its name is its still: no other `journal` is added, on the server's
    // machine or from afar.
    let taken = "an archived repository is called `journal`: bring it back, delete it, or give this one \
                 another name";
    assert!(refused(&world.dir, &["repo", "add", "journal"]).contains(taken));
    let said = owner.call_value("server.repo_add", json!({ "name": "journal" })).unwrap_err().to_string();
    assert!(said.contains(taken), "{said}");
    assert!(!world.repo("journal").exists());
    // A server before 0.39.5 could have made two: which one is deleted is
    // said, and only that one goes.
    ok(&world.dir, &["repo", "add", "journal2"]);
    fs::rename(world.repo("journal2"), world.repo("journal")).unwrap();
    let both = refused(&world.dir, &["repo", "delete", "journal", "--confirm", "journal"]);
    assert!(both.contains("and an archived one of that name: say which"), "{both}");
    let said = ok(&world.dir, &["repo", "delete", "journal", "--confirm", "journal", "--archived"]);
    assert!(said.contains("The archived `journal` is deleted."), "{said}");
    assert!(!world.dir.join("archived/journal").exists());
    assert_eq!(fs::read_to_string(world.repo("journal").join("README.md")).unwrap(), "# journal2\n");
    let asked = json!({ "name": "journal", "confirm": "journal", "archived": true });
    let gone = owner.call_value("server.repo_delete", asked).unwrap_err().to_string();
    assert!(gone.contains("no archived repository is called `journal`"), "{gone}");
    assert!(world.repo("journal").is_dir());
    let audit = ok(&world.dir, &["audit"]);
    for kind in ["repo_added", "repo_renamed", "repo_archived", "repo_unarchived", "repo_deleted"] {
        assert!(audit.contains(kind), "{kind}: {audit}");
    }
    for name in ["site2", "archived/journal"] {
        if let Ok(mut daemon) =
            Client::connect(&world.dir.join(if name.contains('/') { "" } else { "repos" }).join(name))
        {
            let _ = daemon.call_value("shutdown", json!({}));
        }
    }
}

#[test]
fn a_repository_added_starts_with_something_a_lane_can_start_from() {
    let mut world = World::new();
    let rose = token_in(&ok(&world.dir, &["user", "add", "rose", "--role", "admin"]));
    let address = world.serve();
    let mut admin = connect(&address, &rose, "shop");
    let paths = |answer: &Value| -> Vec<String> {
        let files = answer["files"].as_array().unwrap().iter();
        let mut paths: Vec<String> = files.map(|file| file["path"].as_str().unwrap().to_owned()).collect();
        paths.sort();
        paths
    };

    // Given files, they are the first version of its main lane, and a lane
    // opens in it right away. `.git` is git's; `.gitignore` is a file, and
    // so is one that holds nothing.
    let recipe = "# Review\n\nRead the diff before the description.\n";
    let files = json!([
        { "path": "kitchen.toml", "text": "name = \"Shopworks\"\n" },
        { "path": "recipes/review.md", "text": recipe },
        { "path": ".gitignore", "text": "*.log\n" },
        { "path": ".github/CODEOWNERS", "text": "* @ada\n" },
        { "path": "drafts/.keep", "text": "" },
    ]);
    let added = admin.call_value("server.repo_add", json!({ "name": "kitchen", "files": files })).unwrap();
    assert_eq!(added, json!({ "name": "kitchen" }));
    assert_eq!(fs::read_to_string(world.repo("kitchen").join("recipes/review.md")).unwrap(), recipe);
    assert!(!world.repo("kitchen").join("README.md").exists());
    let mut kitchen = connect(&address, &rose, "kitchen");
    let on_main = kitchen.call_value("lane_files", json!({})).unwrap();
    assert_eq!(
        paths(&on_main),
        [".github/CODEOWNERS", ".gitignore", "drafts/.keep", "kitchen.toml", "recipes/review.md"]
    );
    let review = on_main["files"].as_array().unwrap().iter().find(|file| file["path"] == "recipes/review.md");
    let blob = review.unwrap()["blob"].as_str().unwrap().to_owned();
    let kept = kitchen.call_value("get_blobs", json!({ "blobs": [blob] })).unwrap();
    assert_eq!(kitod::rpc::decode(kept["blobs"][&blob].as_str().unwrap()).unwrap(), recipe.as_bytes());
    let log = kitchen.call_value("log", json!({})).unwrap();
    let first =
        log.as_array().unwrap().iter().find(|item| item["type"] == "checkpoint").expect("a checkpoint");
    assert_eq!((first["id"].as_str(), first["label"].as_str()), (Some("ckpt#1"), Some("first version")));
    // Not protected, as one added with nothing never was.
    let status = kitchen.call_value("status", json!({})).unwrap();
    assert_eq!(
        (status["repo"]["protected"].as_bool(), status["repo"]["ops_since_checkpoint"].as_u64()),
        (Some(false), Some(0)),
        "{status}"
    );
    let lane = kitchen
        .call_value(
            "open_lane",
            json!({ "agent": "cook", "intent": "a recipe for releases", "scope": ["recipes/"] }),
        )
        .unwrap();
    assert_eq!((lane["id"].as_str(), lane["owner"].as_str()), (Some("lane/1"), Some("rose")), "{lane}");
    assert_eq!(
        paths(&kitchen.call_value("lane_files", json!({ "lane": "lane/1" })).unwrap()),
        paths(&on_main)
    );

    // Given nothing, or a list of nothing: a README.md with its name, and
    // a lane just the same.
    let nothing = [json!({ "name": "notes" }), json!({ "name": "blank", "files": [], "from": null })];
    for params in nothing {
        let name = params["name"].as_str().unwrap().to_owned();
        admin.call_value("server.repo_add", params).unwrap();
        assert_eq!(fs::read_to_string(world.repo(&name).join("README.md")).unwrap(), format!("# {name}\n"));
        let mut there = connect(&address, &rose, &name);
        assert_eq!(paths(&there.call_value("lane_files", json!({})).unwrap()), ["README.md"]);
        let lane = there
            .call_value(
                "open_lane",
                json!({ "agent": "writer", "intent": "say what is kept here", "scope": ["README.md"] }),
            )
            .unwrap();
        assert_eq!(lane["id"], "lane/1", "{lane}");
    }

    // On the server's machine too.
    let said = ok(&world.dir, &["repo", "add", "journal"]);
    assert!(said.contains("It starts with a README.md"), "{said}");
    assert_eq!(fs::read_to_string(world.repo("journal").join("README.md")).unwrap(), "# journal\n");
    let listed = ok(&world.dir, &["repo", "list"]);
    for name in ["journal", "kitchen", "notes"] {
        assert!(listed.contains(&format!("{name}  [main open to direct edits, no checks]\n")), "{listed}");
    }
    ok(&world.dir, &["grant", "ada", "journal", "write"]);
    let mut ada = connect(&address, &world.ada, "journal");
    let lane = ada
        .call_value("open_lane", json!({ "agent": "writer", "intent": "the first entry", "scope": ["./"] }))
        .unwrap();
    assert_eq!(lane["owner"], "ada", "{lane}");

    // As many and as large as may be given: 32 files of 256 KiB.
    let most: Vec<Value> = (0..32)
        .map(|n| json!({ "path": format!("notes/{n}.md"), "text": format!("{n:02}").repeat(128 * 1024) }))
        .collect();
    admin.call_value("server.repo_add", json!({ "name": "edge", "files": most })).unwrap();
    assert_eq!(fs::metadata(world.repo("edge").join("notes/31.md")).unwrap().len(), 256 * 1024);
    let mut edge = connect(&address, &rose, "edge");
    assert_eq!(edge.call_value("status", json!({})).unwrap()["repo"]["files"], 32);

    // What cannot be given is said, with its path, and nothing is made.
    let not_added = |admin: &mut Client, files: Value| -> String {
        let asked = admin.call_value("server.repo_add", json!({ "name": "wrong", "files": files }));
        let said = asked.unwrap_err().to_string();
        assert!(!world.repo("wrong").exists(), "made all the same: {said}");
        said
    };
    let file = |path: &str| json!([{ "path": path, "text": "x\n" }]);
    for (path, why) in [
        ("", "it is empty"),
        ("/etc/motd", "it starts with `/`"),
        ("docs\\intro.md", "separated with `/`, not `\\`"),
        ("docs//intro.md", "it has an empty part"),
        ("docs/", "it has an empty part"),
        ("./intro.md", "it has `.` in it"),
        ("docs/../../intro.md", "it has `..` in it"),
        ("..", "it has `..` in it"),
        ("docs/a\nb.md", "a control character"),
        (".kito/config.toml", "`.kito` is Kito's own directory"),
        (".git/hooks/pre-commit", "`.git` is git's own directory"),
        (".GIT/config", "`.git` is git's own directory"),
        (".git", "`.git` is git's own directory"),
    ] {
        let said = not_added(&mut admin, file(path));
        let named = format!("{path:?} is not a path inside the repository: ");
        assert!(said.contains(&named) && said.contains(why), "{path:?}: {said}");
    }
    let many: Vec<Value> = (0..33).map(|n| json!({ "path": format!("notes/{n}.md"), "text": "" })).collect();
    let said = not_added(&mut admin, json!(many));
    assert!(said.contains("at most 32 files, and 33 were given"), "{said}");
    let large = json!([
        { "path": "README.md", "text": "# wrong\n" },
        { "path": "docs/large.md", "text": "x".repeat(256 * 1024 + 1) },
    ]);
    let said = not_added(&mut admin, large);
    assert!(
        said.contains("\"docs/large.md\" is too large to start a repository with: 262145 bytes"),
        "{said}"
    );
    let twice = json!([{ "path": "a.md", "text": "one\n" }, { "path": "a.md", "text": "two\n" }]);
    assert!(not_added(&mut admin, twice).contains("\"a.md\" is given twice"));
    let both = json!([{ "path": "docs", "text": "" }, { "path": "docs/intro.md", "text": "" }]);
    let said = not_added(&mut admin, both);
    assert!(said.contains("\"docs/intro.md\" is given under \"docs\", which is given as a file"), "{said}");
    // A file its own `.gitignore` leaves out would be on no lane.
    let left_out = json!([{ "path": ".gitignore", "text": "*.log\n" }, { "path": "debug.log", "text": "" }]);
    let said = not_added(&mut admin, left_out);
    assert!(said.contains("\"debug.log\" would not be in the first version"), "{said}");
    // What is not a list of paths and texts.
    assert!(not_added(&mut admin, json!("README.md")).contains("`files` is a list"));
    assert!(not_added(&mut admin, json!([{ "text": "x" }])).contains("file 1 of `files` has no `path`"));
    assert!(not_added(&mut admin, json!([{ "path": "a.md" }])).contains("\"a.md\" has no `text`"));

    // One way to start a repository at a time: files, a git address, or
    // what is sent from a machine.
    let from = json!({ "name": "wrong", "from": "https://example.com/x.git", "files": file("README.md") });
    let said = admin.call_value("server.repo_add", from).unwrap_err().to_string();
    assert!(said.contains("from a git address or from the files given, not both"), "{said}");
    let id = admin.call_value("server.repo_upload_begin", json!({})).unwrap()["id"].clone();
    let sent = json!({ "id": id, "name": "wrong", "kind": "git", "files": file("README.md") });
    let said = admin.call_value("server.repo_upload_end", sent).unwrap_err().to_string();
    assert!(said.contains("from what was sent or from the files given, not both"), "{said}");
    assert!(!world.repo("wrong").exists());

    // Who added what, and with how much, is kept.
    let audit = ok(&world.dir, &["audit"]);
    assert!(
        audit.contains("kitchen, with 5 files given") && audit.contains("edge, with 32 files given"),
        "{audit}"
    );
    for name in ["kitchen", "notes", "blank", "journal", "edge"] {
        if let Ok(mut daemon) = Client::connect(&world.repo(name)) {
            let _ = daemon.call_value("shutdown", json!({}));
        }
    }
}
