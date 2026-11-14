//! Content-addressed blobs (BLAKE3), compressed with zstd, in append-only
//! packs.
//!
//! Payload of a record: `hash[32] codec:u8 raw_len:u32le data`.

use crate::error::{Result, StoreError};
use crate::fs::FileSystem;
use crate::logfile::LogRecovery;
use crate::record::{LogKind, MAX_PAYLOAD};
use crate::segments::{LogOptions, Pos, SegmentedLog};
use kito_core::{BlobId, Hash};
use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;

const CODEC_RAW: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const BLOB_HEADER: usize = 32 + 1 + 4;
/// Below this size compression does not pay for the zstd header.
const MIN_COMPRESS: usize = 64;
/// Maximum size of a blob.
pub const MAX_BLOB: usize = MAX_PAYLOAD - BLOB_HEADER;
/// Threshold beyond which the pack buffer is written to the file.
const PACK_AUTO_FLUSH: usize = 4 << 20;

pub(crate) struct Packs {
    log: SegmentedLog,
    /// Compression context reused from one blob to the next: creating one on
    /// every call dominated the cost of a small blob (benchmark
    /// `write/put_blob_512_bytes`, see `docs/benchmarks.md`).
    compressor: zstd::bulk::Compressor<'static>,
}

thread_local! {
    /// Decompression context reused by the reads of this thread
    /// (benchmark `read/get_blob`).
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

fn decompress(body: &[u8], raw_len: usize) -> std::io::Result<Vec<u8>> {
    DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let decompressor = match slot.as_mut() {
            Some(d) => d,
            None => slot.insert(zstd::bulk::Decompressor::new()?),
        };
        decompressor.decompress(body, raw_len)
    })
}

impl Packs {
    pub(crate) fn open(
        dir: &Path,
        target_size: u64,
        level: i32,
        fs: Arc<dyn FileSystem>,
        lost_found: &Path,
        floor: Option<Pos>,
    ) -> Result<(Packs, LogRecovery)> {
        let opts =
            LogOptions { kind: LogKind::Pack, ext: "pack", target_size, auto_flush: Some(PACK_AUTO_FLUSH) };
        let (log, recovery) = SegmentedLog::open(dir, opts, fs, lost_found, floor)?;
        let compressor = zstd::bulk::Compressor::new(level)
            .map_err(|e| StoreError::InvalidOp(format!("invalid zstd level {level}: {e}")))?;
        Ok((Packs { log, compressor }, recovery))
    }

    pub(crate) fn log(&self) -> &SegmentedLog {
        &self.log
    }

    pub(crate) fn sync(&mut self) -> Result<()> {
        self.log.sync()
    }

    /// Appends a blob. `id` must be the hash of `data`.
    pub(crate) fn append(&mut self, id: &BlobId, data: &[u8]) -> Result<Pos> {
        if data.len() > MAX_BLOB {
            return Err(StoreError::RecordTooLarge(data.len()));
        }
        debug_assert_eq!(*id, BlobId::of(data));
        let compressed = if data.len() >= MIN_COMPRESS {
            let c = self
                .compressor
                .compress(data)
                .map_err(|e| StoreError::InvalidOp(format!("zstd compression failed: {e}")))?;
            (c.len() < data.len()).then_some(c)
        } else {
            None
        };
        let mut header = [0u8; BLOB_HEADER];
        header[..32].copy_from_slice(id.as_bytes());
        header[32] = if compressed.is_some() { CODEC_ZSTD } else { CODEC_RAW };
        header[33..].copy_from_slice(&(data.len() as u32).to_le_bytes());
        self.log.append(&[&header, compressed.as_deref().unwrap_or(data)])
    }

    /// Reads the blob at `pos`, verifying that the content has the expected hash.
    pub(crate) fn read(&self, pos: Pos, expect: &BlobId) -> Result<Vec<u8>> {
        self.log
            .read_with(pos, |payload, _| decode(payload, Some(expect)))?
            .map_err(|reason| self.corrupt(pos, reason))
    }

    /// Visits all the blobs from `from`: `f` receives the position, the
    /// declared ID and the length of the record's payload.
    pub(crate) fn scan_ids(
        &self,
        from: Pos,
        mut f: impl FnMut(Pos, BlobId, usize) -> Result<()>,
    ) -> Result<()> {
        self.log.scan(from, |pos, payload, _| {
            let id = payload
                .first_chunk::<32>()
                .ok_or_else(|| self.corrupt(pos, "blob record too short".into()))?;
            f(pos, BlobId::from_bytes(*id), payload.len())
        })
    }

    /// Visits all the blobs from `from`, decompressing them and verifying their hash.
    pub(crate) fn scan_verified(
        &self,
        from: Pos,
        mut f: impl FnMut(Pos, BlobId, usize) -> Result<()>,
    ) -> Result<()> {
        self.log.scan(from, |pos, payload, _| {
            let data = decode(payload, None).map_err(|reason| self.corrupt(pos, reason))?;
            f(pos, BlobId::of(&data), payload.len())
        })
    }

    fn corrupt(&self, pos: Pos, reason: String) -> StoreError {
        StoreError::corrupt(Path::new("objects"), pos.to_u64(), reason)
    }
}

/// Decodes the payload of a blob record and verifies its hash: against
/// `expect` if given, otherwise against the ID written in the record.
fn decode(payload: &[u8], expect: Option<&BlobId>) -> std::result::Result<Vec<u8>, String> {
    if payload.len() < BLOB_HEADER {
        return Err("blob record too short".into());
    }
    let declared = BlobId::from_bytes(payload[..32].try_into().unwrap());
    if let Some(expect) = expect {
        if declared != *expect {
            return Err(format!("the record contains blob {declared}, expected {expect}"));
        }
    }
    let raw_len = u32::from_le_bytes(payload[33..37].try_into().unwrap()) as usize;
    let body = &payload[BLOB_HEADER..];
    let data = match payload[32] {
        CODEC_RAW => body.to_vec(),
        CODEC_ZSTD => decompress(body, raw_len).map_err(|e| format!("zstd decompression failed: {e}"))?,
        other => return Err(format!("unknown codec {other}")),
    };
    if data.len() != raw_len {
        return Err(format!("length {} differs from the declared length {raw_len}", data.len()));
    }
    if Hash::of(&data) != *declared.hash() {
        return Err(format!("the content does not have the hash {declared}"));
    }
    Ok(data)
}
