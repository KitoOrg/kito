//! Support for the fault injection tests: a file system that runs out of
//! space, fails `fsync` on command and can simulate a power loss.

use crate::fs::{FileSystem, RawFile, StdFs};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct Faults {
    /// Bytes still writable before "disk full" (`u64::MAX` = unlimited).
    space: AtomicU64,
    /// fsyncs still granted before they start failing (`u64::MAX` = unlimited).
    syncs_left: AtomicU64,
    fail_truncate: AtomicBool,
    /// For each file written: how many bytes are durable (covered by an fsync).
    durable: Mutex<HashMap<PathBuf, u64>>,
}

/// Real file system with injectable faults. The clones share the state.
#[derive(Debug, Clone)]
pub struct FaultyFs {
    faults: Arc<Faults>,
}

impl Default for FaultyFs {
    fn default() -> Self {
        Self::new()
    }
}

/// What is left, after a power loss, of the bytes of a file that were
/// written but not synced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsynced {
    /// All lost: the file goes back to its durable length.
    Lost,
    /// Only a prefix of them survives.
    Partial,
    /// They all survive.
    Kept,
    /// The length survives but the content is zeroed.
    Zeroed,
    /// The length survives but the content is garbage.
    Garbage,
}

impl FaultyFs {
    pub fn new() -> Self {
        FaultyFs {
            faults: Arc::new(Faults {
                space: AtomicU64::new(u64::MAX),
                syncs_left: AtomicU64::new(u64::MAX),
                fail_truncate: AtomicBool::new(false),
                durable: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// After `bytes` more bytes written, writes fail with "disk full"; the
    /// write that crosses the limit gets only halfway.
    pub fn set_space(&self, bytes: u64) {
        self.faults.space.store(bytes, Ordering::SeqCst);
    }

    /// Removes the space limit.
    pub fn free_space(&self) {
        self.faults.space.store(u64::MAX, Ordering::SeqCst);
    }

    /// Makes all the fsyncs fail (or succeed again).
    pub fn fail_sync(&self, on: bool) {
        self.faults.syncs_left.store(if on { 0 } else { u64::MAX }, Ordering::SeqCst);
    }

    /// Grants `n` more fsyncs, then makes them all fail.
    pub fn fail_sync_after(&self, n: u64) {
        self.faults.syncs_left.store(n, Ordering::SeqCst);
    }

    /// Also fails the truncation with which the store undoes a partial
    /// write: a half-written record is left in the file.
    pub fn fail_truncate(&self, on: bool) {
        self.faults.fail_truncate.store(on, Ordering::SeqCst);
    }

    /// Durable bytes of a file written through this file system.
    pub fn durable_len(&self, path: &Path) -> Option<u64> {
        self.faults.durable.lock().unwrap().get(path).copied()
    }

    /// Simulates a power loss: for each file written, the bytes past the
    /// last fsync suffer the fate chosen by `fate` (which receives a counter
    /// to vary the choices). The store must already have been dropped.
    /// Returns how many files were altered.
    pub fn power_loss(&self, mut fate: impl FnMut(&Path, u64) -> (Unsynced, u64)) -> io::Result<usize> {
        let durable = std::mem::take(&mut *self.faults.durable.lock().unwrap());
        let mut files: Vec<_> = durable.into_iter().collect();
        files.sort();
        let mut altered = 0;
        for (path, synced) in files {
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            let len = meta.len();
            if len <= synced {
                continue;
            }
            let unsynced = len - synced;
            let (how, salt) = fate(&path, unsynced);
            let mut file = OpenOptions::new().write(true).open(&path)?;
            match how {
                Unsynced::Kept => continue,
                Unsynced::Lost => file.set_len(synced)?,
                Unsynced::Partial => file.set_len(synced + salt % unsynced)?,
                Unsynced::Zeroed | Unsynced::Garbage => {
                    let mut state = salt | 1;
                    let fill: Vec<u8> = (0..unsynced)
                        .map(|_| {
                            if how == Unsynced::Zeroed {
                                return 0;
                            }
                            state ^= state << 13;
                            state ^= state >> 7;
                            state ^= state << 17;
                            state as u8
                        })
                        .collect();
                    file.seek(SeekFrom::Start(synced))?;
                    file.write_all(&fill)?;
                }
            }
            file.sync_all()?;
            altered += 1;
        }
        Ok(altered)
    }

    /// Consumes up to `want` bytes of space; returns how many it grants.
    fn take(&self, want: u64) -> u64 {
        let space = &self.faults.space;
        let mut current = space.load(Ordering::SeqCst);
        loop {
            if current == u64::MAX {
                return want;
            }
            let granted = want.min(current);
            match space.compare_exchange(current, current - granted, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return granted,
                Err(actual) => current = actual,
            }
        }
    }

    fn set_durable(&self, path: &Path, len: u64) {
        self.faults.durable.lock().unwrap().insert(path.to_owned(), len);
    }
}

fn disk_full() -> io::Error {
    io::Error::new(io::ErrorKind::StorageFull, "out of space (injected fault)")
}

struct FaultyFile {
    inner: Box<dyn RawFile>,
    fs: FaultyFs,
    path: PathBuf,
    len: u64,
}

impl RawFile for FaultyFile {
    fn append(&mut self, buf: &[u8]) -> io::Result<()> {
        let granted = self.fs.take(buf.len() as u64) as usize;
        self.inner.append(&buf[..granted])?;
        self.len += granted as u64;
        if granted < buf.len() {
            return Err(disk_full());
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let left = &self.fs.faults.syncs_left;
        // `fetch_update` is deprecated in favour of `try_update` from Rust
        // 1.100; kept so the workspace still builds on 1.90.
        #[allow(deprecated)]
        let granted = left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
                u64::MAX => Some(n),
                0 => None,
                n => Some(n - 1),
            })
            .is_ok();
        if !granted {
            return Err(io::Error::other("fsync failed (injected fault)"));
        }
        self.inner.sync()?;
        self.fs.set_durable(&self.path, self.len);
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        if self.fs.faults.fail_truncate.load(Ordering::SeqCst) {
            return Err(io::Error::other("truncate failed (injected fault)"));
        }
        self.inner.truncate(len)?;
        self.len = len;
        if self.fs.durable_len(&self.path).is_some_and(|d| d > len) {
            self.fs.set_durable(&self.path, len);
        }
        Ok(())
    }
}

impl FileSystem for FaultyFs {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn RawFile>> {
        let len = fs::metadata(path)?.len();
        // What is on disk at open time has, by definition, survived.
        self.faults.durable.lock().unwrap().entry(path.to_owned()).or_insert(len);
        Ok(Box::new(FaultyFile {
            inner: StdFs.open_append(path)?,
            fs: self.clone(),
            path: path.to_owned(),
            len,
        }))
    }

    fn create_atomic(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        if self.take(content.len() as u64) < content.len() as u64 {
            return Err(disk_full());
        }
        StdFs.create_atomic(path, content)?;
        self.set_durable(path, content.len() as u64);
        Ok(())
    }
}
