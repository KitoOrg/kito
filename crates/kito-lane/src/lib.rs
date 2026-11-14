//! `kito-lane`: the working copies of the lanes.
//!
//! A lane works in a directory of its own, `.kito/lanes/<id>/work`, that
//! starts as the files of the checkpoint the lane is based on. Creating it
//! must cost little even for a large repository: where the file system can
//! share the data of two files (reflink: btrfs, XFS, APFS) a file is cloned
//! from the copy the main working directory already has, without copying a
//! byte; elsewhere it is copied, and what has no usable copy on disk is
//! written from the store.
//!
//! Working copies are derived data: they can always be made again from the
//! store, so nothing here is fsynced.

use kito_core::{BlobId, FileMode, Tree, TreeEntry};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A directory that already holds some of the files to create.
pub struct Source<'a> {
    pub root: &'a Path,
    /// `true` if the file at this path under `root` is known to hold
    /// exactly the content the tree asks for.
    pub holds: &'a (dyn Fn(&Path, &TreeEntry) -> bool + Sync),
}

/// What [`materialize`] did.
#[derive(Clone, Debug, Default)]
pub struct Materialized {
    pub files: usize,
    /// Files that share their data with the source (no bytes copied).
    pub cloned: usize,
    /// Files copied from the source.
    pub copied: usize,
    /// Files written from the store.
    pub written: usize,
    /// Per regular file created, what the file system says of it: a scan
    /// that finds the same size and times knows the content is the tree's.
    pub stats: Vec<(PathBuf, fs::Metadata)>,
}

/// How far in the past the files are dated. A file changed right after it
/// was created could otherwise keep the same size and modification time,
/// and look untouched.
const BACKDATE: Duration = Duration::from_secs(2);

fn other(message: String) -> io::Error {
    io::Error::other(message)
}

/// Creates under `dest` the files of `tree`. `dest` must not contain them
/// already. `blob` reads a content from the store.
pub fn materialize(
    tree: &Tree,
    dest: &Path,
    source: Option<&Source<'_>>,
    blob: &(dyn Fn(&BlobId) -> io::Result<Vec<u8>> + Sync),
) -> io::Result<Materialized> {
    let entries: Vec<(&Path, &TreeEntry)> = tree.iter().collect();
    let mut dirs: BTreeSet<&Path> = BTreeSet::new();
    for (path, _) in &entries {
        if let Some(dir) = path.parent() {
            dirs.insert(dir);
        }
    }
    fs::create_dir_all(dest)?;
    for dir in dirs {
        fs::create_dir_all(dest.join(dir))?;
    }

    let stamp = SystemTime::now() - BACKDATE;
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).min(8);
    let chunk = entries.len().div_ceil(workers).max(1);
    let parts: Vec<io::Result<Materialized>> = std::thread::scope(|scope| {
        let handles: Vec<_> = entries
            .chunks(chunk)
            .map(|part| scope.spawn(move || create(part, dest, source, blob, stamp)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err(other("a worker panicked".to_owned()))))
            .collect()
    });
    let mut done = Materialized::default();
    for part in parts {
        let mut part = part?;
        done.files += part.files;
        done.cloned += part.cloned;
        done.copied += part.copied;
        done.written += part.written;
        done.stats.append(&mut part.stats);
    }
    Ok(done)
}

fn create(
    entries: &[(&Path, &TreeEntry)],
    dest: &Path,
    source: Option<&Source<'_>>,
    blob: &(dyn Fn(&BlobId) -> io::Result<Vec<u8>> + Sync),
    stamp: SystemTime,
) -> io::Result<Materialized> {
    let mut done = Materialized::default();
    for (path, entry) in entries {
        let to = dest.join(path);
        let context = |e: io::Error| other(format!("{}: {e}", to.display()));
        done.files += 1;
        if entry.mode == FileMode::Symlink {
            let target = blob(&entry.blob).map_err(context)?;
            std::os::unix::fs::symlink(String::from_utf8_lossy(&target).as_ref(), &to).map_err(context)?;
            done.written += 1;
            continue;
        }
        let from = source.filter(|s| (s.holds)(path, entry)).map(|s| s.root.join(path));
        let shared = match &from {
            // `Ok(None)`: cloned; `Ok(Some(_))`: copied, for lack of reflinks.
            Some(from) => reflink_copy::reflink_or_copy(from, &to).ok(),
            None => None,
        };
        match shared {
            Some(None) => done.cloned += 1,
            Some(Some(_)) => done.copied += 1,
            None => {
                // No copy on disk (or it went away meanwhile): from the store.
                let _ = fs::remove_file(&to);
                fs::write(&to, blob(&entry.blob).map_err(context)?).map_err(context)?;
                done.written += 1;
            }
        }
        let mode = if entry.mode == FileMode::Executable { 0o755 } else { 0o644 };
        let file = fs::File::open(&to).map_err(context)?;
        file.set_permissions(fs::Permissions::from_mode(mode)).map_err(context)?;
        file.set_modified(stamp).map_err(context)?;
        done.stats.push((path.to_path_buf(), file.metadata().map_err(context)?));
    }
    Ok(done)
}

/// Removes a working copy and everything in it.
pub fn remove(dest: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dest) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
