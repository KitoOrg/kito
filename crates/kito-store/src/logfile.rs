//! A single append-only log file: header, then records with a CRC.

use crate::error::{IoContext, Result, StoreError};
use crate::fs::{FileSystem, RawFile};
use crate::mmap::MappedFile;
use crate::record::{check_header, encode_header, encode_record, read_frame, Frame, LogKind, HEADER_LEN};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Outcome of the recovery of a log file on open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogRecovery {
    /// Valid records found in the active file.
    pub records: u64,
    /// Tail bytes discarded because they did not form a valid record.
    pub discarded_bytes: u64,
    /// Where the discarded tail was saved (`lost+found/`), if that was possible.
    pub saved_to: Option<PathBuf>,
}

/// Log file open for writing.
///
/// Records go through three states: in the in-memory buffer (`append`),
/// written to the file (`flush`), durable (`sync`). A write error poisons the
/// file: every later call fails until the store is reopened, and recovery on
/// open discards the partial record, if there is one.
pub(crate) struct LogFile {
    path: PathBuf,
    raw: Box<dyn RawFile>,
    reader: File,
    /// Valid bytes in the file (header + whole records).
    written: u64,
    /// There are bytes written but not yet made durable.
    dirty: bool,
    buf: Vec<u8>,
    map: Mutex<Option<Arc<MappedFile>>>,
    poisoned: bool,
}

impl LogFile {
    /// Creates a new file with just the header, atomically.
    pub(crate) fn create(fs: &dyn FileSystem, path: &Path, kind: LogKind, segment: u32) -> Result<LogFile> {
        fs.create_atomic(path, &encode_header(kind, segment)).at(path)?;
        LogFile::open_at(fs, path, HEADER_LEN as u64)
    }

    fn open_at(fs: &dyn FileSystem, path: &Path, written: u64) -> Result<LogFile> {
        Ok(LogFile {
            path: path.to_owned(),
            raw: fs.open_append(path).at(path)?,
            reader: File::open(path).at(path)?,
            written,
            dirty: false,
            buf: Vec::new(),
            map: Mutex::new(None),
            poisoned: false,
        })
    }

    /// Opens an existing file as the active file: verifies all the records
    /// and truncates the tail that does not form a valid record (a write
    /// interrupted by a crash). The discarded tail is first copied into
    /// `lost_found`.
    ///
    /// `floor` is the number of bytes of the file that are recorded as
    /// already confirmed (by the index watermark). An invalid record before
    /// that floor is not a torn tail but damage to confirmed data: recovery
    /// refuses to discard it and returns an error.
    pub(crate) fn recover(
        fs: &dyn FileSystem,
        path: &Path,
        kind: LogKind,
        segment: u32,
        lost_found: &Path,
        floor: Option<u64>,
    ) -> Result<(LogFile, LogRecovery)> {
        let reader = File::open(path).at(path)?;
        let len = reader.metadata().at(path)?.len();
        let (valid, records, tail) = {
            let map = MappedFile::map_prefix(&reader, len).at(path)?;
            let data = map.bytes();
            check_header(data, kind, segment).map_err(|reason| StoreError::corrupt(path, 0, reason))?;
            let mut off = HEADER_LEN;
            let mut records = 0u64;
            while let Frame::Record { end, .. } = read_frame(data, off) {
                off = end;
                records += 1;
            }
            (off as u64, records, data[off..].to_vec())
            // The mapping is released here, before any truncation.
        };
        drop(reader);

        if let Some(floor) = floor.filter(|&floor| valid < floor) {
            return Err(StoreError::corrupt(
                path,
                valid,
                format!(
                    "invalid record in an already confirmed region (confirmed {floor} bytes, valid {valid}): \
                     to accept the loss, reopen the store in repair mode"
                ),
            ));
        }

        let mut recovery = LogRecovery { records, discarded_bytes: len - valid, saved_to: None };
        if recovery.discarded_bytes > 0 {
            recovery.saved_to = save_lost_tail(lost_found, path, valid, &tail);
            let mut raw = fs.open_append(path).at(path)?;
            raw.truncate(valid).at(path)?;
            raw.sync().at(path)?;
        }
        let mut file = LogFile::open_at(fs, path, valid)?;
        // There is no telling whether the bytes found were ever synced (after
        // a `kill -9` they may be only in the operating system's cache): the
        // first `sync` must really fsync.
        file.dirty = true;
        Ok((file, recovery))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Logical length: bytes written plus those still in the buffer.
    pub(crate) fn len(&self) -> u64 {
        self.written + self.buf.len() as u64
    }

    /// Bytes in the buffer, not yet written to the file.
    pub(crate) fn buffered(&self) -> usize {
        self.buf.len()
    }

    fn check(&self) -> Result<()> {
        if self.poisoned {
            Err(StoreError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Appends a record (to the buffer) and returns its offset.
    pub(crate) fn append(&mut self, parts: &[&[u8]]) -> Result<u64> {
        self.check()?;
        let off = self.len();
        encode_record(&mut self.buf, parts).map_err(StoreError::RecordTooLarge)?;
        Ok(off)
    }

    /// Writes the buffer to the file.
    pub(crate) fn flush(&mut self) -> Result<()> {
        self.check()?;
        if self.buf.is_empty() {
            return Ok(());
        }
        match self.raw.append(&self.buf) {
            Ok(()) => {
                self.written += self.buf.len() as u64;
                self.buf.clear();
                self.dirty = true;
                Ok(())
            }
            Err(e) => {
                // The write may have got only halfway: try to undo it. If
                // that fails too, recovery takes care of it on the next
                // open.
                self.poisoned = true;
                let _ = self.raw.truncate(self.written);
                Err(e).at(&self.path)
            }
        }
    }

    /// Writes the buffer and makes everything durable.
    pub(crate) fn sync(&mut self) -> Result<()> {
        self.flush()?;
        if self.dirty {
            if let Err(e) = self.raw.sync() {
                // After a failed fsync there is no telling what reached the disk.
                self.poisoned = true;
                return Err(e).at(&self.path);
            }
            self.dirty = false;
        }
        Ok(())
    }

    fn map_covering(&self, off: u64) -> Result<Arc<MappedFile>> {
        let mut guard = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = guard.as_ref() {
            if off < map.len() {
                return Ok(map.clone());
            }
        }
        // Records are written whole (`flush`), so every record that starts
        // before `written` is entirely contained in the mapping.
        let map = Arc::new(MappedFile::map_prefix(&self.reader, self.written).at(&self.path)?);
        *guard = Some(map.clone());
        Ok(map)
    }

    /// Reads the record at offset `off` (even if it is still in the buffer).
    pub(crate) fn read_with<R>(&self, off: u64, f: impl FnOnce(&[u8], u32) -> R) -> Result<R> {
        if off < HEADER_LEN as u64 {
            return Err(StoreError::corrupt(&self.path, off, "offset inside the header"));
        }
        if off >= self.written {
            record_at(&self.path, &self.buf, off, self.written, f)
        } else {
            let map = self.map_covering(off)?;
            record_at(&self.path, map.bytes(), off, 0, f)
        }
    }

    /// Visits in order the records starting at `from`, including those in the buffer.
    pub(crate) fn scan(&self, from: u64, f: &mut Visit<'_>) -> Result<()> {
        let mut off = from.max(HEADER_LEN as u64);
        if off < self.written {
            let map = self.map_covering(self.written - 1)?;
            off = scan_slice(&self.path, map.bytes(), off, 0, f)?;
        }
        scan_slice(&self.path, &self.buf, off, self.written, f)?;
        Ok(())
    }

    /// Converts the active file into a sealed segment (after `sync`).
    pub(crate) fn seal(self) -> SealedFile {
        debug_assert!(self.buf.is_empty() && !self.dirty);
        SealedFile { path: self.path, reader: self.reader, len: self.written, map: self.map }
    }
}

/// Callback of a scan: record offset, payload, CRC.
pub(crate) type Visit<'a> = dyn FnMut(u64, &[u8], u32) -> Result<()> + 'a;

/// Reads the record at file offset `off` from `data`, whose bytes
/// correspond to the file offsets `base..`.
fn record_at<R>(path: &Path, data: &[u8], off: u64, base: u64, f: impl FnOnce(&[u8], u32) -> R) -> Result<R> {
    match usize::try_from(off - base).map(|at| read_frame(data, at)) {
        Ok(Frame::Record { payload, crc, .. }) => Ok(f(payload, crc)),
        Ok(Frame::End) | Err(_) => Err(StoreError::corrupt(path, off, "no record at this offset")),
        Ok(Frame::Torn(reason)) => Err(StoreError::corrupt(path, off, reason)),
    }
}

/// Visits the records of `data`, whose bytes correspond to the file offsets
/// `base..`. Returns the file offset after the last record.
fn scan_slice(path: &Path, data: &[u8], from: u64, base: u64, f: &mut Visit<'_>) -> Result<u64> {
    let mut off =
        usize::try_from(from - base).map_err(|_| StoreError::corrupt(path, from, "offset too large"))?;
    loop {
        match read_frame(data, off) {
            Frame::Record { payload, crc, end } => {
                f(base + off as u64, payload, crc)?;
                off = end;
            }
            Frame::End => return Ok(base + off as u64),
            Frame::Torn(reason) => return Err(StoreError::corrupt(path, base + off as u64, reason)),
        }
    }
}

/// Sealed segment: complete, durable, never modified again.
pub(crate) struct SealedFile {
    path: PathBuf,
    reader: File,
    len: u64,
    map: Mutex<Option<Arc<MappedFile>>>,
}

impl SealedFile {
    /// Opens a sealed segment checking only its header: the records are
    /// verified with the CRC on every read.
    pub(crate) fn open(path: &Path, kind: LogKind, segment: u32) -> Result<SealedFile> {
        use std::io::Read;
        let mut reader = File::open(path).at(path)?;
        let len = reader.metadata().at(path)?.len();
        let mut header = [0u8; HEADER_LEN];
        let got = reader.read(&mut header).at(path)?;
        check_header(&header[..got], kind, segment).map_err(|reason| StoreError::corrupt(path, 0, reason))?;
        Ok(SealedFile { path: path.to_owned(), reader, len, map: Mutex::new(None) })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    fn map(&self) -> Result<Arc<MappedFile>> {
        let mut guard = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = guard.as_ref() {
            if map.len() == self.len {
                return Ok(map.clone());
            }
        }
        let map = Arc::new(MappedFile::map_prefix(&self.reader, self.len).at(&self.path)?);
        *guard = Some(map.clone());
        Ok(map)
    }

    pub(crate) fn read_with<R>(&self, off: u64, f: impl FnOnce(&[u8], u32) -> R) -> Result<R> {
        if off < HEADER_LEN as u64 {
            return Err(StoreError::corrupt(&self.path, off, "offset inside the header"));
        }
        let map = self.map()?;
        record_at(&self.path, map.bytes(), off, 0, f)
    }

    /// Visits the records from `from`. In a sealed segment an invalid record
    /// is corruption of confirmed data, not a tail to discard.
    pub(crate) fn scan(&self, from: u64, f: &mut Visit<'_>) -> Result<()> {
        let map = self.map()?;
        scan_slice(&self.path, map.bytes(), from.max(HEADER_LEN as u64), 0, f)?;
        Ok(())
    }
}

/// Copies the discarded tail into `lost+found/`, for inspection. It is a
/// courtesy save: the discarded bytes were never confirmed, so a failure
/// (for example disk full) does not block recovery.
fn save_lost_tail(lost_found: &Path, path: &Path, offset: u64, tail: &[u8]) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    fs::create_dir_all(lost_found).ok()?;
    let target = lost_found.join(format!("{name}.{offset}.{stamp}"));
    fs::write(&target, tail).ok()?;
    Some(target)
}
