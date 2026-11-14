//! Indici ricostruibili (redb): op→posizione e lane, lane→op, simbolo→op,
//! blob→posizione.
//!
//! L'indice è sempre derivabile da `objects/` e `oplog/`: registra fino a che
//! punto dei log è aggiornato (watermark) e all'apertura lo store lo porta in
//! pari, oppure lo ricostruisce da zero se non è coerente con i log.

use crate::error::Result;
use crate::segments::Pos;
use kito_core::{BlobId, LaneId, OpId, SymbolId};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SCHEMA: u64 = 1;

/// op → (posizione nell'oplog, lane)
const OPS: TableDefinition<&[u8; 32], (u64, u64)> = TableDefinition::new("ops");
/// (lane, n-esima op della lane) → posizione nell'oplog
const LANE_OPS: TableDefinition<(u64, u64), u64> = TableDefinition::new("lane_ops");
/// lane → (numero di op, ultima op)
const LANES: TableDefinition<u64, (u64, &[u8; 32])> = TableDefinition::new("lanes");
/// (origine del simbolo, contatore, posizione nell'oplog) → ()
const SYM_OPS: TableDefinition<(u64, u64, u64), ()> = TableDefinition::new("sym_ops");
/// blob → posizione nei pack
const BLOBS: TableDefinition<&[u8; 32], u64> = TableDefinition::new("blobs");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const M_SCHEMA: &str = "schema";
const M_OPLOG_END: &str = "oplog_end";
const M_PACKS_END: &str = "packs_end";
const M_TAIL_POS: &str = "oplog_tail_pos";
const M_TAIL_CRC: &str = "oplog_tail_crc";
const M_CKPT_END: &str = "checkpoints_end";

/// Fin dove l'indice riflette i log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Watermark {
    /// Tutte le op prima di questa posizione sono indicizzate.
    pub oplog_end: Pos,
    /// Tutti i blob prima di questa posizione sono indicizzati.
    pub packs_end: Pos,
    /// Posizione e CRC dell'ultima op indicizzata: all'apertura si controlla
    /// che il log contenga ancora proprio quel record.
    pub tail: Option<(Pos, u32)>,
    /// Lunghezza confermata del log dei checkpoint.
    pub checkpoints_end: u64,
}

pub(crate) struct IndexedOp {
    pub id: OpId,
    pub pos: Pos,
    pub lane: LaneId,
    pub symbol: Option<SymbolId>,
}

#[derive(Default)]
pub(crate) struct IndexBatch {
    /// In ordine di log.
    pub ops: Vec<IndexedOp>,
    pub blobs: Vec<(BlobId, Pos)>,
}

pub(crate) struct Index {
    db: Database,
    path: PathBuf,
}

impl Index {
    /// Apre l'indice, creandolo se manca. Se il file è illeggibile lo scarta
    /// e ne crea uno vuoto: verrà ricostruito dai log.
    pub(crate) fn open(path: &Path) -> Result<Index> {
        let db = match Database::create(path) {
            Ok(db) => db,
            Err(_) => {
                let _ = std::fs::remove_file(path);
                Database::create(path)?
            }
        };
        let index = Index { db, path: path.to_owned() };
        index.create_tables()?;
        Ok(index)
    }

    /// Scarta l'indice e ne crea uno vuoto.
    pub(crate) fn recreate(self) -> Result<Index> {
        let path = self.path.clone();
        drop(self);
        std::fs::remove_file(&path).map_err(|source| crate::StoreError::Io { path: path.clone(), source })?;
        Index::open(&path)
    }

    fn create_tables(&self) -> Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(OPS)?;
        txn.open_table(LANE_OPS)?;
        txn.open_table(LANES)?;
        txn.open_table(SYM_OPS)?;
        txn.open_table(BLOBS)?;
        txn.open_table(META)?;
        txn.commit()?;
        Ok(())
    }

    /// Il watermark registrato, o `None` se l'indice è vuoto o di un altro schema.
    pub(crate) fn watermark(&self) -> Result<Option<Watermark>> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(META)?;
        let get = |key: &str| -> Result<Option<u64>> { Ok(meta.get(key)?.map(|v| v.value())) };
        if get(M_SCHEMA)? != Some(SCHEMA) {
            return Ok(None);
        }
        let (Some(oplog_end), Some(packs_end)) = (get(M_OPLOG_END)?, get(M_PACKS_END)?) else {
            return Ok(None);
        };
        let tail = match (get(M_TAIL_POS)?, get(M_TAIL_CRC)?) {
            (Some(pos), Some(crc)) => Some((Pos::from_u64(pos), crc as u32)),
            _ => None,
        };
        Ok(Some(Watermark {
            oplog_end: Pos::from_u64(oplog_end),
            packs_end: Pos::from_u64(packs_end),
            tail,
            checkpoints_end: get(M_CKPT_END)?.unwrap_or(0),
        }))
    }

    /// Applica un batch e avanza il watermark, in un'unica transazione.
    ///
    /// Con `durable = false` la transazione non fa fsync: dopo un crash
    /// l'indice può tornare a un watermark precedente, e l'apertura lo
    /// riporta in pari rileggendo i log.
    pub(crate) fn apply(&self, batch: &IndexBatch, watermark: Watermark, durable: bool) -> Result<()> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(if durable { Durability::Immediate } else { Durability::None })?;
        {
            let mut ops = txn.open_table(OPS)?;
            let mut lane_ops = txn.open_table(LANE_OPS)?;
            let mut lanes = txn.open_table(LANES)?;
            let mut sym_ops = txn.open_table(SYM_OPS)?;
            let mut counters: HashMap<u64, (u64, [u8; 32])> = HashMap::new();

            for op in &batch.ops {
                let (pos, lane) = (op.pos.to_u64(), op.lane.0);
                // Lo store non scrive mai due volte la stessa op, ma se il log
                // ne contenesse un duplicato vale la prima occorrenza.
                if ops.get(op.id.as_bytes())?.is_some() {
                    continue;
                }
                ops.insert(op.id.as_bytes(), (pos, lane))?;
                let counter = match counters.entry(lane) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let current = lanes.get(lane)?.map(|v| {
                            let (n, head) = v.value();
                            (n, *head)
                        });
                        e.insert(current.unwrap_or((0, [0; 32])))
                    }
                };
                lane_ops.insert((lane, counter.0), pos)?;
                counter.0 += 1;
                counter.1 = *op.id.as_bytes();
                if let Some(sym) = op.symbol {
                    sym_ops.insert((sym.origin().0, sym.seq(), pos), ())?;
                }
            }
            for (lane, (count, head)) in &counters {
                lanes.insert(*lane, (*count, head))?;
            }

            let mut blobs = txn.open_table(BLOBS)?;
            for (id, pos) in &batch.blobs {
                // Vale la prima copia: i blob sono immutabili e identici.
                if blobs.get(id.as_bytes())?.is_none() {
                    blobs.insert(id.as_bytes(), pos.to_u64())?;
                }
            }

            let mut meta = txn.open_table(META)?;
            meta.insert(M_SCHEMA, SCHEMA)?;
            meta.insert(M_OPLOG_END, watermark.oplog_end.to_u64())?;
            meta.insert(M_PACKS_END, watermark.packs_end.to_u64())?;
            meta.insert(M_CKPT_END, watermark.checkpoints_end)?;
            match watermark.tail {
                Some((pos, crc)) => {
                    meta.insert(M_TAIL_POS, pos.to_u64())?;
                    meta.insert(M_TAIL_CRC, crc as u64)?;
                }
                None => {
                    meta.remove(M_TAIL_POS)?;
                    meta.remove(M_TAIL_CRC)?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Rende durevole lo stato attuale dell'indice.
    pub(crate) fn sync(&self) -> Result<()> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        txn.commit()?;
        Ok(())
    }

    pub(crate) fn op(&self, id: &OpId) -> Result<Option<(Pos, LaneId)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(OPS)?;
        Ok(table.get(id.as_bytes())?.map(|v| v.value()).map(|(pos, lane)| (Pos::from_u64(pos), LaneId(lane))))
    }

    pub(crate) fn blob(&self, id: &BlobId) -> Result<Option<Pos>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOBS)?;
        Ok(table.get(id.as_bytes())?.map(|v| Pos::from_u64(v.value())))
    }

    /// Numero di op e ultima op di una lane.
    pub(crate) fn lane(&self, lane: LaneId) -> Result<Option<(u64, OpId)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LANES)?;
        Ok(table.get(lane.0)?.map(|v| {
            let (n, head) = v.value();
            (n, OpId::from_bytes(*head))
        }))
    }

    pub(crate) fn lanes(&self) -> Result<Vec<(LaneId, u64, OpId)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LANES)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (lane, value) = entry?;
            let (count, head) = value.value();
            out.push((LaneId(lane.value()), count, OpId::from_bytes(*head)));
        }
        Ok(out)
    }

    /// Posizioni delle op di una lane dalla `start`-esima, al più `limit`.
    pub(crate) fn lane_positions(&self, lane: LaneId, start: u64, limit: usize) -> Result<Vec<Pos>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LANE_OPS)?;
        let mut out = Vec::new();
        for entry in table.range((lane.0, start)..=(lane.0, u64::MAX))?.take(limit) {
            out.push(Pos::from_u64(entry?.1.value()));
        }
        Ok(out)
    }

    /// Posizioni, in ordine di log, delle op che toccano un simbolo.
    pub(crate) fn symbol_positions(&self, sym: SymbolId) -> Result<Vec<Pos>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SYM_OPS)?;
        let key = |pos: u64| (sym.origin().0, sym.seq(), pos);
        let mut out = Vec::new();
        for entry in table.range(key(0)..=key(u64::MAX))? {
            out.push(Pos::from_u64(entry?.0.value().2));
        }
        Ok(out)
    }

    pub(crate) fn op_count(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(OPS)?.len()?)
    }

    pub(crate) fn blob_count(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(BLOBS)?.len()?)
    }
}
