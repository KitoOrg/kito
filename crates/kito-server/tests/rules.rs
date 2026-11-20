//! The rules of a team: who may do what in a repository, a main lane that
//! changes only through lanes, checks that run on the server, and the git
//! remote that gets every integration.

use kito_core::paths::Canonical as _;
use kitod::client::{Client, Remote};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The daemon binary sits next to the server's. If the tests run for this
/// package alone it may not have been built.
fn kitod() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let path = Path::new(env!("CARGO_BIN_EXE_kito-server")).with_file_name("kitod");
    BUILD.call_once(|| {
        let status =
            Command::new(env!("CARGO")).args(["build", "-p", "kitod", "--bin", "kitod"]).status().unwrap();
        assert!(status.success(), "building kitod");
        // The daemons of the connected directories are started from here.
        std::env::set_var("KITOD_BIN", &path);
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

const SHOP: &str = r#"export function total(price: number, quantity: number): number {
  return price * quantity;
}

export function discount(amount: number, percent: number): number {
  return amount - (amount * percent) / 100;
}
"#;

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).unwrap_or_default()
}

fn write(root: &Path, path: &str, content: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, content).unwrap();
}

/// Waits until `done` says so; fails after ten seconds.
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "still waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A server with one repository and two people, and the directories they
/// connect to it.
struct World {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    dir: PathBuf,
    ada: String,
    bob: String,
    address: String,
    serving: Option<Child>,
    connected: Vec<PathBuf>,
}

fn token_in(printed: &str) -> String {
    printed.lines().find(|line| line.starts_with("kito_")).expect(printed).trim().to_owned()
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonical().unwrap();
        let dir = base.join("server");
        ok(&base, &["init", dir.to_str().unwrap()]);
        let source = base.join("shop-src");
        write(&source, "src/shop.ts", SHOP);
        write(&source, "README.md", "# Shop\n");
        write(&source, ".gitignore", "node_modules/\n*.log\n");
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-q", "-m", "start"]);
        ok(&dir, &["repo", "add", "shop", "--from", source.to_str().unwrap()]);
        let ada = token_in(&ok(&dir, &["user", "add", "ada", "--email", "ada@example.com"]));
        let bob = token_in(&ok(&dir, &["user", "add", "bob", "--role", "write"]));
        let mut world = World {
            _tmp: tmp,
            base,
            dir,
            ada,
            bob,
            address: String::new(),
            serving: None,
            connected: Vec::new(),
        };
        world.serve("127.0.0.1:0");
        world
    }

    fn serve(&mut self, listen: &str) {
        let mut child = server(&self.dir, &["serve", "--listen", listen])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        self.serving = Some(child);
        let address = line.trim().strip_prefix("kito-server: listening on ").expect(&line);
        self.address = address.split(',').next().unwrap().to_owned();
    }

    /// The server goes away; what it kept stays.
    fn stop_serving(&mut self) {
        if let Some(mut child) = self.serving.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// A directory on "another machine", connected to the shop.
    fn connect(&mut self, name: &str, token: &str) -> PathBuf {
        let dir = self.base.join(name);
        fs::create_dir_all(&dir).unwrap();
        let remote = Remote {
            server: Some(self.address.clone()),
            command: Vec::new(),
            repo: "shop".to_owned(),
            token: Some(token.to_owned()),
            ..Default::default()
        };
        remote.save(&dir).unwrap();
        self.connected.push(dir.clone());
        dir
    }

    fn main(&self) -> PathBuf {
        self.dir.join("repos/shop")
    }
}

fn shutdown(root: &Path) {
    if let Ok(mut client) = Client::connect(root) {
        let _ = client.call_value("shutdown", json!({}));
        let socket = kitod::rpc::socket_path(root);
        for _ in 0..1000 {
            if !socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.connected.iter().for_each(|dir| shutdown(dir));
        self.stop_serving();
        shutdown(&self.main());
    }
}

/// The daemon of a connected directory, started if it is not running.
fn kito(dir: &Path) -> Client {
    kitod();
    Client::connect_or_start(dir).unwrap()
}

fn call(dir: &Path, method: &str, params: Value) -> Value {
    kito(dir).call_value(method, params).unwrap_or_else(|e| panic!("{method}: {e}"))
}

fn refused(dir: &Path, method: &str, params: Value) -> String {
    match kito(dir).call_value(method, params) {
        Ok(value) => panic!("{method} should have been refused: {value}"),
        Err(e) => e.to_string(),
    }
}

/// Opens a lane from a connected directory; returns its working copy there.
fn open(dir: &Path, agent: &str) -> PathBuf {
    let opened =
        call(dir, "open_lane", json!({ "agent": agent, "intent": "work from here", "scope": ["./"] }));
    PathBuf::from(opened["workdir"].as_str().expect("a working copy here"))
}

fn integrate(dir: &Path, lane: &str) -> Value {
    let asked = call(dir, "request_integration", json!({ "lane": lane }));
    let job = asked["job"].as_str().unwrap().to_owned();
    loop {
        let state = call(dir, "integration_result", json!({ "job": job, "wait_ms": 2000 }));
        if state["state"] == "done" {
            return state["result"].clone();
        }
    }
}

/// A person's own connected directory, by the name of who it is for.
fn desk(world: &mut World, who: &str, token: &str) -> PathBuf {
    world.connect(who, token)
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The events of the repository so far, as one text.
fn events(dir: &Path) -> String {
    call(dir, "events", json!({ "after": 0, "wait_ms": 0 })).to_string()
}

#[test]
fn who_may_read_who_may_write_and_who_may_integrate() {
    let mut world = World::new();
    let carol = token_in(&ok(&world.dir, &["user", "add", "carol", "--role", "read"]));
    let users = ok(&world.dir, &["user", "list"]);
    assert!(users.contains("ada <ada@example.com>: * (integrate)"), "{users}");
    assert!(users.contains("bob: * (write)") && users.contains("carol: * (read)"), "{users}");
    let (ada, bob) = (world.ada.clone(), world.bob.clone());
    let (ada, bob, carol) =
        (desk(&mut world, "ada", &ada), desk(&mut world, "bob", &bob), desk(&mut world, "carol", &carol));

    // Bob may write: he opens a lane and works in it.
    let work = open(&bob, "builder");
    write(&work, "src/shop.ts", &SHOP.replace("price * quantity", "Math.round(price * 100) * quantity"));

    // Carol may read: she sees the lane and follows it, and changes nothing.
    assert_eq!(call(&carol, "lanes", json!({}))[0]["owner"], "bob");
    call(&carol, "sync", json!({}));
    assert_eq!(call(&carol, "status", json!({}))["repo"]["protected"], true);
    let said = refused(&carol, "open_lane", json!({ "agent": "x", "intent": "y", "scope": ["./"] }));
    assert!(said.contains("carol may read this repository"), "{said}");
    let view = PathBuf::from(call(&carol, "attach", json!({ "lane": "1" }))["workdir"].as_str().unwrap());
    wait_for("bob's save in carol's copy", || read(&view, "src/shop.ts").contains("Math.round"));
    assert!(refused(&carol, "attach", json!({ "lane": "1", "write": true })).contains("may read"));
    assert!(refused(&carol, "close_lane", json!({ "lane": "1" })).contains("may read"));

    // Bob asks for his lane to be integrated. It is merged and checked,
    // and then it waits: bringing it in is not for him.
    let result = integrate(&bob, "1");
    assert_eq!(result["result"], "awaiting_approval", "{result}");
    assert!(result["message"].as_str().unwrap().contains("waits for somebody who may"), "{result}");
    assert_eq!(read(&world.main(), "src/shop.ts"), SHOP, "nothing reached the main lane");
    let lane = &call(&ada, "lanes", json!({}))[0];
    assert_eq!(
        (lane["state"].as_str(), lane["proposed_by"].as_str()),
        (Some("awaiting_approval"), Some("bob"))
    );
    assert_eq!(call(&ada, "board", json!({}))[0]["proposed_by"], "bob");
    assert!(events(&ada).contains("awaiting_approval"));

    // A lane that changes after it was checked has to ask again.
    write(
        &work,
        "src/shop.ts",
        &SHOP.replace("price * quantity", "Math.round(price * 100) * quantity // cents"),
    );
    call(&bob, "sync", json!({ "lane": "1" }));
    assert_eq!(call(&ada, "lanes", json!({}))[0]["state"], "active");
    assert_eq!(integrate(&bob, "1")["result"], "awaiting_approval");

    // A lane is its owner's: another writer does not close it or speak
    // for it. Somebody who may integrate decides for any lane.
    let dave = token_in(&ok(&world.dir, &["user", "add", "dave", "--role", "write"]));
    let dave = desk(&mut world, "dave", &dave);
    for (method, params) in [
        ("close_lane", json!({ "lane": "1" })),
        ("extend_scope", json!({ "lane": "1", "scope": ["README.md"] })),
        ("set_intent", json!({ "lane": "1", "summary": "mine now" })),
        ("request_integration", json!({ "lane": "1" })),
    ] {
        let said = refused(&dave, method, params);
        assert!(said.contains("lane/1 is bob's"), "{method}: {said}");
    }

    // Ada may integrate: she brings bob's lane in.
    let result = integrate(&ada, "1");
    assert_eq!(result["result"], "integrated", "{result}");
    assert!(read(&world.main(), "src/shop.ts").contains("// cents"));
    let lane = &call(&bob, "lanes", json!({}))[0];
    assert_eq!((lane["state"].as_str(), lane["proposed_by"].as_str()), (Some("active"), None));
    assert_eq!(lane["ops"], 0, "bob's lane starts again from the main lane");
    // The commit is the agent's, as for any integration.
    let log = git_out(&world.main(), &["log", "-1", "--format=%an"]);
    assert_eq!(log.trim(), "builder");

    // Why the function is the way it is, asked by somebody who may only
    // read: the reason bob's agent gave, for whom it worked, the checkpoint,
    // and who let it in.
    let why = call(&carol, "why_graph", json!({ "target": "src/shop.ts:total" }));
    let reason = &why["reasons"][0];
    assert_eq!(reason["why"], "work from here", "{why}");
    assert_eq!((reason["by"].as_str(), reason["for"].as_str()), (Some("builder"), Some("bob")));
    assert_eq!(
        (reason["checkpoint"].as_str(), reason["approved_by"].as_str()),
        (Some("ckpt#2"), Some("ada"))
    );
    let found = call(&carol, "reasons", json!({ "about": "bob" }));
    assert_eq!(found[0]["changed"], json!(["src/shop.ts:total"]));
    // From his own machine bob asks about his lane as it is, a save he
    // just made included.
    write(&work, "src/shop.ts", &read(&work, "src/shop.ts").replace("// cents", "// in cents"));
    let mine = call(&bob, "why_graph", json!({ "target": "src/shop.ts:total", "lane": "1" }));
    assert_eq!(mine["reasons"][0]["checkpoint"], Value::Null, "{mine}");
    assert_eq!(mine["reasons"].as_array().unwrap().len(), why["reasons"].as_array().unwrap().len() + 1);
    write(&work, "src/shop.ts", &read(&work, "src/shop.ts").replace("// in cents", "// cents"));

    // What a person may do can change, for one repository too: it holds
    // for the connections made from then on.
    ok(&world.dir, &["user", "role", "bob", "integrate", "--repo", "shop"]);
    assert!(ok(&world.dir, &["user", "list"]).contains("bob: * (write), shop (integrate)"));
    call(&bob, "shutdown", json!({}));
    wait_for("bob's daemon to stop", || Client::connect(&bob).is_err());
    write(&work, "README.md", "# Shop\n\nPrices are in cents.\n");
    assert_eq!(integrate(&bob, "1")["result"], "integrated");
    assert!(read(&world.main(), "README.md").contains("in cents"));
}

#[test]
fn the_checks_run_on_the_server_and_a_failing_one_stops_a_lane() {
    let mut world = World::new();
    // No client can change them: they are set where the repository is.
    let said = ok(
        &world.dir,
        &["repo", "set", "shop", "--check", "! grep -rq FIXME src", "--check", "test -f README.md"],
    );
    assert_eq!(said, "shop  [main protected, checks: ! grep -rq FIXME src; test -f README.md]\n");
    assert_eq!(ok(&world.dir, &["repo", "list"]), said);
    let (ada, bob) = (world.ada.clone(), world.bob.clone());
    let (ada, bob) = (desk(&mut world, "ada", &ada), desk(&mut world, "bob", &bob));
    let said = refused(&ada, "configure", json!({ "checks": [] }));
    assert!(said.contains("not something a client can ask"), "{said}");

    // A lane that fails a check goes nowhere, whoever asks: not into the
    // main lane, and not into the queue of those that wait for approval.
    let work = open(&bob, "builder");
    write(&work, "src/shop.ts", &SHOP.replace("price * quantity", "price * quantity // FIXME"));
    let result = integrate(&bob, "1");
    assert_eq!(result["result"], "verification_failed", "{result}");
    assert_eq!(result["verification"]["failed_command"], "! grep -rq FIXME src");
    assert_eq!(call(&ada, "lanes", json!({}))[0]["proposed_by"], Value::Null);
    assert_eq!(integrate(&ada, "1")["result"], "verification_failed");
    assert_eq!(read(&world.main(), "src/shop.ts"), SHOP);

    // Fixed, it passes, waits, and is brought in with the checks run again.
    write(&work, "src/shop.ts", &SHOP.replace("price * quantity", "price * quantity // in euros"));
    let result = integrate(&bob, "1");
    assert_eq!(result["result"], "awaiting_approval", "{result}");
    assert_eq!(result["verification"]["passed"], true);
    let result = integrate(&ada, "1");
    assert_eq!(result["result"], "integrated");
    assert_eq!(result["verification"]["commands"], json!(["! grep -rq FIXME src", "test -f README.md"]));
    assert!(read(&world.main(), "src/shop.ts").contains("// in euros"));

    // With no checks a lane is integrated as soon as it merges.
    assert!(ok(&world.dir, &["repo", "set", "shop", "--no-checks"]).contains("no checks"));
    write(&work, "src/shop.ts", &SHOP.replace("price * quantity", "price * quantity // FIXME"));
    assert_eq!(integrate(&bob, "1")["result"], "awaiting_approval");
}

#[test]
fn the_main_lane_on_the_server_changes_only_through_lanes() {
    let mut world = World::new();
    let ada = world.ada.clone();
    let ada = desk(&mut world, "ada", &ada);
    let main = world.main();
    assert_eq!(call(&ada, "status", json!({}))["repo"]["protected"], true);

    // Somebody edits the main directory on the server: the file is put
    // back, the edit is kept aside, and whoever watches is told.
    write(&main, "src/shop.ts", "// edited on the server, behind everybody's back\n");
    write(&main, "src/stray.ts", "export const stray = 1;\n");
    wait_for("the edit to be put back", || {
        read(&main, "src/shop.ts") == SHOP && !main.join("src/stray.ts").exists()
    });
    wait_for("the event", || events(&ada).contains("main_edit_refused"));
    let status = call(&ada, "status", json!({}));
    let last = &status["repo"]["last_refused"];
    let mut paths: Vec<&str> = last["paths"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    paths.sort();
    assert!(paths.contains(&"src/shop.ts") || paths.contains(&"src/stray.ts"), "{status}");
    let kept = PathBuf::from(last["kept"].as_str().unwrap());
    assert!(kept.starts_with(main.join(".kito/refused")), "{}", kept.display());
    assert_eq!(git_out(&main, &["log", "--oneline"]).lines().count(), 1, "and git has nothing new");

    // A lane still gets in, and is the only thing that does.
    let work = open(&ada, "builder");
    write(&work, "src/shop.ts", &SHOP.replace("price * quantity", "price * quantity * 100"));
    assert_eq!(integrate(&ada, "1")["result"], "integrated");
    assert!(read(&main, "src/shop.ts").contains("* 100"));
    assert_eq!(git_out(&main, &["log", "--oneline"]).lines().count(), 2);

    // Whoever runs the server can open the main lane again, and close it.
    assert!(ok(&world.dir, &["repo", "set", "shop", "--unprotect"]).contains("main open to direct edits"));
    write(&main, "README.md", "# Shop\n\nEdited on the server, on purpose.\n");
    wait_for("the edit to be recorded", || {
        call(&ada, "log", json!({ "limit": 5 })).to_string().contains("README.md")
    });
    assert!(read(&main, "README.md").contains("on purpose"));
    assert!(ok(&world.dir, &["repo", "set", "shop", "--protect"]).contains("main protected"));
    assert!(read(&main, "README.md").contains("on purpose"), "what was there when it was protected stays");
    assert!(events(&ada).contains("the main lane is protected"));
}

#[test]
fn every_integration_is_pushed_to_the_git_remote() {
    let mut world = World::new();
    let remote = world.base.join("upstream.git");
    git(&world.base, &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    let said = ok(&world.dir, &["repo", "set", "shop", "--push", remote.to_str().unwrap()]);
    assert!(said.contains(&format!("pushes to {}", remote.display())), "{said}");
    let ada = world.ada.clone();
    let ada = desk(&mut world, "ada", &ada);
    // What the repository already had goes first.
    wait_for("the history at the remote", || {
        git_out(&remote, &["log", "--oneline", "main"]).lines().count() == 1
    });

    let work = open(&ada, "builder");
    write(&work, "src/shop.ts", &SHOP.replace("price * quantity", "price * quantity * 100"));
    let result = integrate(&ada, "1");
    assert_eq!(result["result"], "integrated");
    let commit = result["git_commit"].as_str().unwrap().to_owned();
    wait_for("the integration at the remote", || git_out(&remote, &["rev-parse", "main"]).trim() == commit);
    assert!(git_out(&remote, &["show", "main:src/shop.ts"]).contains("* 100"));
    wait_for("the event", || events(&ada).contains("\"pushed\""));
    let status = call(&ada, "status", json!({}));
    assert_eq!(status["repo"]["pushed"], result["checkpoint"]);
    assert_eq!(status["repo"]["push_error"], Value::Null);

    // Somebody pushes to the remote with plain git. Nothing of theirs is
    // overwritten, and nothing stops: before the next push their commit
    // is brought in through a lane, merged, and the merge is pushed.
    let other = world.base.join("other-clone");
    git(&world.base, &["clone", "-q", remote.to_str().unwrap(), other.to_str().unwrap()]);
    write(&other, "NOTES.md", "pushed to git directly\n");
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-q", "-m", "straight to git"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let theirs = git_out(&remote, &["rev-parse", "main"]).trim().to_owned();
    write(&work, "README.md", "# Shop\n\nPrices are in cents.\n");
    assert_eq!(integrate(&ada, "1")["result"], "integrated");
    wait_for("their commit on the main lane", || {
        read(&world.main(), "NOTES.md") == "pushed to git directly\n"
    });
    wait_for("the merge at the remote", || {
        let parents = git_out(&remote, &["rev-list", "--parents", "-n", "1", "main"]);
        let parents: Vec<&str> = parents.split_whitespace().collect();
        parents.len() == 3 && parents[2] == theirs
    });
    assert!(git_out(&remote, &["show", "main:README.md"]).contains("in cents"));
    assert_eq!(git_out(&remote, &["show", "main:NOTES.md"]), "pushed to git directly\n");
    wait_for("the event", || events(&ada).contains("\"pulled\""));
    let status = call(&ada, "status", json!({}));
    assert_eq!(status["repo"]["push_error"], Value::Null, "{status}");
    assert_eq!(status["repo"]["pull_waiting"], Value::Null);
    // Why their file is there: their commit says, under their name.
    let why = call(&ada, "why_graph", json!({ "target": "NOTES.md" }));
    assert_eq!(why["reasons"][0]["why"], "straight to git");
    assert_eq!(why["reasons"][0]["by"], "Ada");

    // A commit that conflicts with what a lane did waits in its lane, and
    // nothing is pushed over it until somebody decides.
    git(&other, &["pull", "-q", "origin", "main"]);
    let text = read(&other, "src/shop.ts");
    write(&other, "src/shop.ts", &text.replace("price * quantity * 100", "price * quantity * 1000"));
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-q", "-m", "thousandths"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let theirs = git_out(&remote, &["rev-parse", "main"]).trim().to_owned();
    let text = read(&work, "src/shop.ts");
    write(
        &work,
        "src/shop.ts",
        &text.replace("price * quantity * 100", "Math.round(price * quantity * 100)"),
    );
    assert_eq!(integrate(&ada, "1")["result"], "integrated");
    wait_for("the commits to wait", || call(&ada, "status", json!({}))["repo"]["pull_waiting"].is_string());
    let status = call(&ada, "status", json!({}));
    let waiting = status["repo"]["pull_waiting"].as_str().unwrap().to_owned();
    wait_for("it to be said", || {
        let error =
            call(&ada, "status", json!({}))["repo"]["push_error"].as_str().unwrap_or_default().to_owned();
        error.contains("wait to be merged")
    });
    assert_eq!(git_out(&remote, &["rev-parse", "main"]).trim(), theirs, "nothing was forced");
    // Ada decides for what the lane did; whoever runs the server asks again.
    let conflicts = call(&ada, "conflicts", json!({}));
    let conflict = conflicts[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(conflicts[0]["lane"], waiting.as_str(), "{conflicts}");
    call(&ada, "resolve_conflict", json!({ "conflict": conflict, "choice": "main" }));
    let pulled = call(&world.main(), "pull", json!({}));
    // Nothing of the commit was kept: the histories are joined all the
    // same, so that the remote takes the pushes again.
    assert!(pulled["message"].as_str().unwrap().contains("the two histories are joined"), "{pulled}");
    assert_eq!(pulled["lane"], Value::Null);
    wait_for("the merge at the remote", || {
        let parents = git_out(&remote, &["rev-list", "--parents", "-n", "1", "main"]);
        parents.split_whitespace().nth(2) == Some(theirs.as_str())
    });
    assert!(git_out(&remote, &["show", "main:src/shop.ts"]).contains("Math.round(price * quantity * 100)"));
    assert_eq!(call(&ada, "status", json!({}))["repo"]["pull_waiting"], Value::Null);
    // A client of the server does not start a pull itself.
    assert!(refused(&ada, "pull", json!({})).contains("not something a client can ask"));

    // Pushing can be stopped.
    assert!(!ok(&world.dir, &["repo", "set", "shop", "--no-push"]).contains("pushes to"));
}
