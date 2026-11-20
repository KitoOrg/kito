//! The real daemon (`kitod` binary), driven through the JSON-RPC socket.

use kitod::client::Client;
use kitod::repo::{CheckpointResult, LogItem};
use kitod::rpc::DaemonStatus;
use kitod::Repo;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
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
    wait_until("the daemon to shut down", || !socket.exists());
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn status(client: &mut Client) -> DaemonStatus {
    client.call("status", json!({})).unwrap()
}

fn ops(client: &mut Client) -> Vec<(String, String)> {
    let items: Vec<LogItem> = client.call("log", json!({ "limit": 1000 })).unwrap();
    items
        .into_iter()
        .filter_map(|i| match i {
            LogItem::Op { op, path, .. } => Some((op, path.unwrap_or_default())),
            LogItem::Checkpoint { .. } => None,
        })
        .collect()
}

fn new_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    Repo::init(tmp.path()).unwrap().close().unwrap();
    tmp
}

#[test]
fn saves_are_captured_without_asking() {
    let tmp = new_repo();
    let root = tmp.path();
    let mut client = start(root);

    fs::write(root.join("a.txt"), "one\n").unwrap();
    wait_until("the capture of a.txt", || status(&mut client).repo.ops == 1);
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::write(root.join("src/deep/b.txt"), "fn b() {}\n").unwrap();
    wait_until("the capture in a new directory", || status(&mut client).repo.ops == 2);
    fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    wait_until("the capture of the edit", || status(&mut client).repo.ops == 3);
    fs::rename(root.join("a.txt"), root.join("c.txt")).unwrap();
    wait_until("the capture of the rename", || status(&mut client).repo.ops == 4);
    fs::remove_dir_all(root.join("src")).unwrap();
    wait_until("the capture of the removal", || status(&mut client).repo.ops == 5);

    let expected = [
        ("FileRemove", "src/deep/b.txt"),
        ("FileRename", "c.txt"),
        ("TextEdit", "a.txt"),
        ("FileAdd", "src/deep/b.txt"),
        ("FileAdd", "a.txt"),
    ];
    let got = ops(&mut client);
    assert_eq!(got.iter().map(|(o, p)| (o.as_str(), p.as_str())).collect::<Vec<_>>(), expected);

    let s = status(&mut client);
    assert!(s.watcher.events >= 5 && s.watcher.batches >= 5, "{:?}", s.watcher);
    // The batch that created the directories waited for quiet: nothing
    // says when a directory is done being filled.
    assert!(s.watcher.latency_p95_ms.unwrap() >= s.debounce_ms as f64, "{:?}", s.watcher);
    assert!(s.watcher.capture_p95_ms.unwrap() < 1000.0);
    stop(root, &mut client);
}

/// Where the system says when a file is closed after being written, a save
/// is captured then, not after a period of quiet.
#[cfg(target_os = "linux")]
#[test]
fn a_finished_save_is_not_kept_waiting() {
    let tmp = new_repo();
    let root = tmp.path();
    // Quiet would be waited for two seconds.
    let config = fs::read_to_string(root.join(".kito/config.toml")).unwrap();
    fs::write(root.join(".kito/config.toml"), config.replace("debounce_ms = 50", "debounce_ms = 2000"))
        .unwrap();
    let mut client = start(root);
    assert_eq!(status(&mut client).debounce_ms, 2000);
    let within = |what: &str, client: &mut Client, ops: u64| {
        let started = Instant::now();
        wait_until(what, || status(client).repo.ops == ops);
        assert!(started.elapsed() < Duration::from_millis(1000), "{what}: {:?}", started.elapsed());
    };

    // A file written in place and closed.
    fs::write(root.join("a.ts"), "export function a(): number {\n  return 1;\n}\n").unwrap();
    within("a save", &mut client, 2);
    // A save through a temporary file put in place by a rename, as many
    // editors do: the file is whole when it gets its name.
    fs::write(root.join(".a.ts.tmp"), "export function a(): number {\n  return 2;\n}\n").unwrap();
    fs::rename(root.join(".a.ts.tmp"), root.join("a.ts")).unwrap();
    wait_until("the save by rename", || {
        ops(&mut client).iter().any(|(op, path)| op == "TextEdit" && path == "a.ts")
    });
    // A file given another name is still one op, and at once.
    let before = status(&mut client).repo.ops;
    fs::rename(root.join("a.ts"), root.join("b.ts")).unwrap();
    within("a rename", &mut client, before + 1);
    assert_eq!(ops(&mut client)[0], ("FileRename".to_owned(), "b.ts".to_owned()));
    // And a removal.
    fs::remove_file(root.join("b.ts")).unwrap();
    wait_until("a removal", || {
        ops(&mut client)[0].0 == "RemoveSymbol" || ops(&mut client)[0].0 == "FileRemove"
    });
    let s = status(&mut client);
    assert!(s.watcher.latency_p50_ms.unwrap() < 1000.0, "{:?}", s.watcher);
    stop(root, &mut client);
}

#[test]
fn a_burst_of_saves_is_debounced_and_sync_is_a_barrier() {
    let tmp = new_repo();
    let root = tmp.path();
    let mut client = start(root);

    // Twenty rewrites of the same file in quick succession: few ops, right final state.
    for i in 0..20 {
        fs::write(root.join("f.txt"), format!("version {i}\n")).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    // A hundred files all at once, then the barrier.
    for i in 0..100 {
        fs::write(root.join(format!("file{i}.txt")), format!("content {i}\n")).unwrap();
    }
    let _: Value = client.call("sync", json!({})).unwrap();
    let s = status(&mut client);
    assert_eq!(s.repo.files, 101, "after sync everything that was on disk is in the log");
    assert!(s.repo.ops < 101 + 20, "rewrites in quick succession are merged: {} ops", s.repo.ops);
    stop(root, &mut client);

    let repo = Repo::open(root).unwrap();
    let entry = repo.tree().get(Path::new("f.txt")).unwrap();
    assert_eq!(repo.store().get_blob(&entry.blob).unwrap().unwrap(), b"version 19\n");
    assert!(repo.store().verify().unwrap().is_ok());
}

#[test]
fn restart_keeps_history_and_catches_offline_changes() {
    let tmp = new_repo();
    let root = tmp.path();
    let mut client = start(root);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    let _: Value = client.call("sync", json!({})).unwrap();
    let done: CheckpointResult = client.call("checkpoint", json!({ "label": "first" })).unwrap();
    assert_eq!(done.id, "ckpt#1");
    assert!(done.git_commit.is_some(), "{:?}", done.git_error);
    stop(root, &mut client);

    // With the daemon stopped.
    fs::write(root.join("a.txt"), "one\nmodified with the daemon stopped\n").unwrap();
    fs::write(root.join("new.txt"), "x\n").unwrap();

    let mut client = start(root);
    let s = status(&mut client);
    assert_eq!((s.repo.ops, s.repo.files, s.repo.checkpoints, s.repo.ops_since_checkpoint), (3, 2, 1, 2));
    let git = Command::new("git").current_dir(root).args(["log", "--format=%s"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&git.stdout).trim(), "first");
    stop(root, &mut client);
}

#[test]
fn only_one_daemon_per_repository_and_errors_are_reported() {
    let tmp = new_repo();
    let root = tmp.path();
    let mut client = start(root);

    let second = Command::new(env!("CARGO_BIN_EXE_kitod")).arg("--repo").arg(root).output().unwrap();
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already open"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    // The first one is still there.
    assert_eq!(status(&mut client).repo.ops, 0);

    let err = client.call_value("no_such_method", json!({})).unwrap_err();
    assert!(err.to_string().contains("unknown method"), "{err}");
    let err = client.call_value("checkpoint", json!({})).unwrap_err();
    assert!(err.to_string().contains("label"), "{err}");
    let err = client.call_value("checkpoint", json!({ "label": "empty" })).unwrap_err();
    assert!(err.to_string().contains("nothing to checkpoint"), "{err}");
    stop(root, &mut client);
}

#[test]
fn killed_daemon_loses_nothing_that_was_captured() {
    let tmp = new_repo();
    let root = tmp.path();
    let mut client = start(root);
    for i in 0..50 {
        fs::write(root.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
    }
    let _: Value = client.call("sync", json!({})).unwrap();
    let pid = status(&mut client).pid;
    // More saves, and kill -9 right away: no clean shutdown.
    for i in 0..50 {
        fs::write(root.join(format!("f{i}.txt")), format!("{i} again\n")).unwrap();
    }
    assert!(Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap().success());
    drop(client);

    // The new daemon recovers: the 50 confirmed ops are there, and it finds
    // the rest by comparing the working directory.
    let mut client = start(root);
    let s = status(&mut client);
    assert_eq!(s.repo.files, 50);
    assert_eq!(s.repo.ops, 100);
    stop(root, &mut client);
    let repo = Repo::open(root).unwrap();
    assert!(repo.store().verify().unwrap().is_ok());
    for i in 0..50 {
        let entry = repo.tree().get(Path::new(&format!("f{i}.txt"))).unwrap();
        assert_eq!(repo.store().get_blob(&entry.blob).unwrap().unwrap(), format!("{i} again\n").as_bytes());
    }
}

/// Modifies a file of the repository without the watcher being able to
/// notice: it writes through a hard link that lives outside the watched
/// directory. This is what happens, for example, on `/mnt/c` under WSL when
/// the file is changed by a Windows program.
fn modify_behind_the_watcher(file: &Path, outside: &Path, content: &str) {
    let link = outside.join("link");
    let _ = fs::remove_file(&link);
    fs::hard_link(file, &link).unwrap();
    fs::write(&link, content).unwrap();
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "a write through a link from outside is missed by inotify only; FSEvents reports it"
)]
fn changes_the_watcher_cannot_see_are_found_by_scanning() {
    let tmp = new_repo();
    let root = tmp.path();
    let outside = tempfile::tempdir().unwrap();
    fs::write(root.join(".kito/config.toml"), "[capture]\nrescan_secs = 60\n").unwrap();
    let mut client = start(root);

    fs::write(root.join("a.txt"), "one\n").unwrap();
    wait_until("the capture of a.txt", || status(&mut client).repo.ops == 1);
    let before = status(&mut client).watcher;
    assert!(
        before.watching && !before.events_unreliable && before.rescan_interval_ms == 60_000,
        "{before:?}"
    );

    // The watcher is silent; the change arrives "from outside".
    std::thread::sleep(Duration::from_millis(1200));
    modify_behind_the_watcher(&root.join("a.txt"), outside.path(), "one\nmodified without events\n");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(status(&mut client).repo.ops, 1, "no event: the watcher alone does not see it");

    // The barrier finds it, and the daemon takes note: from here on it scans often.
    let found: Value = client.call("sync", json!({})).unwrap();
    assert_eq!(found["ops"], 1);
    let after = status(&mut client).watcher;
    assert!(after.events_unreliable && after.missed_by_watcher == 1, "{after:?}");
    // At least once a second, and far more often than the normal minute
    // (the exact value follows how long a scan takes on this machine).
    assert!((1000..10_000).contains(&after.rescan_interval_ms), "{after:?}");

    // Later changes arrive by themselves, without events and without sync.
    std::thread::sleep(Duration::from_millis(1200));
    modify_behind_the_watcher(&root.join("a.txt"), outside.path(), "one\nmodified again\n");
    wait_until("the frequent scan", || status(&mut client).repo.ops == 3);
    stop(root, &mut client);

    let repo = Repo::open(root).unwrap();
    let entry = repo.tree().get(Path::new("a.txt")).unwrap();
    assert_eq!(repo.store().get_blob(&entry.blob).unwrap().unwrap(), b"one\nmodified again\n");
}

#[test]
fn without_a_watcher_the_daemon_polls() {
    let tmp = new_repo();
    let root = tmp.path();
    fs::write(root.join(".kito/config.toml"), "[capture]\nwatch = false\n").unwrap();
    let mut client = start(root);
    let w = status(&mut client).watcher;
    assert!(!w.watching && w.events_unreliable && (1000..10_000).contains(&w.rescan_interval_ms), "{w:?}");

    fs::write(root.join("a.txt"), "one\n").unwrap();
    wait_until("the capture by scanning", || status(&mut client).repo.ops == 1);
    fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    wait_until("the capture of the edit", || status(&mut client).repo.ops == 2);
    let w = status(&mut client).watcher;
    assert_eq!(
        (w.events, w.missed_by_watcher),
        (0, 0),
        "without a watcher there are no events and no missed events"
    );
    stop(root, &mut client);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "a save is known complete by inotify's close-after-write; FSEvents has no such event (known limit on macOS)"
)]
fn a_save_that_takes_its_time_is_captured_once_it_is_complete() {
    use std::io::Write;
    let tmp = new_repo();
    let root = tmp.path();
    let mut client = start(root);
    let functions = |n: usize, last: &str| -> String {
        (0..n)
            .map(|i| {
                format!(
                    "export function f{i}(): string {{\n  return \"{}\";\n}}\n",
                    if i + 1 == n { last } else { "same" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/m.ts"), functions(5, "before")).unwrap();
    let _: Value = client.call("sync", json!({})).unwrap();
    let before = ops(&mut client).len();

    // A program saves the file the usual way, emptying it first, and is
    // held up for a good while before it writes; then it writes in two
    // steps. Far longer than the debounce: the daemon looks at the file in
    // the meantime, and must not take what it sees for the save.
    let mut file = fs::File::create(root.join("src/m.ts")).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let text = functions(5, "after");
    let (first, second) = text.split_at(text.find("export function f2").unwrap());
    file.write_all(first.as_bytes()).unwrap();
    file.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    file.write_all(second.as_bytes()).unwrap();
    drop(file);
    let _: Value = client.call("sync", json!({})).unwrap();

    // One change of the file, one function whose body changed: no symbol
    // was ever taken for removed.
    let all = ops(&mut client);
    let new: Vec<&str> = all[..all.len() - before].iter().map(|(op, _)| op.as_str()).collect();
    assert_eq!(new, ["ModifyBody", "TextEdit"], "{all:?}");
    stop(root, &mut client);
}
