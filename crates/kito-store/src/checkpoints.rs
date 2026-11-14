//! Log append-only dei checkpoint (`.kito/checkpoints`).
//!
//! Un checkpoint si aggiorna (per esempio con il commit git dopo l'export)
//! accodando un nuovo record con lo stesso ID: vale l'ultimo.

use crate::error::{Result, StoreError};
use crate::fs::FileSystem;
use crate::logfile::{LogFile, LogRecovery};
use crate::record::LogKind;
use kito_core::{codec, Checkpoint, CheckpointId};
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) struct CheckpointLog {
    file: LogFile,
    by_id: BTreeMap<CheckpointId, Checkpoint>,
}

impl CheckpointLog {
    pub(crate) fn open(
        path: &Path,
        fs: &dyn FileSystem,
        lost_found: &Path,
        floor: Option<u64>,
    ) -> Result<(CheckpointLog, LogRecovery)> {
        let (file, recovery) = if path.exists() {
            LogFile::recover(fs, path, LogKind::Checkpoints, 0, lost_found, floor)?
        } else {
            if let Some(floor) = floor.filter(|&f| f > crate::record::HEADER_LEN as u64) {
                return Err(StoreError::corrupt(
                    path,
                    0,
                    format!("il file manca ma risultano confermati {floor} byte: per accettare la perdita riaprire lo store in modalità riparazione"),
                ));
            }
            (LogFile::create(fs, path, LogKind::Checkpoints, 0)?, LogRecovery::default())
        };
        let mut by_id = BTreeMap::new();
        file.scan(0, &mut |off, payload, _| {
            let ck: Checkpoint = codec::from_slice(payload)
                .map_err(|e| StoreError::corrupt(path, off, format!("checkpoint illeggibile: {e}")))?;
            by_id.insert(ck.id, ck);
            Ok(())
        })?;
        Ok((CheckpointLog { file, by_id }, recovery))
    }

    /// Registra (o aggiorna) un checkpoint in modo durevole.
    pub(crate) fn put(&mut self, ck: &Checkpoint) -> Result<()> {
        let bytes = codec::to_vec(ck)?;
        self.file.append(&[&bytes])?;
        self.file.sync()?;
        self.by_id.insert(ck.id, ck.clone());
        Ok(())
    }

    pub(crate) fn get(&self, id: CheckpointId) -> Option<&Checkpoint> {
        self.by_id.get(&id)
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &Checkpoint> {
        self.by_id.values()
    }

    /// Lunghezza del file: tutto ciò che contiene è durevole.
    pub(crate) fn len(&self) -> u64 {
        self.file.len()
    }

    pub(crate) fn next_id(&self) -> CheckpointId {
        CheckpointId(self.by_id.keys().next_back().map_or(1, |id| id.0 + 1))
    }
}
