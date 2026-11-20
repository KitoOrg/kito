//! The real CLI (`kito`), with the real daemon it starts by itself.

use kito_core::paths::Canonical as _;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The daemon binary sits next to the CLI's. If the tests run for the
/// `kito-cli` package alone it may not have been built.
fn kitod() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let path = Path::new(env!("CARGO_BIN_EXE_kito")).with_file_name("kitod");
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

fn command(cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kito"));
    command
        .current_dir(cwd)
        .args(args)
        .env("KITOD_BIN", kitod())
        .env("KITO_AUTHOR", "ada")
        .env_remove("KITO_EMAIL")
        // No identity taken from the machine the tests run on.
        .env("KITO_CONFIG_DIR", cwd.join(".absent-personal-config"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    command
}

/// Kito alone: `init` and `import` leave the project's files as they are.
fn kito(cwd: &Path, args: &[&str]) -> Output {
    command(cwd, args).env("KITO_NO_CLAUDE", "1").output().unwrap()
}

/// As it runs for somebody who did not ask otherwise: `init` and `import`
/// set the project up for Claude Code.
fn kito_as_installed(cwd: &Path, args: &[&str]) -> String {
    let out = command(cwd, args).env_remove("KITO_NO_CLAUDE").output().unwrap();
    assert!(
        out.status.success(),
        "kito {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Runs a command that must succeed and returns its standard output.
fn ok(cwd: &Path, args: &[&str]) -> String {
    let out = kito(cwd, args);
    assert!(
        out.status.success(),
        "kito {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn json(cwd: &Path, args: &[&str]) -> Value {
    serde_json::from_str(&ok(cwd, args)).expect("JSON output")
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn work_without_committing_then_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert!(ok(root, &["init"]).contains("Kito repository created"));
    assert!(root.join(".kito/config.toml").is_file());

    // After `init` the daemon is already running: no commit command, just
    // save. `sync` is the barrier that waits for the recording.
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/app.txt"), "export const a = 1;\n").unwrap();
    assert!(ok(root, &["sync"]).contains("Everything is recorded"));
    fs::write(root.join("src/app.txt"), "export const a = 1;\nexport const b = 2;\n").unwrap();
    ok(root, &["sync"]);

    let status = json(root, &["status", "--json"]);
    assert_eq!(status["repo"]["files"], 2);
    assert_eq!(status["repo"]["ops"], 3);
    assert_eq!(status["repo"]["author"], "ada");

    // The log: a line for each file, however many times it was saved.
    let log = ok(root, &["log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(lines[0].contains("src/app.txt") && lines[0].contains("new, 2 saves"), "{log}");
    assert!(
        lines[1].contains(".gitignore") && lines[1].contains("ada") && lines[1].ends_with("new"),
        "{log}"
    );
    // And every save on a line of its own.
    let log = ok(root, &["log", "-v"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3, "{log}");
    assert!(
        lines[0].contains("TextEdit") && lines[0].contains("src/app.txt") && lines[0].contains("+1 −0"),
        "{log}"
    );
    assert!(
        lines[2].contains("FileAdd") && lines[2].contains(".gitignore") && lines[2].contains("ada"),
        "{log}"
    );

    // The log of one file, given as seen from a subdirectory.
    let only = ok(&root.join("src"), &["log", "app.txt"]);
    assert_eq!(only.lines().count(), 1, "{only}");
    let only = ok(&root.join("src"), &["log", "-v", "app.txt"]);
    assert_eq!(only.lines().count(), 2, "{only}");
    let items = json(root, &["log", "--json", "-n", "1"]);
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!(items[0]["type"], "op");

    let done = ok(root, &["checkpoint", "-m", "first version"]);
    assert!(done.contains("ckpt#1 created and exported as git commit"), "{done}");
    assert_eq!(git(root, &["log", "--format=%s|%an"]).trim(), "first version|ada");
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    assert!(ok(root, &["log"]).starts_with("◆ ckpt#1  first version  (git "));
    assert!(ok(root, &["export-git"]).contains("Nothing to export"));

    let daemon = ok(root, &["daemon", "status"]);
    for needle in ["Daemon ............ pid", "Watcher", "Capture", "Save → op", "all exported to git"] {
        assert!(daemon.contains(needle), "missing {needle:?} in:\n{daemon}");
    }
    assert!(ok(root, &["daemon", "stop"]).contains("Daemon stopped"));
    assert!(ok(root, &["daemon", "stop"]).contains("was not running"));
    // After the stop the next command restarts the daemon by itself.
    assert_eq!(json(root, &["status", "--json"])["repo"]["checkpoints"], 1);
    ok(root, &["daemon", "stop"]);
}

#[test]
fn import_a_git_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::write(root.join("main.txt"), "package main\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first commit"]);
    fs::write(root.join("main.txt"), "package main\n\nfunc main() {}\n").unwrap();
    git(root, &["commit", "-q", "-am", "second commit"]);
    fs::write(root.join("uncommitted.txt"), "x\n").unwrap();

    let out = ok(root, &["import"]);
    assert!(out.contains("Imported 2 commits: 3 operations, 2 checkpoints."), "{out}");
    assert!(out.contains("Captured 1 change not yet in git."), "{out}");

    let log = ok(root, &["log", "-v"]);
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines[0].contains("FileAdd") && lines[0].contains("uncommitted.txt"), "{log}");
    assert!(lines[1].starts_with("◆ ckpt#2  second commit  (git "), "{log}");
    assert!(lines[2].contains("TextEdit") && lines[2].contains("Ada") && lines[2].contains("+2 −0"), "{log}");
    assert!(lines[3].starts_with("◆ ckpt#1  first commit"), "{log}");
    // In short: the same lines, and a checkpoint keeps apart what came
    // before it from what came after.
    let log = ok(root, &["log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines[0].contains("uncommitted.txt") && lines[0].ends_with("new"), "{log}");
    assert!(lines[1].starts_with("◆ ckpt#2  second commit  (git "), "{log}");
    assert!(lines[2].contains("main.txt") && lines[2].contains("Ada") && lines[2].contains("+2 −0"), "{log}");
    assert!(lines[3].starts_with("◆ ckpt#1  first commit"), "{log}");

    // A second import makes no sense: the store already has a history.
    let again = kito(root, &["import"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already has history"));

    // `init` in a git repository asks to choose, instead of losing the history.
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["init", "-q", "-b", "main"]);
    let refused = kito(other.path(), &["init"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("kito import"));
    assert!(ok(other.path(), &["init", "--fresh"]).contains("The daemon is running"));
    ok(other.path(), &["daemon", "stop"]);
    ok(root, &["daemon", "stop"]);
}

#[test]
fn errors_are_clear_and_json_stays_json() {
    let tmp = tempfile::tempdir().unwrap();
    let out = kito(tmp.path(), &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no Kito repository"));

    let out = kito(tmp.path(), &["status", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["error"].as_str().unwrap().contains("no Kito repository"));

    ok(tmp.path(), &["init"]);
    let out = kito(tmp.path(), &["checkpoint", "-m", "empty"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing to checkpoint"));
    let out = kito(tmp.path(), &["init"]);
    assert!(!out.status.success(), "init twice");
    ok(tmp.path(), &["daemon", "stop"]);
}

#[test]
fn log_is_up_to_date_even_when_file_events_are_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let outside = tempfile::tempdir().unwrap();
    ok(root, &["init"]);
    fs::write(root.join("note.txt"), "before\n").unwrap();
    assert_eq!(ok(root, &["log"]).lines().count(), 1);

    // A change the watcher cannot see: it goes through a hard link outside
    // the watched directory (like a Windows program on /mnt/c).
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let link = outside.path().join("link");
    fs::hard_link(root.join("note.txt"), &link).unwrap();
    fs::write(&link, "before\nafter\n").unwrap();

    let log = ok(root, &["log", "-v"]);
    assert!(log.lines().next().unwrap().contains("TextEdit"), "kito log must show it right away:\n{log}");
    let log = ok(root, &["log"]);
    assert!(log.contains("note.txt") && log.contains("new, 2 saves"), "{log}");
    // inotify does not see a write through a link from outside; macOS's
    // FSEvents does, and then nothing was missed.
    if cfg!(target_os = "linux") {
        let status = ok(root, &["daemon", "status"]);
        assert!(status.contains("the operating system did not report 1 change"), "{status}");
        assert!(status.contains("full scan every") && !status.contains("full scan every 60 s"), "{status}");
    }
    ok(root, &["daemon", "stop"]);
}

#[test]
fn identity_and_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n.absent-personal-config/\n").unwrap();

    // Identity: first the fallback one, then the one that was set.
    let shown = ok(root, &["identity"]);
    assert!(shown.contains("ada (person)") && shown.contains("environment variables"), "{shown}");
    assert!(shown.contains("No email"), "{shown}");
    let set = ok(root, &["identity", "--name", "Ada Lovelace", "--email", "ada@example.com"]);
    assert!(set.contains("Ada Lovelace <ada@example.com> (person)"), "{set}");
    assert!(set.contains("repository configuration"), "{set}");
    assert_eq!(json(root, &["identity", "--json"])["email"], "ada@example.com");

    fs::write(root.join("note.txt"), "version one\n").unwrap();
    ok(root, &["checkpoint", "-m", "one"]);
    assert_eq!(git(root, &["log", "-1", "--format=%an <%ae>"]).trim(), "Ada Lovelace <ada@example.com>");

    fs::write(root.join("note.txt"), "version two\n").unwrap();
    fs::write(root.join("other.txt"), "added later\n").unwrap();
    ok(root, &["sync"]);

    // Dry run, then a real rollback to the checkpoint.
    let preview = ok(root, &["rollback", "ckpt#1", "--dry-run"]);
    assert!(preview.contains("would change 2 files") && preview.contains("nothing was touched"), "{preview}");
    assert_eq!(fs::read_to_string(root.join("note.txt")).unwrap(), "version two\n");
    let done = ok(root, &["rollback", "ckpt#1"]);
    assert!(done.contains("Rollback to ckpt#1 (one): 2 files"), "{done}");
    assert!(done.contains("restore    note.txt") && done.contains("remove     other.txt"), "{done}");
    assert_eq!(fs::read_to_string(root.join("note.txt")).unwrap(), "version one\n");
    assert!(!root.join("other.txt").exists());

    // The log shows the rollback as new ops, with their why and a short ID.
    let log = ok(root, &["log", "-n", "2"]);
    assert!(log.lines().all(|l| l.contains("[rollback to ckpt#1 (one)]")), "{log}");
    let first = log.lines().next().unwrap().split_whitespace().next().unwrap().to_owned();
    assert_eq!(first.len(), 8, "short op ID: {first}");

    // It is undone with the suggested command.
    let undo = done.lines().find_map(|l| l.strip_prefix("To undo it: kito rollback ")).unwrap().to_owned();
    ok(root, &["rollback", &undo]);
    assert_eq!(fs::read_to_string(root.join("note.txt")).unwrap(), "version two\n");
    assert_eq!(fs::read_to_string(root.join("other.txt")).unwrap(), "added later\n");

    // A single file, given from a subdirectory; and back by N operations.
    fs::create_dir(root.join("sub")).unwrap();
    ok(&root.join("sub"), &["rollback", "ckpt#1", "../note.txt"]);
    assert_eq!(fs::read_to_string(root.join("note.txt")).unwrap(), "version one\n");
    assert!(root.join("other.txt").exists());
    ok(root, &["rollback", "--ops", "1"]);
    assert_eq!(fs::read_to_string(root.join("note.txt")).unwrap(), "version two\n");

    assert!(ok(root, &["rollback", "ckpt#1", "sub"]).contains("nothing to do"));
    let bad = kito(root, &["rollback"]);
    assert!(
        !bad.status.success()
            && String::from_utf8_lossy(&bad.stderr).contains("specify the point to return to")
    );
    let bad = kito(root, &["rollback", "ckpt#42"]);
    assert!(String::from_utf8_lossy(&bad.stderr).contains("does not exist"));
    ok(root, &["daemon", "stop"]);
}

#[test]
fn why_follows_a_function_through_a_rename_and_a_move() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    ok(root, &["init"]);
    let function = "export function getUser(id: string): User {\n  const cached = cache.get(id);\n  if (cached) {\n    return cached;\n  }\n  const user = db.find(id);\n  cache.set(id, user);\n  return user;\n}\n";
    let rest =
        "export class Session {\n  isValid(now: number): boolean {\n    return this.expires > now;\n  }\n}\n";
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/auth.ts"), format!("{function}\n{rest}")).unwrap();
    ok(root, &["sync"]);

    // Rename it, then move it to another file, then change it there.
    let renamed = function.replace("getUser", "fetchUser");
    fs::write(root.join("src/auth.ts"), format!("{renamed}\n{rest}")).unwrap();
    ok(root, &["sync"]);
    fs::write(root.join("src/auth.ts"), rest).unwrap();
    fs::write(root.join("src/users.ts"), &renamed).unwrap();
    ok(root, &["sync"]);
    fs::write(
        root.join("src/users.ts"),
        renamed.replace("cache.set(id, user);", "cache.set(id, user, TTL);"),
    )
    .unwrap();
    ok(root, &["sync"]);

    // Asked by the name and the place it has today, from a subdirectory.
    let why = ok(&root.join("src"), &["why", "--history", "users.ts:fetchUser"]);
    let lines: Vec<&str> = why.lines().collect();
    assert_eq!(lines.len(), 5, "{why}");
    assert!(lines[0].starts_with("function fetchUser  (src/users.ts)  symbol "), "{why}");
    for (line, what) in lines[1..].iter().zip([
        "function `getUser` created in src/auth.ts",
        "renamed `getUser` → `fetchUser`",
        "moved from src/auth.ts to src/users.ts",
        "body changed (+1 −1)",
    ]) {
        assert!(line.contains("ada") && line.ends_with(what), "{why}");
    }

    let histories = json(root, &["why", "--history", "src/users.ts:fetchUser", "--json"]);
    assert_eq!(histories.as_array().unwrap().len(), 1);
    assert_eq!(histories[0]["steps"].as_array().unwrap().len(), 4);
    assert_eq!(histories[0]["name"], "fetchUser");
    let id = histories[0]["id"].as_str().unwrap().to_owned();
    // The method was never touched: one step, and a different symbol.
    let method = json(root, &["why", "--history", "src/auth.ts:Session.isValid", "--json"]);
    assert_eq!(method[0]["steps"].as_array().unwrap().len(), 1);
    assert_ne!(method[0]["id"], id);

    // The log shows what each save meant, under the file it changed.
    let log = ok(root, &["log", "-v", "-n", "4"]);
    assert!(log.contains("↳") && log.contains("`fetchUser` moved from src/auth.ts to src/users.ts"), "{log}");
    // In short, on the line of the file it went to.
    let log = ok(root, &["log", "-n", "4"]);
    let line = log.lines().find(|line| line.contains("fetchUser") && line.contains("moved")).expect(&log);
    assert!(line.contains("src/users.ts") && !log.contains("↳"), "{log}");
    assert_eq!(json(root, &["status", "--json"])["repo"]["symbols"], 3);

    // Where it no longer is, and what does not exist.
    let gone = kito(root, &["why", "--history", "src/auth.ts:fetchUser"]);
    assert!(!gone.status.success());
    let message = String::from_utf8_lossy(&gone.stderr).into_owned();
    assert!(
        message.contains("no symbol named `fetchUser`") && message.contains("Session.isValid"),
        "{message}"
    );
    let malformed = kito(root, &["why", "--history", "fetchUser"]);
    assert!(!malformed.status.success());
    ok(root, &["daemon", "stop"]);
}

#[test]
fn lanes_integration_and_conflicts_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    let shop = "export function total(items: Item[]): number {\n  let sum = 0;\n  for (const item of items) {\n    sum += item.price * item.quantity;\n  }\n  return sum;\n}\n\nexport function discount(amount: number, percent: number): number {\n  const cut = Math.round((amount * percent) / 100);\n  return amount - cut;\n}\n";
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.ts"), shop).unwrap();
    ok(root, &["checkpoint", "-m", "start"]);
    assert!(ok(root, &["lanes"]).contains("No open lanes"));

    // Two lanes, each with a working copy of its own.
    // No lane without saying why and what.
    let refused = kito(root, &["lane", "open", "alice"]);
    assert!(!refused.status.success());
    let opened = ok(
        root,
        &[
            "lane",
            "open",
            "alice",
            "--task",
            "floor the discount",
            "--intent",
            "floor the discount",
            "--scope",
            "src/shop.ts",
        ],
    );
    assert!(
        opened.contains("lane/1 opened for alice on ckpt#1") && opened.contains("Working copy:"),
        "{opened}"
    );
    let bob =
        json(root, &["lane", "open", "bob", "--intent", "the total", "--scope", "src/shop.ts", "--json"]);
    assert_eq!(bob["id"], "lane/2");
    let (alice_dir, bob_dir) =
        (root.join(".kito/lanes/1/work"), PathBuf::from(bob["workdir"].as_str().unwrap()));
    assert_eq!(bob_dir, root.join(".kito/lanes/2/work"));

    fs::write(alice_dir.join("src/shop.ts"), shop.replace("Math.round(", "Math.floor(")).unwrap();
    fs::write(bob_dir.join("src/shop.ts"), shop.replace("return sum;", "return Math.abs(sum);")).unwrap();
    assert!(
        ok(root, &["intent", "absolute totals", "--lane", "lane/2", "--task", "task #9"]).contains("lane/2")
    );
    assert!(ok(root, &["sync", "lane/1"]).contains("Everything is recorded"));
    let lanes = ok(root, &["lanes"]);
    let lines: Vec<&str> = lanes.lines().collect();
    assert_eq!(lines.len(), 2, "{lanes}");
    assert!(
        lines[0].starts_with("lane/1")
            && lines[0].contains("alice")
            && lines[0].contains("floor the discount"),
        "{lanes}"
    );
    assert!(lines[0].contains("2 ops") && lines[0].contains("1 file"), "{lanes}");
    let status = ok(root, &["lane", "status", "lane/1"]);
    assert!(
        status.contains("changed  src/shop.ts") && status.contains("Agent ............. alice"),
        "{status}"
    );
    // The log of a lane, and a symbol's story as the lane knows it.
    let log = ok(root, &["log", "lane/1"]);
    assert!(log.contains("src/shop.ts") && log.contains("discount changed"), "{log}");
    let log = ok(root, &["log", "-v", "lane/1"]);
    assert!(log.contains("TextEdit") && log.contains("`discount` body changed"), "{log}");
    assert!(
        ok(root, &["why", "--history", "src/shop.ts:discount", "--lane", "lane/1"]).contains("body changed")
    );

    // Alice goes in. Meanwhile the main lane changes the line Bob changed.
    let done = ok(root, &["integrate", "lane/1"]);
    assert!(done.starts_with("lane/1: integrated as ckpt#2 (git "), "{done}");
    assert!(fs::read_to_string(root.join("src/shop.ts")).unwrap().contains("Math.floor("));
    let now = fs::read_to_string(root.join("src/shop.ts")).unwrap();
    fs::write(root.join("src/shop.ts"), now.replace("return sum;", "return Math.max(sum, 0);")).unwrap();
    let blocked = kito(root, &["integrate", "lane/2"]);
    assert!(!blocked.status.success(), "a conflict is not a success");
    let blocked = String::from_utf8_lossy(&blocked.stdout).into_owned();
    assert!(
        blocked.contains("lane/2: 1 conflict") && blocked.contains("conflict#1") && blocked.contains("total"),
        "{blocked}"
    );
    assert!(blocked.contains("kito resolve"), "{blocked}");

    let conflicts = ok(root, &["conflicts"]);
    assert!(
        conflicts.contains("conflict#1") && conflicts.contains("lane/2") && conflicts.contains("[to decide]"),
        "{conflicts}"
    );
    let shown = ok(root, &["conflicts", "conflict#1"]);
    for needle in [
        "--- at the common starting point",
        "--- on the main lane",
        "--- on lane/2",
        "Math.max(sum, 0)",
        "Math.abs(sum)",
    ] {
        assert!(shown.contains(needle), "missing {needle:?} in:\n{shown}");
    }
    assert_eq!(json(root, &["conflicts", "--json"])[0]["kind"], "body_body");
    assert!(!kito(root, &["resolve", "conflict#1"]).status.success(), "a decision is needed");
    let decided = ok(root, &["resolve", "1", "--lane"]);
    assert!(decided.contains("conflict#1 decided") && decided.contains("kito integrate lane/2"), "{decided}");
    assert!(ok(root, &["conflicts"]).contains("[decided: lane]"));
    let done = json(root, &["integrate", "lane/2", "--json"]);
    assert_eq!(
        (done[0]["result"].as_str(), done[0]["checkpoint"].as_str()),
        (Some("integrated"), Some("ckpt#3"))
    );
    let merged = fs::read_to_string(root.join("src/shop.ts")).unwrap();
    assert!(merged.contains("return Math.abs(sum);") && merged.contains("Math.floor("), "{merged}");
    assert!(ok(root, &["conflicts"]).contains("No open conflicts"));
    assert_eq!(
        git(root, &["log", "--format=%an|%s"]).lines().collect::<Vec<_>>(),
        ["bob|absolute totals", "alice|floor the discount", "ada|start"]
    );

    // An explicit rename on the main lane, and a lane that closes.
    let renamed = ok(root, &["rename", "src/shop.ts:total", "grandTotal"]);
    assert!(renamed.contains("`total` is now `grandTotal` in src/shop.ts"), "{renamed}");
    assert!(
        ok(root, &["why", "--history", "src/shop.ts:grandTotal"]).contains("renamed `total` → `grandTotal`")
    );
    assert!(ok(root, &["lane", "close", "lane/1"]).contains("lane/1 closed"));
    assert!(!alice_dir.exists());
    let all = ok(root, &["lanes", "--all"]);
    assert!(all.contains("closed") && all.lines().count() == 2, "{all}");
    let unknown = kito(root, &["integrate", "lane/1"]);
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("no such open lane"));
    ok(root, &["daemon", "stop"]);
}

const CLAUDE_FILES: [&str; 4] = [".gitignore", ".mcp.json", ".claude/settings.json", "CLAUDE.md"];

#[test]
fn init_sets_the_project_up_for_claude_code() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("app.ts"), "export const a = 1;\n").unwrap();
    let out = kito_as_installed(root, &["init"]);
    assert!(out.contains("Set up for Claude Code:"), "{out}");
    for file in CLAUDE_FILES {
        assert!(root.join(file).is_file(), "{file} is missing");
        assert!(out.contains(&format!("created     {file}")), "{file} in:\n{out}");
    }
    let servers: Value = serde_json::from_str(&fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(servers["mcpServers"]["kito"]["command"], "kito-mcp");
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(settings["permissions"]["allow"][0], "mcp__kito__*");
    assert_eq!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "kito hook before-edit");
    assert!(fs::read_to_string(root.join("CLAUDE.md")).unwrap().contains("`open_lane`"));

    // They are files of the project like the others: recorded, and in git
    // with the first checkpoint, where `.kito/` does not show.
    ok(root, &["sync"]);
    assert_eq!(json(root, &["status", "--json"])["repo"]["files"], 5);
    ok(root, &["checkpoint", "-m", "start"]);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    let tracked = git(root, &["ls-files"]);
    for file in CLAUDE_FILES {
        assert!(tracked.lines().any(|line| line == file), "{file} not in git:\n{tracked}");
    }

    // Asked again, there is nothing to do.
    let again = kito_as_installed(root, &["claude"]);
    assert!(again.contains("set up here already"), "{again}");
    let again = json(root, &["claude", "--json"]);
    assert!(again["files"].as_array().unwrap().iter().all(|file| file["change"] == "unchanged"), "{again}");
    ok(root, &["sync"]);
    assert!(ok(root, &["status"]).contains("0 since the last checkpoint"));
    ok(root, &["daemon", "stop"]);
}

#[test]
fn a_project_can_be_left_alone_and_set_up_later() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    fs::write(root.join("CLAUDE.md"), "# Notes\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "start"]);

    let out = kito_as_installed(root, &["import", "--no-claude"]);
    assert!(!out.contains("Claude"), "{out}");
    assert!(!root.join(".mcp.json").exists() && !root.join(".claude").exists());
    assert_eq!(fs::read_to_string(root.join("CLAUDE.md")).unwrap(), "# Notes\n");

    // Later, with the daemon running: the files are recorded as they are written.
    let out = kito_as_installed(root, &["claude"]);
    for (file, change) in [
        (".gitignore", "updated"),
        (".mcp.json", "created"),
        (".claude/settings.json", "created"),
        ("CLAUDE.md", "updated"),
    ] {
        assert!(out.contains(&format!("{change:<10}  {file}")), "{change} {file} in:\n{out}");
    }
    assert_eq!(fs::read_to_string(root.join(".gitignore")).unwrap(), "target/\n.kito/\n");
    assert!(fs::read_to_string(root.join("CLAUDE.md"))
        .unwrap()
        .starts_with("# Notes\n\n## Version control: Kito\n"));
    ok(root, &["sync"]);
    assert_eq!(json(root, &["status", "--json"])["repo"]["files"], 4);
    ok(root, &["checkpoint", "-m", "claude"]);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    ok(root, &["daemon", "stop"]);

    // `import` does it by itself, and what it writes is waiting for a checkpoint.
    let other = tempfile::tempdir().unwrap();
    let other = other.path();
    git(other, &["init", "-q", "-b", "main"]);
    fs::write(other.join("a.txt"), "a\n").unwrap();
    git(other, &["add", "-A"]);
    git(other, &["commit", "-q", "-m", "start"]);
    let out = kito_as_installed(other, &["import"]);
    assert!(
        out.contains("Set up for Claude Code:") && out.contains("Captured 4 changes not yet in git"),
        "{out}"
    );
    let imported = json(other, &["status", "--json"]);
    assert_eq!(imported["repo"]["files"], 5);
    ok(other, &["daemon", "stop"]);
}

#[test]
fn the_log_is_short_however_often_a_file_is_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    ok(root, &["init"]);
    fs::write(root.join("first.txt"), "first\n").unwrap();
    ok(root, &["sync"]);
    // More saves than the daemon is asked for at first: the line is whole
    // all the same.
    let mut text = String::new();
    for n in 0..70 {
        text.push_str(&format!("line {n}\n"));
        fs::write(root.join("notes.txt"), &text).unwrap();
        ok(root, &["sync"]);
    }
    let log = ok(root, &["log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(lines[0].contains("notes.txt") && lines[0].contains("new, 70 saves"), "{log}");
    assert!(lines[1].contains("first.txt"), "{log}");
    let one = ok(root, &["log", "-n", "1"]);
    assert_eq!(one.lines().collect::<Vec<_>>(), [lines[0]], "{one}");
    // The short ID is the last save's: the point a rollback returns to.
    let id = lines[0].split_whitespace().next().unwrap();
    let newest = ok(root, &["log", "-v", "-n", "1"]);
    assert!(newest.trim_start().starts_with(id), "{newest}");
    assert_eq!(ok(root, &["log", "-v", "-n", "1000"]).lines().count(), 71);
    assert_eq!(json(root, &["log", "--json", "-n", "5"]).as_array().unwrap().len(), 5);
    ok(root, &["daemon", "stop"]);
}

const SHOP_PY: &str = "def total(price, quantity):\n    subtotal = price * quantity\n    tax = subtotal * 0.22\n    return subtotal + tax\n\n\ndef receipt(price, quantity):\n    return \"to pay: \" + str(total(price, quantity))\n";

#[test]
fn scopes_the_board_and_who_is_on_what_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.py"), SHOP_PY).unwrap();
    fs::write(root.join("README.md"), "# Shop\n").unwrap();
    ok(root, &["checkpoint", "-m", "start"]);

    let erin =
        ok(root, &["lane", "open", "erin", "--intent", "prices in cents", "--scope", "src/shop.py:total"]);
    assert!(erin.contains("Scope: src/shop.py:total") && !erin.contains("overlap"), "{erin}");
    // The second is told before it writes a line, with what the first is there for.
    let frank = ok(
        root,
        &[
            "lane",
            "open",
            "frank",
            "--intent",
            "a discount over 50",
            "--scope",
            "src/shop.py:total",
            "README.md",
        ],
    );
    assert!(
        frank.contains("overlap (function) with lane/1 of erin (ada), there for: prices in cents"),
        "{frank}"
    );
    assert!(frank.contains("src/shop.py:total: erin declared it; you declared it"), "{frank}");
    // A function that uses hers is an overlap too.
    let gina =
        ok(root, &["lane", "open", "gina", "--intent", "a nicer receipt", "--scope", "src/shop.py:receipt"]);
    assert!(gina.contains("overlap (caller) with lane/1 of erin (ada)"), "{gina}");
    assert!(gina.contains("your src/shop.py:receipt uses src/shop.py:total, which erin declared"), "{gina}");

    let board = ok(root, &["board"]);
    for needle in [
        "lane/1  erin (of ada)  active  prices in cents",
        "  scope    src/shop.py:total",
        "lane/2  frank (of ada)  active  a discount over 50",
        "  scope    src/shop.py:total, README.md",
        "overlap (function) with lane/2 of frank (ada), there for: a discount over 50",
    ] {
        assert!(board.contains(needle), "missing {needle:?} in:\n{board}");
    }
    let who = ok(root, &["who", "src/shop.py:total"]);
    assert!(
        who.contains("lane/1 of erin, ada's (function)") && who.contains("lane/2 of frank, ada's (function)"),
        "{who}"
    );
    assert!(ok(root, &["who", "README.md"]).contains("lane/2 of frank, ada's (file)"));
    assert!(ok(root, &["who", "src/other.py"]).contains("No open lane declared or changed src/other.py"));
    assert_eq!(json(root, &["board", "--json"]).as_array().unwrap().len(), 3);

    // A change outside the scope is recorded, shown, and stops the lane.
    fs::write(root.join(".kito/lanes/1/work/README.md"), "# Shop, in cents\n").unwrap();
    let refused = kito(root, &["integrate", "lane/1"]);
    assert!(!refused.status.success());
    let refused = String::from_utf8_lossy(&refused.stdout).into_owned();
    assert!(refused.contains("lane/1: not integrated, it changed things outside its scope"), "{refused}");
    assert!(refused.contains("  outside the scope  README.md"), "{refused}");
    assert!(ok(root, &["lane", "status", "lane/1"]).contains("  outside the scope  README.md"));
    assert_eq!(fs::read_to_string(root.join("README.md")).unwrap(), "# Shop\n");

    // Declared, it goes in; the scope only grows.
    let grown = ok(root, &["lane", "scope", "lane/1", "--add", "README.md"]);
    assert!(grown.contains("Scope ............. src/shop.py:total, README.md"), "{grown}");
    assert!(grown.contains("overlap (file) with lane/2 of frank (ada)"), "{grown}");
    assert!(ok(root, &["integrate", "lane/1"]).contains("lane/1: integrated as ckpt#2"));
    assert_eq!(fs::read_to_string(root.join("README.md")).unwrap(), "# Shop, in cents\n");
    let scope = json(root, &["lane", "scope", "lane/2", "--json"]);
    assert_eq!(scope["scope"], serde_json::json!(["src/shop.py:total", "README.md"]));
    ok(root, &["daemon", "stop"]);
}

#[test]
fn why_tells_the_reasons_of_code_and_reasons_are_found_by_their_words() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    let shop = "export function total(prices: number[]): number {\n  let sum = 0;\n  for (const price of prices) {\n    sum += price;\n  }\n  return sum;\n}\n\nexport function discount(amount: number, percent: number): number {\n  const cut = (amount * percent) / 100;\n  return amount - cut;\n}\n";
    let billing = "import { total } from \"./shop\";\n\nexport function invoice(prices: number[]): string {\n  return \"to pay: \" + total(prices);\n}\n";
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.ts"), shop).unwrap();
    fs::write(root.join("src/billing.ts"), billing).unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);

    // Alice's agent puts the prices in cents, then refuses negative ones:
    // two reasons in one lane, for one task.
    ok(
        root,
        &[
            "lane",
            "open",
            "alice",
            "--intent",
            "prices in cents",
            "--task",
            "task #7",
            "--scope",
            "src/shop.ts:total",
            "src/billing.ts",
        ],
    );
    let work = root.join(".kito/lanes/1/work");
    fs::write(work.join("src/shop.ts"), shop.replace("sum += price;", "sum += Math.round(price * 100);"))
        .unwrap();
    fs::write(work.join("src/billing.ts"), billing.replace("total(prices)", "total(prices) / 100")).unwrap();
    ok(root, &["sync", "1"]);
    ok(root, &["intent", "no negative prices", "--lane", "1"]);
    let now = fs::read_to_string(work.join("src/shop.ts")).unwrap();
    let guarded = now.replace(
        "  let sum = 0;",
        "  let sum = 0;\n  if (prices.some((p) => p < 0)) throw new Error(\"negative\");",
    );
    fs::write(work.join("src/shop.ts"), guarded).unwrap();
    assert!(ok(root, &["integrate", "1"]).contains("integrated as ckpt#2"));
    ok(root, &["lane", "close", "1"]);
    // Carol's agent is on the same function now.
    ok(root, &["lane", "open", "carol", "--intent", "taxes on top", "--scope", "src/shop.ts:total"]);

    let why = ok(root, &["why", "src/shop.ts:total"]);
    assert!(why.starts_with("function total  (src/shop.ts)  lines 1-8\n"), "{why}");
    assert!(why.contains("Why it is the way it is, newest first:"), "{why}");
    assert!(
        why.contains(
            "  1. no negative prices  (task #7)\n     body changed · by alice for ada · lane/1 · ckpt#2 · "
        ),
        "{why}"
    );
    assert!(why.contains("  2. prices in cents  (task #7)\n"), "{why}");
    assert!(why.contains("     with: src/billing.ts:invoice\n"), "{why}");
    assert!(why.contains("  3. first version\n     created · by ada · ckpt#1 · "), "{why}");
    // Each line with the number of its reason.
    assert!(why.contains("    3     1  export function total(prices: number[]): number {\n"), "{why}");
    assert!(
        why.contains("    1     3    if (prices.some((p) => p < 0)) throw new Error(\"negative\");\n"),
        "{why}"
    );
    assert!(why.contains("    2     5      sum += Math.round(price * 100);\n"), "{why}");
    assert!(why.contains("Used by:\n  src/billing.ts:invoice (line 4)  last changed for: prices in cents (alice for ada, ckpt#2)\n"), "{why}");
    assert!(
        why.contains("On it now:\n  lane/2  carol (of ada)  taxes on top  — carol declared it\n"),
        "{why}"
    );

    // A line says which function it is in; a file, what it contains; from
    // a directory inside the repository, paths are the ones typed there.
    let line = ok(root, &["why", "src/shop.ts:11"]);
    assert!(line.starts_with("function discount  (src/shop.ts)  lines 10-13\n"), "{line}");
    assert!(
        line.contains("    1    11    const cut = (amount * percent) / 100;\n")
            && !line.contains("    1    10  "),
        "{line}"
    );
    let file = ok(&root.join("src"), &["why", "billing.ts"]);
    assert!(file.starts_with("file src/billing.ts\n"), "{file}");
    assert!(
        file.contains("  1. prices in cents  (task #7)\n     changed · by alice for ada · lane/1 · ckpt#2"),
        "{file}"
    );
    assert!(
        file.contains("It contains:\n  src/billing.ts:invoice (line 3)  last changed for: prices in cents"),
        "{file}"
    );

    // As JSON, for tools; as a diagram, for people.
    let answer = json(root, &["why", "src/shop.ts:total", "--json"]);
    assert_eq!(answer["reasons"][1]["with"], serde_json::json!(["src/billing.ts:invoice"]));
    assert_eq!(answer["in_progress"][0]["lane"], "lane/2");
    let graph = ok(root, &["why", "src/shop.ts:total", "--graph"]);
    assert!(graph.starts_with("flowchart LR\n  n0[\"src/shop.ts:total\"]\n"), "{graph}");
    assert!(
        graph.contains("([\"prices in cents<br/>alice for ada · ckpt#2\"])")
            && graph.contains("-->|body changed| n0"),
        "{graph}"
    );
    assert!(
        graph.contains("-.->|uses| n0") && graph.contains("{{\"lane/2 carol: taxes on top\"}}"),
        "{graph}"
    );

    // Earlier work, by the words of its reason.
    let found = ok(root, &["reasons", "cents"]);
    assert!(
        found.starts_with("prices in cents  (task #7)\n  by alice for ada · lane/1 · ckpt#2 · "),
        "{found}"
    );
    assert!(found.contains("  changed: src/billing.ts:invoice, src/shop.ts:total\n"), "{found}");
    assert_eq!(ok(root, &["reasons", "task", "#7"]).matches("task #7").count(), 2);
    assert!(ok(root, &["reasons", "nothing", "like", "this"]).contains("No reason has those words."));
    // A lane that has changed nothing yet has given no reason for any code.
    assert_eq!(ok(root, &["reasons", "-n", "1"]).lines().next(), Some("no negative prices  (task #7)"));

    // What cannot be answered says why.
    let missing = kito(root, &["why", "src/shop.ts:nothing"]);
    assert!(String::from_utf8_lossy(&missing.stderr).contains("the file has: discount, total"));
    ok(root, &["daemon", "stop"]);
}

/// Runs a hook as Claude Code does: what it says about the tool on the
/// standard input. Returns what the hook answered (`Null` for nothing),
/// its exit code and what it wrote for people.
fn hook(cwd: &Path, args: &[&str], input: &Value) -> (Value, i32, String) {
    use std::io::Write;
    let mut child = command(cwd, args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let answer = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (answer, out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// An edit as Claude Code announces it before making it.
fn edit(file: &Path, old: &str, new: &str) -> Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Edit",
        "cwd": file.parent().unwrap(),
        "tool_input": { "file_path": file, "old_string": old, "new_string": new, "replace_all": false },
    })
}

/// The words of a refusal; empty if the edit was let through.
fn refusal(answer: &(Value, i32, String)) -> String {
    let said = &answer.0["hookSpecificOutput"];
    assert_eq!(answer.1, 0, "a hook that answers leaves with 0: {answer:?}");
    match said["permissionDecision"].as_str() {
        Some("deny") => {
            assert_eq!(said["hookEventName"], "PreToolUse");
            said["permissionDecisionReason"].as_str().unwrap().to_owned()
        }
        _ => {
            assert_eq!(answer.0, Value::Null, "nothing to say for an edit that may be made");
            String::new()
        }
    }
}

#[test]
fn hooks_hold_an_agent_to_the_rules_before_it_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    let shop = "export function total(prices: number[]): number {\n  let sum = 0;\n  for (const price of prices) {\n    sum += price;\n  }\n  return sum;\n}\n\nexport function discount(amount: number, percent: number): number {\n  const cut = (amount * percent) / 100;\n  return amount - cut;\n}\n";
    fs::write(root.join(".gitignore"), ".kito/\nbuild/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.ts"), shop).unwrap();
    fs::write(root.join("README.md"), "# Shop\n").unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    ok(root, &["lane", "open", "alice", "--intent", "prices in cents", "--scope", "src/shop.ts:total"]);
    let work = root.join(".kito/lanes/1/work");
    fs::write(work.join("src/shop.ts"), shop.replace("sum += price;", "sum += Math.round(price * 100);"))
        .unwrap();
    assert!(ok(root, &["integrate", "1"]).contains("integrated as ckpt#2"));
    ok(root, &["lane", "close", "1"]);

    // 1. The main directory is no place for an agent to edit.
    let no =
        refusal(&hook(root, &["hook", "before-edit"], &edit(&root.join("src/shop.ts"), "let sum = 0;", "x")));
    assert!(no.contains("src/shop.ts is in the main directory"), "{no}");
    assert!(no.contains("open_lane"), "{no}");
    // What Kito does not record may be written; so may anything outside the repository.
    let built = edit(&root.join("build/out.js"), "", "x");
    assert_eq!(refusal(&hook(root, &["hook", "before-edit"], &built)), "");
    let elsewhere = tempfile::tempdir().unwrap();
    let other = edit(&elsewhere.path().join("notes.txt"), "", "x");
    assert_eq!(refusal(&hook(root, &["hook", "before-edit"], &other)), "");
    // Kito's own files are nobody's to edit.
    let no = refusal(&hook(root, &["hook", "before-edit"], &edit(&root.join(".kito/config.toml"), "", "x")));
    assert!(no.contains("one of Kito's own files"), "{no}");

    // 2. A lane is told why what it declares is the way it is.
    let opened =
        ok(root, &["lane", "open", "bob", "--intent", "a flat discount", "--scope", "src/shop.ts:discount"]);
    assert!(opened.contains("Why it is the way it is:\n  src/shop.ts:discount\n"), "{opened}");
    assert!(opened.contains("    \"first version\": ada, checkpoint ckpt#1\n"), "{opened}");
    let work = root.join(".kito/lanes/2/work");

    // 3. Its own function may be edited: it was told. Another one may not.
    let own = edit(&work.join("src/shop.ts"), "  return amount - cut;", "  return amount - percent;");
    assert_eq!(refusal(&hook(root, &["hook", "before-edit"], &own)), "");
    let other = edit(&work.join("src/shop.ts"), "    sum += Math.round(price * 100);", "    sum += price;");
    let no = refusal(&hook(root, &["hook", "before-edit"], &other));
    assert!(no.contains("src/shop.ts:total is outside what lane/2 declared (src/shop.ts:discount)"), "{no}");
    assert!(no.contains("scope [\"src/shop.ts:total\"]"), "{no}");
    // A file it did not declare, written whole.
    let write = serde_json::json!({
        "hook_event_name": "PreToolUse", "tool_name": "Write", "cwd": root,
        "tool_input": { "file_path": work.join("README.md"), "content": "# The shop\n" },
    });
    let no = refusal(&hook(root, &["hook", "before-edit"], &write));
    assert!(no.contains("README.md is outside what lane/2 declared"), "{no}");

    // 4. Declared as a directory, nothing is told up front: the edit is
    //    held back once, with the reasons behind its lines.
    let grown = ok(root, &["lane", "scope", "2", "--add", "src/"]);
    assert!(grown.contains("a directory was declared"), "{grown}");
    let held = refusal(&hook(root, &["hook", "before-edit"], &other));
    assert!(held.contains("this edit was held back once"), "{held}");
    assert!(held.contains("- \"prices in cents\": alice for ada, lane/1, checkpoint ckpt#2"), "{held}");
    assert!(!held.contains("first version"), "told when the lane was opened: {held}");
    assert_eq!(refusal(&hook(root, &["hook", "before-edit"], &other)), "", "the second time it goes through");

    // 5. For any other tool: in words, and 2 when the edit should not be made.
    let (_, code, said) = hook(root, &["hook", "before-edit", "--file", "src/shop.ts"], &Value::Null);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains("is in the main directory"), "{said}");
    let file = work.join("src/shop.ts");
    let (_, code, said) = hook(
        root,
        &["hook", "before-edit", "--file", file.to_str().unwrap(), "--lines", "2-3"],
        &Value::Null,
    );
    assert_eq!((code, said.as_str()), (0, ""));

    // 6. After a shell command: what it changed outside the scope of its
    //    lane. What was written before it started (the files of this test,
    //    a moment ago) is not its doing.
    std::thread::sleep(std::time::Duration::from_millis(400));
    fs::write(work.join("README.md"), "# The shop\n").unwrap();
    let ran = serde_json::json!({
        "hook_event_name": "PostToolUse", "tool_name": "Bash", "cwd": root, "duration_ms": 40,
        "tool_input": { "command": format!("sed -i s/Shop/The\\ shop/ {}", work.join("README.md").display()) },
        "tool_response": { "stdout": "", "stderr": "" },
    });
    let (answer, code, _) = hook(root, &["hook", "after-shell"], &ran);
    assert_eq!(code, 0);
    assert_eq!(answer["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let told = answer["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(told.starts_with("Kito: lane/2 (bob) has changes outside what it declared: README.md"), "{told}");
    // A command that changed nothing where it should not have has nothing to add.
    fs::write(work.join("README.md"), "# Shop\n").unwrap();
    let (answer, code, _) = hook(root, &["hook", "after-shell"], &ran);
    assert_eq!((answer, code), (Value::Null, 0));
    // One that wrote in the main directory is told so.
    fs::write(root.join("README.md"), "# Shop\n\nWritten where it stands.\n").unwrap();
    ok(root, &["sync"]);
    let (answer, _, _) = hook(root, &["hook", "after-shell"], &ran);
    let told = answer["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(told.contains("files of the main directory were changed: README.md"), "{told}");

    // 7. When a session starts: the rules that are enforced, and who is on what.
    let start = serde_json::json!({ "hook_event_name": "SessionStart", "source": "startup", "cwd": root });
    let (answer, code, _) = hook(root, &["hook", "session-start"], &start);
    assert_eq!(code, 0);
    let told = answer["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(told.contains("They are enforced, not advice"), "{told}");
    assert!(told.contains("Lanes open right now (1)"), "{told}");
    assert!(
        told.contains("- lane/2 bob for ada, active: \"a flat discount\" [src/shop.ts:discount, src/]"),
        "{told}"
    );
    // Outside a repository a hook has nothing to say.
    let (answer, code, _) =
        hook(elsewhere.path(), &["hook", "session-start"], &serde_json::json!({ "cwd": elsewhere.path() }));
    assert_eq!((answer, code), (Value::Null, 0));

    // Somebody who watches sees the edits that were not made.
    let seen = ok(root, &["watch", "--past", "--lane", "lane/2", "-n", "2"]);
    assert!(seen.contains("edit refused"), "{seen}");
    assert!(seen.contains("src/shop.ts:total: outside the scope, not made"), "{seen}");
    ok(root, &["daemon", "stop"]);
}

#[test]
#[cfg(unix)] // Unix permissions or links: not on Windows (ADR 0002)
fn a_command_runs_confined_to_its_lane() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::write(root.join("app.py"), "def a():\n    return 1\n").unwrap();
    ok(root, &["checkpoint", "-m", "start"]);
    ok(root, &["lane", "open", "alice", "--intent", "two", "--scope", "app.py"]);
    let work = root.join(".kito/lanes/1/work");
    // Whether this system can confine a command at all.
    let able = kito(root, &["lane", "run", "1", "--", "true"]);
    if !able.status.success() {
        let why = String::from_utf8_lossy(&able.stderr).into_owned();
        assert!(why.contains("cannot be confined here"), "{why}");
        ok(root, &["daemon", "stop"]);
        return;
    }

    // In its lane it reads, writes and runs; its exit code is its own.
    let wrote = kito(root, &["lane", "run", "1", "--", "sh", "-c", "pwd && cat app.py && echo x > made.txt"]);
    assert!(wrote.status.success(), "{wrote:?}");
    let said = String::from_utf8_lossy(&wrote.stdout).into_owned();
    assert!(said.starts_with(&format!("{}\n", work.display())) && said.contains("def a()"), "{said}");
    assert!(work.join("made.txt").is_file());
    assert_eq!(kito(root, &["lane", "run", "1", "--", "sh", "-c", "exit 7"]).status.code(), Some(7));

    // Outside it: the main directory, another place of the machine,
    // Kito's own data and the key of the daemon's socket.
    let script = format!(
        "echo x > {root}/hacked.txt; echo x > {root}/../outside.txt; cat {root}/.kito/config.toml; \
         cat {root}/.kito/socket.key; ls {root}/.kito/lanes; echo done",
        root = root.display()
    );
    let tried = kito(root, &["lane", "run", "1", "--", "sh", "-c", &script]);
    let (out, err) = (String::from_utf8_lossy(&tried.stdout), String::from_utf8_lossy(&tried.stderr));
    assert_eq!(out.trim(), "done", "nothing was read: {out}");
    assert_eq!(err.matches("Permission denied").count(), 5, "{err}");
    assert!(!root.join("hacked.txt").exists() && !root.parent().unwrap().join("outside.txt").exists());

    // It cannot command the daemon either: the socket asks for a key it cannot read.
    use std::io::{BufRead, Write};
    let mut raw = std::os::unix::net::UnixStream::connect(root.join(".kito/kitod.sock")).unwrap();
    raw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"protect\",\"params\":{\"on\":false}}\n")
        .unwrap();
    let mut answer = String::new();
    std::io::BufReader::new(raw).read_line(&mut answer).unwrap();
    assert!(answer.contains("did not say the key"), "{answer}");

    // The network is its to use unless it is told otherwise.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let reach = format!(
        "import socket; socket.create_connection(('127.0.0.1', {}), 2)",
        listener.local_addr().unwrap().port()
    );
    if Command::new("python3").arg("--version").output().is_ok_and(|out| out.status.success()) {
        assert!(kito(root, &["lane", "run", "1", "--", "python3", "-c", &reach]).status.success());
        let cut = kito(root, &["lane", "run", "1", "--no-network", "--", "python3", "-c", &reach]);
        assert!(!cut.status.success());
        assert!(String::from_utf8_lossy(&cut.stderr).contains("Permission"), "{cut:?}");
    }

    // What it wrote in the lane is the lane's, like any other save.
    ok(root, &["sync", "1"]);
    assert!(ok(root, &["lane", "status", "1"]).contains("made.txt"));
    ok(root, &["daemon", "stop"]);
}

#[test]
fn lanes_asked_for_together_are_checked_in_one_run() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let root = base.join("shop");
    fs::create_dir(&root).unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    // A check that takes a moment and counts its runs where it may write
    // even when it is confined: in the cache the checks have for themselves.
    let config = root.join(".kito/config.toml");
    let text = fs::read_to_string(&config).unwrap().replace(
        "# commands = [\"npx tsc --noEmit\", \"npm test\"]",
        "commands = [\"echo run >> ${XDG_CACHE_HOME:-..}/runs.txt; sleep 0.3; ! grep -rq BROKEN docs\"]",
    );
    fs::write(&config, text).unwrap();
    ok(root, &["daemon", "stop"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("docs")).unwrap();
    for name in ["a", "b", "c", "d"] {
        fs::write(root.join(format!("docs/{name}.txt")), "one\n").unwrap();
    }
    ok(root, &["checkpoint", "-m", "start"]);
    let runs = || {
        let confined = root.join(".kito/verify/cache/runs.txt");
        let plain = root.join(".kito/verify/runs.txt");
        fs::read_to_string(confined)
            .or_else(|_| fs::read_to_string(plain))
            .unwrap_or_default()
            .lines()
            .count()
    };

    for (lane, name) in ["a", "b", "c"].iter().enumerate() {
        let file = format!("docs/{name}.txt");
        ok(root, &["lane", "open", name, "--intent", "two", "--scope", &file]);
        fs::write(root.join(format!(".kito/lanes/{}/work/{file}", lane + 1)), "two\n").unwrap();
    }
    // Three lanes in one command: one run of the check, three checkpoints.
    let done = ok(root, &["integrate", "1", "2", "3"]);
    assert_eq!(runs(), 1, "{done}");
    assert!(done.contains("lane/1: integrated as ckpt#2"), "{done}");
    assert!(done.contains("lane/3: integrated as ckpt#4"), "{done}");
    assert_eq!(done.matches("together with 2 other lanes").count(), 3, "{done}");
    for name in ["a", "b", "c"] {
        assert_eq!(fs::read_to_string(root.join(format!("docs/{name}.txt"))).unwrap(), "two\n");
    }

    // Asked by three commands at the same time: the first is checked as
    // it comes, the two that ask while it runs are checked together.
    for (lane, name) in ["a", "b", "c"].iter().enumerate() {
        fs::write(root.join(format!(".kito/lanes/{}/work/docs/{name}.txt", lane + 1)), "three\n").unwrap();
    }
    let asking: Vec<_> = ["1", "2", "3"]
        .iter()
        .map(|lane| {
            let (root, lane) = (root.to_owned(), (*lane).to_owned());
            std::thread::spawn(move || ok(&root, &["integrate", &lane]))
        })
        .collect();
    let said: Vec<String> = asking.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert!(said.iter().all(|out| out.contains("integrated as")), "{said:?}");
    assert!(runs() <= 3, "at most two more runs for three lanes: {}", runs());

    // One of several fails: it is found, and the others go in.
    fs::write(root.join(".kito/lanes/1/work/docs/a.txt"), "four\n").unwrap();
    fs::write(root.join(".kito/lanes/2/work/docs/b.txt"), "BROKEN\n").unwrap();
    fs::write(root.join(".kito/lanes/3/work/docs/c.txt"), "four\n").unwrap();
    let mixed = kito(root, &["integrate", "1", "2", "3"]);
    assert!(!mixed.status.success());
    let mixed = String::from_utf8_lossy(&mixed.stdout).into_owned();
    assert!(mixed.contains("lane/1: integrated") && mixed.contains("lane/3: integrated"), "{mixed}");
    assert!(mixed.contains("lane/2: verification failed"), "{mixed}");
    assert_eq!(fs::read_to_string(root.join("docs/b.txt")).unwrap(), "three\n");
    ok(root, &["daemon", "stop"]);
}

#[test]
fn a_staging_lane_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let root = base.join("app");
    let remote = base.join("remote.git");
    fs::create_dir(&root).unwrap();
    let root = root.as_path();
    git(&base, &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    ok(root, &["init"]);
    let config = root.join(".kito/config.toml");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("# push = \"origin\"", &format!("push = {:?}", remote.to_str().unwrap()));
    fs::write(&config, text).unwrap();
    ok(root, &["daemon", "stop"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/a.txt"), "v1\n").unwrap();
    ok(root, &["checkpoint", "-m", "start"]);

    let created = ok(root, &["lane", "create", "staging"]);
    assert!(created.contains("staging (lane/1) created"), "{created}");
    assert!(ok(root, &["lanes"]).contains("permanent: staging"));
    ok(
        root,
        &[
            "lane",
            "open",
            "alice",
            "--intent",
            "the second version of a",
            "--scope",
            "docs/a.txt",
            "--on",
            "staging",
        ],
    );
    fs::write(root.join(".kito/lanes/2/work/docs/a.txt"), "v2\n").unwrap();
    let landed = ok(root, &["integrate", "2"]);
    assert!(landed.contains("lane/2: landed on staging as ckpt#2"), "{landed}");
    assert_eq!(fs::read_to_string(root.join("docs/a.txt")).unwrap(), "v1\n");
    // Its branch reaches the remote.
    let mut at_remote = String::new();
    for _ in 0..200 {
        let out = Command::new("git")
            .args(["-C", remote.to_str().unwrap(), "rev-parse", "staging"])
            .output()
            .unwrap();
        if out.status.success() {
            at_remote = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(at_remote, git(root, &["rev-parse", "staging"]).trim(), "{}", ok(root, &["status"]));
    let refused = kito(root, &["lane", "close", "staging"]);
    assert!(!refused.status.success());

    fs::write(root.join("docs/b.txt"), "direct\n").unwrap();
    ok(root, &["checkpoint", "-m", "b"]);
    let caught = ok(root, &["lane", "catch-up", "staging"]);
    assert!(caught.contains("staging caught up with main: docs/b.txt"), "{caught}");
    let promoted = ok(root, &["integrate", "staging"]);
    assert!(promoted.contains("lane/1: integrated as"), "{promoted}");
    assert_eq!(fs::read_to_string(root.join("docs/a.txt")).unwrap(), "v2\n");
    assert_eq!(
        git(root, &["log", "-1", "--format=%P", "main"]).split(' ').count(),
        2,
        "a merge: {}",
        git(root, &["log", "--graph", "--all", "--format=%h %s"])
    );
    ok(root, &["daemon", "stop"]);
}

#[test]
fn lanes_on_lanes_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let root = base.join("app");
    fs::create_dir(&root).unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/api.txt"), "v1\n").unwrap();
    fs::write(root.join("docs/ui.txt"), "v1\n").unwrap();
    ok(root, &["checkpoint", "-m", "start"]);

    ok(root, &["lane", "open", "alice", "--intent", "the api", "--scope", "docs/api.txt"]);
    fs::write(root.join(".kito/lanes/1/work/docs/api.txt"), "v2\n").unwrap();
    let opened =
        ok(root, &["lane", "open", "bob", "--intent", "the page", "--scope", "docs/ui.txt", "--on", "1"]);
    assert!(opened.contains("lane/2 opened for bob on lane/1, which it lands on."), "{opened}");
    assert_eq!(fs::read_to_string(root.join(".kito/lanes/2/work/docs/api.txt")).unwrap(), "v2\n");
    let late = ok(
        root,
        &[
            "lane",
            "open",
            "carol",
            "--intent",
            "the tests of the docs",
            "--scope",
            "docs/t.txt",
            "--on",
            "1",
            "--target",
            "main",
        ],
    );
    assert!(late.contains("it lands on main once lane/1 has"), "{late}");
    let lanes = ok(root, &["lanes"]);
    assert!(lanes.contains("on lane/1, lands on main") && lanes.contains("on lane/1 "), "{lanes}");

    fs::write(root.join(".kito/lanes/2/work/docs/ui.txt"), "v2\n").unwrap();
    let landed = ok(root, &["integrate", "2"]);
    assert!(landed.contains("lane/2: landed on lane/1"), "{landed}");
    assert_eq!(fs::read_to_string(root.join(".kito/lanes/1/work/docs/ui.txt")).unwrap(), "v2\n");
    assert_eq!(fs::read_to_string(root.join("docs/ui.txt")).unwrap(), "v1\n");

    fs::write(root.join(".kito/lanes/3/work/docs/t.txt"), "cases\n").unwrap();
    let waits = kito(root, &["integrate", "3"]);
    assert!(!waits.status.success());
    let waits = String::from_utf8_lossy(&waits.stdout).into_owned();
    assert!(waits.contains("integrate lane/1 first"), "{waits}");

    let both = ok(root, &["integrate", "1"]);
    assert!(both.contains("lane/1: integrated as ckpt#2"), "{both}");
    assert_eq!(fs::read_to_string(root.join("docs/ui.txt")).unwrap(), "v2\n");
    let last = ok(root, &["integrate", "3"]);
    assert!(last.contains("lane/3: integrated as ckpt#3"), "{last}");
    assert_eq!(fs::read_to_string(root.join("docs/t.txt")).unwrap(), "cases\n");
    ok(root, &["daemon", "stop"]);
}

#[test]
fn commits_made_with_git_are_pulled_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let (root, remote, clone) = (base.join("shop"), base.join("remote.git"), base.join("grace"));
    fs::create_dir(&root).unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    // With no remote there is nowhere to bring commits from.
    let none = kito(root, &["pull"]);
    assert!(!none.status.success());
    assert!(String::from_utf8_lossy(&none.stderr).contains("no git remote to bring commits from"));

    git(&base, &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    let config = root.join(".kito/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    let pushing = text.replace("# push = \"origin\"", &format!("push = {:?}", remote.to_str().unwrap()));
    assert_ne!(pushing, text, "{text}");
    fs::write(&config, pushing).unwrap();
    ok(root, &["daemon", "stop"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::write(root.join("app.py"), "def a():\n    return 1\n").unwrap();
    ok(root, &["checkpoint", "-m", "start"]);
    let at_remote = |what: &dyn Fn(&str) -> bool| {
        for _ in 0..500 {
            let out = Command::new("git")
                .current_dir(&remote)
                .args(["rev-list", "--parents", "-n", "1", "main"])
                .output();
            if out.is_ok_and(|out| out.status.success() && what(String::from_utf8_lossy(&out.stdout).trim()))
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the remote never got there");
    };
    ok(root, &["pull"]);
    at_remote(&|line| !line.is_empty());
    assert!(ok(root, &["pull"]).contains("nothing to bring in from"));

    // Grace commits with git and pushes.
    git(&base, &["clone", "-q", remote.to_str().unwrap(), clone.to_str().unwrap()]);
    fs::write(clone.join("NOTES.md"), "made with git\n").unwrap();
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "a note"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    let theirs = git(&clone, &["rev-parse", "HEAD"]).trim().to_owned();

    let pulled = ok(root, &["pull"]);
    assert!(
        pulled.contains("1 commit of") && pulled.contains("merged into the main lane as ckpt#2"),
        "{pulled}"
    );
    assert_eq!(fs::read_to_string(root.join("NOTES.md")).unwrap(), "made with git\n");
    // It went back as a merge with her commit, which the remote takes.
    at_remote(&|line| line.split_whitespace().nth(2) == Some(theirs.as_str()));
    let log = ok(root, &["log", "-n", "3"]);
    assert!(log.contains("◆ ckpt#2  merge of commits made with git: a note"), "{log}");
    // Why the file is there: her commit says, under her name.
    let why = ok(root, &["why", "NOTES.md"]);
    assert!(why.contains("a note") && why.contains("by Ada"), "{why}");
    assert!(ok(root, &["lanes"]).contains("No open lanes"), "the lane that brought it is closed");
    ok(root, &["daemon", "stop"]);
}

#[test]
fn a_protected_main_lane_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.py"), SHOP_PY).unwrap();
    ok(root, &["checkpoint", "-m", "start"]);
    assert!(ok(root, &["status"]).contains("Main lane ......... open"));

    let said = ok(root, &["protect"]);
    assert!(
        said.contains("The main lane is protected: it changes only when a lane is integrated."),
        "{said}"
    );
    assert!(ok(root, &["protect"]).contains("was protected already"));
    let status = ok(root, &["status"]);
    assert!(status.contains("Main lane ......... protected"), "{status}");
    assert!(status.contains("Checks ............ none"), "{status}");

    // A file edited in the main directory is put back, and the edit kept.
    fs::write(root.join("src/shop.py"), "edited where nobody should\n").unwrap();
    ok(root, &["sync"]);
    assert_eq!(fs::read_to_string(root.join("src/shop.py")).unwrap(), SHOP_PY);
    let status = ok(root, &["status"]);
    assert!(
        status.contains("1 direct edit put back since the daemon started; the last: src/shop.py, kept in"),
        "{status}"
    );
    let aside = fs::read_dir(root.join(".kito/refused")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(fs::read_to_string(aside.join("src/shop.py")).unwrap(), "edited where nobody should\n");
    assert!(json(root, &["status", "--json"])["repo"]["protected"].as_bool().unwrap());

    // What would change the main lane from outside a lane says no.
    for (args, why) in [
        (&["rollback", "ckpt#1"][..], "the main lane is protected"),
        (&["checkpoint", "-m", "by hand"], "every integration is a checkpoint"),
        (&["claude"], "the set-up writes files in the main directory"),
    ] {
        let out = kito(root, args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(why),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // A lane is the way in.
    ok(root, &["lane", "open", "alice", "--intent", "prices in cents", "--scope", "src/shop.py:total"]);
    let cents = SHOP_PY.replace("price * quantity", "round(price * 100) * quantity");
    fs::write(root.join(".kito/lanes/1/work/src/shop.py"), &cents).unwrap();
    assert!(ok(root, &["integrate", "1"]).contains("lane/1: integrated as ckpt#2"));
    assert_eq!(fs::read_to_string(root.join("src/shop.py")).unwrap(), cents);

    // Turned off, the main directory is edited by hand as before.
    assert!(ok(root, &["protect", "--off"]).contains("The main lane is open again"));
    fs::write(root.join("src/shop.py"), SHOP_PY).unwrap();
    assert!(ok(root, &["checkpoint", "-m", "by hand"]).contains("ckpt#3"));
    ok(root, &["daemon", "stop"]);
}

/// The server binary sits next to the CLI's; built if it is not there.
fn kito_server() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let path = Path::new(env!("CARGO_BIN_EXE_kito")).with_file_name("kito-server");
    BUILD.call_once(|| {
        if !path.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "kito-server", "--bin", "kito-server"])
                .status()
                .unwrap();
            assert!(status.success(), "building kito-server");
        }
    });
    path
}

#[test]
fn a_directory_connected_to_a_server_shows_the_repository_kept_there() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let serverdir = base.join("server");
    let run = |args: &[&str]| -> String {
        let out = Command::new(kito_server())
            .arg("--dir")
            .arg(&serverdir)
            .args(args)
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", base.join(".absent-personal-config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "kito-server {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // A git repository for the server to host.
    let src = base.join("src-repo");
    fs::create_dir_all(src.join("src")).unwrap();
    fs::write(src.join("src/shop.py"), SHOP_PY).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "start"]);
    run(&["init", serverdir.to_str().unwrap()]);
    run(&["repo", "add", "shop", "--from", src.to_str().unwrap()]);
    let token = run(&["user", "add", "ada"]);
    let token = token.lines().find(|line| line.starts_with("kito_")).unwrap().trim().to_owned();
    /// The server, stopped when the test ends, however it ends.
    struct Serving(std::process::Child);
    impl Drop for Serving {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut serving = Serving(
        Command::new(kito_server())
            .arg("--dir")
            .arg(&serverdir)
            .args(["serve", "--listen", "127.0.0.1:0"])
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", base.join(".absent-personal-config"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(serving.0.stdout.take().unwrap()), &mut line)
        .unwrap();
    let address =
        line.trim().strip_prefix("kito-server: listening on ").unwrap().split(',').next().unwrap().to_owned();

    // A token the server does not know connects nothing, and leaves nothing.
    let refused = kito(&base, &["connect", &address, "shop", "nope", "--token", "kito_wrong"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("this token is not one the server knows"));
    assert!(!base.join("nope").exists());

    // The connection is encrypted. Another certificate than the one that
    // was given with the token is not the server.
    let wrong = format!("sha256:{}", "0f".repeat(32));
    let refused =
        kito(&base, &["connect", &address, "shop", "nope", "--token", &token, "--fingerprint", &wrong]);
    assert!(!refused.status.success());
    let said = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(said.contains("not the one this directory was connected with"), "{said}");
    assert!(!base.join("nope").exists());

    let connected = ok(&base, &["connect", &address, "shop", "--token", &token]);
    assert!(connected.contains("is connected to `shop` on") && connected.contains("as ada."), "{connected}");
    let here = base.join("shop");
    // A server on this machine is taken at its word the first time, and
    // held to it from then on: the directory keeps its fingerprint.
    let print = run(&["fingerprint"]).trim().to_owned();
    let kept = fs::read_to_string(here.join(".kito/remote.toml")).unwrap();
    assert!(kept.contains(&format!("fingerprint = \"{print}\"")), "{kept}");
    // The directory is the project: the main lane's files are in it.
    assert!(connected.contains("The main lane is in it (1 file)"), "{connected}");
    assert_eq!(fs::read_to_string(here.join("src/shop.py")).unwrap(), SHOP_PY);
    assert_eq!(ok(&here, &["repos"]), "shop  (this directory)\n");
    assert!(ok(&here, &["lanes"]).contains("No open lanes"));

    // The lane is opened on the server, for the person the token belongs to.
    let opened = ok(
        &here,
        &["lane", "open", "builder", "--intent", "prices in cents", "--scope", "src/shop.py:total"],
    );
    assert!(opened.contains("lane/1 opened for builder"), "{opened}");
    let board = ok(&here, &["board"]);
    assert!(board.contains("lane/1  builder (of ada)  active  prices in cents"), "{board}");
    assert!(serverdir.join("repos/shop/.kito/lanes/1/work/src/shop.py").is_file());
    // What happened is there for whoever watches.
    let watched = ok(&here, &["watch", "--past", "-n", "1"]);
    assert!(watched.contains("lane/1") && watched.contains("lane opened  by ada"), "{watched}");
    assert!(watched.contains("builder: prices in cents [src/shop.py:total]"), "{watched}");
    assert!(ok(&here, &["log"]).contains("src/shop.py"));

    // What belongs to whoever runs the server stays there; and a directory
    // connected to a server is no repository of its own.
    let checkpoint = kito(&here, &["checkpoint", "-m", "from afar"]);
    assert!(!checkpoint.status.success());
    assert!(String::from_utf8_lossy(&checkpoint.stderr)
        .contains("not something a client can ask through the server"));
    let init = kito(&here, &["init"]);
    assert!(String::from_utf8_lossy(&init.stderr).contains("is connected to `shop`"));

    // The lane's working copy is on this machine: what is saved in it is
    // in the lane on the server once `sync` has answered.
    let there = serverdir.join("repos/shop/.kito/lanes/1/work");
    let work = here.join(".kito/lanes/1/work");
    assert!(opened.contains(&format!("Working copy: {}", work.display())), "{opened}");
    assert_eq!(fs::read_to_string(work.join("src/shop.py")).unwrap(), SHOP_PY);
    let cents = SHOP_PY.replace("price * quantity", "round(price * 100) * quantity");
    assert_ne!(cents, SHOP_PY);
    fs::write(work.join("src/shop.py"), &cents).unwrap();
    ok(&here, &["sync", "1"]);
    assert_eq!(fs::read_to_string(there.join("src/shop.py")).unwrap(), cents);
    let status = ok(&here, &["lane", "status", "1"]);
    assert!(status.contains(&format!("Working copy ...... {}", work.display())), "{status}");
    let daemon = ok(&here, &["daemon", "status"]);
    assert!(
        daemon.contains("Connected to ...... `shop` on") && daemon.contains("Lanes kept here ... 2"),
        "{daemon}"
    );
    assert!(daemon.contains("lane/main"), "{daemon}");
    assert!(daemon.contains("lane/1") && daemon.contains("builder") && daemon.contains("writes"), "{daemon}");

    // From another machine the lane is followed: its files are there, and
    // change as the lane does.
    ok(&base, &["connect", &address, "shop", "desk", "--token", &token]);
    let desk = base.join("desk");
    let status = ok(&desk, &["lane", "status", "1"]);
    assert!(status.contains("on the server (`kito lane attach lane/1` brings its files here)"), "{status}");
    let attached = ok(&desk, &["lane", "attach", "1"]);
    let view = desk.join(".kito/lanes/1/work");
    assert!(attached.contains(&format!("lane/1 is here: {}", view.display())), "{attached}");
    assert!(attached.contains("It follows the lane"), "{attached}");
    assert_eq!(fs::read_to_string(view.join("src/shop.py")).unwrap(), cents);
    let euros = cents.replace("* 100", "* 100.0");
    fs::write(work.join("src/shop.py"), &euros).unwrap();
    ok(&here, &["sync", "1"]);
    for _ in 0..2000 {
        if fs::read_to_string(view.join("src/shop.py")).unwrap() == euros {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(fs::read_to_string(view.join("src/shop.py")).unwrap(), euros);

    // A file changed in a copy that only follows is kept, and said; the
    // server's is taken back with one command.
    fs::write(view.join("src/shop.py"), "mine\n").unwrap();
    fs::write(work.join("src/shop.py"), &cents).unwrap();
    ok(&here, &["sync", "1"]);
    let mut held = String::new();
    for _ in 0..2000 {
        held = ok(&desk, &["daemon", "status"]);
        if held.contains("this copy only follows") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(held.contains("src/shop.py was changed here and is not sent: this copy only follows"), "{held}");
    assert!(held.contains("follows"), "{held}");
    let taken = ok(&desk, &["lane", "take", "1", "src/shop.py", "--theirs"]);
    assert_eq!(taken, "src/shop.py: the server's is here now\n");
    assert_eq!(fs::read_to_string(view.join("src/shop.py")).unwrap(), cents);
    let detached = ok(&desk, &["lane", "detach", "1"]);
    assert!(detached.contains("This machine's copy of lane/1 is gone"), "{detached}");
    assert!(!view.exists());

    // It integrates from here as it would on the server.
    let integrated = ok(&here, &["integrate", "1"]);
    assert!(integrated.contains("integrated"), "{integrated}");
    assert_eq!(fs::read_to_string(serverdir.join("repos/shop/src/shop.py")).unwrap(), cents);
    // Both directories follow the main lane: they have what was integrated.
    for dir in [&here, &desk] {
        for _ in 0..2000 {
            if fs::read_to_string(dir.join("src/shop.py")).unwrap() == cents {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(fs::read_to_string(dir.join("src/shop.py")).unwrap(), cents);
    }
    ok(&here, &["lane", "close", "1"]);
    assert!(!work.exists());
    let left = ok(&desk, &["lane", "detach", "main"]);
    assert!(left.contains("no longer follows the main lane; its files stay"), "{left}");
    assert!(desk.join("src/shop.py").is_file());

    // Copies of lanes are for directories connected to a server.
    let own = base.join("own");
    fs::create_dir_all(&own).unwrap();
    ok(&own, &["init"]);
    let attach = kito(&own, &["lane", "attach", "1"]);
    assert!(String::from_utf8_lossy(&attach.stderr).contains("is a repository of its own"));
    ok(&own, &["daemon", "stop"]);

    // Connected as it is for somebody who did not ask otherwise, the
    // directory is set up for Claude Code; there is no git to keep out of.
    let said = kito_as_installed(&base, &["connect", &address, "shop", "claude-dir", "--token", &token]);
    assert!(
        said.contains(".mcp.json") && said.contains("CLAUDE.md") && !said.contains(".gitignore"),
        "{said}"
    );
    assert!(base.join("claude-dir/.mcp.json").is_file() && base.join("claude-dir/CLAUDE.md").is_file());
    assert!(!base.join("claude-dir/.gitignore").exists());
    assert!(base.join("claude-dir/src/shop.py").is_file(), "the project's files come with it");
    ok(&base.join("claude-dir"), &["daemon", "stop"]);

    // The rules of a team. Bob may write and not integrate: his lane is
    // merged and checked, and waits for ada, who may.
    let bob = run(&["user", "add", "bob", "--role", "write"]);
    let bob = bob.lines().find(|line| line.starts_with("kito_")).unwrap().trim().to_owned();
    ok(&base, &["connect", &address, "shop", "bob-dir", "--token", &bob]);
    let bobdir = base.join("bob-dir");
    ok(&bobdir, &["lane", "open", "helper", "--intent", "a note at the end", "--scope", "src/shop.py"]);
    fs::write(bobdir.join(".kito/lanes/2/work/src/shop.py"), format!("{cents}# prices are in cents\n"))
        .unwrap();
    let asked = ok(&bobdir, &["integrate", "2"]);
    assert!(asked.contains("lane/2: merged and checked; it waits for somebody who may integrate"), "{asked}");
    assert!(asked.contains("`kito integrate lane/2`"), "{asked}");
    assert!(ok(&here, &["lanes"]).contains("awaiting approval"));
    let board = ok(&here, &["board"]);
    assert!(
        board.contains("lane/2  helper (of bob)  awaiting approval (asked by bob)  a note at the end"),
        "{board}"
    );
    let brought = ok(&here, &["integrate", "2"]);
    assert!(brought.contains("lane/2: integrated as"), "{brought}");
    assert!(fs::read_to_string(serverdir.join("repos/shop/src/shop.py"))
        .unwrap()
        .ends_with("# prices are in cents\n"));
    // The rules of a repository on a server are set on the server.
    let protect = kito(&here, &["protect"]);
    assert!(String::from_utf8_lossy(&protect.stderr).contains("kito-server repo set <name> --protect"));
    ok(&bobdir, &["lane", "close", "2"]);
    ok(&bobdir, &["daemon", "stop"]);

    ok(&here, &["daemon", "stop"]);
    ok(&desk, &["daemon", "stop"]);
    drop(serving);
    ok(&serverdir.join("repos/shop"), &["daemon", "stop"]);
}
