//! Content-addressed blobs (BLAKE3), compressed with zstd, in append-only
//! packs.
//!
//! Payload of a record: `hash[32] codec:u8 raw_len:u32le data`.
//!
//! A blob that is a new version of another one (a file saved again) can be
//! stored as what it takes to get it from the other: `data` is then
//! `base[32] depth:u8` followed by the content compressed with the base as
//! dictionary. A save of a large file that changes a line costs a few
//! hundred bytes instead of the whole file compressed. Such chains are kept
//! short: `depth` counts the blobs to read before this one, and past
//! [`MAX_DELTA_DEPTH`] a blob is stored whole again.

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
/// The content compressed with another blob as dictionary.
const CODEC_ZSTD_DELTA: u8 = 2;
const BLOB_HEADER: usize = 32 + 1 + 4;
/// In a delta, before the compressed content: the base and the depth.
const DELTA_HEADER: usize = 32 + 1;
/// The longest chain of blobs to read to get one.
pub(crate) const MAX_DELTA_DEPTH: u8 = 24;
/// Blobs smaller or larger than these are stored whole: nothing to gain
/// below, and above the base would not fit in what the compressor looks at.
const MIN_DELTA: usize = 256;
const MAX_DELTA: usize = 4 << 20;
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
    level: i32,
}

/// A blob as a record holds it.
pub(crate) enum Stored {
    /// The content, already verified against its hash.
    Whole(Vec<u8>),
    /// What it takes to get the content from another blob.
    Delta { base: BlobId, body: Vec<u8>, raw_len: usize },
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
        Ok((Packs { log, compressor, level }, recovery))
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

    /// Appends a blob that resembles another one already in the packs: as a
    /// delta from it if that is smaller, whole otherwise. `base_depth` is
    /// the depth of the base ([`Packs::depth`]).
    pub(crate) fn append_like(
        &mut self,
        id: &BlobId,
        data: &[u8],
        base: &BlobId,
        base_data: &[u8],
        base_depth: u8,
    ) -> Result<Pos> {
        let fits = |len: usize| (MIN_DELTA..=MAX_DELTA).contains(&len);
        if !fits(data.len()) || !fits(base_data.len()) || base_depth >= MAX_DELTA_DEPTH {
            return self.append(id, data);
        }
        debug_assert_eq!(*id, BlobId::of(data));
        let failed = |e: std::io::Error| StoreError::InvalidOp(format!("zstd compression failed: {e}"));
        let whole = self.compressor.compress(data).map_err(failed)?;
        let delta = zstd::bulk::Compressor::with_dictionary(self.level, base_data)
            .and_then(|mut compressor| compressor.compress(data))
            .map_err(failed)?;
        let mut header = [0u8; BLOB_HEADER];
        header[..32].copy_from_slice(id.as_bytes());
        header[33..].copy_from_slice(&(data.len() as u32).to_le_bytes());
        if delta.len() + DELTA_HEADER < whole.len().min(data.len()) {
            header[32] = CODEC_ZSTD_DELTA;
            return self.log.append(&[&header, base.as_bytes(), &[base_depth + 1], &delta]);
        }
        header[32] = if whole.len() < data.len() { CODEC_ZSTD } else { CODEC_RAW };
        self.log.append(&[&header, if whole.len() < data.len() { &whole } else { data }])
    }

    /// How many other blobs must be read to get the one at `pos`: 0 for a
    /// blob stored whole.
    pub(crate) fn depth(&self, pos: Pos) -> Result<u8> {
        self.log
            .read_with(pos, |payload, _| match payload.get(32) {
                Some(&CODEC_ZSTD_DELTA) => {
                    payload.get(BLOB_HEADER + 32).copied().ok_or_else(|| "delta record too short".to_owned())
                }
                Some(_) => Ok(0),
                None => Err("blob record too short".to_owned()),
            })?
            .map_err(|reason| self.corrupt(pos, reason))
    }

    /// Reads the record at `pos`, which must be of the blob `expect`. A blob
    /// stored whole comes verified against its hash; for a delta the caller
    /// gets the base and applies it with [`apply_delta`].
    pub(crate) fn read(&self, pos: Pos, expect: &BlobId) -> Result<Stored> {
        self.log
            .read_with(pos, |payload, _| decode(payload, Some(expect)))?
            .map_err(|reason| self.corrupt(pos, reason))
    }

    /// The content of a blob stored as a delta, given its base, verified
    /// against its hash.
    pub(crate) fn apply_delta(
        &self,
        pos: Pos,
        id: &BlobId,
        body: &[u8],
        raw_len: usize,
        base_data: &[u8],
    ) -> Result<Vec<u8>> {
        let data = zstd::bulk::Decompressor::with_dictionary(base_data)
            .and_then(|mut decompressor| decompressor.decompress(body, raw_len))
            .map_err(|e| self.corrupt(pos, format!("zstd decompression failed: {e}")))?;
        check(&data, raw_len, id).map_err(|reason| self.corrupt(pos, reason))?;
        Ok(data)
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

    /// Visits all the blobs from `from`, decompressing them and verifying
    /// their hash. `base` gives the content of the blob a delta starts from.
    pub(crate) fn scan_verified(
        &self,
        from: Pos,
        base: &dyn Fn(&BlobId) -> Result<Vec<u8>>,
        mut f: impl FnMut(Pos, BlobId, usize) -> Result<()>,
    ) -> Result<()> {
        // The records are collected first: reading a base reads the log,
        // which a scan in progress does not allow.
        let mut deltas = Vec::new();
        self.log.scan(from, |pos, payload, _| {
            match decode(payload, None).map_err(|reason| self.corrupt(pos, reason))? {
                Stored::Whole(data) => f(pos, BlobId::of(&data), payload.len()),
                Stored::Delta { base, body, raw_len } => {
                    let id = BlobId::from_bytes(payload[..32].try_into().unwrap());
                    deltas.push((pos, id, base, body, raw_len, payload.len()));
                    Ok(())
                }
            }
        })?;
        for (pos, id, from_blob, body, raw_len, len) in deltas {
            self.apply_delta(pos, &id, &body, raw_len, &base(&from_blob)?)?;
            f(pos, id, len)?;
        }
        Ok(())
    }

    fn corrupt(&self, pos: Pos, reason: String) -> StoreError {
        StoreError::corrupt(Path::new("objects"), pos.to_u64(), reason)
    }
}

/// Decodes the payload of a blob record: against `expect` if given,
/// otherwise against the ID written in the record. The content of a blob
/// stored whole is verified against its hash.
fn decode(payload: &[u8], expect: Option<&BlobId>) -> std::result::Result<Stored, String> {
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
        CODEC_ZSTD_DELTA => {
            if body.len() < DELTA_HEADER || body[32] == 0 || body[32] > MAX_DELTA_DEPTH {
                return Err("malformed delta record".into());
            }
            let base = BlobId::from_bytes(body[..32].try_into().unwrap());
            return Ok(Stored::Delta { base, body: body[DELTA_HEADER..].to_vec(), raw_len });
        }
        other => return Err(format!("unknown codec {other}")),
    };
    check(&data, raw_len, &declared)?;
    Ok(Stored::Whole(data))
}

/// A content has the length its record declares and the hash of its ID.
fn check(data: &[u8], raw_len: usize, id: &BlobId) -> std::result::Result<(), String> {
    if data.len() != raw_len {
        return Err(format!("length {} differs from the declared length {raw_len}", data.len()));
    }
    if Hash::of(data) != *id.hash() {
        return Err(format!("the content does not have the hash {id}"));
    }
    Ok(())
}
