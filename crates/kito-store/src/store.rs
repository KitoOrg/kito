//! Lo store: pack dei blob, oplog, checkpoint e indici, con un solo scrittore.

use crate::checkpoints::CheckpointLog;
use crate::error::{IoContext, Result, StoreError};
use crate::fs::{atomic_write, remove_stray_tmp, FileSystem, StdFs};
use crate::index::{Index, IndexBatch, IndexedOp, Watermark};
use crate::logfile::LogRecovery;
use crate::objects::Packs;
use crate::record::LogKind;
use crate::segments::{pos_after, LogOptions, Pos, SegmentedLog};
use kito_core::{codec, BlobId, Checkpoint, CheckpointId, LaneId, Op, OpId, SymbolId};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Nome convenzionale della cartella dello store dentro un repository.
pub const STORE_DIR: &str = ".kito";

const FORMAT_FILE: &str = "format";
const FORMAT: &str = "kito-store 1\n";
const LOCK_FILE: &str = "lock";
const LOST_FOUND: &str = "lost+found";
/// Record per transazione quando l'indice viene portato in pari o ricostruito.
const INDEX_BATCH: usize = 20_000;

/// Parametri dello store. I default sono quelli della specifica.
#[derive(Clone, Debug)]
pub struct StoreOptions {
    /// Dimensione dei segmenti dell'oplog (default 64 MiB).
    pub segment_size: u64,
    /// Dimensione dei pack dei blob (default 64 MiB).
    pub pack_size: u64,
    /// Livello di compressione zstd dei blob.
    pub zstd_level: i32,
    /// Byte di op non confermate oltre i quali `append_op` fa un commit da sé.
    pub max_buffered: usize,
    /// Ogni quanto l'indice viene reso durevole. Tra un fsync e l'altro i
    /// commit dell'indice restano in memoria: dopo un crash si recuperano
    /// rileggendo la coda dei log.
    pub index_sync_interval: Duration,
    /// Modalità riparazione. Normalmente l'apertura si rifiuta di scartare
    /// dati che risultavano già confermati (file troncati o danneggiati
    /// dall'esterno) e restituisce [`StoreError::Corrupt`]. Con `repair` li
    /// scarta comunque, conserva ciò che precede il danno e ricostruisce gli
    /// indici: è una perdita di dati accettata esplicitamente.
    pub repair: bool,
    /// File system usato per le scritture (sostituibile nei test).
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
                    "{name} = {size} fuori dall'intervallo {MIN}..={MAX}"
                )));
            }
        }
        Ok(())
    }
}

/// Cosa ha fatto l'apertura per riportare l'indice in pari con i log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexRecovery {
    /// L'indice rifletteva già tutti i log.
    UpToDate,
    /// L'indice era indietro: è stata riletta la coda dei log.
    CaughtUp { ops: u64, blobs: u64 },
    /// L'indice mancava o non era coerente con i log: ricostruito da zero.
    Rebuilt { reason: String, ops: u64, blobs: u64 },
}

/// Resoconto del recupero eseguito da [`Store::open`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub oplog: LogRecovery,
    pub packs: LogRecovery,
    pub checkpoints: LogRecovery,
    pub index: IndexRecovery,
}

impl RecoveryReport {
    /// Byte di coda scartati in totale (record mai confermati).
    pub fn discarded_bytes(&self) -> u64 {
        self.oplog.discarded_bytes + self.packs.discarded_bytes + self.checkpoints.discarded_bytes
    }
}

/// Stato di una lane secondo il log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneSummary {
    pub lane: LaneId,
    /// Numero di op registrate nella lane.
    pub ops: u64,
    /// Ultima op registrata nella lane.
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

/// Esito di [`Store::verify`].
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

/// Op e blob accodati ma non ancora confermati né indicizzati.
#[derive(Default)]
struct Pending {
    ops: HashMap<OpId, Pos>,
    op_list: Vec<IndexedOp>,
    blobs: HashMap<BlobId, Pos>,
    blob_list: Vec<(BlobId, Pos)>,
}

/// Lo store di un repository. Un solo processo alla volta può aprirlo.
///
/// Le scritture ([`Store::put_blob`], [`Store::append_op`]) vengono accodate;
/// [`Store::commit`] le rende durevoli. Un'op è *confermata* quando il
/// `commit` che la segue ha restituito `Ok`.
pub struct Store {
    dir: PathBuf,
    opts: StoreOptions,
    _lock: File,
    packs: Packs,
    oplog: SegmentedLog,
    checkpoints: CheckpointLog,
    index: Index,
    pending: Pending,
    poisoned: bool,
    recovery: RecoveryReport,
    index_dirty: bool,
    last_index_sync: Instant,
}

impl Store {
    /// Crea un nuovo store nella cartella `dir` (di solito `<repo>/.kito`).
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
        // Il file `format` è l'ultimo a nascere: una cartella senza di esso è
        // un'inizializzazione interrotta, che si può ripetere.
        let format = dir.join(FORMAT_FILE);
        atomic_write(&format, FORMAT.as_bytes()).at(&format)?;
        Store::open_with(dir, opts)
    }

    /// Apre uno store esistente ed esegue il recupero: scarta l'eventuale
    /// record finale troncato e riporta gli indici in pari con i log.
    pub fn open(dir: &Path) -> Result<Store> {
        Store::open_with(dir, StoreOptions::default())
    }

    pub fn open_with(dir: &Path, opts: StoreOptions) -> Result<Store> {
        opts.validate()?;
        match fs::read_to_string(dir.join(FORMAT_FILE)) {
            Ok(found) if found == FORMAT => {}
            Ok(found) => return Err(StoreError::UnsupportedFormat(found.trim().to_owned())),
            Err(_) => return Err(StoreError::NotAStore(dir.to_owned())),
        }

        let lock_path = dir.join(LOCK_FILE);
        let lock =
            OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).at(&lock_path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::Locked(dir.to_owned())),
            Err(TryLockError::Error(e)) => return Err(e).at(&lock_path),
        }

        remove_stray_tmp(dir).at(dir)?;
        let lost_found = dir.join(LOST_FOUND);

        // L'indice si apre per primo: il suo watermark dice fin dove i log
        // erano già confermati, e il recupero dei log lo usa come soglia sotto
        // la quale non scarta nulla (a meno della modalità riparazione).
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

        // Ciò che il recupero ha trovato nei log potrebbe non essere mai
        // stato sincronizzato. Lo si rende durevole prima che l'indice lo
        // registri come confermato.
        packs.sync()?;
        oplog.sync()?;
        let (index, index_recovery) = reconcile_index(index, &oplog, &packs, checkpoints.len())?;

        Ok(Store {
            dir: dir.to_owned(),
            opts,
            _lock: lock,
            packs,
            oplog,
            checkpoints,
            index,
            pending: Pending::default(),
            poisoned: false,
            recovery: RecoveryReport {
                oplog: oplog_recovery,
                packs: packs_recovery,
                checkpoints: checkpoints_recovery,
                index: index_recovery,
            },
            index_dirty: false,
            last_index_sync: Instant::now(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Cosa ha fatto il recupero all'apertura.
    pub fn recovery(&self) -> &RecoveryReport {
        &self.recovery
    }

    /// `true` dopo un errore di scrittura: lo store accetta solo letture e
    /// va riaperto.
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

    /// Dopo un errore di I/O lo stato dei file è incerto: si smette di scrivere.
    fn guard<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(result, Err(StoreError::Io { .. } | StoreError::Poisoned | StoreError::Index(_))) {
            self.poisoned = true;
        }
        result
    }

    // ---------------------------------------------------------------- blob

    /// Accoda un blob e ne restituisce l'ID (BLAKE3 del contenuto). Un blob
    /// già presente non viene riscritto.
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

    /// Contenuto di un blob, verificato contro il suo hash.
    pub fn get_blob(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        match self.blob_pos(id)? {
            Some(pos) => self.packs.read(pos, id).map(Some),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------ op

    /// Accoda un'operazione. Restituisce `false` se era già nel log.
    ///
    /// L'op viene rifiutata se il suo ID non corrisponde al contenuto, o se
    /// un genitore o un blob a cui fa riferimento non è nello store: il log è
    /// sempre un ordinamento topologico del DAG.
    pub fn append_op(&mut self, op: &Op) -> Result<bool> {
        self.check()?;
        if !op.verify_id() {
            return Err(StoreError::InvalidOp(format!(
                "l'ID {} non corrisponde al contenuto dell'op",
                op.id
            )));
        }
        if self.has_op(&op.id)? {
            return Ok(false);
        }
        for parent in &op.parents {
            if !self.has_op(parent)? {
                return Err(StoreError::InvalidOp(format!("op {}: genitore {parent} sconosciuto", op.id)));
            }
        }
        for blob in op.kind.blobs() {
            if !self.has_blob(&blob)? {
                return Err(StoreError::InvalidOp(format!("op {}: blob {blob} sconosciuto", op.id)));
            }
        }
        let bytes = codec::to_vec(op)?;
        // Il buffer dell'oplog arriva al file solo dentro `commit`, dopo che i
        // pack sono durevoli. Se accodare questa op chiuderebbe il segmento
        // (e quindi lo scriverebbe), o se il buffer è pieno, si conferma prima.
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

    /// Rende durevole tutto ciò che è stato accodato. Quando restituisce
    /// `Ok`, le op e i blob accodati finora sono confermati.
    pub fn commit(&mut self) -> Result<()> {
        self.check()?;
        let result = self.commit_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn commit_inner(&mut self) -> Result<()> {
        // Ordine obbligato: prima i blob, poi le op che li citano. Così
        // un'op presente su disco non cita mai un blob assente.
        self.packs.sync()?;
        self.oplog.sync()?;
        // Da qui le op sono confermate; l'indice è un derivato.
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
        })
    }

    fn sync_index(&mut self) -> Result<()> {
        self.index.sync()?;
        self.index_dirty = false;
        self.last_index_sync = Instant::now();
        Ok(())
    }

    /// Conferma tutto e chiude lo store lasciando l'indice durevole, così la
    /// prossima apertura non deve rileggere i log.
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
        self.oplog.read_with(pos, |payload, _| codec::from_slice::<Op>(payload))?.map_err(|e| {
            StoreError::corrupt(Path::new("oplog"), pos.to_u64(), format!("op illeggibile: {e}"))
        })
    }

    /// Numero totale di op nel log (comprese quelle non ancora confermate).
    pub fn op_count(&self) -> Result<u64> {
        Ok(self.index.op_count()? + self.pending.op_list.len() as u64)
    }

    /// Numero di op registrate in una lane.
    pub fn lane_len(&self, lane: LaneId) -> Result<u64> {
        let indexed = self.index.lane(lane)?.map_or(0, |(n, _)| n);
        Ok(indexed + self.pending.op_list.iter().filter(|op| op.lane == lane).count() as u64)
    }

    /// Ultima op registrata in una lane.
    pub fn lane_head(&self, lane: LaneId) -> Result<Option<OpId>> {
        if let Some(op) = self.pending.op_list.iter().rev().find(|op| op.lane == lane) {
            return Ok(Some(op.id));
        }
        Ok(self.index.lane(lane)?.map(|(_, head)| head))
    }

    /// Le lane che hanno almeno un'op nel log.
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

    /// Op di una lane in ordine di registrazione, dalla `start`-esima, al più `limit`.
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

    /// Op che toccano un simbolo, in ordine di log.
    pub fn symbol_ops(&self, sym: SymbolId) -> Result<Vec<Op>> {
        let mut positions = self.index.symbol_positions(sym)?;
        positions.extend(self.pending.op_list.iter().filter(|op| op.symbol == Some(sym)).map(|op| op.pos));
        positions.into_iter().map(|pos| self.read_op(pos)).collect()
    }

    /// Visita tutte le op in ordine di log.
    pub fn scan_ops(&self, mut f: impl FnMut(Pos, Op) -> Result<()>) -> Result<()> {
        self.oplog.scan(self.oplog.start(), |pos, payload, _| {
            let op = codec::from_slice::<Op>(payload).map_err(|e| {
                StoreError::corrupt(Path::new("oplog"), pos.to_u64(), format!("op illeggibile: {e}"))
            })?;
            f(pos, op)
        })
    }

    // ---------------------------------------------------------- checkpoint

    /// Registra (o aggiorna) un checkpoint in modo durevole. Prima conferma
    /// tutto ciò che è accodato, così il checkpoint punta a un'op durevole.
    pub fn put_checkpoint(&mut self, ck: &Checkpoint) -> Result<()> {
        self.check()?;
        if !self.has_op(&ck.head)? {
            return Err(StoreError::InvalidOp(format!("{}: op {} sconosciuta", ck.id, ck.head)));
        }
        self.commit()?;
        let put = self.checkpoints.put(ck);
        self.guard(put)?;
        // Il checkpoint è durevole; l'indice ne prende nota come soglia.
        let noted = self.watermark(None).and_then(|wm| self.index.apply(&IndexBatch::default(), wm, false));
        self.guard(noted)?;
        self.index_dirty = true;
        Ok(())
    }

    pub fn checkpoint(&self, id: CheckpointId) -> Option<&Checkpoint> {
        self.checkpoints.get(id)
    }

    /// Tutti i checkpoint, in ordine di ID.
    pub fn checkpoints(&self) -> impl DoubleEndedIterator<Item = &Checkpoint> {
        self.checkpoints.iter()
    }

    /// ID da assegnare al prossimo checkpoint.
    pub fn next_checkpoint_id(&self) -> CheckpointId {
        self.checkpoints.next_id()
    }

    // ------------------------------------------------------- manutenzione

    pub fn stats(&self) -> Result<StoreStats> {
        Ok(StoreStats {
            ops: self.op_count()?,
            blobs: self.index.blob_count()? + self.pending.blob_list.len() as u64,
            oplog_segments: self.oplog.segment_count(),
            packs: self.packs.log().segment_count(),
            checkpoints: self.checkpoints.iter().count(),
        })
    }

    /// Scarta gli indici e li ricostruisce da `objects/` e `oplog/`.
    pub fn rebuild_index(&mut self) -> Result<()> {
        self.commit()?;
        let path = self.dir.join("index").join("index.redb");
        // L'indice va chiuso prima di poterlo ricreare: lo si sostituisce
        // per un momento con uno temporaneo.
        let placeholder_path = self.dir.join("index").join("index.redb.rebuild");
        let placeholder = Index::open(&placeholder_path)?;
        let old = std::mem::replace(&mut self.index, placeholder);
        let rebuilt = old.recreate().and_then(|index| {
            let start = Watermark {
                oplog_end: self.oplog.start(),
                packs_end: self.packs.log().start(),
                tail: None,
                checkpoints_end: self.checkpoints.len(),
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
                // Lo store resta inutilizzabile finché non viene riaperto:
                // l'apertura ricostruirà l'indice da `path`.
                self.poisoned = true;
                let _ = path;
                Err(e)
            }
        }
    }

    /// Controllo completo di integrità (`fsck`): rilegge tutti i record,
    /// verifica CRC, hash dei blob, ID e genitori delle op, e che gli indici
    /// corrispondano ai log. Non modifica nulla.
    pub fn verify(&self) -> Result<VerifyReport> {
        let mut report = VerifyReport { checkpoints: self.checkpoints.iter().count(), ..Default::default() };
        let mut problems = Vec::new();

        let mut blobs: HashSet<BlobId> = HashSet::new();
        let scan = self.packs.scan_verified(self.packs.log().start(), |pos, id, _| {
            if blobs.insert(id) {
                match self.blob_pos(&id) {
                    Ok(Some(found)) if found == pos => {}
                    Ok(found) => {
                        problems.push(format!("blob {id}: nei pack a {pos:?}, nell'indice a {found:?}"))
                    }
                    Err(e) => problems.push(format!("blob {id}: indice illeggibile: {e}")),
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
                problems.push(format!("op a {pos:?}: l'ID {} non corrisponde al contenuto", op.id));
            }
            for parent in &op.parents {
                if !ops.contains(parent) {
                    problems.push(format!("op {}: il genitore {parent} non la precede nel log", op.id));
                }
            }
            for blob in op.kind.blobs() {
                if !blobs.contains(&blob) {
                    problems.push(format!("op {}: blob {blob} assente dai pack", op.id));
                }
            }
            if !ops.insert(op.id) {
                problems.push(format!("op {}: duplicata nel log a {pos:?}", op.id));
                return Ok(());
            }
            let entry = per_lane.entry(op.lane).or_insert((0, op.id));
            entry.0 += 1;
            entry.1 = op.id;
            match self.op_pos(&op.id) {
                Ok(Some(found)) if found == pos => {}
                Ok(found) => {
                    problems.push(format!("op {}: nel log a {pos:?}, nell'indice a {found:?}", op.id))
                }
                Err(e) => problems.push(format!("op {}: indice illeggibile: {e}", op.id)),
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
            Ok(n) => problems.push(format!("l'indice conta {n} op, il log ne contiene {}", report.ops)),
            Err(e) => problems.push(format!("indice illeggibile: {e}")),
        }
        match self.lanes() {
            Ok(lanes) => {
                if lanes.len() != per_lane.len() {
                    problems.push(format!("l'indice conta {} lane, il log {}", lanes.len(), per_lane.len()));
                }
                for l in lanes {
                    match per_lane.get(&l.lane) {
                        Some(&(n, head)) if n == l.ops && head == l.head => {}
                        other => {
                            problems.push(format!("{}: indice {:?}, log {other:?}", l.lane, (l.ops, l.head)))
                        }
                    }
                }
            }
            Err(e) => problems.push(format!("indice illeggibile: {e}")),
        }
        for ck in self.checkpoints.iter() {
            if !ops.contains(&ck.head) {
                problems.push(format!("{}: punta all'op {} che non è nel log", ck.id, ck.head));
            }
        }

        report.problems = problems;
        Ok(report)
    }
}

/// Porta l'indice in pari con i log, o lo ricostruisce se non è coerente.
fn reconcile_index(
    index: Index,
    oplog: &SegmentedLog,
    packs: &Packs,
    checkpoints_end: u64,
) -> Result<(Index, IndexRecovery)> {
    let start =
        Watermark { oplog_end: oplog.start(), packs_end: packs.log().start(), tail: None, checkpoints_end };
    let logs_end = (oplog.end(), packs.log().end());

    let (from, stale) = match index.watermark()? {
        None => (start, Some("indice assente o di un altro schema")),
        Some(wm) if !oplog.contains(wm.oplog_end) => (start, Some("l'indice è più avanti dell'oplog")),
        Some(wm) if !packs.log().contains(wm.packs_end) => (start, Some("l'indice è più avanti dei pack")),
        Some(wm) => match wm.tail {
            Some((pos, crc)) if !matches!(oplog.read_with(pos, |_, c| c), Ok(found) if found == crc) => {
                (start, Some("l'ultima op indicizzata non è più nel log"))
            }
            _ => (Watermark { checkpoints_end, ..wm }, None),
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
            // Uno store appena creato non ha niente da "ricostruire".
            if ops == 0 && blobs == 0 {
                Ok((index, IndexRecovery::UpToDate))
            } else {
                Ok((index, IndexRecovery::Rebuilt { reason: reason.to_owned(), ops, blobs }))
            }
        }
    }
}

/// Indicizza i record dei log da `from` in poi; l'ultima transazione è
/// durevole. Restituisce quante op e quanti blob ha indicizzato.
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
            StoreError::corrupt(Path::new("oplog"), pos.to_u64(), format!("op illeggibile: {e}"))
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
