//! Append-only log split into numbered segments.

use crate::error::{IoContext, Result, StoreError};
use crate::fs::{remove_stray_tmp, FileSystem};
use crate::logfile::{LogFile, LogRecovery, SealedFile};
use crate::record::{framed_len, LogKind, HEADER_LEN};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Position of a record: segment and offset within the segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    pub seg: u32,
    pub off: u32,
}

impl Pos {
    pub const fn new(seg: u32, off: u32) -> Self {
        Pos { seg, off }
    }

    /// Ordered encoding in a `u64` (segment in the high 32 bits).
    pub const fn to_u64(self) -> u64 {
        ((self.seg as u64) << 32) | self.off as u64
    }

    pub const fn from_u64(v: u64) -> Self {
        Pos { seg: (v >> 32) as u32, off: v as u32 }
    }
}

pub(crate) struct LogOptions {
    pub kind: LogKind,
    /// Extension of the segment files (`seg`, `pack`).
    pub ext: &'static str,
    /// Size beyond which a new segment is opened.
    pub target_size: u64,
    /// If set, the buffer is written to the file when it exceeds this
    /// threshold. `None`: the caller decides (the oplog needs it, see `Store::commit`).
    pub auto_flush: Option<usize>,
}

pub(crate) struct SegmentedLog {
    dir: PathBuf,
    opts: LogOptions,
    fs: Arc<dyn FileSystem>,
    /// Sealed segments, in order; `sealed[i]` has number `first + i`.
    sealed: Vec<SealedFile>,
    first: u32,
    active: LogFile,
    active_no: u32,
}

impl SegmentedLog {
    pub(crate) fn open(
        dir: &Path,
        opts: LogOptions,
        fs: Arc<dyn FileSystem>,
        lost_found: &Path,
        floor: Option<Pos>,
    ) -> Result<(SegmentedLog, LogRecovery)> {
        fs::create_dir_all(dir).at(dir)?;
        remove_stray_tmp(dir).at(dir)?;

        let mut numbers = Vec::new();
        for entry in fs::read_dir(dir).at(dir)? {
            let name = entry.at(dir)?.file_name();
            if let Some(no) = parse_segment_name(&name.to_string_lossy(), opts.ext) {
                numbers.push(no);
            }
        }
        numbers.sort_unstable();
        for pair in numbers.windows(2) {
            if pair[1] != pair[0] + 1 {
                return Err(StoreError::corrupt(
                    dir,
                    0,
                    format!("segment {} is missing (found {} and {})", pair[0] + 1, pair[0], pair[1]),
                ));
            }
        }

        let lost = |what: String| {
            StoreError::corrupt(
                dir,
                0,
                format!("{what}: to accept the loss, reopen the store in repair mode"),
            )
        };
        let Some((&last, earlier)) = numbers.split_last() else {
            if let Some(floor) = floor.filter(|f| f.seg > 0 || f.off as usize > HEADER_LEN) {
                return Err(lost(format!(
                    "the log is empty but data is recorded as confirmed up to {floor:?}"
                )));
            }
            let active = LogFile::create(&*fs, &segment_path(dir, 0, opts.ext), opts.kind, 0)?;
            let log = SegmentedLog {
                dir: dir.to_owned(),
                opts,
                fs,
                sealed: Vec::new(),
                first: 0,
                active,
                active_no: 0,
            };
            return Ok((log, LogRecovery::default()));
        };

        let sealed = earlier
            .iter()
            .map(|&no| SealedFile::open(&segment_path(dir, no, opts.ext), opts.kind, no))
            .collect::<Result<Vec<_>>>()?;
        let first = numbers[0];
        // Confirmed-data floor, mapped onto the segment it falls in.
        let mut active_floor = None;
        if let Some(floor) = floor {
            if floor.seg > last {
                return Err(lost(format!(
                    "segments are missing: the last one is {last}, \
                     but data is recorded as confirmed up to {floor:?}"
                )));
            } else if floor.seg == last {
                active_floor = Some(floor.off as u64);
            } else if let Some(file) = floor.seg.checked_sub(first).and_then(|i| sealed.get(i as usize)) {
                if file.len() < floor.off as u64 {
                    return Err(lost(format!(
                        "segment {} is shorter than the confirmed data ({floor:?})",
                        floor.seg
                    )));
                }
            }
        }
        let (active, recovery) = LogFile::recover(
            &*fs,
            &segment_path(dir, last, opts.ext),
            opts.kind,
            last,
            lost_found,
            active_floor,
        )?;
        Ok((SegmentedLog { dir: dir.to_owned(), opts, fs, sealed, first, active, active_no: last }, recovery))
    }

    /// Position right after the last record (including those in the buffer).
    pub(crate) fn end(&self) -> Pos {
        Pos::new(self.active_no, self.active.len() as u32)
    }

    /// Position of the first record of the log.
    pub(crate) fn start(&self) -> Pos {
        Pos::new(self.first, HEADER_LEN as u32)
    }

    /// `true` if `pos` is a position between the start and the end of the log.
    pub(crate) fn contains(&self, pos: Pos) -> bool {
        if pos.seg < self.first || pos > self.end() || (pos.off as usize) < HEADER_LEN {
            return false;
        }
        match self.sealed_file(pos.seg) {
            Some(file) => pos.off as u64 <= file.len(),
            None => true,
        }
    }

    pub(crate) fn buffered(&self) -> usize {
        self.active.buffered()
    }

    pub(crate) fn segment_count(&self) -> usize {
        self.sealed.len() + 1
    }

    /// `true` if appending a record with this payload would open a new
    /// segment (and therefore make the current one durable).
    pub(crate) fn needs_roll(&self, payload_len: usize) -> bool {
        let len = self.active.len();
        len > HEADER_LEN as u64 && len + framed_len(payload_len) > self.opts.target_size
    }

    /// Appends a record whose payload is the concatenation of `parts`.
    pub(crate) fn append(&mut self, parts: &[&[u8]]) -> Result<Pos> {
        let payload_len: usize = parts.iter().map(|p| p.len()).sum();
        if self.needs_roll(payload_len) {
            self.roll()?;
        }
        let off = self.active.append(parts)?;
        // The start offset of a record never exceeds `target_size`
        // (≤ 1 GiB), so it fits in 32 bits.
        let off = u32::try_from(off).map_err(|_| StoreError::RecordTooLarge(payload_len))?;
        if let Some(limit) = self.opts.auto_flush {
            if self.active.buffered() >= limit {
                self.active.flush()?;
            }
        }
        Ok(Pos::new(self.active_no, off))
    }

    fn roll(&mut self) -> Result<()> {
        // The segment being closed must be durable before the next one
        // exists: that way only the last segment can have a torn tail.
        self.active.sync()?;
        let no = self
            .active_no
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidOp("maximum number of segments reached".into()))?;
        let next =
            LogFile::create(&*self.fs, &segment_path(&self.dir, no, self.opts.ext), self.opts.kind, no)?;
        let previous = std::mem::replace(&mut self.active, next);
        self.sealed.push(previous.seal());
        self.active_no = no;
        Ok(())
    }

    /// Writes the buffer to the file, without making it durable.
    #[allow(dead_code)]
    pub(crate) fn flush(&mut self) -> Result<()> {
        self.active.flush()
    }

    /// Makes durable everything that has been appended so far.
    pub(crate) fn sync(&mut self) -> Result<()> {
        self.active.sync()
    }

    fn sealed_file(&self, seg: u32) -> Option<&SealedFile> {
        seg.checked_sub(self.first).and_then(|i| self.sealed.get(i as usize))
    }

    /// Reads the record at `pos`; `f` receives the payload and its CRC.
    pub(crate) fn read_with<R>(&self, pos: Pos, f: impl FnOnce(&[u8], u32) -> R) -> Result<R> {
        if pos.seg == self.active_no {
            self.active.read_with(pos.off as u64, f)
        } else if let Some(file) = self.sealed_file(pos.seg) {
            file.read_with(pos.off as u64, f)
        } else {
            Err(StoreError::corrupt(&self.dir, pos.to_u64(), format!("segment {} does not exist", pos.seg)))
        }
    }

    /// Visits in order all the records from `from` onwards. `f` receives the
    /// position of the record, the payload and the CRC.
    pub(crate) fn scan(&self, from: Pos, mut f: impl FnMut(Pos, &[u8], u32) -> Result<()>) -> Result<()> {
        for (i, file) in self.sealed.iter().enumerate() {
            let seg = self.first + i as u32;
            if seg < from.seg {
                continue;
            }
            let start = if seg == from.seg { from.off as u64 } else { 0 };
            file.scan(start, &mut |off, payload, crc| f(Pos::new(seg, off as u32), payload, crc))?;
        }
        if from.seg <= self.active_no {
            let seg = self.active_no;
            let start = if seg == from.seg { from.off as u64 } else { 0 };
            self.active.scan(start, &mut |off, payload, crc| f(Pos::new(seg, off as u32), payload, crc))?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn active_path(&self) -> &Path {
        self.active.path()
    }
}

/// Position of the record that follows the one at `pos` with this payload.
pub(crate) fn pos_after(pos: Pos, payload_len: usize) -> Pos {
    Pos::new(pos.seg, pos.off + framed_len(payload_len) as u32)
}

pub(crate) fn segment_path(dir: &Path, no: u32, ext: &str) -> PathBuf {
    dir.join(format!("{no:08}.{ext}"))
}

fn parse_segment_name(name: &str, ext: &str) -> Option<u32> {
    let (stem, found_ext) = name.split_once('.')?;
    if found_ext != ext || stem.len() != 8 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}
