//! Lanes through the real daemon: agents save in their working copies, ask
//! for integration over the socket and read the result.

use kito_integrate::{ConflictInfo, IntegrationResult, JobState, Outcome};
use kitod::client::Client;
use kitod::lanes::LaneInfo;
use kitod::rename::RenameResult;
use kitod::repo::LogItem;
use kitod::semantic::Why;
use kitod::Repo;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn start(root: &Path) -> Client {
    static BIN: std::sync::Once = std::sync::Once::new();
    BIN.call_once(|| std::env::set_var("KITOD_BIN", env!("CARGO_BIN_EXE_kitod")));
    Client::connect_or_start(root).expect("the daemon starts")
}

fn stop(root: &Path, client: &mut Client) {
    let _: Value = client.call("shutdown", json!({})).unwrap();
    let socket = kitod::rpc::socket_path(&root.canonicalize().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    while socket.exists() {
        assert!(Instant::now() < deadline, "the daemon did not stop");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Asks for the integration of a lane and waits for the result.
fn integrate(client: &mut Client, lane: &str) -> IntegrationResult {
    let asked: Value = client.call("request_integration", json!({ "lane": lane })).unwrap();
    let job = asked["job"].as_str().unwrap().to_owned();
    let state: JobState =
        client.call("integration_result", json!({ "job": job, "wait_ms": 20_000 })).unwrap();
    match state {
        JobState::Done { result } => *result,
        other => panic!("{job} is still {other:?}"),
    }
}

fn open(client: &mut Client, agent: &str, task: &str) -> (String, PathBuf) {
    let lane: LaneInfo = client.call("open_lane", json!({ "agent": agent, "task": task })).unwrap();
    (lane.id.clone(), lane.workdir.unwrap())
}

const SHOP: &str = "export function total(items: Item[]): number {\n  let sum = 0;\n  for (const item of items) {\n    sum += item.price * item.quantity;\n  }\n  return sum;\n}\n\nexport function discount(amount: number, percent: number): number {\n  const cut = Math.round((amount * percent) / 100);\n  return amount - cut;\n}\n";

fn new_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    Repo::init(root).unwrap().close().unwrap();
    write(root, ".gitignore", ".kito/\n");
    write(root, "src/shop.ts", SHOP);
    write(
        root,
        ".kito/config.toml",
        "[author]\nname = \"ada\"\n[verify]\ncommands = [\"! grep -rq FIXME src\"]\n",
    );
    tmp
}

#[test]
fn agents_work_in_their_lanes_and_integrate_through_the_daemon() {
    let tmp = new_repo();
    let root = tmp.path().canonicalize().unwrap();
    let mut client = start(&root);
    let _: Value = client.call("sync", json!({})).unwrap();

    let (alice, alice_dir) = open(&mut client, "alice", "floor the discount");
    let (bob, bob_dir) = open(&mut client, "bob", "receipts");
    assert_eq!((alice.as_str(), bob.as_str()), ("lane/1", "lane/2"));
    let lanes: Vec<LaneInfo> = client.call("lanes", json!({})).unwrap();
    assert_eq!(
        lanes.iter().map(|l| (l.id.as_str(), l.agent.as_str())).collect::<Vec<_>>(),
        [("lane/1", "alice"), ("lane/2", "bob")]
    );

    // The agents just save: the watcher records each save on its lane.
    let _: Value =
        client.call("set_intent", json!({ "lane": bob, "summary": "add a receipt function" })).unwrap();
    write(&alice_dir, "src/shop.ts", &SHOP.replace("Math.round(", "Math.floor("));
    write(
        &bob_dir,
        "src/shop.ts",
        &(SHOP.to_owned() + "\nexport function receipt(items: Item[]): string {\n  return items.map((item) => item.name).join(\", \");\n}\n"),
    );
    // `sync` on a lane is the barrier: after it, what was saved is in the log.
    let _: Value = client.call("sync", json!({ "lane": alice })).unwrap();
    let _: Value = client.call("sync", json!({ "lane": bob })).unwrap();
    let status: Value = client.call("lane_status", json!({ "lane": bob })).unwrap();
    assert_eq!(
        (status["ops"].as_u64(), status["files"][0].as_str(), status["intent"].as_str()),
        (Some(2), Some("src/shop.ts"), Some("add a receipt function"))
    );
    let log: Vec<LogItem> = client.call("log", json!({ "lane": bob, "limit": 10 })).unwrap();
    assert!(
        matches!(&log[..], [LogItem::Op { op: add, lane, intent: Some(why), .. }, LogItem::Op { op: edit, .. }]
            if add == "AddSymbol" && edit == "TextEdit" && lane == "lane/2" && why == "add a receipt function"),
        "{log:?}"
    );
    // The main lane has seen nothing so far.
    assert_eq!(read(&root, "src/shop.ts"), SHOP);

    let first = integrate(&mut client, &alice);
    assert_eq!(
        (first.result, first.checkpoint.as_deref()),
        (Outcome::Integrated, Some("ckpt#2")),
        "{first:?}"
    );
    assert!(first.verification.as_ref().unwrap().passed);
    let second = integrate(&mut client, &bob);
    assert_eq!(
        (second.result, second.your_next_base.as_deref()),
        (Outcome::Integrated, Some("ckpt#3")),
        "{second:?}"
    );

    let merged = read(&root, "src/shop.ts");
    assert!(merged.contains("Math.floor(") && merged.contains("export function receipt"), "{merged}");
    // What the daemon wrote into the main directory is not captured again.
    let _: Value = client.call("sync", json!({})).unwrap();
    let main_log: Vec<LogItem> = client.call("log", json!({ "limit": 3 })).unwrap();
    assert!(
        matches!(&main_log[0], LogItem::Checkpoint { id, label, author, .. } if id == "ckpt#3" && label == "receipts" && author == "bob"),
        "{main_log:?}"
    );
    assert_eq!(
        git(&root, &["log", "--format=%an|%s"]).lines().collect::<Vec<_>>(),
        ["bob|receipts", "alice|floor the discount", "ada|start"]
    );
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
    // The story of the new function, asked on the main lane.
    let why: Vec<Why> = client.call("why", json!({ "file": "src/shop.ts", "symbol": "receipt" })).unwrap();
    assert_eq!(
        why[0].steps.iter().map(|s| s.what.as_str()).collect::<Vec<_>>(),
        ["function `receipt` created in src/shop.ts, on lane/2", "integrated into the main lane"]
    );

    // A lane closes; the other survives a restart of the daemon.
    let closed: LaneInfo = client.call("close_lane", json!({ "lane": alice })).unwrap();
    assert_eq!((closed.state.as_str(), alice_dir.exists()), ("closed", false));
    stop(&root, &mut client);
    let mut client = start(&root);
    let lanes: Vec<LaneInfo> = client.call("lanes", json!({ "all": true })).unwrap();
    assert_eq!(
        lanes.iter().map(|l| (l.id.as_str(), l.state.as_str())).collect::<Vec<_>>(),
        [("lane/1", "closed"), ("lane/2", "active")]
    );
    write(&bob_dir, "src/extra.ts", "export const extra = 1;\n");
    assert_eq!(integrate(&mut client, &bob).result, Outcome::Integrated);
    assert_eq!(read(&root, "src/extra.ts"), "export const extra = 1;\n");
    stop(&root, &mut client);
}

#[test]
fn conflicts_verification_and_explicit_renames_over_the_socket() {
    let tmp = new_repo();
    let root = tmp.path().canonicalize().unwrap();
    let mut client = start(&root);
    let _: Value = client.call("sync", json!({})).unwrap();
    let (alice, alice_dir) = open(&mut client, "alice", "absolute totals");

    // An explicit rename: asked, not guessed. The function is recursive, so
    // its body changes with its name; another file still says the old name.
    write(
        &alice_dir,
        "src/tree.ts",
        "export function walk(node: Node): number {\n  return node.next ? 1 + walk(node.next) : 0;\n}\n",
    );
    write(&alice_dir, "src/use.ts", "import { walk } from \"./tree\";\nexport const depth = walk(root);\n");
    let renamed: RenameResult = client
        .call("rename_symbol", json!({ "lane": alice, "symbol": "src/tree.ts:walk", "new_name": "descend" }))
        .unwrap();
    assert_eq!((renamed.from.as_str(), renamed.to.as_str(), renamed.ops.len()), ("walk", "descend", 2));
    assert_eq!(renamed.possible_references, ["src/use.ts:1", "src/use.ts:2"]);
    assert_eq!(
        read(&alice_dir, "src/tree.ts"),
        "export function descend(node: Node): number {\n  return node.next ? 1 + descend(node.next) : 0;\n}\n"
    );
    let log: Vec<LogItem> = client.call("log", json!({ "lane": alice, "limit": 1 })).unwrap();
    assert!(
        matches!(&log[0], LogItem::Op { op, detail: Some(detail), .. } if op == "Rename" && detail.contains("`walk` → `descend`")),
        "{log:?}"
    );
    let error = client
        .call_value("rename_symbol", json!({ "lane": alice, "symbol": "nowhere", "new_name": "x" }))
        .unwrap_err();
    assert!(error.to_string().contains("no symbol `nowhere`"), "{error}");

    // The same line changes on the main lane and on the lane.
    write(&root, "src/shop.ts", &SHOP.replace("return sum;", "return Math.max(sum, 0);"));
    write(&alice_dir, "src/shop.ts", &SHOP.replace("return sum;", "return Math.abs(sum); // FIXME"));
    let blocked = integrate(&mut client, &alice);
    assert_eq!(blocked.result, Outcome::Conflicts, "{blocked:?}");
    let conflicts: Vec<ConflictInfo> = client.call("conflicts", json!({})).unwrap();
    assert_eq!(conflicts, blocked.conflicts);
    assert_eq!((conflicts[0].id.as_str(), conflicts[0].symbol.as_deref()), ("conflict#1", Some("total")));
    let status: Value = client.call("lane_status", json!({ "lane": alice })).unwrap();
    assert_eq!(
        (status["state"].as_str(), status["conflicts"][0]["id"].as_str()),
        (Some("blocked"), Some("conflict#1"))
    );

    // The lane's version is chosen: it merges, but does not pass the verification.
    let error = client.call_value("resolve_conflict", json!({ "conflict": "conflict#1" })).unwrap_err();
    assert!(error.to_string().contains("choice"), "{error}");
    let decided: Value =
        client.call("resolve_conflict", json!({ "conflict": "conflict#1", "choice": "lane" })).unwrap();
    assert_eq!(
        (decided["remaining"].as_u64(), decided["conflict"]["resolution"].as_str()),
        (Some(0), Some("lane"))
    );
    let failed = integrate(&mut client, &alice);
    assert_eq!(failed.result, Outcome::VerificationFailed, "{failed:?}");
    assert_eq!(failed.verification.unwrap().failed_command.as_deref(), Some("! grep -rq FIXME src"));
    assert!(!read(&root, "src/shop.ts").contains("Math.abs"));

    // Fixed in the lane: the conflict is the same in kind but on a new
    // text, so it is asked again; decided with a text, it goes through.
    write(&alice_dir, "src/shop.ts", &SHOP.replace("return sum;", "return Math.abs(sum);"));
    let again = integrate(&mut client, &alice);
    assert_eq!(again.result, Outcome::Conflicts);
    assert_eq!((again.conflicts[0].id.as_str(), again.conflicts[0].resolution.clone()), ("conflict#1", None));
    let text = "export function total(items: Item[]): number {\n  let sum = 0;\n  for (const item of items) {\n    sum += item.price * item.quantity;\n  }\n  return Math.max(Math.abs(sum), 0);\n}\n";
    let _: Value =
        client.call("resolve_conflict", json!({ "conflict": "conflict#1", "text": text })).unwrap();
    let done = integrate(&mut client, &alice);
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    assert!(read(&root, "src/shop.ts").contains("return Math.max(Math.abs(sum), 0);"));
    assert_eq!(read(&root, "src/tree.ts"), read(&alice_dir, "src/tree.ts"));
    let open: Vec<ConflictInfo> = client.call("conflicts", json!({})).unwrap();
    assert!(open.is_empty());
    let why: Vec<Why> = client.call("why", json!({ "file": "src/tree.ts", "symbol": "descend" })).unwrap();
    assert!(
        why[0].steps.iter().any(|s| s.what == "renamed `walk` → `descend`, on lane/1"),
        "{:?}",
        why[0].steps
    );

    // Requests that make no sense are refused with a reason.
    for (method, params, needle) in [
        ("request_integration", json!({ "lane": "main" }), "integrated into"),
        ("request_integration", json!({ "lane": "lane/9" }), "no such open lane"),
        ("integration_result", json!({ "job": "job#99" }), "no such integration request"),
        ("open_lane", json!({}), "agent"),
        ("close_lane", json!({ "lane": "main" }), "cannot be closed"),
    ] {
        let error = client.call_value(method, params).unwrap_err();
        assert!(error.to_string().contains(needle), "{method}: {error}");
    }
    stop(&root, &mut client);
}
