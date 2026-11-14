//! Import of the history of a git repository.

use crate::{git_err, oid20, GitError};
use gix::objs::tree::EntryKind;
use kito_core::{Author, Checkpoint, FileMode, HlcClock, LaneId, Op, OpId, Tree};
use kito_diff::fileops::{ops_for_changes, Snapshot};
use kito_store::Store;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Outcome of an import.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    /// Commits read (first-parent chain of HEAD).
    pub commits: usize,
    pub ops: usize,
    pub checkpoints: usize,
    /// Skipped entries (submodules, non-UTF-8 paths), one line per entry.
    pub skipped: Vec<String>,
}

type Entries = BTreeMap<Vec<u8>, (EntryKind, gix::ObjectId)>;
type Changes = BTreeMap<PathBuf, Option<(gix::ObjectId, FileMode)>>;

/// Imports the history of `root/.git` into the store (which must be empty).
///
/// The first-parent chain of HEAD is followed: each commit becomes the
/// `File*`/`TextEdit` ops that lead from its parent's tree to its own,
/// followed by a checkpoint that remembers the source commit (and that will
/// therefore not be re-exported). Merged branches show up as the effect of
/// the merge commit.
pub fn import(store: &mut Store, root: &Path, clock: &mut HlcClock) -> Result<Imported, GitError> {
    if store.op_count()? > 0 || store.checkpoints().next().is_some() {
        return Err(GitError::NotEmpty);
    }
    let repo = gix::open(root).map_err(|_| GitError::NotARepository(root.to_owned()))?;
    let mut report = Imported::default();
    let Ok(head) = repo.head_commit() else {
        return Ok(report); // repository with no commits
    };

    let mut chain = vec![head.id];
    let mut first_parent = head.parent_ids().next().map(|id| id.detach());
    while let Some(parent) = first_parent {
        chain.push(parent);
        first_parent = repo.find_commit(parent).map_err(git_err)?.parent_ids().next().map(|id| id.detach());
    }
    chain.reverse();

    let mut tree = Tree::new();
    let mut prev_tree: Option<gix::ObjectId> = None;
    let mut head_op: Option<OpId> = None;

    for commit_id in chain {
        let commit = repo.find_commit(commit_id).map_err(git_err)?;
        let tree_id = commit.tree_id().map_err(git_err)?.detach();
        let mut changes = Changes::new();
        diff_trees(&repo, prev_tree, Some(tree_id), "", &mut changes, &mut report.skipped)?;
        prev_tree = Some(tree_id);

        let mut snapshots: BTreeMap<PathBuf, Option<Snapshot>> = BTreeMap::new();
        for (path, change) in changes {
            let snapshot = match change {
                Some((blob_id, mode)) => {
                    let blob = repo.find_blob(blob_id).map_err(git_err)?;
                    // A file changed by a commit is stored as what changed
                    // since the version before it.
                    let stored = match tree.get(&path) {
                        Some(before) => store.put_blob_like(&blob.data, &before.blob)?,
                        None => store.put_blob(&blob.data)?,
                    };
                    Some(Snapshot { blob: stored, mode })
                }
                None => None,
            };
            snapshots.insert(path, snapshot);
        }
        let kinds = ops_for_changes(&tree, &snapshots, &mut |blob| store.get_blob(blob).ok().flatten());

        let signature = commit.author().map_err(git_err)?;
        let mut author = Author::human(signature.name.to_string().trim().to_owned());
        let email = signature.email.to_string();
        if !email.trim().is_empty() {
            author = author.with_email(email.trim());
        }
        let seconds = commit.time().map_err(git_err)?.seconds.max(0) as u64;
        for kind in kinds {
            let op = Op::new(head_op, LaneId::MAIN, author.clone(), clock.tick(seconds * 1000), kind, None)?;
            tree.apply(&op.kind)?;
            store.append_op(&op)?;
            head_op = Some(op.id);
            report.ops += 1;
        }
        report.commits += 1;

        // A checkpoint points to an op: empty commits at the start of the
        // history (before any file) have none and leave no trace.
        if let Some(head) = head_op {
            let message = commit.message_raw_sloppy().to_string();
            let checkpoint = Checkpoint {
                id: store.next_checkpoint_id(),
                head,
                label: message.lines().next().unwrap_or("").trim().to_owned(),
                created_by: author,
                verification: None,
                git_commit: Some(oid20(commit_id)),
            };
            store.put_checkpoint(&checkpoint)?;
            report.checkpoints += 1;
        }
    }
    store.commit()?;
    Ok(report)
}

fn entries(repo: &gix::Repository, id: Option<gix::ObjectId>) -> Result<Entries, GitError> {
    let mut out = Entries::new();
    if let Some(id) = id {
        let tree = repo.find_tree(id).map_err(git_err)?;
        for entry in tree.iter() {
            let entry = entry.map_err(git_err)?;
            out.insert(entry.filename().to_vec(), (entry.mode().kind(), entry.object_id()));
        }
    }
    Ok(out)
}

fn file_mode(kind: EntryKind) -> Option<FileMode> {
    match kind {
        EntryKind::Blob => Some(FileMode::Regular),
        EntryKind::BlobExecutable => Some(FileMode::Executable),
        EntryKind::Link => Some(FileMode::Symlink),
        EntryKind::Tree | EntryKind::Commit => None,
    }
}

/// Differences between two git trees, descending only into the directories that change.
fn diff_trees(
    repo: &gix::Repository,
    old: Option<gix::ObjectId>,
    new: Option<gix::ObjectId>,
    prefix: &str,
    out: &mut Changes,
    skipped: &mut Vec<String>,
) -> Result<(), GitError> {
    let (old, new) = (entries(repo, old)?, entries(repo, new)?);
    let names: std::collections::BTreeSet<&Vec<u8>> = old.keys().chain(new.keys()).collect();
    for name in names {
        let (a, b) = (old.get(name), new.get(name));
        if a == b {
            continue;
        }
        let Ok(name) = std::str::from_utf8(name) else {
            skipped.push(format!("{prefix}{}: non-UTF-8 name", String::from_utf8_lossy(name)));
            continue;
        };
        let path = format!("{prefix}{name}");
        for (kind, _) in a.iter().chain(b.iter()) {
            if *kind == EntryKind::Commit {
                skipped.push(format!("{path}: submodule"));
            }
        }
        let tree_of = |e: Option<&(EntryKind, gix::ObjectId)>| {
            e.filter(|(k, _)| *k == EntryKind::Tree).map(|(_, id)| *id)
        };
        let file_of =
            |e: Option<&(EntryKind, gix::ObjectId)>| e.and_then(|(k, id)| file_mode(*k).map(|m| (*id, m)));

        let (old_tree, new_tree) = (tree_of(a), tree_of(b));
        if old_tree.is_some() || new_tree.is_some() {
            diff_trees(repo, old_tree, new_tree, &format!("{path}/"), out, skipped)?;
        }
        match (file_of(a), file_of(b)) {
            (_, Some(file)) => {
                out.insert(PathBuf::from(&path), Some(file));
            }
            (Some(_), None) => {
                out.insert(PathBuf::from(&path), None);
            }
            (None, None) => {}
        }
    }
    Ok(())
}
