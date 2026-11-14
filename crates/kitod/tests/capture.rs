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

    write(root, "src/main.txt", "fn main() {\n    println!(\"one\");\n}\n");
    write(root, "README.md", "# title\n");
    write(root, "data.bin", "\0\u{1}\u{2}");
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "FileAdd", "FileAdd"]);
    assert_eq!(kito_state(&repo), disk(root));

    // Edit: a TextEdit with line-based edits.
    write(root, "src/main.txt", "fn main() {\n    println!(\"two\");\n}\n");
    let ops = repo.scan().unwrap();
    match &ops[..] {
        [Op { kind: OpKind::TextEdit { file, edits, .. }, .. }] => {
            assert_eq!(file, Path::new("src/main.txt"));
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
    assert!(
        ops.iter()
            .any(|o| o.kind
                == OpKind::FileRename { from: "src/main.txt".into(), to: "sources/main.txt".into() })
    );

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
    write(root, "src/lib.txt", "pub fn f() {}\n");

    let ops = repo.scan().unwrap();
    let paths: Vec<String> = repo.tree().iter().map(|(p, _)| p.display().to_string()).collect();
    assert_eq!(paths, [".gitignore", "src/lib.txt"]);
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
    write(root, "src/app.txt", "export const a = 1;\n");
    // The checkpoint captures by itself what is on disk: no explicit scan.
    let first = repo.checkpoint("initial structure").unwrap();
    assert_eq!(first.id, "ckpt#1");
    assert!(first.git_error.is_none(), "{:?}", first.git_error);

    write(root, "src/app.txt", "export const a = 1;\nexport const b = 2;\n");
    repo.scan().unwrap();
    write(root, "src/util.txt", "export function id<T>(x: T) { return x; }\n");
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
            "FileAdd src/util.txt",
            "TextEdit src/app.txt",
            "ckpt#1 initial structure",
            "FileAdd src/app.txt",
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
    let only: Vec<_> = repo.log(50, Some(Path::new("src/app.txt"))).unwrap();
    assert_eq!(only.iter().filter(|i| matches!(i, LogItem::Op { .. })).count(), 2);
    assert_eq!(repo.log(1, None).unwrap().iter().filter(|i| matches!(i, LogItem::Op { .. })).count(), 1);

    let status = repo.status().unwrap();
    assert_eq!(
        (status.ops, status.files, status.checkpoints, status.unexported, status.ops_since_checkpoint),
        (4, 3, 2, 0, 0)
    );
    write(root, "src/app.txt", "export const a = 3;\n");
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
    write(root, "lib.txt", "def f():\n    return 1\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first"]);
    write(root, "lib.txt", "def f():\n    return 2\n");
    git(root, &["commit", "-q", "-am", "second"]);
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
    // A change not yet in git.
    write(root, "lib.txt", "def f():\n    return 3\n");

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

const AUTH_TS: &str = r#"import { db } from "./db";

export function getUser(id: string): User {
  const cached = cache.get(id);
  if (cached) {
    return cached;
  }
  const user = db.find(id);
  cache.set(id, user);
  return user;
}

export class Session {
  constructor(private user: User) {}

  isValid(now: number): boolean {
    return this.user.expires > now && !this.user.revoked;
  }
}
"#;

/// The steps of a symbol's history, in words.
fn story(repo: &Repo, file: &str, name: &str) -> Vec<String> {
    let histories = repo.why(Path::new(file), name).unwrap();
    assert_eq!(histories.len(), 1, "{name} in {file}");
    histories[0].steps.iter().map(|s| s.what.clone()).collect()
}

#[test]
fn a_function_is_followed_through_a_rename_and_a_move() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();

    write(root, "src/auth.ts", AUTH_TS);
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "AddSymbol", "AddSymbol", "AddSymbol", "AddSymbol"]);
    let id = repo.symbols().find(Path::new("src/auth.ts"), "getUser")[0];

    // A reformat changes the file but means nothing for its symbols.
    write(
        root,
        "src/auth.ts",
        &AUTH_TS.replace(
            "  const cached = cache.get(id);",
            "  // look in the cache first\n  const cached =\n    cache.get(id);",
        ),
    );
    assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit"]);
    write(root, "src/auth.ts", AUTH_TS);
    repo.scan().unwrap();

    // 1. Rename the function.
    let renamed = AUTH_TS.replace("getUser", "fetchUser");
    write(root, "src/auth.ts", &renamed);
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["TextEdit", "Rename"]);
    assert!(
        matches!(&ops[1].kind, OpKind::Rename { id: i, from, to } if *i == id && from == "getUser" && to == "fetchUser")
    );

    // 2. Move it to another file.
    let start = renamed.find("export function fetchUser").unwrap();
    let end = renamed.find("export class Session").unwrap();
    write(root, "src/auth.ts", &format!("{}{}", &renamed[..start], &renamed[end..]));
    write(root, "src/users.ts", &renamed[start..end]);
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "TextEdit", "Move"]);

    // 3. Change its body there.
    let moved = fs::read_to_string(root.join("src/users.ts")).unwrap();
    write(root, "src/users.ts", &moved.replace("cache.set(id, user);", "cache.set(id, user, TTL);"));
    assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit", "ModifyBody"]);

    // `kito why` follows the whole story from the name it has today.
    assert_eq!(repo.symbols().find(Path::new("src/users.ts"), "fetchUser"), [id]);
    assert_eq!(
        story(&repo, "src/users.ts", "fetchUser"),
        [
            "function `getUser` created in src/auth.ts",
            "renamed `getUser` → `fetchUser`",
            "moved from src/auth.ts to src/users.ts",
            "body changed (+1 −1)",
        ]
    );
    // The method that never moved has a short one, and is found by its qualified name.
    assert_eq!(story(&repo, "src/auth.ts", "Session.isValid"), ["method `isValid` created in src/auth.ts"]);
    let missing = repo.why(Path::new("src/auth.ts"), "fetchUser").unwrap_err().to_string();
    assert!(
        missing.contains("no symbol named `fetchUser`") && missing.contains("Session.isValid"),
        "{missing}"
    );

    // The state survives a restart, from the saved file and from the log alone.
    let (symbols, status) = (repo.symbols().clone(), repo.status().unwrap());
    assert_eq!(status.symbols, 4);
    repo.close().unwrap();
    let repo = Repo::open(root).unwrap();
    assert_eq!(repo.symbols(), &symbols);
    repo.close().unwrap();
    fs::remove_file(root.join(".kito/lanes/main/state")).unwrap();
    let mut repo = Repo::open(root).unwrap();
    assert_eq!(repo.symbols(), &symbols);
    assert!(repo.scan().unwrap().is_empty());
    assert!(repo.store().verify().unwrap().is_ok());

    // The log tells the same story, newest first: each change of a file,
    // preceded by what it meant for the symbols. The limit counts changes.
    let lines = |items: Vec<LogItem>| -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                LogItem::Op { op, path, detail, .. } => Some(format!(
                    "{op} {}: {}",
                    path.clone().unwrap_or_default(),
                    detail.clone().unwrap_or_default()
                )),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        lines(repo.log(3, None).unwrap()),
        [
            "ModifyBody src/users.ts: `fetchUser` body changed (+1 −1)",
            "TextEdit src/users.ts: ",
            "Move src/users.ts: `fetchUser` moved from src/auth.ts to src/users.ts",
            "TextEdit src/auth.ts: ",
            "FileAdd src/users.ts: ",
        ]
    );
    // The log of one file: its changes, and the symbols that were in it at
    // the time. The rename happened when the function was still in auth.ts.
    assert_eq!(
        lines(repo.log(2, Some(Path::new("src/users.ts"))).unwrap()),
        [
            "ModifyBody src/users.ts: `fetchUser` body changed (+1 −1)",
            "TextEdit src/users.ts: ",
            "Move src/users.ts: `fetchUser` moved from src/auth.ts to src/users.ts",
            "FileAdd src/users.ts: ",
        ]
    );
    assert_eq!(
        lines(repo.log(2, Some(Path::new("src/auth.ts"))).unwrap()),
        [
            "Move src/users.ts: `fetchUser` moved from src/auth.ts to src/users.ts",
            "TextEdit src/auth.ts: ",
            "Rename src/auth.ts: renamed `getUser` → `fetchUser`",
            "TextEdit src/auth.ts: ",
        ]
    );
}

#[test]
fn a_class_is_followed_out_of_its_namespace_and_a_method_into_another_class() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();

    let item = "    public class Item\n    {\n        public string Describe()\n        {\n            return string.Format(\"{0} x {1}\", Quantity, Price);\n        }\n    }\n";
    let cart = "    public class Cart\n    {\n        public decimal Total()\n        {\n            return items.Sum(item => item.Price * item.Quantity);\n        }\n    }\n";
    write(root, "Shop.cs", &format!("namespace Shop\n{{\n{cart}\n{item}}}\n"));
    repo.scan().unwrap();
    let class = repo.symbols().find(Path::new("Shop.cs"), "Shop.Item")[0];
    let method = repo.symbols().find(Path::new("Shop.cs"), "Shop.Item.Describe")[0];

    // "Move type to its own file": into a namespace block of the same name.
    write(root, "Shop.cs", &format!("namespace Shop\n{{\n{cart}}}\n"));
    write(root, "Item.cs", &format!("namespace Shop\n{{\n{item}}}\n"));
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "TextEdit", "AddSymbol", "Move"]);
    assert_eq!(repo.symbols().find(Path::new("Item.cs"), "Shop.Item"), [class]);
    assert_eq!(repo.symbols().find(Path::new("Item.cs"), "Shop.Item.Describe"), [method]);

    // Then the method goes to the other class, in the other file.
    let describe = "        public string Describe()\n        {\n            return string.Format(\"{0} x {1}\", Quantity, Price);\n        }\n";
    let total = "        public decimal Total()\n        {\n            return items.Sum(item => item.Price * item.Quantity);\n        }\n";
    write(root, "Item.cs", "namespace Shop\n{\n    public class Item\n    {\n    }\n}\n");
    write(
        root,
        "Shop.cs",
        &format!("namespace Shop\n{{\n    public class Cart\n    {{\n{total}\n{describe}    }}\n}}\n"),
    );
    let ops = repo.scan().unwrap();
    assert_eq!(kinds(&ops), ["TextEdit", "TextEdit", "Move"]);
    assert_eq!(repo.symbols().find(Path::new("Shop.cs"), "Shop.Cart.Describe"), [method]);
    assert_eq!(
        story(&repo, "Shop.cs", "Shop.Cart.Describe"),
        [
            "method `Describe` created in Shop.cs",
            // It had no move of its own: its class took it along.
            "moved from Shop.cs to Item.cs, along with `Shop.Item`",
            "moved from Item.cs to Shop.cs",
        ]
    );
    assert_eq!(
        story(&repo, "Item.cs", "Shop.Item"),
        ["class `Item` created in Shop.cs", "moved from Shop.cs to Item.cs"]
    );

    // A renamed file is part of the story of what it contains.
    fs::rename(root.join("Item.cs"), root.join("Product.cs")).unwrap();
    assert_eq!(kinds(&repo.scan().unwrap()), ["FileRename"]);
    assert_eq!(
        story(&repo, "Product.cs", "Shop.Item"),
        [
            "class `Item` created in Shop.cs",
            "moved from Shop.cs to Item.cs",
            "file renamed from Item.cs to Product.cs"
        ]
    );

    // Replaying the log gives the same nesting.
    let symbols = repo.symbols().clone();
    repo.close().unwrap();
    fs::remove_file(root.join(".kito/lanes/main/state")).unwrap();
    let mut repo = Repo::open(root).unwrap();
    assert_eq!(repo.symbols(), &symbols);
    assert!(repo.scan().unwrap().is_empty());
}

#[test]
fn half_written_code_does_not_disturb_the_symbols() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    write(root, "src/auth.ts", AUTH_TS);
    repo.scan().unwrap();
    let before = repo.symbols().clone();

    // Saved in the middle of typing: the file is recorded, the symbols wait.
    let broken = AUTH_TS
        .replace("export class Session {", "export class Session { isValid(now: number): boolean { return (");
    write(root, "src/auth.ts", &broken);
    assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit"]);
    assert_eq!(repo.symbols(), &before);

    // Once it parses again, the comparison picks up where it left off: one
    // rename, nothing removed, nothing invented.
    write(root, "src/auth.ts", &AUTH_TS.replace("isValid", "stillValid"));
    let id = before.find(Path::new("src/auth.ts"), "Session.isValid")[0];
    assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit", "Rename"]);
    assert_eq!(repo.symbols().find(Path::new("src/auth.ts"), "Session.stillValid"), [id]);
}

#[test]
fn rollback_and_git_keep_working_with_symbols() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    write(root, ".gitignore", ".kito/\n");
    write(root, "src/auth.ts", AUTH_TS);
    repo.checkpoint("one").unwrap();
    let id = repo.symbols().find(Path::new("src/auth.ts"), "getUser")[0];

    write(root, "src/auth.ts", &AUTH_TS.replace("getUser", "fetchUser"));
    repo.checkpoint("renamed").unwrap();
    assert_eq!(git(root, &["log", "--format=%s"]).lines().collect::<Vec<_>>(), ["renamed", "one"]);
    assert_eq!(git(root, &["status", "--porcelain"]), "");

    // Rolling the rename back is itself a rename: the symbol is the same one.
    let done = repo.rollback("ckpt#1", &[], false).unwrap();
    assert!(done.applied);
    assert_eq!(repo.symbols().find(Path::new("src/auth.ts"), "getUser"), [id]);
    assert_eq!(
        story(&repo, "src/auth.ts", "getUser"),
        [
            "function `getUser` created in src/auth.ts",
            "renamed `getUser` → `fetchUser`",
            "renamed `fetchUser` → `getUser`"
        ]
    );
    // `~1` counts file changes, not the symbolic ops that follow them.
    let back = repo.rollback("~1", &[], false).unwrap();
    assert!(back.applied);
    assert_eq!(repo.symbols().find(Path::new("src/auth.ts"), "fetchUser"), [id]);
}

#[test]
fn an_imported_repository_gets_its_symbols_analyzed() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    write(root, ".gitignore", ".kito/\n");
    write(root, "server.go", "package demo\n\ntype Server struct {\n\tport int\n}\n\nfunc (s *Server) Start() error {\n\treturn listen(s.port)\n}\n");
    write(root, "notes.md", "# notes\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first"]);

    let mut repo = Repo::init(root).unwrap();
    repo.import_git().unwrap();
    // History is imported as text; the state it ends in is analyzed in full.
    assert_eq!(repo.status().unwrap().symbols, 2);
    assert_eq!(story(&repo, "server.go", "Server.Start"), ["method `Server.Start` created in server.go"]);
    assert_eq!(repo.analyze().unwrap(), 0, "nothing left to analyze");
    assert!(repo.scan().unwrap().is_empty());

    // In the log the analysis stands on its own: its symbolic ops came with
    // no file change, and are not shown as part of the import's.
    let items = repo.log(10, None).unwrap();
    let described: Vec<(String, bool, bool)> = items
        .iter()
        .filter_map(|i| match i {
            LogItem::Op { op, change, intent, .. } => Some((op.clone(), change.is_some(), intent.is_some())),
            _ => None,
        })
        .collect();
    assert_eq!(
        described[..2],
        [("AddSymbol".to_owned(), false, true), ("AddSymbol".to_owned(), false, true)]
    );
    assert!(described[2..].iter().all(|(op, change, _)| op == "FileAdd" && !change), "{described:?}");
    // They still belong in the log of their file.
    let of_file = repo.log(10, Some(Path::new("server.go"))).unwrap();
    assert_eq!(
        of_file.iter().filter(|i| matches!(i, LogItem::Op { op, .. } if op == "AddSymbol")).count(),
        2
    );
    assert!(repo
        .log(10, Some(Path::new("notes.md")))
        .unwrap()
        .iter()
        .all(|i| !matches!(i, LogItem::Op { op, .. } if op == "AddSymbol")));
}

#[test]
fn a_file_caught_in_the_middle_of_a_save_is_not_captured_cut_short() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    let functions = |n: usize| -> String {
        (0..n)
            .map(|i| format!("export function f{i}(): number {{\n  return {i};\n}}\n"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    write(root, "src/m.ts", &functions(4));
    repo.scan().unwrap();
    let ids: Vec<SymbolId> =
        (0..4).map(|i| repo.symbols().find(Path::new("src/m.ts"), &format!("f{i}"))[0]).collect();
    std::thread::sleep(kitod::repo::SETTLE * 2);

    // A program is saving the file: it has emptied it and written the first
    // half, which happens to be valid code. A capture that follows the file
    // events arrives right now: it leaves the file alone.
    write(root, "src/m.ts", &functions(2));
    assert!(repo.capture_events(LaneId::MAIN, [PathBuf::from("src/m.ts")]).unwrap().is_empty());
    assert!(repo.has_deferred());
    assert_eq!(repo.symbols().len(), 4, "nothing was removed");
    // The save completes (with an edit to the last function).
    write(root, "src/m.ts", &functions(4).replace("return 3;", "return 33;"));
    assert!(repo.capture_deferred().unwrap().is_empty(), "still too fresh");
    std::thread::sleep(kitod::repo::SETTLE * 2);
    let ops = repo.capture_deferred().unwrap();
    assert_eq!(kinds(&ops), ["TextEdit", "ModifyBody"]);
    assert!(!repo.has_deferred());
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            repo.symbols().find(Path::new("src/m.ts"), &format!("f{i}")),
            [*id],
            "f{i} is the same symbol"
        );
    }

    // A barrier (`kito sync`, an integration) waits for the file to settle
    // instead: when it returns, what is on disk is in the log.
    write(root, "src/m.ts", &functions(5));
    let started = std::time::Instant::now();
    assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit", "AddSymbol", "ModifyBody"]);
    assert!(started.elapsed() >= kitod::repo::SETTLE / 2, "{:?}", started.elapsed());
    assert!(repo.scan().unwrap().is_empty());
}

#[test]
fn what_is_removed_and_then_restored_is_the_same_symbol() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init(root).unwrap();
    write(root, "src/auth.ts", AUTH_TS);
    repo.scan().unwrap();
    repo.checkpoint("with the function").unwrap();
    let function = repo.symbols().find(Path::new("src/auth.ts"), "getUser")[0];
    let method = repo.symbols().find(Path::new("src/auth.ts"), "Session.isValid")[0];

    // The function is cut out in one save and pasted back in a later one.
    let start = AUTH_TS.find("export function getUser").unwrap();
    let end = AUTH_TS.find("export class Session").unwrap();
    write(root, "src/auth.ts", &format!("{}{}", &AUTH_TS[..start], &AUTH_TS[end..]));
    assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit", "RemoveSymbol"]);
    assert!(repo.symbols().get(function).is_none());
    write(root, "src/auth.ts", AUTH_TS);
    assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit", "AddSymbol"]);
    assert_eq!(repo.symbols().find(Path::new("src/auth.ts"), "getUser"), [function]);
    assert_eq!(
        story(&repo, "src/auth.ts", "getUser"),
        ["function `getUser` created in src/auth.ts", "removed", "restored in src/auth.ts"]
    );

    // A whole file emptied and written again: the class and its methods too.
    write(root, "src/auth.ts", "");
    repo.scan().unwrap();
    assert_eq!(repo.symbols().len(), 0);
    write(root, "src/auth.ts", AUTH_TS);
    repo.scan().unwrap();
    assert_eq!(repo.symbols().find(Path::new("src/auth.ts"), "getUser"), [function]);
    assert_eq!(repo.symbols().find(Path::new("src/auth.ts"), "Session.isValid"), [method]);

    // A rollback that brings back what a later save removed.
    fs::remove_file(root.join("src/auth.ts")).unwrap();
    repo.scan().unwrap();
    assert_eq!(repo.symbols().len(), 0);
    repo.rollback("ckpt#1", &[], false).unwrap();
    assert_eq!(repo.symbols().find(Path::new("src/auth.ts"), "getUser"), [function]);
    assert_eq!(repo.symbols().find(Path::new("src/auth.ts"), "Session.isValid"), [method]);
    assert!(repo.store().verify().unwrap().is_ok());
}
