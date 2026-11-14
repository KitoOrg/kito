//! Working copies: every file of the tree, with its content and its kind.

use kito_core::*;
use kito_lane::{materialize, remove, Source};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

struct Repo {
    tree: Tree,
    blobs: HashMap<BlobId, Vec<u8>>,
}

impl Repo {
    fn new(files: &[(&str, &str, FileMode)]) -> Repo {
        let mut repo = Repo { tree: Tree::new(), blobs: HashMap::new() };
        for (path, content, mode) in files {
            let blob = BlobId::of(content.as_bytes());
            repo.blobs.insert(blob, content.as_bytes().to_vec());
            repo.tree.apply(&OpKind::FileAdd { path: PathBuf::from(path), blob, mode: *mode }).unwrap();
        }
        repo
    }

    fn blob(&self, id: &BlobId) -> io::Result<Vec<u8>> {
        self.blobs.get(id).cloned().ok_or_else(|| io::Error::other(format!("no blob {id}")))
    }
}

fn sample() -> Repo {
    Repo::new(&[
        ("README.md", "# Demo\n", FileMode::Regular),
        ("src/deep/er/main.rs", "fn main() {}\n", FileMode::Regular),
        ("scripts/run.sh", "#!/bin/sh\necho hi\n", FileMode::Executable),
        ("latest", "src/deep", FileMode::Symlink),
        ("empty.txt", "", FileMode::Regular),
    ])
}

fn check(repo: &Repo, dest: &Path) {
    for (path, entry) in repo.tree.iter() {
        let at = dest.join(path);
        let meta = fs::symlink_metadata(&at).unwrap();
        match entry.mode {
            FileMode::Symlink => {
                assert_eq!(
                    fs::read_link(&at).unwrap().to_string_lossy().as_bytes(),
                    &repo.blobs[&entry.blob][..]
                );
            }
            mode => {
                assert_eq!(fs::read(&at).unwrap(), repo.blobs[&entry.blob], "{}", path.display());
                let executable = meta.permissions().mode() & 0o111 != 0;
                assert_eq!(executable, mode == FileMode::Executable, "{}", path.display());
            }
        }
    }
}

#[test]
fn a_working_copy_is_written_from_the_store() {
    let repo = sample();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("lanes/1/work");
    let done = materialize(&repo.tree, &dest, None, &|id| repo.blob(id)).unwrap();
    assert_eq!((done.files, done.written, done.cloned + done.copied), (5, 5, 0));
    check(&repo, &dest);

    // Regular files are dated in the past and reported as the file system
    // sees them: a scan can trust them without reading them, and a save
    // right after the creation cannot hide behind the same timestamp.
    assert_eq!(done.stats.len(), 4, "every file but the link");
    for (path, reported) in &done.stats {
        let now = fs::metadata(dest.join(path)).unwrap();
        assert_eq!(
            (reported.len(), reported.mtime(), reported.mtime_nsec()),
            (now.len(), now.mtime(), now.mtime_nsec())
        );
        let age = SystemTime::now().duration_since(now.modified().unwrap()).unwrap();
        assert!(age >= Duration::from_secs(1), "{}: {age:?}", path.display());
    }

    remove(&dest).unwrap();
    assert!(!dest.exists());
    remove(&dest).unwrap();
}

#[test]
fn files_already_on_disk_are_cloned_or_copied_and_stay_independent() {
    let repo = sample();
    let tmp = tempfile::tempdir().unwrap();
    // The main working directory: all the files, one of them edited since
    // (it no longer holds what the tree says), one missing.
    let main = tmp.path().join("main");
    materialize(&repo.tree, &main, None, &|id| repo.blob(id)).unwrap();
    fs::write(main.join("README.md"), "# Edited, not saved in the tree\n").unwrap();
    fs::remove_file(main.join("empty.txt")).unwrap();

    let dest = tmp.path().join("lane");
    let holds = |path: &Path, _: &TreeEntry, _: &fs::Metadata| path != Path::new("README.md");
    let source = Source { root: &main, holds: &holds };
    let done = materialize(&repo.tree, &dest, Some(&source), &|id| repo.blob(id)).unwrap();
    assert_eq!(done.files, 5);
    // The two regular files the source vouches for and has; the rest from the store.
    assert_eq!(done.cloned + done.copied, 2, "{done:?}");
    assert_eq!(done.written, 3, "{done:?}");
    check(&repo, &dest);

    // A change in the lane does not show in the main directory, nor the reverse.
    fs::write(dest.join("src/deep/er/main.rs"), "fn main() { lane(); }\n").unwrap();
    assert_eq!(fs::read(main.join("src/deep/er/main.rs")).unwrap(), b"fn main() {}\n");
    fs::write(main.join("scripts/run.sh"), "#!/bin/sh\necho changed\n").unwrap();
    assert_eq!(fs::read(dest.join("scripts/run.sh")).unwrap(), b"#!/bin/sh\necho hi\n");
}

#[test]
fn a_source_that_changes_while_it_is_taken_is_not_used() {
    let repo = sample();
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    materialize(&repo.tree, &main, None, &|id| repo.blob(id)).unwrap();
    // What the main directory has of this file is not what the tree says,
    // and the source finds out only when asked again, after the copy.
    fs::write(main.join("README.md"), "# Changed while the lane was being created\n").unwrap();
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let holds = |path: &Path, _: &TreeEntry, _: &fs::Metadata| {
        path != Path::new("README.md") || asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
    };
    let source = Source { root: &main, holds: &holds };
    let dest = tmp.path().join("lane");
    let done = materialize(&repo.tree, &dest, Some(&source), &|id| repo.blob(id)).unwrap();
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2);
    // The other three regular files come from the main directory; this one
    // and the link from the store.
    assert_eq!(done.cloned + done.copied, 3, "{done:?}");
    assert_eq!(done.written, 2, "{done:?}");
    // The lane has what the tree says, not what happened to be on disk.
    check(&repo, &dest);
}

#[test]
fn a_missing_blob_is_an_error_that_names_the_file() {
    let mut repo = sample();
    let entry = *repo.tree.get(Path::new("README.md")).unwrap();
    repo.blobs.remove(&entry.blob);
    let tmp = tempfile::tempdir().unwrap();
    let error = materialize(&repo.tree, &tmp.path().join("w"), None, &|id| repo.blob(id)).unwrap_err();
    assert!(error.to_string().contains("README.md"), "{error}");
}
