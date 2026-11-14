//! La CLI vera (`kito`), con il daemon vero avviato da sé.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Il binario del daemon sta accanto a quello della CLI. Se i test girano
/// per il solo pacchetto `kito-cli` potrebbe non essere stato compilato.
fn kitod() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let path = Path::new(env!("CARGO_BIN_EXE_kito")).with_file_name("kitod");
    BUILD.call_once(|| {
        if !path.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "kitod", "--bin", "kitod"])
                .status()
                .unwrap();
            assert!(status.success(), "compilazione di kitod");
        }
    });
    path
}

fn kito(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kito"))
        .current_dir(cwd)
        .args(args)
        .env("KITOD_BIN", kitod())
        .env("KITO_AUTHOR", "ada")
        .env_remove("KITO_EMAIL")
        // Niente identità presa dalla macchina su cui girano i test.
        .env("KITO_CONFIG_DIR", cwd.join(".config-personale-assente"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap()
}

/// Esegue un comando che deve riuscire e ne restituisce lo standard output.
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
    serde_json::from_str(&ok(cwd, args)).expect("output JSON")
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
    assert!(ok(root, &["init"]).contains("Repository Kito creato"));
    assert!(root.join(".kito/config.toml").is_file());

    // Dopo `init` il daemon è già attivo: nessun comando di commit, basta
    // salvare. `sync` è la barriera che attende la registrazione.
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/app.ts"), "export const a = 1;\n").unwrap();
    assert!(ok(root, &["sync"]).contains("Tutto registrato"));
    fs::write(root.join("src/app.ts"), "export const a = 1;\nexport const b = 2;\n").unwrap();
    ok(root, &["sync"]);

    let status = json(root, &["status", "--json"]);
    assert_eq!(status["repo"]["files"], 2);
    assert_eq!(status["repo"]["ops"], 3);
    assert_eq!(status["repo"]["author"], "ada");

    let log = ok(root, &["log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3, "{log}");
    assert!(
        lines[0].contains("TextEdit") && lines[0].contains("src/app.ts") && lines[0].contains("+1 −0"),
        "{log}"
    );
    assert!(
        lines[2].contains("FileAdd") && lines[2].contains(".gitignore") && lines[2].contains("ada"),
        "{log}"
    );

    // Il log di un file, indicato come lo si vede da una sottocartella.
    let only = ok(&root.join("src"), &["log", "app.ts"]);
    assert_eq!(only.lines().count(), 2, "{only}");
    let items = json(root, &["log", "--json", "-n", "1"]);
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!(items[0]["type"], "op");

    let done = ok(root, &["checkpoint", "-m", "prima versione"]);
    assert!(done.contains("ckpt#1 creato ed esportato come commit git"), "{done}");
    assert_eq!(git(root, &["log", "--format=%s|%an"]).trim(), "prima versione|ada");
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    assert!(ok(root, &["log"]).starts_with("◆ ckpt#1  prima versione  (git "));
    assert!(ok(root, &["export-git"]).contains("Niente da esportare"));

    let daemon = ok(root, &["daemon", "status"]);
    for needle in
        ["Daemon ............ pid", "Watcher", "Cattura", "Salvataggio → op", "tutti esportati su git"]
    {
        assert!(daemon.contains(needle), "manca {needle:?} in:\n{daemon}");
    }
    assert!(ok(root, &["daemon", "stop"]).contains("Daemon fermato"));
    assert!(ok(root, &["daemon", "stop"]).contains("non era attivo"));
    // Dopo lo stop il comando successivo riavvia il daemon da sé.
    assert_eq!(json(root, &["status", "--json"])["repo"]["checkpoints"], 1);
    ok(root, &["daemon", "stop"]);
}

#[test]
fn import_a_git_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    fs::write(root.join("main.go"), "package main\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "primo commit"]);
    fs::write(root.join("main.go"), "package main\n\nfunc main() {}\n").unwrap();
    git(root, &["commit", "-q", "-am", "secondo commit"]);
    fs::write(root.join("non-committato.txt"), "x\n").unwrap();

    let out = ok(root, &["import"]);
    assert!(out.contains("Importati 2 commit: 3 operazioni, 2 checkpoint."), "{out}");
    assert!(out.contains("Catturate 1 modifiche non ancora presenti in git."), "{out}");

    let log = ok(root, &["log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines[0].contains("FileAdd") && lines[0].contains("non-committato.txt"), "{log}");
    assert!(lines[1].starts_with("◆ ckpt#2  secondo commit  (git "), "{log}");
    assert!(lines[2].contains("TextEdit") && lines[2].contains("Ada") && lines[2].contains("+2 −0"), "{log}");
    assert!(lines[3].starts_with("◆ ckpt#1  primo commit"), "{log}");

    // Un secondo import non ha senso: lo store ha già una storia.
    let again = kito(root, &["import"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("già una storia"));

    // `init` in un repository git chiede di scegliere, invece di perdere la storia.
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["init", "-q", "-b", "main"]);
    let refused = kito(other.path(), &["init"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("kito import"));
    assert!(ok(other.path(), &["init", "--fresh"]).contains("Il daemon è attivo"));
    ok(other.path(), &["daemon", "stop"]);
    ok(root, &["daemon", "stop"]);
}

#[test]
fn errors_are_clear_and_json_stays_json() {
    let tmp = tempfile::tempdir().unwrap();
    let out = kito(tmp.path(), &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nessun repository Kito"));

    let out = kito(tmp.path(), &["status", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["error"].as_str().unwrap().contains("nessun repository Kito"));

    ok(tmp.path(), &["init"]);
    let out = kito(tmp.path(), &["checkpoint", "-m", "vuoto"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("niente da salvare"));
    let out = kito(tmp.path(), &["init"]);
    assert!(!out.status.success(), "init due volte");
    ok(tmp.path(), &["daemon", "stop"]);
}

#[test]
fn log_is_up_to_date_even_when_file_events_are_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let outside = tempfile::tempdir().unwrap();
    ok(root, &["init"]);
    fs::write(root.join("nota.txt"), "prima\n").unwrap();
    assert_eq!(ok(root, &["log"]).lines().count(), 1);

    // Una modifica che il watcher non può vedere: passa da un hard link
    // fuori dalla cartella osservata (come un programma Windows su /mnt/c).
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let link = outside.path().join("link");
    fs::hard_link(root.join("nota.txt"), &link).unwrap();
    fs::write(&link, "prima\ndopo\n").unwrap();

    let log = ok(root, &["log"]);
    assert!(log.lines().next().unwrap().contains("TextEdit"), "kito log deve mostrarla subito:\n{log}");
    let status = ok(root, &["daemon", "status"]);
    assert!(status.contains("non sono state segnalate dal sistema operativo"), "{status}");
    assert!(status.contains("scansione completa ogni 1 s"), "{status}");
    ok(root, &["daemon", "stop"]);
}

#[test]
fn identity_and_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    ok(root, &["init"]);
    fs::write(root.join(".gitignore"), ".kito/\n.config-personale-assente/\n").unwrap();

    // Identità: prima quella di ripiego, poi quella impostata.
    let shown = ok(root, &["identity"]);
    assert!(shown.contains("ada (persona)") && shown.contains("variabili d'ambiente"), "{shown}");
    assert!(shown.contains("Nessuna email"), "{shown}");
    let set = ok(root, &["identity", "--name", "Ada Lovelace", "--email", "ada@example.com"]);
    assert!(set.contains("Ada Lovelace <ada@example.com> (persona)"), "{set}");
    assert!(set.contains("configurazione del repository"), "{set}");
    assert_eq!(json(root, &["identity", "--json"])["email"], "ada@example.com");

    fs::write(root.join("nota.txt"), "versione uno\n").unwrap();
    ok(root, &["checkpoint", "-m", "uno"]);
    assert_eq!(git(root, &["log", "-1", "--format=%an <%ae>"]).trim(), "Ada Lovelace <ada@example.com>");

    fs::write(root.join("nota.txt"), "versione due\n").unwrap();
    fs::write(root.join("altro.txt"), "aggiunto dopo\n").unwrap();
    ok(root, &["sync"]);

    // Prova a vuoto, poi rollback vero al checkpoint.
    let preview = ok(root, &["rollback", "ckpt#1", "--dry-run"]);
    assert!(preview.contains("cambierebbe 2 file") && preview.contains("nulla è stato toccato"), "{preview}");
    assert_eq!(fs::read_to_string(root.join("nota.txt")).unwrap(), "versione due\n");
    let done = ok(root, &["rollback", "ckpt#1"]);
    assert!(done.contains("Rollback a ckpt#1 (uno): 2 file"), "{done}");
    assert!(done.contains("ripristina nota.txt") && done.contains("rimuove    altro.txt"), "{done}");
    assert_eq!(fs::read_to_string(root.join("nota.txt")).unwrap(), "versione uno\n");
    assert!(!root.join("altro.txt").exists());

    // Il log mostra il rollback come nuove op, con il loro perché e un ID breve.
    let log = ok(root, &["log", "-n", "2"]);
    assert!(log.lines().all(|l| l.contains("[rollback a ckpt#1 (uno)]")), "{log}");
    let first = log.lines().next().unwrap().split_whitespace().next().unwrap().to_owned();
    assert_eq!(first.len(), 8, "ID breve dell'op: {first}");

    // Si annulla con il comando suggerito.
    let undo =
        done.lines().find_map(|l| l.strip_prefix("Per annullarlo: kito rollback ")).unwrap().to_owned();
    ok(root, &["rollback", &undo]);
    assert_eq!(fs::read_to_string(root.join("nota.txt")).unwrap(), "versione due\n");
    assert_eq!(fs::read_to_string(root.join("altro.txt")).unwrap(), "aggiunto dopo\n");

    // Un solo file, indicato da una sottocartella; e all'indietro di N operazioni.
    fs::create_dir(root.join("sub")).unwrap();
    ok(&root.join("sub"), &["rollback", "ckpt#1", "../nota.txt"]);
    assert_eq!(fs::read_to_string(root.join("nota.txt")).unwrap(), "versione uno\n");
    assert!(root.join("altro.txt").exists());
    ok(root, &["rollback", "--ops", "1"]);
    assert_eq!(fs::read_to_string(root.join("nota.txt")).unwrap(), "versione due\n");

    assert!(ok(root, &["rollback", "ckpt#1", "sub"]).contains("niente da fare"));
    let bad = kito(root, &["rollback"]);
    assert!(
        !bad.status.success() && String::from_utf8_lossy(&bad.stderr).contains("indica a che punto tornare")
    );
    let bad = kito(root, &["rollback", "ckpt#42"]);
    assert!(String::from_utf8_lossy(&bad.stderr).contains("non esiste"));
    ok(root, &["daemon", "stop"]);
}
