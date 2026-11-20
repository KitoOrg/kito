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
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// A directory that already holds some of the files to create.
pub struct Source<'a> {
    pub root: &'a Path,
    /// `true` if the file at this path under `root`, of which the file
    /// system says what is given, is known to hold exactly the content the
    /// tree asks for. It is asked before and after taking the content: a
    /// file that changes meanwhile is not used.
    pub holds: &'a (dyn Fn(&Path, &TreeEntry, &fs::Metadata) -> bool + Sync),
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

/// Files up to this size are copied with one read and one write.
const SMALL: u64 = 64 << 10;

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
    // Whether the file system shares data between files is found out with
    // the first file: where it does not, the others are copied right away.
    let reflinks = AtomicBool::new(true);
    let reflinks = &reflinks;
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).min(8);
    let chunk = entries.len().div_ceil(workers).max(1);
    let parts: Vec<io::Result<Materialized>> = std::thread::scope(|scope| {
        let handles: Vec<_> = entries
            .chunks(chunk)
            .map(|part| scope.spawn(move || create(part, dest, source, blob, stamp, reflinks)))
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
    reflinks: &AtomicBool,
) -> io::Result<Materialized> {
    let mut done = Materialized::default();
    for (path, entry) in entries {
        let to = dest.join(path);
        let context = |e: io::Error| other(format!("{}: {e}", to.display()));
        done.files += 1;
        if entry.mode == FileMode::Symlink {
            let target = blob(&entry.blob).map_err(context)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(String::from_utf8_lossy(&target).as_ref(), &to).map_err(context)?;
            // Where links need privileges (Windows), a file that holds the
            // target, as git does with `core.symlinks = false`.
            #[cfg(not(unix))]
            fs::write(&to, &target).map_err(context)?;
            done.written += 1;
            continue;
        }
        let mode = if entry.mode == FileMode::Executable { 0o755 } else { 0o644 };
        let create = || {
            let mut options = fs::File::options();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            options.mode(mode);
            options.open(&to)
        };
        let mut file = None;
        // `true` once something that is not the content was left at `to`.
        let mut leftover = false;
        if let Some(source) = source {
            let from = source.root.join(path);
            let holds =
                |meta: io::Result<fs::Metadata>| meta.is_ok_and(|meta| (source.holds)(path, entry, &meta));
            if reflinks.load(Ordering::Relaxed) && holds(fs::symlink_metadata(&from)) {
                match reflink_copy::reflink(&from, &to) {
                    Ok(()) if holds(fs::symlink_metadata(&from)) => {
                        file = Some(fs::File::open(&to).map_err(context)?);
                        done.cloned += 1;
                    }
                    Ok(()) => leftover = true,
                    // A source that went away says nothing about the file
                    // system; anything else does.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => reflinks.store(false, Ordering::Relaxed),
                }
            }
            if file.is_none() && !reflinks.load(Ordering::Relaxed) {
                if let Ok(mut original) = fs::File::open(&from) {
                    let before = original.metadata();
                    let size = before.as_ref().map_or(0, |meta| meta.len());
                    if holds(before) {
                        let mut copy = create().map_err(context)?;
                        leftover = true;
                        // Most files are small: one read, one write. The
                        // buffer has room for one byte more than the file,
                        // so a file that grew meanwhile is noticed.
                        let copied = if size <= SMALL {
                            let mut content = vec![0u8; size as usize + 1];
                            matches!(original.read(&mut content), Ok(n) if n as u64 == size)
                                && copy.write_all(&content[..size as usize]).is_ok()
                        } else {
                            io::copy(&mut original, &mut copy).is_ok()
                        };
                        if copied && holds(original.metadata()) {
                            file = Some(copy);
                            done.copied += 1;
                        }
                    }
                }
            }
        }
        let file = match file {
            Some(file) => file,
            None => {
                // No usable copy on disk (or it changed meanwhile): from
                // the store.
                if leftover {
                    let _ = fs::remove_file(&to);
                }
                let mut file = create().map_err(context)?;
                file.write_all(&blob(&entry.blob).map_err(context)?).map_err(context)?;
                done.written += 1;
                file
            }
        };
        file.set_modified(stamp).map_err(context)?;
        #[allow(unused_mut)]
        let mut meta = file.metadata().map_err(context)?;
        // A cloned file has the permissions of the original, and a created
        // one those the process is allowed to give: set only if they differ.
        // Windows has no executable bit: the mode stays in the tree.
        #[cfg(unix)]
        if meta.permissions().mode() & 0o777 != mode {
            file.set_permissions(fs::Permissions::from_mode(mode)).map_err(context)?;
            meta = file.metadata().map_err(context)?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        done.stats.push((path.to_path_buf(), meta));
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
