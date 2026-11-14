//! Export of checkpoints as git commits.

use crate::{git_err, object_id, oid20, GitError};
use gix::bstr::BString;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::PreviousValue;
use kito_core::{Checkpoint, CheckpointId, FileMode, LaneId, OpId, Tree};
use kito_store::Store;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// Branch to advance. `None`: the branch HEAD points to.
    pub branch: Option<String>,
    /// Domain of the commits' email addresses (`<author>@<domain>`).
    pub email_domain: String,
    /// Rewrites the git index from the last exported commit, so that
    /// `git status` comes out clean when the working directory matches.
    pub update_index: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions { branch: None, email_domain: "kito.invalid".into(), update_index: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exported {
    pub checkpoint: CheckpointId,
    pub commit: [u8; 20],
}

/// What the caller already knows of the two ends of an export: the files at
/// the last checkpoint exported and at the head of the main lane. When the
/// only checkpoint to export is at that head, the commit is made from the
/// difference between the two, without reading the log again.
pub struct Known<'a> {
    pub exported: CheckpointId,
    pub exported_tree: &'a Tree,
    pub head: OpId,
    pub tree: &'a Tree,
}

/// Exports to `root/.git` the checkpoints not yet exported, in order.
/// If `root` is not a git repository, it initializes it.
///
/// The export is deterministic: author, date (that of the checkpoint's last
/// op) and message depend only on the checkpoint, so redoing it gives the
/// same commits.
pub fn export(store: &mut Store, root: &Path, opts: &ExportOptions) -> Result<Vec<Exported>, GitError> {
    export_known(store, root, opts, None)
}

/// Like [`export`], for a caller that keeps the state of the main lane in
/// memory: see [`Known`]. The commits are the same.
pub fn export_known(
    store: &mut Store,
    root: &Path,
    opts: &ExportOptions,
    known: Option<&Known<'_>>,
) -> Result<Vec<Exported>, GitError> {
    let checkpoints: Vec<Checkpoint> = store.checkpoints().cloned().collect();
    let first = checkpoints.iter().position(|c| c.git_commit.is_none());
    if let Some(first) = first {
        if let Some(c) = checkpoints[first..].iter().find(|c| c.git_commit.is_some()) {
            return Err(GitError::Git(format!(
                "{} is already exported but an earlier checkpoint is not",
                c.id
            )));
        }
    }
    let exported_before = &checkpoints[..first.unwrap_or(checkpoints.len())];
    if first.is_none() && exported_before.is_empty() {
        return Ok(Vec::new());
    }

    let mut repo = match gix::open(root) {
        Ok(repo) => repo,
        Err(_) if !root.join(".git").exists() => gix::init(root).map_err(git_err)?,
        Err(e) => return Err(git_err(e)),
    };
    // The branch's reflog wants an identity: if git has none configured
    // (new machine, CI) a fallback one is used instead of failing.
    repo.committer_or_set_fallback("Kito", format!("kito@{}", opts.email_domain).as_str())
        .map_err(git_err)?;
    let branch = match &opts.branch {
        Some(name) if name.starts_with("refs/") => name.clone(),
        Some(name) => format!("refs/heads/{name}"),
        None => match repo.head_name().map_err(git_err)? {
            Some(name) => name.as_bstr().to_string(),
            None => "refs/heads/main".to_owned(),
        },
    };
    let tip: Option<gix::ObjectId> = match repo.try_find_reference(branch.as_str()).map_err(git_err)? {
        Some(mut reference) => Some(reference.peel_to_id().map_err(git_err)?.detach()),
        None => None,
    };

    // Where to resume from: the last checkpoint already exported.
    let prev = exported_before.last();
    let prev_commit = prev.and_then(|p| p.git_commit.as_ref()).map(object_id);
    if let (Some(expected), Some(found)) = (prev_commit, tip) {
        // The branch may have been left behind (export interrupted before
        // moving the reference): that is fine if it points to an ancestor of
        // our last commit, i.e. if realigning it is a fast-forward.
        if !is_first_parent_ancestor(&repo, found, expected, exported_before.len() + 1)? {
            return Err(GitError::Diverged {
                branch,
                expected: expected.to_string(),
                found: found.to_string(),
            });
        }
    }

    let mut out = Progress {
        parent: prev_commit.or(tip),
        base_tree: match prev_commit {
            Some(commit) => repo.find_commit(commit).map_err(git_err)?.tree_id().map_err(git_err)?.detach(),
            None => repo.empty_tree().id,
        },
        last_commit: prev_commit,
        last_tree: None,
        done: Vec::new(),
    };

    // One checkpoint to export, at the head of the main lane, right after
    // the one the caller knows the files of: the usual case in the daemon.
    let shortcut = first.and_then(|first| {
        let known = known?;
        let [only] = &checkpoints[first..] else { return None };
        (only.head == known.head && prev.map(|p| p.id) == Some(known.exported)).then_some((only, known))
    });
    if let Some((ck, known)) = shortcut {
        let at = store.get_op(&ck.head)?.ok_or(GitError::HeadNotOnMain(ck.id, ck.head))?.at.wall_ms;
        let dirty: Vec<&Path> = known.exported_tree.differences(known.tree).collect();
        commit_checkpoint(&repo, store, opts, ck, known.tree, &dirty, at, &mut out)?;
    } else if let Some(first) = first {
        let prev_head = prev.map(|p| p.head);
        let mut tracking = prev.is_none();
        let mut dirty: BTreeSet<PathBuf> = BTreeSet::new();
        let mut tree = Tree::new();
        let mut todo = checkpoints[first..].iter().peekable();
        let mut start = 0u64;

        'replay: loop {
            let page = store.lane_ops(LaneId::MAIN, start, 1000)?;
            if page.is_empty() {
                break;
            }
            start += page.len() as u64;
            for op in page {
                tree.apply(&op.kind)?;
                if tracking {
                    dirty.extend(op.kind.paths().into_iter().cloned());
                }
                if Some(op.id) == prev_head {
                    tracking = true;
                }
                while let Some(ck) = todo.next_if(|c| c.head == op.id) {
                    let paths: Vec<&Path> = dirty.iter().map(PathBuf::as_path).collect();
                    commit_checkpoint(&repo, store, opts, ck, &tree, &paths, op.at.wall_ms, &mut out)?;
                    dirty.clear();
                }
                if todo.peek().is_none() {
                    break 'replay;
                }
            }
        }
        if let Some(ck) = todo.peek() {
            return Err(GitError::HeadNotOnMain(ck.id, ck.head));
        }
    }
    let Progress { last_commit, last_tree, done, .. } = out;

    if let Some(commit) = last_commit {
        if tip != Some(commit) {
            let message = format!(
                "kito: export up to {}",
                checkpoints.last().map(|c| c.id.to_string()).unwrap_or_default()
            );
            repo.reference(branch.as_str(), commit, PreviousValue::Any, message).map_err(git_err)?;
        }
        let head_is_branch =
            repo.head_name().map_err(git_err)?.is_some_and(|n| n.as_bstr() == branch.as_str());
        if opts.update_index && head_is_branch {
            let tree_id = match last_tree {
                Some(id) => id,
                None => repo.find_commit(commit).map_err(git_err)?.tree_id().map_err(git_err)?.detach(),
            };
            let mut index = repo.index_from_tree(&tree_id).map_err(git_err)?;
            index.write(Default::default()).map_err(git_err)?;
        }
    }
    Ok(done)
}

/// Where an export has got to.
struct Progress {
    parent: Option<gix::ObjectId>,
    base_tree: gix::ObjectId,
    last_commit: Option<gix::ObjectId>,
    last_tree: Option<gix::ObjectId>,
    done: Vec<Exported>,
}

/// Writes the commit of one checkpoint: the tree of the commit before it
/// with the files at `dirty` as `tree` has them now.
#[allow(clippy::too_many_arguments)]
fn commit_checkpoint(
    repo: &gix::Repository,
    store: &mut Store,
    opts: &ExportOptions,
    ck: &Checkpoint,
    tree: &Tree,
    dirty: &[&Path],
    wall_ms: u64,
    out: &mut Progress,
) -> Result<(), GitError> {
    let mut editor = repo.edit_tree(out.base_tree).map_err(git_err)?;
    for path in dirty {
        let rela = path.to_str().ok_or_else(|| git_err(format!("non-UTF-8 path: {path:?}")))?;
        match tree.get(path) {
            Some(entry) => {
                let content = store.get_blob(&entry.blob)?.ok_or(GitError::MissingBlob(entry.blob))?;
                let blob = repo.write_blob(&content).map_err(git_err)?.detach();
                let kind = match entry.mode {
                    FileMode::Regular => EntryKind::Blob,
                    FileMode::Executable => EntryKind::BlobExecutable,
                    FileMode::Symlink => EntryKind::Link,
                };
                editor.upsert(rela, kind, blob).map_err(git_err)?;
            }
            None => {
                editor.remove(rela).map_err(git_err)?;
            }
        }
    }
    let tree_id = editor.write().map_err(git_err)?.detach();

    let time = gix::date::Time { seconds: (wall_ms / 1000) as i64, offset: 0 };
    let name = ck.created_by.name.clone();
    // The real email, if the author has one: it is what GitHub uses to
    // attribute the commit. Otherwise a placeholder address.
    let email = ck
        .created_by
        .email
        .clone()
        .unwrap_or_else(|| format!("{}@{}", email_local_part(&name), opts.email_domain));
    let signature = gix::actor::Signature { name: name.into(), email: email.into(), time };
    let commit = gix::objs::Commit {
        tree: tree_id,
        parents: out.parent.into_iter().collect(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: commit_message(ck),
        extra_headers: Vec::new(),
    };
    let commit_id = repo.write_object(&commit).map_err(git_err)?.detach();

    // First it is recorded in the store, then (at the end) the branch is
    // moved: if the process dies in between, the next export finds the
    // branch behind and realigns it.
    let mut updated = ck.clone();
    updated.git_commit = Some(oid20(commit_id));
    store.put_checkpoint(&updated)?;
    out.done.push(Exported { checkpoint: ck.id, commit: oid20(commit_id) });

    out.parent = Some(commit_id);
    out.base_tree = tree_id;
    out.last_commit = Some(commit_id);
    out.last_tree = Some(tree_id);
    Ok(())
}

/// `true` if `ancestor` is `descendant` or can be reached from it by walking
/// up at most `max_steps` first parents.
fn is_first_parent_ancestor(
    repo: &gix::Repository,
    ancestor: gix::ObjectId,
    descendant: gix::ObjectId,
    max_steps: usize,
) -> Result<bool, GitError> {
    let mut cursor = Some(descendant);
    for _ in 0..=max_steps {
        match cursor {
            Some(id) if id == ancestor => return Ok(true),
            Some(id) => {
                cursor = repo.find_commit(id).map_err(git_err)?.parent_ids().next().map(|p| p.detach());
            }
            None => break,
        }
    }
    Ok(false)
}

fn commit_message(ck: &Checkpoint) -> BString {
    let label = ck.label.trim();
    let subject = if label.is_empty() { format!("Checkpoint {}", ck.id) } else { label.to_owned() };
    let mut message = format!("{subject}\n\nKito-Checkpoint: {}\n", ck.id);
    if let Some(v) = &ck.verification {
        let outcome = if v.passed { "passed" } else { "failed" };
        message.push_str(&format!("Kito-Verification: {outcome} ({} ms)\n", v.duration_ms));
    }
    message.into()
}

/// Local part of an email address derived from the author's name.
fn email_local_part(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "kito".to_owned()
    } else {
        trimmed.to_owned()
    }
}
