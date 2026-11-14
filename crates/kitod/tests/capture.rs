//! The capture engine, without the daemon: the working directory becomes ops.

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

/// Files on disk (excluding `.kito` and `.git`): path → (content, type).
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

/// Kito's tree, materialized from the store.
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
    assert!(repo.scan().unwrap().is_empty(), "empty directory: no ops");

    write(root, "src/main.rs", "fn main() {\n    println!(\"one\");\n}\n");
    write(root, "README.md", "# title\n");
    write(root, "data.bin", "\0\u{1}\u{2}");
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "FileAdd", "FileAdd"]);
    assert_eq!(kito_state(&repo), disk(root));

    // Edit: a TextEdit with line-based edits.
    write(root, "src/main.rs", "fn main() {\n    println!(\"two\");\n}\n");
    let ops = repo.scan().unwrap();
    match &ops[..] {
        [Op { kind: OpKind::TextEdit { file, edits, .. }, .. }] => {
            assert_eq!(file, Path::new("src/main.rs"));
            assert_eq!(edits, &vec![TextEdit::new(12, 33, "    println!(\"two\");\n")]);
        }
        other => panic!("{other:?}"),
    }
    // A binary file changes without inline edits.
    write(root, "data.bin", "\0\u{3}");
    let ops = repo.scan().unwrap();
    assert!(matches!(&ops[..], [Op { kind: OpKind::TextEdit { edits, .. }, .. }] if edits.is_empty()));

    // Rename (same content), of a whole directory too.
    fs::rename(root.join("README.md"), root.join("GUIDE.md")).unwrap();
    fs::rename(root.join("src"), root.join("sources")).unwrap();
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileRename", "FileRename"]);
    assert!(ops
        .iter()
        .any(|o| o.kind == OpKind::FileRename { from: "src/main.rs".into(), to: "sources/main.rs".into() }));

    // Permissions and links.
    fs::set_permissions(root.join("GUIDE.md"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("GUIDE.md", root.join("link")).unwrap();
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileRemove", "FileAdd", "FileAdd"]);
    assert_eq!(kito_state(&repo), disk(root));

    // Removal, and a file replaced by a directory with the same name.
    fs::remove_file(root.join("data.bin")).unwrap();
    fs::remove_file(root.join("link")).unwrap();
    write(root, "link/inside.txt", "x\n");
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileRemove", "FileRemove", "FileAdd"]);
    assert_eq!(kito_state(&repo), disk(root));

    // Nothing has changed: no ops.
    assert!(repo.scan().unwrap().is_empty());

    // Every op is chained to the previous one, signed and confirmed.
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
    // Only the reported path is looked at.
    assert_eq!(kinds(&repo.capture(["a.txt".into()]).unwrap()), ["FileAdd"]);
    assert!(!repo.tree().contains(Path::new("b.txt")));
    // The same event twice produces nothing.
    assert!(repo.capture(["a.txt".into()]).unwrap().is_empty());

    // An event on a directory stands for all of its content.
    write(root, "pkg/x/1.txt", "1\n");
    write(root, "pkg/y/2.txt", "2\n");
    assert_eq!(kinds(&repo.capture(["pkg".into()]).unwrap()), ["FileAdd", "FileAdd"]);
    fs::remove_dir_all(root.join("pkg/x")).unwrap();
    assert_eq!(kinds(&repo.capture(["pkg/x".into()]).unwrap()), ["FileRemove"]);
    fs::remove_dir_all(root.join("pkg")).unwrap();
    assert_eq!(kinds(&repo.capture(["pkg".into()]).unwrap()), ["FileRemove"]);

    // An event on a file that has disappeared in the meantime and was not tracked.
    assert!(repo.capture(["never-existed.txt".into()]).unwrap().is_empty());
    assert_eq!(kinds(&repo.scan().unwrap()), ["FileAdd"], "b.txt is found by the scan");
    assert_eq!(kito_state(&repo), disk(root));
}

#[test]
fn ignored_paths_are_not_captured() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    write(root, ".gitignore", "target/\n*.log\n");
    write(root, "target/debug/app", "binary");
    write(root, "debug.log", "noise\n");
    write(root, ".git/HEAD", "ref: refs/heads/main\n");
    write(root, "src/lib.rs", "pub fn f() {}\n");

    let ops = repo.scan().unwrap();
    let paths: Vec<String> = repo.tree().iter().map(|(p, _)| p.display().to_string()).collect();
    assert_eq!(paths, [".gitignore", "src/lib.rs"]);
    assert_eq!(ops.len(), 2);
    // The same holds for the watcher events.
    assert!(repo
        .capture(["debug.log".into(), "target/debug/app".into(), ".kito/format".into()])
        .unwrap()
        .is_empty());

    // A .gitignore that changes takes effect right away, without restarting.
    write(root, ".gitignore", "target/\n");
    let ops = repo.capture([".gitignore".into(), "debug.log".into()]).unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "TextEdit"]);
    // A file already tracked stays tracked even if it is later ignored (as in git).
    write(root, ".gitignore", "target/\n*.log\n");
    write(root, "debug.log", "more noise\n");
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
    // Same size, right afterwards: size and modification time could coincide.
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
    write(root, "a.txt", "one\n");
    write(root, "b.txt", "two\n");
    repo.scan().unwrap();
    repo.save_state().unwrap();
    write(root, "c.txt", "three\n");
    write(root, "a.txt", "one again\n");
    repo.scan().unwrap();
    let (tree, head) = (repo.tree().clone(), repo.head());
    drop(repo); // crash: the saved state is older than the log

    // Changes made while the daemon was not running.
    write(root, "b.txt", "two, changed with the daemon stopped\n");
    fs::remove_file(root.join("c.txt")).unwrap();

    let mut repo = Repo::open(root).unwrap();
    assert_eq!((repo.tree(), repo.head()), (&tree, head), "saved state + later ops reapplied");
    assert_eq!(kinds(&repo.scan().unwrap()), ["FileRemove", "TextEdit"]);
    assert_eq!(kito_state(&repo), disk(root));
    repo.close().unwrap();

    // Without a saved state it starts over from the whole log.
    fs::remove_file(root.join(".kito/lanes/main/state")).unwrap();
    let mut repo = Repo::open(root).unwrap();
    assert_eq!(kito_state(&repo), disk(root));
    assert!(repo.scan().unwrap().is_empty());
    // An unreadable saved state is ignored.
    repo.close().unwrap();
    fs::write(root.join(".kito/lanes/main/state"), b"garbage").unwrap();
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
    assert!(matches!(repo.checkpoint("empty"), Err(kitod::Error::NothingToCheckpoint)));

    write(root, ".gitignore", ".kito/\n");
    write(root, "src/app.ts", "export const a = 1;\n");
    // The checkpoint captures by itself what is on disk: no explicit scan.
    let first = repo.checkpoint("initial structure").unwrap();
    assert_eq!(first.id, "ckpt#1");
    assert!(first.git_error.is_none(), "{:?}", first.git_error);

    write(root, "src/app.ts", "export const a = 1;\nexport const b = 2;\n");
    repo.scan().unwrap();
    write(root, "src/util.ts", "export function id<T>(x: T) { return x; }\n");
    let second = repo.checkpoint("adds b and util").unwrap();

    // git sees the checkpoints as commits, and the working directory is clean.
    let log = git(root, &["log", "--format=%H %s"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], format!("{} adds b and util", second.git_commit.as_deref().unwrap()));
    assert_eq!(lines[1], format!("{} initial structure", first.git_commit.as_deref().unwrap()));
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    git(root, &["fsck", "--strict", "--no-dangling"]);

    // kito log: most recent first, with the checkpoints in their place.
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
            "ckpt#2 adds b and util",
            "FileAdd src/util.ts",
            "TextEdit src/app.ts",
            "ckpt#1 initial structure",
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
    // Filter by file, and limit.
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
    git(root, &["commit", "-q", "-m", "first"]);
    write(root, "lib.py", "def f():\n    return 2\n");
    git(root, &["commit", "-q", "-am", "second"]);
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
    // A change not yet in git.
    write(root, "lib.py", "def f():\n    return 3\n");

    let mut repo = Repo::init(root).unwrap();
    let report = repo.import_git().unwrap();
    assert_eq!((report.commits, report.checkpoints, report.ops), (2, 2, 3));
    assert_eq!(repo.status().unwrap().unexported, 0);

    // The uncommitted change is the first thing the capture records.
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["TextEdit"]);
    assert_eq!(kito_state(&repo), disk(root));
    let done = repo.checkpoint("third, from kito").unwrap();
    assert_eq!(git(root, &["rev-parse", "HEAD~1"]).trim(), head);
    assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), done.git_commit.unwrap());
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

/// No personal configuration and no git identity from the machine: the tests
/// see only what they set.
fn isolate_identity() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let empty = std::env::temp_dir().join(format!("kito-test-config-{}", std::process::id()));
        std::env::set_var("KITO_CONFIG_DIR", &empty);
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::remove_var("KITO_AUTHOR");
        std::env::remove_var("KITO_EMAIL");
        std::env::set_var("USER", "system-user");
    });
}

#[test]
fn identity_signs_ops_and_exported_commits() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    // With nothing configured it falls back to the system user.
    assert_eq!(repo.identity().name, "system-user");
    assert_eq!(repo.identity().source, kitod::identity::Source::SystemUser);
    assert_eq!(repo.identity().email, None);

    write(root, ".gitignore", ".kito/\n");
    write(root, "a.txt", "before\n");
    let first = repo.scan().unwrap();
    assert_eq!(first[0].author, Author::human("system-user"));

    // `kito identity --name … --email …` writes to the config and the daemon rereads it.
    let config = root.join(".kito/config.toml");
    kitod::identity::write(&config, Some("Ada Lovelace"), Some("ada@example.com"), None).unwrap();
    repo.reload_config().unwrap();
    assert_eq!(repo.identity().source, kitod::identity::Source::Repository);
    write(root, "a.txt", "after\n");
    let ops = repo.scan().unwrap();
    assert_eq!(ops[0].author, Author::human("Ada Lovelace").with_email("ada@example.com"));
    assert_eq!(repo.status().unwrap().author, "Ada Lovelace <ada@example.com>");

    // The exported commit carries the real email: it is what GitHub uses to attribute it.
    repo.checkpoint("with identity").unwrap();
    assert_eq!(git(root, &["log", "-1", "--format=%an <%ae>"]).trim(), "Ada Lovelace <ada@example.com>");

    // An agent.
    kitod::identity::write(&config, Some("agent-7"), None, Some("agent")).unwrap();
    repo.reload_config().unwrap();
    write(root, "a.txt", "from the agent\n");
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

    // State 1.
    write(root, "src/auth.ts", "export const v = 1;\n");
    write(root, "docs/guide.md", "# Guide\n");
    write(root, "run.sh", "#!/bin/sh\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("docs/guide.md", root.join("link")).unwrap();
    repo.checkpoint("state one").unwrap();
    let state1 = disk(root);

    // State 2: edit, addition, removal, rename, change of type.
    write(root, "src/auth.ts", "export const v = 2;\n");
    write(root, "src/new/module.ts", "export {};\n");
    fs::remove_file(root.join("link")).unwrap();
    fs::rename(root.join("docs/guide.md"), root.join("docs/manual.md")).unwrap();
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    let second = repo.checkpoint("state two").unwrap();
    let state2 = disk(root);
    // A change nobody has seen yet: the rollback must not lose it.
    write(root, "src/auth.ts", "export const v = 3; // saved a moment before the rollback\n");
    let state3 = disk(root);
    assert_ne!(state1, state2);

    // Dry run: it lists, it does not touch.
    let preview = repo.rollback("ckpt#1", &[], true).unwrap();
    assert!(!preview.applied && preview.ops == 0);
    let listed: Vec<(&str, &str)> =
        preview.changes.iter().map(|c| (c.action.as_str(), c.path.as_str())).collect();
    assert_eq!(
        listed,
        [
            ("recreate", "docs/guide.md"),
            ("remove", "docs/manual.md"),
            ("recreate", "link"),
            ("restore", "run.sh"),
            ("restore", "src/auth.ts"),
            ("remove", "src/new/module.ts"),
        ]
    );
    assert_eq!(disk(root), state3);

    // A real rollback to the first checkpoint.
    let done = repo.rollback("ckpt#1", &[], false).unwrap();
    assert!(done.applied && done.ops > 0);
    assert_eq!(done.target, "ckpt#1 (state one)");
    assert_eq!(disk(root), state1, "contents, permissions and links as they were then");
    assert!(!root.join("src/new").exists(), "directories left empty disappear");
    assert_eq!(kito_state(&repo), state1);
    assert!(repo.scan().unwrap().is_empty(), "the rollback is already fully recorded");

    // History has not been rewritten: the rollback's ops are at the tail, with their why.
    let items = repo.log(3, None).unwrap();
    assert!(items.iter().all(
        |i| matches!(i, LogItem::Op { intent: Some(why), .. } if why == "rollback to ckpt#1 (state one)")
    ));
    assert_eq!(repo.store().checkpoints().count(), 2);

    // Undoing the rollback also brings back the change made a moment before.
    let undo = done.undo.unwrap();
    repo.rollback(&undo, &[], false).unwrap();
    assert_eq!(disk(root), state3);

    // To a later checkpoint, by number; then back by N ops; then to an op by ID.
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

    // Nothing to do if the state is already that one.
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
    repo.checkpoint("one").unwrap();
    write(root, "a.txt", "a2\n");
    write(root, "dir/b.txt", "b2\n");
    write(root, "dir/c.txt", "c2\n");
    repo.scan().unwrap();

    // Only one directory: the rest stays as it is.
    let done = repo.rollback("ckpt#1", &["dir".into()], false).unwrap();
    assert_eq!(done.changes.len(), 2);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a2\n");
    assert_eq!(fs::read_to_string(root.join("dir/b.txt")).unwrap(), "b1\n");
    // Only one file.
    repo.rollback("ckpt#1", &["a.txt".into()], false).unwrap();
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a1\n");

    // Points that do not exist or cannot be understood.
    for (spec, needle) in [
        ("ckpt#9", "does not exist"),
        ("~999", "only"),
        ("abcdef123456", "no operation"),
        ("huh", "unrecognized"),
    ] {
        let err = repo.rollback(spec, &[], false).unwrap_err().to_string();
        assert!(err.contains(needle), "{spec}: {err}");
    }

    // An ignored file in the place where the rollback would recreate a file:
    // it is not in the log, so it is not overwritten.
    fs::remove_file(root.join("a.txt")).unwrap();
    repo.scan().unwrap();
    write(root, ".gitignore", "a.txt\n");
    repo.scan().unwrap();
    write(root, "a.txt", "content never recorded\n");
    let err = repo.rollback("ckpt#1", &["a.txt".into()], false).unwrap_err().to_string();
    assert!(err.contains("is not tracked"), "{err}");
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "content never recorded\n");
}
