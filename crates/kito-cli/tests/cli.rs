//! The real CLI (`kito`), with the real daemon it starts by itself.

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
    let status = ok(root, &["daemon", "status"]);
    assert!(status.contains("the operating system did not report 1 change"), "{status}");
    assert!(status.contains("full scan every") && !status.contains("full scan every 60 s"), "{status}");
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
    let why = ok(&root.join("src"), &["why", "users.ts:fetchUser"]);
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

    let histories = json(root, &["why", "src/users.ts:fetchUser", "--json"]);
    assert_eq!(histories.as_array().unwrap().len(), 1);
    assert_eq!(histories[0]["steps"].as_array().unwrap().len(), 4);
    assert_eq!(histories[0]["name"], "fetchUser");
    let id = histories[0]["id"].as_str().unwrap().to_owned();
    // The method was never touched: one step, and a different symbol.
    let method = json(root, &["why", "src/auth.ts:Session.isValid", "--json"]);
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
    let gone = kito(root, &["why", "src/auth.ts:fetchUser"]);
    assert!(!gone.status.success());
    let message = String::from_utf8_lossy(&gone.stderr).into_owned();
    assert!(
        message.contains("no symbol named `fetchUser`") && message.contains("Session.isValid"),
        "{message}"
    );
    let malformed = kito(root, &["why", "fetchUser"]);
    assert!(!malformed.status.success());
    ok(root, &["daemon", "stop"]);
}

#[test]
fn lanes_integration_and_conflicts_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
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
    assert!(ok(root, &["why", "src/shop.ts:discount", "--lane", "lane/1"]).contains("body changed"));

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
    assert!(ok(root, &["why", "src/shop.ts:grandTotal"]).contains("renamed `total` → `grandTotal`"));
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
    let base = tmp.path().canonicalize().unwrap();
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

    let connected = ok(&base, &["connect", &address, "shop", "--token", &token]);
    assert!(connected.contains("is connected to `shop` on") && connected.contains("as ada."), "{connected}");
    let here = base.join("shop");
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

    drop(serving);
    ok(&serverdir.join("repos/shop"), &["daemon", "stop"]);
}
