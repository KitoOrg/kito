//! Supporto per i test di fault injection: un file system che esaurisce lo
//! spazio, fallisce `fsync` a comando e sa simulare una perdita di corrente.

use crate::fs::{FileSystem, RawFile, StdFs};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct Faults {
    /// Byte ancora scrivibili prima di "disco pieno" (`u64::MAX` = illimitati).
    space: AtomicU64,
    /// fsync ancora concessi prima che comincino a fallire (`u64::MAX` = illimitati).
    syncs_left: AtomicU64,
    fail_truncate: AtomicBool,
    /// Per ogni file scritto: quanti byte sono durevoli (coperti da un fsync).
    durable: Mutex<HashMap<PathBuf, u64>>,
}

/// File system reale con guasti iniettabili. I cloni condividono lo stato.
#[derive(Debug, Clone)]
pub struct FaultyFs {
    faults: Arc<Faults>,
}

impl Default for FaultyFs {
    fn default() -> Self {
        Self::new()
    }
}

/// Cosa resta, dopo una perdita di corrente, dei byte scritti ma non
/// sincronizzati di un file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsynced {
    /// Tutti persi: il file torna alla lunghezza durevole.
    Lost,
    /// Ne sopravvive solo un prefisso.
    Partial,
    /// Sopravvivono tutti.
    Kept,
    /// La lunghezza sopravvive ma il contenuto è azzerato.
    Zeroed,
    /// La lunghezza sopravvive ma il contenuto è spazzatura.
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

    /// Dopo altri `bytes` byte scritti, le scritture falliscono con "disco
    /// pieno"; la scrittura che supera il limite arriva a metà.
    pub fn set_space(&self, bytes: u64) {
        self.faults.space.store(bytes, Ordering::SeqCst);
    }

    /// Toglie il limite di spazio.
    pub fn free_space(&self) {
        self.faults.space.store(u64::MAX, Ordering::SeqCst);
    }

    /// Fa fallire (o torna a far riuscire) tutti gli fsync.
    pub fn fail_sync(&self, on: bool) {
        self.faults.syncs_left.store(if on { 0 } else { u64::MAX }, Ordering::SeqCst);
    }

    /// Concede ancora `n` fsync, poi li fa fallire tutti.
    pub fn fail_sync_after(&self, n: u64) {
        self.faults.syncs_left.store(n, Ordering::SeqCst);
    }

    /// Fa fallire anche il troncamento con cui lo store annulla una
    /// scrittura parziale: nel file resta un record a metà.
    pub fn fail_truncate(&self, on: bool) {
        self.faults.fail_truncate.store(on, Ordering::SeqCst);
    }

    /// Byte durevoli di un file scritto tramite questo file system.
    pub fn durable_len(&self, path: &Path) -> Option<u64> {
        self.faults.durable.lock().unwrap().get(path).copied()
    }

    /// Simula una perdita di corrente: per ogni file scritto, i byte oltre
    /// l'ultimo fsync subiscono la sorte scelta da `fate` (che riceve un
    /// contatore per variare le scelte). Lo store deve essere già stato
    /// abbandonato. Restituisce quanti file sono stati alterati.
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

    /// Consuma fino a `want` byte di spazio; restituisce quanti ne concede.
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
    io::Error::new(io::ErrorKind::StorageFull, "spazio esaurito (guasto iniettato)")
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
        let granted = left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
                u64::MAX => Some(n),
                0 => None,
                n => Some(n - 1),
            })
            .is_ok();
        if !granted {
            return Err(io::Error::other("fsync fallito (guasto iniettato)"));
        }
        self.inner.sync()?;
        self.fs.set_durable(&self.path, self.len);
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        if self.fs.faults.fail_truncate.load(Ordering::SeqCst) {
            return Err(io::Error::other("truncate fallito (guasto iniettato)"));
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
        // Ciò che è su disco all'apertura è, per definizione, sopravvissuto.
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
