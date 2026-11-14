//! Il motore di cattura, senza daemon: la cartella di lavoro diventa op.

use kito_core::*;
use kitod::repo::LogItem;
use kitod::Repo;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn kinds(ops: &[Op]) -> Vec<&'static str> {
    ops.iter().map(|o| o.kind.name()).collect()
}

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// File sul disco (esclusi `.kito` e `.git`): percorso → (contenuto, tipo).
fn disk(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, FileMode)> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, (Vec<u8>, FileMode)>) {
        for entry in fs::read_dir(root.join(dir)).unwrap() {
            let entry = entry.unwrap();
            let rel = dir.join(entry.file_name());
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            if rel == Path::new(".kito") || rel == Path::new(".git") {
                continue;
            }
            if meta.is_dir() {
                visit(root, &rel, out);
            } else if meta.file_type().is_symlink() {
                let target = fs::read_link(entry.path()).unwrap();
                out.insert(rel, (target.to_string_lossy().into_owned().into_bytes(), FileMode::Symlink));
            } else {
                let mode = if meta.permissions().mode() & 0o111 != 0 {
                    FileMode::Executable
                } else {
                    FileMode::Regular
                };
                out.insert(rel, (fs::read(entry.path()).unwrap(), mode));
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, Path::new(""), &mut out);
    out
}

/// L'albero di Kito, materializzato dallo store.
fn kito_state(repo: &Repo) -> BTreeMap<PathBuf, (Vec<u8>, FileMode)> {
    repo.tree()
        .iter()
        .map(|(p, e)| (p.to_owned(), (repo.store().get_blob(&e.blob).unwrap().unwrap(), e.mode)))
        .collect()
}

#[test]
fn every_kind_of_change_on_disk_becomes_the_right_op() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    assert!(repo.scan().unwrap().is_empty(), "cartella vuota: nessuna op");

    write(root, "src/main.rs", "fn main() {\n    println!(\"uno\");\n}\n");
    write(root, "README.md", "# titolo\n");
    write(root, "dati.bin", "\0\u{1}\u{2}");
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "FileAdd", "FileAdd"]);
    assert_eq!(kito_state(&repo), disk(root));

    // Modifica: un TextEdit con gli edit per righe.
    write(root, "src/main.rs", "fn main() {\n    println!(\"due\");\n}\n");
    let ops = repo.scan().unwrap();
    match &ops[..] {
        [Op { kind: OpKind::TextEdit { file, edits, .. }, .. }] => {
            assert_eq!(file, Path::new("src/main.rs"));
            assert_eq!(edits, &vec![TextEdit::new(12, 33, "    println!(\"due\");\n")]);
        }
        other => panic!("{other:?}"),
    }
    // Un file binario cambia senza edit in linea.
    write(root, "dati.bin", "\0\u{3}");
    let ops = repo.scan().unwrap();
    assert!(matches!(&ops[..], [Op { kind: OpKind::TextEdit { edits, .. }, .. }] if edits.is_empty()));

    // Rename (stesso contenuto), anche di una cartella intera.
    fs::rename(root.join("README.md"), root.join("LEGGIMI.md")).unwrap();
    fs::rename(root.join("src"), root.join("sorgenti")).unwrap();
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileRename", "FileRename"]);
    assert!(ops
        .iter()
        .any(|o| o.kind == OpKind::FileRename { from: "src/main.rs".into(), to: "sorgenti/main.rs".into() }));

    // Permessi e link.
    fs::set_permissions(root.join("LEGGIMI.md"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("LEGGIMI.md", root.join("link")).unwrap();
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileRemove", "FileAdd", "FileAdd"]);
    assert_eq!(kito_state(&repo), disk(root));

    // Rimozione, e un file sostituito da una cartella con lo stesso nome.
    fs::remove_file(root.join("dati.bin")).unwrap();
    fs::remove_file(root.join("link")).unwrap();
    write(root, "link/dentro.txt", "x\n");
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileRemove", "FileRemove", "FileAdd"]);
    assert_eq!(kito_state(&repo), disk(root));

    // Niente è cambiato: nessuna op.
    assert!(repo.scan().unwrap().is_empty());

    // Ogni op è concatenata alla precedente, firmata e confermata.
    let all = repo.store().lane_ops(LaneId::MAIN, 0, usize::MAX).unwrap();
    for pair in all.windows(2) {
        assert_eq!(pair[1].parents.as_slice(), &[pair[0].id]);
        assert!(pair[1].at > pair[0].at);
    }
    assert_eq!(all.last().map(|o| o.id), repo.head());
    assert!(repo.store().verify().unwrap().is_ok());
}

#[test]
fn watcher_events_are_captured_path_by_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();

    write(root, "a.txt", "a\n");
    write(root, "b.txt", "b\n");
    // Solo il percorso segnalato viene guardato.
    assert_eq!(kinds(&repo.capture(["a.txt".into()]).unwrap()), ["FileAdd"]);
    assert!(!repo.tree().contains(Path::new("b.txt")));
    // Lo stesso evento due volte non produce nulla.
    assert!(repo.capture(["a.txt".into()]).unwrap().is_empty());

    // Un evento su una cartella vale per tutto il suo contenuto.
    write(root, "pkg/x/1.txt", "1\n");
    write(root, "pkg/y/2.txt", "2\n");
    assert_eq!(kinds(&repo.capture(["pkg".into()]).unwrap()), ["FileAdd", "FileAdd"]);
    fs::remove_dir_all(root.join("pkg/x")).unwrap();
    assert_eq!(kinds(&repo.capture(["pkg/x".into()]).unwrap()), ["FileRemove"]);
    fs::remove_dir_all(root.join("pkg")).unwrap();
    assert_eq!(kinds(&repo.capture(["pkg".into()]).unwrap()), ["FileRemove"]);

    // Evento su un file che nel frattempo è sparito e non era tracciato.
    assert!(repo.capture(["mai-esistito.txt".into()]).unwrap().is_empty());
    assert_eq!(kinds(&repo.scan().unwrap()), ["FileAdd"], "b.txt lo trova la scansione");
    assert_eq!(kito_state(&repo), disk(root));
}

#[test]
fn ignored_paths_are_not_captured() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    write(root, ".gitignore", "target/\n*.log\n");
    write(root, "target/debug/app", "binario");
    write(root, "debug.log", "rumore\n");
    write(root, ".git/HEAD", "ref: refs/heads/main\n");
    write(root, "src/lib.rs", "pub fn f() {}\n");

    let ops = repo.scan().unwrap();
    let paths: Vec<String> = repo.tree().iter().map(|(p, _)| p.display().to_string()).collect();
    assert_eq!(paths, [".gitignore", "src/lib.rs"]);
    assert_eq!(ops.len(), 2);
    // Vale anche per gli eventi del watcher.
    assert!(repo
        .capture(["debug.log".into(), "target/debug/app".into(), ".kito/format".into()])
        .unwrap()
        .is_empty());

    // Un .gitignore che cambia ha effetto subito, senza riavviare.
    write(root, ".gitignore", "target/\n");
    let ops = repo.capture([".gitignore".into(), "debug.log".into()]).unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "TextEdit"]);
    // Un file già tracciato resta tracciato anche se poi viene ignorato (come in git).
    write(root, ".gitignore", "target/\n*.log\n");
    write(root, "debug.log", "altro rumore\n");
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["TextEdit", "TextEdit"]);
    assert_eq!(repo.tree().len(), 3);
}

#[test]
fn same_size_rewrite_right_after_a_scan_is_not_missed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    write(root, "f.txt", "aaaa\n");
    repo.scan().unwrap();
    // Stessa dimensione, subito dopo: dimensione e data potrebbero coincidere.
    for content in ["bbbb\n", "cccc\n", "dddd\n"] {
        write(root, "f.txt", content);
        assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit"], "{content:?}");
    }
    assert_eq!(kito_state(&repo), disk(root));
}

#[test]
fn state_is_rebuilt_after_a_crash_and_offline_changes_are_caught() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    write(root, "a.txt", "uno\n");
    write(root, "b.txt", "due\n");
    repo.scan().unwrap();
    repo.save_state().unwrap();
    write(root, "c.txt", "tre\n");
    write(root, "a.txt", "uno bis\n");
    repo.scan().unwrap();
    let (tree, head) = (repo.tree().clone(), repo.head());
    drop(repo); // crash: lo stato salvato è più vecchio del log

    // Modifiche fatte mentre il daemon non girava.
    write(root, "b.txt", "due, cambiato a daemon spento\n");
    fs::remove_file(root.join("c.txt")).unwrap();

    let mut repo = Repo::open(root).unwrap();
    assert_eq!((repo.tree(), repo.head()), (&tree, head), "stato salvato + op successive riapplicate");
    assert_eq!(kinds(&repo.scan().unwrap()), ["FileRemove", "TextEdit"]);
    assert_eq!(kito_state(&repo), disk(root));
    repo.close().unwrap();

    // Senza stato salvato si riparte dal log intero.
    fs::remove_file(root.join(".kito/lanes/main/state")).unwrap();
    let mut repo = Repo::open(root).unwrap();
    assert_eq!(kito_state(&repo), disk(root));
    assert!(repo.scan().unwrap().is_empty());
    // Uno stato salvato illeggibile viene ignorato.
    repo.close().unwrap();
    fs::write(root.join(".kito/lanes/main/state"), b"spazzatura").unwrap();
    let mut repo = Repo::open(root).unwrap();
    assert_eq!(kito_state(&repo), disk(root));
    assert!(repo.scan().unwrap().is_empty());
}

#[test]
fn checkpoints_show_up_in_git_log_and_in_kito_log() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    assert!(matches!(repo.checkpoint("vuoto"), Err(kitod::Error::NothingToCheckpoint)));

    write(root, ".gitignore", ".kito/\n");
    write(root, "src/app.ts", "export const a = 1;\n");
    // Il checkpoint cattura da sé ciò che è sul disco: nessuna scansione esplicita.
    let first = repo.checkpoint("struttura iniziale").unwrap();
    assert_eq!(first.id, "ckpt#1");
    assert!(first.git_error.is_none(), "{:?}", first.git_error);

    write(root, "src/app.ts", "export const a = 1;\nexport const b = 2;\n");
    repo.scan().unwrap();
    write(root, "src/util.ts", "export function id<T>(x: T) { return x; }\n");
    let second = repo.checkpoint("aggiunge b e util").unwrap();

    // git vede i checkpoint come commit, e la cartella di lavoro è pulita.
    let log = git(root, &["log", "--format=%H %s"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], format!("{} aggiunge b e util", second.git_commit.as_deref().unwrap()));
    assert_eq!(lines[1], format!("{} struttura iniziale", first.git_commit.as_deref().unwrap()));
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    git(root, &["fsck", "--strict", "--no-dangling"]);

    // kito log: dalla più recente, con i checkpoint al loro posto.
    let items = repo.log(50, None).unwrap();
    let shape: Vec<String> = items
        .iter()
        .map(|i| match i {
            LogItem::Checkpoint { id, label, .. } => format!("{id} {label}"),
            LogItem::Op { op, path, .. } => format!("{op} {}", path.as_deref().unwrap_or("")),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "ckpt#2 aggiunge b e util",
            "FileAdd src/util.ts",
            "TextEdit src/app.ts",
            "ckpt#1 struttura iniziale",
            "FileAdd src/app.ts",
            "FileAdd .gitignore",
        ]
    );
    match &items[2] {
        LogItem::Op { lines_added, lines_removed, .. } => {
            assert_eq!((*lines_added, *lines_removed), (Some(1), Some(0)))
        }
        other => panic!("{other:?}"),
    }
    // Filtro per file e limite.
    let only: Vec<_> = repo.log(50, Some(Path::new("src/app.ts"))).unwrap();
    assert_eq!(only.iter().filter(|i| matches!(i, LogItem::Op { .. })).count(), 2);
    assert_eq!(repo.log(1, None).unwrap().iter().filter(|i| matches!(i, LogItem::Op { .. })).count(), 1);

    let status = repo.status().unwrap();
    assert_eq!(
        (status.ops, status.files, status.checkpoints, status.unexported, status.ops_since_checkpoint),
        (4, 3, 2, 0, 0)
    );
    write(root, "src/app.ts", "export const a = 3;\n");
    repo.scan().unwrap();
    assert_eq!(repo.status().unwrap().ops_since_checkpoint, 1);
}

#[test]
fn a_git_repository_is_imported_and_then_continued() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    write(root, ".gitignore", ".kito/\n");
    write(root, "lib.py", "def f():\n    return 1\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "primo"]);
    write(root, "lib.py", "def f():\n    return 2\n");
    git(root, &["commit", "-q", "-am", "secondo"]);
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
    // Una modifica non ancora in git.
    write(root, "lib.py", "def f():\n    return 3\n");

    let mut repo = Repo::init(root).unwrap();
    let report = repo.import_git().unwrap();
    assert_eq!((report.commits, report.checkpoints, report.ops), (2, 2, 3));
    assert_eq!(repo.status().unwrap().unexported, 0);

    // La modifica non committata è la prima cosa che la cattura registra.
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["TextEdit"]);
    assert_eq!(kito_state(&repo), disk(root));
    let done = repo.checkpoint("terzo, da kito").unwrap();
    assert_eq!(git(root, &["rev-parse", "HEAD~1"]).trim(), head);
    assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), done.git_commit.unwrap());
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

/// Niente configurazione personale né identità di git della macchina: i test
/// vedono solo ciò che impostano.
fn isolate_identity() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let empty = std::env::temp_dir().join(format!("kito-test-config-{}", std::process::id()));
        std::env::set_var("KITO_CONFIG_DIR", &empty);
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::remove_var("KITO_AUTHOR");
        std::env::remove_var("KITO_EMAIL");
        std::env::set_var("USER", "utente-di-sistema");
    });
}

#[test]
fn identity_signs_ops_and_exported_commits() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    // Senza nulla di configurato si ripiega sull'utente del sistema.
    assert_eq!(repo.identity().name, "utente-di-sistema");
    assert_eq!(repo.identity().source, kitod::identity::Source::SystemUser);
    assert_eq!(repo.identity().email, None);

    write(root, ".gitignore", ".kito/\n");
    write(root, "a.txt", "prima\n");
    let first = repo.scan().unwrap();
    assert_eq!(first[0].author, Author::human("utente-di-sistema"));

    // `kito identity --name … --email …` scrive nel config e il daemon lo rilegge.
    let config = root.join(".kito/config.toml");
    kitod::identity::write(&config, Some("Ada Lovelace"), Some("ada@example.com"), None).unwrap();
    repo.reload_config().unwrap();
    assert_eq!(repo.identity().source, kitod::identity::Source::Repository);
    write(root, "a.txt", "dopo\n");
    let ops = repo.scan().unwrap();
    assert_eq!(ops[0].author, Author::human("Ada Lovelace").with_email("ada@example.com"));
    assert_eq!(repo.status().unwrap().author, "Ada Lovelace <ada@example.com>");

    // Il commit esportato porta l'email vera: è ciò che GitHub usa per attribuirlo.
    repo.checkpoint("con identità").unwrap();
    assert_eq!(git(root, &["log", "-1", "--format=%an <%ae>"]).trim(), "Ada Lovelace <ada@example.com>");

    // Un agente.
    kitod::identity::write(&config, Some("agent-7"), None, Some("agent")).unwrap();
    repo.reload_config().unwrap();
    write(root, "a.txt", "dall'agente\n");
    assert_eq!(repo.scan().unwrap()[0].author.kind, AuthorKind::Agent);
}

#[test]
fn rollback_restores_files_and_can_itself_be_undone() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    fs::write(root.join(".kito/config.toml"), "[git]\nexport_on_checkpoint = false\n").unwrap();
    repo.reload_config().unwrap();

    // Stato 1.
    write(root, "src/auth.ts", "export const v = 1;\n");
    write(root, "docs/guida.md", "# Guida\n");
    write(root, "run.sh", "#!/bin/sh\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("docs/guida.md", root.join("link")).unwrap();
    repo.checkpoint("stato uno").unwrap();
    let state1 = disk(root);

    // Stato 2: modifica, aggiunta, rimozione, rename, cambio di tipo.
    write(root, "src/auth.ts", "export const v = 2;\n");
    write(root, "src/nuovo/modulo.ts", "export {};\n");
    fs::remove_file(root.join("link")).unwrap();
    fs::rename(root.join("docs/guida.md"), root.join("docs/manuale.md")).unwrap();
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    let second = repo.checkpoint("stato due").unwrap();
    let state2 = disk(root);
    // Una modifica non ancora vista da nessuno: il rollback non deve perderla.
    write(root, "src/auth.ts", "export const v = 3; // salvato un attimo prima del rollback\n");
    let state3 = disk(root);
    assert_ne!(state1, state2);

    // Prova a vuoto: elenca, non tocca.
    let preview = repo.rollback("ckpt#1", &[], true).unwrap();
    assert!(!preview.applied && preview.ops == 0);
    let listed: Vec<(&str, &str)> =
        preview.changes.iter().map(|c| (c.action.as_str(), c.path.as_str())).collect();
    assert_eq!(
        listed,
        [
            ("ricrea", "docs/guida.md"),
            ("rimuove", "docs/manuale.md"),
            ("ricrea", "link"),
            ("ripristina", "run.sh"),
            ("ripristina", "src/auth.ts"),
            ("rimuove", "src/nuovo/modulo.ts"),
        ]
    );
    assert_eq!(disk(root), state3);

    // Rollback vero al primo checkpoint.
    let done = repo.rollback("ckpt#1", &[], false).unwrap();
    assert!(done.applied && done.ops > 0);
    assert_eq!(done.target, "ckpt#1 (stato uno)");
    assert_eq!(disk(root), state1, "contenuti, permessi e link come allora");
    assert!(!root.join("src/nuovo").exists(), "le cartelle rimaste vuote spariscono");
    assert_eq!(kito_state(&repo), state1);
    assert!(repo.scan().unwrap().is_empty(), "il rollback è già tutto registrato");

    // La storia non è stata riscritta: le op del rollback sono in coda, con il loro perché.
    let items = repo.log(3, None).unwrap();
    assert!(items.iter().all(
        |i| matches!(i, LogItem::Op { intent: Some(why), .. } if why == "rollback a ckpt#1 (stato uno)")
    ));
    assert_eq!(repo.store().checkpoints().count(), 2);

    // Annullare il rollback riporta anche la modifica fatta un attimo prima.
    let undo = done.undo.unwrap();
    repo.rollback(&undo, &[], false).unwrap();
    assert_eq!(disk(root), state3);

    // A un checkpoint successivo, per numero; poi indietro di N op; poi a un'op per ID.
    repo.rollback("2", &[], false).unwrap();
    assert_eq!(disk(root), state2);
    assert_eq!(second.id, "ckpt#2");
    let before = repo.status().unwrap().ops;
    let last = repo.rollback("~1", &[], false).unwrap();
    assert!(last.applied);
    assert_eq!(repo.status().unwrap().ops, before + last.ops as u64);
    let head_before_that = last.undo.unwrap();
    repo.rollback(&head_before_that, &[], false).unwrap();
    assert_eq!(disk(root), state2);

    // Niente da fare se lo stato è già quello.
    let noop = repo.rollback("ckpt#2", &[], false).unwrap();
    assert!(noop.changes.is_empty() && !noop.applied);
    assert!(repo.store().verify().unwrap().is_ok());
}

#[test]
fn rollback_of_selected_paths_and_its_refusals() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    fs::write(root.join(".kito/config.toml"), "[git]\nexport_on_checkpoint = false\n").unwrap();
    repo.reload_config().unwrap();
    write(root, "a.txt", "a1\n");
    write(root, "dir/b.txt", "b1\n");
    write(root, "dir/c.txt", "c1\n");
    repo.checkpoint("uno").unwrap();
    write(root, "a.txt", "a2\n");
    write(root, "dir/b.txt", "b2\n");
    write(root, "dir/c.txt", "c2\n");
    repo.scan().unwrap();

    // Solo una cartella: il resto resta com'è.
    let done = repo.rollback("ckpt#1", &["dir".into()], false).unwrap();
    assert_eq!(done.changes.len(), 2);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a2\n");
    assert_eq!(fs::read_to_string(root.join("dir/b.txt")).unwrap(), "b1\n");
    // Solo un file.
    repo.rollback("ckpt#1", &["a.txt".into()], false).unwrap();
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a1\n");

    // Punti che non esistono o non si capiscono.
    for (spec, needle) in [
        ("ckpt#9", "non esiste"),
        ("~999", "solo"),
        ("abcdef123456", "nessuna operazione"),
        ("boh", "non riconosciuto"),
    ] {
        let err = repo.rollback(spec, &[], false).unwrap_err().to_string();
        assert!(err.contains(needle), "{spec}: {err}");
    }

    // Un file ignorato nel posto in cui il rollback ricreerebbe un file: non
    // è nel log, quindi non viene sovrascritto.
    fs::remove_file(root.join("a.txt")).unwrap();
    repo.scan().unwrap();
    write(root, ".gitignore", "a.txt\n");
    repo.scan().unwrap();
    write(root, "a.txt", "contenuto mai registrato\n");
    let err = repo.rollback("ckpt#1", &["a.txt".into()], false).unwrap_err().to_string();
    assert!(err.contains("non è tracciato"), "{err}");
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "contenuto mai registrato\n");
}
