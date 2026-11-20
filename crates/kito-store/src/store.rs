//! The store: blob packs, oplog, checkpoints and indexes, with a single writer.

use crate::checkpoints::CheckpointLog;
use crate::error::{IoContext, Result, StoreError};
use crate::fs::{atomic_write, remove_stray_tmp, FileSystem, StdFs};
use crate::index::{Index, IndexBatch, IndexedOp, Watermark};
use crate::intents::IntentLog;
use crate::keyed::KeyedLog;
use crate::logfile::LogRecovery;
use crate::objects::{Packs, Stored};
use crate::record::LogKind;
use crate::segments::{pos_after, LogOptions, Pos, SegmentedLog};
use kito_core::{
    codec, BlobId, Checkpoint, CheckpointId, Conflict, ConflictId, Intent, IntentId, Lane, LaneId, Op, OpId,
    SymbolId,
};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Conventional name of the store directory inside a repository.
pub const STORE_DIR: &str = ".kito";

const FORMAT_FILE: &str = "format";
const FORMAT: &str = "kito-store 2\n";
/// The format before blobs could be stored as deltas: read as it is, and
/// marked as the current one, since from now on it may hold them.
const FORMAT_WITHOUT_DELTAS: &str = "kito-store 1\n";
const LOCK_FILE: &str = "lock";
/// How long a lock that is held is asked for again before the store is
/// said to be in use: longer than a fork takes to reach its exec.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOST_FOUND: &str = "lost+found";
/// Records per transaction when the index is caught up or rebuilt.
const INDEX_BATCH: usize = 20_000;

/// Parameters of the store. The defaults are those of the specification.
#[derive(Clone, Debug)]
pub struct StoreOptions {
    /// Size of the oplog segments (default 64 MiB).
    pub segment_size: u64,
    /// Size of the blob packs (default 64 MiB).
    pub pack_size: u64,
    /// zstd compression level of the blobs.
    pub zstd_level: i32,
    /// Bytes of unconfirmed ops beyond which `append_op` commits on its own.
    pub max_buffered: usize,
    /// How often the index is made durable. Between one fsync and the next
    /// the commits of the index stay in memory: after a crash they are
    /// recovered by rereading the tail of the logs.
    pub index_sync_interval: Duration,
    /// Repair mode. Normally opening refuses to discard data that was
    /// recorded as already confirmed (files truncated or damaged from
    /// outside) and returns [`StoreError::Corrupt`]. With `repair` it
    /// discards that data anyway, keeps what precedes the damage and
    /// rebuilds the indexes: it is a data loss that is accepted explicitly.
    pub repair: bool,
    /// File system used for the writes (replaceable in tests).
    pub fs: Arc<dyn FileSystem>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            segment_size: 64 << 20,
            pack_size: 64 << 20,
            zstd_level: 3,
            max_buffered: 4 << 20,
            index_sync_interval: Duration::from_secs(1),
            repair: false,
            fs: Arc::new(StdFs),
        }
    }
}

impl StoreOptions {
    fn validate(&self) -> Result<()> {
        const MIN: u64 = 1 << 10;
        const MAX: u64 = 1 << 30;
        for (name, size) in [("segment_size", self.segment_size), ("pack_size", self.pack_size)] {
            if !(MIN..=MAX).contains(&size) {
                return Err(StoreError::InvalidOp(format!(
                    "{name} = {size} outside the range {MIN}..={MAX}"
                )));
            }
        }
        Ok(())
    }
}

/// What opening did to catch the index up with the logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexRecovery {
    /// The index already reflected all the logs.
    UpToDate,
    /// The index was behind: the tail of the logs was reread.
    CaughtUp { ops: u64, blobs: u64 },
    /// The index was missing or not consistent with the logs: rebuilt from scratch.
    Rebuilt { reason: String, ops: u64, blobs: u64 },
}

/// Report of the recovery performed by [`Store::open`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub oplog: LogRecovery,
    pub packs: LogRecovery,
    pub checkpoints: LogRecovery,
    pub intents: LogRecovery,
    pub lanes: LogRecovery,
    pub conflicts: LogRecovery,
    pub index: IndexRecovery,
}

impl RecoveryReport {
    /// Tail bytes discarded in total (records never confirmed).
    pub fn discarded_bytes(&self) -> u64 {
        self.oplog.discarded_bytes
            + self.packs.discarded_bytes
            + self.checkpoints.discarded_bytes
            + self.intents.discarded_bytes
            + self.lanes.discarded_bytes
            + self.conflicts.discarded_bytes
    }
}

/// State of a lane according to the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneSummary {
    pub lane: LaneId,
    /// Number of ops recorded in the lane.
    pub ops: u64,
    /// Last op recorded in the lane.
    pub head: OpId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreStats {
    pub ops: u64,
    pub blobs: u64,
    pub oplog_segments: usize,
    pub packs: usize,
    pub checkpoints: usize,
}

/// Outcome of [`Store::verify`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub ops: u64,
    pub blobs: u64,
    pub lanes: usize,
    pub checkpoints: usize,
    pub problems: Vec<String>,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Ops and blobs appended but not yet confirmed or indexed.
#[derive(Default)]
struct Pending {
    ops: HashMap<OpId, Pos>,
    op_list: Vec<IndexedOp>,
    blobs: HashMap<BlobId, Pos>,
    blob_list: Vec<(BlobId, Pos)>,
}

/// The store of a repository. Only one process at a time can open it.
///
/// Writes ([`Store::put_blob`], [`Store::append_op`]) are queued;
/// [`Store::commit`] makes them durable. An op is *confirmed* once the
/// `commit` that follows it has returned `Ok`.
/// The contents of the blobs read or written last.
#[derive(Default)]
struct Recent {
    blobs: std::collections::VecDeque<(BlobId, std::sync::Arc<[u8]>)>,
    bytes: usize,
}

impl Recent {
    const MAX_BLOBS: usize = 8;
    const MAX_BYTES: usize = 8 << 20;
    /// Larger contents are not kept.
    const MAX_BLOB: usize = 2 << 20;

    fn get(&self, id: &BlobId) -> Option<Vec<u8>> {
        self.blobs.iter().find(|(known, _)| known == id).map(|(_, data)| data.to_vec())
    }

    fn put(&mut self, id: BlobId, data: &[u8]) {
        if data.len() > Self::MAX_BLOB || self.blobs.iter().any(|(known, _)| *known == id) {
            return;
        }
        self.bytes += data.len();
        self.blobs.push_back((id, data.into()));
        while self.blobs.len() > Self::MAX_BLOBS || self.bytes > Self::MAX_BYTES {
            match self.blobs.pop_front() {
                Some((_, gone)) => self.bytes -= gone.len(),
                None => break,
            }
        }
    }
}

pub struct Store {
    dir: PathBuf,
    opts: StoreOptions,
    _lock: File,
    packs: Packs,
    oplog: SegmentedLog,
    checkpoints: CheckpointLog,
    intents: IntentLog,
    lanes: KeyedLog<LaneId, Lane>,
    conflicts: KeyedLog<ConflictId, Conflict>,
    index: Index,
    pending: Pending,
    /// The blobs read or written last: a new version of a file is stored as
    /// what it takes to get it from the one before, which is read for it.
    recent: Mutex<Recent>,
    poisoned: bool,
    recovery: RecoveryReport,
    index_dirty: bool,
    last_index_sync: Instant,
}

impl Store {
    /// Creates a new store in the directory `dir` (usually `<repo>/.kito`).
    pub fn init(dir: &Path) -> Result<Store> {
        Store::init_with(dir, StoreOptions::default())
    }

    pub fn init_with(dir: &Path, opts: StoreOptions) -> Result<Store> {
        opts.validate()?;
        fs::create_dir_all(dir).at(dir)?;
        if dir.join(FORMAT_FILE).exists() {
            return Err(StoreError::AlreadyExists(dir.to_owned()));
        }
        for sub in ["objects", "oplog", "index", "lanes"] {
            let path = dir.join(sub);
            fs::create_dir_all(&path).at(&path)?;
        }
        // The `format` file is the last one to be created: a directory
        // without it is an interrupted initialization, which can be repeated.
        let format = dir.join(FORMAT_FILE);
        atomic_write(&format, FORMAT.as_bytes()).at(&format)?;
        Store::open_with(dir, opts)
    }

    /// Opens an existing store and runs recovery: discards the torn final
    /// record, if there is one, and catches the indexes up with the logs.
    pub fn open(dir: &Path) -> Result<Store> {
        Store::open_with(dir, StoreOptions::default())
    }

    pub fn open_with(dir: &Path, opts: StoreOptions) -> Result<Store> {
        opts.validate()?;
        match fs::read_to_string(dir.join(FORMAT_FILE)) {
            Ok(found) if found == FORMAT => {}
            Ok(found) if found == FORMAT_WITHOUT_DELTAS => {
                let format = dir.join(FORMAT_FILE);
                atomic_write(&format, FORMAT.as_bytes()).at(&format)?;
            }
            Ok(found) => return Err(StoreError::UnsupportedFormat(found.trim().to_owned())),
            Err(_) => return Err(StoreError::NotAStore(dir.to_owned())),
        }

        let lock_path = dir.join(LOCK_FILE);
        let lock =
            OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).at(&lock_path)?;
        // The lock belongs to the open file, and a process forked by any
        // thread of this one shares it until it executes its program: a
        // store closed and opened again right away can find its own lock
        // still held for a moment. It is asked again for a short while
        // before the store is said to be in use (D-132).
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::WouldBlock) => return Err(StoreError::Locked(dir.to_owned())),
                Err(TryLockError::Error(e)) => return Err(e).at(&lock_path),
            }
        }

        remove_stray_tmp(dir).at(dir)?;
        let lost_found = dir.join(LOST_FOUND);

        // The index is opened first: its watermark says how far the logs
        // were already confirmed, and the recovery of the logs uses it as the
        // floor below which it discards nothing (except in repair mode).
        let index_dir = dir.join("index");
        fs::create_dir_all(&index_dir).at(&index_dir)?;
        let index = Index::open(&index_dir.join("index.redb"))?;
        let floor = if opts.repair { None } else { index.watermark()? };

        let (mut packs, packs_recovery) = Packs::open(
            &dir.join("objects"),
            opts.pack_size,
            opts.zstd_level,
            opts.fs.clone(),
            &lost_found,
            floor.map(|w| w.packs_end),
        )?;
        let oplog_opts =
            LogOptions { kind: LogKind::Oplog, ext: "seg", target_size: opts.segment_size, auto_flush: None };
        let (mut oplog, oplog_recovery) = SegmentedLog::open(
            &dir.join("oplog"),
            oplog_opts,
            opts.fs.clone(),
            &lost_found,
            floor.map(|w| w.oplog_end),
        )?;
        let (checkpoints, checkpoints_recovery) = CheckpointLog::open(
            &dir.join("checkpoints"),
            &*opts.fs,
            &lost_found,
            floor.map(|w| w.checkpoints_end),
        )?;

        // What recovery found in the logs may never have been synced. It is
        // made durable before the index records it as confirmed.
        packs.sync()?;
        oplog.sync()?;
        let (intents, intents_recovery) =
            IntentLog::open(&dir.join("intents"), &*opts.fs, &lost_found, floor.map(|w| w.intents_end))?;
        let (lanes, lanes_recovery) = KeyedLog::open(
            &dir.join("lanes").join("journal"),
            LogKind::Lanes,
            "lane",
            |lane: &Lane| lane.id,
            &*opts.fs,
            &lost_found,
            floor.map(|w| w.lanes_end),
        )?;
        let (conflicts, conflicts_recovery) = KeyedLog::open(
            &dir.join("conflicts"),
            LogKind::Conflicts,
            "conflict",
            |conflict: &Conflict| conflict.id,
            &*opts.fs,
            &lost_found,
            floor.map(|w| w.conflicts_end),
        )?;
        let ends = SmallLogs {
            checkpoints: checkpoints.len(),
            intents: intents.len(),
            lanes: lanes.len(),
            conflicts: conflicts.len(),
        };
        let (index, index_recovery) = reconcile_index(index, &oplog, &packs, ends)?;

        Ok(Store {
            dir: dir.to_owned(),
            opts,
            _lock: lock,
            packs,
            oplog,
            checkpoints,
            intents,
            lanes,
            conflicts,
            index,
            pending: Pending::default(),
            recent: Mutex::new(Recent::default()),
            poisoned: false,
            recovery: RecoveryReport {
                oplog: oplog_recovery,
                packs: packs_recovery,
                checkpoints: checkpoints_recovery,
                intents: intents_recovery,
                lanes: lanes_recovery,
                conflicts: conflicts_recovery,
                index: index_recovery,
            },
            index_dirty: false,
            last_index_sync: Instant::now(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What recovery did on open.
    pub fn recovery(&self) -> &RecoveryReport {
        &self.recovery
    }

    /// `true` after a write error: the store accepts only reads and must be
    /// reopened.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    fn check(&self) -> Result<()> {
        if self.poisoned {
            Err(StoreError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// After an I/O error the state of the files is uncertain: writing stops.
    fn guard<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(result, Err(StoreError::Io { .. } | StoreError::Poisoned | StoreError::Index(_))) {
            self.poisoned = true;
        }
        result
    }

    // ---------------------------------------------------------------- blob

    /// Appends a blob and returns its ID (BLAKE3 of the content). A blob that
    /// is already present is not rewritten.
    pub fn put_blob(&mut self, data: &[u8]) -> Result<BlobId> {
        self.check()?;
        let id = BlobId::of(data);
        if self.has_blob(&id)? {
            return Ok(id);
        }
        let appended = self.packs.append(&id, data);
        let pos = self.guard(appended)?;
        self.pending.blobs.insert(id, pos);
        self.pending.blob_list.push((id, pos));
        Ok(id)
    }

    fn blob_pos(&self, id: &BlobId) -> Result<Option<Pos>> {
        match self.pending.blobs.get(id) {
            Some(pos) => Ok(Some(*pos)),
            None => self.index.blob(id),
        }
    }

    pub fn has_blob(&self, id: &BlobId) -> Result<bool> {
        Ok(self.blob_pos(id)?.is_some())
    }

    /// Appends a blob that is a new version of `like` (a file saved again):
    /// if `like` is in the store, the new content may be stored as what it
    /// takes to get it from there, which for a small change to a large file
    /// is a small part of it. Reading it gives the same bytes either way.
    pub fn put_blob_like(&mut self, data: &[u8], like: &BlobId) -> Result<BlobId> {
        self.check()?;
        let id = BlobId::of(data);
        if self.has_blob(&id)? {
            return Ok(id);
        }
        let base = match self.blob_pos(like)? {
            Some(pos) => Some((self.packs.depth(pos)?, self.read_at(pos, like)?)),
            None => None,
        };
        let appended = match &base {
            Some((depth, content)) => self.packs.append_like(&id, data, like, content, *depth),
            None => self.packs.append(&id, data),
        };
        let pos = self.guard(appended)?;
        self.pending.blobs.insert(id, pos);
        self.pending.blob_list.push((id, pos));
        self.recent.lock().unwrap_or_else(|e| e.into_inner()).put(id, data);
        Ok(id)
    }

    /// Content of a blob, verified against its hash.
    pub fn get_blob(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        match self.blob_pos(id)? {
            Some(pos) => self.read_at(pos, id).map(Some),
            None => Ok(None),
        }
    }

    /// Reads the blob at `pos`: whole, or from the blob it is a delta of.
    fn read_at(&self, pos: Pos, id: &BlobId) -> Result<Vec<u8>> {
        if let Some(hit) = self.recent.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            return Ok(hit);
        }
        let data = match self.packs.read(pos, id)? {
            Stored::Whole(data) => data,
            Stored::Delta { base, body, raw_len } => {
                let from = self.blob_pos(&base)?.ok_or_else(|| {
                    StoreError::corrupt(
                        Path::new("objects"),
                        pos.to_u64(),
                        format!("blob {id} is stored as a delta of {base}, which is not in the store"),
                    )
                })?;
                // A chain is at most `MAX_DELTA_DEPTH` long.
                let base_data = self.read_at(from, &base)?;
                let data = self.packs.apply_delta(pos, id, &body, raw_len, &base_data)?;
                self.recent.lock().unwrap_or_else(|e| e.into_inner()).put(*id, &data);
                data
            }
        };
        Ok(data)
    }

    // ------------------------------------------------------------------ op

    /// Appends an operation. Returns `false` if it was already in the log.
    ///
    /// The op is rejected if its ID does not match its content, or if a
    /// parent or a blob it refers to is not in the store: the log is always
    /// a topological ordering of the DAG.
    pub fn append_op(&mut self, op: &Op) -> Result<bool> {
        self.check()?;
        if !op.verify_id() {
            return Err(StoreError::InvalidOp(format!(
                "the ID {} does not match the content of the op",
                op.id
            )));
        }
        if self.has_op(&op.id)? {
            return Ok(false);
        }
        for parent in &op.parents {
            if !self.has_op(parent)? {
                return Err(StoreError::InvalidOp(format!("op {}: unknown parent {parent}", op.id)));
            }
        }
        for blob in op.kind.blobs() {
            if !self.has_blob(&blob)? {
                return Err(StoreError::InvalidOp(format!("op {}: unknown blob {blob}", op.id)));
            }
        }
        if let Some(intent) = &op.intent {
            if self.intents.get(intent).is_none() {
                return Err(StoreError::InvalidOp(format!("op {}: unknown intent {intent}", op.id)));
            }
        }
        let bytes = codec::to_vec(op)?;
        // The oplog buffer reaches the file only inside `commit`, after the
        // packs are durable. If appending this op would close the segment
        // (and therefore write it), or if the buffer is full, confirm first.
        if self.oplog.needs_roll(bytes.len()) || self.oplog.buffered() + bytes.len() > self.opts.max_buffered
        {
            self.commit()?;
        }
        let appended = self.oplog.append(&[&bytes]);
        let pos = self.guard(appended)?;
        self.pending.ops.insert(op.id, pos);
        self.pending.op_list.push(IndexedOp { id: op.id, pos, lane: op.lane, symbol: op.kind.symbol() });
        Ok(true)
    }

    /// Makes durable everything that has been appended. When it returns
    /// `Ok`, the ops and blobs appended so far are confirmed.
    pub fn commit(&mut self) -> Result<()> {
        self.check()?;
        let result = self.commit_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn commit_inner(&mut self) -> Result<()> {
        // Mandatory order: first the blobs, then the ops that cite them. That
        // way an op present on disk never cites a missing blob.
        self.packs.sync()?;
        self.oplog.sync()?;
        // From here on the ops are confirmed; the index is derived data.
        let due = self.last_index_sync.elapsed() >= self.opts.index_sync_interval;
        if self.pending.op_list.is_empty() && self.pending.blob_list.is_empty() {
            if due && self.index_dirty {
                self.sync_index()?;
            }
            return Ok(());
        }
        let batch = IndexBatch {
            ops: std::mem::take(&mut self.pending.op_list),
            blobs: std::mem::take(&mut self.pending.blob_list),
        };
        let watermark = self.watermark(batch.ops.last().map(|op| op.pos))?;
        self.index.apply(&batch, watermark, due)?;
        self.pending.ops.clear();
        self.pending.blobs.clear();
        if due {
            self.index_dirty = false;
            self.last_index_sync = Instant::now();
        } else {
            self.index_dirty = true;
        }
        Ok(())
    }

    fn watermark(&self, last_op: Option<Pos>) -> Result<Watermark> {
        let tail = match last_op {
            Some(pos) => Some((pos, self.oplog.read_with(pos, |_, crc| crc)?)),
            None => self.index.watermark()?.and_then(|w| w.tail),
        };
        Ok(Watermark {
            oplog_end: self.oplog.end(),
            packs_end: self.packs.log().end(),
            tail,
            checkpoints_end: self.checkpoints.len(),
            intents_end: self.intents.len(),
            lanes_end: self.lanes.len(),
            conflicts_end: self.conflicts.len(),
        })
    }

    fn sync_index(&mut self) -> Result<()> {
        self.index.sync()?;
        self.index_dirty = false;
        self.last_index_sync = Instant::now();
        Ok(())
    }

    /// Confirms everything and closes the store leaving the index durable, so
    /// that the next open does not have to reread the logs.
    pub fn close(mut self) -> Result<()> {
        self.commit()?;
        self.sync_index()
    }

    fn op_pos(&self, id: &OpId) -> Result<Option<Pos>> {
        match self.pending.ops.get(id) {
            Some(pos) => Ok(Some(*pos)),
            None => Ok(self.index.op(id)?.map(|(pos, _)| pos)),
        }
    }

    pub fn has_op(&self, id: &OpId) -> Result<bool> {
        Ok(self.op_pos(id)?.is_some())
    }

    pub fn get_op(&self, id: &OpId) -> Result<Option<Op>> {
        match self.op_pos(id)? {
            Some(pos) => self.read_op(pos).map(Some),
            None => Ok(None),
        }
    }

    fn read_op(&self, pos: Pos) -> Result<Op> {
        self.oplog
            .read_with(pos, |payload, _| codec::from_slice::<Op>(payload))?
            .map_err(|e| StoreError::corrupt(Path::new("oplog"), pos.to_u64(), format!("unreadable op: {e}")))
    }

    /// Total number of ops in the log (including those not yet confirmed).
    pub fn op_count(&self) -> Result<u64> {
        Ok(self.index.op_count()? + self.pending.op_list.len() as u64)
    }

    /// Number of ops recorded in a lane.
    pub fn lane_len(&self, lane: LaneId) -> Result<u64> {
        let indexed = self.index.lane(lane)?.map_or(0, |(n, _)| n);
        Ok(indexed + self.pending.op_list.iter().filter(|op| op.lane == lane).count() as u64)
    }

    /// Last op recorded in a lane.
    pub fn lane_head(&self, lane: LaneId) -> Result<Option<OpId>> {
        if let Some(op) = self.pending.op_list.iter().rev().find(|op| op.lane == lane) {
            return Ok(Some(op.id));
        }
        Ok(self.index.lane(lane)?.map(|(_, head)| head))
    }

    /// The lanes that have at least one op in the log.
    pub fn lanes(&self) -> Result<Vec<LaneSummary>> {
        let mut lanes: Vec<LaneSummary> = self
            .index
            .lanes()?
            .into_iter()
            .map(|(lane, ops, head)| LaneSummary { lane, ops, head })
            .collect();
        for op in &self.pending.op_list {
            match lanes.iter_mut().find(|l| l.lane == op.lane) {
                Some(l) => {
                    l.ops += 1;
                    l.head = op.id;
                }
                None => lanes.push(LaneSummary { lane: op.lane, ops: 1, head: op.id }),
            }
        }
        lanes.sort_by_key(|l| l.lane);
        Ok(lanes)
    }

    /// Ops of a lane in recording order, from the `start`-th, at most `limit`.
    pub fn lane_ops(&self, lane: LaneId, start: u64, limit: usize) -> Result<Vec<Op>> {
        let indexed = self.index.lane(lane)?.map_or(0, |(n, _)| n);
        let mut positions =
            if start < indexed { self.index.lane_positions(lane, start, limit)? } else { Vec::new() };
        let skip = start.saturating_sub(indexed) as usize;
        let room = limit - positions.len();
        positions.extend(
            self.pending.op_list.iter().filter(|op| op.lane == lane).skip(skip).take(room).map(|op| op.pos),
        );
        positions.into_iter().map(|pos| self.read_op(pos)).collect()
    }

    /// Ops that touch a symbol, in log order.
    pub fn symbol_ops(&self, sym: SymbolId) -> Result<Vec<Op>> {
        let mut positions = self.index.symbol_positions(sym)?;
        positions.extend(self.pending.op_list.iter().filter(|op| op.symbol == Some(sym)).map(|op| op.pos));
        positions.into_iter().map(|pos| self.read_op(pos)).collect()
    }

    /// Visits all the ops in log order.
    pub fn scan_ops(&self, mut f: impl FnMut(Pos, Op) -> Result<()>) -> Result<()> {
        self.oplog.scan(self.oplog.start(), |pos, payload, _| {
            let op = codec::from_slice::<Op>(payload).map_err(|e| {
                StoreError::corrupt(Path::new("oplog"), pos.to_u64(), format!("unreadable op: {e}"))
            })?;
            f(pos, op)
        })
    }

    // ---------------------------------------------------------- checkpoint

    /// Records (or updates) a checkpoint durably. It first confirms everything
    /// that has been appended, so that the checkpoint points to a durable op.
    pub fn put_checkpoint(&mut self, ck: &Checkpoint) -> Result<()> {
        self.check()?;
        if !self.has_op(&ck.head)? {
            return Err(StoreError::InvalidOp(format!("{}: unknown op {}", ck.id, ck.head)));
        }
        self.commit()?;
        let put = self.checkpoints.put(ck);
        self.guard(put)?;
        // The checkpoint is durable; the index takes note of it as a floor.
        let noted = self.watermark(None).and_then(|wm| self.index.apply(&IndexBatch::default(), wm, false));
        self.guard(noted)?;
        self.index_dirty = true;
        Ok(())
    }

    pub fn checkpoint(&self, id: CheckpointId) -> Option<&Checkpoint> {
        self.checkpoints.get(id)
    }

    /// All the checkpoints, in ID order.
    pub fn checkpoints(&self) -> impl DoubleEndedIterator<Item = &Checkpoint> {
        self.checkpoints.iter()
    }

    /// ID to assign to the next checkpoint.
    pub fn next_checkpoint_id(&self) -> CheckpointId {
        self.checkpoints.next_id()
    }

    // -------------------------------------------------------------- intent

    /// Records an intent durably: later ops can cite it.
    pub fn put_intent(&mut self, intent: &Intent) -> Result<()> {
        self.check()?;
        if !intent.verify_id() {
            return Err(StoreError::InvalidOp(format!(
                "the ID {} does not match the content of the intent",
                intent.id
            )));
        }
        let put = self.intents.put(intent);
        self.guard(put)
    }

    pub fn intent(&self, id: &IntentId) -> Option<&Intent> {
        self.intents.get(id)
    }

    // ---------------------------------------------------- lanes, conflicts

    /// Records (or updates) a lane durably. It first confirms everything
    /// that has been appended, so that the lane points to durable ops.
    pub fn put_lane(&mut self, lane: &Lane) -> Result<()> {
        self.check()?;
        for op in [lane.head, lane.base_head] {
            if !self.has_op(&op)? {
                return Err(StoreError::InvalidOp(format!("{}: unknown op {op}", lane.id)));
            }
        }
        if self.checkpoints.get(lane.base).is_none() {
            return Err(StoreError::InvalidOp(format!("{}: unknown checkpoint {}", lane.id, lane.base)));
        }
        self.commit()?;
        let put = self.lanes.put(lane);
        self.guard(put)?;
        self.note_small_logs()
    }

    /// What was last recorded about a lane (the main lane has no record).
    pub fn lane_record(&self, id: LaneId) -> Option<&Lane> {
        self.lanes.get(id)
    }

    /// The recorded lanes, in ID order, closed ones included.
    pub fn lane_records(&self) -> impl DoubleEndedIterator<Item = &Lane> {
        self.lanes.iter()
    }

    /// ID to assign to the next lane. IDs are never reused.
    pub fn next_lane_id(&self) -> LaneId {
        LaneId(self.lanes.last_key().map_or(1, |id| id.0 + 1))
    }

    /// Records (or updates) a conflict durably.
    pub fn put_conflict(&mut self, conflict: &Conflict) -> Result<()> {
        self.check()?;
        self.commit()?;
        let put = self.conflicts.put(conflict);
        self.guard(put)?;
        self.note_small_logs()
    }

    pub fn conflict(&self, id: ConflictId) -> Option<&Conflict> {
        self.conflicts.get(id)
    }

    /// All the conflicts ever recorded, in ID order, resolved ones included.
    pub fn conflicts(&self) -> impl DoubleEndedIterator<Item = &Conflict> {
        self.conflicts.iter()
    }

    pub fn next_conflict_id(&self) -> ConflictId {
        ConflictId(self.conflicts.last_key().map_or(1, |id| id.0 + 1))
    }

    /// The index takes note of the confirmed length of the logs it does not
    /// index, as a floor for the next open.
    fn note_small_logs(&mut self) -> Result<()> {
        let noted = self.watermark(None).and_then(|wm| self.index.apply(&IndexBatch::default(), wm, false));
        self.guard(noted)?;
        self.index_dirty = true;
        Ok(())
    }

    // -------------------------------------------------------- maintenance

    pub fn stats(&self) -> Result<StoreStats> {
        Ok(StoreStats {
            ops: self.op_count()?,
            blobs: self.index.blob_count()? + self.pending.blob_list.len() as u64,
            oplog_segments: self.oplog.segment_count(),
            packs: self.packs.log().segment_count(),
            checkpoints: self.checkpoints.iter().count(),
        })
    }

    /// Discards the indexes and rebuilds them from `objects/` and `oplog/`.
    pub fn rebuild_index(&mut self) -> Result<()> {
        self.commit()?;
        let path = self.dir.join("index").join("index.redb");
        // The index must be closed before it can be recreated: it is replaced
        // for a moment with a temporary one.
        let placeholder_path = self.dir.join("index").join("index.redb.rebuild");
        let placeholder = Index::open(&placeholder_path)?;
        let old = std::mem::replace(&mut self.index, placeholder);
        let rebuilt = old.recreate().and_then(|index| {
            let start = Watermark {
                oplog_end: self.oplog.start(),
                packs_end: self.packs.log().start(),
                tail: None,
                checkpoints_end: self.checkpoints.len(),
                intents_end: self.intents.len(),
                lanes_end: self.lanes.len(),
                conflicts_end: self.conflicts.len(),
            };
            catch_up(&index, &self.oplog, &self.packs, start)?;
            Ok(index)
        });
        match rebuilt {
            Ok(index) => {
                self.index = index;
                let _ = fs::remove_file(&placeholder_path);
                self.index_dirty = false;
                self.last_index_sync = Instant::now();
                Ok(())
            }
            Err(e) => {
                // The store stays unusable until it is reopened: opening
                // will rebuild the index from `path`.
                self.poisoned = true;
                let _ = path;
                Err(e)
            }
        }
    }

    /// Full integrity check (`fsck`): rereads all the records, verifies
    /// CRCs, blob hashes, IDs and parents of the ops, and that the indexes
    /// match the logs. It modifies nothing.
    pub fn verify(&self) -> Result<VerifyReport> {
        let mut report = VerifyReport { checkpoints: self.checkpoints.iter().count(), ..Default::default() };
        let mut problems = Vec::new();

        let mut blobs: HashSet<BlobId> = HashSet::new();
        let base = |id: &BlobId| {
            self.get_blob(id)?.ok_or_else(|| {
                StoreError::corrupt(Path::new("objects"), 0, format!("the base {id} of a delta is missing"))
            })
        };
        let scan = self.packs.scan_verified(self.packs.log().start(), &base, |pos, id, _| {
            if blobs.insert(id) {
                match self.blob_pos(&id) {
                    Ok(Some(found)) if found == pos => {}
                    Ok(found) => problems
                        .push(format!("blob {id}: in the packs at {pos:?}, in the index at {found:?}")),
                    Err(e) => problems.push(format!("blob {id}: unreadable index: {e}")),
                }
            }
            Ok(())
        });
        if let Err(e) = scan {
            problems.push(format!("pack: {e}"));
        }
        report.blobs = blobs.len() as u64;

        let mut ops: HashSet<OpId> = HashSet::new();
        let mut per_lane: HashMap<LaneId, (u64, OpId)> = HashMap::new();
        let scan = self.scan_ops(|pos, op| {
            if !op.verify_id() {
                problems.push(format!("op at {pos:?}: the ID {} does not match the content", op.id));
            }
            for parent in &op.parents {
                if !ops.contains(parent) {
                    problems
                        .push(format!("op {}: the parent {parent} does not precede it in the log", op.id));
                }
            }
            for blob in op.kind.blobs() {
                if !blobs.contains(&blob) {
                    problems.push(format!("op {}: blob {blob} missing from the packs", op.id));
                }
            }
            if let Some(intent) = &op.intent {
                if self.intents.get(intent).is_none() {
                    problems.push(format!("op {}: intent {intent} missing", op.id));
                }
            }
            if !ops.insert(op.id) {
                problems.push(format!("op {}: duplicated in the log at {pos:?}", op.id));
                return Ok(());
            }
            let entry = per_lane.entry(op.lane).or_insert((0, op.id));
            entry.0 += 1;
            entry.1 = op.id;
            match self.op_pos(&op.id) {
                Ok(Some(found)) if found == pos => {}
                Ok(found) => {
                    problems.push(format!("op {}: in the log at {pos:?}, in the index at {found:?}", op.id))
                }
                Err(e) => problems.push(format!("op {}: unreadable index: {e}", op.id)),
            }
            Ok(())
        });
        if let Err(e) = scan {
            problems.push(format!("oplog: {e}"));
        }
        report.ops = ops.len() as u64;
        report.lanes = per_lane.len();

        match self.op_count() {
            Ok(n) if n == report.ops => {}
            Ok(n) => problems.push(format!("the index counts {n} ops, the log contains {}", report.ops)),
            Err(e) => problems.push(format!("unreadable index: {e}")),
        }
        match self.lanes() {
            Ok(lanes) => {
                if lanes.len() != per_lane.len() {
                    problems.push(format!(
                        "the index counts {} lanes, the log {}",
                        lanes.len(),
                        per_lane.len()
                    ));
                }
                for l in lanes {
                    match per_lane.get(&l.lane) {
                        Some(&(n, head)) if n == l.ops && head == l.head => {}
                        other => {
                            problems.push(format!("{}: index {:?}, log {other:?}", l.lane, (l.ops, l.head)))
                        }
                    }
                }
            }
            Err(e) => problems.push(format!("unreadable index: {e}")),
        }
        for ck in self.checkpoints.iter() {
            if !ops.contains(&ck.head) {
                problems.push(format!("{}: points to the op {}, which is not in the log", ck.id, ck.head));
            }
        }

        report.problems = problems;
        Ok(report)
    }
}

/// Lengths of the logs that are not indexed: the index only records how
/// much of each is confirmed.
#[derive(Clone, Copy)]
struct SmallLogs {
    checkpoints: u64,
    intents: u64,
    lanes: u64,
    conflicts: u64,
}

/// Catches the index up with the logs, or rebuilds it if it is not consistent.
fn reconcile_index(
    index: Index,
    oplog: &SegmentedLog,
    packs: &Packs,
    ends: SmallLogs,
) -> Result<(Index, IndexRecovery)> {
    let start = Watermark {
        oplog_end: oplog.start(),
        packs_end: packs.log().start(),
        tail: None,
        checkpoints_end: ends.checkpoints,
        intents_end: ends.intents,
        lanes_end: ends.lanes,
        conflicts_end: ends.conflicts,
    };
    let logs_end = (oplog.end(), packs.log().end());

    let (from, stale) = match index.watermark()? {
        None => (start, Some("index missing or of another schema")),
        Some(wm) if !oplog.contains(wm.oplog_end) => (start, Some("the index is ahead of the oplog")),
        Some(wm) if !packs.log().contains(wm.packs_end) => (start, Some("the index is ahead of the packs")),
        Some(wm) => match wm.tail {
            Some((pos, crc)) if !matches!(oplog.read_with(pos, |_, c| c), Ok(found) if found == crc) => {
                (start, Some("the last indexed op is no longer in the log"))
            }
            _ => (
                Watermark {
                    checkpoints_end: ends.checkpoints,
                    intents_end: ends.intents,
                    lanes_end: ends.lanes,
                    conflicts_end: ends.conflicts,
                    ..wm
                },
                None,
            ),
        },
    };

    match stale {
        None if (from.oplog_end, from.packs_end) == logs_end => Ok((index, IndexRecovery::UpToDate)),
        None => {
            let (ops, blobs) = catch_up(&index, oplog, packs, from)?;
            Ok((index, IndexRecovery::CaughtUp { ops, blobs }))
        }
        Some(reason) => {
            let index = index.recreate()?;
            let (ops, blobs) = catch_up(&index, oplog, packs, start)?;
            // A newly created store has nothing to "rebuild".
            if ops == 0 && blobs == 0 {
                Ok((index, IndexRecovery::UpToDate))
            } else {
                Ok((index, IndexRecovery::Rebuilt { reason: reason.to_owned(), ops, blobs }))
            }
        }
    }
}

/// Indexes the records of the logs from `from` onwards; the last transaction
/// is durable. Returns how many ops and how many blobs it indexed.
fn catch_up(index: &Index, oplog: &SegmentedLog, packs: &Packs, from: Watermark) -> Result<(u64, u64)> {
    let mut wm = from;
    let mut batch = IndexBatch::default();

    let mut blobs = 0u64;
    packs.scan_ids(from.packs_end, |pos, id, payload_len| {
        batch.blobs.push((id, pos));
        blobs += 1;
        wm.packs_end = pos_after(pos, payload_len);
        if batch.blobs.len() >= INDEX_BATCH {
            index.apply(&batch, wm, false)?;
            batch.blobs.clear();
        }
        Ok(())
    })?;
    wm.packs_end = packs.log().end();

    let mut ops = 0u64;
    oplog.scan(from.oplog_end, |pos, payload, crc| {
        let op: Op = codec::from_slice(payload).map_err(|e| {
            StoreError::corrupt(Path::new("oplog"), pos.to_u64(), format!("unreadable op: {e}"))
        })?;
        batch.ops.push(IndexedOp { id: op.id, pos, lane: op.lane, symbol: op.kind.symbol() });
        ops += 1;
        wm.oplog_end = pos_after(pos, payload.len());
        wm.tail = Some((pos, crc));
        if batch.ops.len() >= INDEX_BATCH {
            index.apply(&batch, wm, false)?;
            batch.ops.clear();
            batch.blobs.clear();
        }
        Ok(())
    })?;
    wm.oplog_end = oplog.end();

    index.apply(&batch, wm, true)?;
    Ok((ops, blobs))
}
