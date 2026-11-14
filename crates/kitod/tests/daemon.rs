//! Il daemon vero (binario `kitod`), pilotato tramite il socket JSON-RPC.

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
    Client::connect_or_start(root).expect("il daemon parte")
}

fn stop(root: &Path, client: &mut Client) {
    let _: Value = client.call("shutdown", json!({})).unwrap();
    let socket = kitod::rpc::socket_path(&root.canonicalize().unwrap());
    wait_until("lo spegnimento del daemon", || !socket.exists());
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "tempo scaduto aspettando {what}");
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

    fs::write(root.join("a.txt"), "uno\n").unwrap();
    wait_until("la cattura di a.txt", || status(&mut client).repo.ops == 1);
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::write(root.join("src/deep/b.rs"), "fn b() {}\n").unwrap();
    wait_until("la cattura in una cartella nuova", || status(&mut client).repo.ops == 2);
    fs::write(root.join("a.txt"), "uno\ndue\n").unwrap();
    wait_until("la cattura della modifica", || status(&mut client).repo.ops == 3);
    fs::rename(root.join("a.txt"), root.join("c.txt")).unwrap();
    wait_until("la cattura del rename", || status(&mut client).repo.ops == 4);
    fs::remove_dir_all(root.join("src")).unwrap();
    wait_until("la cattura della rimozione", || status(&mut client).repo.ops == 5);

    let expected = [
        ("FileRemove", "src/deep/b.rs"),
        ("FileRename", "c.txt"),
        ("TextEdit", "a.txt"),
        ("FileAdd", "src/deep/b.rs"),
        ("FileAdd", "a.txt"),
    ];
    let got = ops(&mut client);
    assert_eq!(got.iter().map(|(o, p)| (o.as_str(), p.as_str())).collect::<Vec<_>>(), expected);

    let s = status(&mut client);
    assert!(s.watcher.events >= 5 && s.watcher.batches >= 5, "{:?}", s.watcher);
    assert!(s.watcher.latency_p95_ms.unwrap() >= s.debounce_ms as f64, "la latenza comprende il debounce");
    assert!(s.watcher.capture_p95_ms.unwrap() < 1000.0);
    stop(root, &mut client);
}

#[test]
fn a_burst_of_saves_is_debounced_and_sync_is_a_barrier() {
    let tmp = new_repo();
    let root = tmp.path();
    let mut client = start(root);

    // Venti riscritture ravvicinate dello stesso file: poche op, stato finale giusto.
    for i in 0..20 {
        fs::write(root.join("f.txt"), format!("versione {i}\n")).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    // Cento file tutti insieme, poi la barriera.
    for i in 0..100 {
        fs::write(root.join(format!("file{i}.txt")), format!("contenuto {i}\n")).unwrap();
    }
    let _: Value = client.call("sync", json!({})).unwrap();
    let s = status(&mut client);
    assert_eq!(s.repo.files, 101, "dopo sync tutto ciò che era su disco è nel log");
    assert!(s.repo.ops < 101 + 20, "le riscritture ravvicinate si fondono: {} op", s.repo.ops);
    stop(root, &mut client);

    let repo = Repo::open(root).unwrap();
    let entry = repo.tree().get(Path::new("f.txt")).unwrap();
    assert_eq!(repo.store().get_blob(&entry.blob).unwrap().unwrap(), b"versione 19\n");
    assert!(repo.store().verify().unwrap().is_ok());
}

#[test]
fn restart_keeps_history_and_catches_offline_changes() {
    let tmp = new_repo();
    let root = tmp.path();
    let mut client = start(root);
    fs::write(root.join("a.txt"), "uno\n").unwrap();
    let _: Value = client.call("sync", json!({})).unwrap();
    let done: CheckpointResult = client.call("checkpoint", json!({ "label": "primo" })).unwrap();
    assert_eq!(done.id, "ckpt#1");
    assert!(done.git_commit.is_some(), "{:?}", done.git_error);
    stop(root, &mut client);

    // A daemon spento.
    fs::write(root.join("a.txt"), "uno\nmodificato a daemon spento\n").unwrap();
    fs::write(root.join("nuovo.txt"), "x\n").unwrap();

    let mut client = start(root);
    let s = status(&mut client);
    assert_eq!((s.repo.ops, s.repo.files, s.repo.checkpoints, s.repo.ops_since_checkpoint), (3, 2, 1, 2));
    let git = Command::new("git").current_dir(root).args(["log", "--format=%s"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&git.stdout).trim(), "primo");
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
        String::from_utf8_lossy(&second.stderr).contains("già aperto"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    // Il primo è ancora lì.
    assert_eq!(status(&mut client).repo.ops, 0);

    let err = client.call_value("metodo_inesistente", json!({})).unwrap_err();
    assert!(err.to_string().contains("metodo sconosciuto"), "{err}");
    let err = client.call_value("checkpoint", json!({})).unwrap_err();
    assert!(err.to_string().contains("label"), "{err}");
    let err = client.call_value("checkpoint", json!({ "label": "vuoto" })).unwrap_err();
    assert!(err.to_string().contains("niente da salvare"), "{err}");
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
    // Altri salvataggi, e subito kill -9: nessuna chiusura pulita.
    for i in 0..50 {
        fs::write(root.join(format!("f{i}.txt")), format!("{i} bis\n")).unwrap();
    }
    assert!(Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap().success());
    drop(client);

    // Il nuovo daemon recupera: le 50 op confermate ci sono, il resto lo
    // ritrova confrontando la cartella di lavoro.
    let mut client = start(root);
    let s = status(&mut client);
    assert_eq!(s.repo.files, 50);
    assert_eq!(s.repo.ops, 100);
    stop(root, &mut client);
    let repo = Repo::open(root).unwrap();
    assert!(repo.store().verify().unwrap().is_ok());
    for i in 0..50 {
        let entry = repo.tree().get(Path::new(&format!("f{i}.txt"))).unwrap();
        assert_eq!(repo.store().get_blob(&entry.blob).unwrap().unwrap(), format!("{i} bis\n").as_bytes());
    }
}
