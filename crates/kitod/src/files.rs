//! A lane's files for clients on other machines: reading them, and
//! recording the saves of a writer whose working copy is elsewhere.
//!
//! The working copy of a lane on the server stays what the lane is made
//! of. A writer on another machine sends each file it saved together with
//! the version it started from: when the lane still has that version the
//! file is written as sent; when the lane moved on meanwhile (an
//! integration brought it up to the main lane) the two are merged, and
//! when they do not merge nothing is changed and the writer is told.

use crate::error::{Error, Result};
use crate::repo::{Repo, Unsettled};
use kito_core::{BlobId, FileMode, LaneId, TreeEntry};
use kito_diff::ast::Identities;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A file of a lane, as a client is told.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    /// The ID of its content: `get_blobs` gives the content.
    pub blob: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
    /// A symbolic link: the content is its target.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub symlink: bool,
}

/// What became of a file a writer sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Put {
    /// `recorded`: the lane has the file as it was sent. `merged`: the lane
    /// had moved on, and has the two merged. `conflict`: they do not
    /// merge, and nothing was changed.
    pub result: String,
    /// The content the lane has for the path now; `None`: no such file.
    pub blob: Option<String>,
}

fn blob_of(text: &str) -> Result<BlobId> {
    text.parse().map_err(|_| Error::Other(format!("{text:?} is not the ID of a content")))
}

impl Repo {
    /// The files of a lane as recorded; with `paths`, the ones among them
    /// that exist. A closed lane's are those it had when it closed.
    pub fn lane_files(&self, lane: LaneId, paths: Option<&[PathBuf]>) -> Result<Vec<FileEntry>> {
        match self.is_closed(lane) {
            true => Ok(files_of(&self.tree_of(crate::history::Point::Lane(lane))?, paths)),
            false => Ok(files_of(&self.work(lane)?.tree, paths)),
        }
    }

    /// The files as they were at a point: a checkpoint (`ckpt#4`), `main`,
    /// or a lane (a closed one as it ended).
    pub fn files_at(&self, at: &str, paths: Option<&[PathBuf]>) -> Result<Vec<FileEntry>> {
        Ok(files_of(&self.tree_of(self.point(at)?)?, paths))
    }
}

/// The files of a tree, as a client is told; with `paths`, the ones among
/// them that exist.
fn files_of(tree: &kito_core::Tree, paths: Option<&[PathBuf]>) -> Vec<FileEntry> {
    let told = |path: &Path, entry: &TreeEntry| FileEntry {
        path: path.display().to_string(),
        blob: entry.blob.to_string(),
        executable: entry.mode == FileMode::Executable,
        symlink: entry.mode == FileMode::Symlink,
    };
    match paths {
        Some(paths) => {
            paths.iter().filter_map(|path| tree.get(path).map(|entry| told(path, entry))).collect()
        }
        None => tree.iter().map(|(path, entry)| told(path, entry)).collect(),
    }
}

impl Repo {
    /// A content, by its ID.
    pub fn content(&self, blob: &str) -> Result<Vec<u8>> {
        self.store.get_blob(&blob_of(blob)?)?.ok_or_else(|| Error::Other(format!("no content {blob} here")))
    }

    /// A lane is written to by the person it was opened for.
    pub fn may_write(&self, lane: LaneId, person: Option<&str>) -> Result<()> {
        let Some(person) = person else { return Ok(()) };
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        match at.record.owner.as_ref().map(|owner| owner.name.as_str()) {
            Some(owner) if owner != person => {
                Err(Error::Refused(format!("{lane} is {owner}'s: only its owner writes in it")))
            }
            _ => Ok(()),
        }
    }

    fn written_from_elsewhere(&self, lane: LaneId, path: &str) -> Result<PathBuf> {
        if lane.is_main() {
            return Err(Error::Refused("the main lane is not written to from elsewhere: open a lane".into()));
        }
        let rel = crate::scope::clean(path).map_err(|e| Error::Refused(e.to_string()))?;
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let inside = rel.components().next().is_some_and(|first| {
            let first = first.as_os_str();
            first != ".kito" && first != ".git"
        });
        if !inside || at.work.ignore.is_ignored(&rel, false) {
            return Err(Error::Refused(format!(
                "{}: ignored in this repository, not recorded",
                rel.display()
            )));
        }
        Ok(rel)
    }

    /// Records a file saved by a writer whose working copy is elsewhere.
    /// `base` is the content the writer started from; `None` for a file it
    /// believes is new.
    pub fn put_file(
        &mut self,
        lane: LaneId,
        path: &str,
        content: &[u8],
        executable: bool,
        base: Option<&str>,
    ) -> Result<Put> {
        let rel = self.written_from_elsewhere(lane, path)?;
        let base = base.map(blob_of).transpose()?;
        // What the working copy here has is what the lane has: a file
        // changed here a moment ago is recorded before it is compared.
        self.capture_lane(lane, [rel.clone()])?;
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let (current, root) = (at.work.tree.get(&rel).copied(), at.work.root.clone());
        let mode = if executable { FileMode::Executable } else { FileMode::Regular };
        let sent = BlobId::of(content);
        let put = |result: &str, blob: Option<BlobId>| Put {
            result: result.to_owned(),
            blob: blob.map(|blob| blob.to_string()),
        };
        // The lane has it already: a save sent twice is one save.
        if current.is_some_and(|entry| entry.blob == sent && entry.mode == mode) {
            return Ok(put("recorded", Some(sent)));
        }
        let (text, result) = match (current, base) {
            // New here, and new for the writer.
            (None, None) => (content.to_vec(), "recorded"),
            // The lane no longer has the file the writer changed.
            (None, Some(_)) => return Ok(put("conflict", None)),
            (Some(entry), Some(base)) if entry.blob == base => (content.to_vec(), "recorded"),
            // The lane moved on since the writer read the file.
            (Some(entry), base) => {
                let was = match base {
                    Some(base) => self.store.get_blob(&base)?,
                    None => None,
                };
                let theirs = self.store.get_blob(&entry.blob)?.unwrap_or_default();
                match was.and_then(|was| kito_diff::text::merge3(&was, &theirs, content)) {
                    Some(merged) => (merged, "merged"),
                    None => return Ok(put("conflict", Some(entry.blob))),
                }
            }
        };
        let blob = match current {
            Some(entry) => self.store.put_blob_like(&text, &entry.blob)?,
            None => self.store.put_blob(&text)?,
        };
        self.write_file_in(&root, &rel, &TreeEntry { blob, mode })?;
        self.with(lane, |repo, work| {
            let intent = work.intent;
            repo.capture_work(work, [rel.clone()], intent, &Identities::new(), Unsettled::Trust)
        })?;
        Ok(put(result, Some(blob)))
    }

    /// Records the removal of a file by a writer whose working copy is
    /// elsewhere. A file the lane changed since is not removed.
    pub fn remove_file(&mut self, lane: LaneId, path: &str, base: Option<&str>) -> Result<Put> {
        let rel = self.written_from_elsewhere(lane, path)?;
        let base = base.map(blob_of).transpose()?;
        self.capture_lane(lane, [rel.clone()])?;
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let (current, root) = (at.work.tree.get(&rel).copied(), at.work.root.clone());
        let Some(entry) = current else {
            return Ok(Put { result: "recorded".to_owned(), blob: None });
        };
        if base != Some(entry.blob) {
            return Ok(Put { result: "conflict".to_owned(), blob: Some(entry.blob.to_string()) });
        }
        crate::integrate::remove_in(&root, &rel)?;
        self.capture_reported(lane, [(rel, Some(None))])?;
        Ok(Put { result: "recorded".to_owned(), blob: None })
    }
}
