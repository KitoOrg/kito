//! Blob indirizzati per contenuto (BLAKE3), compressi con zstd, in pack
//! append-only.
//!
//! Payload di un record: `hash[32] codec:u8 raw_len:u32le dati`.

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
/// Sotto questa dimensione la compressione non ripaga l'intestazione zstd.
const MIN_COMPRESS: usize = 64;
/// Dimensione massima di un blob.
pub const MAX_BLOB: usize = MAX_PAYLOAD - BLOB_HEADER;
/// Soglia oltre la quale il buffer dei pack viene scritto nel file.
const PACK_AUTO_FLUSH: usize = 4 << 20;

pub(crate) struct Packs {
    log: SegmentedLog,
    /// Contesto di compressione riusato tra un blob e l'altro: crearne uno a
    /// ogni chiamata dominava il costo di un blob piccolo (benchmark
    /// `write/put_blob_512_bytes`, vedi `docs/benchmarks.md`).
    compressor: zstd::bulk::Compressor<'static>,
}

thread_local! {
    /// Contesto di decompressione riusato dalle letture di questo thread
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
            .map_err(|e| StoreError::InvalidOp(format!("livello zstd {level} non valido: {e}")))?;
        Ok((Packs { log, compressor }, recovery))
    }

    pub(crate) fn log(&self) -> &SegmentedLog {
        &self.log
    }

    pub(crate) fn sync(&mut self) -> Result<()> {
        self.log.sync()
    }

    /// Accoda un blob. `id` deve essere l'hash di `data`.
    pub(crate) fn append(&mut self, id: &BlobId, data: &[u8]) -> Result<Pos> {
        if data.len() > MAX_BLOB {
            return Err(StoreError::RecordTooLarge(data.len()));
        }
        debug_assert_eq!(*id, BlobId::of(data));
        let compressed = if data.len() >= MIN_COMPRESS {
            let c = self
                .compressor
                .compress(data)
                .map_err(|e| StoreError::InvalidOp(format!("compressione zstd fallita: {e}")))?;
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

    /// Legge il blob in `pos`, verificando che il contenuto abbia l'hash atteso.
    pub(crate) fn read(&self, pos: Pos, expect: &BlobId) -> Result<Vec<u8>> {
        self.log
            .read_with(pos, |payload, _| decode(payload, Some(expect)))?
            .map_err(|reason| self.corrupt(pos, reason))
    }

    /// Visita tutti i blob da `from`: `f` riceve posizione, ID dichiarato e
    /// lunghezza del payload del record.
    pub(crate) fn scan_ids(
        &self,
        from: Pos,
        mut f: impl FnMut(Pos, BlobId, usize) -> Result<()>,
    ) -> Result<()> {
        self.log.scan(from, |pos, payload, _| {
            let id = payload
                .first_chunk::<32>()
                .ok_or_else(|| self.corrupt(pos, "record di blob troppo corto".into()))?;
            f(pos, BlobId::from_bytes(*id), payload.len())
        })
    }

    /// Visita tutti i blob da `from` decomprimendoli e verificandone l'hash.
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

/// Decodifica il payload di un record di blob e ne verifica l'hash: contro
/// `expect` se dato, altrimenti contro l'ID scritto nel record.
fn decode(payload: &[u8], expect: Option<&BlobId>) -> std::result::Result<Vec<u8>, String> {
    if payload.len() < BLOB_HEADER {
        return Err("record di blob troppo corto".into());
    }
    let declared = BlobId::from_bytes(payload[..32].try_into().unwrap());
    if let Some(expect) = expect {
        if declared != *expect {
            return Err(format!("il record contiene il blob {declared}, atteso {expect}"));
        }
    }
    let raw_len = u32::from_le_bytes(payload[33..37].try_into().unwrap()) as usize;
    let body = &payload[BLOB_HEADER..];
    let data = match payload[32] {
        CODEC_RAW => body.to_vec(),
        CODEC_ZSTD => decompress(body, raw_len).map_err(|e| format!("decompressione zstd fallita: {e}"))?,
        other => return Err(format!("codec {other} sconosciuto")),
    };
    if data.len() != raw_len {
        return Err(format!("lunghezza {} diversa da quella dichiarata {raw_len}", data.len()));
    }
    if Hash::of(&data) != *declared.hash() {
        return Err(format!("il contenuto non ha l'hash {declared}"));
    }
    Ok(data)
}
