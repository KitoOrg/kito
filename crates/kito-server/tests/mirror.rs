//! Lanes from any machine: the real server, the real daemons of connected
//! directories, and working copies that live somewhere else than the
//! repository.

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
        let base = tmp.path().canonicalize().unwrap();
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
        let bob = token_in(&ok(&dir, &["user", "add", "bob"]));
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
        };
        remote.save(&dir).unwrap();
        self.connected.push(dir.clone());
        dir
    }

    /// The working copy of a lane on the server.
    fn lane(&self, number: u64) -> PathBuf {
        self.dir.join("repos/shop/.kito/lanes").join(format!("{number:x}")).join("work")
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

fn mirror(dir: &Path, lane: &str) -> Value {
    let status = call(dir, "mirror_status", json!({}));
    let found = status["mirrors"].as_array().unwrap().iter().find(|mirror| mirror["lane"] == lane).cloned();
    found.unwrap_or_else(|| panic!("no copy of {lane} in {status}"))
}

/// Kills the daemon of a connected directory as a crash would.
fn kill(dir: &Path) {
    let pid = call(dir, "mirror_status", json!({}))["pid"].as_u64().unwrap();
    let status = Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap();
    assert!(status.success());
    // Killed, it stays as a process nobody waited for: it is a child of
    // this test, which started it.
    wait_for("the daemon to be gone", || {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        stat.rsplit(')').next().is_none_or(|rest| rest.trim_start().starts_with('Z') || rest.is_empty())
    });
}

#[test]
fn a_lane_opened_from_another_machine_is_edited_there() {
    let mut world = World::new();
    let ada = world.ada.clone();
    let laptop = world.connect("laptop", &ada);

    // The lane is on the server; its files are here.
    let work = open(&laptop, "alice");
    assert!(work.starts_with(&laptop), "{}", work.display());
    assert_eq!(read(&work, "src/shop.ts"), SHOP);
    assert_eq!(read(&work, "README.md"), "# Shop\n");
    let there = world.lane(1);
    assert_eq!(read(&there, "src/shop.ts"), SHOP);

    // A save here is in the lane soon after, with nobody asking.
    write(&work, "src/shop.ts", &SHOP.replace("price * quantity", "Math.round(price * 100) * quantity"));
    wait_for("the save on the server", || read(&there, "src/shop.ts").contains("Math.round"));

    // After the barrier everything saved so far is there: a new file in a
    // new directory, an executable, a removal. What is ignored stays here.
    write(&work, "src/tax/vat.ts", "export const VAT = 22;\n");
    write(&work, "bin/run", "#!/bin/sh\n");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(work.join("bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(&work, "node_modules/left-pad/index.js", "module.exports = 1;\n");
    write(&work, "debug.log", "noise\n");
    fs::remove_file(work.join("README.md")).unwrap();
    call(&laptop, "sync", json!({ "lane": "1" }));
    assert_eq!(read(&there, "src/tax/vat.ts"), "export const VAT = 22;\n");
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(there.join("bin/run")).unwrap().permissions().mode() & 0o111, 0o111);
    }
    assert!(!there.join("README.md").exists());
    assert!(!there.join("node_modules").exists() && !there.join("debug.log").exists());

    // It is recorded as the agent's, for the person, with the lane's intent.
    let log = call(&laptop, "log", json!({ "lane": "1", "limit": 20 })).to_string();
    assert!(log.contains("alice") && log.contains("work from here") && log.contains("vat.ts"), "{log}");
    let lanes = call(&laptop, "lanes", json!({}));
    assert_eq!(lanes[0]["owner"], "ada");
    assert_eq!(lanes[0]["workdir"], json!(work), "the working copy a client is told is the one here");
    let status = mirror(&laptop, "lane/1");
    assert_eq!((status["write"].as_bool(), status["pending"].as_u64()), (Some(true), Some(0)), "{status}");

    // The lane integrates as any other, and starts again from the main lane.
    let result = integrate(&laptop, "1");
    assert_eq!(result["result"], "integrated", "{result}");
    assert!(read(&world.main(), "src/shop.ts").contains("Math.round"));
    assert!(read(&world.main(), "src/tax/vat.ts").contains("VAT"));
    assert!(!world.main().join("README.md").exists());

    // Another lane lands on the main lane; with its next integration this
    // one has that too, here as on the server, and goes on from there.
    let bob = world.bob.clone();
    let desk = world.connect("desk", &bob);
    let other = open(&desk, "bruno");
    let now = read(&other, "src/shop.ts");
    write(&other, "src/shop.ts", &now.replace("/ 100;", "/ 100.0;"));
    assert_eq!(integrate(&desk, "2")["result"], "integrated");
    write(
        &work,
        "src/tax/vat.ts",
        "export const VAT = 23;
",
    );
    assert_eq!(integrate(&laptop, "1")["result"], "integrated");
    assert!(read(&work, "src/shop.ts").contains("/ 100.0;"), "the other lane's change is in this copy");
    let here = read(&work, "src/shop.ts");
    write(&work, "src/shop.ts", &here.replace("/ 100.0;", "/ 100.0; // percent"));
    call(&laptop, "sync", json!({ "lane": "1" }));
    assert!(read(&there, "src/shop.ts").contains("// percent"));
    assert_eq!(mirror(&laptop, "lane/1")["diverged"], json!([]));

    // Closed, its copy here is gone with it.
    call(&laptop, "close_lane", json!({ "lane": "1" }));
    assert!(!work.exists());
    assert_eq!(call(&laptop, "mirror_status", json!({}))["mirrors"], json!([]));
    call(&desk, "close_lane", json!({ "lane": "2" }));
}

#[test]
fn a_follower_has_each_save_within_300_ms() {
    let mut world = World::new();
    let (ada, bob) = (world.ada.clone(), world.bob.clone());
    let laptop = world.connect("laptop", &ada);
    let desk = world.connect("desk", &bob);
    let work = open(&laptop, "alice");

    // Bob follows ada's lane from his own machine.
    let following = call(&desk, "attach", json!({ "lane": "1" }));
    let view = PathBuf::from(following["workdir"].as_str().unwrap());
    assert!(view.starts_with(&desk));
    assert_eq!(read(&view, "src/shop.ts"), SHOP);
    assert_eq!(following["write"], false);

    // A hundred saves, each timed from the moment it is on ada's disk to
    // the moment it is on bob's.
    let mut times: Vec<Duration> = Vec::new();
    for round in 0..100 {
        let content = SHOP.replace("price * quantity", &format!("price * quantity + {round}"));
        let saved = Instant::now();
        write(&work, "src/shop.ts", &content);
        wait_for("the save at the follower", || read(&view, "src/shop.ts") == content);
        times.push(saved.elapsed());
    }
    times.sort();
    let (median, p95, worst) = (times[49], times[94], times[99]);
    eprintln!("save to follower's disk: median {median:?}, 95th percentile {p95:?}, worst {worst:?}");
    assert!(p95 < Duration::from_millis(300), "95th percentile {p95:?}");

    // New files and removals follow too.
    write(&work, "src/tax.ts", "export const VAT = 22;\n");
    fs::remove_file(work.join("README.md")).unwrap();
    wait_for("the new file at the follower", || view.join("src/tax.ts").is_file());
    wait_for("the removal at the follower", || !view.join("README.md").exists());

    // What bob changes in his view stays in his view: it is not sent, and
    // not overwritten either, until he says which he keeps.
    write(&view, "src/tax.ts", "export const VAT = 10;\n");
    write(&work, "src/tax.ts", "export const VAT = 23;\n");
    call(&laptop, "sync", json!({ "lane": "1" }));
    wait_for("bob's change to be noticed", || mirror(&desk, "lane/1")["held"] == json!(["src/tax.ts"]));
    assert_eq!(read(&view, "src/tax.ts"), "export const VAT = 10;\n");
    assert_eq!(read(&world.lane(1), "src/tax.ts"), "export const VAT = 23;\n");
    let said = refused(&desk, "take", json!({ "lane": "1", "path": "src/tax.ts", "mine": true }));
    assert!(said.contains("only follows"), "{said}");
    call(&desk, "take", json!({ "lane": "1", "path": "src/tax.ts", "mine": false }));
    assert_eq!(read(&view, "src/tax.ts"), "export const VAT = 23;\n");
    assert_eq!(mirror(&desk, "lane/1")["held"], json!([]));

    // A lane is written by its owner: bob may follow ada's, not write in it.
    let said = refused(&desk, "attach", json!({ "lane": "1", "write": true }));
    assert!(said.contains("ada's"), "{said}");

    // The main lane is followed like any other, and nobody writes in it
    // from elsewhere.
    let main = PathBuf::from(call(&desk, "attach", json!({ "lane": "main" }))["workdir"].as_str().unwrap());
    assert_eq!(read(&main, "src/shop.ts"), SHOP);
    assert!(refused(&desk, "attach", json!({ "lane": "main", "write": true })).contains("no writer"));
    assert_eq!(integrate(&laptop, "1")["result"], "integrated");
    wait_for("the integration in bob's copy of main", || read(&main, "src/shop.ts").contains("+ 99"));
    wait_for("the new file in bob's copy of main", || main.join("src/tax.ts").is_file());

    // Detached, the copy is gone and the lane is as it was.
    call(&desk, "detach", json!({ "lane": "1" }));
    assert!(!view.exists());
    assert!(world.lane(1).join("src/shop.ts").is_file());
}

#[test]
fn no_save_is_lost_when_the_daemon_is_killed() {
    let mut world = World::new();
    let ada = world.ada.clone();
    let laptop = world.connect("laptop", &ada);
    let work = open(&laptop, "alice");
    let there = world.lane(1);
    let files = ["src/shop.ts", "src/a.ts", "src/b.ts", "notes/todo.md"];

    // Saves keep coming while the daemon is killed at arbitrary moments:
    // before it saw a save, while it was sending one, with a journal half
    // sent. After each restart and barrier, the lane has what is on disk.
    for round in 0..12u32 {
        for (i, file) in files.iter().enumerate() {
            write(&work, file, &format!("// {file} in round {round}\nexport const v{i} = {round};\n"));
            if (round + i as u32).is_multiple_of(3) {
                std::thread::sleep(Duration::from_millis(u64::from(round % 4) * 7));
            }
        }
        if round.is_multiple_of(2) {
            std::thread::sleep(Duration::from_millis(u64::from(round) * 3));
        }
        kill(&laptop);
        // More saves with no daemon to see them, and one file removed.
        write(&work, "src/a.ts", &format!("// saved while nobody looked, round {round}\n"));
        if round == 5 {
            fs::remove_file(work.join("notes/todo.md")).unwrap();
        }
        call(&laptop, "sync", json!({ "lane": "1" }));
        for file in files {
            assert_eq!(read(&there, file), read(&work, file), "{file} after round {round}");
            assert_eq!(there.join(file).exists(), work.join(file).exists(), "{file} after round {round}");
        }
        let status = mirror(&laptop, "lane/1");
        assert_eq!(status["pending"], 0, "{status}");
        assert_eq!(status["diverged"], json!([]), "{status}");
    }
    let journal = laptop.join(".kito/lanes/1/journal");
    assert_eq!(fs::read_dir(journal).unwrap().count(), 0, "a journal with nothing left to send is empty");

    // A server that is away: saves wait in the journal, the barrier says
    // so, and they go when it is back.
    let address = world.address.clone();
    world.stop_serving();
    write(&work, "src/b.ts", "// saved while the server was away\n");
    let said = refused(&laptop, "sync", json!({ "lane": "1" }));
    assert!(said.contains("did not reach the server yet"), "{said}");
    assert_eq!(mirror(&laptop, "lane/1")["pending"], 1);
    kill(&laptop);
    write(&work, "src/shop.ts", "// and this one with neither\n");
    world.serve(&address);
    call(&laptop, "sync", json!({ "lane": "1" }));
    assert_eq!(read(&there, "src/b.ts"), "// saved while the server was away\n");
    assert_eq!(read(&there, "src/shop.ts"), "// and this one with neither\n");
    assert_eq!(mirror(&laptop, "lane/1")["pending"], 0);
}

#[test]
fn a_lane_that_moved_on_is_merged_or_settled_by_hand() {
    let mut world = World::new();
    let ada = world.ada.clone();
    let laptop = world.connect("laptop", &ada);
    let work = open(&laptop, "alice");
    let there = world.lane(1);

    // The lane changes on the server while this machine is not looking,
    // in one function; here the other function is changed.
    kill(&laptop);
    write(&there, "src/shop.ts", &SHOP.replace("/ 100;", "/ 100.0;"));
    let mine = SHOP.replace("price * quantity", "Math.round(price * 100) * quantity");
    write(&work, "src/shop.ts", &mine);
    call(&laptop, "sync", json!({ "lane": "1" }));
    let merged = read(&there, "src/shop.ts");
    assert!(merged.contains("Math.round") && merged.contains("/ 100.0;"), "{merged}");
    wait_for("the merge to come back", || read(&work, "src/shop.ts") == merged);

    // The same line changed on both sides does not merge: the barrier
    // says so, and nothing is integrated behind the agent's back.
    kill(&laptop);
    write(&there, "src/shop.ts", &merged.replace("/ 100.0;", "/ 50;"));
    write(&work, "src/shop.ts", &merged.replace("/ 100.0;", "/ 200;"));
    let said = refused(&laptop, "request_integration", json!({ "lane": "1" }));
    assert!(said.contains("do not merge") && said.contains("kito lane take lane/1 src/shop.ts"), "{said}");
    assert_eq!(mirror(&laptop, "lane/1")["diverged"], json!(["src/shop.ts"]));
    assert!(read(&work, "src/shop.ts").contains("/ 200;"), "what was saved here is still here");
    assert!(read(&there, "src/shop.ts").contains("/ 50;"));

    // Settled for this machine's: the lane has it.
    call(&laptop, "take", json!({ "lane": "1", "path": "src/shop.ts", "mine": true }));
    assert!(read(&there, "src/shop.ts").contains("/ 200;"));
    assert_eq!(mirror(&laptop, "lane/1")["diverged"], json!([]));

    // Again, settled for the server's this time.
    kill(&laptop);
    write(&there, "src/shop.ts", &merged.replace("/ 100.0;", "/ 10;"));
    write(&work, "src/shop.ts", &merged.replace("/ 100.0;", "/ 20;"));
    assert!(refused(&laptop, "sync", json!({ "lane": "1" })).contains("do not merge"));
    call(&laptop, "take", json!({ "lane": "1", "path": "src/shop.ts", "mine": false }));
    assert!(read(&work, "src/shop.ts").contains("/ 10;"));
    assert_eq!(integrate(&laptop, "1")["result"], "integrated");
    assert!(read(&world.main(), "src/shop.ts").contains("/ 10;"));
}

#[test]
fn one_machine_writes_in_a_lane_at_a_time() {
    let mut world = World::new();
    let ada = world.ada.clone();
    let laptop = world.connect("laptop", &ada);
    let tower = world.connect("tower", &ada);
    let work = open(&laptop, "alice");
    write(&work, "src/shop.ts", &SHOP.replace("price * quantity", "price * quantity * 100"));
    call(&laptop, "sync", json!({ "lane": "1" }));

    // The same person carries on from another machine: the lane's files
    // are there, and what is saved there goes to the lane.
    let taken = call(&tower, "attach", json!({ "lane": "1", "write": true }));
    let other = PathBuf::from(taken["workdir"].as_str().unwrap());
    assert!(read(&other, "src/shop.ts").contains("* 100"));
    write(&other, "src/tax.ts", "export const VAT = 22;\n");
    call(&tower, "sync", json!({ "lane": "1" }));
    assert_eq!(read(&world.lane(1), "src/tax.ts"), "export const VAT = 22;\n");

    // The first machine follows from now on: it gets the other's saves,
    // and what is saved on it waits, with the reason, until it takes the
    // lane back.
    wait_for("the other machine's save here", || work.join("src/tax.ts").is_file());
    write(&work, "README.md", "# Shop, from the laptop\n");
    let said = refused(&laptop, "sync", json!({ "lane": "1" }));
    assert!(said.contains("did not reach the server yet"), "{said}");
    let status = mirror(&laptop, "lane/1");
    assert_eq!(status["write"], false, "{status}");
    assert!(status["note"].as_str().unwrap().contains("is written from"), "{status}");
    assert_eq!(read(&world.lane(1), "README.md"), "# Shop\n");

    call(&laptop, "attach", json!({ "lane": "1", "write": true }));
    call(&laptop, "sync", json!({ "lane": "1" }));
    assert_eq!(read(&world.lane(1), "README.md"), "# Shop, from the laptop\n");
    wait_for("it at the other machine", || read(&other, "README.md").contains("from the laptop"));
    assert_eq!(mirror(&tower, "lane/1")["write"], true, "it writes until it tries and is told");

    // A copy with saves that never left is not removed by accident.
    kill(&tower);
    write(&other, "src/tax.ts", "export const VAT = 4;\n");
    let said = refused(&tower, "detach", json!({ "lane": "1" }));
    assert!(said.contains("--discard"), "{said}");
    call(&tower, "detach", json!({ "lane": "1", "discard": true }));
    assert!(!other.exists());
}
