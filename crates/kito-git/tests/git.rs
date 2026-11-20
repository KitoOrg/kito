//! Round trip with git: after the export `git fsck` passes and every commit
//! contains exactly Kito's state; the import rebuilds the history.

use kito_core::*;
use kito_diff::fileops::{ops_for_changes, Snapshot};
#[cfg(unix)]
use kito_git::import;
use kito_git::{export, ExportOptions, GitError};
use kito_store::Store;
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Desired state of the files: path → (content, type).
type State = BTreeMap<String, (Vec<u8>, FileMode)>;

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
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .output()
        .expect("git is installed");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A store with its main lane, driven by file states.
struct Kito {
    store: Store,
    tree: Tree,
    head: Option<OpId>,
    clock: HlcClock,
}

impl Kito {
    fn init(root: &Path) -> Kito {
        Kito {
            store: Store::init(&root.join(".kito")).unwrap(),
            tree: Tree::new(),
            head: None,
            clock: HlcClock::new(),
        }
    }

    /// Brings the tree to the given state, producing the necessary ops.
    fn set(&mut self, state: &State) {
        let mut changes: BTreeMap<PathBuf, Option<Snapshot>> =
            self.tree.iter().map(|(p, _)| (p.to_owned(), None)).collect();
        for (path, (content, mode)) in state {
            let blob = self.store.put_blob(content).unwrap();
            changes.insert(path.into(), Some(Snapshot { blob, mode: *mode }));
        }
        let store = &self.store;
        let kinds = ops_for_changes(&self.tree, &changes, &mut |b| store.get_blob(b).unwrap());
        for kind in kinds {
            let at = self.clock.tick(1_767_323_045_000);
            let op = Op::new(self.head, LaneId::MAIN, Author::agent("agent-1"), at, kind, None).unwrap();
            self.tree.apply(&op.kind).unwrap();
            self.store.append_op(&op).unwrap();
            self.head = Some(op.id);
        }
    }

    fn checkpoint(&mut self, label: &str) -> CheckpointId {
        let ck = Checkpoint {
            id: self.store.next_checkpoint_id(),
            head: self.head.expect("at least one op"),
            label: label.into(),
            created_by: Author::human("Francesco D."),
            verification: None,
            git_commit: None,
            approved_by: None,
            git_merge: None,
            lane: None,
        };
        self.store.put_checkpoint(&ck).unwrap();
        ck.id
    }

    fn commit_of(&self, id: CheckpointId) -> String {
        let oid = self.store.checkpoint(id).unwrap().git_commit.expect("exported");
        oid.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The state contained in a git commit, read with the real git.
fn git_state(root: &Path, commit: &str) -> State {
    let mut state = State::new();
    for line in git(root, &["ls-tree", "-r", "-z", commit]).split('\0').filter(|l| !l.is_empty()) {
        let (meta, path) = line.split_once('\t').unwrap();
        let mut parts = meta.split(' ');
        let (mode, _kind, oid) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
        let mode = match mode {
            "100644" => FileMode::Regular,
            "100755" => FileMode::Executable,
            "120000" => FileMode::Symlink,
            other => panic!("mode {other}"),
        };
        let content =
            Command::new("git").current_dir(root).args(["cat-file", "blob", oid]).output().unwrap().stdout;
        state.insert(path.to_owned(), (content, mode));
    }
    state
}

fn file(content: &str) -> (Vec<u8>, FileMode) {
    (content.as_bytes().to_vec(), FileMode::Regular)
}

#[test]
#[cfg(unix)] // Unix permissions or links: not on Windows (ADR 0002)
fn checkpoints_become_commits_that_git_accepts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut kito = Kito::init(root);

    let mut s1 = State::new();
    s1.insert("README.md".into(), file("# project\n"));
    s1.insert("src/main.rs".into(), file("fn main() {}\n"));
    s1.insert("scripts/run.sh".into(), (b"#!/bin/sh\necho hello\n".to_vec(), FileMode::Executable));
    s1.insert("link".into(), (b"README.md".to_vec(), FileMode::Symlink));
    s1.insert("data.bin".into(), (vec![0, 159, 146, 150, 0, 1], FileMode::Regular));
    kito.set(&s1);
    let c1 = kito.checkpoint("first checkpoint");

    let mut s2 = s1.clone();
    s2.insert("src/main.rs".into(), file("fn main() {\n    println!(\"hello\");\n}\n"));
    s2.remove("README.md");
    s2.insert("docs/README.md".into(), file("# project\n")); // rename
    s2.remove("link");
    s2.insert("src/deep/nested/mod.rs".into(), file("pub fn f() {}\n"));
    kito.set(&s2);
    let c2 = kito.checkpoint("second checkpoint");
    let c3 = kito.checkpoint(""); // no ops in between: commit with the same tree

    let done = export(&mut kito.store, root, &ExportOptions::default()).unwrap();
    assert_eq!(done.iter().map(|e| e.checkpoint).collect::<Vec<_>>(), [c1, c2, c3]);

    git(root, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(git_state(root, &kito.commit_of(c1)), s1);
    assert_eq!(git_state(root, &kito.commit_of(c2)), s2);
    assert_eq!(git_state(root, &kito.commit_of(c3)), s2);

    let log = git(root, &["log", "--format=%H|%s|%an|%ae|%ad", "--date=unix"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(
        lines[0],
        format!("{}|Checkpoint ckpt#3|Francesco D.|francesco-d.@kito.invalid|1767323045", kito.commit_of(c3))
    );
    assert!(lines[1].starts_with(&format!("{}|second checkpoint|", kito.commit_of(c2))));
    assert!(lines[2].starts_with(&format!("{}|first checkpoint|", kito.commit_of(c1))));
    assert!(git(root, &["log", "-1", "--format=%B"]).contains("Kito-Checkpoint: ckpt#3"));

    // The rename is recognizable by git too, because the blob is the same.
    let renames = git(root, &["diff", "--name-status", "-M", &kito.commit_of(c1), &kito.commit_of(c2)]);
    assert!(renames.contains("R100\tREADME.md\tdocs/README.md"), "{renames}");

    // Redoing the export changes nothing.
    assert!(export(&mut kito.store, root, &ExportOptions::default()).unwrap().is_empty());
    assert_eq!(git(root, &["rev-list", "--count", "HEAD"]).trim(), "3");

    // With the files on disk, `git status` is clean: the index has been rewritten.
    for (path, (content, mode)) in &s2 {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        if *mode == FileMode::Executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    std::fs::write(root.join(".gitignore"), ".kito/\n.gitignore\n").unwrap();
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

#[test]
fn export_continues_history_and_detects_foreign_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // A git repository that existed before Kito.
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("old.txt"), "before kito\n").unwrap();
    git(root, &["add", "old.txt"]);
    git(root, &["commit", "-q", "-m", "commit made with git"]);
    let original = git(root, &["rev-parse", "HEAD"]).trim().to_owned();

    let mut kito = Kito::init(root);
    let mut s = State::new();
    s.insert("old.txt".into(), file("before kito\n"));
    s.insert("new.txt".into(), file("with kito\n"));
    kito.set(&s);
    let c1 = kito.checkpoint("first from kito");
    export(&mut kito.store, root, &ExportOptions::default()).unwrap();
    assert_eq!(
        git(root, &["rev-parse", "HEAD~1"]).trim(),
        original,
        "the first export continues the existing history"
    );
    assert_eq!(git_state(root, "HEAD"), s);

    // Export interrupted before moving the branch: the next one realigns it.
    git(root, &["update-ref", "refs/heads/main", &original]);
    assert!(export(&mut kito.store, root, &ExportOptions::default()).unwrap().is_empty());
    assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), kito.commit_of(c1));

    // A commit made directly in git: the export stops instead of skipping over it.
    git(root, &["commit", "-q", "--allow-empty", "-m", "outside kito"]);
    s.insert("other.txt".into(), file("x\n"));
    kito.set(&s);
    kito.checkpoint("second from kito");
    let err = export(&mut kito.store, root, &ExportOptions::default()).unwrap_err();
    assert!(matches!(err, GitError::Diverged { .. }), "{err}");
    assert!(git(root, &["log", "-1", "--format=%s"]).contains("outside kito"));
    git(root, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
#[cfg(unix)] // Unix permissions or links: not on Windows (ADR 0002)
fn git_history_is_imported_as_ops_and_checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    let write = |path: &str, content: &str| {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    };
    git(root, &["commit", "-q", "--allow-empty", "-m", "initial empty commit"]);
    write("a.txt", "one\ntwo\nthree\n");
    write("dir/b.txt", "content of b\n");
    write("run.sh", "#!/bin/sh\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "add the files\n\nmessage body"]);
    write("a.txt", "one\nTWO\nthree\n");
    git(root, &["mv", "dir/b.txt", "dir/c.txt"]);
    git(root, &["commit", "-q", "-am", "edit and rename"]);
    // A side branch, merged with a merge commit.
    git(root, &["checkout", "-q", "-b", "feature"]);
    write("feature.txt", "from the branch\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "work on the branch"]);
    git(root, &["checkout", "-q", "main"]);
    git(root, &["rm", "-q", "run.sh"]);
    git(root, &["commit", "-q", "-m", "remove the script"]);
    git(root, &["merge", "-q", "--no-ff", "feature", "-m", "merge the branch"]);
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
    let chain: Vec<String> =
        git(root, &["rev-list", "--first-parent", "--reverse", "HEAD"]).lines().map(str::to_owned).collect();
    assert_eq!(chain.len(), 5);

    let mut kito = Kito::init(root);
    let report = import(&mut kito.store, root, &mut kito.clock).unwrap();
    assert_eq!(
        (report.commits, report.checkpoints),
        (5, 4),
        "the initial empty commit has no op to point to"
    );
    assert!(report.skipped.is_empty());

    // Every checkpoint remembers its commit and reproduces its tree.
    let checkpoints: Vec<Checkpoint> = kito.store.checkpoints().cloned().collect();
    let labels: Vec<&str> = checkpoints.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["add the files", "edit and rename", "remove the script", "merge the branch"]);
    let ops = kito.store.lane_ops(LaneId::MAIN, 0, usize::MAX).unwrap();
    assert_eq!(ops.len(), report.ops);
    let mut tree = Tree::new();
    let mut next = checkpoints.iter().peekable();
    for op in &ops {
        tree.apply(&op.kind).unwrap();
        assert_eq!(op.author, Author::human("Ada").with_email("ada@example.com"));
        while let Some(ck) = next.next_if(|c| c.head == op.id) {
            let commit: String = ck.git_commit.unwrap().iter().map(|b| format!("{b:02x}")).collect();
            assert!(chain.contains(&commit));
            let expected = git_state(root, &commit);
            let got: State = tree
                .iter()
                .map(|(p, e)| {
                    (p.to_str().unwrap().to_owned(), (kito.store.get_blob(&e.blob).unwrap().unwrap(), e.mode))
                })
                .collect();
            assert_eq!(got, expected, "{}", ck.label);
        }
    }
    assert!(next.peek().is_none());
    let kinds: Vec<&str> = ops.iter().map(|o| o.kind.name()).collect();
    assert_eq!(kinds, ["FileAdd", "FileAdd", "FileAdd", "FileRename", "TextEdit", "FileRemove", "FileAdd"]);
    match &ops[4].kind {
        OpKind::TextEdit { edits, .. } => assert_eq!(edits, &vec![TextEdit::new(4, 8, "TWO\n")]),
        other => panic!("{other:?}"),
    }

    // Nothing to re-export; a new checkpoint is grafted on top of HEAD.
    assert!(export(&mut kito.store, root, &ExportOptions::default()).unwrap().is_empty());
    assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), head);
    kito.tree = tree;
    kito.head = ops.last().map(|o| o.id);
    let mut s = git_state(root, &head);
    s.insert("after-import.txt".into(), file("new\n"));
    kito.set(&s);
    kito.checkpoint("first checkpoint after the import");
    assert_eq!(export(&mut kito.store, root, &ExportOptions::default()).unwrap().len(), 1);
    assert_eq!(git(root, &["rev-parse", "HEAD~1"]).trim(), head);
    assert_eq!(git_state(root, "HEAD"), s);
    git(root, &["fsck", "--strict", "--no-dangling"]);

    // Import only works on an empty store.
    assert!(matches!(import(&mut kito.store, root, &mut HlcClock::new()), Err(GitError::NotEmpty)));
}

fn arb_state() -> impl Strategy<Value = State> {
    let path = prop::sample::select(vec!["a", "b.txt", "d/a", "d/b", "d/e/f", "x/y/z.rs", "x/w"]);
    let content = prop::sample::select(vec!["", "one\n", "one\ntwo\n", "three\n", "binary\0\u{1}", "four"]);
    let mode = prop::sample::select(vec![
        FileMode::Regular,
        FileMode::Regular,
        FileMode::Executable,
        FileMode::Symlink,
    ]);
    prop::collection::btree_map(path, (content, mode), 0..6).prop_map(|m| {
        m.into_iter().map(|(p, (c, mode))| (p.to_owned(), (c.as_bytes().to_vec(), mode))).collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]

    /// Any sequence of states: every exported commit contains Kito's state
    /// at that checkpoint, and git considers the repository valid.
    #[test]
    fn exported_commits_match_kito_state(states in prop::collection::vec(arb_state(), 1..6), split in 0usize..6) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut kito = Kito::init(root);
        let mut expected: Vec<(CheckpointId, State)> = Vec::new();
        for (i, state) in states.iter().enumerate() {
            kito.set(state);
            if kito.head.is_some() {
                expected.push((kito.checkpoint(&format!("state {i}")), state.clone()));
            }
            // Also export midway: the incremental export must give the same result.
            if i == split {
                export(&mut kito.store, root, &ExportOptions::default()).unwrap();
            }
        }
        export(&mut kito.store, root, &ExportOptions::default()).unwrap();
        if !expected.is_empty() {
            git(root, &["fsck", "--strict", "--no-dangling"]);
            let commits = git(root, &["rev-list", "--count", "HEAD"]);
            prop_assert_eq!(commits.trim(), expected.len().to_string());
        }
        for (id, state) in &expected {
            prop_assert_eq!(&git_state(root, &kito.commit_of(*id)), state);
        }
    }
}
