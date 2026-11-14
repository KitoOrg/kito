//! Un singolo file di log append-only: header, poi record con CRC.

use crate::error::{IoContext, Result, StoreError};
use crate::fs::{FileSystem, RawFile};
use crate::mmap::MappedFile;
use crate::record::{check_header, encode_header, encode_record, read_frame, Frame, LogKind, HEADER_LEN};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Esito del recupero di un file di log all'apertura.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogRecovery {
    /// Record validi trovati nel file attivo.
    pub records: u64,
    /// Byte di coda scartati perché non formavano un record valido.
    pub discarded_bytes: u64,
    /// Dove è stata salvata la coda scartata (`lost+found/`), se è stato possibile.
    pub saved_to: Option<PathBuf>,
}

/// File di log aperto in scrittura.
///
/// I record passano per tre stati: nel buffer in memoria (`append`), scritti
/// nel file (`flush`), durevoli (`sync`). Un errore di scrittura avvelena il
/// file: ogni chiamata successiva fallisce finché lo store non viene riaperto,
/// e il recupero all'apertura scarta l'eventuale record parziale.
pub(crate) struct LogFile {
    path: PathBuf,
    raw: Box<dyn RawFile>,
    reader: File,
    /// Byte validi nel file (header + record interi).
    written: u64,
    /// Ci sono byte scritti ma non ancora resi durevoli.
    dirty: bool,
    buf: Vec<u8>,
    map: Mutex<Option<Arc<MappedFile>>>,
    poisoned: bool,
}

impl LogFile {
    /// Crea un nuovo file con il solo header, in modo atomico.
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

    /// Apre un file esistente come file attivo: verifica tutti i record e
    /// tronca la coda che non forma un record valido (scrittura interrotta da
    /// un crash). La coda scartata viene prima copiata in `lost_found`.
    ///
    /// `floor` è il numero di byte del file che risultano già confermati
    /// (dal watermark dell'indice). Un record non valido prima di quella
    /// soglia non è una coda troncata ma un danno a dati confermati: il
    /// recupero si rifiuta di scartarli e restituisce un errore.
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
            // La mappatura viene rilasciata qui, prima di qualsiasi troncamento.
        };
        drop(reader);

        if let Some(floor) = floor.filter(|&floor| valid < floor) {
            return Err(StoreError::corrupt(
                path,
                valid,
                format!(
                    "record non valido in una zona già confermata (confermati {floor} byte, validi {valid}): \
                     per accettare la perdita riaprire lo store in modalità riparazione"
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
        // Non si sa se i byte trovati siano mai stati sincronizzati (dopo un
        // `kill -9` possono essere solo nella cache del sistema operativo):
        // il primo `sync` deve fare davvero fsync.
        file.dirty = true;
        Ok((file, recovery))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Lunghezza logica: byte scritti più quelli ancora nel buffer.
    pub(crate) fn len(&self) -> u64 {
        self.written + self.buf.len() as u64
    }

    /// Byte nel buffer, non ancora scritti nel file.
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

    /// Accoda un record (nel buffer) e ne restituisce l'offset.
    pub(crate) fn append(&mut self, parts: &[&[u8]]) -> Result<u64> {
        self.check()?;
        let off = self.len();
        encode_record(&mut self.buf, parts).map_err(StoreError::RecordTooLarge)?;
        Ok(off)
    }

    /// Scrive il buffer nel file.
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
                // La scrittura può essere arrivata a metà: si prova ad
                // annullarla. Se anche questo fallisce, ci pensa il recupero
                // alla prossima apertura.
                self.poisoned = true;
                let _ = self.raw.truncate(self.written);
                Err(e).at(&self.path)
            }
        }
    }

    /// Scrive il buffer e rende tutto durevole.
    pub(crate) fn sync(&mut self) -> Result<()> {
        self.flush()?;
        if self.dirty {
            if let Err(e) = self.raw.sync() {
                // Dopo un fsync fallito non si sa cosa sia arrivato su disco.
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
        // I record vengono scritti interi (`flush`), quindi ogni record che
        // inizia prima di `written` è contenuto per intero nella mappatura.
        let map = Arc::new(MappedFile::map_prefix(&self.reader, self.written).at(&self.path)?);
        *guard = Some(map.clone());
        Ok(map)
    }

    /// Legge il record all'offset `off` (anche se è ancora nel buffer).
    pub(crate) fn read_with<R>(&self, off: u64, f: impl FnOnce(&[u8], u32) -> R) -> Result<R> {
        if off < HEADER_LEN as u64 {
            return Err(StoreError::corrupt(&self.path, off, "offset dentro l'header"));
        }
        if off >= self.written {
            record_at(&self.path, &self.buf, off, self.written, f)
        } else {
            let map = self.map_covering(off)?;
            record_at(&self.path, map.bytes(), off, 0, f)
        }
    }

    /// Visita in ordine i record a partire da `from`, compresi quelli nel buffer.
    pub(crate) fn scan(&self, from: u64, f: &mut Visit<'_>) -> Result<()> {
        let mut off = from.max(HEADER_LEN as u64);
        if off < self.written {
            let map = self.map_covering(self.written - 1)?;
            off = scan_slice(&self.path, map.bytes(), off, 0, f)?;
        }
        scan_slice(&self.path, &self.buf, off, self.written, f)?;
        Ok(())
    }

    /// Converte il file attivo in un segmento sigillato (dopo `sync`).
    pub(crate) fn seal(self) -> SealedFile {
        debug_assert!(self.buf.is_empty() && !self.dirty);
        SealedFile { path: self.path, reader: self.reader, len: self.written, map: self.map }
    }
}

/// Callback di una scansione: offset del record, payload, CRC.
pub(crate) type Visit<'a> = dyn FnMut(u64, &[u8], u32) -> Result<()> + 'a;

/// Legge il record all'offset di file `off` da `data`, i cui byte
/// corrispondono agli offset di file `base..`.
fn record_at<R>(path: &Path, data: &[u8], off: u64, base: u64, f: impl FnOnce(&[u8], u32) -> R) -> Result<R> {
    match usize::try_from(off - base).map(|at| read_frame(data, at)) {
        Ok(Frame::Record { payload, crc, .. }) => Ok(f(payload, crc)),
        Ok(Frame::End) | Err(_) => Err(StoreError::corrupt(path, off, "nessun record a questo offset")),
        Ok(Frame::Torn(reason)) => Err(StoreError::corrupt(path, off, reason)),
    }
}

/// Visita i record di `data`, i cui byte corrispondono agli offset di file
/// `base..`. Restituisce l'offset di file dopo l'ultimo record.
fn scan_slice(path: &Path, data: &[u8], from: u64, base: u64, f: &mut Visit<'_>) -> Result<u64> {
    let mut off =
        usize::try_from(from - base).map_err(|_| StoreError::corrupt(path, from, "offset troppo grande"))?;
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

/// Segmento sigillato: completo, durevole, mai più modificato.
pub(crate) struct SealedFile {
    path: PathBuf,
    reader: File,
    len: u64,
    map: Mutex<Option<Arc<MappedFile>>>,
}

impl SealedFile {
    /// Apre un segmento sigillato controllandone solo l'header: i record si
    /// verificano con il CRC a ogni lettura.
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
            return Err(StoreError::corrupt(&self.path, off, "offset dentro l'header"));
        }
        let map = self.map()?;
        record_at(&self.path, map.bytes(), off, 0, f)
    }

    /// Visita i record da `from`. In un segmento sigillato un record non
    /// valido è corruzione di dati confermati, non una coda da scartare.
    pub(crate) fn scan(&self, from: u64, f: &mut Visit<'_>) -> Result<()> {
        let map = self.map()?;
        scan_slice(&self.path, map.bytes(), from.max(HEADER_LEN as u64), 0, f)?;
        Ok(())
    }
}

/// Copia la coda scartata in `lost+found/`, per ispezione. È un salvataggio
/// di cortesia: i byte scartati non sono mai stati confermati, quindi un
/// fallimento (per esempio disco pieno) non blocca il recupero.
fn save_lost_tail(lost_found: &Path, path: &Path, offset: u64, tail: &[u8]) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    fs::create_dir_all(lost_found).ok()?;
    let target = lost_found.join(format!("{name}.{offset}.{stamp}"));
    fs::write(&target, tail).ok()?;
    Some(target)
}
