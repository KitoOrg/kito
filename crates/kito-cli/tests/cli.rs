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
    let path = Path::new(env!("CARGO_BIN_EXE_kito"))
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

/// Kito alone, as a person whose own Kito directory is `config`: the
/// servers they are connected to are kept there, whatever directory
/// they are in.
fn kito_as(config: &Path, cwd: &Path, args: &[&str]) -> Output {
    command(cwd, args).env("KITO_NO_CLAUDE", "1").env("KITO_CONFIG_DIR", config).output().unwrap()
}

/// As `ok`, for the person whose Kito directory is `config`.
fn ok_as(config: &Path, cwd: &Path, args: &[&str]) -> String {
    let out = kito_as(config, cwd, args);
    assert!(
        out.status.success(),
        "kito {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
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
    let rules = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(
        rules.starts_with("<!-- kito: begin (written by Kito") && rules.contains("`open_lane`"),
        "{rules}"
    );

    // What the assistant reads is not the project's (ADR 0012): out of the
    // history and of git; `.gitignore` is the project's, and says so.
    ok(root, &["sync"]);
    assert_eq!(json(root, &["status", "--json"])["repo"]["files"], 2);
    ok(root, &["checkpoint", "-m", "start"]);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    assert_eq!(git(root, &["ls-files"]), ".gitignore\napp.ts\n");
    let ignored = fs::read_to_string(root.join(".gitignore")).unwrap();
    assert!(ignored.contains("/CLAUDE.md\n") && ignored.contains("/.claude/\n"), "{ignored}");

    // Asked again, there is nothing to do.
    let again = kito_as_installed(root, &["assistant", "add", "claude"]);
    assert!(again.contains("set up here already"), "{again}");
    let again = json(root, &["assistant", "add", "claude", "--json"]);
    let files = again[0]["said"]["files"].as_array().unwrap();
    assert!(files.iter().all(|file| file["change"] == "unchanged"), "{again}");
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
    let out = kito_as_installed(root, &["assistant", "add", "claude"]);
    for (file, change) in [
        (".gitignore", "updated"),
        (".mcp.json", "created"),
        (".claude/settings.json", "created"),
        ("CLAUDE.md", "updated"),
    ] {
        assert!(out.contains(&format!("{change:<10}  {file}")), "{change} {file} in:\n{out}");
    }
    assert!(fs::read_to_string(root.join(".gitignore")).unwrap().starts_with("target/\n.kito/\n# Read by"));
    // What the project's own CLAUDE.md said is an instruction now, for
    // everyone who works here; the file is written from them.
    assert!(out.contains("what CLAUDE.md said is the instruction claude/project.md now"), "{out}");
    let told = json(root, &["instructions", "--json"]);
    assert_eq!(told["items"][0]["name"], "claude/project.md", "{told}");
    assert_eq!(told["items"][0]["text"], "# Notes\n");
    let rules = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(rules.starts_with("<!-- kito: begin") && rules.contains("\n# Notes\n"), "{rules}");
    // It was in the history: it stays there, as any file that was.
    ok(root, &["sync"]);
    assert_eq!(json(root, &["status", "--json"])["repo"]["files"], 2);
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
    assert!(out.contains("Set up for Claude Code:"), "{out}");
    let imported = json(other, &["status", "--json"]);
    assert_eq!(imported["repo"]["files"], 2);
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

/// A hook run by an assistant that marks, or does not, what it runs.
fn hook_in(cwd: &Path, args: &[&str], input: &Value, claude_code: bool) -> (Value, i32, String) {
    use std::io::Write;
    let mut command = command(cwd, args);
    match claude_code {
        true => command.env("CLAUDECODE", "1"),
        false => command.env_remove("CLAUDECODE"),
    };
    let mut child = command
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
    if !held.contains("this edit was held back once") {
        // Seen under load, and not explained yet: what Kito knew then.
        let said = |args: &[&str]| {
            let out = kito(root, args);
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
        };
        panic!(
            "the edit was let through: {held:?}\n--- why src/shop.ts:4 in lane 2\n{}\n--- lane status\n{}\n--- log\n{}",
            said(&["why", "src/shop.ts:4", "--lane", "2"]),
            said(&["lane", "status", "2"]),
            said(&["log", "-v"]),
        );
    }
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
    // On a busy machine the test takes longer than those 40 ms to get from
    // the write to the hook, and Kito is right to say that the write was
    // not the command's: written again until the hook is that quick.
    let told_after = |write: &dyn Fn(usize)| -> Value {
        for attempt in 0..40 {
            write(attempt);
            let (answer, code, _) = hook(root, &["hook", "after-shell"], &ran);
            assert_eq!(code, 0);
            if !answer.is_null() {
                return answer;
            }
        }
        panic!("a command that wrote where it should not have was never told so");
    };
    let answer = told_after(&|attempt| {
        fs::write(work.join("README.md"), format!("# The shop\n{}", "\n".repeat(attempt))).unwrap()
    });
    assert_eq!(answer["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let told = answer["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(told.starts_with("Kito: lane/2 (bob) has changes outside what it declared: README.md"), "{told}");
    // A command that changed nothing where it should not have has nothing to add.
    fs::write(work.join("README.md"), "# Shop\n").unwrap();
    let (answer, code, _) = hook(root, &["hook", "after-shell"], &ran);
    assert_eq!((answer, code), (Value::Null, 0));
    // One that wrote in the main directory is told so.
    let answer = told_after(&|attempt| {
        let text = format!("# Shop\n\nWritten where it stands.\n{}", "\n".repeat(attempt));
        fs::write(root.join("README.md"), text).unwrap();
        ok(root, &["sync"]);
    });
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
fn after_a_command_the_first_import_is_nobodys_edit_and_an_edit_is_told_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    // A project that was there before Kito, its files written a moment ago.
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/a.py"), "def one():\n    return 1\n").unwrap();
    fs::write(root.join("README.md"), "hello\n").unwrap();
    ok(root, &["init"]);
    ok(root, &["sync"]);
    ok(root, &["checkpoint", "-m", "First import"]);
    assert_eq!(json(root, &["status", "--json"])["repo"]["files"], 2);

    // A command that changed nothing, as an assistant reports it (it does
    // not say how long it took): the files of the import are not its doing.
    let ran = |session: &str, command: &str| {
        let input = serde_json::json!({
            "session_id": session, "cwd": root, "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_input": { "command": command },
            "tool_response": { "stdout": "", "stderr": "", "interrupted": false },
        });
        let (answer, code, said) = hook(root, &["hook", "after-shell"], &input);
        assert_eq!(code, 0, "{said}");
        answer["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default().to_owned()
    };
    assert_eq!(ran("s", "ls"), "");
    assert_eq!(ran("s", "ls -l"), "");

    // An edit made by hand in the main directory is told to the session,
    // once: not again after every command that follows.
    fs::write(root.join("README.md"), "hello again\n").unwrap();
    ok(root, &["sync"]);
    let told = ran("s", "echo hello again > README.md");
    assert!(told.contains("files of the main directory were changed: README.md"), "{told}");
    assert!(!told.contains("src/a.py"), "{told}");
    assert_eq!(ran("s", "ls"), "");
    // Another session has not been told yet.
    assert!(ran("t", "ls").contains("README.md"));
    assert_eq!(ran("t", "ls"), "");
    // The next edit is told again.
    fs::write(root.join("README.md"), "hello once more\n").unwrap();
    ok(root, &["sync"]);
    assert!(ran("s", "echo hello once more > README.md").contains("README.md"));
    assert_eq!(ran("s", "ls"), "");
    ok(root, &["daemon", "stop"]);
}

#[test]
fn a_configuration_written_while_the_daemon_runs_is_the_one_in_force() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    fs::write(root.join("a.txt"), "one\n").unwrap();
    ok(root, &["init"]);
    ok(root, &["checkpoint", "-m", "base"]);
    assert_eq!(json(root, &["status", "--json"])["repo"]["checks"], serde_json::json!([]));

    // The checks are written into the file while the daemon runs: they
    // are the checks, with nothing restarted.
    let config = root.join(".kito/config.toml");
    let default = fs::read_to_string(&config).unwrap();
    let checked = default.replace(
        "# commands = [\"npx tsc --noEmit\", \"npm test\"]",
        "commands = [\"! grep -q FIXME a.txt\"]",
    );
    assert_ne!(checked, default);
    fs::write(&config, &checked).unwrap();
    let checks = serde_json::json!(["! grep -q FIXME a.txt"]);
    assert_eq!(json(root, &["status", "--json"])["repo"]["checks"], checks);
    let lane = json(root, &["lane", "open", "alice", "--intent", "a note", "--scope", "./", "--json"]);
    let work = PathBuf::from(lane["workdir"].as_str().unwrap());
    fs::write(work.join("a.txt"), "one\nFIXME\n").unwrap();
    let integrate = |args: &[&str]| -> (Value, String) {
        let out = kito(root, args);
        let printed = String::from_utf8_lossy(&out.stdout).into_owned();
        (serde_json::from_str(&printed).unwrap_or(Value::Null), printed)
    };
    let (result, _) = integrate(&["integrate", "1", "--json"]);
    assert_eq!(result[0]["result"], "verification_failed", "{result}");
    assert!(result[0].get("config_error").is_none(), "{result}");

    // The file is broken. It is said where somebody looks, and the checks
    // read before it broke still hold: never "no checks".
    fs::write(&config, checked.replace("commands = [\"! grep", "commands = [[\"! grep")).unwrap();
    let status = json(root, &["status", "--json"]);
    assert_eq!(status["repo"]["checks"], checks);
    let error = status["repo"]["config_error"].as_str().expect("the status says it").to_owned();
    assert!(error.contains("cannot be read") && error.contains("config.toml"), "{error}");
    let said = ok(root, &["status"]);
    assert!(said.contains("Configuration ..... the configuration file cannot be read"), "{said}");
    assert!(said.contains("Checks ............ ! grep -q FIXME a.txt"), "{said}");
    let (result, _) = integrate(&["integrate", "1", "--json"]);
    assert_eq!(result[0]["result"], "verification_failed", "{result}");
    assert_eq!(result[0]["config_error"], serde_json::json!(error), "{result}");
    let (_, printed) = integrate(&["integrate", "1"]);
    assert!(printed.contains("verification failed"), "{printed}");
    assert!(printed.contains("Configuration ..... the configuration file cannot be read"), "{printed}");

    // Put right, the error is gone. A setting the daemon reads only when
    // it starts is said to wait for that, until it has started again.
    fs::write(&config, checked.replace("watch = true", "watch = false")).unwrap();
    let status = json(root, &["status", "--json"]);
    assert!(status["repo"].get("config_error").is_none(), "{status}");
    assert_eq!(status["restart_needed"], serde_json::json!(["[capture] watch"]), "{status}");
    let said = ok(root, &["status"]);
    assert!(said.contains("Restart needed .... [capture] watch"), "{said}");
    ok(root, &["daemon", "stop"]);
    let status = json(root, &["status", "--json"]);
    assert!(status.get("restart_needed").is_none(), "{status}");
    assert_eq!(status["watcher"]["watching"], false);
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
fn notes_on_code_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let root = base.join("shop");
    fs::create_dir(&root).unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.py"), "def total(prices):\n    return sum(prices)\n").unwrap();
    ok(root, &["checkpoint", "-m", "start"]);

    let left = ok(
        root,
        &["note", "src/shop.py:total", "called", "by", "the", "nightly", "job:", "do", "not", "rename"],
    );
    assert!(
        left.contains("note#1  src/shop.py:total") && left.contains("called by the nightly job"),
        "{left}"
    );
    let why = ok(root, &["why", "src/shop.py:total"]);
    assert!(why.contains("Notes left on it:") && why.contains("do not rename"), "{why}");
    assert!(ok(root, &["note", "--list"]).contains("note#1"));
    assert!(ok(root, &["note", "--remove", "note#1"]).contains("note#1 removed."));
    assert_eq!(ok(root, &["note", "--list"]).trim(), "No notes.");
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
    let path = Path::new(env!("CARGO_BIN_EXE_kito"))
        .with_file_name(format!("kito-server{}", std::env::consts::EXE_SUFFIX));
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
    // Each person has their own Kito directory, where their servers are kept.
    // ada, rose and bob on one machine: their servers are kept in one Kito
    // directory, each account under a name of its own (`--as`).
    let ada = base.join(".me");
    let kito = |cwd: &Path, args: &[&str]| kito_as(&ada, cwd, args);
    let ok = |cwd: &Path, args: &[&str]| ok_as(&ada, cwd, args);
    // A git repository for the server to host.
    let src = base.join("src-repo");
    fs::create_dir_all(src.join("src")).unwrap();
    fs::write(src.join("src/shop.py"), SHOP_PY).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "start"]);
    run(&["init", serverdir.to_str().unwrap()]);
    run(&["repo", "add", "shop", "--from", src.to_str().unwrap()]);
    let token = run(&["user", "add", "ada", "--repo", "shop:integrate"]);
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
    // held to it from then on: ada's servers keep its fingerprint; the
    // directory names the server, and keeps no token.
    let print = run(&["fingerprint"]).trim().to_owned();
    let kept = fs::read_to_string(ada.join("servers.toml")).unwrap();
    assert!(kept.contains(&format!("fingerprint = \"{print}\"")), "{kept}");
    assert_eq!(
        fs::read_to_string(here.join(".kito/remote.toml")).unwrap(),
        format!("server = \"{address}\"\nrepo = \"shop\"\n")
    );
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
    // As the system writes it: on Windows with `\` all through.
    let work = here.join(".kito/lanes/1/work").components().collect::<PathBuf>();
    assert!(opened.contains(&format!("Working copy: {}", work.display())), "{opened}");
    assert_eq!(fs::read_to_string(work.join("src/shop.py")).unwrap(), SHOP_PY);
    let cents = SHOP_PY.replace("price * quantity", "round(price * 100) * quantity");
    assert_ne!(cents, SHOP_PY);
    fs::write(work.join("src/shop.py"), &cents).unwrap();
    ok(&here, &["sync", "1"]);
    assert_eq!(fs::read_to_string(there.join("src/shop.py")).unwrap(), cents);
    let status = ok(&here, &["lane", "status", "1"]);
    assert!(status.contains(&format!("Working copy ...... {}", work.display())), "{status}");
    // The diff of the lane is the server's to tell (ADR 0004).
    let diff = ok(&here, &["lane", "diff", "1"]);
    assert!(diff.contains("changed  src/shop.py  +1 -1\n  changed  total"), "{diff}");
    assert!(diff.contains("+    subtotal = round(price * 100) * quantity"), "{diff}");
    // So are its comments: said from here, kept there.
    let said = ok(&here, &["lane", "comment", "1", "Half a cent rounds up?", "--on", "src/shop.py:2"]);
    assert!(said.starts_with("comment#1 on src/shop.py:2 ("), "{said}");
    assert!(said.contains("  >     subtotal = round(price * 100) * quantity"), "{said}");
    assert!(ok(&here, &["lane", "status", "1"]).contains(
        "Comments:
  comment#1 on src/shop.py:2"
    ));
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
    let view = desk.join(".kito/lanes/1/work").components().collect::<PathBuf>();
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

    // An admin adds repositories from a connected directory: new, with a
    // README.md for lanes to start from, or sent whole from this machine,
    // a Kito repository or a git one.
    let rose = run(&["user", "add", "rose", "--role", "admin", "--repo", "shop:read"]);
    let rose = rose.lines().find(|line| line.starts_with("kito_")).unwrap().trim().to_owned();
    ok(&base, &["connect", &address, "shop", "admin", "--token", &rose, "--as", "rose"]);
    let admin = base.join("admin");
    let ok_rose = |cwd: &Path, args: &[&str]| ok(cwd, args);
    assert!(!kito(&here, &["repos", "add", "notes"]).status.success());
    assert!(ok_rose(&admin, &["repos", "add", "notes"]).contains("`notes` is on"));
    assert_eq!(fs::read_to_string(serverdir.join("repos/notes/README.md")).unwrap(), "# notes\n");
    let mine = base.join("mine");
    fs::create_dir_all(&mine).unwrap();
    fs::write(mine.join("notes.py"), "def note():\n    return 1\n").unwrap();
    ok(&mine, &["init"]);
    ok(&mine, &["checkpoint", "-m", "a note"]);
    assert!(ok_rose(&admin, &["repos", "add", "pushed", "--push", mine.to_str().unwrap()])
        .contains("`pushed` is on"));
    ok(&mine, &["daemon", "stop"]);
    assert!(fs::read_to_string(serverdir.join("repos/pushed/notes.py")).unwrap().contains("note"));
    assert!(serverdir.join("repos/pushed/.kito/format").is_file());
    ok_rose(&admin, &["repos", "add", "fromgit", "--push", src.to_str().unwrap()]);
    assert_eq!(fs::read_to_string(serverdir.join("repos/fromgit/src/shop.py")).unwrap(), SHOP_PY);
    ok_rose(&admin, &["repos", "rename", "fromgit", "shop2"]);
    ok_rose(&admin, &["repos", "archive", "shop2"]);
    let all = ok_rose(&admin, &["repos", "all"]);
    assert!(all.contains("pushed") && all.contains("shop2                archived by rose"), "{all}");
    assert!(all.contains("fromgit              is shop2 now"), "{all}");
    assert!(!kito(&admin, &["repos", "delete", "shop2", "--confirm", "shop2"]).status.success());
    // An archived repository's name is its still.
    let taken = kito(&admin, &["repos", "add", "shop2"]);
    let said = String::from_utf8_lossy(&taken.stderr).into_owned();
    assert!(!taken.status.success() && said.contains("an archived repository is called `shop2`"), "{said}");
    // This server hears which of two is meant: it is asked, and answers
    // as it does to an admin.
    let which = kito(&admin, &["repos", "delete", "shop2", "--confirm", "shop2", "--archived"]);
    let said = String::from_utf8_lossy(&which.stderr).into_owned();
    assert!(!which.status.success() && said.contains("only an owner"), "{said}");

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
    let bob = run(&["user", "add", "bob", "--repo", "shop:write"]);
    let bob = bob.lines().find(|line| line.starts_with("kito_")).unwrap().trim().to_owned();
    ok(&base, &["connect", &address, "shop", "bob-dir", "--token", &bob, "--as", "bob"]);
    let bobdir = base.join("bob-dir");
    // Three accounts of one server on one machine, each directory as its own.
    let listed = ok(&base, &["server", "list"]);
    let lines: Vec<String> =
        listed.lines().map(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")).collect();
    assert_eq!(
        lines,
        [
            format!("{address} {address} as ada"),
            format!("* bob {address} as bob"),
            format!("rose {address} as rose"),
        ],
        "{listed}"
    );
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

#[test]
fn the_diff_of_a_lane_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    let shop = "export function total(prices: number[]): number {\n  let sum = 0;\n  for (const price of prices) {\n    sum += price;\n  }\n  return sum;\n}\n\nexport function discount(amount: number, percent: number): number {\n  return amount - (amount * percent) / 100;\n}\n";
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.ts"), shop).unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    ok(root, &["lane", "open", "alice", "--intent", "prices in cents", "--scope", "./"]);
    let work = root.join(".kito/lanes/1/work");

    assert_eq!(ok(root, &["lane", "diff", "1"]), "lane/1 onto main (base ckpt#1): 0 files, +0 -0\n");

    fs::write(work.join("src/shop.ts"), shop.replace("sum += price;", "sum += Math.round(price * 100);"))
        .unwrap();
    fs::write(work.join("NOTES.md"), "Prices are in cents.\n").unwrap();
    ok(root, &["rename", "src/shop.ts:discount", "rebate", "--lane", "1"]);

    // The diff is of the working copy as it is: nobody has to sync first.
    let diff = ok(root, &["lane", "diff", "1"]);
    let expected = "lane/1 onto main (base ckpt#1): 2 files, +3 -2

added    NOTES.md  +1 -0
@@ -0,0 +1,1 @@
+Prices are in cents.

changed  src/shop.ts  +2 -2
  changed  total (lines 1-7): \"prices in cents\"
  renamed  rebate (lines 9-11), was src/shop.ts:discount: \"prices in cents\"
@@ -1,11 +1,11 @@
 export function total(prices: number[]): number {
   let sum = 0;
   for (const price of prices) {
-    sum += price;
+    sum += Math.round(price * 100);
   }
   return sum;
 }
 
-export function discount(amount: number, percent: number): number {
+export function rebate(amount: number, percent: number): number {
   return amount - (amount * percent) / 100;
 }
";
    assert_eq!(diff, expected);

    // Without the lines; some paths only; as JSON.
    let stat = ok(root, &["lane", "diff", "1", "--stat"]);
    assert!(stat.contains("renamed  rebate") && !stat.contains("@@"), "{stat}");
    let one = ok(root, &["lane", "diff", "1", "NOTES.md"]);
    assert!(one.starts_with("lane/1 onto main (base ckpt#1): 1 file, +1 -0\n"), "{one}");
    let asked = json(root, &["lane", "diff", "1", "src/", "--json"]);
    assert_eq!(asked["files"][0]["symbols"][1]["was"], "src/shop.ts:discount");
    assert_eq!(asked["files"][0]["hunks"][0]["before"], serde_json::json!([1, 11]));

    // The main lane moves on: the lane is told what it has not seen.
    fs::write(root.join("README.md"), "# Shop\n").unwrap();
    ok(root, &["sync"]);
    assert!(ok(root, &["lane", "diff", "1", "--stat"])
        .contains("main changed 1 file since the lane started: it has not seen them"));
    let none = kito(root, &["lane", "diff", "main"]);
    assert!(String::from_utf8_lossy(&none.stderr).contains("the main lane has no diff"));
}

#[test]
fn comments_on_a_lane_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    let shop = "export function total(prices: number[]): number {
  let sum = 0;
  for (const price of prices) {
    sum += price;
  }
  return sum;
}
";
    fs::write(
        root.join(".gitignore"),
        ".kito/
",
    )
    .unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.ts"), shop).unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    ok(root, &["lane", "open", "alice", "--intent", "prices in cents", "--scope", "./"]);
    let work = root.join(".kito/lanes/1/work");
    assert_eq!(
        ok(root, &["lane", "comments", "1"]),
        "No comment waits on it.
"
    );

    fs::write(work.join("src/shop.ts"), shop.replace("sum += price;", "sum += Math.round(price * 100);"))
        .unwrap();
    // On the working copy as it is: nobody has to sync first.
    let said = ok(root, &["lane", "comment", "1", "Half a cent rounds up?", "--on", "src/shop.ts:4"]);
    assert_eq!(
        said,
        "comment#1 on src/shop.ts:4 (ada): Half a cent rounds up?
  >     sum += Math.round(price * 100);
"
    );
    ok(root, &["lane", "comment", "1", "Looks right otherwise."]);
    ok(root, &["lane", "comment", "1", "It does: Math.round.", "--reply", "comment#1"]);
    let listed = "comment#1 on src/shop.ts:4 (ada): Half a cent rounds up?
  >     sum += Math.round(price * 100);
  comment#3 (ada): It does: Math.round.
comment#2 (ada): Looks right otherwise.
";
    assert_eq!(ok(root, &["lane", "comments", "1"]), listed);
    // The lane says what waits on it.
    let status = ok(root, &["lane", "status", "1"]);
    assert!(
        status.contains(
            "Comments:
  comment#1 on src/shop.ts:4 (ada): Half a cent rounds up?"
        ),
        "{status}"
    );
    assert_eq!(json(root, &["lane", "status", "1", "--json"])["comments"].as_array().unwrap().len(), 3);

    // The agent in the lane is told before its next edit, once.
    let file = work.join("src/shop.ts");
    let held = hook(root, &["hook", "before-edit"], &edit(&file, "let sum = 0;", "let sum = 0.0;"));
    let told = refusal(&held);
    assert!(told.contains("Somebody commented on lane/1") && told.contains("comment#2 (ada)"), "{told}");

    assert_eq!(
        ok(root, &["lane", "resolve-comment", "comment#1"]),
        "comment#1 is resolved.
"
    );
    assert_eq!(
        ok(root, &["lane", "comments", "1"]),
        "comment#2 (ada): Looks right otherwise.
"
    );
    let all = ok(root, &["lane", "comments", "1", "--all"]);
    assert!(all.contains("Half a cent rounds up?  [resolved by ada]"), "{all}");
    let twice = kito(root, &["lane", "resolve-comment", "comment#1"]);
    assert!(String::from_utf8_lossy(&twice.stderr).contains("resolved already"));
    let both = kito(root, &["lane", "comment", "1", "x", "--on", "src/shop.ts", "--reply", "comment#2"]);
    assert!(!both.status.success());
}

#[test]
fn a_lane_is_reviewed_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    let config = fs::read_to_string(root.join(".kito/config.toml")).unwrap();
    assert!(config.contains("\n[review]\n") && config.contains("\nrequired = 0\n"), "{config}");
    let shop = "export function total(prices: number[]): number {\n  return prices.reduce((sum, price) => sum + price, 0);\n}\n";
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::write(root.join("shop.ts"), shop).unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    ok(root, &["lane", "open", "alice", "--intent", "prices in cents", "--scope", "./"]);
    let work = root.join(".kito/lanes/1/work");
    fs::write(work.join("shop.ts"), shop.replace("sum + price", "sum + Math.round(price * 100)")).unwrap();
    assert_eq!(ok(root, &["lane", "review", "1"]), "lane/1: no verdict yet, and none is needed\n");

    // Ada looks at what her agent did and wants it changed: said with
    // words, it stops the lane.
    let none = kito(root, &["lane", "request-changes", "1"]);
    assert!(!none.status.success());
    let asked = ok(root, &["lane", "request-changes", "1", "Whole cents only."]);
    assert_eq!(asked, "lane/1: changes requested by ada\n  changes requested by ada: Whole cents only.\n");
    assert!(ok(root, &["board"]).contains("active (changes requested by ada)"));
    assert!(ok(root, &["lanes"]).contains("[changes requested by ada]"));
    let waits = ok(root, &["integrate", "1"]);
    assert!(waits.contains("it waits: changes requested by ada"), "{waits}");
    assert_eq!(fs::read_to_string(root.join("shop.ts")).unwrap(), shop);
    assert!(ok(root, &["board"]).contains("awaiting approval (asked by ada; changes requested by ada)"));

    // Her approval takes the place of her request. It is hers, on her own
    // lane: it lifts the request and counts for nothing more, and it
    // brings nothing in.
    ok(root, &["lane", "comment", "1", "Math.round does that."]);
    let approved = ok(root, &["lane", "approve", "1", "Right."]);
    assert_eq!(
        approved,
        "lane/1: no verdict yet, and none is needed\n  approved by ada: Right.  [does not count: the lane is theirs]\n  1 comment waits (`kito lane comments lane/1`)\n"
    );
    assert_eq!(fs::read_to_string(root.join("shop.ts")).unwrap(), shop);
    assert_eq!(json(root, &["lane", "review", "1", "--json"])["verdicts"][0]["counts"], false);

    // Taken back; then integrating is its own action.
    assert_eq!(
        ok(root, &["lane", "withdraw", "1"]),
        "lane/1: no verdict yet, and none is needed\n  1 comment waits (`kito lane comments lane/1`)\n"
    );
    let done = ok(root, &["integrate", "1"]);
    assert!(done.contains("integrated"), "{done}");
    assert!(fs::read_to_string(root.join("shop.ts")).unwrap().contains("Math.round"));
}

/// A directory with the plugin of the tests in it, as a download would
/// leave it: its manifest and its program (an example of this crate).
fn hello_plugin(base: &Path, manifest: &str) -> PathBuf {
    let name = format!("kito-hello{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().unwrap();
    let built = exe
        .ancestors()
        .map(|dir| dir.join("examples").join(&name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("examples/{name} is not built: `cargo test -p kito-cli` builds it"));
    let dir = base.join("hello-src");
    fs::create_dir_all(&dir).unwrap();
    fs::copy(built, dir.join(&name)).unwrap();
    fs::write(dir.join("kito-plugin.toml"), manifest).unwrap();
    dir
}

const HELLO: &str = "name = \"hello\"\nversion = \"0.1.0\"\ndescription = \"Says what it is told\"\napi = 1\npermissions = [\"read\", \"write\"]\n\n[[command]]\nname = \"hello\"\nabout = \"Ask the daemon as a plugin\"\n\n[[command]]\nname = \"hi\"\nabout = \"The same, shorter\"\n";

#[test]
fn a_plugin_is_installed_enabled_and_answered_what_it_was_given() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n.absent-personal-config/\nhello-src/\n").unwrap();
    fs::write(root.join("shop.ts"), "export const price = 1;\n").unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);

    // Not a command of Kito, and of no plugin.
    let none = kito(root, &["hello", "env"]);
    assert!(!none.status.success());
    assert!(String::from_utf8_lossy(&none.stderr).contains("`hello` is not a command of Kito"));
    assert!(ok(root, &["plugin", "list"]).contains("No plugins."));

    // Installed for the person: it runs nowhere yet.
    let source = hello_plugin(root, HELLO);
    let added = ok(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
    assert!(added.starts_with("hello 0.1.0 is installed in "), "{added}");
    assert_eq!(ok(root, &["plugin", "list"]), "hello 0.1.0  [not enabled here]  Says what it is told\n");
    let info = ok(root, &["plugin", "info", "hello"]);
    assert!(info.contains("asks for: read, write") && info.contains("kito hi  The same, shorter"), "{info}");
    let not_here = kito(root, &["hello", "env"]);
    let said = String::from_utf8_lossy(&not_here.stderr).into_owned();
    assert!(said.contains("which this repository did not enable: `kito plugin enable hello`"), "{said}");

    // Enabled by the repository, with what it asks for.
    let enabled = ok(root, &["plugin", "enable", "hello"]);
    assert!(enabled.starts_with("hello is enabled in this repository. It may: read, write.\n"), "{enabled}");
    let config = fs::read_to_string(root.join(".kito/config.toml")).unwrap();
    assert!(
        config.contains("[plugins.hello]\nenabled = true\npermissions = [\"read\", \"write\"]"),
        "{config}"
    );
    assert!(ok(root, &["plugin", "list"]).contains("[enabled here (read, write)]"));

    // Its commands are Kito's now, with what it needs to reach the daemon.
    let env = ok(root, &["hello", "env"]);
    assert!(env.contains("api: 2\n") && env.contains("key: 64 characters\n"), "{env}");
    // The repository, as the system names it.
    let told = env.lines().find_map(|line| line.strip_prefix("repo: ")).unwrap();
    assert_eq!(Path::new(told).canonical().unwrap(), root, "{env}");
    // And where the daemon listens, for a plugin with a client of its own.
    let socket = env.lines().find_map(|line| line.strip_prefix("socket: ")).unwrap();
    match cfg!(windows) {
        true => assert!(socket.starts_with(r"\\.\pipe\kito-"), "{env}"),
        false => assert!(Path::new(socket).exists(), "{env}"),
    }
    assert_eq!(kito(root, &["hi", "exit", "7"]).status.code(), Some(7));
    let ping: Value = serde_json::from_str(&ok(root, &["hello", "ask", "ping"])).unwrap();
    assert_eq!(ping["api"], 2);

    // The daemon answers it what it was given: it reads and it writes...
    let lanes: Value = serde_json::from_str(&ok(root, &["hello", "ask", "lanes"])).unwrap();
    assert_eq!(lanes, serde_json::json!([]));
    let opened = ok(
        root,
        &["hello", "ask", "open_lane", r#"{"agent":"helper","intent":"a cheaper price","scope":["./"]}"#],
    );
    assert!(opened.contains("\"id\":\"lane/1\""), "{opened}");
    // ...and neither integrates, nor configures, nor gets a key.
    for (method, params, needs) in [
        ("request_integration", r#"{"lane":"1"}"#, "needs the permission `integrate`"),
        ("configure", r#"{"checks":["true"]}"#, "needs the permission `admin`"),
        ("plugin_enable", r#"{"name":"hello","permissions":["admin"]}"#, "needs the permission `admin`"),
        ("plugin_key", r#"{"name":"hello"}"#, "is not for plugins"),
        ("shutdown", "{}", "is not for plugins"),
    ] {
        let refused = kito(root, &["hello", "ask", method, params]);
        let said = String::from_utf8_lossy(&refused.stderr).into_owned();
        assert_eq!(refused.status.code(), Some(3), "{method}: {said}");
        assert!(said.contains(needs) && said.contains("hello"), "{method}: {said}");
    }

    // Given less than it asks for; never more.
    ok(root, &["plugin", "enable", "hello", "--only", "read"]);
    let refused = kito(root, &["hello", "ask", "close_lane", r#"{"lane":"1"}"#]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("needs the permission `write`"));
    let more = kito(root, &["plugin", "enable", "hello", "--only", "read,admin"]);
    assert!(String::from_utf8_lossy(&more.stderr).contains("does not ask for `admin`"));

    // Disabled, nothing of it runs here; removed, it is gone for the person.
    assert_eq!(ok(root, &["plugin", "disable", "hello"]), "hello is disabled in this repository.\n");
    assert!(!kito(root, &["hello", "env"]).status.success());
    assert!(ok(root, &["plugin", "list"]).contains("[disabled here]"));
    ok(root, &["plugin", "enable", "hello"]);
    assert!(ok(root, &["plugin", "remove", "hello"]).starts_with("hello is removed."));
    let gone = kito(root, &["hello", "env"]);
    assert!(String::from_utf8_lossy(&gone.stderr).contains("is not installed here: `kito plugin add hello`"));
    assert!(ok(root, &["plugin", "list"]).contains("is not installed here"));
}

#[test]
fn what_is_not_a_plugin_is_not_installed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    let refused = |manifest: &str, words: &str| {
        let source = hello_plugin(root, manifest);
        let out = kito(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
        let said = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success() && said.contains(words), "{said}");
    };
    refused(&HELLO.replace("api = 1", "api = 99"), "update Kito");
    refused(
        &HELLO.replace("name = \"hello\"\nversion", "name = \"other\"\nversion"),
        "the program of the plugin `other`",
    );
    refused(&HELLO.replace("\"write\"", "\"everything\""), "kito-plugin.toml");
    let none = kito(root, &["plugin", "add", "--path", root.join("nowhere").to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&none.stderr).contains("kito-plugin.toml"));
    assert!(String::from_utf8_lossy(&kito(root, &["plugin", "info", "hello"]).stderr)
        .contains("is not installed"));
}

const GATED: &str = "name = \"hello\"\nversion = \"0.2.0\"\napi = 1\npermissions = [\"read\"]\n\n[[gate]]\nname = \"licence\"\nabout = \"Every file is allowed in\"\n\n[[event]]\nkinds = [\"lane_opened\", \"commented\"]\n";

/// How asking to integrate lane/1 ended, however it did.
fn integrate_json(root: &Path) -> Value {
    let out = kito(root, &["integrate", "1", "--json"]);
    let results: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("{e}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    });
    results[0].clone()
}

/// Waits until `done` says so; fails after twenty seconds.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "still waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn a_gate_of_a_plugin_passes_fails_or_says_to_wait() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n.absent-personal-config/\nhello-src/\n").unwrap();
    fs::write(root.join("shop.ts"), "export const price = 1;\n").unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    let source = hello_plugin(root, GATED);
    ok(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
    ok(root, &["plugin", "enable", "hello"]);
    let info = ok(root, &["plugin", "info", "hello"]);
    assert!(info.contains("gate licence  Every file is allowed in"), "{info}");
    assert!(info.contains("run for: lane_opened, commented"), "{info}");
    assert!(ok(root, &["status"]).contains("Plugin ............ hello 0.2.0 (read)"));

    ok(root, &["lane", "open", "alice", "--intent", "a cheaper price", "--scope", "./"]);
    let work = root.join(".kito/lanes/1/work");

    // The gate is asked after the checks, and passes: it is in what was
    // verified, with them.
    fs::write(work.join("shop.ts"), "export const price = 0.5;\n").unwrap();
    let done = integrate_json(root);
    assert_eq!(done["result"], "integrated", "{done}");
    assert_eq!(done["verification"]["commands"], serde_json::json!(["plugin hello: gate licence"]));

    // It fails: nothing is integrated, and it says why, about what it was told.
    fs::write(work.join("shop.ts"), "export const price = 0.25;\n").unwrap();
    fs::write(work.join("GATE_FAIL"), "prices under 0.5 are not allowed\n").unwrap();
    let failed = integrate_json(root);
    assert_eq!(failed["result"], "verification_failed", "{failed}");
    assert_eq!(failed["verification"]["failed_command"], "plugin hello: gate licence");
    let why = failed["verification"]["output"].as_str().unwrap();
    assert_eq!(why, "prices under 0.5 are not allowed: lane/1 by alice (2 files) onto main");
    assert_eq!(fs::read_to_string(root.join("shop.ts")).unwrap(), "export const price = 0.5;\n");
    let said = String::from_utf8_lossy(&kito(root, &["integrate", "1"]).stdout).into_owned();
    assert!(said.contains("plugin hello: gate licence") && said.contains("are not allowed"), "{said}");

    // It says to wait: nothing failed, and the lane waits with its reason.
    fs::remove_file(work.join("GATE_FAIL")).unwrap();
    fs::write(work.join("GATE_WAIT"), "the ticket SHOP-12 is not approved yet\n").unwrap();
    let waits = integrate_json(root);
    assert_eq!(waits["result"], "awaiting_approval", "{waits}");
    assert_eq!(
        waits["missing"],
        "the gate `licence` of the plugin `hello`: the ticket SHOP-12 is not approved yet"
    );
    assert!(ok(root, &["integrate", "1"]).contains("it waits: the gate `licence` of the plugin `hello`"));
    assert_eq!(fs::read_to_string(root.join("shop.ts")).unwrap(), "export const price = 0.5;\n");

    // Asked again when integration is, it lets the lane in.
    fs::remove_file(work.join("GATE_WAIT")).unwrap();
    assert_eq!(integrate_json(root)["result"], "integrated");
    assert_eq!(fs::read_to_string(root.join("shop.ts")).unwrap(), "export const price = 0.25;\n");

    // Disabled, the plugin gates nothing.
    ok(root, &["plugin", "disable", "hello"]);
    fs::write(work.join("shop.ts"), "export const price = 0.1;\n").unwrap();
    fs::write(work.join("GATE_FAIL"), "never asked\n").unwrap();
    let free = integrate_json(root);
    assert_eq!(
        (free["result"].as_str(), free["verification"].is_null()),
        (Some("integrated"), true),
        "{free}"
    );
}

#[test]
fn a_gate_that_gives_no_answer_or_never_ends_is_a_gate_that_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n.absent-personal-config/\nhello-src/\n").unwrap();
    fs::write(root.join("shop.ts"), "export const price = 1;\n").unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    let manifest = "name = \"hello\"\nversion = \"0.2.0\"\napi = 1\n\n[[gate]]\nname = \"mute\"\n";
    let source = hello_plugin(root, manifest);
    ok(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
    ok(root, &["plugin", "enable", "hello"]);
    ok(root, &["lane", "open", "alice", "--intent", "a cheaper price", "--scope", "./"]);
    fs::write(root.join(".kito/lanes/1/work/shop.ts"), "export const price = 0.5;\n").unwrap();

    let mute = integrate_json(root);
    assert_eq!(mute["result"], "verification_failed", "{mute}");
    assert!(
        mute["verification"]["output"].as_str().unwrap().contains("gave no answer that can be read"),
        "{mute}"
    );

    // One that never ends is stopped when its time is up.
    let slow = manifest.replace("\"mute\"", "\"slow\"\ntimeout_secs = 1");
    let source = hello_plugin(root, &slow);
    ok(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
    let stopped = integrate_json(root);
    assert_eq!(stopped["result"], "verification_failed", "{stopped}");
    assert_eq!(stopped["verification"]["output"], "still running after 1 s: stopped");
    assert_eq!(fs::read_to_string(root.join("shop.ts")).unwrap(), "export const price = 1;\n");
}

#[test]
fn a_plugin_is_run_for_the_events_it_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n.absent-personal-config/\nhello-src/\n").unwrap();
    fs::write(root.join("shop.ts"), "export const price = 1;\n").unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    let source = hello_plugin(root, GATED);
    ok(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
    ok(root, &["plugin", "enable", "hello"]);
    let log = root.join(".kito/plugins/hello/events.log");
    let told = || fs::read_to_string(&log).unwrap_or_default();

    // After the fact, in order, and only the kinds it listed.
    ok(root, &["lane", "open", "alice", "--intent", "a cheaper price", "--scope", "./"]);
    ok(root, &["lane", "comment", "1", "Cheaper by how much?"]);
    ok(root, &["lane", "comment", "1", "By half."]);
    wait_until("the plugin to be told", || told().lines().count() == 3);
    let lines: Vec<String> = told().lines().map(str::to_owned).collect();
    assert!(lines[0].starts_with("lane_opened lane/1 "), "{lines:?}");
    assert_eq!(lines[1..], ["commented lane/1 Cheaper by how much?", "commented lane/1 By half."]);

    // One that fails is said, and the next event is still delivered.
    ok(root, &["lane", "comment", "1", "This will explode."]);
    wait_until("the failure to be known", || {
        ok(root, &["plugin", "list"]).contains("last failed on `commented`")
    });
    let listed = ok(root, &["plugin", "list"]);
    assert!(listed.contains("exit code 1: hello: it exploded"), "{listed}");
    assert!(ok(root, &["plugin", "info", "hello"]).contains("it last failed on `commented`"));
    assert!(ok(root, &["status"]).contains("Plugin ............ hello 0.2.0: last failed on `commented`"));
    ok(root, &["lane", "comment", "1", "And after."]);
    wait_until("the event after the failure", || told().contains("commented lane/1 And after."));
    // It ran well since: nothing is held against it.
    wait_until("the failure to be forgotten", || !ok(root, &["plugin", "list"]).contains("last failed"));

    // Disabled, it is told nothing more.
    ok(root, &["plugin", "disable", "hello"]);
    ok(root, &["lane", "comment", "1", "Said to nobody."]);
    ok(root, &["lane", "comment", "1", "Nor this."]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!told().contains("Said to nobody"), "{}", told());
}

#[test]
fn an_assistant_is_set_up_by_the_plugin_that_knows_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n.absent-personal-config/\nhello-src/\n").unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    assert!(ok(root, &["assistant", "list"]).contains("claude   Claude Code"));
    // Nothing installed knows it, and the registry does not have it.
    let none = command(root, &["assistant", "add", "hello"])
        .env("KITO_NO_CLAUDE", "1")
        .env("KITO_REGISTRY", "file:///nonexistent-kito-registry")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&none.stderr).into_owned();
    assert!(!none.status.success() && said.contains("its plugin `hello` could not be installed"), "{said}");

    let manifest = "name = \"hello\"\nversion = \"0.3.0\"\napi = 1\npermissions = [\"read\"]\n\n[assistant]\nname = \"hello\"\n\n[[tool]]\nname = \"shout\"\ndescription = \"Says it louder.\"\n";
    let source = hello_plugin(root, manifest);
    ok(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
    let info = ok(root, &["plugin", "info", "hello"]);
    assert!(info.contains("sets up the assistant hello: `kito assistant add hello`"), "{info}");
    assert!(info.contains("tool kito.hello.shout  Says it louder."), "{info}");
    // Installed and not enabled here: `add` enables it, and says so; the
    // plugin writes that assistant's files, and says what the assistant
    // cannot be held to.
    let set = ok(root, &["assistant", "add", "hello"]);
    assert!(set.contains("enabled the plugin hello here: it may read"), "{set}");
    assert!(set.contains("hello is set up for Kito") && set.contains("no hook before an edit"), "{set}");
    let listed = ok(root, &["assistant", "list"]);
    assert!(listed.contains("plugin hello, enabled here"), "{listed}");
    let set = ok(root, &["assistant", "add", "hello"]);
    assert!(!set.contains("enabled the plugin"), "{set}");
    assert!(fs::read_to_string(root.join("HELLO.md")).unwrap().contains("Work in a Kito lane"));
    assert!(ok(root, &["assistant", "remove", "hello"]).contains("no longer set up"));
    assert!(!root.join("HELLO.md").exists());
}

const SIGNS: &str = r#"name = "hello"
version = "0.6.0"
api = 1
permissions = ["sign_in"]

[sign_in]
method = "hello"
about = "Sign in with hello"
"#;

#[test]
fn somebody_signs_in_to_a_server_as_who_a_plugin_says_they_are() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let serverdir = base.join("server");
    let server = |args: &[&str]| {
        let mut command = Command::new(kito_server());
        command
            .arg("--dir")
            .arg(&serverdir)
            .args(args)
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", base.join(".server-config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        command
    };
    let run = |args: &[&str]| -> String {
        let out = server(args).output().unwrap();
        assert!(out.status.success(), "kito-server {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let src = base.join("src-repo");
    fs::create_dir_all(src.join("src")).unwrap();
    fs::write(src.join("src/shop.py"), SHOP_PY).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "start"]);
    run(&["init", serverdir.to_str().unwrap()]);
    run(&["repo", "add", "shop", "--from", src.to_str().unwrap()]);
    let plugin = hello_plugin(&base, SIGNS);
    run(&["plugin", "add", "--path", plugin.to_str().unwrap()]);
    run(&["plugin", "enable", "hello"]);
    run(&["user", "add", "bob", "--identity", "hello:bob", "--repo", "shop:integrate"]);
    struct Serving(std::process::Child);
    impl Drop for Serving {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut serving = Serving(
        server(&["serve", "--listen", "127.0.0.1:0"])
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
    // Who signed in at the provider that does not exist: a file says.
    let at_hello = serverdir.join("plugins/hello/sign-in");
    fs::create_dir_all(at_hello.parent().unwrap()).unwrap();
    let errors = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();
    // Bob's own Kito directory, where his servers are kept.
    let me = base.join(".bob");
    let kito = |cwd: &Path, args: &[&str]| kito_as(&me, cwd, args);
    let ok = |cwd: &Path, args: &[&str]| ok_as(&me, cwd, args);

    // A way the server does not have, and somebody it does not have:
    // nothing is connected.
    let none = kito(&base, &["connect", &address, "shop", "--sign-in", "github"]);
    assert!(errors(&none).contains("no way to sign in with `github`: it has hello"), "{}", errors(&none));
    fs::write(&at_hello, "eve").unwrap();
    let eve = kito(&base, &["connect", &address, "shop", "--sign-in", "hello"]);
    assert!(!eve.status.success());
    assert!(errors(&eve).contains("hello:eve is nobody on this server"), "{}", errors(&eve));
    assert!(!base.join("shop").exists());
    assert!(!me.join("servers.toml").exists(), "a server nobody signed in to is not kept");

    // bob is told where to go and what to enter, and is connected when
    // he has: the command waits meanwhile.
    fs::remove_file(&at_hello).unwrap();
    let (dir, at, config) = (base.clone(), address.clone(), me.clone());
    // One address, and the only way the server offers: like `git clone`.
    let waiting =
        std::thread::spawn(move || kito_as(&config, &dir, &["clone", &format!("kito://{at}/shop")]));
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(!base.join("shop").exists(), "connected before anybody signed in");
    fs::write(&at_hello, "bob").unwrap();
    let connected = waiting.join().unwrap();
    assert!(connected.status.success(), "{}", errors(&connected));
    let asked = errors(&connected);
    assert!(asked.contains("To sign in with hello, open https://hello.example/device"), "{asked}");
    assert!(asked.contains("and enter the code HELLO-1234"), "{asked}");
    let said = String::from_utf8_lossy(&connected.stdout).into_owned();
    assert!(said.contains("is connected to `shop` on") && said.contains("as bob."), "{said}");
    let shop = base.join("shop");
    // The session is kept with his servers, and the directory names the server.
    let kept = fs::read_to_string(me.join("servers.toml")).unwrap();
    assert!(kept.contains("token = \"kitos_") && kept.contains("sign_in = \"hello\""), "{kept}");
    assert!(!fs::read_to_string(shop.join(".kito/remote.toml")).unwrap().contains("token"));
    assert!(ok(&shop, &["repos"]).contains("shop"));

    // A session that ended says how to get another, and that works.
    fs::remove_file(serverdir.join("sessions.toml")).unwrap();
    let ended = kito(&shop, &["repos"]);
    assert!(!ended.status.success());
    assert!(errors(&ended).contains("this session ended") && errors(&ended).contains("kito sign-in"));
    let again = ok(&shop, &["sign-in"]);
    assert!(again.contains("as bob: the session lasts 30 days."), "{again}");
    assert!(ok(&shop, &["repos"]).contains("shop"));
    // The directory's daemon asks with the new session, too.
    ok(&shop, &["status"]);

    // One who says no at the provider keeps the session they had.
    fs::write(&at_hello, "refuse").unwrap();
    let no = kito(&shop, &["sign-in"]);
    assert!(!no.status.success());
    assert!(errors(&no).contains("they said no at hello"), "{}", errors(&no));
    assert!(ok(&shop, &["repos"]).contains("shop"));

    // One session serves every directory of the server: a second one is
    // made without signing in, and one sign-in serves both when it ended,
    // their daemons too.
    let too = base.join("shop-too");
    assert!(ok(&base, &["clone", "shop", "shop-too"]).contains("as bob."));
    ok(&too, &["status"]);
    fs::remove_file(serverdir.join("sessions.toml")).unwrap();
    for dir in [&shop, &too] {
        assert!(errors(&kito(dir, &["repos"])).contains("this session ended"));
    }
    fs::write(&at_hello, "bob").unwrap();
    assert!(ok(&too, &["sign-in"]).contains("as bob: the session lasts 30 days."));
    for dir in [&shop, &too] {
        assert!(ok(dir, &["repos"]).contains("shop"));
        ok(dir, &["status"]);
        assert!(ok(dir, &["lanes"]).contains("No open lanes"));
    }

    // Forgotten, the server is asked to end the session; the directories
    // that name it say what to do.
    let sessions = || {
        fs::read_to_string(serverdir.join("sessions.toml"))
            .unwrap_or_default()
            .matches("user = \"bob\"")
            .count()
    };
    assert_eq!(sessions(), 1);
    let said = ok(&base, &["server", "remove", &address]);
    assert!(said.contains("is forgotten") && said.contains("Your session there ended."), "{said}");
    assert_eq!(sessions(), 0);
    assert!(!fs::read_to_string(me.join("servers.toml")).unwrap().contains(&address));
    let gone = kito(&shop, &["repos"]);
    assert!(
        !gone.status.success() && errors(&gone).contains("is not one of your servers"),
        "{}",
        errors(&gone)
    );
    for dir in [&shop, &too] {
        ok(dir, &["daemon", "stop"]);
    }
}

#[test]
fn how_people_sign_in_is_changed_from_a_connected_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let serverdir = base.join("server");
    let server = |args: &[&str]| {
        let mut command = Command::new(kito_server());
        command
            .arg("--dir")
            .arg(&serverdir)
            .args(args)
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", base.join(".server-config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        command
    };
    let run = |args: &[&str]| -> String {
        let out = server(args).output().unwrap();
        assert!(out.status.success(), "kito-server {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let token_in = |printed: &str| {
        printed.lines().find(|line| line.starts_with("kito_")).expect(printed).trim().to_owned()
    };
    let src = base.join("src-repo");
    fs::create_dir_all(src.join("src")).unwrap();
    fs::write(src.join("src/shop.py"), SHOP_PY).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "start"]);
    run(&["init", serverdir.to_str().unwrap()]);
    run(&["repo", "add", "shop", "--from", src.to_str().unwrap()]);
    let plugin = hello_plugin(&base, SIGNS);
    run(&["plugin", "add", "--path", plugin.to_str().unwrap()]);
    run(&["plugin", "enable", "hello"]);
    let ada = token_in(&run(&["user", "add", "ada", "--role", "admin"]));
    let bob = token_in(&run(&["user", "add", "bob", "--repo", "shop:write"]));
    run(&["user", "add", "carol", "--identity", "hello:carol", "--repo", "shop:read"]);
    struct Serving(std::process::Child);
    impl Drop for Serving {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut serving = Serving(
        server(&["serve", "--listen", "127.0.0.1:0"])
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
    // Who signed in at the provider that does not exist: a file says.
    let at_hello = serverdir.join("plugins/hello/sign-in");
    fs::create_dir_all(at_hello.parent().unwrap()).unwrap();
    let errors = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();
    // Each person's own Kito directory, where their servers are kept: the
    // directories of one person share it. Carol's desk is on another machine.
    let config_of = |dir: &Path| -> PathBuf {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        match name.as_str() {
            "carol-desk" => base.join(".carol-elsewhere"),
            name => base.join(format!(".{}", name.split('-').next().unwrap())),
        }
    };
    let kito = |dir: &Path, args: &[&str]| kito_as(&config_of(dir), dir, args);
    let ok = |dir: &Path, args: &[&str]| ok_as(&config_of(dir), dir, args);
    let json =
        |dir: &Path, args: &[&str]| -> Value { serde_json::from_str(&ok(dir, args)).expect("JSON output") };
    let connect = |dir: &str, how: &[&str]| -> String {
        let mut args = vec!["connect", &address, "shop", dir];
        args.extend_from_slice(how);
        ok_as(&config_of(&base.join(dir)), &base, &args)
    };
    let kept_in = |dir: &Path| fs::read_to_string(config_of(dir).join("servers.toml")).unwrap();
    let unknown = "this token is not one the server knows";

    connect("ada-dir", &["--token", &ada]);
    connect("bob-dir", &["--token", &bob]);
    connect("bob-desk", &["--token", &bob]);
    let (adadir, bobdir, bobdesk) = (base.join("ada-dir"), base.join("bob-dir"), base.join("bob-desk"));

    // The admins see how each person signs in, never with what.
    let people = |dir: &Path| -> Vec<String> {
        let listed = ok(dir, &["people"]);
        listed.lines().map(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")).collect()
    };
    assert_eq!(
        people(&adadir),
        [
            "ada admin every repository; signs in with a token",
            "bob member shop (write); signs in with a token",
            "carol member shop (read); signs in as hello:carol",
        ]
    );

    // Bob gets a new token of his own: said this once, and kept with his
    // servers, so that every directory of that server goes on with it,
    // their daemons too.
    ok(&bobdir, &["status"]);
    ok(&bobdesk, &["status"]);
    let said = ok(&bobdir, &["people", "token"]);
    let new = token_in(&said);
    assert!(said.contains("Your new token on") && said.contains(", shown this once:"), "{said}");
    let others =
        format!("It is kept with your servers: every directory connected to `{address}` uses it already.");
    assert!(said.contains(&others), "{said}");
    assert!(new != bob && kept_in(&bobdir).contains(&format!("token = \"{new}\"")), "{}", kept_in(&bobdir));
    assert!(!kept_in(&bobdir).contains(&bob));
    assert!(ok(&bobdir, &["repos"]).contains("shop"));
    ok(&bobdir, &["status"]);
    assert!(ok(&bobdir, &["lanes"]).contains("No open lanes"));
    // His other directory has it too, its daemon as well, which connects
    // again with it.
    assert!(ok(&bobdesk, &["repos"]).contains("shop"));
    let mut lanes = kito(&bobdesk, &["lanes"]);
    for _ in 0..50 {
        if lanes.status.success() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        lanes = kito(&bobdesk, &["lanes"]);
    }
    assert!(String::from_utf8_lossy(&lanes.stdout).contains("No open lanes"), "{}", errors(&lanes));
    // For a program, with whether it was kept; and by his own name he is
    // himself.
    let said = json(&bobdir, &["--json", "people", "token"]);
    let newer = said["token"].as_str().unwrap().to_owned();
    assert_eq!(said, serde_json::json!({ "user": "bob", "token": newer, "logins": [], "kept_here": true }));
    assert!(kept_in(&bobdir).contains(&newer) && !kept_in(&bobdir).contains(&new));
    let said = ok(&bobdir, &["people", "token", "bob"]);
    assert!(said.contains("Your new token on") && said.contains("It is kept with your servers"), "{said}");
    assert!(kept_in(&bobdir).contains(&token_in(&said)) && ok(&bobdir, &["repos"]).contains("shop"));
    // Never printed by what lists his servers.
    let listed = ok(&bobdir, &["server", "list"]);
    assert!(listed.contains(&address) && listed.contains("as bob") && !listed.contains("kito_"), "{listed}");

    // Nobody else's is his to change; a token is all he signs in with;
    // and a login named proves nothing.
    let no = |dir: &Path, args: &[&str]| -> String {
        let out = kito(dir, args);
        assert!(!out.status.success(), "kito {args:?} should have failed");
        errors(&out)
    };
    let said = no(&bobdir, &["people", "token", "carol"]);
    assert!(said.contains("only an admin of this server may manage its people: bob is a member"), "{said}");
    let said = no(&bobdir, &["people", "token", "--remove"]);
    assert!(said.contains("bob would have no way to sign in: link an identity to them first"), "{said}");
    let said = no(&bobdir, &["people", "unlink", "hello"]);
    assert!(said.contains("bob has no identity of `hello`: there is nothing to unlink"), "{said}");
    let said = no(&bobdir, &["people", "link", "bob", "hello:bob"]);
    assert!(said.contains("naming a login proves nothing: bob links an account of their own"), "{said}");
    assert!(ok(&bobdir, &["repos"]).contains("shop"));

    // An admin attaches an identity to him: he keeps his token. He takes
    // it away himself, with the way to sign in alone; she does by his name.
    let said = ok(&adadir, &["people", "link", "bob", "hello:bob"]);
    assert!(said.contains("Whoever signs in to") && said.contains("as hello:bob is bob."), "{said}");
    assert!(said.contains("bob signs in with a token or as hello:bob."), "{said}");
    assert!(people(&adadir)
        .contains(&"bob member shop (write); signs in with a token or as hello:bob".to_owned()));
    let said = ok(&bobdir, &["people", "unlink", "hello"]);
    assert!(said.contains("bob no longer signs in to") && said.contains("with hello: their other sessions"));
    assert!(said.contains("bob signs in with a token."), "{said}");
    ok(&adadir, &["people", "link", "bob", "hello:bob"]);
    let said = ok(&adadir, &["people", "unlink", "bob", "hello"]);
    assert!(said.contains("with hello: their sessions ended.\nbob signs in with a token."), "{said}");
    let said = no(&adadir, &["people", "unlink", "carol", "hello"]);
    assert!(said.contains("carol would have no way to sign in: give them a token first"), "{said}");

    // She gives him a new token: it is said to her, once, and the
    // directory of his that had the one before is refused.
    let said = ok(&adadir, &["people", "token", "bob"]);
    assert!(said.contains("bob's new token on") && said.contains(", shown this once:"), "{said}");
    assert!(said.contains("A token they had no longer works, and their sessions ended"), "{said}");
    let given = token_in(&said);
    assert!(errors(&kito(&bobdir, &["repos"])).contains(unknown));
    assert!(!kept_in(&adadir).contains(&given) && ok(&adadir, &["repos"]).contains("shop"));
    connect("bob-dir", &["--token", &given]);

    // A new token takes no identity away: whoever held the one before may
    // have attached an account of theirs to him. She is told what is left,
    // and what to do if it leaked.
    ok(&adadir, &["people", "link", "bob", "hello:bob"]);
    let said = ok(&adadir, &["people", "token", "bob"]);
    let left = "bob signs in with a token or as hello:bob.\nIf the token was replaced because somebody else \
                had it: look at each identity (hello:bob) and unlink what is not bob's (`kito people unlink \
                bob <method>`), or `kito people token bob --alone`.";
    assert!(said.contains(left), "{said}");
    // Given alone, it is all he signs in with: every identity is unlinked.
    let said = ok(&adadir, &["people", "token", "bob", "--alone"]);
    let alone = "Unlinked: hello:bob. The token is all bob signs in with now, and their sessions ended.";
    assert!(said.contains(alone), "{said}");
    assert!(people(&adadir).contains(&"bob member shop (write); signs in with a token".to_owned()));
    let said = no(&adadir, &["people", "token", "bob", "--alone", "--remove"]);
    assert!(said.contains("cannot be used with"), "{said}");
    connect("bob-dir", &["--token", &token_in(&ok(&adadir, &["people", "token", "bob", "--alone"]))]);

    // With an identity too, he takes his token away from the directory
    // that connects with it, which is told how to go on: by signing in.
    ok(&adadir, &["people", "link", "bob", "hello:bob"]);
    let said = ok(&bobdir, &["people", "token", "--remove"]);
    assert!(said.contains("You have no token on") && said.contains("you sign in instead."), "{said}");
    let how = "You connected with it: `kito sign-in <method>` signs you in again.";
    assert!(said.contains(how), "{said}");
    assert!(errors(&kito(&bobdir, &["repos"])).contains(unknown));
    fs::write(&at_hello, "bob").unwrap();
    assert!(ok(&bobdir, &["sign-in", "hello"]).contains("as bob: the session lasts 30 days."));
    assert!(ok(&bobdir, &["repos"]).contains("shop"));
    assert!(people(&adadir).contains(&"bob member shop (write); signs in as hello:bob".to_owned()));

    // carol signs in: her directory holds a session, and goes on as it
    // is. The token she asks for is said to her, for somewhere else.
    fs::write(&at_hello, "carol").unwrap();
    connect("carol-dir", &["--sign-in", "hello"]);
    let caroldir = base.join("carol-dir");
    let session = kept_in(&caroldir);
    assert!(session.contains("token = \"kitos_") && session.contains("sign_in = \"hello\""), "{session}");
    let said = ok(&caroldir, &["people", "token"]);
    let hers = token_in(&said);
    assert!(said.contains("Your new token on"), "{said}");
    assert!(said.contains("You go on as you are connected to"), "{said}");
    assert_eq!(kept_in(&caroldir), session);
    assert!(ok(&caroldir, &["repos"]).contains("shop"));
    connect("carol-desk", &["--token", &hers]);
    let caroldesk = base.join("carol-desk");
    assert!(ok(&caroldesk, &["repos"]).contains("shop"));
    // She takes it away again: sign-in only. What it connected stops,
    // the session she asked through does not; and her identity stays.
    let said = ok(&caroldir, &["people", "token", "--remove"]);
    assert!(
        said.contains("You have no token on") && said.contains("any more: you sign in instead."),
        "{said}"
    );
    assert!(!said.contains("You connected with it"), "{said}");
    assert!(errors(&kito(&caroldesk, &["repos"])).contains(unknown));
    assert!(ok(&caroldir, &["repos"]).contains("shop"));
    let said = no(&caroldir, &["people", "unlink", "hello"]);
    assert!(said.contains("carol would have no way to sign in: give them a token first"), "{said}");

    for dir in [&adadir, &bobdir, &bobdesk, &caroldir, &caroldesk] {
        ok(dir, &["daemon", "stop"]);
    }
    drop(serving);
    ok(&serverdir.join("repos/shop"), &["daemon", "stop"]);
}

const FOR_CLIENTS: &str = r#"name = "hello"
version = "0.7.0"
description = "Says what it is told"
api = 2
permissions = ["read"]

[[command]]
name = "hello"
about = "Says what it is told"
"#;

#[test]
fn a_directory_connected_to_a_server_runs_the_plugins_the_server_lets_it() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let (serverdir, mine) = (base.join("server"), base.join("my-config"));
    let server = |args: &[&str]| {
        let mut command = Command::new(kito_server());
        command
            .arg("--dir")
            .arg(&serverdir)
            .args(args)
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", base.join(".server-config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        command
    };
    let run = |args: &[&str]| -> String {
        let out = server(args).output().unwrap();
        assert!(out.status.success(), "kito-server {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // The person's own plugins are in one place, whatever directory they are in.
    let me = |cwd: &Path, args: &[&str]| -> (bool, String) {
        let out =
            command(cwd, args).env("KITO_NO_CLAUDE", "1").env("KITO_CONFIG_DIR", &mine).output().unwrap();
        let said =
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        (out.status.success(), said)
    };
    let src = base.join("src-repo");
    fs::create_dir_all(src.join("src")).unwrap();
    fs::write(src.join("src/shop.py"), SHOP_PY).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "start"]);
    run(&["init", serverdir.to_str().unwrap()]);
    run(&["repo", "add", "shop", "--from", src.to_str().unwrap()]);
    let plugin = hello_plugin(&base, FOR_CLIENTS);
    run(&["plugin", "add", "--path", plugin.to_str().unwrap()]);
    // Enabled for the server alone, it runs nowhere else.
    run(&["plugin", "enable", "hello"]);
    let token = run(&["user", "add", "ada", "--repo", "shop:integrate"]);
    let token = token.lines().find(|line| line.starts_with("kito_")).unwrap().trim().to_owned();
    struct Serving(std::process::Child);
    impl Drop for Serving {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut serving = Serving(
        server(&["serve", "--listen", "127.0.0.1:0"])
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

    let (connected, said) = me(&base, &["connect", &address, "shop", "--token", &token]);
    assert!(connected, "{said}");
    let shop = base.join("shop");
    let (added, said) = me(&shop, &["plugin", "add", "--path", plugin.to_str().unwrap()]);
    assert!(added, "{said}");
    let (ran, said) = me(&shop, &["hello", "env"]);
    assert!(!ran, "a plugin the server did not let run here ran: {said}");
    // A person enables nothing in a connected directory: the server says.
    let (enabled, said) = me(&shop, &["plugin", "enable", "hello"]);
    assert!(!enabled && said.contains("the server says which plugins run"), "{said}");

    // The server lets it run in the directories connected to it. What
    // the server last said holds for a moment: a daemon started now asks.
    let said = run(&["plugin", "enable", "hello", "--for-clients"]);
    assert!(said.contains("runs in the directories connected to this server too"), "{said}");
    assert!(run(&["plugin", "list"]).contains("[enabled (read), for clients too]"));
    me(&shop, &["daemon", "stop"]);
    let (ran, said) = me(&shop, &["hello", "env"]);
    assert!(ran && said.contains("api: 2") && said.contains("key: 64 characters"), "{said}");
    let (_, listed) = me(&shop, &["plugin", "list"]);
    assert!(listed.contains("hello 0.7.0") && listed.contains("read"), "{listed}");

    // With its own key it asks what the server gave it, through the
    // directory's daemon, and is refused the rest.
    let (asked, said) = me(&shop, &["hello", "ask", "status"]);
    assert!(asked, "{said}");
    let (asked, said) = me(&shop, &["hello", "ask", "close_lane", "{\"lane\":\"1\"}"]);
    assert!(!asked && said.contains("the plugin `hello` may not ask `close_lane`"), "{said}");
    let (asked, said) = me(&shop, &["hello", "ask", "plugin_key", "{\"name\":\"hello\"}"]);
    assert!(!asked && said.contains("not for plugins"), "{said}");

    // It asks its counterpart on the server, which is told who asks.
    let (asked, said) =
        me(&shop, &["hello", "ask", "plugin_call", "{\"ask\":\"greet\",\"params\":{\"n\":1}}"]);
    assert!(asked, "{said}");
    let answer: Value = serde_json::from_str(said.trim()).unwrap_or_else(|e| panic!("{e}: {said}"));
    assert_eq!(
        answer,
        serde_json::json!({
            "asked": "greet", "params": { "n": 1 }, "by": "ada", "repo": "shop", "role": "integrate",
            "on_the_server": true,
        })
    );
    // Its own counterpart, whatever name it gives; and what that one
    // fails at is said as a failure of the server's plugin.
    let (asked, said) = me(&shop, &["hello", "ask", "plugin_call", "{\"ask\":\"greet\",\"plugin\":\"web\"}"]);
    assert!(asked && said.contains("\"asked\":\"greet\""), "{said}");
    let (asked, said) = me(&shop, &["hello", "ask", "plugin_call", "{\"ask\":\"explode\"}"]);
    assert!(!asked && said.contains("the plugin `hello` of the server failed at `explode`"), "{said}");

    // Somebody who does not have it installed is told so, and how. The
    // directory's daemon is one person's: it is started again as theirs.
    me(&shop, &["daemon", "stop"]);
    let other = base.join("other-config");
    // Connected to the server as well: the directory names it.
    fs::create_dir_all(&other).unwrap();
    fs::copy(mine.join("servers.toml"), other.join("servers.toml")).unwrap();
    let theirs = |args: &[&str]| {
        let out =
            command(&shop, args).env("KITO_NO_CLAUDE", "1").env("KITO_CONFIG_DIR", &other).output().unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };
    let (ran, said) = theirs(&["hello", "env"]);
    assert!(!ran && said.contains("the plugin `hello` is not installed here"), "{said}");
    theirs(&["daemon", "stop"]);

    // In a repository of its own there is no server to ask.
    let alone = base.join("alone");
    fs::create_dir_all(&alone).unwrap();
    assert!(me(&alone, &["init"]).0);
    assert!(me(&alone, &["plugin", "enable", "hello"]).0);
    let (asked, said) = me(&alone, &["hello", "ask", "plugin_call", "{\"ask\":\"greet\"}"]);
    assert!(!asked && said.contains("not connected to a server"), "{said}");
    me(&alone, &["daemon", "stop"]);
    me(&shop, &["daemon", "stop"]);
}

const SPEAKS: &str = r#"name = "hello"
version = "0.8.0"
api = 2
permissions = ["read"]

[session]
about = "Says to mind the step"
"#;

#[test]
fn a_plugin_has_its_say_when_a_session_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    let start = serde_json::json!({ "hook_event_name": "SessionStart", "cwd": root });
    let told = |root: &Path| {
        let (answer, code, errors) = hook(root, &["hook", "session-start"], &start);
        assert_eq!(code, 0, "{errors}");
        answer["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default().to_owned()
    };
    let source = hello_plugin(root, SPEAKS);
    ok(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
    // Installed and not enabled here: it says nothing.
    assert!(!told(root).contains("hello"));

    // Enabled: after Kito's own rules, and under whose words they are.
    ok(root, &["plugin", "enable", "hello"]);
    let text = told(root);
    let (rules, theirs) = text.split_once("From the plugin `hello`").unwrap_or_else(|| panic!("{text}"));
    assert!(rules.contains("Work in a lane.") && rules.contains("No lane is open right now."), "{rules}");
    assert!(theirs.contains("not Kito's rules, which stand whatever it says:\nMind the step."), "{theirs}");

    // A plugin with nothing to say at the start of a session is not asked.
    ok(root, &["plugin", "disable", "hello"]);
    assert!(!told(root).contains("Mind the step."));
    ok(root, &["daemon", "stop"]);
}

const LISTED: &str = r#"name = "hello"
version = "0.9.0"
description = "Says what it is told"
api = 2
permissions = ["read"]

[[command]]
name = "hello"
about = "Says what it is told"
"#;

#[test]
fn a_plugin_is_installed_by_its_name_from_a_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let (registry, mine) = (base.join("registry"), base.join("my-config"));
    fs::create_dir_all(registry.join("plugins")).unwrap();
    fs::create_dir_all(registry.join("files")).unwrap();
    let me = |args: &[&str]| -> (bool, String) {
        let out = command(&base, args)
            .env("KITO_NO_CLAUDE", "1")
            .env("KITO_CONFIG_DIR", &mine)
            .env("KITO_REGISTRY", format!("file://{}/", registry.display()))
            .output()
            .unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };

    // An empty registry says so.
    fs::write(registry.join("index.json"), "{\"registry\": 1, \"plugins\": {}}").unwrap();
    let (ok, said) = me(&["plugin", "search"]);
    assert!(ok && said.contains("lists no plugin yet"), "{said}");

    // The plugin as its author packs it: one directory, in an archive.
    let source = hello_plugin(&base, LISTED);
    let platform = kitod::registry::platform();
    let archive = registry.join(format!("files/kito-hello-0.9.0-{platform}.tar.gz"));
    let packed = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&base)
        .arg(source.file_name().unwrap())
        .status()
        .unwrap();
    assert!(packed.success());
    let sum = kitod::registry::sha256_hex(&fs::read(&archive).unwrap());
    // What the registry says of it: `version` as listed, the file by its checksum.
    let list = |version: &str, sum: &str| {
        let index = serde_json::json!({ "registry": 1, "plugins": {
            "hello": { "about": "Says what it is told", "latest": version },
            "web": { "about": "Kito in a browser", "latest": "0.4.0" },
        }});
        fs::write(registry.join("index.json"), index.to_string()).unwrap();
        let plugin = serde_json::json!({ "name": "hello", "about": "Says what it is told", "versions": {
            version: { "api": 2, "files": { platform.as_str(): {
                // Beside the registry's own files.
                "url": format!("files/kito-hello-0.9.0-{platform}.tar.gz"), "sha256": sum, "size": 1,
            }}},
            "9.0.0": { "api": 99, "files": { platform.as_str(): { "url": "files/later.tar.gz", "sha256": "0" } } },
        }});
        fs::write(registry.join("plugins/hello.json"), plugin.to_string()).unwrap();
    };
    list("0.9.0", &sum);

    let (ok, said) = me(&["plugin", "search"]);
    assert!(ok && said == "hello 0.9.0  Says what it is told\nweb 0.4.0  Kito in a browser\n", "{said}");
    assert_eq!(me(&["plugin", "search", "BROWSER"]).1, "web 0.4.0  Kito in a browser\n");
    assert!(me(&["plugin", "search", "nothing-like-it"]).1.contains("has no plugin that says that"));

    // What the registry does not have, and what does not fit this Kito.
    let (ok, said) = me(&["plugin", "add", "nope"]);
    assert!(!ok && said.contains("has no plugin `nope`: `kito plugin search` lists what it has"), "{said}");
    let (ok, said) = me(&["plugin", "add", "hello@0.1.0"]);
    assert!(!ok && said.contains("`hello` has no version 0.1.0"), "{said}");
    let (ok, said) = me(&["plugin", "add", "hello@9.0.0"]);
    assert!(!ok && said.contains("update Kito"), "{said}");

    // A file that is not the one the registry listed installs nothing.
    list("0.9.0", &"0".repeat(64));
    let (ok, said) = me(&["plugin", "add", "hello"]);
    assert!(!ok && said.contains("is not the file the registry listed for `hello` 0.9.0"), "{said}");
    assert!(said.contains("nothing was installed") && !mine.join("plugins/hello").exists(), "{said}");
    // Nor does one that is another version than the registry says.
    list("0.9.1", &sum);
    let (ok, said) = me(&["plugin", "add", "hello"]);
    assert!(
        !ok && said.contains("the registry lists it as `hello` 0.9.1, and it is `hello` 0.9.0"),
        "{said}"
    );
    assert!(!mine.join("plugins/hello").exists());

    // The latest that fits is installed, and where it came from is said.
    list("0.9.0", &sum);
    let (ok, said) = me(&["plugin", "add", "hello"]);
    assert!(ok, "{said}");
    assert!(said.starts_with("hello 0.9.0 is installed in "), "{said}");
    assert!(said.contains(&format!("(SHA-256 {sum})")) && said.contains("files/kito-hello-0.9.0-"), "{said}");
    assert!(mine.join("plugins/hello/kito-plugin.toml").is_file());
    assert!(me(&["plugin", "list"]).1.contains("hello 0.9.0"));
    // Its program is still one.
    let ran =
        Command::new(mine.join("plugins/hello").join(format!("kito-hello{}", std::env::consts::EXE_SUFFIX)))
            .args(["exit", "7"])
            .status()
            .unwrap();
    assert_eq!(ran.code(), Some(7));
    // Asked for by its version, the same.
    let (ok, said) = me(&["plugin", "add", "hello@0.9.0"]);
    assert!(ok && said.contains("hello 0.9.0 is installed"), "{said}");
    // A name or a directory, not both and not neither.
    assert!(!me(&["plugin", "add"]).0);

    // Nothing later yet; then the registry has a later version.
    assert!(me(&["plugin", "outdated"]).1.contains("is the latest the registry has"));
    assert!(me(&["plugin", "update", "hello"]).1.contains("hello 0.9.0 is the latest"));
    let later = hello_plugin(&base, &LISTED.replace("0.9.0", "0.10.0"));
    let archive = registry.join(format!("files/kito-hello-0.10.0-{platform}.tar.gz"));
    let packed = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&base)
        .arg(later.file_name().unwrap())
        .status();
    assert!(packed.unwrap().success());
    let later_sum = kitod::registry::sha256_hex(&fs::read(&archive).unwrap());
    let mut plugin: Value =
        serde_json::from_str(&fs::read_to_string(registry.join("plugins/hello.json")).unwrap()).unwrap();
    plugin["versions"]["0.10.0"] = serde_json::json!({ "api": 2, "files": { platform.as_str(): {
        "url": format!("files/kito-hello-0.10.0-{platform}.tar.gz"), "sha256": later_sum,
    }}});
    fs::write(registry.join("plugins/hello.json"), plugin.to_string()).unwrap();
    // Later by its numbers (0.10 after 0.9), and never the one for a later Kito (9.0.0).
    let (ok, said) = me(&["plugin", "outdated"]);
    assert!(
        ok && said.starts_with("hello 0.9.0 -> 0.10.0\n") && said.contains("`kito plugin update`"),
        "{said}"
    );
    let (ok, said) = me(&["plugin", "update"]);
    assert!(ok && said.starts_with("hello 0.9.0 -> 0.10.0 (SHA-256 "), "{said}");
    assert!(me(&["plugin", "list"]).1.contains("hello 0.10.0"));
    assert!(me(&["plugin", "outdated"]).1.contains("is the latest the registry has"));
    let (ok, said) = me(&["plugin", "update", "nope"]);
    assert!(!ok && said.contains("the plugin `nope` is not installed"), "{said}");
}

const LEAVES: &str = r#"name = "hello"
version = "0.9.0"
api = 2
permissions = ["read"]

[assistant]
name = "hello"

[cleanup]
about = "Takes HELLO.md out of the project"
"#;

#[test]
fn a_plugin_takes_out_what_it_wrote_when_it_goes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    let source = hello_plugin(root, LEAVES);
    ok(root, &["plugin", "add", "--path", source.to_str().unwrap()]);
    let (rules, own) = (root.join("HELLO.md"), root.join(".kito/plugins/hello"));
    let set_up = || {
        ok(root, &["plugin", "enable", "hello"]);
        ok(root, &["assistant", "add", "hello"]);
        assert!(rules.is_file() && own.join("key").is_file());
        // Something of its own, kept in the repository.
        fs::write(own.join("notes"), "what it remembers").unwrap();
    };

    // Disabled: it is run one last time, and what it wrote in the project
    // comes out. Its key is gone; what it kept stays for the next time.
    set_up();
    let said = ok(root, &["plugin", "disable", "hello"]);
    assert_eq!(said, "  took HELLO.md out\nhello is disabled in this repository.\n");
    assert!(!rules.exists() && !own.join("key").exists() && own.join("notes").is_file());
    assert_eq!(ok(root, &["plugin", "disable", "hello"]), "hello was not enabled in this repository.\n");

    // With --purge, what it kept goes too; with --no-cleanup, it is not run.
    set_up();
    let said = ok(root, &["plugin", "disable", "hello", "--purge", "--no-cleanup"]);
    assert_eq!(said, "hello is disabled in this repository, and what it kept here is removed.\n");
    assert!(rules.is_file() && !own.exists());
    fs::remove_file(&rules).unwrap();

    // A cleanup that fails is said, and the plugin goes all the same.
    set_up();
    fs::write(root.join("CLEANUP_FAIL"), "").unwrap();
    let out = kito(root, &["plugin", "disable", "hello"]);
    let warned = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{warned}");
    assert!(
        warned.contains("did not take out what it wrote here") && warned.contains("it would not come out"),
        "{warned}"
    );
    assert!(warned.contains("it goes all the same"), "{warned}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello is disabled in this repository.\n");
    assert!(
        ok(root, &["plugin", "list"]).contains("not enabled") || !ok(root, &["status"]).contains("Plugin")
    );
    fs::remove_file(root.join("CLEANUP_FAIL")).unwrap();

    // Removed from the person's machine: one last time for what it wrote
    // outside any repository, and the repository that still wants it is said.
    set_up();
    let told = root.join("cleaned");
    let out = command(root, &["plugin", "remove", "hello"])
        .env("KITO_NO_CLAUDE", "1")
        .env("KITO_HELLO_CLEANED", &told)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.starts_with("hello is removed.\nThis repository still enables it"), "{said}");
    assert!(said.contains("`kito plugin disable hello`"), "{said}");
    assert_eq!(fs::read_to_string(&told).unwrap(), "person server=false\n");
    assert!(!root.join(".absent-personal-config/plugins/hello").exists());
    // Gone, it can still be disabled here: nothing is left to run.
    assert_eq!(
        ok(root, &["plugin", "disable", "hello", "--purge"]),
        "hello is disabled in this repository, and what it kept here is removed.\n"
    );
    assert!(!own.exists());
    let again = kito(root, &["plugin", "remove", "hello"]);
    assert!(String::from_utf8_lossy(&again.stderr).contains("the plugin `hello` is not installed"));
    ok(root, &["daemon", "stop"]);
}

#[test]
fn a_registry_is_signed_and_checked_against_its_key() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let (registry, mine) = (base.join("registry"), base.join("my-config"));
    fs::create_dir_all(registry.join("plugins")).unwrap();
    // `key`: what the person checks the registry with, or nothing.
    let me = |key: Option<&str>, args: &[&str]| -> (bool, String) {
        let mut command = command(&base, args);
        command
            .env("KITO_NO_CLAUDE", "1")
            .env("KITO_CONFIG_DIR", &mine)
            .env("KITO_REGISTRY", format!("file://{}", registry.display()))
            .env_remove("KITO_REGISTRY_KEY");
        if let Some(key) = key {
            command.env("KITO_REGISTRY_KEY", key);
        }
        let out = command.output().unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };

    // A registry of one plugin, as its keeper has it.
    let source = hello_plugin(&base, LISTED);
    let platform = kitod::registry::platform();
    let archive = registry.join("hello.tar.gz");
    let packed = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&base)
        .arg(source.file_name().unwrap())
        .status();
    assert!(packed.unwrap().success());
    let sum = kitod::registry::sha256_hex(&fs::read(&archive).unwrap());
    let index =
        serde_json::json!({ "registry": 1, "plugins": { "hello": { "about": "x", "latest": "0.9.0" } } });
    fs::write(registry.join("index.json"), index.to_string()).unwrap();
    let plugin = serde_json::json!({ "name": "hello", "about": "x", "versions": { "0.9.0": {
        "api": 2, "files": { platform.as_str(): { "url": "hello.tar.gz", "sha256": sum } },
    }}});
    fs::write(registry.join("plugins/hello.json"), plugin.to_string()).unwrap();

    // Its keeper makes a key, once, and signs it.
    let secret = base.join("registry.key");
    let (ok, said) = me(None, &["registry", "key", secret.to_str().unwrap()]);
    assert!(ok, "{said}");
    let public = said
        .lines()
        .find(|line| line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap()
        .to_owned();
    assert!(said.contains(&format!("KITO_REGISTRY_KEY={public}")), "{said}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&secret).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let (ok, said) = me(None, &["registry", "key", secret.to_str().unwrap()]);
    assert!(!ok && said.contains("a key is never written over"), "{said}");

    // Not signed yet: whoever checks it is refused, whoever does not is told so.
    let (ok, said) = me(Some(&public), &["plugin", "search"]);
    assert!(
        !ok && said.contains("it is not signed, and this registry's files are checked against its key"),
        "{said}"
    );
    let (ok, said) = me(None, &["plugin", "add", "hello"]);
    assert!(
        ok && said.contains("the registry was not checked against a key: nothing but its address says"),
        "{said}"
    );

    let (ok, said) =
        me(None, &["registry", "sign", registry.to_str().unwrap(), "--key", secret.to_str().unwrap()]);
    assert!(ok && said.contains("its index is number 1, with 1 plugin.") && said.contains(&public), "{said}");
    assert!(registry.join("index.json.sig").is_file() && registry.join("plugins/hello.json.sig").is_file());
    let (ok, said) = me(Some(&public), &["plugin", "add", "hello"]);
    assert!(ok && said.contains("the registry's files were checked against its key"), "{said}");
    assert_eq!(me(Some(&public), &["plugin", "search"]).1, "hello 0.9.0  x\n");

    // Changed since it was signed, or signed by somebody else: nothing of it is taken.
    let listed = fs::read_to_string(registry.join("plugins/hello.json")).unwrap();
    fs::write(registry.join("plugins/hello.json"), listed.replace(&sum, &"0".repeat(64))).unwrap();
    let (ok, said) = me(Some(&public), &["plugin", "add", "hello"]);
    assert!(
        !ok && said.contains("it is not signed with a key of this registry: nothing of it is taken"),
        "{said}"
    );
    fs::write(registry.join("plugins/hello.json"), &listed).unwrap();
    let other = "ab".repeat(32);
    let (ok, said) = me(Some(&other), &["plugin", "search"]);
    assert!(!ok && said.contains("not signed with a key of this registry"), "{said}");
    // A key that cannot be read is not "no key".
    let (ok, said) = me(Some("not-a-key"), &["plugin", "search"]);
    assert!(!ok && said.contains("KITO_REGISTRY_KEY: `not-a-key` is not a key of a registry"), "{said}");
    // One of two keys: how a registry changes its key.
    assert!(me(Some(&format!("{other},{public}")), &["plugin", "search"]).0);

    // Signed again, its index has the next number, and does not go back.
    let before: Vec<(String, Vec<u8>)> = ["index.json", "index.json.sig"]
        .iter()
        .map(|file| ((*file).to_owned(), fs::read(registry.join(file)).unwrap()))
        .collect();
    let (ok, said) =
        me(None, &["registry", "sign", registry.to_str().unwrap(), "--key", secret.to_str().unwrap()]);
    assert!(ok && said.contains("number 2"), "{said}");
    assert!(me(Some(&public), &["plugin", "search"]).0);
    for (file, bytes) in &before {
        fs::write(registry.join(file), bytes).unwrap();
    }
    let (ok, said) = me(Some(&public), &["plugin", "search"]);
    assert!(!ok && said.contains("an older one (number 1) than this machine has seen (number 2)"), "{said}");
}

#[test]
fn a_lane_says_which_assistant_works_in_it_and_on_what_model() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let root = root.as_path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::write(root.join("README.md"), "# Shop\n").unwrap();
    fs::write(root.join("NOTES.md"), "# Notes\n").unwrap();
    ok(root, &["checkpoint", "-m", "first version"]);
    let assistant = |lane: usize| json(root, &["--json", "lanes"])[lane]["assistant"].clone();

    // Said when the lane is opened, and shown wherever the lane is.
    let open = ["lane", "open", "alice", "--intent", "say what the shop sells", "--scope", "README.md"];
    ok(root, &[&open[..], &["--assistant", "Codex", "--model", "gpt-5-codex"][..]].concat());
    assert_eq!(assistant(0), serde_json::json!({ "name": "codex", "model": "gpt-5-codex" }));
    assert!(ok(root, &["lanes"]).contains("  in codex (gpt-5-codex)"));
    assert!(ok(root, &["board"]).contains(" in codex (gpt-5-codex)  active"));
    // A model is somebody's: it is not said alone.
    let alone = kito(root, &["lane", "open", "x", "--intent", "y", "--scope", "README.md", "--model", "m"]);
    assert!(!alone.status.success());

    // A lane nobody said anything of says nothing.
    ok(root, &["lane", "open", "bob", "--intent", "a line about returns", "--scope", "NOTES.md"]);
    assert_eq!(assistant(1), Value::Null);
    assert!(!ok(root, &["lanes"]).lines().nth(1).unwrap().contains(" in "));

    // Claude Code says its model when a session starts, and of which
    // session an edit is: the lane the edit is in says both from then on.
    let work = root.join(".kito/lanes/2/work");
    let started = serde_json::json!({
        "hook_event_name": "SessionStart", "session_id": "session-1", "cwd": root, "model": "claude-opus-5",
    });
    let said = hook_in(root, &["hook", "session-start"], &started, true);
    assert_eq!(said.1, 0, "{said:?}");
    let mut announced = edit(&work.join("NOTES.md"), "# Notes", "# Returns");
    announced["session_id"] = "session-1".into();
    assert_eq!(refusal(&hook_in(root, &["hook", "before-edit"], &announced, true)), "");
    assert_eq!(assistant(1), serde_json::json!({ "name": "claude-code", "model": "claude-opus-5" }));
    // An edit of a session that said nothing adds nothing, and takes nothing away.
    announced["session_id"] = "session-2".into();
    assert_eq!(refusal(&hook_in(root, &["hook", "before-edit"], &announced, true)), "");
    assert_eq!(assistant(1), serde_json::json!({ "name": "claude-code", "model": "claude-opus-5" }));
    // An assistant that does not mark what it runs is not taken for Claude Code.
    ok(root, &["lane", "open", "carol", "--intent", "notes in order", "--scope", "NOTES.md"]);
    let other = edit(&root.join(".kito/lanes/3/work/NOTES.md"), "# Notes", "# Notes, in order");
    assert_eq!(refusal(&hook_in(root, &["hook", "before-edit"], &other, false)), "");
    assert_eq!(assistant(2), Value::Null);

    // Claude Code's mark is only a guess of who asks: in a lane another
    // program was named for, neither it nor the session's model is taken.
    let mut marked = edit(&root.join(".kito/lanes/1/work/README.md"), "# Shop", "# The shop");
    marked["session_id"] = "session-1".into();
    assert_eq!(refusal(&hook_in(root, &["hook", "before-edit"], &marked, true)), "");
    assert_eq!(assistant(0), serde_json::json!({ "name": "codex", "model": "gpt-5-codex" }));

    // Another assistant takes the first lane over: what was known of the
    // one before goes with it. What it says later of itself is added.
    let file = root.join(".kito/lanes/1/work/README.md");
    let file = file.to_str().unwrap();
    ok(root, &["hook", "before-edit", "--file", file, "--assistant", "zcode"]);
    assert_eq!(assistant(0), serde_json::json!({ "name": "zcode" }));
    ok(root, &["hook", "before-edit", "--file", file, "--assistant", "zcode", "--model", "glm-5"]);
    assert_eq!(assistant(0), serde_json::json!({ "name": "zcode", "model": "glm-5" }));
    // It is still said once the daemon started again: the lane keeps it.
    ok(root, &["daemon", "stop"]);
    assert_eq!(assistant(0), serde_json::json!({ "name": "zcode", "model": "glm-5" }));
    ok(root, &["daemon", "stop"]);
}

#[test]
fn every_assistant_is_added_the_same_way_and_new_projects_get_the_ones_said() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let (home, bin, config) = (base.join("home"), base.join("bin"), base.join("config"));
    fs::create_dir_all(home.join(".zcode")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    // What Kito looks for: Codex's program on the PATH, ZCode's settings
    // in the home directory. Claude Code is nowhere.
    let codex = bin.join(format!("codex{}", std::env::consts::EXE_SUFFIX));
    fs::write(&codex, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([bin.clone(), PathBuf::from("/usr/bin"), PathBuf::from("/bin")]).unwrap();
    let run = |cwd: &Path, args: &[&str]| -> Output {
        command(cwd, args)
            .env("HOME", &home)
            .env("PATH", &path)
            .env("KITO_CONFIG_DIR", &config)
            .env("KITO_REGISTRY", "file:///nonexistent-kito-registry")
            .env_remove("KITO_NO_CLAUDE")
            .env_remove("USERPROFILE")
            .output()
            .unwrap()
    };
    let good = |cwd: &Path, args: &[&str]| -> String {
        let out = run(cwd, args);
        let (said, errors) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "kito {args:?}: {said}{errors}");
        said.into_owned()
    };
    let project = |name: &str| -> PathBuf {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    };

    // What this machine has, and how each would be set up.
    let listed = good(&base, &["assistant", "list"]);
    assert!(listed.contains("codex    Codex        on this machine (`codex` on the PATH)"), "{listed}");
    // `~/.zcode`, or `~\.zcode` where that is how a path is written.
    let zcode = format!("zcode    ZCode        on this machine (~{}.zcode)", std::path::MAIN_SEPARATOR);
    assert!(listed.contains(&zcode), "{listed}");
    assert!(listed.contains("claude   Claude Code  not found here"), "{listed}");
    assert!(listed.contains("plugin codex, from the registry"), "{listed}");
    assert!(listed.contains("`kito init` asks"), "{listed}");

    // Nobody at the terminal and nothing said: Claude Code, as always, and
    // the others found are named with the command that adds them.
    let a = project("a");
    let made = good(&a, &["init"]);
    assert!(made.contains("Set up for Claude Code"), "{made}");
    assert!(
        made.contains(
            "Also on this machine: Codex, ZCode. `kito assistant add codex zcode` sets them up here."
        ),
        "{made}"
    );
    assert!(a.join(".mcp.json").is_file());
    // The same command for every assistant; one the registry cannot give is said.
    let out = run(&a, &["assistant", "add", "codex"]);
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success()
            && said.contains("Codex is not set up: its plugin `codex` could not be installed"),
        "{said}"
    );
    // Claude Code comes out the same way it went in.
    let out = good(&a, &["assistant", "remove", "claude"]);
    assert!(out.contains("Claude Code is no longer set up for Kito here"), "{out}");
    assert!(!a.join(".mcp.json").exists() && !a.join("CLAUDE.md").exists());
    let out = good(&a, &["assistant", "add", "claude"]);
    assert!(out.contains("Set up for Claude Code") && a.join("CLAUDE.md").is_file(), "{out}");

    // Said once, for every new project: a plugin installed and not enabled
    // anywhere is enabled where it is wanted.
    let manifest = "name = \"hello\"\nversion = \"0.3.0\"\napi = 1\npermissions = [\"read\"]\n\n[assistant]\nname = \"hello\"\n";
    let source = hello_plugin(&base, manifest);
    good(&base, &["plugin", "add", "--path", source.to_str().unwrap()]);
    let said = good(&base, &["assistant", "default", "hello", "claude"]);
    assert!(said.contains("New projects get: hello, Claude Code"), "{said}");
    assert!(fs::read_to_string(config.join("config.toml"))
        .unwrap()
        .contains("default = [\"hello\", \"claude\"]"));
    assert!(good(&base, &["assistant", "default"]).contains("New projects get: hello, claude."));
    let b = project("b");
    let made = good(&b, &["init"]);
    assert!(made.contains("Set up for hello:") && made.contains("enabled the plugin hello here"), "{made}");
    assert!(!made.contains("Also on this machine"), "{made}");
    assert!(b.join("HELLO.md").is_file() && b.join(".mcp.json").is_file());

    // For one project: what it is told, or nothing.
    let c = project("c");
    good(&c, &["init", "--assistant", "hello"]);
    assert!(c.join("HELLO.md").is_file() && !c.join(".mcp.json").exists());
    let d = project("d");
    good(&d, &["init", "--no-assistants"]);
    assert!(!d.join("HELLO.md").exists() && !d.join(".mcp.json").exists());
    let e = project("e");
    good(&e, &["init", "--no-claude"]);
    assert!(e.join("HELLO.md").is_file() && !e.join(".mcp.json").exists());

    // No assistant for new projects, said once.
    assert!(good(&base, &["assistant", "default", "--none"]).contains("New projects get no assistant"));
    let f = project("f");
    let made = good(&f, &["--json", "init"]);
    let made: Value = serde_json::from_str(&made).unwrap();
    assert_eq!(made["assistants"]["set_up"], serde_json::json!([]));
    assert!(!f.join(".mcp.json").exists());

    for dir in [&a, &b, &c, &d, &e, &f] {
        run(dir, &["daemon", "stop"]);
    }
}

#[test]
fn instructions_are_kept_by_kito_written_for_each_assistant_and_a_kitchen_comes_first() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("shop");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("app.ts"), "export const a = 1;\n").unwrap();
    kito_as_installed(&root, &["init"]);
    let kitchen = tmp.path().join("kitchen");
    fs::create_dir_all(kitchen.join("instructions/all")).unwrap();
    fs::write(kitchen.join("instructions/all/style.md"), "Two spaces.\n").unwrap();
    fs::write(kitchen.join("instructions/all/tests.md"), "Run the tests.\n").unwrap();
    fs::create_dir_all(kitchen.join("instructions/codex")).unwrap();
    fs::write(kitchen.join("instructions/codex/plan.md"), "Plan first.\n").unwrap();

    let out = kito_as_installed(&root, &["instructions", "kitchen", kitchen.to_str().unwrap()]);
    assert!(out.contains("wrote CLAUDE.md"), "{out}");
    let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(claude.contains("\nTwo spaces.\n") && !claude.contains("Plan first."), "{claude}");

    // The repository's own word over the kitchen's; an empty one takes it away.
    let mine = tmp.path().join("style.md");
    fs::write(&mine, "# Style\n\nTabs.\n").unwrap();
    let out = kito_as_installed(&root, &["instructions", "set", "style", "--file", mine.to_str().unwrap()]);
    assert!(out.contains("all/style.md is set"), "{out}");
    fs::write(&mine, "").unwrap();
    kito_as_installed(&root, &["instructions", "set", "all/tests", "--file", mine.to_str().unwrap()]);
    let list = kito_as_installed(&root, &["instructions"]);
    assert!(list.contains(&format!("Kitchen: {}", kitchen.display())), "{list}");
    assert!(list.contains("all/style.md             this repository, over the kitchen's: Style"), "{list}");
    assert!(list.contains("codex/plan.md            the kitchen: Plan first."), "{list}");
    assert!(list.contains("all/tests.md             the kitchen's, not told here"), "{list}");
    let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(claude.contains("# Style\n\nTabs.\n") && !claude.contains("Run the tests"), "{claude}");
    let codex = kito_as_installed(&root, &["instructions", "show", "--for", "codex"]);
    assert!(
        codex.contains("Tabs.") && codex.contains("Plan first.") && codex.contains("`open_lane`"),
        "{codex}"
    );

    // Lines of one's own, outside the marks, are kept and not shared.
    fs::write(root.join("CLAUDE.md"), format!("Mine only.\n\n{claude}")).unwrap();
    kito_as_installed(&root, &["instructions", "remove", "style"]);
    let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(
        claude.starts_with("Mine only.\n\n<!-- kito: begin") && claude.contains("Two spaces."),
        "{claude}"
    );

    // None of it is in the history.
    ok(&root, &["sync"]);
    assert_eq!(json(&root, &["status", "--json"])["repo"]["files"], 2);
    // Taken out with the assistant: nothing of Kito's is left.
    fs::write(root.join("CLAUDE.md"), claude.replace("Mine only.\n\n", "")).unwrap();
    kito_as_installed(&root, &["assistant", "remove", "claude"]);
    assert!(!root.join("CLAUDE.md").exists());
    ok(&root, &["daemon", "stop"]);
}

#[test]
fn a_task_is_asked_for_done_in_lanes_and_closed_by_whoever_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.py"), SHOP_PY).unwrap();
    ok(root, &["init", "--no-assistants"]);
    let added = json(
        root,
        &["task", "add", "Prices", "in", "cents", "--text", "Everywhere money is.", "--for", "bob", "--json"],
    );
    assert_eq!(
        (added["id"].as_str(), added["state"].as_str(), added["for"].as_str()),
        (Some("task#1"), Some("open"), Some("bob"))
    );
    ok(root, &["task", "add", "A", "receipt", "for", "each", "order"]);
    let list = ok(root, &["task"]);
    assert!(list.contains("task#1   open            Prices in cents  (for bob)"), "{list}");
    assert!(list.contains("task#2   open            A receipt for each order"), "{list}");

    // A lane for a task that is not there, or no longer to do, is refused.
    let none = kito(
        root,
        &[
            "lane",
            "open",
            "erin",
            "--intent",
            "prices in cents",
            "--scope",
            "src/shop.py",
            "--task",
            "task#9",
        ],
    );
    assert!(String::from_utf8_lossy(&none.stderr).contains("task#9: no such task"));
    ok(
        root,
        &[
            "lane",
            "open",
            "erin",
            "--intent",
            "prices in cents",
            "--scope",
            "src/shop.py",
            "--task",
            "task#1",
        ],
    );
    let shown = ok(root, &["task", "show", "task#1"]);
    assert!(
        shown.contains("in a lane") && shown.contains("lane/1") && shown.contains("Everywhere money is."),
        "{shown}"
    );
    ok(root, &["task", "comment", "task#1", "Round", "half", "to", "even."]);
    assert!(ok(root, &["task", "show", "1"]).contains("Round half to even."));

    // Its last lane closed: ready to close, and closed by a person.
    ok(root, &["lane", "close", "1"]);
    let ready = ok(root, &["task", "show", "task#1"]);
    assert!(ready.contains("ready to close") && ready.contains("kito task done task#1"), "{ready}");
    ok(root, &["task", "done", "task#1"]);
    assert!(!ok(root, &["task"]).contains("task#1"));
    assert!(ok(root, &["task", "list", "--all"]).contains("task#1   done"));
    let closed = kito(
        root,
        &["lane", "open", "erin", "--intent", "prices again", "--scope", "src/shop.py", "--task", "task#1"],
    );
    assert!(String::from_utf8_lossy(&closed.stderr).contains("task#1 is done: reopen it first"));
    ok(root, &["task", "drop", "2"]);
    assert_eq!(ok(root, &["task"]).trim(), "No task to do. `kito task add <title>` asks for one.");
    // Kept by Kito, not in the project's files.
    assert!(root.join(".kito/tasks.json").is_file());
    ok(root, &["daemon", "stop"]);
}

#[test]
fn what_was_done_is_found_and_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.py"), SHOP_PY).unwrap();
    ok(root, &["init", "--no-assistants"]);
    ok(root, &["checkpoint", "-m", "start"]);
    ok(root, &["task", "add", "Prices", "in", "cents"]);
    ok(
        root,
        &[
            "lane",
            "open",
            "erin",
            "--intent",
            "prices in whole cents",
            "--scope",
            "src/shop.py",
            "--task",
            "task#1",
        ],
    );
    fs::write(root.join(".kito/lanes/1/work/src/shop.py"), format!("{SHOP_PY}\n# cents\n")).unwrap();
    ok(root, &["sync", "1"]);
    assert!(ok(root, &["integrate", "1"]).contains("integrated"));
    ok(
        root,
        &["lane", "open", "frank", "--intent", "a receipt for every order", "--scope", "src/receipt.py"],
    );
    ok(root, &["lane", "close", "2"]);

    // Found by its words, its place, who did it, and how it ended.
    let found = json(root, &["search", "cents", "--json"]);
    let kinds: Vec<&str> = found.as_array().unwrap().iter().filter_map(|hit| hit["kind"].as_str()).collect();
    assert!(kinds.contains(&"lane") && kinds.contains(&"task") && kinds.contains(&"change"), "{found}");
    let lanes = json(root, &["search", "--kind", "lane", "--where", "src/shop.py", "--json"]);
    assert_eq!(lanes.as_array().unwrap().len(), 1, "only erin.s lane was on src/shop.py: {lanes}");
    assert_eq!(lanes[0]["outcome"][0], "integrated");
    let given_up = json(root, &["search", "--kind", "lane", "--outcome", "given_up", "--json"]);
    assert_eq!(given_up[0]["by"], "frank", "{given_up}");
    assert_eq!(json(root, &["search", "--who", "nobody-at-all", "--json"]), serde_json::json!([]));
    assert!(ok(root, &["search", "receipt"]).contains("a receipt for every order"));

    // Counted.
    let stats = json(root, &["stats", "--json"])["this"].clone();
    let weeks = stats["flow"]["weeks"].as_array().unwrap();
    let sum = |key: &str| weeks.iter().map(|week| week[key].as_u64().unwrap()).sum::<u64>();
    assert_eq!((sum("opened"), sum("integrated"), sum("given_up")), (2, 1, 1), "{stats}");
    assert!(stats["flow"]["median_open_to_integrated_ms"].is_u64(), "{stats}");
    assert!(
        stats["hotspots"]["files"].as_array().unwrap().iter().any(|file| file["path"] == "src/shop.py"),
        "{stats}"
    );
    let said = ok(root, &["stats", "--since", "7d"]);
    assert!(said.contains("Lanes: 2 opened, 1 integrated, 1 given up"), "{said}");
    assert!(root.join(".kito/activity.jsonl").is_file());
    assert!(kito(root, &["stats", "--since", "yesterday"]).status.code() != Some(0));
    ok(root, &["daemon", "stop"]);
}

/// A server of its own, running until dropped, with a repository `shop`
/// and ada, an admin, and bob, who may write in it: its address and their tokens.
struct Running {
    child: std::process::Child,
    dir: PathBuf,
    address: String,
    ada: String,
    bob: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = kito(&self.dir.join("repos/shop"), &["daemon", "stop"]);
    }
}

fn running(base: &Path, name: &str) -> Running {
    let dir = base.join(name);
    let server = |args: &[&str]| {
        let mut command = Command::new(kito_server());
        command
            .arg("--dir")
            .arg(&dir)
            .args(args)
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", base.join(".server-config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        command
    };
    let run = |args: &[&str]| -> String {
        let out = server(args).output().unwrap();
        assert!(out.status.success(), "kito-server {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let src = base.join(format!("{name}-src"));
    fs::create_dir_all(src.join("src")).unwrap();
    fs::write(src.join("src/shop.py"), SHOP_PY).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "start"]);
    run(&["init", dir.to_str().unwrap()]);
    run(&["repo", "add", "shop", "--from", src.to_str().unwrap()]);
    let ada = run(&["user", "add", "ada", "--role", "admin"]);
    let ada = ada.lines().find(|line| line.starts_with("kito_")).unwrap().trim().to_owned();
    let bob = run(&["user", "add", "bob", "--repo", "shop:write"]);
    let bob = bob.lines().find(|line| line.starts_with("kito_")).unwrap().trim().to_owned();
    let mut child = server(&["serve", "--listen", "127.0.0.1:0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line)
        .unwrap();
    let address =
        line.trim().strip_prefix("kito-server: listening on ").unwrap().split(',').next().unwrap().to_owned();
    Running { child, dir, address, ada, bob }
}

#[test]
fn servers_are_connected_to_kept_switched_between_and_forgotten() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let (one, two) = (running(&base, "one"), running(&base, "two"));
    let me = base.join(".ada");
    let kito = |cwd: &Path, args: &[&str]| kito_as(&me, cwd, args);
    let ok = |cwd: &Path, args: &[&str]| ok_as(&me, cwd, args);
    let errors = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();
    let kept = || fs::read_to_string(me.join("servers.toml")).unwrap();

    // Nothing known yet: said, with what to do.
    let none = kito(&base, &["repos"]);
    assert!(
        !none.status.success() && errors(&none).contains("`kito connect <server>` first"),
        "{}",
        errors(&none)
    );
    assert!(ok(&base, &["server"]).contains("No server yet"));

    // The server alone: signed in, kept, current; no directory is made.
    let said = ok(&base, &["connect", &one.address, "--token", &one.ada]);
    assert!(said.contains(&format!(
        "Connected to {} as ada: `{}` is your current server.",
        one.address, one.address
    )));
    assert!(said.contains("The repositories you may use there: shop."), "{said}");
    assert!(said.contains("`kito clone shop` makes a directory of one."), "{said}");
    assert!(!base.join("shop").exists());
    assert!(kept().starts_with(&format!("current = \"{}\"\n", one.address)), "{}", kept());
    assert!(kept().contains(&format!("token = \"{}\"", one.ada)) && kept().contains("user = \"ada\""));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(me.join("servers.toml")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "servers.toml is its owner's alone");
    }

    // A directory of a repository of it: it names the server, no token.
    let cloned = ok(&base, &["clone", "shop"]);
    assert!(cloned.contains("is connected to `shop` on") && cloned.contains("as ada."), "{cloned}");
    let shop = base.join("shop");
    assert_eq!(
        fs::read_to_string(shop.join(".kito/remote.toml")).unwrap(),
        format!("server = \"{}\"\nrepo = \"shop\"\n", one.address)
    );
    assert_eq!(fs::read_to_string(shop.join("src/shop.py")).unwrap(), SHOP_PY);
    let refused = kito(&base, &["clone", "nope"]);
    assert!(errors(&refused).contains("there is no repository `nope` for ada"), "{}", errors(&refused));
    assert!(!base.join("nope").exists());
    // Connected again while what is kept lets ada in: nobody signs in.
    assert!(ok(&base, &["connect", &one.address]).contains("as ada"));

    // A second server, under a name of her choosing: current now.
    ok(&base, &["connect", &two.address, "--token", &two.ada, "--as", "two"]);
    assert_eq!(ok(&base, &["repos"]), "shop\n");
    ok(&base, &["repos", "add", "notes"]);
    assert!(two.dir.join("repos/notes/README.md").is_file() && !one.dir.join("repos/notes").exists());
    let listed = ok(&base, &["server", "list"]);
    let lines: Vec<String> =
        listed.lines().map(|line| line.split_whitespace().collect::<Vec<_>>().join(" ")).collect();
    assert_eq!(
        lines,
        [format!("{} {} as ada", one.address, one.address), format!("* two {} as ada", two.address)],
        "{listed}"
    );
    assert!(!listed.contains("kito_"), "no token is ever listed: {listed}");
    let listed = ok(&base, &["--json", "server", "list"]);
    let parsed: Value = serde_json::from_str(&listed).unwrap();
    assert!(
        parsed["current"] == "two" && parsed["servers"][1]["user"] == "ada" && !listed.contains("kito_"),
        "{listed}"
    );

    // A connected directory uses its own server whatever is current;
    // --server says another, for one command.
    assert_eq!(ok(&shop, &["repos"]), "shop  (this directory)\n");
    assert_eq!(ok(&shop, &["repos", "--server", "two"]), "notes\nshop\n");
    assert_eq!(ok(&base, &["repos", "--server", &one.address]), "shop\n");
    let unknown = kito(&base, &["repos", "--server", "three"]);
    assert!(errors(&unknown).contains("`three` is not one of your servers"), "{}", errors(&unknown));
    ok(&base, &["clone", "notes", "two-notes"]);
    assert!(base.join("two-notes/README.md").is_file());
    let people = ok(&base, &["people", "--server", &one.address]);
    assert!(people.starts_with("ada"), "{people}");
    let backups = ok(&base, &["backup"]);
    assert!(backups.contains(&two.address), "{backups}");
    assert!(ok(&base, &["audit", "--server", "two", "--kind", "repo"]).contains("notes"));

    // Switched: the current one answers.
    assert!(ok(&base, &["server", "use", &one.address]).contains("is your current server"));
    assert_eq!(ok(&base, &["repos"]), "shop\n");
    let wrong = kito(&base, &["server", "use", "three"]);
    assert!(!wrong.status.success() && errors(&wrong).contains("`three` is not one of your servers"));

    // The old form still makes the directory, and keeps the server it names.
    let old = ok(&base, &["connect", &format!("{}/shop", two.address), "two-shop", "--token", &two.ada]);
    assert!(
        old.contains("is connected to `shop` on") && old.contains("The main lane is in it (1 file)"),
        "{old}"
    );
    assert!(kept().starts_with(&format!("current = \"{}\"\n", two.address)), "{}", kept());
    assert_eq!(
        fs::read_to_string(base.join("two-shop/.kito/remote.toml")).unwrap(),
        format!("server = \"{}\"\nrepo = \"shop\"\n", two.address)
    );

    // Forgotten: listed no more, and its directories say so. A token is
    // the person's: no session to end.
    let said = ok(&base, &["server", "remove", "two"]);
    assert!(said.contains("`two` is forgotten") && !said.contains("session"), "{said}");
    assert!(!ok(&base, &["server", "list"]).contains("* two"));
    let notes = base.join("two-notes");
    let gone = kito(&notes, &["repos"]);
    assert!(errors(&gone).contains("two is not one of your servers"), "{}", errors(&gone));
    for dir in [&shop, &notes, &base.join("two-shop")] {
        ok(dir, &["daemon", "stop"]);
    }
}

#[test]
fn a_directory_connected_the_old_way_keeps_working_and_comes_to_name_its_server() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let one = running(&base, "one");
    let me = base.join(".ada");
    let ok = |cwd: &Path, args: &[&str]| ok_as(&me, cwd, args);
    // Two directories connected before 0.40: an address and a token each.
    let fingerprint = String::from_utf8(
        Command::new(kito_server()).arg("--dir").arg(&one.dir).arg("fingerprint").output().unwrap().stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let (first, second) = (base.join("first"), base.join("second"));
    let old = format!(
        "server = \"{}\"\nrepo = \"shop\"\ntoken = \"{}\"\nfingerprint = \"{fingerprint}\"\n",
        one.address, one.ada
    );
    for dir in [&first, &second] {
        fs::create_dir_all(dir.join(".kito")).unwrap();
        fs::write(dir.join(".kito/remote.toml"), &old).unwrap();
    }

    // Two commands at once, one in each: both work, and take turns to
    // move their directory to the form that names its server.
    let ran: Vec<Output> = [&first, &second]
        .map(|dir| {
            let (dir, me) = (dir.clone(), me.clone());
            std::thread::spawn(move || kito_as(&me, &dir, &["repos"]))
        })
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    for out in &ran {
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "shop  (this directory)\n");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains(&format!("names its server now: `{}`", one.address)), "{said}");
    }
    let named = format!("server = \"{}\"\nrepo = \"shop\"\n", one.address);
    for dir in [&first, &second] {
        assert_eq!(fs::read_to_string(dir.join(".kito/remote.toml")).unwrap(), named);
    }
    let kept = fs::read_to_string(me.join("servers.toml")).unwrap();
    assert_eq!(kept.matches("[servers.").count(), 1, "{kept}");
    assert!(kept.contains(&format!("fingerprint = \"{fingerprint}\"")) && kept.contains(&one.ada), "{kept}");
    // Moved once: said once.
    let again = kito_as(&me, &first, &["repos"]);
    assert!(again.status.success() && again.stderr.is_empty(), "{}", String::from_utf8_lossy(&again.stderr));
    // Its daemon and its lanes work as before.
    assert!(ok(&first, &["lanes"]).contains("No open lanes"));
    assert!(ok(&first, &["lane", "attach", "main"]).contains("lane/main"));
    assert_eq!(fs::read_to_string(first.join("src/shop.py")).unwrap(), SHOP_PY);
    // A directory nobody moved yet, read by the daemon alone, works too:
    // the daemon never writes it.
    let third = base.join("third");
    fs::create_dir_all(third.join(".kito")).unwrap();
    fs::write(third.join(".kito/remote.toml"), &old).unwrap();
    let remote = kitod::client::Remote::load(&third).unwrap();
    assert_eq!((remote.token.as_deref(), remote.name), (Some(one.ada.as_str()), None));
    let mut daemon = Command::new(kitod())
        .arg("--repo")
        .arg(&third)
        .env("KITO_CONFIG_DIR", &me)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut asked = None;
    for _ in 0..200 {
        if let Ok(mut client) = kitod::client::Client::connect(&third) {
            asked = client.call::<Value>("lanes", serde_json::json!({})).ok();
            if asked.is_some() {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert_eq!(asked, Some(serde_json::json!([])));
    assert_eq!(fs::read_to_string(third.join(".kito/remote.toml")).unwrap(), old);
    for dir in [&first, &second] {
        ok(dir, &["daemon", "stop"]);
    }
}

#[test]
fn the_repository_you_are_in_is_sent_and_connected_to_it_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let one = running(&base, "one");
    let me = base.join(".ada");
    let kito = |cwd: &Path, args: &[&str]| kito_as(&me, cwd, args);
    let ok = |cwd: &Path, args: &[&str]| ok_as(&me, cwd, args);
    ok(&base, &["connect", &one.address, "--token", &one.ada]);

    // A repository of this machine, with a file Kito does not record.
    let mine = base.join("mine");
    fs::create_dir_all(mine.join("src")).unwrap();
    fs::write(mine.join("src/notes.py"), "def note():\n    return 1\n").unwrap();
    fs::write(mine.join(".kitoignore"), "*.log\n").unwrap();
    fs::write(mine.join("debug.log"), "mine alone\n").unwrap();
    ok(&mine, &["init"]);
    ok(&mine, &["checkpoint", "-m", "a note"]);
    // Changed after the checkpoint: the main lane has it as it is now.
    fs::write(mine.join("src/notes.py"), "def note():\n    return 2\n").unwrap();
    ok(&mine, &["sync"]);
    let history = ok(&mine, &["log"]);
    // A permanent lane goes with it, and is named.
    assert!(ok(&mine, &["lane", "create", "staging"]).contains("staging"));
    // Written just before: recorded before anything is packed.
    fs::write(mine.join("src/late.py"), "late = True\n").unwrap();

    // Sent from a directory inside it: the repository it is in.
    let said = ok(&mine.join("src"), &["repos", "add", "notes", "--push", "."]);
    assert!(
        said.contains("`notes` is on") && said.contains("This directory is connected to it now"),
        "{said}"
    );
    assert!(said.contains("Its permanent lanes (staging) are in the repository on the server"), "{said}");
    assert_eq!(fs::read_to_string(one.dir.join("repos/notes/src/late.py")).unwrap(), "late = True\n");
    // The local history is kept, whole, beside it, and named.
    let aside: Vec<PathBuf> = fs::read_dir(&mine)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with(".kito-local-"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(aside[0].join("format").is_file() && aside[0].join("remote.toml").metadata().is_err());
    assert!(said.contains(&aside[0].display().to_string()), "{said}");
    // The directory names the server; its files are as they were.
    assert_eq!(
        fs::read_to_string(mine.join(".kito/remote.toml")).unwrap(),
        format!("server = \"{}\"\nrepo = \"notes\"\n", one.address)
    );
    assert_eq!(fs::read_to_string(mine.join("src/notes.py")).unwrap(), "def note():\n    return 2\n");
    assert_eq!(fs::read_to_string(mine.join("debug.log")).unwrap(), "mine alone\n");
    assert_eq!(
        fs::read_to_string(one.dir.join("repos/notes/src/notes.py")).unwrap(),
        "def note():\n    return 2\n"
    );
    // It works as a directory of the repository on the server, history and all.
    assert_eq!(ok(&mine, &["repos"]), "notes  (this directory)\nshop\n");
    assert!(ok(&mine, &["log"]).contains("a note") && history.contains("a note"));
    let opened = ok(
        &mine,
        &["lane", "open", "builder", "--intent", "notes count from two", "--scope", "src/notes.py"],
    );
    assert!(opened.contains("lane/2 opened"), "{opened}");
    ok(&mine, &["lane", "close", "2"]);
    let status = ok(&mine, &["daemon", "status"]);
    assert!(status.contains("Connected to ...... `notes` on"), "{status}");
    assert!(!status.contains(".kito-local-"), "the history set aside is no file of the project: {status}");

    // Only sent: the repository stays as it is.
    let kept = base.join("kept");
    fs::create_dir_all(&kept).unwrap();
    fs::write(kept.join("kept.py"), "x = 1\n").unwrap();
    ok(&kept, &["init"]);
    ok(&kept, &["checkpoint", "-m", "kept"]);
    let said = ok(&kept, &["repos", "add", "kept", "--push", ".", "--keep-local"]);
    assert!(said.contains("`kept` is on") && !said.contains("connected to it now"), "{said}");
    assert!(kept.join(".kito/format").is_file() && !kept.join(".kito/remote.toml").exists());
    assert!(one.dir.join("repos/kept/kept.py").is_file());
    assert!(kito(&kept, &["repos", "add", "x", "--keep-local"]).status.code() != Some(0));
    for dir in [&mine, &kept] {
        ok(dir, &["daemon", "stop"]);
    }
}

#[test]
fn a_repository_with_open_lanes_is_not_sent_to_be_connected_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let one = running(&base, "one");
    let me = base.join(".ada");
    let kito = |cwd: &Path, args: &[&str]| kito_as(&me, cwd, args);
    let ok = |cwd: &Path, args: &[&str]| ok_as(&me, cwd, args);
    ok(&base, &["connect", &one.address, "--token", &one.ada]);
    let busy = base.join("busy");
    fs::create_dir_all(&busy).unwrap();
    fs::write(busy.join("busy.py"), "def busy():\n    return 1\n").unwrap();
    ok(&busy, &["init"]);
    ok(&busy, &["checkpoint", "-m", "busy"]);
    ok(&busy, &["lane", "open", "builder", "--intent", "busy returns two", "--scope", "busy.py"]);

    // Refused before anything is sent, and why.
    let refused = kito(&busy, &["repos", "add", "busy", "--push", "."]);
    let said = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(!refused.status.success() && said.contains("this repository has open lanes (lane/1)"), "{said}");
    assert!(said.contains("close or integrate them first, or only send it with --keep-local"), "{said}");
    assert!(!one.dir.join("repos/busy").exists());
    assert!(busy.join(".kito/format").is_file() && !busy.join(".kito/remote.toml").exists());
    let names: Vec<String> = fs::read_dir(&busy)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|name| !name.starts_with(".kito-local-")), "{names:?}");
    // The lane goes on here.
    assert!(ok(&busy, &["lanes"]).contains("lane/1"));
    ok(&busy, &["daemon", "stop"]);
}

#[test]
fn moving_old_directories_never_changes_who_they_act_as_nor_the_current_server() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let one = running(&base, "one");
    let fingerprint = String::from_utf8(
        Command::new(kito_server()).arg("--dir").arg(&one.dir).arg("fingerprint").output().unwrap().stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let old = |token: &str| {
        format!(
            "server = \"{}\"\nrepo = \"shop\"\ntoken = \"{token}\"\nfingerprint = \"{fingerprint}\"\n",
            one.address
        )
    };
    // ada's directory and bob's, connected before 0.40 on one machine, under
    // one Kito directory: moved one after the other, then at the same time.
    for together in [false, true] {
        let me = base.join(format!(".me-{together}"));
        let (adas, bobs) = (base.join(format!("ada-{together}")), base.join(format!("bob-{together}")));
        for (dir, token) in [(&adas, &one.ada), (&bobs, &one.bob)] {
            fs::create_dir_all(dir.join(".kito")).unwrap();
            fs::write(dir.join(".kito/remote.toml"), old(token)).unwrap();
        }
        let ran: Vec<Output> = match together {
            false => [&adas, &bobs].iter().map(|dir| kito_as(&me, dir, &["repos"])).collect(),
            true => [&adas, &bobs]
                .map(|dir| {
                    let (dir, me) = (dir.clone(), me.clone());
                    std::thread::spawn(move || kito_as(&me, &dir, &["repos"]))
                })
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect(),
        };
        let mut said = String::new();
        for out in &ran {
            said.push_str(&String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success(), "{said}");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "shop  (this directory)\n");
        }
        let beside = format!(
            "another account of that server, or that server known another way, is kept as `{}`",
            one.address
        );
        assert!(said.contains(&beside), "{said}");
        // Two entries, each with its own token; each directory acts as its
        // own account, as the server answered it.
        let kept = fs::read_to_string(me.join("servers.toml")).unwrap();
        assert!(kept.contains(&one.ada) && kept.contains(&one.bob), "{kept}");
        let listed: Value = serde_json::from_str(&ok_as(&me, &base, &["--json", "server", "list"])).unwrap();
        let user_of = |dir: &Path| {
            let named = fs::read_to_string(dir.join(".kito/remote.toml")).unwrap();
            let entry = listed["servers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| {
                    named.starts_with(&format!("server = \"{}\"\n", entry["name"].as_str().unwrap()))
                })
                .unwrap_or_else(|| panic!("{named}: {listed}"))
                .clone();
            entry["user"].as_str().unwrap_or("?").to_owned()
        };
        assert_eq!((user_of(&adas), user_of(&bobs)), ("ada".to_owned(), "bob".to_owned()), "{listed}");
        // Nothing became current: a command outside a directory asks which.
        assert_eq!(listed["current"], Value::Null, "{listed}");
        let refused = kito_as(&me, &base, &["repos"]);
        let names = format!("{}, {}-2", one.address, one.address);
        let wanted = format!("no current server: `kito server use <name>` (you have: {names})");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(&wanted),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
}

#[test]
fn connecting_never_replaces_another_server_nor_another_person() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let (one, two) = (running(&base, "one"), running(&base, "two"));
    let me = base.join(".me");
    let kito = |cwd: &Path, args: &[&str]| kito_as(&me, cwd, args);
    let ok = |cwd: &Path, args: &[&str]| ok_as(&me, cwd, args);
    let errors = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();
    let kept = || fs::read_to_string(me.join("servers.toml")).unwrap();
    let (first, second) = (one.address.as_str(), two.address.as_str());

    // Another person under a name that is ada's: refused, nothing changes.
    ok(&base, &["connect", first, "--token", &one.ada]);
    let before = kept();
    let refused = kito(&base, &["connect", first, "--token", &one.bob]);
    let wanted = format!(
        "you are ada on `{first}`: to be bob there too, add `--as <name>`; to stop being ada there, `kito server \
         remove {first}` first"
    );
    assert!(!refused.status.success() && errors(&refused).contains(&wanted), "{}", errors(&refused));
    assert_eq!(kept(), before);
    // The same person signs in again: kept as now.
    assert!(ok(&base, &["connect", first, "--token", &one.ada]).contains("as ada"));
    // Two accounts of one server under two names; each directory acts as its own.
    ok(&base, &["connect", first, "--token", &one.bob, "--as", "bob"]);
    ok(&base, &["clone", "shop", "ada-shop", "--server", first]);
    ok(&base, &["clone", "shop", "bob-shop"]);
    let (adas, bobs) = (base.join("ada-shop"), base.join("bob-shop"));
    ok(&adas, &["lane", "open", "pricer", "--intent", "prices in cents", "--scope", "src/shop.py:total"]);
    ok(
        &bobs,
        &["lane", "open", "receipts", "--intent", "receipts in euros", "--scope", "src/shop.py:receipt"],
    );
    let board = ok(&adas, &["board"]);
    assert!(board.contains("pricer (of ada)") && board.contains("receipts (of bob)"), "{board}");

    // A name another server has: refused with --as, never replaced.
    let taken = kito(&base, &["connect", second, "--token", &two.ada, "--as", "bob"]);
    let wanted = format!("`bob` is the name of another server of yours ({first})");
    assert!(!taken.status.success() && errors(&taken).contains(&wanted), "{}", errors(&taken));
    // Without --as, the new one is kept under a free name, and said.
    ok(&base, &["connect", first, "--token", &one.ada, "--as", second]);
    let moved = kito(&base, &["connect", second, "--token", &two.ada]);
    let wanted =
        format!("`{second}` is the name of another server of yours: this one is kept as `{second}-2`");
    assert!(moved.status.success() && errors(&moved).contains(&wanted), "{}", errors(&moved));
    assert!(kept().contains(&format!("[servers.\"{second}-2\"]\naddress = \"{second}\"")), "{}", kept());
    assert!(kept().contains(&format!("[servers.\"{second}\"]\naddress = \"{first}\"")), "{}", kept());
    // Connected again, it finds its own name.
    let again = kito(&base, &["connect", second]);
    assert!(again.status.success() && errors(&again).contains(&format!("kept as `{second}-2`")));
    assert!(!kept().contains(&format!("{second}-3")), "{}", kept());

    // Forgetting a person's token says so, first.
    let said = ok(&base, &["server", "remove", "bob"]);
    assert!(
        said.starts_with(
            "`bob` holds your token there, which Kito keeps nowhere else: it is forgotten with it, \
                          and only an admin of that server can give you another.\n`bob` is forgotten"
        ),
        "{said}"
    );
    ok(&adas, &["lane", "close", "1"]);
    for dir in [&adas, &bobs] {
        ok(dir, &["daemon", "stop"]);
    }
}
