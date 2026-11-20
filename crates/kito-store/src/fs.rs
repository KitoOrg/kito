//! Write access to files, behind a trait: it makes it possible to inject
//! faults (disk full, failed fsync) in tests without touching the store's
//! code.

use std::fmt::Debug;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// A file opened in append-only mode.
pub trait RawFile: Send + Sync {
    /// Appends all the bytes at the end of the file. On error it may have
    /// written only part of them.
    fn append(&mut self, buf: &[u8]) -> io::Result<()>;
    /// Makes the written data durable (`fdatasync`).
    fn sync(&mut self) -> io::Result<()>;
    /// Truncates the file to `len` bytes.
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

/// The file system write operations used by the store.
pub trait FileSystem: Send + Sync + Debug {
    /// Opens an existing file in append-only mode.
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn RawFile>>;

    /// Creates (or replaces) `path` with `content` atomically:
    /// write to a temporary file, fsync, rename, fsync of the directory.
    fn create_atomic(&self, path: &Path, content: &[u8]) -> io::Result<()>;
}

/// The real file system.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFs;

struct StdFile {
    file: File,
    /// Where it is: on Windows a file opened to append cannot change its
    /// length, and a truncation goes through a handle of its own.
    #[cfg_attr(not(windows), allow(dead_code))]
    path: std::path::PathBuf,
}

impl RawFile for StdFile {
    fn append(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file.write_all(buf)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        // A file opened to append has no right to set its end on Windows
        // (FILE_APPEND_DATA without FILE_WRITE_DATA): recovery and the undo
        // of a partial write cut it through a handle opened to write
        // (ADR 0002). Every write still goes through the append handle.
        #[cfg(windows)]
        {
            OpenOptions::new().write(true).open(&self.path)?.set_len(len)
        }
        #[cfg(not(windows))]
        {
            self.file.set_len(len)
        }
    }
}

impl FileSystem for StdFs {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn RawFile>> {
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Box::new(StdFile { file, path: path.to_owned() }))
    }

    fn create_atomic(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        atomic_write(path, content)
    }
}

/// Suffix of the temporary files of atomic writes.
pub(crate) const TMP_SUFFIX: &str = ".tmp";

/// Atomic write: temporary file, fsync, rename, fsync of the directory.
/// After a crash the file has either the old content or the new one, whole.
pub fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(TMP_SUFFIX);
    let tmp = Path::new(&tmp);
    let result = (|| {
        let mut f = File::create(tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        fs::rename(tmp, path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Makes the entries of a directory durable (creations, renames, removals).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    // Windows cannot open a directory to flush it, and has nothing to
    // flush: NTFS journals its directory entries (ADR 0002).
    if cfg!(windows) {
        return Ok(());
    }
    File::open(dir)?.sync_all()
}

/// Removes the temporary files left behind by interrupted atomic writes.
pub(crate) fn remove_stray_tmp(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with(TMP_SUFFIX) && entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn stray_tmp_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("00000001.seg.tmp"), b"x").unwrap();
        fs::write(dir.path().join("00000000.seg"), b"y").unwrap();
        remove_stray_tmp(dir.path()).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["00000000.seg"]);
    }
}
